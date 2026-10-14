"""Strategies on live data: paper trading against the venue, live trading
against an account, and recording the stream for later backtests."""

from __future__ import annotations

import asyncio
import shutil
import tempfile
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from decimal import Decimal
from pathlib import Path
from typing import TYPE_CHECKING, Any, Awaitable, Callable, Mapping, Sequence

from .. import performance
from .._native import HistoryStore, PaperBroker
from ..errors import SchwabError
from ..schedule import MarketCalendar, Subscriptions, run_during_market_hours
from ._backtest import (
    ACCOUNT,
    RunResult,
    Simulation,
    book_event,
    quote_event,
    regular_hours,
    to_candle,
)
from ._context import Candle, Context, Event, Mode, Strategy, log
from ._engine import LiveVenue, PaperVenue, Runner, Venue, dec

if TYPE_CHECKING:
    from .._native import Client, StreamMessage

#: Level-one fields the runner asks for: the quote, its sizes and its time.
QUOTE_FIELDS = (
    "key",
    "bidPrice",
    "askPrice",
    "lastPrice",
    "bidSize",
    "askSize",
    "lastSize",
    "totalVolume",
    "quoteTime",
    "tradeTime",
)
BOOK_SERVICES = ("NYSE_BOOK", "NASDAQ_BOOK")


def now_ms() -> int:
    return int(time.time() * 1000)


def candle_event(content: Mapping[str, Any]) -> Candle | None:
    """A completed bar from ``CHART_EQUITY`` content with named fields."""
    try:
        return {
            "type": "candle",
            "symbol": str(content["key"]),
            "time": int(content["chartTime"]),
            "open": float(content["open"]),
            "high": float(content["high"]),
            "low": float(content["low"]),
            "close": float(content["close"]),
            "volume": float(content["volume"]),
        }
    except (KeyError, TypeError, ValueError):
        return None


@dataclass
class _Parsed:
    events: list[Event]
    quotes: list[tuple[str, int, Mapping[str, Any]]]
    activity: bool


def parse(message: StreamMessage) -> _Parsed:
    """Strategy events from one stream message, plus the raw level-one
    updates (for the paper venue) and whether account activity arrived."""
    parsed = _Parsed([], [], False)
    if message.get("type") != "data":
        return parsed
    for item in message.get("data") or ():
        service = item.get("service")
        stamp = int(item.get("timestamp") or now_ms())
        for content in item.get("content") or ():
            symbol = str(content.get("key", ""))
            if service == "CHART_EQUITY":
                candle = candle_event(content)
                if candle is not None:
                    parsed.events.append(candle)
            elif service == "LEVELONE_EQUITIES":
                at = int(content.get("quoteTime") or stamp)
                parsed.quotes.append((symbol, at, content))
                parsed.events.append(quote_event(symbol, at, content))
            elif service in BOOK_SERVICES:
                parsed.events.append(book_event(symbol, stamp, content))
            elif service == "ACCT_ACTIVITY":
                parsed.activity = True
    return parsed


class _QuoteFeed:
    """Merges level-one deltas into whole quotes for the paper venue."""

    def __init__(self, size_multiplier: Decimal) -> None:
        self.size_multiplier = size_multiplier
        self.book: dict[str, dict[str, Any]] = {}

    def update(self, symbol: str, at_ms: int, content: Mapping[str, Any]) -> dict[str, Any] | None:
        merged = self.book.setdefault(symbol, {})
        merged.update({k: v for k, v in content.items() if v is not None})
        needed = ("bidPrice", "askPrice", "bidSize", "askSize")
        if any(merged.get(name) is None for name in needed):
            return None
        bid, ask = dec(merged["bidPrice"]), dec(merged["askPrice"])
        if bid <= 0 or ask <= 0 or bid > ask:
            return None
        return {
            "kind": "quote",
            "symbol": symbol,
            # Never after the venue's clock, which is this machine's.
            "source_at_ms": min(at_ms, now_ms()),
            "bid": str(bid),
            "ask": str(ask),
            "bid_size": str(dec(merged["bidSize"]) * self.size_multiplier),
            "ask_size": str(dec(merged["askSize"]) * self.size_multiplier),
        }


class Session:
    """A strategy running on live data. Start it with :meth:`run`; end it
    with :meth:`stop` (or by cancelling the task), which runs ``on_stop``
    and returns a :class:`RunResult` for the time it ran."""

    def __init__(
        self,
        mode: Mode,
        strategy: Strategy,
        symbols: tuple[str, ...],
        *,
        client: Client,
        cash: Decimal | None,
        account_hash: str | None = None,
        simulation: Simulation | None = None,
        chart: bool = True,
        level1: bool = False,
        level2: bool = False,
        size_multiplier: Decimal = Decimal(1),
        state: Path | None = None,
        limits: Mapping[str, Any] | None = None,
        poll_seconds: float = 5.0,
        max_candles: int | None = None,
        warmup_days: float = 0,
        history: Callable[[], Awaitable[HistoryStore]] | None = None,
    ) -> None:
        if mode == "live" and not account_hash:
            raise ValueError("live trading needs the account_hash of the account to trade")
        if warmup_days < 0 or (warmup_days > 0 and history is None):
            raise ValueError("warmup_days must be non-negative and needs a history cache")
        self.mode: Mode = mode
        self.strategy = strategy
        self.symbols = symbols
        self.client = client
        self.cash = cash
        self.account_hash = account_hash
        self.simulation = simulation or Simulation()
        self.chart, self.level1, self.level2 = chart, level1, level2
        self.size_multiplier = size_multiplier
        self.state = state
        self.limits = limits
        self.poll_seconds = poll_seconds
        self.max_candles = max_candles
        self.warmup_days = warmup_days
        self.history = history
        self._stop = asyncio.Event()
        self._started = 0
        self.tc: Context | None = None
        self.runner: Runner | None = None

    def stop(self) -> None:
        """Ask the session to finish after the message in hand."""
        self._stop.set()

    def snapshot(self) -> RunResult:
        """The session so far, for a report or a chart while it runs."""
        if self.tc is None or self.runner is None:
            raise RuntimeError("the session has not started")
        return _result(
            self.mode,
            self.symbols,
            self.tc,
            self.runner,
            self._started,
            self.simulation,
            self.state,
        )

    async def _warm_up(self, tc: Context) -> None:
        """Fill ``tc.candles`` with the last ``warmup_days`` of minute bars,
        without waking the strategy."""
        assert self.history is not None
        store = await self.history()
        end = now_ms()
        start = end - int(self.warmup_days * 86_400_000)
        for symbol in self.symbols:
            loaded = await store.load(self.client, symbol, start, end)
            tc._ingest(
                [
                    to_candle(symbol, raw)
                    for raw in loaded["candles"]
                    if not self.simulation.regular_hours_only or regular_hours(int(raw["datetime"]))
                ]
            )

    async def run(self, *, until: datetime | None = None) -> RunResult:
        """Run until :meth:`stop`, ``until`` or cancellation."""
        directory: Path | None = None
        broker: PaperBroker | None = None
        venue: Venue
        if self.mode == "paper":
            directory = self.state or Path(tempfile.mkdtemp(prefix="rustyschwab-paper-"))
            if self.state is not None:
                self.state.mkdir(mode=0o700, parents=True, exist_ok=True)
            cash = self.cash if self.cash is not None else Decimal(10_000)
            broker = await PaperBroker.open(
                directory / "venue.sqlite3",
                self.simulation.paper_config(
                    cash, clock="wall", quote_max_age_ms=5_000, limits=self.limits
                ),
            )
            venue = PaperVenue(broker, ACCOUNT)
        else:
            assert self.account_hash is not None
            venue = LiveVenue(self.client, self.account_hash)
            cash = Decimal(0)
        tc = Context(self.mode, self.symbols, cash, max_candles=self.max_candles)
        runner = Runner(self.strategy, tc, venue)
        tc.now = now_ms()
        if self.warmup_days > 0:
            await self._warm_up(tc)
        if isinstance(venue, LiveVenue):
            await venue.sync(tc, runner)
            tc.starting_cash = tc.equity()
        self.tc, self.runner = tc, runner
        feed = _QuoteFeed(self.size_multiplier)
        calendar = MarketCalendar(self.client)
        started = self._started = now_ms()
        stream = await self.client.open_stream(named_fields=True)
        background: list[asyncio.Task[None]] = []
        try:
            await self._subscribe(stream)
            if broker is not None:
                background.append(asyncio.create_task(self._sessions(broker, calendar)))
            if isinstance(venue, LiveVenue):
                background.append(asyncio.create_task(self._poll(venue, tc, runner)))
            await runner.start()
            deadline = None if until is None else until.timestamp() * 1000
            messages = stream.__aiter__()
            while not self._stop.is_set():
                if deadline is not None and now_ms() >= deadline:
                    break
                try:
                    message = await asyncio.wait_for(messages.__anext__(), timeout=1.0)
                except asyncio.TimeoutError:
                    continue
                except StopAsyncIteration:
                    log.warning("the stream ended; the session stops")
                    break
                await self._handle(message, tc, runner, venue, broker, feed)
        finally:
            for task in background:
                task.cancel()
            await asyncio.gather(*background, return_exceptions=True)
            try:
                await runner.stop()
                if isinstance(venue, (PaperVenue, LiveVenue)):
                    await venue.sync(tc, runner)
                runner.mark_equity()
            finally:
                await stream.close()
                if directory is not None and self.state is None:
                    shutil.rmtree(directory, ignore_errors=True)
        return _result(self.mode, self.symbols, tc, runner, started, self.simulation, self.state)

    async def _subscribe(self, stream: Any) -> None:
        symbols = list(self.symbols)
        if self.chart:
            await stream.subscribe("CHART_EQUITY", symbols)
        if self.level1 or self.mode == "paper":
            await stream.subscribe("LEVELONE_EQUITIES", symbols, fields=list(QUOTE_FIELDS))
        if self.level2:
            for service in BOOK_SERVICES:
                await stream.subscribe(service, symbols)
        if self.mode == "live":
            await stream.subscribe("ACCT_ACTIVITY", ["Account Activity"])

    async def _handle(
        self,
        message: StreamMessage,
        tc: Context,
        runner: Runner,
        venue: Venue,
        broker: PaperBroker | None,
        feed: _QuoteFeed,
    ) -> None:
        parsed = parse(message)
        tc.now = now_ms()
        if broker is not None and isinstance(venue, PaperVenue):
            results = []
            for symbol, at, content in parsed.quotes:
                quote = feed.update(symbol, at, content)
                if quote is None:
                    continue
                try:
                    results.append(await broker.append_live(quote))
                except SchwabError as error:
                    log.debug("paper venue refused a quote for %s: %s", symbol, error)
            if await venue.fills_from(runner, results):
                await venue.sync(tc, runner)
        if parsed.activity and isinstance(venue, LiveVenue):
            await venue.sync(tc, runner)
        wanted: list[Event] = [
            event
            for event in parsed.events
            if (event["type"] == "candle" and self.chart)
            or (event["type"] == "quote" and self.level1)
            or (event["type"] == "book" and self.level2)
        ]
        # Quotes the strategy did not ask for still mark its positions.
        delivered = {id(event) for event in wanted}
        tc._ingest([event for event in parsed.events if id(event) not in delivered])
        if wanted or runner._unreported:
            if await runner.wake(wanted) and isinstance(venue, PaperVenue):
                await venue.sync(tc, runner)
            runner.mark_equity()

    async def _sessions(self, broker: PaperBroker, calendar: MarketCalendar) -> None:
        """Open and close the paper venue's session with the market's."""
        is_open: bool | None = None
        while True:
            moment = datetime.now(timezone.utc)
            try:
                windows = await calendar.windows(moment.astimezone(calendar_zone()).date())
            except SchwabError as error:
                log.warning("market hours unavailable (%s); keeping the session as it is", error)
                await asyncio.sleep(30)
                continue
            current = next((w for w in windows if w.contains(moment)), None)
            wanted = current is not None
            if wanted != is_open:
                session_id = moment.astimezone(calendar_zone()).strftime("%Y-%m-%d")
                await broker.append_live(
                    {"kind": "session", "session_id": session_id, "open": wanted}
                )
                log.info("paper session %s %s", session_id, "open" if wanted else "closed")
                is_open = wanted
            await asyncio.sleep(15)

    async def _poll(self, venue: LiveVenue, tc: Context, runner: Runner) -> None:
        """Read open orders and the account every ``poll_seconds``, in case an
        account-activity message was missed."""
        while True:
            await asyncio.sleep(self.poll_seconds)
            if not runner.open and not any(o.status == "UNKNOWN" for o in tc.orders.values()):
                continue
            try:
                await venue.sync(tc, runner)
            except SchwabError as error:
                log.warning("live order poll failed: %s", error)
                continue
            if runner._unreported:
                tc.now = now_ms()
                await runner.wake([])
                runner.mark_equity()


def calendar_zone() -> Any:
    from zoneinfo import ZoneInfo

    return ZoneInfo("America/New_York")


def _result(
    mode: Mode,
    symbols: tuple[str, ...],
    tc: Context,
    runner: Runner,
    started: int,
    simulation: Simulation,
    state: Path | None,
) -> RunResult:
    fills = sorted(runner.fills, key=lambda fill: fill["time"])
    stats = performance.analyze(
        [
            {
                "symbol": fill["symbol"],
                "side": fill["side"],
                "quantity": fill["quantity"],
                "price": fill["price"],
                "fee": fill["fee"],
                "at_ms": fill["time"],
            }
            for fill in fills
        ],
        [{"at_ms": at, "equity": value} for at, value in runner.equity] or None,
        starting_equity=tc.starting_cash if tc.starting_cash > 0 else Decimal(1),
        marks=dict(tc._marks),
    )
    return RunResult(
        mode=mode,
        symbols=symbols,
        start_ms=started,
        end_ms=now_ms(),
        starting_cash=tc.starting_cash,
        simulation=simulation,
        stats=stats,
        fills=fills,
        orders=tc.orders,
        equity=runner.equity,
        plots=tc.plots,
        candles=tc.candles,
        state=state,
        rejected=[o for o in tc.orders.values() if o.status == "REJECTED"],
    )


async def record(
    client: Client,
    store: HistoryStore,
    symbols: Sequence[str],
    *,
    chart: bool,
    level1: bool,
    level2: bool,
    market_hours: bool,
    until: datetime | None,
) -> int:
    """Write the stream to ``store`` until ``until`` (or cancellation); the
    number of payloads written."""
    plan = Subscriptions(named_fields=True)
    keys = list(symbols)
    if chart:
        plan.add("CHART_EQUITY", keys)
    if level1:
        plan.add("LEVELONE_EQUITIES", keys, fields=list(QUOTE_FIELDS))
    if level2:
        for service in BOOK_SERVICES:
            plan.add(service, keys)
    written = 0

    async def receiver(message: StreamMessage) -> None:
        nonlocal written
        if message.get("type") != "data":
            return
        entries = []
        for item in message.get("data") or ():
            service = str(item.get("service"))
            stamp = int(item.get("timestamp") or now_ms())
            for content in item.get("content") or ():
                symbol = str(content.get("key", ""))
                if not symbol:
                    continue
                if service == "CHART_EQUITY":
                    candle = candle_event(content)
                    if candle is not None:
                        await store.put_candles(
                            symbol,
                            [
                                {
                                    "datetime": candle["time"],
                                    "open": candle["open"],
                                    "high": candle["high"],
                                    "low": candle["low"],
                                    "close": candle["close"],
                                    "volume": int(candle["volume"]),
                                }
                            ],
                        )
                        written += 1
                else:
                    at = int(content.get("quoteTime") or content.get("marketSnapshotTime") or stamp)
                    entries.append(
                        {"kind": service, "symbol": symbol, "at_ms": at, "payload": dict(content)}
                    )
        if entries:
            await store.record(entries)
            written += len(entries)

    async def recording() -> None:
        if market_hours:
            await run_during_market_hours(client, plan, receiver=receiver)
            return
        stream = await plan(client)
        try:
            async for message in stream:
                await receiver(message)
        finally:
            await stream.close()

    task = asyncio.ensure_future(recording())
    try:
        if until is None:
            await task
        else:
            timeout = max(0.0, until.timestamp() - time.time())
            try:
                await asyncio.wait_for(asyncio.shield(task), timeout)
            except asyncio.TimeoutError:
                pass
    finally:
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)
    return written
