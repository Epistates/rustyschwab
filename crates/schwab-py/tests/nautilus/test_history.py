"""Schwab history as backtest bars: the loader, its cache, and a real engine run."""

from __future__ import annotations

from collections.abc import Iterator
from datetime import date, datetime, timezone
from pathlib import Path
from typing import Any

import pytest

pytest.importorskip("nautilus_trader")

from conftest import MockSchwab, Recorded, Reply  # noqa: E402
from nautilus_trader.backtest.engine import BacktestEngine  # noqa: E402
from nautilus_trader.config import BacktestEngineConfig, LoggingConfig  # noqa: E402
from nautilus_trader.model.currencies import USD  # noqa: E402
from nautilus_trader.model.data import Bar, BarType  # noqa: E402
from nautilus_trader.model.enums import AccountType, OmsType, OrderSide, OrderStatus  # noqa: E402
from nautilus_trader.model.identifiers import InstrumentId  # noqa: E402
from nautilus_trader.model.instruments import Equity  # noqa: E402
from nautilus_trader.model.objects import Money, Quantity  # noqa: E402
from nautilus_trader.persistence.catalog import ParquetDataCatalog  # noqa: E402
from nautilus_trader.trading.strategy import Strategy  # noqa: E402

import rustyschwab  # noqa: E402
from rustyschwab import HistoryStore  # noqa: E402
from rustyschwab.nautilus import SCHWAB_VENUE, load_bars, write_catalog  # noqa: E402
from rustyschwab.nautilus import history as history_module  # noqa: E402
from rustyschwab.nautilus.parsing import UnsupportedBars, parse_candles  # noqa: E402

PRICE_HISTORY = "/marketdata/v1/pricehistory"
MINUTE = 60_000
SECOND = 1_000_000_000
#: Monday 2023-11-13 09:30 New York, a settled day.
OPEN = 1_699_885_800_000
CLOSES = [189.5, 189.75, 190.0, 189.25, 189.5]


def candles(frequency_ms: int = MINUTE) -> list[dict[str, Any]]:
    return [
        {
            "open": close - 0.25,
            "high": close + 0.5,
            "low": close - 0.5,
            "close": close,
            "volume": 1_000 + n,
            "datetime": OPEN + n * frequency_ms,
        }
        for n, close in enumerate(CLOSES)
    ]


def window() -> tuple[datetime, datetime]:
    return (
        datetime.fromtimestamp(OPEN / 1000, tz=timezone.utc),
        datetime.fromtimestamp((OPEN + 10 * MINUTE) / 1000, tz=timezone.utc),
    )


@pytest.fixture
def minute_bars(aapl: Equity) -> BarType:
    return BarType.from_str(f"{aapl.id}-1-MINUTE-LAST-EXTERNAL")


@pytest.fixture
async def store(tmp_path: Path) -> HistoryStore:
    return await HistoryStore.open(tmp_path / "history.sqlite3")


@pytest.fixture
async def filled_store(store: HistoryStore) -> HistoryStore:
    await store.put_candles(
        "AAPL", candles(), complete=(OPEN, OPEN + 10 * MINUTE)
    )
    return store


@pytest.fixture
def history(server: MockSchwab) -> list[Recorded]:
    """Schwab's price history on the mock; the list is each request it served."""
    served: list[Recorded] = []

    def reply(request: Recorded) -> Reply:
        served.append(request)
        start = int(request.query["startDate"][0])
        end = int(request.query["endDate"][0])
        wanted = [c for c in candles() if start <= c["datetime"] <= end]
        return Reply(200, {"symbol": "AAPL", "empty": not wanted, "candles": wanted})

    server.on_call("GET", PRICE_HISTORY, reply)
    return served


# ---- the loader --------------------------------------------------------------------


async def test_a_store_alone_serves_bars_offline(
    filled_store: HistoryStore, aapl: Equity, minute_bars: BarType
) -> None:
    start, end = window()
    bars = await load_bars(filled_store, aapl, minute_bars, start, end)
    assert len(bars) == len(CLOSES)
    assert all(isinstance(bar, Bar) and bar.bar_type == minute_bars for bar in bars)
    assert [float(bar.close) for bar in bars] == CLOSES
    assert str(bars[0].open) == "189.25" and str(bars[0].high) == "190.00"
    assert str(bars[0].low) == "189.00" and int(bars[0].volume) == 1_000
    # Stamped on the close, as the live client's historical bars are.
    closes_ns = [(OPEN + (n + 1) * MINUTE) * 1_000_000 for n in range(len(CLOSES))]
    assert [bar.ts_event for bar in bars] == closes_ns
    # A backtest replays by ts_init, so it is the bar's own time and not the load's.
    assert [bar.ts_init for bar in bars] == closes_ns


async def test_bars_are_stamped_exactly_as_the_live_client_stamps_them(
    filled_store: HistoryStore, aapl: Equity, minute_bars: BarType
) -> None:
    start, end = window()
    raw = await filled_store.candles("AAPL", OPEN, OPEN + 10 * MINUTE)
    for on_close in (True, False):
        loaded = await load_bars(
            filled_store, aapl, minute_bars, start, end, timestamp_on_close=on_close
        )
        live = parse_candles(minute_bars, raw, aapl, 7, on_close=on_close)
        assert [(b.open, b.close, b.volume, b.ts_event) for b in loaded] == [
            (b.open, b.close, b.volume, b.ts_event) for b in live
        ]
    on_open = await load_bars(filled_store, aapl, minute_bars, start, end, timestamp_on_close=False)
    assert on_open[0].ts_event == OPEN * 1_000_000 == on_open[0].ts_init


async def test_the_window_takes_dates_and_naive_datetimes_as_utc(
    filled_store: HistoryStore, aapl: Equity, minute_bars: BarType
) -> None:
    naive = await load_bars(
        filled_store, aapl, minute_bars, datetime(2023, 11, 13, 14, 30), datetime(2023, 11, 13, 14, 33)
    )
    assert len(naive) == 4  # candles opening 14:30 through 14:33
    day = await load_bars(filled_store, aapl, minute_bars, date(2023, 11, 13), date(2023, 11, 14))
    assert len(day) == len(CLOSES)
    assert await load_bars(filled_store, aapl, minute_bars, date(2023, 11, 14), date(2023, 11, 15)) == []


async def test_unsupported_and_foreign_bar_types_are_refused(
    filled_store: HistoryStore, aapl: Equity
) -> None:
    start, end = window()
    with pytest.raises(UnsupportedBars):
        await load_bars(filled_store, aapl, BarType.from_str(f"{aapl.id}-2-MINUTE-LAST-EXTERNAL"), start, end)
    other = BarType.from_str("MSFT.SCHWAB-1-MINUTE-LAST-EXTERNAL")
    with pytest.raises(ValueError, match="not a bar type of AAPL.SCHWAB"):
        await load_bars(filled_store, aapl, other, start, end)


async def test_a_store_source_takes_no_store_argument(
    filled_store: HistoryStore, aapl: Equity, minute_bars: BarType
) -> None:
    start, end = window()
    with pytest.raises(ValueError, match="only with a Client"):
        await load_bars(filled_store, aapl, minute_bars, start, end, store=filled_store)


# ---- through a client --------------------------------------------------------------


async def test_the_second_load_makes_no_request(
    client: rustyschwab.Client,
    history: list[Recorded],
    store: HistoryStore,
    aapl: Equity,
    minute_bars: BarType,
) -> None:
    start, end = window()
    first = await load_bars(client, aapl, minute_bars, start, end, store=store)
    assert len(first) == len(CLOSES)
    (request,) = history
    assert request.query["symbol"] == ["AAPL"]
    assert (request.query["frequencyType"], request.query["frequency"]) == (["minute"], ["1"])

    again = await load_bars(client, aapl, minute_bars, start, end, store=store)
    assert len(history) == 1
    assert [(b.close, b.ts_event, b.ts_init) for b in again] == [
        (b.close, b.ts_event, b.ts_init) for b in first
    ]
    # And with nothing but the file, once the network is not needed at all.
    offline = await load_bars(store, aapl, minute_bars, start, end)
    assert len(offline) == len(first) and len(history) == 1


async def test_the_cache_may_be_a_path_and_defaults_to_the_trader_cache(
    client: rustyschwab.Client,
    history: list[Recorded],
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    aapl: Equity,
    minute_bars: BarType,
) -> None:
    start, end = window()
    by_path = tmp_path / "chosen" / "history.sqlite3"
    await load_bars(client, aapl, minute_bars, start, end, store=by_path)
    await load_bars(client, aapl, minute_bars, start, end, store=str(by_path))
    assert len(history) == 1 and by_path.exists()

    default = tmp_path / "default" / "history.sqlite3"
    monkeypatch.setattr(history_module, "DEFAULT_CACHE", default)
    await load_bars(client, aapl, minute_bars, start, end)
    await load_bars(client, aapl, minute_bars, start, end)
    assert len(history) == 2 and default.exists()


def test_the_default_cache_is_the_traders() -> None:
    assert history_module.DEFAULT_CACHE is rustyschwab.trader.DEFAULT_CACHE


async def test_each_bar_type_asks_schwab_for_its_frequency(
    client: rustyschwab.Client,
    history: list[Recorded],
    store: HistoryStore,
    aapl: Equity,
) -> None:
    start, end = window()
    for spec, expected in (("5-MINUTE", (["minute"], ["5"])), ("1-DAY", (["daily"], ["1"]))):
        bar_type = BarType.from_str(f"{aapl.id}-{spec}-LAST-EXTERNAL")
        await load_bars(client, aapl, bar_type, start, end, store=store)
        assert (history[-1].query["frequencyType"], history[-1].query["frequency"]) == expected


async def test_extended_hours_candles_are_a_separate_series(
    client: rustyschwab.Client,
    history: list[Recorded],
    store: HistoryStore,
    aapl: Equity,
    minute_bars: BarType,
) -> None:
    start, end = window()
    await load_bars(client, aapl, minute_bars, start, end, store=store)
    await load_bars(client, aapl, minute_bars, start, end, store=store, extended_hours=True)
    assert [r.query["needExtendedHoursData"] for r in history] == [["false"], ["true"]]
    # The regular series is untouched, so it still needs no request.
    await load_bars(client, aapl, minute_bars, start, end, store=store)
    assert len(history) == 2


# ---- a catalog ---------------------------------------------------------------------


async def test_bars_go_to_a_catalog_a_node_can_read(
    filled_store: HistoryStore, tmp_path: Path, aapl: Equity, minute_bars: BarType
) -> None:
    start, end = window()
    bars = await load_bars(filled_store, aapl, minute_bars, start, end)
    catalog = ParquetDataCatalog(tmp_path / "catalog")
    write_catalog(catalog, bars, [aapl])
    assert [i.id for i in catalog.instruments()] == [aapl.id]
    stored = catalog.bars(bar_types=[str(minute_bars)])
    assert [(b.close, b.ts_event, b.ts_init) for b in stored] == [
        (b.close, b.ts_event, b.ts_init) for b in bars
    ]


# ---- a backtest --------------------------------------------------------------------


class BuyOnce(Strategy):  # type: ignore[misc]
    """Sends one market order when the first bar arrives."""

    def __init__(self, instrument: Equity, bar_type: BarType) -> None:
        super().__init__()
        self.instrument = instrument
        self.bar_type = bar_type
        self.seen: list[Bar] = []

    def on_start(self) -> None:
        self.subscribe_bars(self.bar_type)

    def on_bar(self, bar: Bar) -> None:
        self.seen.append(bar)
        if len(self.seen) == 1:
            self.submit_order(
                self.order_factory.market(
                    instrument_id=self.instrument.id,
                    order_side=OrderSide.BUY,
                    quantity=Quantity.from_int(10),
                )
            )


@pytest.fixture
def engine() -> Iterator[BacktestEngine]:
    backtest = BacktestEngine(
        config=BacktestEngineConfig(logging=LoggingConfig(bypass_logging=True))
    )
    backtest.add_venue(
        venue=SCHWAB_VENUE,
        oms_type=OmsType.NETTING,
        account_type=AccountType.MARGIN,
        base_currency=USD,
        starting_balances=[Money(100_000, USD)],
    )
    yield backtest
    backtest.dispose()


# NautilusTrader 1.231 calls pandas' deprecated Timestamp.utcnow inside the engine's run.
@pytest.mark.filterwarnings("ignore:Timestamp.utcnow is deprecated")
async def test_loaded_bars_drive_a_backtest_engine(
    engine: BacktestEngine, filled_store: HistoryStore, aapl: Equity, minute_bars: BarType
) -> None:
    start, end = window()
    bars = await load_bars(filled_store, aapl, minute_bars, start, end)
    engine.add_instrument(aapl)
    engine.add_data(bars)
    strategy = BuyOnce(aapl, minute_bars)
    engine.add_strategy(strategy)
    engine.run()

    assert len(strategy.seen) == len(CLOSES)
    assert [bar.ts_event for bar in strategy.seen] == [bar.ts_event for bar in bars]
    (order,) = engine.cache.orders()
    assert order.status == OrderStatus.FILLED and order.filled_qty == Quantity.from_int(10)
    (position,) = engine.cache.positions()
    assert position.instrument_id == InstrumentId.from_str("AAPL.SCHWAB")
    assert position.quantity == Quantity.from_int(10) and position.is_open
    assert 189.0 <= position.avg_px_open <= 190.5  # inside the loaded bars' range
    # The order went in on the first bar's close, so it cannot fill earlier.
    assert order.ts_last >= bars[0].ts_event
