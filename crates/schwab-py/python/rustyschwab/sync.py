"""Blocking versions of the client and the stream, for code without asyncio.

``rustyschwab.sync.Client`` has every method of :class:`rustyschwab.Client`, as
an ordinary function that returns the result. ``rustyschwab.sync.Stream`` is
iterable, and runs a callback on a background thread the way Schwabdev's
``stream.start(receiver)`` does.

.. code-block:: python

    from rustyschwab import sync

    with sync.Client.from_env() as client:
        print(client.quote("AAPL")["quote"]["lastPrice"])

        stream = client.open_stream()
        stream.subscribe("LEVELONE_EQUITIES", ["AAPL"], fields=["bidPrice", "askPrice"])
        stream.start(print)
        ...
        stream.stop()

The work runs on the same Rust runtime as the async API. Waiting for a result
happens on one private asyncio event loop on a daemon thread, created on first
use and shared by every client, stream and schedule in the process. It stops at
interpreter exit, or earlier with :func:`shutdown`; using the package again
after that starts a new one.

Calling a blocking method from inside a running event loop blocks that loop for
the duration of the call. In asyncio code use :class:`rustyschwab.Client`.
"""

from __future__ import annotations

import asyncio
import atexit
import concurrent.futures
import functools
import logging
import os
import threading
import weakref
from datetime import date, time, timedelta, tzinfo
from types import TracebackType
from typing import (
    TYPE_CHECKING,
    Any,
    Awaitable,
    Callable,
    Coroutine,
    Iterable,
    Iterator,
    Mapping,
    Sequence,
    TypeVar,
)

from . import auth as _auth
from . import schedule as _schedule
from ._native import Client as _NativeClient
from ._native import Stream as _NativeStream
from .auth import CallOnAuth
from .errors import AuthError
from .schedule import WEEKDAYS, Subscriptions
from .streaming import OnError, Receiver, check_on_error

if TYPE_CHECKING:
    from ._native import JsonObject, RequestBudget, StreamMessage, Submitted

__all__ = ["Client", "Schedule", "Stream", "shutdown"]

_log = logging.getLogger("rustyschwab.sync")

_T = TypeVar("_T")
_ClientT = TypeVar("_ClientT", bound="Client")
_StreamT = TypeVar("_StreamT", bound="Stream")
_ScheduleT = TypeVar("_ScheduleT", bound="Schedule")


# ---- the private event loop ------------------------------------------------------------------


def _cancel_all_tasks(loop: asyncio.AbstractEventLoop) -> None:
    tasks = asyncio.all_tasks(loop)
    for task in tasks:
        task.cancel()
    if tasks:
        loop.run_until_complete(asyncio.gather(*tasks, return_exceptions=True))


class _LoopThread:
    """An event loop running on a daemon thread, and the means to submit to it."""

    def __init__(self) -> None:
        self._loop = asyncio.new_event_loop()
        self._started = threading.Event()
        self._thread = threading.Thread(target=self._main, name="rustyschwab-sync", daemon=True)

    def start(self) -> None:
        self._thread.start()
        self._started.wait()

    @property
    def alive(self) -> bool:
        return self._thread.is_alive()

    def _main(self) -> None:
        loop = self._loop
        asyncio.set_event_loop(loop)
        loop.call_soon(self._started.set)
        try:
            loop.run_forever()
        finally:
            try:
                _cancel_all_tasks(loop)
                loop.run_until_complete(loop.shutdown_asyncgens())
                loop.run_until_complete(loop.shutdown_default_executor())
            finally:
                asyncio.set_event_loop(None)
                loop.close()

    def submit(self, awaitable: Awaitable[_T]) -> concurrent.futures.Future[_T]:
        async def await_it() -> _T:
            return await awaitable

        # A coroutine goes to the loop as it is: wrapping it would leave it
        # un-awaited, and warning, when the wrapper is cancelled before it runs.
        coroutine: Coroutine[Any, Any, _T] = (
            awaitable if asyncio.iscoroutine(awaitable) else await_it()
        )
        if threading.current_thread() is self._thread:
            coroutine.close()
            raise RuntimeError(
                "a blocking rustyschwab.sync call was made from the sync event loop's own "
                "thread (for example inside a coroutine); use the async API there"
            )
        return asyncio.run_coroutine_threadsafe(coroutine, self._loop)

    def spawn(
        self, coroutine: Coroutine[Any, Any, _T], done: Callable[[asyncio.Task[_T]], None]
    ) -> asyncio.Task[_T]:
        """Start ``coroutine`` as a task and call ``done(task)`` on the loop when it ends."""

        async def create() -> asyncio.Task[_T]:
            task = asyncio.ensure_future(coroutine)
            task.add_done_callback(done)
            return task

        return self.run(create())

    def cancel(self, task: asyncio.Task[Any]) -> None:
        self._loop.call_soon_threadsafe(task.cancel)

    def run(self, awaitable: Awaitable[_T], timeout: float | None = None) -> _T:
        """Wait for ``awaitable`` on the loop. A timeout or interrupt cancels it."""
        future = self.submit(awaitable)
        try:
            return future.result(timeout)
        except concurrent.futures.TimeoutError:
            future.cancel()
            raise TimeoutError(f"no result within {timeout} seconds") from None
        except BaseException:
            future.cancel()
            raise

    def stop(self, timeout: float | None = None) -> None:
        if self._thread.is_alive():
            self._loop.call_soon_threadsafe(self._loop.stop)
            self._thread.join(timeout)


_lock = threading.Lock()
_current: _LoopThread | None = None
#: Streams and schedules that may still hold threads or tasks; `shutdown` stops them.
_resources: weakref.WeakSet[Stream | Schedule] = weakref.WeakSet()


def _runner() -> _LoopThread:
    global _current
    with _lock:
        if _current is None or not _current.alive:
            _current = _LoopThread()
            _current.start()
        return _current


def shutdown(timeout: float | None = 5.0) -> None:
    """Stop every sync stream and schedule, then the private event loop.

    Called automatically at interpreter exit. The next blocking call starts a
    fresh loop, so this is also how a program releases the thread early.
    """
    global _current
    for resource in list(_resources):
        try:
            resource._shutdown(timeout)
        except Exception:
            _log.exception("stopping %r failed", resource)
    with _lock:
        runner, _current = _current, None
    if runner is not None:
        runner.stop(timeout)


def _forget_loop_in_child() -> None:
    # A forked child has none of the parent's threads.
    global _current, _lock
    _current = None
    _lock = threading.Lock()
    _resources.clear()


atexit.register(shutdown)
if hasattr(os, "register_at_fork"):
    os.register_at_fork(after_in_child=_forget_loop_in_child)


# ---- the client -------------------------------------------------------------------------------


class Client:
    """:class:`rustyschwab.Client` with blocking methods.

    Every method of the async client exists here with the same arguments and
    result, minus the ``await``. ``timeout`` (seconds, default none) bounds how
    long a call waits for its result; a call that exceeds it is cancelled and
    raises ``TimeoutError``. The SDK's own request timeouts apply regardless,
    and a cancelled order submission is resolved by the execution journal
    exactly as for a cancelled coroutine, so leave the default for trading
    code.

    ``call_on_auth`` is a function from the consent URL to the redirect URL
    (see :mod:`rustyschwab.auth`). When it is set and a call fails because the
    refresh token is missing or expired, it is called in the calling thread, the
    authorization is completed, and the call is retried once. Order
    submission, replacement and cancellation never prompt or retry this way:
    authorize first with :meth:`authorize`.

    It is a context manager: leaving the block closes the streams opened
    through it.
    """

    def __init__(
        self,
        config: Mapping[str, Any],
        *,
        timeout: float | None = None,
        call_on_auth: CallOnAuth | None = None,
    ) -> None:
        self._setup(_NativeClient(config), timeout, call_on_auth)

    @classmethod
    def from_env(
        cls: type[_ClientT],
        overrides: Mapping[str, Any] | None = None,
        *,
        timeout: float | None = None,
        call_on_auth: CallOnAuth | None = None,
    ) -> _ClientT:
        client = object.__new__(cls)
        client._setup(_NativeClient.from_env(overrides), timeout, call_on_auth)
        return client

    def _setup(
        self, native: _NativeClient, timeout: float | None, call_on_auth: CallOnAuth | None
    ) -> None:
        self._native = native
        self.timeout = timeout
        self.call_on_auth = call_on_auth
        self._auth_lock = threading.Lock()
        self._auth_generation = 0
        self._streams: weakref.WeakSet[Stream] = weakref.WeakSet()
        self._schedules: weakref.WeakSet[Schedule] = weakref.WeakSet()

    @property
    def native(self) -> _NativeClient:
        """The async client underneath, for use on your own event loop."""
        return self._native

    def __repr__(self) -> str:
        return repr(self._native).replace("Client(", "sync.Client(", 1)

    def _call(self, make: Callable[[], Awaitable[_T]], *, reauth: bool = True) -> _T:
        generation = self._auth_generation
        try:
            return _runner().run(make(), self.timeout)
        except AuthError as error:
            hook = self.call_on_auth
            if hook is None or not reauth or not error.reauthorization_required:
                raise
            with self._auth_lock:
                # Another thread may have authorized while this one waited.
                renewed = self._auth_generation != generation or self.authorize(hook)
                if not renewed:
                    raise
            return _runner().run(make(), self.timeout)

    def authorize(self, call_on_auth: CallOnAuth | None = None) -> bool:
        """Run the interactive authorization flow and store the new tokens.

        Uses ``call_on_auth`` (else the client's own hook, else the terminal
        prompt of :func:`rustyschwab.auth.terminal_prompt`) to get the redirect
        URL. Returns ``False`` if the hook gave none.
        """
        hook = call_on_auth if call_on_auth is not None else self.call_on_auth
        done = _auth.authorize(self, hook)
        if done:
            self._auth_generation += 1
        return done

    def close(self) -> None:
        """Stop the streams and schedules opened through this client. Idempotent."""
        failure: Exception | None = None
        for stop in [*(s.stop for s in self._schedules), *(s.close for s in self._streams)]:
            try:
                stop()
            except Exception as error:
                failure = failure or error
        if failure is not None:
            raise failure

    def __enter__(self: _ClientT) -> _ClientT:
        self.init()
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None = None,
        exc: BaseException | None = None,
        tb: TracebackType | None = None,
    ) -> None:
        self.close()

    def init(self) -> None:
        self._call(lambda: self._native.init())

    def authorization_url(self) -> str:
        return self._call(lambda: self._native.authorization_url(), reauth=False)

    def complete_authorization(self, redirect_url_or_code: str) -> None:
        self._call(
            lambda: self._native.complete_authorization(redirect_url_or_code), reauth=False
        )

    # market data

    def quotes(
        self,
        symbols: Sequence[str],
        *,
        fields: Sequence[str] | None = None,
        indicative: bool | None = None,
    ) -> dict[str, JsonObject]:
        return self._call(
            lambda: self._native.quotes(symbols, fields=fields, indicative=indicative)
        )

    def quote(self, symbol: str, *, fields: Sequence[str] | None = None) -> JsonObject:
        return self._call(lambda: self._native.quote(symbol, fields=fields))

    def price_history(self, params: Mapping[str, Any]) -> JsonObject:
        return self._call(lambda: self._native.price_history(params))

    def option_chain(self, params: Mapping[str, Any]) -> JsonObject:
        return self._call(lambda: self._native.option_chain(params))

    def option_expirations(self, symbol: str) -> JsonObject:
        return self._call(lambda: self._native.option_expirations(symbol))

    def movers(
        self,
        index: str,
        *,
        sort: str | None = None,
        frequency: int | None = None,
    ) -> JsonObject:
        return self._call(lambda: self._native.movers(index, sort=sort, frequency=frequency))

    def market_hours(
        self, markets: Sequence[str], *, date: date | str | None = None
    ) -> JsonObject:
        return self._call(lambda: self._native.market_hours(markets, date=date))

    def instruments(self, symbol: str, projection: str = "symbol-search") -> JsonObject:
        return self._call(lambda: self._native.instruments(symbol, projection))

    def instrument(self, cusip: str) -> JsonObject:
        return self._call(lambda: self._native.instrument(cusip))

    def get_json(self, path: str, query: Mapping[str, str] | None = None) -> Any:
        return self._call(lambda: self._native.get_json(path, query))

    # accounts

    def account_numbers(self) -> list[JsonObject]:
        return self._call(lambda: self._native.account_numbers())

    def accounts(self, *, include_positions: bool = False) -> list[JsonObject]:
        return self._call(lambda: self._native.accounts(include_positions=include_positions))

    def account(self, account_hash: str, *, include_positions: bool = False) -> JsonObject:
        return self._call(
            lambda: self._native.account(account_hash, include_positions=include_positions)
        )

    def user_preferences(self) -> JsonObject:
        return self._call(lambda: self._native.user_preferences())

    # orders

    def account_orders(self, account_hash: str, params: Mapping[str, Any]) -> list[JsonObject]:
        return self._call(lambda: self._native.account_orders(account_hash, params))

    def all_orders(self, params: Mapping[str, Any]) -> list[JsonObject]:
        return self._call(lambda: self._native.all_orders(params))

    def order(self, account_hash: str, order_id: str) -> JsonObject:
        return self._call(lambda: self._native.order(account_hash, order_id))

    def transactions(self, account_hash: str, params: Mapping[str, Any]) -> list[JsonObject]:
        return self._call(lambda: self._native.transactions(account_hash, params))

    def transaction(self, account_hash: str, transaction_id: str) -> JsonObject:
        return self._call(lambda: self._native.transaction(account_hash, transaction_id))

    def preview_order(self, account_hash: str, order: Mapping[str, Any]) -> JsonObject:
        return self._call(lambda: self._native.preview_order(account_hash, order))

    def place_order(self, account_hash: str, order: Mapping[str, Any]) -> Submitted:
        return self._call(lambda: self._native.place_order(account_hash, order), reauth=False)

    def replace_order(
        self, account_hash: str, order_id: str, order: Mapping[str, Any]
    ) -> Submitted:
        return self._call(
            lambda: self._native.replace_order(account_hash, order_id, order), reauth=False
        )

    def cancel_order(self, account_hash: str, order_id: str) -> None:
        self._call(lambda: self._native.cancel_order(account_hash, order_id), reauth=False)

    # state and controls

    def request_budget(self) -> RequestBudget | None:
        return self._native.request_budget()

    def circuit_state(self) -> str:
        return self._native.circuit_state()

    def readiness(self, accounts: Sequence[str], *, max_age_ms: int = 60_000) -> JsonObject:
        return self._call(lambda: self._native.readiness(accounts, max_age_ms=max_age_ms))

    def halt(self, reason: str, operator: str) -> None:
        self._call(lambda: self._native.halt(reason, operator))

    def clear_halt(self, operator: str) -> None:
        self._call(lambda: self._native.clear_halt(operator))

    def halted(self) -> str | None:
        return self._call(lambda: self._native.halted())

    # streaming

    def open_stream(
        self,
        overrides: Mapping[str, Any] | None = None,
        *,
        named_fields: bool = True,
    ) -> Stream:
        native = self._call(lambda: self._native.open_stream(overrides, named_fields=named_fields))
        stream = Stream(native)
        self._streams.add(stream)
        return stream

    def run_during_market_hours(
        self,
        start_stream: Subscriptions | Callable[[Client], Stream],
        receiver: Receiver,
        *,
        receiver_kwargs: Mapping[str, Any] | None = None,
        market: str = "equity",
        sessions: str | Iterable[str] = ("regularMarket",),
        product: str | None = None,
        lead: timedelta = timedelta(minutes=1),
        tz: str | tzinfo = "America/New_York",
        on_error: OnError = "log",
        max_sessions: int | None = None,
        lookahead_days: int = 10,
        retry_initial: float = 5.0,
        retry_max: float = 300.0,
    ) -> Schedule:
        """Stream during the market's real sessions, in the background.

        The blocking form of :func:`rustyschwab.schedule.run_during_market_hours`:
        it returns a :class:`Schedule` at once, and ``receiver`` is called with
        each message on a worker thread, in order, one message at a time.
        ``start_stream`` is a :class:`~rustyschwab.schedule.Subscriptions`, or a
        function that takes this client and returns an open, subscribed
        :class:`Stream`.
        """

        def main(receive: Receiver, opener: Callable[[Any], Awaitable[Any]]) -> Awaitable[int]:
            return _schedule.run_during_market_hours(
                self._native,
                opener,
                receiver=receive,
                receiver_kwargs=receiver_kwargs,
                market=market,
                sessions=sessions,
                product=product,
                lead=lead,
                tz=tz,
                on_error=on_error,
                max_sessions=max_sessions,
                lookahead_days=lookahead_days,
                retry_initial=retry_initial,
                retry_max=retry_max,
            )

        return self._launch(start_stream, receiver, on_error, main)

    def run_during_hours(
        self,
        start_stream: Subscriptions | Callable[[Client], Stream],
        receiver: Receiver,
        *,
        receiver_kwargs: Mapping[str, Any] | None = None,
        start_time: time = time(9, 29),
        stop_time: time = time(16, 0),
        on_days: Iterable[int] = WEEKDAYS,
        tz: str | tzinfo = "America/New_York",
        lead: timedelta = timedelta(0),
        on_error: OnError = "log",
        max_sessions: int | None = None,
        retry_initial: float = 5.0,
        retry_max: float = 300.0,
    ) -> Schedule:
        """Stream between fixed clock times, in the background (Schwabdev's ``start_auto``).

        The blocking form of :func:`rustyschwab.schedule.run_during_hours`; see
        :meth:`run_during_market_hours` for ``start_stream`` and ``receiver``.
        """

        def main(receive: Receiver, opener: Callable[[Any], Awaitable[Any]]) -> Awaitable[int]:
            return _schedule.run_during_hours(
                self._native,
                opener,
                receiver=receive,
                receiver_kwargs=receiver_kwargs,
                start_time=start_time,
                stop_time=stop_time,
                on_days=on_days,
                tz=tz,
                lead=lead,
                on_error=on_error,
                max_sessions=max_sessions,
                retry_initial=retry_initial,
                retry_max=retry_max,
            )

        return self._launch(start_stream, receiver, on_error, main)

    def _launch(
        self,
        start_stream: Subscriptions | Callable[[Client], Stream],
        receiver: Receiver,
        on_error: OnError,
        main: Callable[[Receiver, Callable[[Any], Awaitable[Any]]], Awaitable[int]],
    ) -> Schedule:
        _require_plain_function(receiver)
        check_on_error(on_error)
        executor = concurrent.futures.ThreadPoolExecutor(
            max_workers=1, thread_name_prefix="rustyschwab-receiver"
        )

        async def receive(message: StreamMessage, **kwargs: Any) -> None:
            loop = asyncio.get_running_loop()
            await loop.run_in_executor(executor, functools.partial(receiver, message, **kwargs))

        async def open_blocking(_native: Any) -> Any:
            assert not isinstance(start_stream, Subscriptions)
            loop = asyncio.get_running_loop()
            stream = await loop.run_in_executor(None, start_stream, self)
            return stream._native

        opener: Callable[[Any], Awaitable[Any]] = (
            start_stream if isinstance(start_stream, Subscriptions) else open_blocking
        )
        async def run() -> int:
            return await main(receive, opener)

        def finished(task: asyncio.Task[int]) -> None:
            executor.shutdown(wait=False, cancel_futures=True)
            if not task.cancelled() and task.exception() is not None:
                _log.error("the stream schedule stopped", exc_info=task.exception())

        runner = _runner()
        schedule = Schedule(runner, runner.spawn(run(), finished))
        self._schedules.add(schedule)
        return schedule


# ---- streams --------------------------------------------------------------------------------


def _require_plain_function(receiver: Receiver) -> None:
    import inspect

    if inspect.iscoroutinefunction(receiver):
        raise TypeError(
            "a blocking receiver cannot be a coroutine function; use "
            "rustyschwab.streaming.dispatch with the async client for that"
        )


async def _next_message(native: _NativeStream) -> StreamMessage:
    return await anext(native)


class Stream:
    """A streamer session with blocking methods; get one from :meth:`Client.open_stream`.

    Read it by iterating (``for message in stream``), by calling :meth:`recv`
    with a timeout, or by handing :meth:`start` a callback that a background
    thread calls for every message, as Schwabdev's ``stream.start(receiver)``
    does. A started stream cannot also be iterated, because there is one queue
    of messages.

    Messages are those of :class:`rustyschwab.Stream`: dicts with a ``type`` of
    ``data``, ``response``, ``notify`` or ``state``. The SDK reconnects and
    replays subscriptions itself.
    """

    def __init__(self, native: _NativeStream) -> None:
        self._native = native
        self._lock = threading.Lock()
        self._pending: concurrent.futures.Future[StreamMessage] | None = None
        self._thread: threading.Thread | None = None
        self._stop_requested = threading.Event()
        self._error: BaseException | None = None
        self._closed = False
        _resources.add(self)

    def __repr__(self) -> str:
        return repr(self._native).replace("Stream(", "sync.Stream(", 1)

    # session

    def subscribe(
        self,
        service: str,
        keys: Sequence[str],
        *,
        fields: Sequence[int | str] | None = None,
    ) -> None:
        _runner().run(self._native.subscribe(service, keys, fields=fields))

    def unsubscribe(self, service: str, keys: Sequence[str]) -> None:
        _runner().run(self._native.unsubscribe(service, keys))

    def view(self, service: str, fields: Sequence[int | str]) -> None:
        _runner().run(self._native.view(service, fields))

    def subscriptions(self) -> dict[str, list[str]]:
        return self._native.subscriptions()

    def status(self) -> JsonObject:
        return self._native.status()

    @property
    def is_authenticated(self) -> bool:
        return self._native.is_authenticated

    @property
    def native(self) -> _NativeStream:
        """The async stream underneath, for use on your own event loop."""
        return self._native

    # reading

    def _recv(self, timeout: float | None) -> StreamMessage:
        future = _runner().submit(_next_message(self._native))
        with self._lock:
            self._pending = future
        if self._stop_requested.is_set() and self._thread is threading.current_thread():
            future.cancel()
        try:
            return future.result(timeout)
        except concurrent.futures.TimeoutError:
            future.cancel()
            raise TimeoutError(f"no stream message within {timeout} seconds") from None
        except BaseException:
            future.cancel()
            raise
        finally:
            with self._lock:
                if self._pending is future:
                    self._pending = None

    def _check_not_started(self) -> None:
        if self.running:
            raise RuntimeError("the stream is being read by start(); stop() it to read directly")

    def recv(self, timeout: float | None = None) -> StreamMessage | None:
        """The next message, or ``None`` once the stream has ended.

        Raises ``TimeoutError`` if none arrives within ``timeout`` seconds; the
        stream stays usable.
        """
        self._check_not_started()
        try:
            return self._recv(timeout)
        except StopAsyncIteration:
            return None

    def __iter__(self) -> Iterator[StreamMessage]:
        return self

    def __next__(self) -> StreamMessage:
        message = self.recv()
        if message is None:
            raise StopIteration
        return message

    # callbacks

    @property
    def running(self) -> bool:
        """Whether the :meth:`start` thread is alive."""
        thread = self._thread
        return thread is not None and thread.is_alive()

    @property
    def error(self) -> BaseException | None:
        """The receiver exception that stopped a ``start(on_error="raise")`` thread."""
        return self._error

    def start(
        self,
        receiver: Receiver,
        *,
        daemon: bool = True,
        on_error: OnError = "log",
        **kwargs: Any,
    ) -> None:
        """Call ``receiver(message, **kwargs)`` for each message on a background thread.

        The thread ends when the stream does, or at :meth:`stop` or
        :meth:`close`. An exception in the receiver is logged to
        ``rustyschwab.sync`` and the next message is delivered, unless
        ``on_error="raise"``: then the thread stops, :attr:`error` holds the
        exception, and :meth:`stop` and :meth:`join` raise it. With
        ``daemon=False`` the interpreter waits for the thread at exit, so
        something must call :meth:`stop`. ``receiver`` must be a plain function.
        """
        _require_plain_function(receiver)
        check_on_error(on_error)
        if self._closed:
            raise RuntimeError("the stream is closed")
        with self._lock:
            if self.running:
                raise RuntimeError("the stream is already started")
            self._stop_requested.clear()
            self._error = None
            self._thread = threading.Thread(
                target=self._pump,
                args=(receiver, kwargs, on_error),
                name="rustyschwab-stream",
                daemon=daemon,
            )
            self._thread.start()

    def _pump(self, receiver: Receiver, kwargs: dict[str, Any], on_error: OnError) -> None:
        try:
            while not self._stop_requested.is_set():
                try:
                    message = self._recv(None)
                except (StopAsyncIteration, concurrent.futures.CancelledError):
                    return
                except Exception as error:
                    self._fail(error, "reading the stream failed")
                    return
                try:
                    receiver(message, **kwargs)
                except Exception as error:
                    if on_error == "raise":
                        self._fail(error, "the stream receiver raised")
                        return
                    _log.exception("stream receiver %r raised", receiver)
        finally:
            with self._lock:
                self._pending = None

    def _fail(self, error: Exception, what: str) -> None:
        _log.error("%s; stopping the stream thread", what, exc_info=error)
        self._error = error

    def join(self, timeout: float | None = None) -> None:
        """Wait for the :meth:`start` thread to end (``timeout`` seconds at most).

        Re-raises the receiver exception of a ``start(on_error="raise")`` stream
        once.
        """
        thread = self._thread
        if thread is not None and thread is not threading.current_thread():
            thread.join(timeout)
        self._raise_error()

    def _raise_error(self) -> None:
        error, self._error = self._error, None
        if error is not None:
            raise error

    def stop(self, timeout: float | None = None) -> None:
        """Stop the :meth:`start` thread and wait for it. Idempotent.

        The connection stays open with its subscriptions, so :meth:`start` can
        be called again; messages that arrive meanwhile queue up to the SDK's
        limit, and any lost beyond it are counted in ``lost_messages``. Use
        :meth:`close` to disconnect. Raises the receiver exception once if the
        thread was stopped by ``on_error="raise"``.
        """
        self._stop_requested.set()
        with self._lock:
            pending = self._pending
        if pending is not None:
            pending.cancel()
        self.join(timeout)

    def close(self) -> None:
        """Stop the thread and log out and disconnect. Idempotent."""
        try:
            self.stop()
        finally:
            if not self._closed:
                self._closed = True
                _runner().run(self._native.close())

    def _shutdown(self, timeout: float | None) -> None:
        if self._thread is not None:
            self._stop_requested.set()
            with self._lock:
                pending = self._pending
            if pending is not None:
                pending.cancel()
            self._thread.join(timeout)
        if not self._closed:
            self._closed = True
            _runner().run(self._native.close(), timeout)

    def __enter__(self: _StreamT) -> _StreamT:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None = None,
        exc: BaseException | None = None,
        tb: TracebackType | None = None,
    ) -> None:
        self.close()


# ---- schedules ----------------------------------------------------------------------------------


class Schedule:
    """A stream that opens and closes with the market, running in the background.

    Returned by :meth:`Client.run_during_market_hours` and
    :meth:`Client.run_during_hours`. :meth:`stop` ends it and closes the stream
    if one is open; :meth:`join` waits for it to finish by itself, which only
    happens with ``max_sessions``, or on a receiver error with
    ``on_error="raise"``.
    """

    def __init__(self, runner: _LoopThread, task: asyncio.Task[int]) -> None:
        self._runner = runner
        self._task = task
        self._finished = threading.Event()
        self._reported = False
        task.get_loop().call_soon_threadsafe(task.add_done_callback, self._on_done)
        _resources.add(self)

    def _on_done(self, _task: asyncio.Task[int]) -> None:
        self._finished.set()

    def __repr__(self) -> str:
        return f"Schedule(running={self.running})"

    @property
    def running(self) -> bool:
        """Whether the schedule is still active."""
        return not self._finished.is_set()

    def join(self, timeout: float | None = None) -> int | None:
        """Wait for the schedule to end and return the sessions completed.

        Returns ``None`` if it was stopped. Raises what stopped it, if
        anything, once. Raises ``TimeoutError`` if it is still running after
        ``timeout`` seconds.
        """
        if not self._finished.wait(timeout):
            raise TimeoutError(f"the schedule is still running after {timeout} seconds")
        if not self._task.done() or self._task.cancelled():
            return None
        error = self._task.exception()
        if error is not None and not self._reported:
            self._reported = True
            raise error
        return None if error is not None else self._task.result()

    def stop(self, timeout: float | None = None) -> None:
        """Cancel the schedule, close its stream and wait. Idempotent.

        Raises what had already stopped it, such as a receiver error with
        ``on_error="raise"``.
        """
        if not self._runner.alive:
            # `shutdown` already cancelled the task along with the loop.
            self._finished.set()
        elif not self._finished.is_set():
            self._runner.cancel(self._task)
        # The task closes its stream as it unwinds, and reports last.
        self._finished.wait(timeout)
        if self._finished.is_set():
            self.join(0)

    def _shutdown(self, timeout: float | None) -> None:
        self.stop(timeout)

    def __enter__(self: _ScheduleT) -> _ScheduleT:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None = None,
        exc: BaseException | None = None,
        tb: TracebackType | None = None,
    ) -> None:
        self.stop()


def _copy_docs() -> None:
    """Give each blocking twin the docstring of the native method it mirrors."""
    for name, method in vars(Client).items():
        native = getattr(_NativeClient, name, None)
        if callable(method) and method.__doc__ is None and native is not None:
            method.__doc__ = native.__doc__


_copy_docs()
