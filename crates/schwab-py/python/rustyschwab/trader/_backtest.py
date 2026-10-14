"""Backtests: cached price history, replayed through the paper venue."""

from __future__ import annotations

import shutil
import tempfile
from dataclasses import dataclass, field
from datetime import datetime, timezone
from decimal import Decimal
from pathlib import Path
from typing import TYPE_CHECKING, Any, Mapping, Sequence
from zoneinfo import ZoneInfo

from .. import performance
from .._native import HistoryStore, PaperBroker, synthesize_quotes
from ._context import Book, Candle, Context, Event, Fill, Mode, Quote, Strategy, TrackedOrder
from ._engine import PaperVenue, Runner, dec

if TYPE_CHECKING:
    from .._native import Client

#: The paper account every run trades on.
ACCOUNT = "paper-strategy"
_NEW_YORK = ZoneInfo("America/New_York")
_MAX_BATCH = 10_000
#: Paper limits that leave cash as the only constraint unless overridden.
_OPEN_LIMITS = {
    "max_open_quantity": "1000000000",
    "max_open_notional": "1000000000000",
    "max_position_quantity": "1000000000",
}


def _amount(value: Decimal | str | int) -> str:
    return str(value)


@dataclass(frozen=True)
class Simulation:
    """How the paper venue models a market. Every field is an assumption.

    ``spread_bps`` and ``participation_bps`` shape the quotes a backtest
    derives from candles (the ``candle-bbo-v1`` model): the full spread
    around each price, and the share of a bar's volume offered on each side.
    The rest configure the venue in every simulated mode: ``slippage_bps``
    moves each fill against the order; ``fee_per_share`` and ``fee_bps`` (of
    notional) are charged on every fill, and ``sell_fee_bps`` and
    ``sell_fee_per_share`` (capped per fill at ``sell_fee_cap``) on sells
    only, for regulatory fees such as the SEC's Section 31 fee and FINRA's
    trading activity fee, whose current rates you supply. An order becomes
    eligible to fill ``execution_latency_ms`` after it is accepted and a
    cancel takes effect ``cancel_latency_ms`` after it is asked for. Market
    orders are protected by ``market_collar_bps``. ``regular_hours_only``
    drops pre- and post-market bars.
    """

    spread_bps: Decimal | str | int = "2"
    participation_bps: Decimal | str | int = "1000"
    slippage_bps: Decimal | str | int = "0"
    fee_per_share: Decimal | str | int = "0"
    fee_bps: Decimal | str | int = "0"
    sell_fee_bps: Decimal | str | int = "0"
    sell_fee_per_share: Decimal | str | int = "0"
    sell_fee_cap: Decimal | str | int | None = None
    execution_latency_ms: int = 250
    cancel_latency_ms: int = 250
    market_collar_bps: Decimal | str | int = "500"
    regular_hours_only: bool = True
    max_working_orders: int = 1000

    def paper_config(
        self,
        cash: Decimal,
        *,
        clock: str,
        quote_max_age_ms: int,
        limits: Mapping[str, Any] | None = None,
    ) -> dict[str, Any]:
        return {
            "model_version": "equity-bbo-v2",
            "clock": clock,
            "accounts": {
                ACCOUNT: {
                    "initial_cash": str(cash),
                    "limits": {
                        **_OPEN_LIMITS,
                        "max_snapshot_age_ms": quote_max_age_ms,
                        **(limits or {}),
                    },
                }
            },
            "execution_latency_ms": self.execution_latency_ms,
            "cancel_latency_ms": self.cancel_latency_ms,
            "quote_max_age_ms": quote_max_age_ms,
            "slippage_bps": _amount(self.slippage_bps),
            "market_collar_bps": _amount(self.market_collar_bps),
            "fee_per_share": _amount(self.fee_per_share),
            "fee_bps": _amount(self.fee_bps),
            "sell_fee_bps": _amount(self.sell_fee_bps),
            "sell_fee_per_share": _amount(self.sell_fee_per_share),
            "sell_fee_per_share_cap": (
                None if self.sell_fee_cap is None else _amount(self.sell_fee_cap)
            ),
            "max_working_orders": self.max_working_orders,
        }

    def quote_model(self, candle_ms: int) -> dict[str, Any]:
        return {
            "candle_ms": candle_ms,
            "spread_bps": _amount(self.spread_bps),
            "participation_bps": _amount(self.participation_bps),
            "regular_hours_only": self.regular_hours_only,
        }


def regular_hours(at_ms: int) -> bool:
    """Whether ``at_ms`` falls in New York's regular equity session, as the
    quote model decides it."""
    moment = datetime.fromtimestamp(at_ms / 1000, _NEW_YORK)
    return 9 * 60 + 30 <= moment.hour * 60 + moment.minute < 16 * 60


def epoch_ms(value: datetime | int | float) -> int:
    if isinstance(value, datetime):
        moment = value if value.tzinfo is not None else value.replace(tzinfo=timezone.utc)
        return int(moment.timestamp() * 1000)
    return int(value)


@dataclass
class RunResult:
    """One finished run: a backtest, or a paper or live session up to when it
    stopped. Runs are independent; keep several to compare."""

    mode: Mode
    symbols: tuple[str, ...]
    start_ms: int
    end_ms: int
    starting_cash: Decimal
    simulation: Simulation
    #: The performance report; see :func:`rustyschwab.performance.analyze`.
    stats: performance.PerformanceReport
    #: Every fill, in time order.
    fills: list[Fill]
    #: The strategy's orders by tag, with their outcomes.
    orders: dict[str, TrackedOrder]
    #: ``(at_ms, equity)`` after every bar.
    equity: list[tuple[int, Decimal]]
    #: The strategy's overlays, as drawn by :meth:`Context.plot`.
    plots: dict[str, list[tuple[int, float]]]
    #: The candles the strategy saw, by symbol.
    candles: dict[str, list[Candle]]
    #: The venue's state file, when the run was asked to keep it.
    state: Path | None = None
    #: The bars replayed.
    bars: int = 0
    rejected: list[TrackedOrder] = field(default_factory=list)

    def report(self) -> str:
        """The performance summary as text."""
        start = datetime.fromtimestamp(self.start_ms / 1000, _NEW_YORK)
        end = datetime.fromtimestamp(self.end_ms / 1000, _NEW_YORK)
        bars = f"{self.bars} bars, " if self.mode == "backtest" else ""
        head = (
            f"{self.mode.capitalize()} {', '.join(self.symbols)}: "
            f"{start:%Y-%m-%d %H:%M} to {end:%Y-%m-%d %H:%M} New York, {bars}"
            f"{len(self.fills)} fills, {len(self.rejected)} rejected orders"
        )
        return f"{head}\n{performance.format_report(self.stats)}"

    def to_html(self, path: str | Path) -> Path:
        """Write the charts (prices, fills, overlays and equity) to one
        self-contained HTML file and return its path."""
        from ._chart import render

        target = Path(path)
        target.write_text(
            render(
                title=f"{self.mode.capitalize()}: {', '.join(self.symbols)}",
                candles=self.candles,
                fills=self.fills,
                plots=self.plots,
                equity=self.equity,
                summary=self.report(),
            ),
            encoding="utf-8",
        )
        return target

    def __repr__(self) -> str:
        return (
            f"RunResult(mode={self.mode!r}, symbols={self.symbols!r}, fills={len(self.fills)}, "
            f"net_return={self.stats['net_return']:.4%})"
        )


def _candle(symbol: str, raw: Mapping[str, Any]) -> Candle:
    return {
        "type": "candle",
        "symbol": symbol,
        "time": int(raw["datetime"]),
        "open": float(raw["open"]),
        "high": float(raw["high"]),
        "low": float(raw["low"]),
        "close": float(raw["close"]),
        "volume": float(raw["volume"]),
    }


def recorded_events(entries: Sequence[Mapping[str, Any]]) -> list[Event]:
    """Strategy events from recorded stream payloads."""
    events: list[Event] = []
    for entry in entries:
        payload = entry["payload"]
        at = int(entry["at_ms"])
        if entry["kind"] == "LEVELONE_EQUITIES":
            events.append(quote_event(entry["symbol"], at, payload))
        else:
            events.append(book_event(entry["symbol"], at, payload))
    return events


_QUOTE_FIELDS = {
    "bidPrice": "bid",
    "askPrice": "ask",
    "lastPrice": "last",
    "bidSize": "bid_size",
    "askSize": "ask_size",
    "lastSize": "last_size",
    "totalVolume": "volume",
}


def quote_event(symbol: str, at_ms: int, content: Mapping[str, Any]) -> Quote:
    """A quote event from level-one streamer content with named fields."""
    event: Quote = {"type": "quote", "symbol": symbol, "time": at_ms}
    for wire, name in _QUOTE_FIELDS.items():
        value = content.get(wire)
        if value is not None:
            event[name] = float(value)  # type: ignore[literal-required]
    return event


def book_event(symbol: str, at_ms: int, content: Mapping[str, Any]) -> Book:
    return {
        "type": "book",
        "symbol": symbol,
        "time": int(content.get("marketSnapshotTime") or at_ms),
        "bids": list(content.get("bidSideLevels") or ()),
        "asks": list(content.get("askSideLevels") or ()),
    }


async def run_backtest(
    strategy: Strategy,
    symbols: tuple[str, ...],
    *,
    client: Client | None,
    store: HistoryStore,
    start_ms: int,
    end_ms: int,
    minutes: int,
    cash: Decimal,
    simulation: Simulation,
    level1: bool,
    level2: bool,
    keep_state: Path | None,
    limits: Mapping[str, Any] | None,
    max_candles: int | None,
) -> RunResult:
    candle_ms = minutes * 60_000
    histories = []
    for symbol in symbols:
        if client is not None:
            history = await store.load(
                client, symbol, start_ms, end_ms, frequency_type="minute", frequency=minutes
            )
        else:
            candles = await store.candles(
                symbol, start_ms, end_ms, frequency_type="minute", frequency=minutes
            )
            history = {"symbol": symbol, "empty": not candles, "candles": candles}
        histories.append(history)

    bars: dict[int, list[Candle]] = {}
    first_last: dict[str, tuple[Decimal, Decimal]] = {}
    for history in histories:
        kept = [
            c
            for c in history["candles"]
            if not simulation.regular_hours_only or regular_hours(int(c["datetime"]))
        ]
        if kept:
            first_last[history["symbol"]] = (dec(kept[0]["open"]), dec(kept[-1]["close"]))
        for raw in kept:
            candle = _candle(history["symbol"], raw)
            bars.setdefault(candle["time"], []).append(candle)
    if not bars:
        where = "Schwab returned" if client is not None else "the cache holds"
        raise ValueError(
            f"{where} no {minutes}-minute candles for {', '.join(symbols)} in the window"
        )

    events = synthesize_quotes(histories, simulation.quote_model(candle_ms))
    extra: list[Event] = []
    if level1:
        extra += recorded_events(
            await store.recorded("LEVELONE_EQUITIES", list(symbols), start_ms, end_ms)
        )
    if level2:
        for kind in ("NYSE_BOOK", "NASDAQ_BOOK"):
            extra += recorded_events(await store.recorded(kind, list(symbols), start_ms, end_ms))
    extra.sort(key=lambda event: event["time"])

    directory = keep_state or Path(tempfile.mkdtemp(prefix="rustyschwab-backtest-"))
    if keep_state is not None:
        keep_state.mkdir(mode=0o700, parents=True, exist_ok=False)
    try:
        broker = await PaperBroker.open(
            directory / "venue.sqlite3",
            simulation.paper_config(
                cash, clock="replay", quote_max_age_ms=min(60_000, candle_ms), limits=limits
            ),
        )
        tc = Context("backtest", symbols, cash, max_candles=max_candles)
        venue = PaperVenue(broker, ACCOUNT)
        runner = Runner(strategy, tc, venue)
        await runner.start(send=False)

        cursor = extra_cursor = 0
        sent = True
        times = sorted(bars)
        for at in times:
            # A bar's quotes run from its open up to the next bar's; the
            # strategy sees the candle once they have all printed.
            boundary = at + candle_ms
            batch_end = cursor
            while batch_end < len(events) and events[batch_end]["at_ms"] < boundary:
                batch_end += 1
            session_changed, filled = await _apply(venue, runner, events[cursor:batch_end])
            if batch_end > cursor:
                tc.now = int(events[batch_end - 1]["at_ms"])
            cursor = batch_end
            # The account changes only through fills, sessions (expiries and
            # settlement) and the orders the strategy sent or still has open.
            if sent or filled or session_changed or runner.open:
                await venue.sync(tc, runner)
            wake: list[Event] = list(bars[at])
            while extra_cursor < len(extra) and extra[extra_cursor]["time"] < boundary:
                wake.append(extra[extra_cursor])
                extra_cursor += 1
            sent = await runner.wake(wake)
            runner.mark_equity()

        await _apply(venue, runner, events[cursor:])
        await venue.sync(tc, runner)
        await runner.stop()
        await venue.sync(tc, runner)
        runner.mark_equity()

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
            [{"at_ms": at_ms, "equity": value} for at_ms, value in runner.equity],
            starting_equity=cash,
            benchmark=first_last,
            marks={symbol: last for symbol, (_, last) in first_last.items()},
        )
        return RunResult(
            mode="backtest",
            symbols=symbols,
            start_ms=times[0],
            end_ms=times[-1] + candle_ms,
            starting_cash=cash,
            simulation=simulation,
            stats=stats,
            fills=fills,
            orders=tc.orders,
            equity=runner.equity,
            plots=tc.plots,
            candles=tc.candles,
            state=keep_state,
            bars=len(times),
            rejected=[o for o in tc.orders.values() if o.status == "REJECTED"],
        )
    finally:
        if keep_state is None:
            shutil.rmtree(directory, ignore_errors=True)


async def _apply(
    venue: PaperVenue, runner: Runner, events: Sequence[Mapping[str, Any]]
) -> tuple[bool, bool]:
    """Apply events in batches: whether any opened or closed a session, and
    whether any order filled."""
    session = filled = False
    for start in range(0, len(events), _MAX_BATCH):
        chunk = events[start : start + _MAX_BATCH]
        results = await venue.broker.apply_batch(chunk)
        filled = await venue.fills_from(runner, results) > 0 or filled
        session = session or any(event["kind"]["kind"] == "session" for event in chunk)
    return session, filled
