"""asyncio integration: coroutines, concurrency, cancellation, the GIL."""

from __future__ import annotations

import asyncio
import inspect
import threading
import time

import pytest
from conftest import MockSchwab

import rustyschwab
from rustyschwab import errors

QUOTE = {"AAPL": {"assetMainType": "EQUITY", "symbol": "AAPL", "quote": {"lastPrice": 1.0}}}


async def test_calls_are_real_coroutines(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", "/marketdata/v1/quotes", QUOTE)
    call = client.quotes(["AAPL"])
    assert inspect.iscoroutine(call)
    # create_task needs a coroutine; a bare asyncio.Future would be refused.
    task = asyncio.create_task(call)
    assert (await task)["AAPL"]["symbol"] == "AAPL"


async def test_nothing_is_sent_until_a_call_is_awaited(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", "/marketdata/v1/quotes", QUOTE)
    call = client.quotes(["AAPL"])
    await asyncio.sleep(0.2)
    assert server.requests == []
    await call
    assert len(server.requests) == 1


async def test_a_call_can_be_awaited_only_once(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", "/marketdata/v1/quotes", QUOTE)
    call = client.quotes(["AAPL"])
    await call
    with pytest.raises(RuntimeError):
        await call


async def test_requests_overlap_on_the_shared_runtime(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", "/marketdata/v1/quotes", QUOTE, delay=0.3)
    started = time.monotonic()
    results = await asyncio.gather(*(client.quotes(["AAPL"]) for _ in range(20)))
    elapsed = time.monotonic() - started
    assert len(results) == 20
    # Serial would be six seconds; the event loop and the runtime both stay free.
    assert elapsed < 3.0


async def test_the_gil_is_free_while_a_request_is_in_flight(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", "/marketdata/v1/quotes", QUOTE, delay=0.5)
    ticks = 0
    stop = threading.Event()

    def spin() -> None:
        nonlocal ticks
        while not stop.is_set():
            ticks += 1
            time.sleep(0.005)

    worker = threading.Thread(target=spin)
    worker.start()
    try:
        await client.quotes(["AAPL"])
    finally:
        stop.set()
        worker.join()
    assert ticks > 20


async def test_cancelling_a_call_returns_promptly_and_leaves_the_client_usable(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", "/marketdata/v1/quotes", QUOTE, delay=1.0)
    task = asyncio.create_task(client.quotes(["AAPL"]))
    await asyncio.sleep(0.1)
    started = time.monotonic()
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    assert time.monotonic() - started < 0.5
    server.on("GET", "/marketdata/v1/quotes", QUOTE)
    assert (await client.quotes(["AAPL"]))["AAPL"]["symbol"] == "AAPL"


async def test_wait_for_times_out_a_slow_call(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", "/marketdata/v1/quotes", QUOTE, delay=1.0)
    with pytest.raises(asyncio.TimeoutError):
        await asyncio.wait_for(client.quotes(["AAPL"]), timeout=0.2)


async def test_the_client_timeout_raises_timeout(
    server: MockSchwab, make_client: object
) -> None:
    client = make_client(client={"timeout": 0.2, "retry": {"max_retries": 0}})  # type: ignore[operator]
    server.on("GET", "/marketdata/v1/quotes", QUOTE, delay=1.0)
    with pytest.raises((errors.Timeout, errors.NetworkError)) as caught:
        await client.quotes(["AAPL"])
    assert caught.value.retryable is True


async def test_async_context_manager_initializes_the_client(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", "/marketdata/v1/quotes", QUOTE)
    async with client as entered:
        assert entered is client
        await entered.quotes(["AAPL"])


async def test_exceptions_can_be_gathered(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", "/marketdata/v1/quotes", QUOTE)
    results = await asyncio.gather(
        client.quotes(["AAPL"]),
        client.quotes([""]),
        return_exceptions=True,
    )
    assert isinstance(results[0], dict)
    assert isinstance(results[1], errors.InvalidParameter)


def test_a_client_can_be_built_without_a_running_loop(tmp_path: object) -> None:
    from conftest import client_config

    # Construction is synchronous and needs no event loop.
    class _Server:
        url = "http://127.0.0.1:9"

    config = client_config(_Server(), tmp_path / "t.json")  # type: ignore[arg-type]
    assert "Client(" in repr(rustyschwab.Client(config))


def test_each_loop_gets_its_own_results() -> None:
    """The one runtime serves several event loops, one after another."""

    from conftest import paper_config

    async def venue(path: str) -> str:
        broker = await rustyschwab.PaperBroker.open(path, paper_config())
        return repr(broker)

    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        first = asyncio.run(venue(f"{tmp}/a.sqlite3"))
        second = asyncio.run(venue(f"{tmp}/b.sqlite3"))
    assert first == second == "PaperBroker(accounts=['paper-test'])"
