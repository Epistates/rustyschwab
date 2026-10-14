"""The strategy runner on live data: paper sessions fill against streamed
quotes, live sessions send real orders and read their fills, and the
recorder writes the stream into the cache."""

from __future__ import annotations

import asyncio
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Callable

from conftest import ACCOUNT_HASH, MockSchwab
from test_stream import Streamer, advertise, streamer  # noqa: F401  (fixture)

import rustyschwab
from rustyschwab import HistoryStore, orders
from rustyschwab.trader import Context, Event, Trader

ACCOUNT_PATH = f"/trader/v1/accounts/{ACCOUNT_HASH}"


def now_ms() -> int:
    return int(time.time() * 1000)


def hours_around_now() -> dict[str, Any]:
    """An equity session that opened an hour ago and closes in an hour."""
    now = datetime.now(timezone.utc)
    stamp = lambda moment: moment.isoformat(timespec="seconds")  # noqa: E731
    return {
        "equity": {
            "EQ": {
                "date": now.date().isoformat(),
                "marketType": "EQUITY",
                "product": "EQ",
                "isOpen": True,
                "sessionHours": {
                    "regularMarket": [
                        {
                            "start": stamp(now - timedelta(hours=1)),
                            "end": stamp(now + timedelta(hours=1)),
                        }
                    ]
                },
            }
        }
    }


def quote(bid: float, ask: float, size: int = 500) -> dict[str, Any]:
    return {
        "data": [
            {
                "service": "LEVELONE_EQUITIES", "command": "SUBS",
                "timestamp": now_ms(),
                "content": [
                    {"key": "AAPL", "1": bid, "2": ask, "4": size, "5": size, "34": now_ms()}
                ],
            }
        ]
    }


def candle(close: float) -> dict[str, Any]:
    minute = now_ms() // 60_000 * 60_000 - 60_000
    return {
        "data": [
            {
                "service": "CHART_EQUITY", "command": "SUBS",
                "timestamp": now_ms(),
                "content": [
                    {
                        "key": "AAPL",
                        "seq": 1,
                        "1": 1,
                        "2": close,
                        "3": close + 0.1,
                        "4": close - 0.1,
                        "5": close,
                        "6": 1000,
                        "7": minute,
                        "8": minute // 86_400_000,
                    }
                ],
            }
        ]
    }


async def until(check: Callable[[], bool], timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout
    while not check():
        if time.monotonic() > deadline:
            raise AssertionError("condition not met in time")
        await asyncio.sleep(0.05)


class BuyOnFirstCandle:
    def __init__(self) -> None:
        self.events: list[Event] = []

    def __call__(self, tc: Context, events: list[Event]) -> None:
        self.events.extend(events)
        if any(e["type"] == "candle" for e in events) and "buy" not in tc.orders:
            tc.order(orders.equity_buy_market("AAPL", 5, tag="buy"))


async def test_a_paper_session_fills_against_streamed_quotes(
    client: rustyschwab.Client, server: MockSchwab, streamer: Streamer, tmp_path: Path
) -> None:
    advertise(server, streamer.url)
    server.on("GET", "/marketdata/v1/markets", hours_around_now())
    strategy = BuyOnFirstCandle()
    session = Trader(client, cache=tmp_path / "history.sqlite3").paper(
        strategy, ["AAPL"], cash=10_000
    )
    task = asyncio.create_task(session.run())
    await until(lambda: len(streamer.sent("ADD")) + len(streamer.sent("SUBS")) >= 2)
    await asyncio.sleep(0.3)  # the venue's session opens from the calendar

    await streamer.push(quote(189.50, 189.52))
    await streamer.push(candle(189.51))
    await until(lambda: session.tc is not None and "buy" in session.tc.orders)
    await asyncio.sleep(0.35)  # past the venue's execution latency
    await streamer.push(quote(189.54, 189.56))
    await until(lambda: session.tc is not None and session.tc.orders["buy"].status == "FILLED")

    session.stop()
    result = await asyncio.wait_for(task, 10)
    assert result.mode == "paper"
    (fill,) = result.fills
    assert fill["quantity"] == 5 and str(fill["price"]) == "189.56"
    assert result.orders["buy"].fills == [fill]
    assert result.stats["trades"]["round_trips"] == 0
    assert "Paper AAPL" in result.report()


async def test_a_live_session_sends_orders_and_reads_their_fills(
    client: rustyschwab.Client, server: MockSchwab, streamer: Streamer, tmp_path: Path
) -> None:
    advertise(server, streamer.url)
    server.on(
        "GET",
        ACCOUNT_PATH,
        {
            "securitiesAccount": {
                "type": "CASH",
                "accountNumber": "123",
                "currentBalances": {"cashAvailableForTrading": 5000, "cashBalance": 5000},
                "positions": [],
            }
        },
    )
    server.on(
        "POST",
        f"{ACCOUNT_PATH}/orders",
        status=201,
        headers={"Location": f"{server.url}{ACCOUNT_PATH}/orders/777"},
    )
    filled_at = datetime.now(timezone.utc).isoformat()
    server.on(
        "GET",
        f"{ACCOUNT_PATH}/orders/777",
        {
            "orderId": 777,
            "status": "FILLED",
            "filledQuantity": 5,
            "orderLegCollection": [
                {"instruction": "BUY", "quantity": 5, "instrument": {"symbol": "AAPL"}}
            ],
            "orderActivityCollection": [
                {
                    "activityType": "EXECUTION",
                    "executionType": "FILL",
                    "activityId": 1,
                    "executionLegs": [{"quantity": 5, "price": 190.01, "time": filled_at}],
                }
            ],
        },
    )
    strategy = BuyOnFirstCandle()
    session = Trader(client, cache=tmp_path / "history.sqlite3").live(
        strategy, ["AAPL"], account_hash=ACCOUNT_HASH, poll_seconds=0.2
    )
    task = asyncio.create_task(session.run())
    await until(lambda: len(streamer.sent("ADD")) + len(streamer.sent("SUBS")) >= 2)
    await streamer.push(candle(190.0))
    await until(lambda: session.tc is not None and session.tc.orders.get("buy") is not None)
    await until(lambda: session.tc is not None and session.tc.orders["buy"].status == "FILLED")

    session.stop()
    result = await asyncio.wait_for(task, 10)
    (fill,) = result.fills
    assert str(fill["price"]) == "190.01" and fill["order_id"] == "777"
    (sent,) = server.calls("POST", f"{ACCOUNT_PATH}/orders")
    assert sent.json()["tag"] == "buy"
    services = {r["service"] for r in streamer.requests}
    assert "ACCT_ACTIVITY" in services


async def test_the_recorder_writes_quotes_and_candles(
    client: rustyschwab.Client, server: MockSchwab, streamer: Streamer, tmp_path: Path
) -> None:
    advertise(server, streamer.url)
    trader = Trader(client, cache=tmp_path / "history.sqlite3")
    task = asyncio.create_task(trader.record(["AAPL"], market_hours=False))
    await until(lambda: len(streamer.sent("ADD")) + len(streamer.sent("SUBS")) >= 2)
    await streamer.push(quote(10.0, 10.01))
    await streamer.push(candle(10.0))
    store: HistoryStore = await trader.store()
    start, end = now_ms() - 3_600_000, now_ms() + 60_000
    for _ in range(100):
        recorded = await store.recorded("LEVELONE_EQUITIES", ["AAPL"], start, end)
        candles = await store.candles("AAPL", start, end)
        if recorded and candles:
            break
        await asyncio.sleep(0.05)
    task.cancel()
    await asyncio.gather(task, return_exceptions=True)
    assert recorded[0]["payload"]["bidPrice"] == 10.0
    assert candles[0]["close"] == 10.0
