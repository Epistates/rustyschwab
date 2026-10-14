"""The loop shared by every mode: wake the strategy, send what it placed, and
keep its orders, fills and account in step with the venue."""

from __future__ import annotations

from decimal import Decimal
from typing import TYPE_CHECKING, Any, Iterable, Mapping, Protocol

from ..errors import PendingSubmissionError, SchwabError
from ._context import Context, Event, Fill, Strategy, TrackedOrder, call_hook, call_strategy, log

if TYPE_CHECKING:
    from .._native import Client, PaperBroker


def dec(value: Any) -> Decimal:
    return value if isinstance(value, Decimal) else Decimal(str(value))


class Venue(Protocol):
    """Where orders go and where the account lives."""

    async def submit(self, order: Mapping[str, Any]) -> str | None:
        """Send an order; its broker order ID, or ``None`` if not yet known."""

    async def cancel(self, order_id: str) -> None: ...

    async def sync(self, tc: Context, runner: Runner) -> None:
        """Refresh the account and the open orders' outcomes."""


class Runner:
    """Drives one strategy against one venue."""

    def __init__(self, strategy: Strategy, tc: Context, venue: Venue) -> None:
        self.strategy = strategy
        self.tc = tc
        self.venue = venue
        self.by_order_id: dict[str, TrackedOrder] = {}
        #: Orders sent and not yet finished, by broker order ID.
        self.open: dict[str, TrackedOrder] = {}
        #: Every fill, in time order, for the performance report.
        self.fills: list[Fill] = []
        #: ``(at_ms, equity)`` after every wake.
        self.equity: list[tuple[int, Decimal]] = []
        self._unreported: list[Fill] = []

    async def start(self, *, send: bool = True) -> None:
        """Run ``on_start``. With ``send=False`` what it places waits for the
        first wake, when the venue has a market to accept it against."""
        await call_hook(self.strategy, "on_start", self.tc)
        if send:
            await self.flush()

    async def stop(self) -> None:
        await call_hook(self.strategy, "on_stop", self.tc)
        await self.flush()

    async def wake(self, events: list[Event]) -> bool:
        """Deliver ``events`` (and any fills since the last wake), then send
        what the strategy placed. Whether anything was sent."""
        pending, self._unreported = self._unreported, []
        delivered: list[Event] = [*pending, *events]
        delivered.sort(key=lambda event: event["time"])
        self.tc._ingest(events)
        if delivered:
            await call_strategy(self.strategy, self.tc, delivered)
        return await self.flush()

    async def flush(self) -> bool:
        """Send queued placements and cancels in the order they were made.
        Whether there were any."""
        actions = self.tc._take_actions()
        for action in actions:
            tracked = self.tc.orders[action.tag]
            if action.kind == "cancel":
                await self._cancel(tracked)
            elif tracked.status == "PENDING":
                await self._place(tracked)
        return bool(actions)

    def settle(self, tracked: TrackedOrder, status: str, at_ms: int) -> None:
        """Record an order's status from the venue."""
        tracked.status = status
        tracked.updated_at = at_ms
        if not tracked.is_open and tracked.order_id is not None:
            self.open.pop(tracked.order_id, None)

    async def _place(self, tracked: TrackedOrder) -> None:
        try:
            order_id = await self.venue.submit(tracked.order)
        except PendingSubmissionError as error:
            tracked.status = "UNKNOWN"
            tracked.reason = str(error)
            log.warning("order %s: outcome unknown (%s); reconciling", tracked.tag, error)
            return
        except (SchwabError, ValueError) as error:
            tracked.status = "REJECTED"
            tracked.reason = str(error)
            log.info("order %s rejected: %s", tracked.tag, error)
            return
        tracked.updated_at = self.tc.now
        if order_id is None:
            tracked.status = "UNKNOWN"
            return
        self.bind(tracked, order_id)
        tracked.status = "WORKING"

    async def _cancel(self, tracked: TrackedOrder) -> None:
        if tracked.status == "PENDING":
            tracked.status = "CANCELED"
            tracked.reason = "withdrawn before it was sent"
            return
        if tracked.order_id is None or not tracked.is_open:
            return
        try:
            await self.venue.cancel(tracked.order_id)
        except SchwabError as error:
            log.info("cancel of %s refused: %s", tracked.tag, error)
            return
        if tracked.status == "WORKING":
            tracked.status = "PENDING_CANCEL"

    def bind(self, tracked: TrackedOrder, order_id: str) -> None:
        tracked.order_id = order_id
        self.by_order_id[order_id] = tracked
        self.open[order_id] = tracked

    async def record_fills(self, fills: Iterable[Fill]) -> list[Fill]:
        """Apply new fills to their orders and queue them for the next wake."""
        new = []
        for fill in fills:
            tracked = self.tc.orders.get(fill["tag"])
            if tracked is None:
                continue
            tracked.record_fill(fill)
            if not tracked.is_open and tracked.order_id is not None:
                self.open.pop(tracked.order_id, None)
            self.fills.append(fill)
            self._unreported.append(fill)
            new.append(fill)
            await call_hook(self.strategy, "on_fill", self.tc, fill)
        return new

    def fill_for(
        self, order_id: str, at_ms: int, quantity: Any, price: Any, fee: Any = 0
    ) -> Fill | None:
        tracked = self.by_order_id.get(order_id)
        if tracked is None:
            return None
        return {
            "type": "fill",
            "symbol": tracked.symbol,
            "time": int(at_ms),
            "tag": tracked.tag,
            "order_id": order_id,
            "side": tracked.instruction,
            "quantity": dec(quantity),
            "price": dec(price),
            "fee": dec(fee),
        }

    def mark_equity(self) -> None:
        point = (self.tc.now, self.tc.equity())
        if self.equity and self.equity[-1][0] == point[0]:
            self.equity[-1] = point
        else:
            self.equity.append(point)


class PaperVenue:
    """The SDK's paper venue: backtests and paper trading on live data."""

    def __init__(self, broker: PaperBroker, account: str) -> None:
        self.broker = broker
        self.account = account

    async def submit(self, order: Mapping[str, Any]) -> str | None:
        submitted = await self.broker.place_order(self.account, order)
        return submitted["order_id"]

    async def cancel(self, order_id: str) -> None:
        await self.broker.cancel_order(self.account, order_id)

    async def fills_from(self, runner: Runner, results: Iterable[Mapping[str, Any]]) -> int:
        """Record the fills in event results; how many there were."""
        fills = []
        for result in results:
            for raw in result.get("fills") or ():
                fill = runner.fill_for(
                    str(raw["order_id"]), raw["at_ms"], raw["quantity"], raw["price"], raw["fee"]
                )
                if fill is not None:
                    fills.append(fill)
        await runner.record_fills(fills)
        return len(fills)

    async def sync(self, tc: Context, runner: Runner) -> None:
        snapshot = await self.broker.snapshot(self.account)
        ledger = snapshot["ledger"]
        tc.cash = dec(snapshot["available_cash"])
        tc._ledger_cash = dec(ledger["cash"])
        tc.positions = {
            symbol: dec(position["quantity"])
            for symbol, position in (ledger.get("positions") or {}).items()
            if dec(position["quantity"]) != 0
        }
        tc.now = max(tc.now, int(snapshot["at_ms"]))
        working = {str(order["order_id"]): order for order in snapshot.get("working_orders") or ()}
        for order_id, tracked in list(runner.open.items()):
            if order_id in working:
                runner.settle(tracked, str(working[order_id]["status"]), tc.now)
            else:
                record = await self.broker.order(self.account, order_id)
                runner.settle(tracked, str(record["status"]), tc.now)


class LiveVenue:
    """A Schwab account, through the SDK's journaled order path."""

    def __init__(self, client: Client, account_hash: str) -> None:
        self.client = client
        self.account_hash = account_hash
        self._seen: set[str] = set()

    async def submit(self, order: Mapping[str, Any]) -> str | None:
        submitted = await self.client.place_order(self.account_hash, order)
        return submitted["order_id"]

    async def cancel(self, order_id: str) -> None:
        await self.client.cancel_order(self.account_hash, order_id)

    async def sync(self, tc: Context, runner: Runner) -> None:
        account = await self.client.account(self.account_hash, include_positions=True)
        apply_account(tc, account)
        for tracked in list(tc.orders.values()):
            if not tracked.is_open or tracked.status == "PENDING":
                continue
            if tracked.order_id is None:
                await self._find(tracked, runner)
                if tracked.order_id is None:
                    continue
            order = await self.client.order(self.account_hash, tracked.order_id)
            fills = self._new_fills(runner, tracked.order_id, order)
            fills.sort(key=lambda fill: fill["time"])
            await runner.record_fills(fills)
            # Schwab's status is the outcome, even when an execution's legs
            # were not in the order's activity list.
            tracked.reason = order.get("statusDescription") or tracked.reason
            runner.settle(tracked, str(order.get("status") or tracked.status), tc.now)

    async def _find(self, tracked: TrackedOrder, runner: Runner) -> None:
        """Look an order whose ID never came back up by its tag."""
        from datetime import datetime, timedelta, timezone

        placed = datetime.fromtimestamp(tracked.placed_at / 1000, timezone.utc)
        orders = await self.client.account_orders(
            self.account_hash,
            {
                "fromEnteredTime": (placed - timedelta(minutes=5)).isoformat(),
                "toEnteredTime": datetime.now(timezone.utc).isoformat(),
            },
        )
        for order in orders:
            if order.get("tag") == tracked.tag and order.get("orderId") is not None:
                runner.bind(tracked, str(order["orderId"]))
                tracked.status = str(order.get("status") or "WORKING")
                return

    def _new_fills(self, runner: Runner, order_id: str, order: Mapping[str, Any]) -> list[Fill]:
        out = []
        for index, activity in enumerate(order.get("orderActivityCollection") or ()):
            if activity.get("activityType") != "EXECUTION":
                continue
            if activity.get("executionType", "FILL") != "FILL":
                continue
            base = activity.get("activityId", index)
            for leg_index, leg in enumerate(activity.get("executionLegs") or ()):
                key = f"{order_id}:{base}:{leg_index}"
                if key in self._seen or leg.get("quantity") is None or leg.get("price") is None:
                    continue
                self._seen.add(key)
                fill = runner.fill_for(
                    order_id, _epoch_ms(leg.get("time")), leg["quantity"], leg["price"]
                )
                if fill is not None:
                    out.append(fill)
        return out


def _epoch_ms(value: Any) -> int:
    from datetime import datetime, timezone

    if isinstance(value, (int, float)):
        return int(value)
    if isinstance(value, str):
        try:
            moment = datetime.fromisoformat(value.replace("Z", "+00:00"))
        except ValueError:
            pass
        else:
            if moment.tzinfo is None:
                moment = moment.replace(tzinfo=timezone.utc)
            return int(moment.timestamp() * 1000)
    return int(datetime.now(timezone.utc).timestamp() * 1000)


def apply_account(tc: Context, account: Mapping[str, Any]) -> None:
    """Cash and positions from Schwab's account JSON.

    Spendable cash is ``cashAvailableForTrading`` for a cash account and
    ``availableFunds`` for margin; positions net long against short.
    """
    securities = account.get("securitiesAccount") or account
    balances = securities.get("currentBalances") or {}
    margin = securities.get("type") == "MARGIN"
    names = ("availableFunds", "cashAvailableForTrading") if margin else (
        "cashAvailableForTrading",
        "availableFunds",
    )
    for name in names:
        if balances.get(name) is not None:
            tc.cash = dec(balances[name])
            break
    if balances.get("cashBalance") is not None:
        tc._ledger_cash = dec(balances["cashBalance"])
    positions: dict[str, Decimal] = {}
    for position in securities.get("positions") or ():
        symbol = (position.get("instrument") or {}).get("symbol")
        if not symbol:
            continue
        quantity = dec(position.get("longQuantity") or 0) - dec(position.get("shortQuantity") or 0)
        if quantity:
            positions[str(symbol)] = positions.get(str(symbol), Decimal(0)) + quantity
        if position.get("marketValue") is not None and quantity:
            tc._marks.setdefault(str(symbol), abs(dec(position["marketValue"]) / quantity))
    tc.positions = positions
