"""What a strategy sees and does: the context, the events and tracked orders."""

from __future__ import annotations

import inspect
import logging
import uuid
from dataclasses import dataclass, field
from decimal import Decimal
from typing import (
    TYPE_CHECKING,
    Any,
    Awaitable,
    Callable,
    Literal,
    Mapping,
    Protocol,
    TypedDict,
    Union,
)

if TYPE_CHECKING:
    from typing_extensions import TypeAlias

Mode = Literal["backtest", "paper", "live"]

#: Order statuses as Schwab spells them, plus ``PENDING`` (placed by the
#: strategy, not yet sent) and ``UNKNOWN`` (sent, outcome not yet known).
OPEN_STATUSES = frozenset(
    {
        "PENDING",
        "UNKNOWN",
        "WORKING",
        "PENDING_CANCEL",
        "QUEUED",
        "ACCEPTED",
        "AWAITING_PARENT_ORDER",
        "AWAITING_CONDITION",
        "AWAITING_STOP_CONDITION",
        "AWAITING_MANUAL_REVIEW",
        "AWAITING_UR_OUT",
        "AWAITING_RELEASE_TIME",
        "PENDING_ACTIVATION",
        "PENDING_ACKNOWLEDGEMENT",
        "PENDING_REPLACE",
        "PENDING_RECALL",
        "NEW",
    }
)

_BUYS = frozenset({"BUY", "BUY_TO_COVER", "BUY_TO_OPEN", "BUY_TO_CLOSE"})

log = logging.getLogger("rustyschwab.trader")


class Candle(TypedDict):
    """A completed bar. ``time`` is when it opened, in epoch milliseconds."""

    type: Literal["candle"]
    symbol: str
    time: int
    open: float
    high: float
    low: float
    close: float
    volume: float


class Quote(TypedDict, total=False):
    """A level-one update. Fields the update did not change are absent; the
    merged latest quote is ``tc.quotes[symbol]``."""

    type: Literal["quote"]
    symbol: str
    time: int
    bid: float
    ask: float
    last: float
    bid_size: float
    ask_size: float
    last_size: float
    volume: float


class Book(TypedDict):
    """A book snapshot: ``bids`` and ``asks`` are Schwab's price levels."""

    type: Literal["book"]
    symbol: str
    time: int
    bids: list[Any]
    asks: list[Any]


class Fill(TypedDict):
    """An execution of one of the strategy's orders."""

    type: Literal["fill"]
    symbol: str
    time: int
    tag: str
    order_id: str
    side: str
    quantity: Decimal
    price: Decimal
    fee: Decimal


Event: TypeAlias = Union[Candle, Quote, Book, Fill]


class StrategyFunction(Protocol):
    def __call__(
        self, tc: Context, events: list[Event], /
    ) -> Awaitable[None] | None: ...


#: A callable ``strategy(tc, events)``, plain or ``async``. An object may also
#: define ``on_start(tc)``, ``on_fill(tc, fill)`` and ``on_stop(tc)``.
Strategy = StrategyFunction


@dataclass
class TrackedOrder:
    """One order the strategy placed, as the runner last saw it."""

    tag: str
    symbol: str
    instruction: str
    quantity: Decimal
    order: dict[str, Any]
    placed_at: int
    status: str = "PENDING"
    order_id: str | None = None
    filled_quantity: Decimal = Decimal(0)
    average_price: Decimal | None = None
    fees: Decimal = Decimal(0)
    reason: str | None = None
    updated_at: int = 0
    fills: list[Fill] = field(default_factory=list)

    @property
    def is_open(self) -> bool:
        return self.status in OPEN_STATUSES

    @property
    def buys(self) -> bool:
        return self.instruction in _BUYS

    @property
    def remaining(self) -> Decimal:
        return self.quantity - self.filled_quantity

    def record_fill(self, fill: Fill) -> None:
        cost = (self.average_price or Decimal(0)) * self.filled_quantity
        self.filled_quantity += fill["quantity"]
        self.average_price = (cost + fill["price"] * fill["quantity"]) / self.filled_quantity
        self.fees += fill["fee"]
        self.updated_at = fill["time"]
        self.fills.append(fill)
        if self.filled_quantity >= self.quantity:
            self.status = "FILLED"


@dataclass
class _Action:
    kind: Literal["place", "cancel"]
    tag: str


def _single_leg(order: Mapping[str, Any]) -> tuple[str, str, Decimal]:
    legs = order.get("orderLegCollection") or []
    if len(legs) != 1:
        raise ValueError(
            "the runner tracks single-leg orders; build multi-leg orders with "
            "rustyschwab.orders and place them through the Client"
        )
    leg = legs[0]
    try:
        symbol = str(leg["instrument"]["symbol"])
        instruction = str(leg["instruction"])
        quantity = Decimal(str(leg["quantity"]))
    except (KeyError, TypeError, ArithmeticError) as error:
        raise ValueError(f"order leg lacks a symbol, instruction or quantity: {error}") from None
    return symbol, instruction, quantity


class Context:
    """Everything a strategy can see or do, the same in every mode.

    Read ``candles``, ``quotes``, ``books``, ``positions``, ``cash``,
    ``orders`` and ``now``; act with :meth:`order` and :meth:`cancel`; draw
    with :meth:`plot`. Orders placed during a call are sent, in order, when
    the strategy returns; :meth:`order` returns the order's tag at once, and
    ``tc.orders[tag]`` follows it from ``PENDING`` to its outcome.

    Amounts the venue reports (``cash``, ``positions``, order prices) are
    :class:`~decimal.Decimal`; market data (candles, quotes) keeps Schwab's
    floats.
    """

    def __init__(
        self,
        mode: Mode,
        symbols: tuple[str, ...],
        starting_cash: Decimal,
        *,
        max_candles: int | None = None,
    ) -> None:
        self.mode: Mode = mode
        self.symbols = symbols
        self.starting_cash = starting_cash
        self.now = 0
        self.candles: dict[str, list[Candle]] = {symbol: [] for symbol in symbols}
        self.quotes: dict[str, dict[str, Any]] = {}
        self.books: dict[str, Book] = {}
        self.positions: dict[str, Decimal] = {}
        self.cash = starting_cash
        self.orders: dict[str, TrackedOrder] = {}
        self.plots: dict[str, list[tuple[int, float]]] = {}
        #: Free for the strategy's own state; the runner never touches it.
        self.state: dict[str, Any] = {}
        self.log = log
        self._ledger_cash = starting_cash
        self._marks: dict[str, Decimal] = {}
        self._actions: list[_Action] = []
        self._max_candles = max_candles

    # ---- reading -------------------------------------------------------

    def sellable(self, symbol: str) -> Decimal:
        """Shares held less those committed to open sell orders."""
        committed = sum(
            (o.remaining for o in self.orders.values() if o.symbol == symbol and o.is_open and not o.buys),
            Decimal(0),
        )
        return max(self.positions.get(symbol, Decimal(0)) - committed, Decimal(0))

    def equity(self) -> Decimal:
        """Account value: all cash, including cash set aside for open buys,
        plus positions at their latest price (the last close or trade)."""
        value = self._ledger_cash
        for symbol, quantity in self.positions.items():
            value += quantity * self._marks.get(symbol, Decimal(0))
        return value

    def working(self, symbol: str | None = None) -> list[TrackedOrder]:
        """Open orders, optionally of one symbol."""
        return [
            o for o in self.orders.values() if o.is_open and (symbol is None or o.symbol == symbol)
        ]

    # ---- acting --------------------------------------------------------

    def order(self, order: Mapping[str, Any]) -> str:
        """Place an order (Schwab's JSON shape; see :mod:`rustyschwab.orders`).

        Returns its tag: the order's own ``tag`` if it has one, else a new
        ``rs-<uuid>``. A tag names one order; reusing one raises
        ``ValueError``. The order is sent when the strategy returns.
        """
        body = dict(order)
        symbol, instruction, quantity = _single_leg(body)
        tag = str(body.get("tag") or f"rs-{uuid.uuid4()}")
        if tag in self.orders:
            raise ValueError(f"tag {tag!r} already names an order")
        body["tag"] = tag
        self.orders[tag] = TrackedOrder(
            tag=tag,
            symbol=symbol,
            instruction=instruction,
            quantity=quantity,
            order=body,
            placed_at=self.now,
            updated_at=self.now,
        )
        self._actions.append(_Action("place", tag))
        return tag

    def cancel(self, tag: str) -> bool:
        """Ask to cancel an open order. Returns ``False`` if it is not open.

        An order still ``PENDING`` is withdrawn before it is sent.
        """
        tracked = self.orders.get(tag)
        if tracked is None or not tracked.is_open:
            return False
        self._actions.append(_Action("cancel", tag))
        return True

    def plot(self, name: str, value: float, *, at: int | None = None) -> None:
        """Add a point to the overlay ``name``. A name containing a symbol is
        drawn on that symbol's chart; any other on every chart."""
        self.plots.setdefault(name, []).append((self.now if at is None else at, float(value)))

    # ---- runner side ---------------------------------------------------

    def _take_actions(self) -> list[_Action]:
        actions, self._actions = self._actions, []
        return actions

    def _ingest(self, events: list[Event]) -> None:
        for event in events:
            symbol = event["symbol"]
            if event["type"] == "candle":
                candles = self.candles.setdefault(symbol, [])
                candles.append(event)
                if self._max_candles is not None and len(candles) > self._max_candles:
                    del candles[: len(candles) - self._max_candles]
                self._marks[symbol] = Decimal(str(event["close"]))
            elif event["type"] == "quote":
                merged = self.quotes.setdefault(symbol, {"symbol": symbol})
                merged.update({k: v for k, v in event.items() if k != "type"})
                last = event.get("last")
                if last is not None:
                    self._marks[symbol] = Decimal(str(last))
            elif event["type"] == "book":
                self.books[symbol] = event


async def call_strategy(strategy: Strategy, tc: Context, events: list[Event]) -> None:
    result = strategy(tc, events)
    if inspect.isawaitable(result):
        await result


async def call_hook(strategy: Any, name: str, *args: Any) -> None:
    hook: Callable[..., Any] | None = getattr(strategy, name, None)
    if hook is None:
        return
    result = hook(*args)
    if inspect.isawaitable(result):
        await result
