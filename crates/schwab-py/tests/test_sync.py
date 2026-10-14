"""The blocking facade: parity with the async client, the loop thread, streams and schedules."""

from __future__ import annotations

import ast
import asyncio
import inspect
import json
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Callable, Iterator
from urllib.parse import parse_qs, urlsplit

import pytest
from conftest import (
    ACCESS_TOKEN,
    ACCOUNT_HASH,
    MockSchwab,
    client_config,
    limit_order,
)
from test_stream import Streamer, advertise, level_one
from websockets.asyncio.server import serve

import rustyschwab
from rustyschwab import _native, errors, sync
from rustyschwab.schedule import Subscriptions

ACCOUNT_PATH = f"/trader/v1/accounts/{ACCOUNT_HASH}"


@pytest.fixture(autouse=True)
def private_loop() -> Iterator[None]:
    """Each test starts with no sync loop thread and leaves none behind."""
    sync.shutdown()
    yield
    sync.shutdown()
    leftovers = [t.name for t in threading.enumerate() if t.name.startswith("rustyschwab-")]
    assert leftovers == []


@pytest.fixture
def blocking(server: MockSchwab, tokens: Path) -> Iterator[sync.Client]:
    client = sync.Client(client_config(server, tokens))
    yield client
    client.close()


# ---- parity with the async client -----------------------------------------------------------


def stub_methods() -> dict[str, ast.AsyncFunctionDef | ast.FunctionDef]:
    """The public methods of ``Client`` in the native module's type stub."""
    stub = Path(_native.__file__).with_name("_native.pyi")
    tree = ast.parse(stub.read_text())
    (cls,) = [n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "Client"]
    return {
        node.name: node
        for node in cls.body
        if isinstance(node, (ast.AsyncFunctionDef, ast.FunctionDef))
        and not node.name.startswith("_")
    }


def test_the_sync_client_has_a_twin_for_every_public_async_method() -> None:
    expected = stub_methods()
    assert len(expected) > 30, "the stub was not read"
    live = {n for n in dir(rustyschwab.Client) if not n.startswith("_")}
    assert live == set(expected), "the stub and the compiled class disagree"

    missing = sorted(name for name in expected if not hasattr(sync.Client, name))
    assert missing == [], f"rustyschwab.sync.Client lacks: {missing}"

    for name, node in expected.items():
        twin = getattr(sync.Client, name)
        assert not inspect.iscoroutinefunction(twin), f"sync.Client.{name} must block"
        if isinstance(node, ast.AsyncFunctionDef):
            assert twin.__doc__, f"sync.Client.{name} has no docstring"


def test_twin_signatures_match_the_native_ones() -> None:
    expected = stub_methods()
    for name, node in expected.items():
        native = inspect.signature(getattr(rustyschwab.Client, name))
        twin = inspect.signature(getattr(sync.Client, name))
        native_params = [(p.name, p.kind, p.default) for p in native.parameters.values()]
        twin_params = [(p.name, p.kind, p.default) for p in twin.parameters.values()]
        # The compiled signature writes `self, /`; compare the rest. The
        # constructor adds `timeout` and `call_on_auth` after the native ones.
        native_params = [p for p in native_params if p[0] != "self"]
        twin_params = [p for p in twin_params if p[0] != "self"]
        if name == "from_env":
            twin_params = twin_params[: len(native_params)]
        assert native_params == twin_params, f"{name} differs from the native signature"
        if name not in ("open_stream", "from_env"):  # these return the blocking classes
            ret = ast.unparse(node.returns) if node.returns else None
            assert twin.return_annotation == ret, f"{name} return type differs from the stub"
        stub_args = [
            (a.arg, ast.unparse(a.annotation))
            for a in [*node.args.args, *node.args.kwonlyargs]
            if a.arg != "self" and a.annotation is not None
        ]
        twin_args = [
            (p.name, p.annotation) for p in twin.parameters.values() if p.name != "self"
        ]
        assert twin_args[: len(stub_args)] == stub_args, f"{name} parameter types differ from the stub"
        if name != "from_env":
            assert len(twin_args) == len(stub_args)


def test_every_async_stream_method_has_a_blocking_twin() -> None:
    for name in (n for n in dir(rustyschwab.Stream) if not n.startswith("_")):
        assert hasattr(sync.Stream, name), f"sync.Stream lacks {name}"
        assert not inspect.iscoroutinefunction(getattr(sync.Stream, name))


def test_constructors_and_context_manager_exist() -> None:
    assert callable(sync.Client.from_env) and callable(sync.Client.__enter__)
    params = inspect.signature(sync.Client.from_env).parameters
    assert list(params)[:2] == ["overrides", "timeout"] or "overrides" in params


# ---- calls ----------------------------------------------------------------------------------------


def test_calls_block_and_return_the_async_result(blocking: sync.Client, server: MockSchwab) -> None:
    server.on("GET", "/marketdata/v1/quotes", {"AAPL": {"symbol": "AAPL", "quote": {"lastPrice": 1.5}}})
    quotes = blocking.quotes(["AAPL"], fields=["quote"])
    assert quotes["AAPL"]["quote"]["lastPrice"] == 1.5
    (request,) = server.calls("GET", "/marketdata/v1/quotes")
    assert request.headers["authorization"] == f"Bearer {ACCESS_TOKEN}"
    assert request.query["fields"] == ["quote"]


def test_errors_are_the_same_exceptions(blocking: sync.Client, server: MockSchwab) -> None:
    with pytest.raises(errors.ValidationError, match="quote field"):
        blocking.quotes(["AAPL"], fields=["nonsense"])
    server.on("GET", "/marketdata/v1/movers/$SPX", {"message": "nope"}, status=400)
    with pytest.raises(errors.SchwabError) as caught:
        blocking.movers("$SPX")
    assert caught.value.kind in ("broker", "http", "api")


def test_synchronous_methods_pass_through(blocking: sync.Client) -> None:
    assert blocking.circuit_state() in ("closed", "Closed")
    assert blocking.request_budget() is None or "perMinute" in (blocking.request_budget() or {})


def test_orders_and_controls(blocking: sync.Client, server: MockSchwab) -> None:
    server.on("POST", f"{ACCOUNT_PATH}/orders", status=201, headers={"Location": "/orders/9"})
    placed = blocking.place_order(ACCOUNT_HASH, limit_order())
    assert placed["order_id"] == "9" and placed["attempt_id"]
    with pytest.raises(errors.ValidationError):
        blocking.place_order(ACCOUNT_HASH, {"bogus": 1})
    blocking.halt("testing", "ops")
    assert blocking.halted() == "testing"
    blocking.clear_halt("ops")
    assert blocking.halted() is None


def test_calls_from_many_threads_share_the_loop(blocking: sync.Client, server: MockSchwab) -> None:
    server.on("GET", "/marketdata/v1/AAPL/quotes", {"AAPL": {"symbol": "AAPL"}})
    with ThreadPoolExecutor(8) as pool:
        results = list(pool.map(lambda _: blocking.quote("AAPL")["symbol"], range(40)))
    assert results == ["AAPL"] * 40
    assert sum(t.name == "rustyschwab-sync" for t in threading.enumerate()) == 1


def test_a_call_timeout_cancels_and_the_client_stays_usable(
    server: MockSchwab, tokens: Path
) -> None:
    server.on("GET", "/marketdata/v1/quotes", {"AAPL": {}}, delay=1.5)
    client = sync.Client(client_config(server, tokens), timeout=0.2)
    started = time.monotonic()
    with pytest.raises(TimeoutError):
        client.quotes(["AAPL"])
    assert time.monotonic() - started < 1.2
    server.on("GET", "/marketdata/v1/quotes", {"AAPL": {"symbol": "AAPL"}})
    assert client.quotes(["AAPL"])["AAPL"]["symbol"] == "AAPL"


def test_context_manager_loads_tokens_and_closes_streams(
    server: MockSchwab, tokens: Path
) -> None:
    with sync.Client(client_config(server, tokens)) as client:
        server.on("GET", "/trader/v1/accounts/accountNumbers", [])
        assert client.account_numbers() == []
    client.close()  # idempotent


def test_a_blocking_call_on_the_loop_thread_is_refused_not_deadlocked(
    blocking: sync.Client,
) -> None:
    async def misuse() -> None:
        blocking.circuit_state()  # passes through: fine
        blocking.init()  # would wait on its own loop

    with pytest.raises(RuntimeError, match="own thread"):
        sync._runner().run(misuse())


def test_shutdown_is_idempotent_and_the_loop_restarts_on_demand(
    blocking: sync.Client, server: MockSchwab
) -> None:
    server.on("GET", "/marketdata/v1/AAPL/quotes", {"AAPL": {"symbol": "AAPL"}})
    blocking.quote("AAPL")
    assert any(t.name == "rustyschwab-sync" for t in threading.enumerate())
    sync.shutdown()
    sync.shutdown()
    assert not any(t.name == "rustyschwab-sync" for t in threading.enumerate())
    assert blocking.quote("AAPL")["symbol"] == "AAPL"
    assert any(t.name == "rustyschwab-sync" for t in threading.enumerate())


def test_from_env_builds_from_overrides(
    monkeypatch: pytest.MonkeyPatch, server: MockSchwab, tokens: Path
) -> None:
    monkeypatch.setenv("SCHWAB_APP_KEY", "K" * 32)
    monkeypatch.setenv("SCHWAB_APP_SECRET", "S" * 16)
    client = sync.Client.from_env(client_config(server, tokens), timeout=5)
    assert client.timeout == 5
    server.on("GET", "/trader/v1/accounts/accountNumbers", [])
    assert client.account_numbers() == []
    assert "sync.Client" in repr(client)


# ---- reauthorization hook ---------------------------------------------------------------------------


def authorizing_hook(seen: list[str], code: str = "C" * 60) -> Callable[[str], str | None]:
    def hook(url: str) -> str | None:
        seen.append(url)
        state = parse_qs(urlsplit(url).query)["state"][0]
        return f"https://127.0.0.1:8080/?code={code}&session=x&state={state}"

    return hook


def token_reply(server: MockSchwab) -> None:
    server.on(
        "POST",
        "/oauth/token",
        {
            "access_token": "fresh-access",
            "refresh_token": "fresh-refresh",
            "expires_in": 1800,
            "token_type": "Bearer",
            "scope": "api",
        },
    )


def test_call_on_auth_runs_the_flow_and_retries_the_call(
    server: MockSchwab, tokens: Path
) -> None:
    tokens.unlink()
    token_reply(server)
    server.on("GET", "/trader/v1/accounts/accountNumbers", [{"accountNumber": "1", "hashValue": "H"}])
    seen: list[str] = []
    client = sync.Client(client_config(server, tokens), call_on_auth=authorizing_hook(seen))
    assert client.account_numbers() == [{"accountNumber": "1", "hashValue": "H"}]
    assert len(seen) == 1 and seen[0].startswith("https://api.schwabapi.com/")
    (token_request,) = server.calls("POST", "/oauth/token")
    assert b"authorization_code" in token_request.body
    (call,) = server.calls("GET", "/trader/v1/accounts/accountNumbers")
    assert call.headers["authorization"] == "Bearer fresh-access"
    # Authorized now: a later call does not prompt again.
    client.account_numbers()
    assert len(seen) == 1


def test_the_hook_is_not_used_for_orders(server: MockSchwab, tokens: Path) -> None:
    tokens.unlink()
    seen: list[str] = []
    client = sync.Client(client_config(server, tokens), call_on_auth=authorizing_hook(seen))
    with pytest.raises(errors.AuthError) as caught:
        client.place_order(ACCOUNT_HASH, limit_order())
    assert caught.value.reauthorization_required and seen == []
    assert server.calls("POST", f"{ACCOUNT_PATH}/orders") == []


def test_a_hook_that_gives_up_leaves_the_original_error(server: MockSchwab, tokens: Path) -> None:
    tokens.unlink()
    client = sync.Client(client_config(server, tokens), call_on_auth=lambda url: None)
    with pytest.raises(errors.AuthError) as caught:
        client.account_numbers()
    assert caught.value.reauthorization_required


def test_without_a_hook_reauthorization_is_just_an_error(server: MockSchwab, tokens: Path) -> None:
    tokens.unlink()
    client = sync.Client(client_config(server, tokens))
    with pytest.raises(errors.AuthError):
        client.account_numbers()


def test_authorize_is_explicit_and_returns_whether_it_completed(
    server: MockSchwab, tokens: Path
) -> None:
    tokens.unlink()
    token_reply(server)
    client = sync.Client(client_config(server, tokens))
    assert client.authorize(lambda url: None) is False
    assert client.authorize(authorizing_hook([])) is True
    assert tokens.exists()


def test_one_prompt_serves_concurrent_callers(server: MockSchwab, tokens: Path) -> None:
    tokens.unlink()
    token_reply(server)
    server.on("GET", "/trader/v1/accounts/accountNumbers", [])
    seen: list[str] = []
    client = sync.Client(client_config(server, tokens), call_on_auth=authorizing_hook(seen))
    with ThreadPoolExecutor(4) as pool:
        list(pool.map(lambda _: client.account_numbers(), range(4)))
    assert len(seen) == 1


# ---- streams ------------------------------------------------------------------------------------------


class StreamerThread:
    """A ``Streamer`` served from its own thread, since these tests have no event loop."""

    def __init__(self) -> None:
        self.streamer = Streamer()
        self._loop = asyncio.new_event_loop()
        self._ready = threading.Event()
        self._stop: asyncio.Event | None = None
        self._thread = threading.Thread(target=self._run, name="test-streamer", daemon=True)
        self._thread.start()
        self._ready.wait(10)

    @property
    def url(self) -> str:
        return self.streamer.url

    def _run(self) -> None:
        asyncio.set_event_loop(self._loop)
        self._loop.run_until_complete(self._serve())
        self._loop.close()

    async def _serve(self) -> None:
        self._stop = asyncio.Event()
        async with serve(self.streamer.handle, "127.0.0.1", 0) as listener:
            self.streamer.url = f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}/ws"
            self._ready.set()
            await self._stop.wait()

    def push(self, frame: dict[str, Any]) -> None:
        asyncio.run_coroutine_threadsafe(self.streamer.push(frame), self._loop).result(5)

    def close(self) -> None:
        assert self._stop is not None
        self._loop.call_soon_threadsafe(self._stop.set)
        self._thread.join(10)


@pytest.fixture
def streamer() -> Iterator[StreamerThread]:
    thread = StreamerThread()
    yield thread
    thread.close()


def wait_for(condition: Callable[[], bool], timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while not condition():
        assert time.monotonic() < deadline, "timed out waiting"
        time.sleep(0.005)


def next_of_type(stream: sync.Stream, kind: str) -> dict[str, Any]:
    for _ in range(20):
        message = stream.recv(5)
        assert message is not None
        if message["type"] == kind:
            return dict(message)
    raise AssertionError(f"no {kind} message")


def test_iterating_and_the_blocking_session_methods(
    blocking: sync.Client, server: MockSchwab, streamer: StreamerThread
) -> None:
    advertise(server, streamer.url)
    stream = blocking.open_stream()
    assert isinstance(stream, sync.Stream) and stream.is_authenticated
    assert next_of_type(stream, "state")["ready"] is True
    stream.subscribe("LEVELONE_EQUITIES", ["AAPL"], fields=["key", "bidPrice"])
    assert stream.subscriptions() == {"LEVELONE_EQUITIES": ["AAPL"]}
    streamer.push(level_one(**{"1": 5.5}))
    assert next_of_type(stream, "data")["data"][0]["content"] == [{"key": "AAPL", "bidPrice": 5.5}]
    stream.view("LEVELONE_EQUITIES", ["bidPrice", "askPrice"])
    assert streamer.streamer.sent("VIEW")
    assert stream.status()["ready"] is True
    stream.unsubscribe("LEVELONE_EQUITIES", ["AAPL"])
    assert stream.subscriptions() == {}
    stream.close()
    stream.close()  # idempotent
    tail = list(stream)
    assert tail[-1]["ended"] == {"reason": "disconnected"}
    assert list(stream) == [] and stream.recv() is None
    assert not stream.is_authenticated


def test_recv_timeout_leaves_the_stream_usable(
    blocking: sync.Client, server: MockSchwab, streamer: StreamerThread
) -> None:
    advertise(server, streamer.url)
    with blocking.open_stream() as stream:
        next_of_type(stream, "state")
        with pytest.raises(TimeoutError):
            stream.recv(0.2)
        stream.subscribe("LEVELONE_EQUITIES", ["AAPL"])
        streamer.push(level_one(**{"1": 1.0}))
        assert next_of_type(stream, "data")["type"] == "data"


def test_start_calls_the_receiver_on_a_background_thread(
    blocking: sync.Client, server: MockSchwab, streamer: StreamerThread
) -> None:
    advertise(server, streamer.url)
    stream = blocking.open_stream()
    got: list[tuple[str, str, str]] = []
    threads: set[str] = set()

    def receiver(message: dict[str, Any], *, label: str) -> None:
        threads.add(threading.current_thread().name)
        if message["type"] == "data":
            got.append((label, message["data"][0]["service"], str(message["data"][0]["content"][0]["bidPrice"])))

    stream.subscribe("LEVELONE_EQUITIES", ["AAPL"], fields=["bidPrice"])
    stream.start(receiver, label="L")
    assert stream.running
    streamer.push(level_one(**{"1": 7.5}))
    wait_for(lambda: bool(got))
    assert got == [("L", "LEVELONE_EQUITIES", "7.5")]
    assert threads == {"rustyschwab-stream"}
    stream.stop()
    stream.stop()  # idempotent
    assert not stream.running
    # The connection survives stop(), and a restart resumes delivery.
    stream.start(receiver, label="M")
    streamer.push(level_one(**{"1": 8.5}))
    wait_for(lambda: len(got) == 2)
    assert got[1][0] == "M"
    stream.close()


def test_start_guards(
    blocking: sync.Client, server: MockSchwab, streamer: StreamerThread
) -> None:
    advertise(server, streamer.url)
    stream = blocking.open_stream()
    with pytest.raises(TypeError, match="coroutine"):
        stream.start(asyncio.sleep)  # type: ignore[arg-type]
    with pytest.raises(ValueError, match="on_error"):
        stream.start(print, on_error="ignore")  # type: ignore[arg-type]
    stream.start(lambda message: None)
    with pytest.raises(RuntimeError, match="already started"):
        stream.start(lambda message: None)
    with pytest.raises(RuntimeError, match="start"):
        stream.recv(0.1)
    with pytest.raises(RuntimeError, match="start"):
        next(iter(stream))
    stream.close()
    with pytest.raises(RuntimeError, match="closed"):
        stream.start(lambda message: None)


def test_receiver_errors_are_logged_and_delivery_continues(
    blocking: sync.Client,
    server: MockSchwab,
    streamer: StreamerThread,
    caplog: pytest.LogCaptureFixture,
) -> None:
    advertise(server, streamer.url)
    stream = blocking.open_stream()
    got: list[float] = []

    def receiver(message: dict[str, Any]) -> None:
        if message["type"] != "data":
            return
        value = message["data"][0]["content"][0]["bidPrice"]
        if value < 0:
            raise ValueError("negative price")
        got.append(value)

    stream.subscribe("LEVELONE_EQUITIES", ["AAPL"], fields=["bidPrice"])
    with caplog.at_level("ERROR", logger="rustyschwab.sync"):
        stream.start(receiver)
        streamer.push(level_one(**{"1": -1.0}))
        streamer.push(level_one(**{"1": 2.0}))
        wait_for(lambda: got == [2.0])
        assert stream.running
        stream.stop()
    assert "ValueError: negative price" in caplog.text
    assert stream.error is None
    stream.close()


def test_on_error_raise_stops_the_thread_and_surfaces_the_exception_once(
    blocking: sync.Client, server: MockSchwab, streamer: StreamerThread
) -> None:
    advertise(server, streamer.url)
    stream = blocking.open_stream()

    def receiver(message: dict[str, Any]) -> None:
        if message["type"] == "data":
            raise ValueError("fatal")

    stream.subscribe("LEVELONE_EQUITIES", ["AAPL"])
    stream.start(receiver, on_error="raise")
    streamer.push(level_one(**{"1": 1.0}))
    wait_for(lambda: not stream.running)
    assert isinstance(stream.error, ValueError)
    with pytest.raises(ValueError, match="fatal"):
        stream.stop()
    stream.stop()  # raised once; now quiet
    assert stream.error is None
    stream.close()


def test_stop_from_inside_the_receiver_does_not_deadlock(
    blocking: sync.Client, server: MockSchwab, streamer: StreamerThread
) -> None:
    advertise(server, streamer.url)
    stream = blocking.open_stream()
    seen: list[str] = []

    def receiver(message: dict[str, Any]) -> None:
        seen.append(message["type"])
        stream.stop()

    stream.start(receiver)
    wait_for(lambda: not stream.running)
    assert seen == ["state"]
    stream.close()


def test_client_close_stops_its_streams(
    server: MockSchwab, tokens: Path, streamer: StreamerThread
) -> None:
    advertise(server, streamer.url)
    client = sync.Client(client_config(server, tokens))
    stream = client.open_stream()
    stream.start(lambda message: None)
    client.close()
    assert not stream.running and not stream.is_authenticated


def test_shutdown_stops_started_streams(
    blocking: sync.Client, server: MockSchwab, streamer: StreamerThread
) -> None:
    advertise(server, streamer.url)
    stream = blocking.open_stream()
    stream.start(lambda message: None)
    sync.shutdown()
    assert not stream.running and not stream.is_authenticated


# ---- schedules ------------------------------------------------------------------------------------------


def hours_around_now(server: MockSchwab, *, seconds: float) -> None:
    """A calendar whose regular session started a minute ago and ends in ``seconds``."""
    now = datetime.now(timezone.utc)
    start = (now - timedelta(minutes=1)).isoformat(timespec="seconds")
    end = (now + timedelta(seconds=seconds)).isoformat(timespec="seconds")
    server.on(
        "GET",
        "/marketdata/v1/markets",
        {
            "equity": {
                "EQ": {
                    "isOpen": True,
                    "sessionHours": {"regularMarket": [{"start": start, "end": end}]},
                }
            }
        },
    )


def test_a_market_hours_schedule_streams_for_the_session_and_closes(
    blocking: sync.Client, server: MockSchwab, streamer: StreamerThread
) -> None:
    advertise(server, streamer.url)
    hours_around_now(server, seconds=2.5)
    got: list[dict[str, Any]] = []
    threads: set[str] = set()

    def receiver(message: dict[str, Any]) -> None:
        threads.add(threading.current_thread().name)
        got.append(message)

    plan = Subscriptions().add("LEVELONE_EQUITIES", ["AAPL"], fields=["bidPrice"])
    schedule = blocking.run_during_market_hours(plan, receiver, max_sessions=1)
    assert schedule.running
    wait_for(lambda: bool(streamer.streamer.sent("ADD")))
    streamer.push(level_one(**{"1": 3.5}))
    wait_for(lambda: any(m["type"] == "data" for m in got))
    assert schedule.join(15) == 1
    assert not schedule.running
    # The session ended: the stream logged out and the receiver saw it end.
    assert streamer.streamer.sent("LOGOUT")
    assert got[-1]["type"] == "state" and got[-1]["ended"] == {"reason": "disconnected"}
    assert threads == {"rustyschwab-receiver_0"}
    (request,) = (r for r in server.requests if r.path == "/marketdata/v1/markets")
    assert request.query["markets"] == ["equity"] and "date" in request.query


def test_a_schedule_can_be_stopped_and_closes_its_stream(
    blocking: sync.Client, server: MockSchwab, streamer: StreamerThread
) -> None:
    advertise(server, streamer.url)
    hours_around_now(server, seconds=600)
    schedule = blocking.run_during_market_hours(
        Subscriptions().add("LEVELONE_EQUITIES", ["AAPL"]), lambda message: None
    )
    wait_for(lambda: bool(streamer.streamer.sent("ADD")))
    schedule.stop(10)
    schedule.stop(10)  # idempotent
    assert not schedule.running and schedule.join() is None
    assert streamer.streamer.sent("LOGOUT")


def test_a_function_can_open_the_blocking_stream(
    blocking: sync.Client, server: MockSchwab, streamer: StreamerThread
) -> None:
    advertise(server, streamer.url)
    hours_around_now(server, seconds=2)
    opened: list[sync.Stream] = []

    def open_it(client: sync.Client) -> sync.Stream:
        stream = client.open_stream()
        stream.subscribe("LEVELONE_EQUITIES", ["MSFT"])
        opened.append(stream)
        return stream

    with blocking.run_during_market_hours(open_it, lambda m: None, max_sessions=1) as schedule:
        assert schedule.join(15) == 1
    assert len(opened) == 1 and not opened[0].is_authenticated


def test_a_fixed_hours_schedule(
    blocking: sync.Client, server: MockSchwab, streamer: StreamerThread
) -> None:
    now = datetime.now(timezone.utc)
    if now.hour == 23 and now.minute == 59 or now.hour == 0 and now.minute == 0:
        pytest.skip("too close to midnight UTC for a same-day window")
    advertise(server, streamer.url)
    start = (now - timedelta(minutes=1)).time().replace(microsecond=0)
    stop = (now + timedelta(seconds=2)).time().replace(microsecond=0)
    schedule = blocking.run_during_hours(
        Subscriptions().add("LEVELONE_EQUITIES", ["AAPL"]),
        lambda message: None,
        start_time=start,
        stop_time=stop,
        on_days=range(7),
        tz="UTC",
        max_sessions=1,
    )
    assert schedule.join(15) == 1
    assert server.calls("GET", "/marketdata/v1/markets") == []
    assert streamer.streamer.sent("ADD")


def test_schedule_errors_surface_from_join(
    blocking: sync.Client, server: MockSchwab, streamer: StreamerThread
) -> None:
    advertise(server, streamer.url)
    hours_around_now(server, seconds=600)

    def receiver(message: dict[str, Any]) -> None:
        raise ValueError("fatal receiver")

    schedule = blocking.run_during_market_hours(
        Subscriptions().add("LEVELONE_EQUITIES", ["AAPL"]),
        receiver,
        on_error="raise",
    )
    with pytest.raises(ValueError, match="fatal receiver"):
        schedule.join(15)
    schedule.join()  # reported once
    assert streamer.streamer.sent("LOGOUT")


def test_schedule_rejects_coroutine_receivers(blocking: sync.Client) -> None:
    async def receiver(message: dict[str, Any]) -> None: ...

    with pytest.raises(TypeError, match="coroutine"):
        blocking.run_during_market_hours(Subscriptions(), receiver)


def test_json_round_trips_through_the_loop(blocking: sync.Client, server: MockSchwab) -> None:
    server.on("GET", "/marketdata/v1/anything", {"ok": [1, 2, {"a": None}]})
    assert json.dumps(blocking.get_json("/marketdata/v1/anything")) == '{"ok": [1, 2, {"a": null}]}'
