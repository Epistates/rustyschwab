"""The strategy runner: backtests over cached history through the paper venue,
and the history cache itself."""

from __future__ import annotations

from datetime import datetime, timezone
from decimal import Decimal
from pathlib import Path
from typing import Any

import pytest
from conftest import MockSchwab, Recorded, Reply

import rustyschwab
from rustyschwab import HistoryStore, orders
from rustyschwab.trader import Context, Event, Simulation, Trader

MINUTE = 60_000
#: 2026-01-05 09:30 New York, a Monday.
OPEN = 1_767_623_400_000
CLOSES = [10.0, 10.1, 10.2, 10.4, 10.3, 10.6, 10.5, 10.2, 10.0, 9.9]


def bars(closes: list[float], start: int = OPEN) -> list[dict[str, Any]]:
    out, previous = [], closes[0]
    for index, close in enumerate(closes):
        out.append(
            {
                "datetime": start + index * MINUTE,
                "open": previous,
                "high": max(previous, close) + 0.05,
                "low": min(previous, close) - 0.05,
                "close": close,
                "volume": 100_000,
            }
        )
        previous = close
    return out


@pytest.fixture
async def store(tmp_path: Path) -> HistoryStore:
    store = await HistoryStore.open(tmp_path / "cache" / "history.sqlite3")
    await store.put_candles("ABC", bars(CLOSES))
    return store


def window() -> dict[str, int]:
    return {"start": OPEN, "end": OPEN + len(CLOSES) * MINUTE}


class BuyThenSell:
    """Buys 10 on the third bar, sells them on the seventh."""

    def __init__(self) -> None:
        self.seen: list[Event] = []
        self.started = self.stopped = False

    def on_start(self, tc: Context) -> None:
        self.started = True

    def on_stop(self, tc: Context) -> None:
        self.stopped = True

    def __call__(self, tc: Context, events: list[Event]) -> None:
        self.seen.extend(events)
        bar = len(tc.candles["ABC"])
        if bar == 3 and any(e["type"] == "candle" for e in events):
            tc.order(orders.equity_buy_market("ABC", 10, tag="entry"))
        if bar == 7 and any(e["type"] == "candle" for e in events):
            tc.order(orders.equity_sell_market("ABC", tc.sellable("ABC"), tag="exit"))


async def test_a_backtest_fills_on_the_next_bar_and_reports(store: HistoryStore, tmp_path: Path) -> None:
    strategy = BuyThenSell()
    run = await Trader(cache=store).backtest(strategy, ["ABC"], cash=1_000, **window())

    assert strategy.started and strategy.stopped
    assert run.bars == len(CLOSES)
    entry, exit_ = run.orders["entry"], run.orders["exit"]
    assert entry.status == exit_.status == "FILLED"
    # Placed when the third bar closed, so it fills at the fourth bar's open
    # mark: the third close (10.2) plus half the 2 bps spread, rounded up.
    buy = run.fills[0]
    assert buy["side"] == "BUY" and buy["quantity"] == Decimal(10)
    assert buy["price"] == Decimal("10.21")
    assert buy["time"] == OPEN + 3 * MINUTE
    assert run.fills[1]["side"] == "SELL"
    assert run.fills[1]["time"] == OPEN + 7 * MINUTE

    # Fill reports reach the strategy as events on its next call.
    reported = [e for e in strategy.seen if e["type"] == "fill"]
    assert [e["tag"] for e in reported] == ["entry", "exit"]

    stats = run.stats
    assert stats["trades"]["round_trips"] == 1
    expected = (run.fills[1]["price"] - buy["price"]) * 10
    assert Decimal(stats["realized_pnl"]) == expected
    assert len(run.equity) == run.bars + 1
    assert run.equity[-1][1] == Decimal(1000) + expected
    assert "ABC" in run.report()

    page = run.to_html(tmp_path / "run.html").read_text()
    assert page.startswith("<!doctype html>") and "<svg" in page and "BUY 10" in page


async def test_runs_are_reproducible(store: HistoryStore) -> None:
    trader = Trader(cache=store)
    first = await trader.backtest(BuyThenSell(), "ABC", cash=1_000, **window())
    second = await trader.backtest(BuyThenSell(), "ABC", cash=1_000, **window())
    assert first.fills == second.fills
    assert first.equity == second.equity


async def test_costs_follow_the_simulation(store: HistoryStore) -> None:
    simulation = Simulation(fee_per_share="0.01", slippage_bps="10", spread_bps="20")
    run = await Trader(cache=store).backtest(
        BuyThenSell(), "ABC", cash=1_000, simulation=simulation, **window()
    )
    cheap = await Trader(cache=store).backtest(BuyThenSell(), "ABC", cash=1_000, **window())
    assert run.fills[0]["fee"] == Decimal("0.10")
    assert run.fills[0]["price"] > cheap.fills[0]["price"]
    assert Decimal(run.stats["end_equity"]) < Decimal(cheap.stats["end_equity"])


async def test_notional_and_sell_fees(store: HistoryStore) -> None:
    simulation = Simulation(fee_bps="10", sell_fee_bps="0.5", sell_fee_per_share="0.001", sell_fee_cap="0.005")
    run = await Trader(cache=store).backtest(
        BuyThenSell(), "ABC", cash=1_000, simulation=simulation, **window()
    )
    buy, sell = run.fills
    assert buy["fee"] == buy["price"] * 10 * Decimal("0.001")
    assert sell["fee"] == sell["price"] * 10 * Decimal("0.00105") + Decimal("0.005")
    assert Decimal(run.stats["fees"]) == buy["fee"] + sell["fee"]


async def test_rejections_withdrawals_and_start_orders(store: HistoryStore) -> None:
    def strategy(tc: Context, events: list[Event]) -> None:
        if len(tc.candles["ABC"]) == 2:
            # More than the account can pay for: refused by the venue.
            tc.order(orders.equity_buy_limit("ABC", 1_000, "10.50", tag="too-big"))
            tag = tc.order(orders.equity_buy_limit("ABC", 1, "10.50", tag="withdrawn"))
            assert tc.cancel(tag)

    class Starter:
        def on_start(self, tc: Context) -> None:
            tc.order(orders.equity_buy_limit("ABC", 1, "11", tag="at-start"))

        def __call__(self, tc: Context, events: list[Event]) -> None:
            strategy(tc, events)

    run = await Trader(cache=store).backtest(Starter(), "ABC", cash=1_000, **window())
    assert run.orders["too-big"].status == "REJECTED"
    assert run.orders["too-big"].reason
    assert run.orders["withdrawn"].status == "CANCELED"
    assert run.orders["at-start"].status == "FILLED"
    assert [o.tag for o in run.rejected] == ["too-big"]


async def test_async_strategies_and_plots(store: HistoryStore, tmp_path: Path) -> None:
    async def strategy(tc: Context, events: list[Event]) -> None:
        for event in events:
            if event["type"] == "candle":
                tc.plot("close ABC", event["close"])
                tc.plot("everywhere", 10.0)

    run = await Trader(cache=store).backtest(strategy, "ABC", cash=1_000, **window())
    assert len(run.plots["close ABC"]) == len(CLOSES)
    assert run.fills == []
    assert "close ABC" in run.to_html(tmp_path / "plots.html").read_text()


async def test_recorded_quotes_replay_as_events(store: HistoryStore) -> None:
    await store.record(
        [
            {
                "kind": "LEVELONE_EQUITIES",
                "symbol": "ABC",
                "at_ms": OPEN + 90_000,
                "payload": {"key": "ABC", "bidPrice": 10.05, "askPrice": 10.07},
            }
        ]
    )
    seen: list[Event] = []

    def strategy(tc: Context, events: list[Event]) -> None:
        seen.extend(e for e in events if e["type"] == "quote")

    await Trader(cache=store).backtest(strategy, "ABC", cash=1_000, level1=True, **window())
    assert len(seen) == 1 and seen[0]["bid"] == 10.05 and seen[0]["time"] == OPEN + 90_000


async def test_an_empty_window_is_an_error(store: HistoryStore) -> None:
    with pytest.raises(ValueError, match="no 1-minute candles"):
        await Trader(cache=store).backtest(lambda tc, ev: None, "XYZ", **window())


async def test_bad_arguments(store: HistoryStore) -> None:
    trader = Trader(cache=store)
    with pytest.raises(ValueError, match="minutes"):
        await trader.backtest(lambda tc, ev: None, "ABC", minutes=2, **window())
    with pytest.raises(ValueError, match="Client"):
        trader.paper(lambda tc, ev: None, "ABC")

    def two_orders_one_tag(tc: Context, events: list[Event]) -> None:
        tc.order(orders.equity_buy_market("ABC", 1, tag="same"))
        tc.order(orders.equity_buy_market("ABC", 1, tag="same"))

    with pytest.raises(ValueError, match="already names"):
        await trader.backtest(two_orders_one_tag, "ABC", **window())


async def test_the_cache_fetches_a_settled_span_once(
    client: rustyschwab.Client, server: MockSchwab, tmp_path: Path
) -> None:
    def history(request: Recorded) -> Reply:
        start = int(request.query["startDate"][0])
        end = int(request.query["endDate"][0])
        candles = [c for c in bars(CLOSES) if start <= c["datetime"] <= end]
        return Reply(200, {"symbol": "ABC", "empty": not candles, "candles": candles})

    server.on_call("GET", "/marketdata/v1/pricehistory", history)
    store = await HistoryStore.open(tmp_path / "history.sqlite3")
    end = OPEN + len(CLOSES) * MINUTE
    first = await store.load(client, "ABC", OPEN, end)
    again = await store.load(client, "ABC", OPEN, end)
    assert len(first["candles"]) == len(CLOSES)
    assert again["candles"] == first["candles"]
    calls = server.calls("GET", "/marketdata/v1/pricehistory")
    assert len(calls) == 1
    assert calls[0].query["frequencyType"] == ["minute"]
    assert await store.missing("ABC", OPEN, end) == []

    run = await Trader(client, cache=store).backtest(BuyThenSell(), "ABC", start=OPEN, end=end)
    assert len(server.calls("GET", "/marketdata/v1/pricehistory")) == 1
    assert run.orders["entry"].status == "FILLED"


def test_synthesized_quotes_follow_the_declared_model() -> None:
    history = {"symbol": "ABC", "empty": False, "candles": bars([10.0, 10.2])}
    events = rustyschwab.synthesize_quotes(
        [history],
        {"candle_ms": MINUTE, "spread_bps": "2", "participation_bps": "1000"},
    )
    kinds = [event["kind"]["kind"] for event in events]
    assert kinds[0] == "session" and kinds.count("quote") == 8
    assert [event["sequence"] for event in events] == list(range(1, len(events) + 1))
    assert "candle-bbo-v1" in events[0]["kind"]["session_id"]


async def test_apply_batch_is_all_or_nothing(tmp_path: Path) -> None:
    from conftest import paper_config

    from rustyschwab import PaperBroker, paper

    broker = await PaperBroker.open(tmp_path / "state" / "venue.sqlite3", paper_config())
    good = [
        paper.session(1, 0, "s1"),
        paper.quote(2, 10, "ABC", bid="10", ask="10.01", bid_size=100, ask_size=100),
    ]
    results = await broker.apply_batch(good)
    assert [r["sequence"] for r in results] == [1, 2]
    bad = [paper.clock(3, 20), paper.clock(5, 30)]
    with pytest.raises(rustyschwab.SchwabError):
        await broker.apply_batch(bad)
    assert (await broker.apply(paper.clock(3, 20)))["duplicate"] is False


def test_context_bookkeeping() -> None:
    tc = Context("backtest", ("ABC",), Decimal(100))
    tc.positions["ABC"] = Decimal(5)
    tc._marks["ABC"] = Decimal(2)
    assert tc.equity() == Decimal(110)
    tag = tc.order(orders.equity_sell_market("ABC", 3))
    assert tc.sellable("ABC") == Decimal(2)
    assert tc.working("ABC")[0].tag == tag
    assert tc.cancel("nope") is False
    with pytest.raises(ValueError, match="single-leg"):
        tc.order({"orderLegCollection": []})
    when = datetime(2026, 1, 5, tzinfo=timezone.utc)
    tc.now = int(when.timestamp() * 1000)
    tc.plot("x", 1)
    assert tc.plots["x"] == [(tc.now, 1.0)]
