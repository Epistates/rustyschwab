"""The execution client: orders to Schwab (or the paper venue), reports back.

How it keeps Nautilus right about orders it cannot watch live:

* **Commands** (submit, modify, cancel) emit their own events from the answer
  to the request: submitted, accepted, rejected, updated, cancel-rejected.
* **Everything that happens after** (fills, cancels, expiry, a replacement
  that was accepted after its acknowledgement was lost) is found by reading
  the account's orders every ``poll_interval_secs``, and immediately after the
  adapter places or cancels an order. Each order read from Schwab is matched to
  its Nautilus order by its tag, and the difference becomes events. Schwab's
  streamer is not used for this: it allows one session per user, and the data
  client holds it.
* **Reconciliation** (startup, and the engine's in-flight and open-order
  checks) is served by the ``generate_*_report`` methods from the same parsed
  orders.

An order whose outcome is unknown is never sent again. The SDK raises
``SubmissionUnknown`` for it; the order stays ``SUBMITTED``, the polls find it
by its tag if Schwab has it, and the journal ``attempt_id`` is logged for the
operator to resolve.
"""

from __future__ import annotations

import asyncio
import time
from collections.abc import Coroutine
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from decimal import Decimal
from typing import Any
from nautilus_trader.cache.cache import Cache
from nautilus_trader.common.component import LiveClock, MessageBus
from nautilus_trader.common.enums import LogLevel
from nautilus_trader.common.providers import InstrumentProvider
from nautilus_trader.execution.messages import (
    BatchCancelOrders,
    CancelAllOrders,
    CancelOrder,
    GenerateFillReports,
    GenerateOrderStatusReport,
    GenerateOrderStatusReports,
    GeneratePositionStatusReports,
    ModifyOrder,
    QueryAccount,
    SubmitOrder,
    SubmitOrderList,
)
from nautilus_trader.execution.reports import FillReport, OrderStatusReport, PositionStatusReport
from nautilus_trader.live.execution_client import LiveExecutionClient
from nautilus_trader.model.data import QuoteTick
from nautilus_trader.model.enums import AccountType, LiquiditySide, OmsType, OrderStatus
from nautilus_trader.model.identifiers import (
    AccountId,
    ClientId,
    ClientOrderId,
    InstrumentId,
    TradeId,
    VenueOrderId,
)
from nautilus_trader.model.objects import Money
from nautilus_trader.model.orders import Order

from rustyschwab.errors import (
    BrokerError,
    RateLimited,
    ReconciliationRequired,
    SchwabError,
    SubmissionUnknown,
)

from .backend import Backend
from .config import SchwabExecClientConfig
from .constants import ORDER_HISTORY_DAYS, SCHWAB, SCHWAB_CURRENCY, SCHWAB_VENUE
from .ledger import LedgerEntry, OrderLedger
from .orders import (
    SchwabFill,
    SchwabOrder,
    UnsupportedOrder,
    instruction_for,
    parse_order,
    spec_from_order,
    split_tag,
    tag_for,
    to_schwab_order,
)
from .paper import PaperBackend
from .providers import SchwabInstrumentProvider
from .reports import (
    account_balances,
    fill_reports,
    make_price,
    make_quantity,
    order_status_report,
    position_reports,
)

#: Individual reads per poll for working orders the list did not return.
_STRAY_READS = 20
_QUOTE_TOPIC = f"data.quotes.{SCHWAB}.*"


@dataclass
class _Progress:
    """What has been emitted for one order, ahead of the engine's cache.

    Events are queued to the execution engine and applied later, so the cache
    lags what this client has already said. Deciding from the cache alone would
    repeat an event on the next poll.
    """

    venue_order_id: str | None
    accepted: bool
    filled: Decimal
    fills: set[str] = field(default_factory=set)
    closed: bool = False


def _aware(moment: datetime) -> datetime:
    return moment if moment.tzinfo is not None else moment.replace(tzinfo=timezone.utc)


def _reason(error: SchwabError) -> str:
    """A rejection reason for logs and events: the SDK's message plus Schwab's errors."""
    text = f"{type(error).__name__}: {error}"
    if isinstance(error, BrokerError) and error.errors:
        text += f" {str(error.errors)[:300]}"
    return text


class SchwabExecutionClient(LiveExecutionClient):
    """Submits, modifies and cancels orders, and reports orders, fills and positions.

    Parameters
    ----------
    loop : asyncio.AbstractEventLoop
        The event loop for the client.
    backend : Backend
        Schwab's Trader API, or the paper venue.
    msgbus, cache, clock
        The node's message bus, cache and clock.
    instrument_provider : InstrumentProvider
        A :class:`SchwabInstrumentProvider` live; any provider in paper mode.
    config : SchwabExecClientConfig
        The client configuration.
    name : str, optional
        The client ID, ``SCHWAB`` by default. It is also the issuer in the
        account ID, so it must not contain a hyphen.
    """

    def __init__(
        self,
        loop: asyncio.AbstractEventLoop,
        backend: Backend,
        msgbus: MessageBus,
        cache: Cache,
        clock: LiveClock,
        instrument_provider: InstrumentProvider,
        config: SchwabExecClientConfig,
        name: str | None = None,
    ) -> None:
        super().__init__(
            loop=loop,
            client_id=ClientId(name or SCHWAB),
            venue=SCHWAB_VENUE,
            oms_type=OmsType.NETTING,
            account_type=(
                AccountType.CASH if config.resolved_account_type == "CASH" else AccountType.MARGIN
            ),
            base_currency=SCHWAB_CURRENCY,
            instrument_provider=instrument_provider,
            msgbus=msgbus,
            cache=cache,
            clock=clock,
            config=config,
        )
        self._backend = backend
        self._config = config
        self._ledger = OrderLedger(cache)
        self._progress: dict[ClientOrderId, _Progress] = {}
        self._wake = asyncio.Event()
        self._closing = False
        self._monitor: asyncio.Task[None] | None = None
        self._feeder: asyncio.Task[None] | None = None
        self._quotes: asyncio.Queue[QuoteTick] = asyncio.Queue()
        self._orphans: set[str] = set()
        if backend.account_hash is not None:
            self._set_account_id(AccountId(f"{self.id.value}-{backend.account_hash}"))

    # -- lifecycle ---------------------------------------------------------------

    async def _connect(self) -> None:
        self._closing = False
        reported = await self._backend.connect()
        expected = self._config.resolved_account_type
        if reported and reported != expected:
            raise ValueError(
                f"Schwab reports a {reported} account but account_type is {expected}; "
                "set account_type to match"
            )
        if self.account_id is None:
            self._set_account_id(AccountId(f"{self.id.value}-{self._backend.account_hash}"))
        providers = self._config.instrument_provider
        if providers.load_all or providers.load_ids:
            await self._instrument_provider.initialize()
        await self._load_held_instruments()
        await self._update_account_state()
        await self._await_account_registered()
        if isinstance(self._backend, PaperBackend):
            self._msgbus.subscribe(topic=_QUOTE_TOPIC, handler=self._on_quote)
            self._feeder = self._spawn(self._feed_paper(), "rustyschwab-paper-quote-feed")
        self._monitor = self._spawn(self._monitor_orders(), "schwab-order-monitor")

    async def _disconnect(self) -> None:
        self._closing = True
        self._wake.set()
        if isinstance(self._backend, PaperBackend):
            self._msgbus.unsubscribe(topic=_QUOTE_TOPIC, handler=self._on_quote)
        for task in (self._monitor, self._feeder):
            if task is not None:
                task.cancel()
        await asyncio.gather(
            *(t for t in (self._monitor, self._feeder) if t is not None), return_exceptions=True
        )
        self._monitor = self._feeder = None
        await self._backend.close()

    def _spawn(self, coro: Coroutine[Any, Any, None], name: str) -> asyncio.Task[None]:
        """A background task that runs until disconnect, and logs how it died if it did."""
        task: asyncio.Task[None] = self._loop.create_task(coro, name=name)
        task.add_done_callback(self._background_done)
        return task

    def _background_done(self, task: asyncio.Task[None]) -> None:
        if not task.cancelled() and task.exception() is not None:
            self._log.error(f"Background task {task.get_name()} stopped: {task.exception()!r}")

    async def _load_held_instruments(self) -> None:
        """Load the instruments of positions and working orders so reports can use them."""
        provider = self._instrument_provider
        if not isinstance(provider, SchwabInstrumentProvider):
            return
        now = datetime.now(timezone.utc)
        wanted: set[InstrumentId] = set()
        try:
            account = await self._backend.account()
            wanted.update(r.instrument_id for r in position_reports(
                account, account_id=self.account_id, ts_init=self._ts()
            ))
            for raw in await self._backend.orders(
                now - timedelta(days=ORDER_HISTORY_DAYS), now + timedelta(minutes=1)
            ):
                view = parse_order(raw)
                if view is not None and view.is_open:
                    wanted.add(view.instrument_id)
        except SchwabError as error:
            self._log.warning(f"Could not read held instruments: {error}")
            return
        missing = [i for i in wanted if self._cache.instrument(i) is None and provider.find(i) is None]
        if missing:
            await provider.load_ids_async(missing)
        for instrument_id in wanted:
            instrument = provider.find(instrument_id)
            if instrument is not None and self._cache.instrument(instrument_id) is None:
                self._cache.add_instrument(instrument)

    def _ts(self) -> int:
        return int(self._clock.timestamp_ns())

    # -- account -----------------------------------------------------------------

    async def _update_account_state(self) -> None:
        account = await self._backend.account()
        balances, info = account_balances(account, self._config.resolved_account_type)
        self.generate_account_state(
            balances=balances, margins=[], reported=True, ts_event=self._ts(), info=info
        )

    async def _query_account(self, command: QueryAccount) -> None:
        await self._update_account_state()

    def _net_position(self, instrument_id: InstrumentId) -> Decimal:
        total = Decimal(0)
        for position in self._cache.positions_open(
            instrument_id=instrument_id, account_id=self.account_id
        ):
            total += Decimal(str(position.signed_qty))
        return total

    # -- commands ----------------------------------------------------------------

    def _deny(self, order: Order, reason: str) -> None:
        self._log.warning(f"Denied {order.client_order_id!r}: {reason}")
        self.generate_order_denied(
            strategy_id=order.strategy_id,
            instrument_id=order.instrument_id,
            client_order_id=order.client_order_id,
            reason=reason,
            ts_event=self._ts(),
        )

    def _reject(self, order: Order, reason: str) -> None:
        self._log.error(f"Rejected {order.client_order_id!r}: {reason}")
        progress = self._progress.get(order.client_order_id)
        if progress is not None:
            progress.closed = True
        self.generate_order_rejected(
            strategy_id=order.strategy_id,
            instrument_id=order.instrument_id,
            client_order_id=order.client_order_id,
            reason=reason,
            ts_event=self._ts(),
        )

    async def _submit_order(self, command: SubmitOrder) -> None:
        order = command.order
        instrument = self._cache.instrument(order.instrument_id)
        if instrument is None:
            self._deny(order, f"instrument {order.instrument_id} is not loaded")
            return
        try:
            spec = spec_from_order(order, instrument)
            instruction = instruction_for(
                spec, self._net_position(order.instrument_id), self._config.allow_short_positions
            )
            body = to_schwab_order(
                spec,
                tag=tag_for(order.client_order_id),
                instruction=instruction,
                session=self._config.order_session,
            )
        except UnsupportedOrder as error:
            self._deny(order, str(error))
            return
        self.generate_order_submitted(
            strategy_id=order.strategy_id,
            instrument_id=order.instrument_id,
            client_order_id=order.client_order_id,
            ts_event=self._ts(),
        )
        self._progress[order.client_order_id] = _Progress(None, False, Decimal(0))
        try:
            submitted = await self._backend.place_order(body)
        except SubmissionUnknown as error:
            self._log.error(
                f"Outcome of {order.client_order_id!r} is unknown (journal attempt "
                f"{error.attempt_id}). It will not be sent again; Schwab's orders are "
                "being read to find it. If it never appears, resolve the attempt with "
                "`rustyschwab-admin reconcile`."
            )
            self._ledger.record(
                order.client_order_id,
                LedgerEntry(tag=body["tag"], attempt_id=error.attempt_id),
            )
            self._wake.set()
            return
        except ReconciliationRequired as error:
            # Refused before anything was sent: an earlier order is unresolved.
            self._reject(
                order,
                f"ReconciliationRequired: journal attempt {error.attempt_id} on this account "
                "must be resolved first; nothing was sent",
            )
            self._wake.set()
            return
        except SchwabError as error:
            self._reject(order, _reason(error))
            return
        self._ledger.record(
            order.client_order_id,
            LedgerEntry(
                tag=body["tag"],
                attempt_id=submitted.get("attempt_id"),
                venue_order_id=submitted.get("order_id"),
            ),
        )
        order_id = submitted.get("order_id")
        if order_id is None:
            self._log.warning(
                f"Schwab accepted {order.client_order_id!r} without an order ID; "
                "it will be found by its tag"
            )
            self._wake.set()
            return
        self._emit_accepted(order, order_id, self._ts())
        self._wake.set()

    async def _submit_order_list(self, command: SubmitOrderList) -> None:
        for order in command.order_list.orders:
            self._deny(
                order,
                "order lists (brackets, OCO, OTO) are not supported; submit the orders individually",
            )

    async def _modify_order(self, command: ModifyOrder) -> None:
        order = self._cache.order(command.client_order_id)
        if order is None:
            self._log.error(f"Cannot modify {command.client_order_id!r}: not in the cache")
            return
        venue_order_id = command.venue_order_id or order.venue_order_id
        if venue_order_id is None:
            self._modify_rejected(order, "the order has no Schwab order ID yet")
            return
        instrument = self._cache.instrument(order.instrument_id)
        progress = self._progress.get(order.client_order_id)
        filled = progress.filled if progress is not None else order.filled_qty.as_decimal()
        if instrument is None or filled > 0:
            self._modify_rejected(
                order,
                "a partly filled order cannot be replaced; cancel it and send the remainder",
            )
            return
        try:
            spec = spec_from_order(order, instrument).modified(
                quantity=int(command.quantity.as_decimal()) if command.quantity else None,
                price=command.price.as_decimal() if command.price else None,
                trigger_price=command.trigger_price.as_decimal() if command.trigger_price else None,
            )
            current = await self._backend.order(venue_order_id.value)
            instruction = str(current["orderLegCollection"][0]["instruction"])
            ledger = self._ledger.get(order.client_order_id)
            revision = max(
                split_tag(str(current.get("tag") or ""))[1], ledger.revision if ledger else 0
            ) + 1
            body = to_schwab_order(
                spec,
                tag=tag_for(order.client_order_id, revision),
                instruction=instruction,
                session=self._config.order_session,
            )
            submitted = await self._backend.replace_order(venue_order_id.value, body)
        except SubmissionUnknown as error:
            self._log.error(
                f"Outcome of replacing {order.client_order_id!r} is unknown (journal attempt "
                f"{error.attempt_id}); it will not be sent again. The replacement is looked for "
                "by its tag."
            )
            self._wake.set()
            return
        except (UnsupportedOrder, SchwabError, KeyError, IndexError) as error:
            reason = _reason(error) if isinstance(error, SchwabError) else str(error)
            self._modify_rejected(order, reason)
            return
        new_id = submitted.get("order_id")
        self._ledger.record(
            order.client_order_id,
            LedgerEntry(
                tag=body["tag"],
                revision=revision,
                attempt_id=submitted.get("attempt_id"),
                venue_order_id=new_id,
            ),
        )
        if new_id is None:
            self._log.warning(f"Schwab replaced {order.client_order_id!r} without an order ID")
            self._wake.set()
            return
        if progress is not None:
            if progress.venue_order_id == new_id:
                return  # a poll saw the replacement first and has already said so
            progress.venue_order_id = new_id
        self.generate_order_updated(
            strategy_id=order.strategy_id,
            instrument_id=order.instrument_id,
            client_order_id=order.client_order_id,
            venue_order_id=VenueOrderId(new_id),
            quantity=command.quantity or order.quantity,
            price=command.price or getattr(order, "price", None),
            trigger_price=command.trigger_price or getattr(order, "trigger_price", None),
            ts_event=self._ts(),
            venue_order_id_modified=True,
        )
        self._wake.set()

    def _modify_rejected(self, order: Order, reason: str) -> None:
        self._log.error(f"Modify of {order.client_order_id!r} rejected: {reason}")
        self.generate_order_modify_rejected(
            strategy_id=order.strategy_id,
            instrument_id=order.instrument_id,
            client_order_id=order.client_order_id,
            venue_order_id=order.venue_order_id,
            reason=reason,
            ts_event=self._ts(),
        )

    async def _cancel_order(self, command: CancelOrder) -> None:
        order = self._cache.order(command.client_order_id)
        if order is None:
            self._log.error(f"Cannot cancel {command.client_order_id!r}: not in the cache")
            return
        await self._cancel(order, command.venue_order_id)

    async def _cancel(self, order: Order, venue_order_id: VenueOrderId | None = None) -> None:
        venue_order_id = venue_order_id or order.venue_order_id
        if venue_order_id is None:
            self._cancel_rejected(order, "the order has no Schwab order ID yet")
            return
        try:
            await self._backend.cancel_order(venue_order_id.value)
        except SchwabError as error:
            self._cancel_rejected(order, _reason(error), venue_order_id)
            return
        self._wake.set()

    def _cancel_rejected(
        self, order: Order, reason: str, venue_order_id: VenueOrderId | None = None
    ) -> None:
        self._log.error(f"Cancel of {order.client_order_id!r} rejected: {reason}")
        self.generate_order_cancel_rejected(
            strategy_id=order.strategy_id,
            instrument_id=order.instrument_id,
            client_order_id=order.client_order_id,
            venue_order_id=venue_order_id or order.venue_order_id,
            reason=reason,
            ts_event=self._ts(),
        )

    async def _cancel_all_orders(self, command: CancelAllOrders) -> None:
        orders = self._cache.orders_open(
            venue=SCHWAB_VENUE,
            instrument_id=command.instrument_id,
            strategy_id=command.strategy_id,
            side=command.order_side,
            account_id=self.account_id,
        )
        await asyncio.gather(*(self._cancel(order) for order in orders))

    async def _batch_cancel_orders(self, command: BatchCancelOrders) -> None:
        await asyncio.gather(*(self._cancel_order(cancel) for cancel in command.cancels))

    # -- finding what happened ---------------------------------------------------

    def _on_quote(self, tick: QuoteTick) -> None:
        self._quotes.put_nowait(tick)

    async def _feed_paper(self) -> None:
        """Give the paper venue each quote the node publishes, in order."""
        backend = self._backend
        assert isinstance(backend, PaperBackend)  # noqa: S101 (set up in _connect)
        while True:
            tick = await self._quotes.get()
            if await backend.venue.ingest(tick):
                self._wake.set()

    async def _monitor_orders(self) -> None:
        next_account = time.monotonic() + self._config.account_poll_interval_secs
        while not self._closing:
            try:
                await asyncio.wait_for(self._wake.wait(), timeout=self._config.poll_interval_secs)
            except asyncio.TimeoutError:
                pass
            self._wake.clear()
            if self._closing:
                return
            try:
                changed = await self._poll_orders()
                if changed or time.monotonic() >= next_account:
                    await self._update_account_state()
                    next_account = time.monotonic() + self._config.account_poll_interval_secs
            except asyncio.CancelledError:
                raise
            except RateLimited as error:
                await asyncio.sleep(error.retry_after or 5.0)
            except SchwabError as error:
                self._log.error(f"Reading Schwab's orders failed: {error}")
            except Exception as error:
                self._log.exception("Order monitoring failed", error)

    async def _poll_orders(self) -> bool:
        now = datetime.now(timezone.utc)
        raw = await self._backend.orders(
            now - timedelta(hours=self._config.order_lookback_hours),
            now + timedelta(minutes=1),
            recent=True,
        )
        seen: set[str] = set()
        changed = False
        for item in raw:
            view = parse_order(item)
            if view is not None:
                seen.add(view.order_id)
                changed |= self._apply(view)
        # A working order older than the window, or one the list left out.
        strays = [
            order
            for order in self._cache.orders_open(venue=SCHWAB_VENUE, account_id=self.account_id)
            if order.venue_order_id is not None and order.venue_order_id.value not in seen
        ]
        for order in strays[:_STRAY_READS]:
            assert order.venue_order_id is not None  # noqa: S101 (filtered above)
            try:
                item = await self._backend.order(order.venue_order_id.value)
            except SchwabError as error:
                self._log.warning(f"Could not read order {order.venue_order_id}: {error}")
                continue
            view = parse_order(item)
            if view is not None:
                changed |= self._apply(view)
        self._forget_closed()
        return changed

    def _forget_closed(self) -> None:
        for client_order_id in [c for c, p in self._progress.items() if p.closed]:
            order = self._cache.order(client_order_id)
            if order is None or order.is_closed:
                del self._progress[client_order_id]

    def _order_for(self, tag: str | None) -> Order | None:
        """The Nautilus order a Schwab tag belongs to, if this client sent it."""
        if not tag:
            return None
        order = self._cache.order(ClientOrderId(tag))
        if order is None:
            order = self._cache.order(ClientOrderId(split_tag(tag)[0]))
        return order

    def _progress_for(self, order: Order) -> _Progress:
        progress = self._progress.get(order.client_order_id)
        if progress is None:
            venue_order_id = order.venue_order_id
            progress = self._progress[order.client_order_id] = _Progress(
                venue_order_id=venue_order_id.value if venue_order_id else None,
                accepted=order.status not in (OrderStatus.INITIALIZED, OrderStatus.SUBMITTED),
                filled=order.filled_qty.as_decimal(),
                fills={t.value for t in order.trade_ids},
                closed=order.is_closed,
            )
        return progress

    def _apply(self, view: SchwabOrder) -> bool:
        """Turn what Schwab shows for one order into the events Nautilus lacks."""
        order = self._order_for(view.tag)
        if order is None:
            return False
        progress = self._progress_for(order)
        if order.is_closed:
            progress.closed = True
        if progress.closed:
            if view.is_open and view.order_id not in self._orphans:
                self._orphans.add(view.order_id)
                self._log.error(
                    f"Schwab order {view.order_id} is still working but {order.client_order_id!r} "
                    "is closed in Nautilus. Cancel it at Schwab or with `rustyschwab-admin`."
                )
            return False
        ts = self._ts()
        changed = False
        if progress.venue_order_id not in (None, view.order_id):
            ledger = self._ledger.get(order.client_order_id)
            if split_tag(view.tag or "")[1] <= (ledger.revision if ledger else 0):
                return False  # the superseded original
            self._emit_replaced(order, view, ts)
            progress.venue_order_id = view.order_id
            changed = True
        if not progress.accepted:
            if view.status == OrderStatus.REJECTED:
                self._reject(order, "Schwab rejected the order")
                progress.closed = True
                return True
            self._emit_accepted(order, view.order_id, view.entered_ns or ts)
            changed = True
        progress.venue_order_id = view.order_id
        for fill in view.fills:
            if fill.trade_id in progress.fills:
                continue
            if progress.filled + fill.quantity > order.quantity.as_decimal():
                self._log.error(
                    f"Ignoring fill {fill.trade_id} that would overfill {order.client_order_id!r}"
                )
                continue
            self._emit_fill(order, view, fill, ts)
            progress.fills.add(fill.trade_id)
            progress.filled += fill.quantity
            changed = True
        changed |= self._emit_terminal(order, view, progress, ts)
        return changed

    def _emit_terminal(
        self, order: Order, view: SchwabOrder, progress: _Progress, ts: int
    ) -> bool:
        venue_order_id = VenueOrderId(view.order_id)
        when = view.closed_ns or ts
        if view.status == OrderStatus.FILLED:
            if progress.filled < view.filled_quantity:
                # Schwab gave no execution detail for part of the fill; the
                # engine infers it from a report rather than this client guessing a price.
                self._send_order_status_report(
                    order_status_report(
                        view,
                        account_id=self.account_id,
                        client_order_id=order.client_order_id,
                        ts_init=ts,
                    )
                )
            progress.closed = True
            return progress.filled < view.filled_quantity
        if view.status in (OrderStatus.CANCELED, OrderStatus.REJECTED):
            if view.status == OrderStatus.REJECTED:
                self._log.warning(f"Schwab rejected the working order {order.client_order_id!r}")
            self.generate_order_canceled(
                strategy_id=order.strategy_id,
                instrument_id=order.instrument_id,
                client_order_id=order.client_order_id,
                venue_order_id=venue_order_id,
                ts_event=when,
            )
            progress.closed = True
            return True
        if view.status == OrderStatus.EXPIRED:
            self.generate_order_expired(
                strategy_id=order.strategy_id,
                instrument_id=order.instrument_id,
                client_order_id=order.client_order_id,
                venue_order_id=venue_order_id,
                ts_event=when,
            )
            progress.closed = True
            return True
        return False

    def _emit_accepted(self, order: Order, venue_order_id: str, ts_event: int) -> None:
        progress = self._progress_for(order)
        if progress.accepted:
            return
        progress.accepted = True
        progress.venue_order_id = venue_order_id
        self.generate_order_accepted(
            strategy_id=order.strategy_id,
            instrument_id=order.instrument_id,
            client_order_id=order.client_order_id,
            venue_order_id=VenueOrderId(venue_order_id),
            ts_event=ts_event,
        )

    def _emit_replaced(self, order: Order, view: SchwabOrder, ts: int) -> None:
        """A replacement Schwab accepted that this client was not told about."""
        instrument = self._cache.instrument(order.instrument_id)
        precision = instrument.price_precision if instrument else None
        self.generate_order_updated(
            strategy_id=order.strategy_id,
            instrument_id=order.instrument_id,
            client_order_id=order.client_order_id,
            venue_order_id=VenueOrderId(view.order_id),
            quantity=make_quantity(view.quantity),
            price=make_price(view.price, precision) if view.price is not None else None,
            trigger_price=(
                make_price(view.trigger_price, precision) if view.trigger_price is not None else None
            ),
            ts_event=ts,
            venue_order_id_modified=True,
        )
        revision = split_tag(view.tag or "")[1]
        self._ledger.record(
            order.client_order_id,
            LedgerEntry(tag=view.tag or "", revision=revision, venue_order_id=view.order_id),
        )

    def _emit_fill(self, order: Order, view: SchwabOrder, fill: SchwabFill, ts: int) -> None:
        instrument = self._cache.instrument(order.instrument_id)
        precision = instrument.price_precision if instrument else None
        self.generate_order_filled(
            strategy_id=order.strategy_id,
            instrument_id=order.instrument_id,
            client_order_id=order.client_order_id,
            venue_order_id=VenueOrderId(view.order_id),
            venue_position_id=None,
            trade_id=TradeId(fill.trade_id),
            order_side=view.side,
            order_type=order.order_type,
            last_qty=make_quantity(fill.quantity),
            last_px=make_price(fill.price, precision),
            quote_currency=SCHWAB_CURRENCY,
            commission=Money(fill.fee, SCHWAB_CURRENCY),
            liquidity_side=LiquiditySide.NO_LIQUIDITY_SIDE,
            ts_event=fill.time_ns or ts,
        )

    # -- reports -----------------------------------------------------------------

    def _precision(self, instrument_id: InstrumentId) -> int | None:
        instrument = self._cache.instrument(instrument_id)
        return instrument.price_precision if instrument is not None else None

    def _known_client_order_id(self, tag: str | None) -> ClientOrderId | None:
        order = self._order_for(tag)
        return order.client_order_id if order is not None else None

    async def _read_orders(
        self, start: datetime | None, end: datetime | None
    ) -> list[SchwabOrder]:
        now = datetime.now(timezone.utc)
        floor = now - timedelta(days=ORDER_HISTORY_DAYS)
        raw = await self._backend.orders(
            max(_aware(start), floor) if start else floor,
            _aware(end) if end else now + timedelta(minutes=1),
        )
        views = [parse_order(item) for item in raw]
        return [v for v in views if v is not None]

    def _report_for(self, view: SchwabOrder, ts_init: int) -> OrderStatusReport:
        return order_status_report(
            view,
            account_id=self.account_id,
            client_order_id=self._known_client_order_id(view.tag),
            ts_init=ts_init,
            price_precision=self._precision(view.instrument_id),
        )

    async def generate_order_status_report(
        self, command: GenerateOrderStatusReport
    ) -> OrderStatusReport | None:
        if command.client_order_id is None and command.venue_order_id is None:
            self._log.debug("Both `client_order_id` and `venue_order_id` cannot be None")
            return None
        try:
            view: SchwabOrder | None = None
            if command.venue_order_id is not None:
                view = parse_order(await self._backend.order(command.venue_order_id.value))
            else:
                assert command.client_order_id is not None  # noqa: S101 (checked above)
                matches = [
                    v
                    for v in await self._read_orders(None, None)
                    if v.tag and split_tag(v.tag)[0] == command.client_order_id.value
                ]
                view = max(matches, key=lambda v: split_tag(v.tag or "")[1], default=None)
        except SchwabError as error:
            self._log_report_error(error, "OrderStatusReport")
            return None
        if view is None:
            return None
        return self._report_for(view, self._ts())

    async def generate_order_status_reports(
        self, command: GenerateOrderStatusReports
    ) -> list[OrderStatusReport]:
        try:
            views = await self._read_orders(command.start, command.end)
        except SchwabError as error:
            self._log_report_error(error, "OrderStatusReports")
            return []
        ts = self._ts()
        reports = [
            self._report_for(v, ts)
            for v in views
            if (not command.open_only or v.is_open)
            and (command.instrument_id is None or v.instrument_id == command.instrument_id)
        ]
        self._log_report_receipt(
            len(reports), "OrderStatusReport", command.log_receipt_level
        )
        return reports

    async def generate_fill_reports(self, command: GenerateFillReports) -> list[FillReport]:
        try:
            views = await self._read_orders(command.start, command.end)
        except SchwabError as error:
            self._log_report_error(error, "FillReports")
            return []
        ts = self._ts()
        start_ns = int(command.start.timestamp() * 1e9) if command.start else 0
        end_ns = int(command.end.timestamp() * 1e9) if command.end else None
        reports: list[FillReport] = []
        for view in views:
            if command.instrument_id is not None and view.instrument_id != command.instrument_id:
                continue
            if command.venue_order_id is not None and view.order_id != command.venue_order_id.value:
                continue
            for report in fill_reports(
                view,
                account_id=self.account_id,
                client_order_id=self._known_client_order_id(view.tag),
                ts_init=ts,
                price_precision=self._precision(view.instrument_id),
            ):
                if report.ts_event >= start_ns and (end_ns is None or report.ts_event <= end_ns):
                    reports.append(report)
        self._log_report_receipt(len(reports), "FillReport", LogLevel.INFO)
        return reports

    async def generate_position_status_reports(
        self, command: GeneratePositionStatusReports
    ) -> list[PositionStatusReport]:
        try:
            account = await self._backend.account()
        except SchwabError as error:
            self._log_report_error(error, "PositionStatusReports")
            return []
        ts = self._ts()
        reports = position_reports(account, account_id=self.account_id, ts_init=ts)
        # A position Nautilus holds that the account does not is reported flat,
        # which is how the engine learns it was closed elsewhere.
        reported = {r.instrument_id for r in reports}
        for position in self._cache.positions_open(venue=SCHWAB_VENUE, account_id=self.account_id):
            if position.instrument_id not in reported:
                reports.append(
                    PositionStatusReport.create_flat(
                        self.account_id, position.instrument_id, position.size_precision, ts
                    )
                )
        reports = [
            r for r in reports if command.instrument_id in (None, r.instrument_id)
        ]
        self._log_report_receipt(len(reports), "PositionStatusReport", command.log_receipt_level)
        return reports


__all__ = ["SchwabExecutionClient"]
