"""Write a strategy once; backtest it, paper trade it and run it live.

A strategy is any callable ``strategy(tc, events)``, plain or ``async``.
``tc`` is a :class:`Context`: the candles, quotes, books, positions, cash and
orders the strategy can see, and :meth:`Context.order`, :meth:`Context.cancel`
and :meth:`Context.plot` to act. ``events`` are what is new since the last
call, in time order: completed ``candle`` bars, ``quote`` and ``book``
updates when asked for, and ``fill`` reports for the strategy's own orders.
The strategy cannot tell which mode it is in unless it reads ``tc.mode``.

.. code-block:: python

    from rustyschwab import Client, orders
    from rustyschwab.trader import Trader

    def crossover(tc, events):
        for event in events:
            if event["type"] != "candle":
                continue
            closes = [c["close"] for c in tc.candles[event["symbol"]][-30:]]
            if len(closes) < 30:
                continue
            fast, slow = sum(closes[-10:]) / 10, sum(closes) / 30
            held = tc.positions.get(event["symbol"], 0)
            if fast > slow and not held:
                tc.order(orders.equity_buy_market(event["symbol"], 10))
            elif fast < slow and held:
                tc.order(orders.equity_sell_market(event["symbol"], held))

    trader = Trader(Client.from_env())
    run = await trader.backtest(crossover, ["AMD", "INTC"], days=20, cash=10_000)
    print(run.report())
    run.to_html("crossover.html")

    session = trader.paper(crossover, ["AMD"], cash=10_000)   # live data, simulated fills
    result = await session.run()                               # until session.stop()

    session = trader.live(crossover, ["AMD"], account_hash=...)  # real orders

All three modes fill through the SDK: backtests and paper trading through the
deterministic paper venue (decimal accounting, finite liquidity, latency,
fees, settlement, an audited transcript), live trading through the journaled
order path with its risk checks, halts and unknown-outcome handling.
"""

from __future__ import annotations

from datetime import date, datetime, timedelta, timezone
from decimal import Decimal
from pathlib import Path
from typing import TYPE_CHECKING, Any, Mapping, Sequence

from .._native import HistoryStore
from ._backtest import RunResult, Simulation, epoch_ms, run_backtest
from ._context import (
    Book,
    Candle,
    Context,
    Event,
    Fill,
    Mode,
    Quote,
    Strategy,
    TrackedOrder,
)
from ._session import Session, record

if TYPE_CHECKING:
    from .._native import Client

__all__ = [
    "Book",
    "Candle",
    "Context",
    "DEFAULT_CACHE",
    "Event",
    "Fill",
    "Mode",
    "Quote",
    "RunResult",
    "Session",
    "Simulation",
    "Strategy",
    "TrackedOrder",
    "Trader",
]

#: Where price history and recordings are cached unless told otherwise.
DEFAULT_CACHE = Path("~/.rustyschwab/history.sqlite3")


def _symbols(symbols: Sequence[str] | str) -> tuple[str, ...]:
    if isinstance(symbols, str):
        symbols = [symbols]
    unique = tuple(dict.fromkeys(symbol.strip().upper() for symbol in symbols))
    if not unique or any(not symbol for symbol in unique):
        raise ValueError("give at least one symbol")
    return unique


def _instant(value: datetime | date | int | None, default: datetime) -> int:
    if value is None:
        return epoch_ms(default)
    if isinstance(value, date) and not isinstance(value, datetime):
        value = datetime(value.year, value.month, value.day, tzinfo=timezone.utc)
    return epoch_ms(value)


class Trader:
    """Runs strategies on one client and one history cache.

    ``client`` may be ``None`` for backtests over history already cached.
    ``cache`` is the :class:`rustyschwab.HistoryStore` file (or an open
    store); candles fetched once are read from it on every later run.
    """

    def __init__(
        self,
        client: Client | None = None,
        *,
        cache: str | Path | HistoryStore = DEFAULT_CACHE,
    ) -> None:
        self.client = client
        self._cache = cache
        self._store: HistoryStore | None = cache if isinstance(cache, HistoryStore) else None

    async def store(self) -> HistoryStore:
        """The history cache, opened on first use."""
        if self._store is None:
            assert not isinstance(self._cache, HistoryStore)
            self._store = await HistoryStore.open(Path(self._cache).expanduser())
        return self._store

    def _client(self) -> Client:
        if self.client is None:
            raise ValueError("this needs a Client; Trader(None) can only backtest cached history")
        return self.client

    async def backtest(
        self,
        strategy: Strategy,
        symbols: Sequence[str] | str,
        *,
        start: datetime | date | int | None = None,
        end: datetime | date | int | None = None,
        days: float = 30,
        minutes: int = 1,
        cash: Decimal | int | str = 10_000,
        simulation: Simulation | None = None,
        level1: bool = False,
        level2: bool = False,
        keep_state: str | Path | None = None,
        limits: Mapping[str, Any] | None = None,
        max_candles: int | None = None,
    ) -> RunResult:
        """Replay ``minutes``-minute bars from ``start`` to ``end`` (default:
        the ``days`` before now) through the paper venue.

        Candles come from the cache, fetched through the client for the
        spans it lacks. Each bar becomes four quotes under
        :class:`Simulation`'s spread and liquidity, and the strategy sees the
        bar once all four have printed, so an order it places fills no
        earlier than the next bar. ``level1`` and ``level2`` also replay the
        quotes and books recorded with :meth:`record` in the window, as
        events; fills still come from the bars.

        ``keep_state`` keeps the venue's state file (a new directory) for
        inspection or ``schwab-paper-verify``; otherwise it is deleted.
        ``limits`` overrides the paper account's portfolio limits.
        """
        if minutes not in (1, 5, 10, 15, 30):
            raise ValueError("minutes must be 1, 5, 10, 15 or 30")
        end_ms = _instant(end, datetime.now(timezone.utc))
        start_ms = (
            _instant(start, datetime.now(timezone.utc))
            if start is not None
            else end_ms - int(timedelta(days=days).total_seconds() * 1000)
        )
        return await run_backtest(
            strategy,
            _symbols(symbols),
            client=self.client,
            store=await self.store(),
            start_ms=start_ms,
            end_ms=end_ms,
            minutes=minutes,
            cash=Decimal(str(cash)),
            simulation=simulation or Simulation(),
            level1=level1,
            level2=level2,
            keep_state=None if keep_state is None else Path(keep_state),
            limits=limits,
            max_candles=max_candles,
        )

    def paper(
        self,
        strategy: Strategy,
        symbols: Sequence[str] | str,
        *,
        cash: Decimal | int | str = 10_000,
        simulation: Simulation | None = None,
        chart: bool = True,
        level1: bool = False,
        level2: bool = False,
        size_multiplier: Decimal | int | str = 1,
        state: str | Path | None = None,
        limits: Mapping[str, Any] | None = None,
        max_candles: int | None = None,
        warmup_days: float = 0,
    ) -> Session:
        """A session that trades live data on the paper venue: nothing is
        sent to Schwab. Level-one quotes always feed the venue (it fills
        against them); ``level1`` decides whether they also wake the
        strategy. The venue's session follows the market's real hours.

        ``size_multiplier`` scales the streamed bid and ask sizes into shares
        (Schwab documents them as shares; set 100 if your feed reports lots).
        ``state`` keeps the venue's state in that directory across runs.
        ``warmup_days`` loads that much minute history into ``tc.candles``
        before the first live bar, through the cache, without waking the
        strategy.
        """
        return Session(
            "paper",
            strategy,
            _symbols(symbols),
            client=self._client(),
            cash=Decimal(str(cash)),
            simulation=simulation,
            chart=chart,
            level1=level1,
            level2=level2,
            size_multiplier=Decimal(str(size_multiplier)),
            state=None if state is None else Path(state),
            limits=limits,
            max_candles=max_candles,
            warmup_days=warmup_days,
            history=self.store,
        )

    def live(
        self,
        strategy: Strategy,
        symbols: Sequence[str] | str,
        *,
        account_hash: str,
        chart: bool = True,
        level1: bool = False,
        level2: bool = False,
        poll_seconds: float = 5.0,
        max_candles: int | None = None,
        warmup_days: float = 0,
    ) -> Session:
        """A session that sends real orders to ``account_hash``.

        Orders go through the SDK's journaled path, so the client's risk
        limits, halts and request budget apply, and an order whose outcome
        is unknown is reconciled rather than resent. Fills are read from the
        account's orders on every account-activity message and every
        ``poll_seconds`` while orders are open. ``warmup_days`` is as for
        :meth:`paper`. Start with small sizes.
        """
        return Session(
            "live",
            strategy,
            _symbols(symbols),
            client=self._client(),
            cash=None,
            account_hash=account_hash,
            chart=chart,
            level1=level1,
            level2=level2,
            poll_seconds=poll_seconds,
            max_candles=max_candles,
            warmup_days=warmup_days,
            history=self.store,
        )

    async def record(
        self,
        symbols: Sequence[str] | str,
        *,
        chart: bool = True,
        level1: bool = True,
        level2: bool = False,
        market_hours: bool = True,
        until: datetime | None = None,
    ) -> int:
        """Record the stream into the cache for later backtests: minute
        candles (``chart``), level-one quotes (``level1``) and NYSE and
        Nasdaq books (``level2``). With ``market_hours`` the stream opens and
        closes with the market's real sessions, so the recorder can run for
        days; otherwise it streams now. Runs until ``until`` or cancellation
        and returns how many payloads it wrote.
        """
        return await record(
            self._client(),
            await self.store(),
            _symbols(symbols),
            chart=chart,
            level1=level1,
            level2=level2,
            market_hours=market_hours,
            until=until,
        )
