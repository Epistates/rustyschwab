"""The data client: Level One deltas to ticks, stream gaps and endings, and bars."""

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator, Callable, Mapping, Sequence
from dataclasses import dataclass, field
from typing import Any

import pytest

pytest.importorskip("nautilus_trader")

from conftest import MockSchwab  # noqa: E402
from nautilus_trader.common.component import LiveClock, MessageBus  # noqa: E402
from nautilus_trader.core.uuid import UUID4  # noqa: E402
from nautilus_trader.data.messages import (  # noqa: E402
    RequestBars,
    SubscribeQuoteTicks,
    SubscribeTradeTicks,
    UnsubscribeQuoteTicks,
    UnsubscribeTradeTicks,
)
from nautilus_trader.model.data import Bar, BarType, QuoteTick, TradeTick  # noqa: E402
from nautilus_trader.model.identifiers import ClientId, InstrumentId, Venue  # noqa: E402
from nautilus_trader.model.instruments import Equity, OptionContract  # noqa: E402
from nautilus_trader.test_kit.stubs.identifiers import TestIdStubs  # noqa: E402
from websockets.asyncio.server import serve  # noqa: E402

import rustyschwab  # noqa: E402
from rustyschwab.nautilus.config import SchwabDataClientConfig  # noqa: E402
from rustyschwab.nautilus.data import SchwabDataClient  # noqa: E402
from rustyschwab.nautilus.parsing import (  # noqa: E402
    LevelOneDecoder,
    UnsupportedBars,
    parse_candles,
    schwab_frequency,
)
from rustyschwab.nautilus.providers import SchwabInstrumentProvider  # noqa: E402

from .support import OPTION_SYMBOL, equity_quote_item  # noqa: E402

AAPL_ID = InstrumentId.from_str("AAPL.SCHWAB")
CALL_ID = InstrumentId.from_str("AAPL261218C00200000.SCHWAB")
SECOND = 1_000_000_000


# ---- the decoder ------------------------------------------------------------------


def test_deltas_are_merged_into_complete_quote_ticks(aapl: Equity) -> None:
    decoder = LevelOneDecoder(aapl, size_lot=100)
    # Level One sends only what changed, so the first deltas are not a quote yet.
    assert decoder.update({"key": "AAPL", "bidPrice": 189.49, "bidSize": 4}, 1_000, 5) == (None, None)
    quote, trade = decoder.update(
        {"key": "AAPL", "askPrice": 189.51, "askSize": 2, "quoteTime": 1_700_000_000_123}, 1_001, 6
    )
    assert trade is None and isinstance(quote, QuoteTick)
    assert (str(quote.bid_price), str(quote.ask_price)) == ("189.49", "189.51")
    assert (int(quote.bid_size), int(quote.ask_size)) == (400, 200)  # lots to shares
    assert (quote.ts_event, quote.ts_init) == (1_700_000_000_123 * 1_000_000, 6)

    # A change to one side merges with the other's last value.
    quote, _ = decoder.update({"key": "AAPL", "bidPrice": 189.50}, 1_002, 7)
    assert quote is not None and str(quote.bid_price) == "189.50" and str(quote.ask_price) == "189.51"
    # No quote time came with this delta, so the frame's own timestamp stamps it.
    assert quote.ts_event == 1_002 * 1_000_000


def test_a_delta_that_changes_nothing_publishes_nothing(aapl: Equity) -> None:
    decoder = LevelOneDecoder(aapl, size_lot=100)
    full = {"bidPrice": 189.49, "askPrice": 189.51, "bidSize": 4, "askSize": 2}
    assert decoder.update(full, None, 1)[0] is not None
    assert decoder.update(full, None, 2)[0] is None


def test_unusable_quotes_are_skipped(aapl: Equity) -> None:
    decoder = LevelOneDecoder(aapl, size_lot=100)
    zero = {"bidPrice": 0.0, "askPrice": 189.51, "bidSize": 0, "askSize": 2}
    assert decoder.update(zero, None, 1) == (None, None)


def test_option_sizes_are_contracts_and_not_multiplied(aapl_call: OptionContract) -> None:
    decoder = LevelOneDecoder(aapl_call, size_lot=1)
    quote, _ = decoder.update({"bidPrice": 5.1, "askPrice": 5.3, "bidSize": 12, "askSize": 7}, None, 1)
    assert quote is not None and (int(quote.bid_size), int(quote.ask_size)) == (12, 7)
    assert quote.instrument_id == CALL_ID


def test_a_trade_needs_a_new_trade_time_or_volume(aapl: Equity) -> None:
    decoder = LevelOneDecoder(aapl, size_lot=100)
    _, trade = decoder.update(
        {"lastPrice": 189.50, "lastSize": 25, "totalVolume": 1_000, "tradeTime": 1_700_000_000_500}, 9, 1
    )
    assert isinstance(trade, TradeTick)
    assert (str(trade.price), int(trade.size)) == ("189.50", 25)  # trade sizes are shares
    assert trade.trade_id.value == "1700000000500-1000"
    assert trade.ts_event == 1_700_000_000_500 * 1_000_000

    # The same last price restated, with no new volume or time: not a trade.
    assert decoder.update({"lastPrice": 189.50, "totalVolume": 1_000}, 10, 2) == (None, None)
    # Same price, more volume: a trade.
    _, again = decoder.update({"lastPrice": 189.50, "lastSize": 10, "totalVolume": 1_010}, 11, 3)
    assert again is not None and again.trade_id.value == "11-1010"  # message time stands in
    # A zero-size print is not a trade.
    assert decoder.update({"lastPrice": 189.6, "lastSize": 0, "totalVolume": 1_020}, 12, 4)[1] is None


def test_a_reset_forgets_the_state_a_gap_made_stale(aapl: Equity) -> None:
    decoder = LevelOneDecoder(aapl, size_lot=100)
    decoder.update({"bidPrice": 189.49, "askPrice": 189.51, "bidSize": 4, "askSize": 2}, None, 1)
    decoder.reset()
    assert decoder.update({"bidPrice": 189.50}, None, 2) == (None, None)


def test_a_rest_snapshot_seeds_the_state_and_publishes_only_the_quote(aapl: Equity) -> None:
    decoder = LevelOneDecoder(aapl, size_lot=100)
    snapshot = {
        "bidPrice": 189.49, "askPrice": 189.51, "bidSize": 4, "askSize": 2,
        "lastPrice": 189.5, "lastSize": 100, "totalVolume": 5_000, "quoteTime": 1_700_000_000_000,
        "tradeTime": 1_699_999_999_000,
    }
    quote = decoder.seed(snapshot, 3)
    assert quote is not None and str(quote.bid_price) == "189.49"
    # The next stream delta merges with the snapshot, and an old trade is not replayed.
    next_quote, trade = decoder.update({"askPrice": 189.52}, None, 4)
    assert next_quote is not None and str(next_quote.bid_price) == "189.49" and trade is None


# ---- bars -------------------------------------------------------------------------


def bar_type(spec: str, aapl: Equity) -> BarType:
    return BarType.from_str(f"{aapl.id}-{spec}-EXTERNAL")


@pytest.mark.parametrize(
    ("spec", "expected"),
    [
        ("1-MINUTE-LAST", ("minute", 1)),
        ("5-MINUTE-LAST", ("minute", 5)),
        ("30-MINUTE-LAST", ("minute", 30)),
        ("1-DAY-LAST", ("daily", 1)),
        ("1-WEEK-LAST", ("weekly", 1)),
        ("1-MONTH-LAST", ("monthly", 1)),
    ],
)
def test_bar_types_map_to_schwabs_frequencies(aapl: Equity, spec: str, expected: tuple[str, int]) -> None:
    assert schwab_frequency(bar_type(spec, aapl)) == expected


@pytest.mark.parametrize("spec", ["2-MINUTE-LAST", "1-HOUR-LAST", "2-DAY-LAST", "1-MINUTE-BID", "100-TICK-LAST"])
def test_other_bar_types_are_refused(aapl: Equity, spec: str) -> None:
    with pytest.raises(UnsupportedBars):
        schwab_frequency(bar_type(spec, aapl))
    with pytest.raises(UnsupportedBars, match="externally aggregated"):
        schwab_frequency(BarType.from_str(f"{aapl.id}-1-MINUTE-LAST-INTERNAL"))


CANDLES = [
    {"open": 189.0, "high": 190.0, "low": 188.5, "close": 189.5, "volume": 1200, "datetime": 1_700_000_040_000},
    {"open": 189.5, "high": 190.5, "low": 189.0, "close": 190.0, "volume": 800, "datetime": 1_700_000_000_000},
    {"open": None, "datetime": 1_700_000_100_000},
]


def test_candles_become_bars_stamped_on_the_close(aapl: Equity) -> None:
    bars = parse_candles(bar_type("1-MINUTE-LAST", aapl), CANDLES, aapl, 9)
    assert [b.ts_event for b in bars] == [1_700_000_060 * SECOND, 1_700_000_100 * SECOND]  # sorted; open+60s
    first = bars[0]
    assert (str(first.open), str(first.high), str(first.low), str(first.close)) == (
        "189.50", "190.50", "189.00", "190.00",
    )
    assert int(first.volume) == 800
    on_open = parse_candles(bar_type("1-MINUTE-LAST", aapl), CANDLES, aapl, 9, on_close=False)
    assert on_open[0].ts_event == 1_700_000_000 * SECOND


def test_a_monthly_bar_closes_a_calendar_month_after_it_opens(aapl: Equity) -> None:
    candle = {"open": 1.0, "high": 2.0, "low": 1.0, "close": 2.0, "volume": 1, "datetime": 1_698_796_800_000}
    (bar,) = parse_candles(bar_type("1-MONTH-LAST", aapl), [candle], aapl, 1)
    assert bar.ts_event == 1_701_388_800 * SECOND  # 2023-11-01 to 2023-12-01 UTC


# ---- the client -------------------------------------------------------------------


@dataclass
class FakeStream:
    """The part of ``rustyschwab.Stream`` the data client uses."""

    queue: asyncio.Queue[Mapping[str, Any] | None] = field(default_factory=asyncio.Queue)
    subscribed: list[tuple[str, list[str], list[Any]]] = field(default_factory=list)
    unsubscribed: list[tuple[str, list[str]]] = field(default_factory=list)
    closed: bool = False

    def __aiter__(self) -> AsyncIterator[Mapping[str, Any]]:
        return self._messages()

    async def _messages(self) -> AsyncIterator[Mapping[str, Any]]:
        while True:
            message = await self.queue.get()
            if message is None:
                return
            yield message

    async def subscribe(self, service: str, keys: Sequence[str], *, fields: Sequence[Any] | None = None) -> None:
        self.subscribed.append((service, list(keys), list(fields or [])))

    async def unsubscribe(self, service: str, keys: Sequence[str]) -> None:
        self.unsubscribed.append((service, list(keys)))

    async def close(self) -> None:
        self.closed = True
        self.queue.put_nowait(None)

    def push(self, *messages: Mapping[str, Any]) -> None:
        for message in messages:
            self.queue.put_nowait(message)

    def end(self, reason: str) -> None:
        self.push(state(ready=False, ended={"reason": reason}), None)  # type: ignore[arg-type]


class FakeClient:
    """The part of ``rustyschwab.Client`` the data client uses."""

    def __init__(self) -> None:
        self.streams: list[FakeStream] = []
        self.quote_calls: list[list[str]] = []
        self.history_calls: list[Mapping[str, Any]] = []
        self.quotes_reply: dict[str, Any] = {}
        self.history_reply: dict[str, Any] = {"candles": []}
        self.open_failures = 0

    async def open_stream(self, overrides: Mapping[str, Any] | None = None) -> FakeStream:
        if self.open_failures:
            self.open_failures -= 1
            raise rustyschwab.NetworkError("down")
        self.streams.append(FakeStream())
        return self.streams[-1]

    async def quotes(self, symbols: Sequence[str], **_: Any) -> dict[str, Any]:
        self.quote_calls.append(list(symbols))
        return self.quotes_reply

    async def price_history(self, params: Mapping[str, Any]) -> dict[str, Any]:
        self.history_calls.append(params)
        return self.history_reply

    async def option_chain(self, params: Mapping[str, Any]) -> dict[str, Any]:  # pragma: no cover
        raise AssertionError("not used")


def state(*, ready: bool = True, generation: int = 1, lost: int = 0, ended: Any = None) -> dict[str, Any]:
    message: dict[str, Any] = {"type": "state", "ready": ready, "generation": generation, "lost_messages": lost}
    if ended is not None:
        message["ended"] = ended
    return message


def level_one(service: str, key: str, **fields: Any) -> dict[str, Any]:
    return {
        "type": "data",
        "data": [{"service": service, "timestamp": 1_700_000_000_000, "command": "SUBS",
                  "content": [{"key": key, **fields}]}],
    }


@dataclass
class Bus:
    data: list[Any] = field(default_factory=list)
    responses: list[Any] = field(default_factory=list)

    def of(self, kind: type) -> list[Any]:
        return [d for d in self.data if isinstance(d, kind)]


@dataclass
class DataHarness:
    client: SchwabDataClient
    fake: FakeClient
    bus: Bus
    clock: LiveClock

    @property
    def stream(self) -> FakeStream:
        return self.fake.streams[-1]


@pytest.fixture
async def harness(aapl: Equity, aapl_call: OptionContract) -> AsyncIterator[DataHarness]:
    clock = LiveClock()
    msgbus = MessageBus(trader_id=TestIdStubs.trader_id(), clock=clock)
    bus = Bus()
    msgbus.register(endpoint="DataEngine.process", handler=bus.data.append)
    msgbus.register(endpoint="DataEngine.response", handler=bus.responses.append)
    from nautilus_trader.cache.cache import Cache

    cache = Cache(database=None)
    cache.add_instrument(aapl)
    cache.add_instrument(aapl_call)
    fake = FakeClient()
    provider = SchwabInstrumentProvider(fake)  # type: ignore[arg-type]
    provider.add(aapl)
    provider.add(aapl_call)
    client = SchwabDataClient(
        loop=asyncio.get_running_loop(),
        client=fake,  # type: ignore[arg-type]
        msgbus=msgbus, cache=cache, clock=clock,
        instrument_provider=provider,
        config=SchwabDataClientConfig(stream_reconnect_attempts=2),
    )
    await client._connect()
    client._set_connected(True)
    yield DataHarness(client, fake, bus, clock)
    await client._disconnect()


def quote_command(instrument_id: InstrumentId = AAPL_ID) -> SubscribeQuoteTicks:
    return SubscribeQuoteTicks(instrument_id, ClientId("SCHWAB"), Venue("SCHWAB"), UUID4(), 0)


def trade_command(instrument_id: InstrumentId = AAPL_ID) -> SubscribeTradeTicks:
    return SubscribeTradeTicks(instrument_id, ClientId("SCHWAB"), Venue("SCHWAB"), UUID4(), 0)


async def until(predicate: Callable[[], bool], timeout: float = 5.0) -> None:
    deadline = asyncio.get_running_loop().time() + timeout
    while not predicate():
        if asyncio.get_running_loop().time() > deadline:
            raise AssertionError("condition not reached")
        await asyncio.sleep(0.01)


async def test_connect_publishes_the_loaded_instruments(harness: DataHarness, aapl: Equity) -> None:
    assert {str(i.id) for i in harness.bus.data} == {"AAPL.SCHWAB", "AAPL261218C00200000.SCHWAB"}
    assert harness.fake.streams == []  # the one streaming session is not opened until it is needed


async def test_subscribing_opens_the_stream_and_maps_quotes_and_trades(harness: DataHarness) -> None:
    await harness.client._subscribe_quote_ticks(quote_command())
    await harness.client._subscribe_trade_ticks(trade_command())
    (service, keys, fields) = harness.stream.subscribed[0]
    assert (service, keys) == ("LEVELONE_EQUITIES", ["AAPL"])
    assert {"bidPrice", "askPrice", "bidSize", "askSize", "lastPrice", "lastSize"} <= set(fields)
    assert len(harness.stream.subscribed) == 1  # one Level One subscription serves both

    harness.stream.push(
        level_one("LEVELONE_EQUITIES", "AAPL", bidPrice=189.49, askPrice=189.51, bidSize=4, askSize=2),
        level_one("LEVELONE_EQUITIES", "AAPL", lastPrice=189.5, lastSize=50, totalVolume=77, tradeTime=1_700_000_000_900),
    )
    await until(lambda: harness.bus.of(QuoteTick) and harness.bus.of(TradeTick))
    (quote,) = harness.bus.of(QuoteTick)
    (trade,) = harness.bus.of(TradeTick)
    assert quote.instrument_id == AAPL_ID and (int(quote.bid_size), int(quote.ask_size)) == (400, 200)
    assert (str(trade.price), int(trade.size)) == ("189.50", 50)


async def test_only_the_subscribed_kinds_are_published(harness: DataHarness) -> None:
    await harness.client._subscribe_quote_ticks(quote_command())
    harness.stream.push(
        level_one("LEVELONE_EQUITIES", "AAPL", bidPrice=189.49, askPrice=189.51, bidSize=4, askSize=2,
                  lastPrice=189.5, lastSize=50, totalVolume=77, tradeTime=1_700_000_000_900),
        level_one("LEVELONE_EQUITIES", "MSFT", bidPrice=410.0, askPrice=410.1, bidSize=1, askSize=1),
    )
    await until(lambda: harness.bus.of(QuoteTick))
    await asyncio.sleep(0.05)
    assert len(harness.bus.of(QuoteTick)) == 1 and harness.bus.of(TradeTick) == []


async def test_the_subscription_ends_with_the_last_kind(harness: DataHarness) -> None:
    await harness.client._subscribe_quote_ticks(quote_command())
    await harness.client._subscribe_trade_ticks(trade_command())
    await harness.client._unsubscribe_quote_ticks(
        UnsubscribeQuoteTicks(AAPL_ID, ClientId("SCHWAB"), Venue("SCHWAB"), UUID4(), 0)
    )
    assert harness.stream.unsubscribed == []
    await harness.client._unsubscribe_trade_ticks(
        UnsubscribeTradeTicks(AAPL_ID, ClientId("SCHWAB"), Venue("SCHWAB"), UUID4(), 0)
    )
    assert harness.stream.unsubscribed == [("LEVELONE_EQUITIES", ["AAPL"])]


async def test_an_option_uses_the_options_service_and_schwabs_padded_key(harness: DataHarness) -> None:
    await harness.client._subscribe_quote_ticks(quote_command(CALL_ID))
    assert harness.stream.subscribed[0][:2] == ("LEVELONE_OPTIONS", [OPTION_SYMBOL])
    harness.stream.push(
        level_one("LEVELONE_OPTIONS", OPTION_SYMBOL, bidPrice=5.1, askPrice=5.3, bidSize=12, askSize=7)
    )
    await until(lambda: harness.bus.of(QuoteTick))
    (quote,) = harness.bus.of(QuoteTick)
    assert quote.instrument_id == CALL_ID and (int(quote.bid_size), int(quote.ask_size)) == (12, 7)


async def test_an_unknown_instrument_is_not_subscribed(harness: DataHarness) -> None:
    harness.fake.quotes_reply = {"errors": {"invalidSymbols": ["NOPE"]}}
    await harness.client._subscribe_quote_ticks(quote_command(InstrumentId.from_str("NOPE.SCHWAB")))
    assert harness.fake.streams == []


async def test_a_gap_resets_the_state_and_rereads_the_quotes(harness: DataHarness) -> None:
    await harness.client._subscribe_quote_ticks(quote_command())
    harness.stream.push(
        state(generation=1),
        level_one("LEVELONE_EQUITIES", "AAPL", bidPrice=189.49, askPrice=189.51, bidSize=4, askSize=2),
    )
    await until(lambda: len(harness.bus.of(QuoteTick)) == 1)

    harness.fake.quotes_reply = {
        "AAPL": {"quote": {"bidPrice": 190.0, "askPrice": 190.02, "bidSize": 3, "askSize": 5,
                           "quoteTime": 1_700_000_100_000, "lastPrice": 190.01}}
    }
    harness.stream.push(state(generation=2, lost=7))
    await until(lambda: len(harness.bus.of(QuoteTick)) == 2)
    reread = harness.bus.of(QuoteTick)[-1]
    assert (str(reread.bid_price), int(reread.bid_size)) == ("190.00", 300)
    assert harness.fake.quote_calls == [["AAPL"]]

    # The state before the gap is gone: a one-sided delta now merges with the snapshot.
    harness.stream.push(level_one("LEVELONE_EQUITIES", "AAPL", askPrice=190.03))
    await until(lambda: len(harness.bus.of(QuoteTick)) == 3)
    assert str(harness.bus.of(QuoteTick)[-1].bid_price) == "190.00"

    # `lost_messages` is a running total: repeating it is not a new gap, raising it is.
    harness.stream.push(state(generation=2, lost=7))
    await asyncio.sleep(0.1)
    assert len(harness.fake.quote_calls) == 1
    harness.stream.push(state(generation=2, lost=9))
    await until(lambda: len(harness.fake.quote_calls) == 2)


async def test_a_new_generation_without_loss_is_still_a_gap(harness: DataHarness) -> None:
    await harness.client._subscribe_quote_ticks(quote_command())
    harness.fake.quotes_reply = {"AAPL": {"quote": {"bidPrice": 1.5, "askPrice": 1.6, "bidSize": 1, "askSize": 1}}}
    harness.stream.push(state(generation=1), state(generation=2))
    await until(lambda: harness.fake.quote_calls == [["AAPL"]])


@pytest.mark.parametrize("reason", ["replaced", "login_denied", "disconnected"])
async def test_some_endings_are_final(harness: DataHarness, reason: str) -> None:
    await harness.client._subscribe_quote_ticks(quote_command())
    assert harness.client.is_connected
    harness.stream.end(reason)
    await until(lambda: not harness.client.is_connected)
    await asyncio.sleep(0.2)
    # Not reopened: for "replaced" that would end the session that replaced this one.
    assert len(harness.fake.streams) == 1


async def test_a_stream_with_nothing_subscribed_may_close_quietly(harness: DataHarness) -> None:
    await harness.client._subscribe_quote_ticks(quote_command())
    await harness.client._unsubscribe_quote_ticks(
        UnsubscribeQuoteTicks(AAPL_ID, ClientId("SCHWAB"), Venue("SCHWAB"), UUID4(), 0)
    )
    harness.stream.end("idle")
    await asyncio.sleep(0.2)
    assert harness.client.is_connected and len(harness.fake.streams) == 1
    # The next subscription opens a new stream.
    await harness.client._subscribe_quote_ticks(quote_command())
    assert len(harness.fake.streams) == 2


async def test_a_dropped_stream_is_reopened_and_resubscribed(harness: DataHarness) -> None:
    await harness.client._subscribe_quote_ticks(quote_command())
    await harness.client._subscribe_quote_ticks(quote_command(CALL_ID))
    harness.fake.quotes_reply = {
        "AAPL": {"quote": {"bidPrice": 189.0, "askPrice": 189.1, "bidSize": 1, "askSize": 1}},
        OPTION_SYMBOL: {"quote": {"bidPrice": 5.0, "askPrice": 5.2, "bidSize": 1, "askSize": 1}},
    }
    harness.stream.end("server_closed")
    await until(lambda: len(harness.fake.streams) == 2, timeout=5)
    await until(lambda: len(harness.bus.of(QuoteTick)) == 2)
    resubscribed = {(s, tuple(k)) for s, k, _ in harness.stream.subscribed}
    assert resubscribed == {("LEVELONE_EQUITIES", ("AAPL",)), ("LEVELONE_OPTIONS", (OPTION_SYMBOL,))}

    # And the new stream delivers.
    harness.stream.push(level_one("LEVELONE_EQUITIES", "AAPL", bidPrice=189.2))
    await until(lambda: len(harness.bus.of(QuoteTick)) == 3)


async def test_giving_up_after_the_configured_attempts(harness: DataHarness) -> None:
    await harness.client._subscribe_quote_ticks(quote_command())
    harness.fake.open_failures = 5
    harness.stream.end("reconnect_exhausted")
    await until(lambda: not harness.client.is_connected, timeout=10)
    assert len(harness.fake.streams) == 1


async def test_bars_come_from_price_history(harness: DataHarness, aapl: Equity) -> None:
    harness.fake.history_reply = {"symbol": "AAPL", "candles": CANDLES}
    request = RequestBars(
        bar_type=bar_type("1-MINUTE-LAST", aapl),
        start=None, end=None, limit=0, client_id=ClientId("SCHWAB"), venue=Venue("SCHWAB"),
        callback=None, request_id=UUID4(), ts_init=0, params=None,
    )
    await harness.client._request_bars(request)
    (params,) = harness.fake.history_calls
    assert (params["symbol"], params["frequencyType"], params["frequency"]) == ("AAPL", "minute", 1)
    assert params["needExtendedHoursData"] is False and params["endDate"] > params["startDate"]
    (response,) = harness.bus.responses
    assert [type(b) for b in response.data] == [Bar, Bar] and response.correlation_id == request.id


async def test_a_limit_keeps_the_latest_bars(harness: DataHarness, aapl: Equity) -> None:
    harness.fake.history_reply = {"candles": CANDLES}
    request = RequestBars(
        bar_type=bar_type("5-MINUTE-LAST", aapl),
        start=None, end=None, limit=1, client_id=ClientId("SCHWAB"), venue=Venue("SCHWAB"),
        callback=None, request_id=UUID4(), ts_init=0, params=None,
    )
    await harness.client._request_bars(request)
    (response,) = harness.bus.responses
    assert len(response.data) == 1 and response.data[0].ts_event > 1_700_000_000 * SECOND


async def test_an_unsupported_bar_request_is_logged_not_sent(harness: DataHarness, aapl: Equity) -> None:
    request = RequestBars(
        bar_type=bar_type("1-HOUR-LAST", aapl),
        start=None, end=None, limit=0, client_id=ClientId("SCHWAB"), venue=Venue("SCHWAB"),
        callback=None, request_id=UUID4(), ts_init=0, params=None,
    )
    await harness.client._request_bars(request)
    assert harness.fake.history_calls == [] and harness.bus.responses == []


# ---- end to end through the real stream ------------------------------------------


async def test_quotes_flow_from_the_wire_to_the_message_bus(
    client: rustyschwab.Client, server: MockSchwab, aapl: Equity
) -> None:
    from test_stream import Streamer, advertise

    streamer = Streamer()
    async with serve(streamer.handle, "127.0.0.1", 0) as listener:
        streamer.url = f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}/ws"
        advertise(server, streamer.url)
        server.on("GET", "/marketdata/v1/quotes", {"AAPL": equity_quote_item()})

        clock = LiveClock()
        msgbus = MessageBus(trader_id=TestIdStubs.trader_id(), clock=clock)
        bus = Bus()
        msgbus.register(endpoint="DataEngine.process", handler=bus.data.append)
        from nautilus_trader.cache.cache import Cache

        data_client = SchwabDataClient(
            loop=asyncio.get_running_loop(), client=client, msgbus=msgbus, cache=Cache(database=None),
            clock=clock, instrument_provider=SchwabInstrumentProvider(client),
            config=SchwabDataClientConfig(),
        )
        try:
            await data_client._subscribe_quote_ticks(quote_command())  # loads AAPL over REST first
            await until(lambda: streamer.sent("ADD"))
            (add,) = streamer.sent("ADD")
            assert add["service"] == "LEVELONE_EQUITIES" and add["parameters"]["keys"] == "AAPL"
            await streamer.push(
                {"data": [{"service": "LEVELONE_EQUITIES", "timestamp": 1_700_000_000_000, "command": "SUBS",
                           "content": [{"key": "AAPL", "1": 189.49, "2": 189.51, "4": 4, "5": 2}]}]}
            )
            await until(lambda: bus.of(QuoteTick))
            (quote,) = bus.of(QuoteTick)
            assert (str(quote.bid_price), str(quote.ask_price), int(quote.bid_size)) == ("189.49", "189.51", 400)
        finally:
            await data_client._disconnect()
