"""Open the stream for the market's sessions and close it afterwards.

Schwabdev's ``start_auto`` starts and stops the stream at fixed clock times on
fixed weekdays, so it opens on holidays and runs past an early close. This
module asks Schwab's market-hours calendar (``client.market_hours``) for the
real session windows instead, so holidays, weekends and early closes are
honored. A fixed-window mode (:func:`run_during_hours`) remains for the cases
where a plain clock schedule is wanted.

.. code-block:: python

    plan = rustyschwab.schedule.Subscriptions()
    plan.add("LEVELONE_EQUITIES", ["AAPL", "MSFT"], fields=["bidPrice", "askPrice"])

    async def receiver(message):
        ...

    client = rustyschwab.Client.from_env()
    await rustyschwab.schedule.run_during_market_hours(client, plan, receiver=receiver)

Each session opens a new stream, so what to subscribe to is described by a
:class:`Subscriptions` (or by any async function that opens and subscribes
itself) and replayed every time. The SDK reconnects inside a session on its own;
the scheduler only opens, closes and, if the stream ends for good before the
session does, reopens it with a backoff.
"""

from __future__ import annotations

import asyncio
import logging
from dataclasses import dataclass
from datetime import date, datetime, time, timedelta, timezone, tzinfo
from typing import (
    TYPE_CHECKING,
    Any,
    AsyncIterable,
    Awaitable,
    Callable,
    Iterable,
    Mapping,
    Protocol,
    Sequence,
    TypeVar,
    Union,
)
from zoneinfo import ZoneInfo

from .streaming import OnError, Receiver, check_on_error, dispatch

if TYPE_CHECKING:
    from ._native import StreamMessage  # noqa: F401  (named in the string below)

__all__ = [
    "FixedSchedule",
    "MarketCalendar",
    "SessionWindow",
    "Subscriptions",
    "WindowSource",
    "parse_windows",
    "run_during_hours",
    "run_during_market_hours",
    "run_scheduled",
]

_log = logging.getLogger("rustyschwab.schedule")

#: The session names Schwab uses under ``sessionHours``, and the short forms accepted.
SESSION_ALIASES = {
    "pre": "preMarket",
    "premarket": "preMarket",
    "regular": "regularMarket",
    "regularmarket": "regularMarket",
    "post": "postMarket",
    "postmarket": "postMarket",
    "outcry": "outcryMarket",
    "outcrymarket": "outcryMarket",
}

#: The longest single sleep. A longer wait is taken in steps, so a clock that
#: jumps (a laptop that slept) is noticed within this time.
MAX_SLEEP = 3600.0

#: How long to wait before asking the calendar again when it shows no session
#: within the look-ahead.
IDLE_RECHECK = 6 * 3600.0

#: Weekdays as ``datetime.weekday()`` numbers: Monday is 0.
WEEKDAYS = (0, 1, 2, 3, 4)

_C = TypeVar("_C")
_H = TypeVar("_H", bound="HoursClient")
_DateArg = Union[date, str, None]
Clock = Callable[[], datetime]
Sleep = Callable[[float], Awaitable[None]]


# ---- session windows -----------------------------------------------------------------


@dataclass(frozen=True, order=True)
class SessionWindow:
    """A span of time the stream should be open: ``start`` to ``end``, both aware.

    ``sessions`` names the calendar sessions it covers, such as
    ``("preMarket", "regularMarket")`` when the two are back to back.
    """

    start: datetime
    end: datetime
    sessions: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        if self.start.tzinfo is None or self.end.tzinfo is None:
            raise ValueError("a session window needs timezone-aware bounds")
        if self.end <= self.start:
            raise ValueError("a session window must end after it starts")

    def contains(self, at: datetime) -> bool:
        """Whether ``at`` falls in ``[start, end)``."""
        return self.start <= at < self.end


def _instant(text: object) -> datetime | None:
    if not isinstance(text, str):
        return None
    try:
        # `fromisoformat` takes an offset but, before Python 3.11, not "Z".
        parsed = datetime.fromisoformat(text[:-1] + "+00:00" if text.endswith("Z") else text)
    except ValueError:
        return None
    return parsed if parsed.tzinfo is not None else None


def normalize_sessions(sessions: str | Iterable[str]) -> tuple[str, ...]:
    """``sessions`` as the calendar's names: ``"regular"`` becomes ``"regularMarket"``."""
    names = [sessions] if isinstance(sessions, str) else list(sessions)
    return tuple(SESSION_ALIASES.get(name.lower(), name) for name in names)


def _merge(windows: list[SessionWindow]) -> list[SessionWindow]:
    merged: list[SessionWindow] = []
    for window in sorted(windows):
        if merged and window.start <= merged[-1].end:
            last = merged[-1]
            names = last.sessions + tuple(n for n in window.sessions if n not in last.sessions)
            merged[-1] = SessionWindow(last.start, max(last.end, window.end), names)
        else:
            merged.append(window)
    return merged


def parse_windows(
    response: Mapping[str, Any],
    market: str = "equity",
    *,
    sessions: str | Iterable[str] = ("regularMarket",),
    product: str | None = None,
) -> list[SessionWindow]:
    """The session windows in a ``client.market_hours`` response, soonest first.

    ``response`` is Schwab's ``{"equity": {"EQ": {"isOpen", "sessionHours": ...}}}``.
    A market that is closed all day (``isOpen: false``, or no windows) gives an
    empty list. ``sessions`` picks which sessions count; windows that touch, such
    as pre-market ending where the regular session starts, are merged into one.
    ``product`` keeps a single product code (``"EQ"``); by default every
    product of the market counts. A window whose bounds do not parse is ignored.
    """
    wanted = normalize_sessions(sessions)
    by_market = {str(key).lower(): value for key, value in response.items()}
    products = by_market.get(market.lower())
    if not isinstance(products, Mapping):
        return []
    found: list[SessionWindow] = []
    for code, hours in products.items():
        if not isinstance(hours, Mapping) or (product is not None and code != product):
            continue
        if hours.get("isOpen") is False:
            continue
        session_hours = hours.get("sessionHours")
        if not isinstance(session_hours, Mapping):
            continue
        for name in wanted:
            for span in session_hours.get(name) or ():
                if not isinstance(span, Mapping):
                    continue
                start, end = _instant(span.get("start")), _instant(span.get("end"))
                if start is not None and end is not None and start < end:
                    found.append(SessionWindow(start, end, (name,)))
    return _merge(found)


class WindowSource(Protocol):
    """Something that knows when the stream should be open."""

    #: How many days before today to look for a window still under way, for
    #: sessions that begin the evening before.
    lookback: int

    async def windows(self, day: date) -> Sequence[SessionWindow]:
        """The windows that belong to ``day``, soonest first."""
        ...


class HoursClient(Protocol):
    """The part of :class:`rustyschwab.Client` the calendar needs."""

    async def market_hours(
        self, markets: Sequence[str], *, date: _DateArg = None
    ) -> dict[str, Any]: ...


class MarketCalendar:
    """Session windows from Schwab's market-hours endpoint, one request per day.

    ``market`` is ``"equity"``, ``"option"``, ``"future"``, ``"bond"`` or
    ``"forex"``. Answers are cached for the days the scheduler still needs, so
    the endpoint is asked once per day. ``lookback`` defaults to ``0`` for
    equities and options and ``1`` for the markets that trade overnight.
    """

    def __init__(
        self,
        client: HoursClient,
        market: str = "equity",
        *,
        sessions: str | Iterable[str] = ("regularMarket",),
        product: str | None = None,
        lookback: int | None = None,
    ) -> None:
        self.market = market.lower()
        self.sessions = normalize_sessions(sessions)
        self.product = product
        self.lookback = (
            lookback if lookback is not None else (0 if self.market in ("equity", "option") else 1)
        )
        self._client = client
        self._cache: dict[date, list[SessionWindow]] = {}

    async def windows(self, day: date) -> list[SessionWindow]:
        for cached in [d for d in self._cache if d < day - timedelta(days=self.lookback + 1)]:
            del self._cache[cached]
        if day not in self._cache:
            response = await self._client.market_hours([self.market], date=day)
            self._cache[day] = parse_windows(
                response, self.market, sessions=self.sessions, product=self.product
            )
        return self._cache[day]


class FixedSchedule:
    """A plain clock schedule: ``start_time`` to ``stop_time`` on ``on_days``.

    The same inputs as Schwabdev's ``start_auto``: ``on_days`` are
    ``datetime.weekday()`` numbers (Monday is ``0``) and the times are in
    ``tz``. It knows nothing of holidays or early closes. A ``stop_time`` at or
    before ``start_time`` means the window ends the next day.
    """

    def __init__(
        self,
        start_time: time = time(9, 29),
        stop_time: time = time(16, 0),
        on_days: Iterable[int] = WEEKDAYS,
        tz: str | tzinfo = "America/New_York",
    ) -> None:
        if start_time == stop_time:
            raise ValueError("start_time and stop_time must differ")
        self.start_time = start_time.replace(tzinfo=None)
        self.stop_time = stop_time.replace(tzinfo=None)
        self.on_days = frozenset(on_days)
        if not self.on_days <= set(range(7)):
            raise ValueError("on_days are weekday numbers 0 (Monday) to 6 (Sunday)")
        self.tz = _zone(tz)
        self.lookback = 1 if self.stop_time <= self.start_time else 0

    async def windows(self, day: date) -> list[SessionWindow]:
        if day.weekday() not in self.on_days:
            return []
        start = datetime.combine(day, self.start_time, tzinfo=self.tz)
        stop_day = day + timedelta(days=1) if self.stop_time <= self.start_time else day
        end = datetime.combine(stop_day, self.stop_time, tzinfo=self.tz)
        return [SessionWindow(start, end, ("fixed",))]


# ---- what to open ----------------------------------------------------------------------


class _Stream(AsyncIterable["StreamMessage"], Protocol):
    """What the scheduler needs of a stream: messages, and a way to close it."""

    async def close(self) -> None: ...


class _SubscribableStream(_Stream, Protocol):
    async def subscribe(
        self, service: str, keys: Sequence[str], *, fields: Sequence[int | str] | None = None
    ) -> None: ...


class _StreamClient(Protocol):
    async def open_stream(
        self, overrides: Mapping[str, Any] | None = None, *, named_fields: bool = True
    ) -> _SubscribableStream: ...


class Subscriptions:
    """A replayable description of what to stream.

    A new stream is opened for every session, so what to subscribe to is kept
    here and replayed each time. An instance is itself the ``start_stream``
    argument of the ``run_*`` functions.

    .. code-block:: python

        plan = Subscriptions(named_fields=True)
        plan.add("LEVELONE_EQUITIES", ["AAPL"], fields=["bidPrice", "askPrice"])
        plan.add("ACCT_ACTIVITY", ["Account Activity"])
    """

    def __init__(
        self,
        overrides: Mapping[str, Any] | None = None,
        *,
        named_fields: bool = True,
    ) -> None:
        self.overrides = overrides
        self.named_fields = named_fields
        self._items: list[tuple[str, list[str], list[int | str] | None]] = []

    def add(
        self,
        service: str,
        keys: Sequence[str],
        *,
        fields: Sequence[int | str] | None = None,
    ) -> Subscriptions:
        """Subscribe to ``keys`` on ``service`` in every stream this opens.

        Takes the same arguments as ``Stream.subscribe``. Returns ``self``.
        """
        if isinstance(keys, str):
            raise TypeError("keys must be a sequence of strings, not one string")
        self._items.append((service, list(keys), None if fields is None else list(fields)))
        return self

    def __bool__(self) -> bool:
        return bool(self._items)

    def __repr__(self) -> str:
        services = sorted({service for service, _, _ in self._items})
        return f"Subscriptions({', '.join(services)})"

    async def __call__(self, client: _StreamClient) -> _SubscribableStream:
        """Open a stream on ``client`` and subscribe to everything added."""
        if not self._items:
            _log.warning("no subscriptions; the stream will only carry state messages")
        stream = await client.open_stream(self.overrides, named_fields=self.named_fields)
        try:
            for service, keys, fields in self._items:
                await stream.subscribe(service, keys, fields=fields)
        except BaseException:
            await _close_quietly(stream)
            raise
        return stream


# ---- the scheduler -------------------------------------------------------------------------


class _Backoff:
    def __init__(self, initial: float, maximum: float) -> None:
        self._initial, self._maximum = initial, maximum
        self._next = initial

    def next(self) -> float:
        delay = self._next
        self._next = min(self._next * 2, self._maximum)
        return delay

    def reset(self) -> None:
        self._next = self._initial


def _zone(tz: str | tzinfo) -> tzinfo:
    return ZoneInfo(tz) if isinstance(tz, str) else tz


def _utc_now() -> datetime:
    return datetime.now(timezone.utc)


async def _close_quietly(stream: _Stream) -> None:
    try:
        await stream.close()
    except Exception:
        _log.exception("closing the stream failed")


class _Scheduler:
    def __init__(
        self,
        source: WindowSource,
        client: Any,
        start_stream: Callable[[Any], Awaitable[_Stream]],
        receiver: Receiver,
        *,
        receiver_kwargs: Mapping[str, Any] | None,
        lead: timedelta,
        tz: str | tzinfo,
        clock: Clock | None,
        sleep: Sleep | None,
        on_error: OnError,
        max_sessions: int | None,
        lookahead_days: int,
        retry_initial: float,
        retry_max: float,
        drain_timeout: float,
    ) -> None:
        check_on_error(on_error)
        if lead < timedelta(0):
            raise ValueError("lead must not be negative")
        if lookahead_days < 0 or retry_initial <= 0 or retry_max < retry_initial:
            raise ValueError("lookahead_days and the retry delays are out of range")
        self.source, self.client, self.start_stream = source, client, start_stream
        self.receiver, self.receiver_kwargs = receiver, dict(receiver_kwargs or {})
        self.lead, self.tz = lead, _zone(tz)
        self.clock = clock if clock is not None else _utc_now
        self.sleep: Sleep = sleep if sleep is not None else asyncio.sleep
        self.on_error, self.max_sessions = on_error, max_sessions
        self.lookahead_days = lookahead_days
        self.retry_initial, self.retry_max = retry_initial, retry_max
        self.drain_timeout = drain_timeout

    def now(self) -> datetime:
        now = self.clock()
        if now.tzinfo is None:
            raise ValueError("the clock must return timezone-aware datetimes")
        return now

    async def next_window(self, now: datetime) -> SessionWindow | None:
        """The window under way at ``now``, else the next one within the look-ahead."""
        today = now.astimezone(self.tz).date()
        for offset in range(-self.source.lookback, self.lookahead_days + 1):
            pending = [
                window
                for window in await self.source.windows(today + timedelta(days=offset))
                if window.end > now
            ]
            if pending:
                return min(pending)
        return None

    async def sleep_until(self, moment: datetime) -> None:
        while True:
            remaining = (moment - self.now()).total_seconds()
            if remaining <= 0:
                return
            await self.sleep(min(remaining, MAX_SLEEP))

    def local(self, moment: datetime) -> str:
        return moment.astimezone(self.tz).isoformat(timespec="minutes")

    async def run(self) -> int:
        completed = 0
        lookup_retry = _Backoff(self.retry_initial, self.retry_max)
        announced: SessionWindow | None = None
        while self.max_sessions is None or completed < self.max_sessions:
            now = self.now()
            try:
                window = await self.next_window(now)
            except Exception as error:
                delay = lookup_retry.next()
                _log.warning(
                    "market hours unavailable (%s: %s); retrying in %.0fs",
                    type(error).__name__,
                    error,
                    delay,
                )
                await self.sleep(delay)
                continue
            lookup_retry.reset()
            if window is None:
                _log.warning(
                    "no session in the next %d days; asking again in %.0f hours",
                    self.lookahead_days,
                    IDLE_RECHECK / 3600,
                )
                await self.sleep(IDLE_RECHECK)
                continue
            opens_at = window.start - self.lead
            if window != announced:
                announced = window
                _log.info(
                    "next session %s to %s (%s)",
                    self.local(window.start),
                    self.local(window.end),
                    ", ".join(window.sessions),
                )
            wait = (opens_at - now).total_seconds()
            if wait > 0:
                await self.sleep(min(wait, MAX_SLEEP))
                continue
            await self.run_window(window)
            completed += 1
        return completed

    async def run_window(self, window: SessionWindow) -> None:
        restart = _Backoff(self.retry_initial, self.retry_max)
        while self.now() < window.end:
            try:
                stream = await self.start_stream(self.client)
            except Exception as error:
                delay = restart.next()
                _log.warning(
                    "opening the stream failed (%s: %s); retrying in %.0fs",
                    type(error).__name__,
                    error,
                    delay,
                )
                await self.sleep(delay)
                continue
            _log.info("stream open for the session ending %s", self.local(window.end))
            opened = self.now()
            reader = asyncio.ensure_future(self.read(stream))
            timer = asyncio.ensure_future(self.sleep_until(window.end))
            try:
                await asyncio.wait({reader, timer}, return_when=asyncio.FIRST_COMPLETED)
                over = timer.done()
                if reader.done() and not reader.cancelled():
                    reader.result()  # a receiver error with on_error="raise"
            finally:
                timer.cancel()
                await _close_quietly(stream)
                if not reader.done():
                    # Closing ends the iteration after the final `ended` state
                    # message; let the receiver see it before cancelling.
                    await asyncio.wait({reader}, timeout=self.drain_timeout)
                    reader.cancel()
                await asyncio.gather(reader, timer, return_exceptions=True)
            if over:
                _log.info("session over; stream closed")
                return
            if (self.now() - opened).total_seconds() > self.retry_max:
                restart.reset()
            delay = restart.next()
            _log.warning("the stream ended before the session did; reopening in %.0fs", delay)
            await self.sleep(delay)

    async def read(self, stream: _Stream) -> None:
        await dispatch(
            stream,
            self.receiver,
            on_error=self.on_error,
            **self.receiver_kwargs,
        )


async def run_scheduled(
    source: WindowSource,
    client: _C,
    start_stream: Callable[[_C], Awaitable[_Stream]],
    *,
    receiver: Receiver,
    receiver_kwargs: Mapping[str, Any] | None = None,
    lead: timedelta = timedelta(0),
    tz: str | tzinfo = "America/New_York",
    clock: Clock | None = None,
    sleep: Sleep | None = None,
    on_error: OnError = "log",
    max_sessions: int | None = None,
    lookahead_days: int = 10,
    retry_initial: float = 5.0,
    retry_max: float = 300.0,
    drain_timeout: float = 2.0,
) -> int:
    """Run a stream for each window ``source`` yields, until cancelled.

    The general form behind :func:`run_during_market_hours` and
    :func:`run_during_hours`. Returns the number of sessions completed, which is
    only ever reached when ``max_sessions`` is given; otherwise the call runs
    until its task is cancelled, which closes the open stream.

    ``start_stream(client)`` is awaited at the start of each session and must
    return a subscribed stream (a :class:`Subscriptions` does this). Messages go
    to ``receiver(message, **receiver_kwargs)`` as in
    :func:`rustyschwab.streaming.dispatch`, and ``on_error`` has the same
    meaning, except that ``"raise"`` also ends the schedule after closing the
    stream. ``clock`` (a function returning an aware ``datetime``) and ``sleep``
    (a coroutine function taking seconds) default to the wall clock and
    ``asyncio.sleep``; tests replace them.
    """
    scheduler = _Scheduler(
        source,
        client,
        start_stream,
        receiver,
        receiver_kwargs=receiver_kwargs,
        lead=lead,
        tz=tz,
        clock=clock,
        sleep=sleep,
        on_error=on_error,
        max_sessions=max_sessions,
        lookahead_days=lookahead_days,
        retry_initial=retry_initial,
        retry_max=retry_max,
        drain_timeout=drain_timeout,
    )
    return await scheduler.run()


async def run_during_market_hours(
    client: _H,
    start_stream: Callable[[_H], Awaitable[_Stream]],
    *,
    receiver: Receiver,
    receiver_kwargs: Mapping[str, Any] | None = None,
    market: str = "equity",
    sessions: str | Iterable[str] = ("regularMarket",),
    product: str | None = None,
    lead: timedelta = timedelta(minutes=1),
    tz: str | tzinfo = "America/New_York",
    clock: Clock | None = None,
    sleep: Sleep | None = None,
    on_error: OnError = "log",
    max_sessions: int | None = None,
    lookahead_days: int = 10,
    retry_initial: float = 5.0,
    retry_max: float = 300.0,
    drain_timeout: float = 2.0,
) -> int:
    """Stream during the market's real sessions: open, dispatch, close, repeat.

    Reads each day's windows from ``client.market_hours`` (one request a day),
    sleeps until ``lead`` before the next window opens, opens a stream with
    ``start_stream(client)``, hands every message to ``receiver`` and closes the
    stream when the window ends. Closed days (weekends, holidays) have no window
    and are skipped; an early close ends the window early.

    ``sessions`` chooses which calendar sessions count: ``"regularMarket"`` by
    default, or any of ``"preMarket"``, ``"postMarket"``; adjacent sessions form
    one window, so the stream stays open across them. ``market`` is
    ``"equity"``, ``"option"``, ``"future"``, ``"bond"`` or ``"forex"``, and
    ``product`` narrows it to one product code. ``tz`` decides which day is
    "today".

    If the calendar cannot be read, the call retries after ``retry_initial``
    seconds, doubling up to ``retry_max``, and logs each failure to
    ``rustyschwab.schedule``. If the stream cannot be opened, or ends for good
    before the window does, it is reopened the same way. Returns the number of
    sessions completed once ``max_sessions`` is reached; without it the call runs
    until cancelled. Other parameters are described on :func:`run_scheduled`.
    """
    calendar = MarketCalendar(client, market, sessions=sessions, product=product)
    return await run_scheduled(
        calendar,
        client,
        start_stream,
        receiver=receiver,
        receiver_kwargs=receiver_kwargs,
        lead=lead,
        tz=tz,
        clock=clock,
        sleep=sleep,
        on_error=on_error,
        max_sessions=max_sessions,
        lookahead_days=lookahead_days,
        retry_initial=retry_initial,
        retry_max=retry_max,
        drain_timeout=drain_timeout,
    )


async def run_during_hours(
    client: _C,
    start_stream: Callable[[_C], Awaitable[_Stream]],
    *,
    receiver: Receiver,
    receiver_kwargs: Mapping[str, Any] | None = None,
    start_time: time = time(9, 29),
    stop_time: time = time(16, 0),
    on_days: Iterable[int] = WEEKDAYS,
    tz: str | tzinfo = "America/New_York",
    lead: timedelta = timedelta(0),
    clock: Clock | None = None,
    sleep: Sleep | None = None,
    on_error: OnError = "log",
    max_sessions: int | None = None,
    retry_initial: float = 5.0,
    retry_max: float = 300.0,
    drain_timeout: float = 2.0,
) -> int:
    """Stream between fixed clock times, as Schwabdev's ``start_auto`` does.

    The defaults match it: 09:29 to 16:00 New York time, Monday to Friday. It
    does not know holidays or early closes; prefer :func:`run_during_market_hours`.
    Other parameters are described on :func:`run_scheduled`.
    """
    schedule = FixedSchedule(start_time, stop_time, on_days, tz)
    return await run_scheduled(
        schedule,
        client,
        start_stream,
        receiver=receiver,
        receiver_kwargs=receiver_kwargs,
        lead=lead,
        tz=schedule.tz,
        clock=clock,
        sleep=sleep,
        on_error=on_error,
        max_sessions=max_sessions,
        lookahead_days=8,
        retry_initial=retry_initial,
        retry_max=retry_max,
        drain_timeout=drain_timeout,
    )
