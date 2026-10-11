"""The paper venue behind the execution client.

:class:`rustyschwab.PaperBroker` reads back its own snake_case records
(``filled_quantity``, ``accepted_at_ms``, fills with a ``fee``) rather than
Schwab's JSON. Everything that converts between the two is in this module, so
the execution client sees the same shapes from Schwab and from the venue and
the paper path cannot drift from the live one. The conversion is the place to
read for what a paper order looks like to Nautilus.

Quotes reach the venue from the node's own data: the execution client
subscribes to ``data.quotes.SCHWAB.*`` on the message bus and hands each tick to
:meth:`PaperVenue.ingest`. Whatever publishes SCHWAB quotes drives the venue.
"""

from __future__ import annotations

import asyncio
import time
from collections.abc import Mapping, Sequence
from datetime import datetime, timezone
from decimal import Decimal
from typing import TYPE_CHECKING, Any, ClassVar

from nautilus_trader.model.data import QuoteTick

from rustyschwab import PaperBroker
from rustyschwab.errors import SchwabError

from .config import SchwabPaperConfig
from .symbols import is_option_symbol

if TYPE_CHECKING:
    from rustyschwab._native import JsonObject, Submitted

#: A Schwab-shaped timestamp keeps its milliseconds; ``parse_order`` reads it.
_TIME_FORMAT = "%Y-%m-%dT%H:%M:%S.%f%z"


def iso_time(millis: int) -> str:
    return datetime.fromtimestamp(millis / 1000, tz=timezone.utc).strftime(_TIME_FORMAT)


# ---- paper records to Schwab's shapes ----------------------------------------


def to_schwab_order(order: Mapping[str, Any], fills: Sequence[Mapping[str, Any]]) -> JsonObject:
    """A paper order and its fills as a Schwab order.

    The venue keeps the Schwab request it accepted under ``request``; this lays
    the venue's lifecycle fields over it with Schwab's names, and expresses the
    fills as ``orderActivityCollection`` executions. Each fill's ``fee`` rides
    on its execution leg as an extension, because Schwab's order JSON has no
    commission of its own.
    """
    filled = Decimal(order["filled_quantity"])
    remaining = Decimal(order["remaining_quantity"])
    status = str(order["status"])
    activities = [
        {
            "activityType": "EXECUTION",
            "executionType": "FILL",
            "activityId": fill["event_sequence"],
            "quantity": fill["quantity"],
            "orderRemainingQuantity": "0",
            "executionLegs": [
                {
                    "legId": 1,
                    "quantity": fill["quantity"],
                    "price": fill["price"],
                    "time": iso_time(int(fill["at_ms"])),
                    "fee": fill["fee"],
                }
            ],
        }
        for fill in fills
    ]
    schwab: dict[str, Any] = {
        **order["request"],
        "orderId": order["order_id"],
        "status": status,
        "quantity": str(filled + remaining),
        "filledQuantity": str(filled),
        "remainingQuantity": str(remaining),
        "enteredTime": iso_time(int(order["accepted_at_ms"])),
        "orderActivityCollection": activities,
    }
    if status not in ("WORKING", "PENDING_CANCEL"):
        closed_ms = max(
            [int(f["at_ms"]) for f in fills]
            + [int(order["cancel_at_ms"]) if order.get("cancel_at_ms") else 0]
            + [int(order["accepted_at_ms"])]
        )
        schwab["closeTime"] = iso_time(closed_ms)
    return schwab


def to_schwab_account(snapshot: Mapping[str, Any]) -> JsonObject:
    """A paper snapshot as a Schwab cash account with positions and balances."""
    ledger = snapshot["ledger"]
    positions: list[dict[str, Any]] = []
    held_cost = Decimal(0)
    for symbol, position in ledger["positions"].items():
        quantity = Decimal(position["quantity"])
        if quantity == 0:
            continue
        cost = Decimal(position["cost_basis"])
        held_cost += cost
        average = str(cost / quantity)
        positions.append(
            {
                "longQuantity": str(quantity),
                "shortQuantity": "0",
                "averagePrice": average,
                "averageLongPrice": average,
                "instrument": {"symbol": symbol, "assetType": "EQUITY"},
            }
        )
    cash = Decimal(ledger["cash"])
    equity = snapshot.get("equity")
    # Without a fresh quote on every holding the venue has no mark; positions
    # at cost is the honest stand-in, and the balance is refreshed on each fill.
    liquidation = Decimal(equity) if equity is not None else cash + held_cost
    return {
        "securitiesAccount": {
            "type": "CASH",
            "accountNumber": snapshot["account"],
            "positions": positions,
            "currentBalances": {
                "cashBalance": str(cash),
                "cashAvailableForTrading": snapshot["available_cash"],
                "liquidationValue": str(liquidation),
            },
        }
    }


# ---- the venue ----------------------------------------------------------------


class PaperVenue:
    """One open paper state file, shared by everything in the process that uses it."""

    _venues: ClassVar[dict[str, PaperVenue]] = {}

    def __init__(self, config: SchwabPaperConfig) -> None:
        self.config = config
        self._broker: PaperBroker | None = None
        self._lock = asyncio.Lock()
        self._session_id = ""
        self.rejected_quotes = 0

    @classmethod
    def shared(cls, config: SchwabPaperConfig) -> PaperVenue:
        existing = cls._venues.get(config.state_path)
        if existing is not None:
            if existing.config != config:
                raise ValueError(
                    f"the paper venue at {config.state_path} is already open with different settings"
                )
            return existing
        venue = cls._venues[config.state_path] = cls(config)
        return venue

    @property
    def broker(self) -> PaperBroker:
        if self._broker is None:
            raise RuntimeError("the paper venue is not open")
        return self._broker

    async def open(self) -> None:
        """Open the state file and start a trading session.

        Each connection is one session. A session left open by a run that
        never closed it is closed first, so its DAY orders expire as they would
        have at the close.
        """
        async with self._lock:
            if self._broker is not None:
                return
            broker = await PaperBroker.open(self.config.state_path, self.config.venue_config())
            previous, is_open = await broker.market_session()
            if is_open:
                await broker.append_live({"kind": "session", "session_id": previous, "open": False})
            self._session_id = f"nautilus-{time.time_ns()}"
            await broker.append_live(
                {"kind": "session", "session_id": self._session_id, "open": True}
            )
            self._broker = broker

    async def close(self) -> None:
        """Close the session, which expires working DAY orders, and refuse new orders."""
        async with self._lock:
            broker, self._broker = self._broker, None
            if broker is None:
                return
            broker.begin_drain()
            try:
                await broker.append_live(
                    {"kind": "session", "session_id": self._session_id, "open": False}
                )
            except SchwabError:
                pass

    async def ingest(self, tick: QuoteTick) -> list[dict[str, Any]]:
        """Give the venue a quote. Returns the fills it produced.

        The venue refuses a quote that is crossed, under one dollar, older than
        its freshness window or from the future; those are counted in
        ``rejected_quotes`` and dropped, because a feed must not be stopped by
        one bad tick.
        """
        if self._broker is None or is_option_symbol(tick.instrument_id.symbol.value):
            return []  # the venue trades equities only
        bid, ask = tick.bid_price.as_decimal(), tick.ask_price.as_decimal()
        now_ms = int(time.time() * 1000)
        source_ms = min(tick.ts_event // 1_000_000, now_ms)
        try:
            result = await self._broker.append_live(
                {
                    "kind": "quote",
                    "symbol": tick.instrument_id.symbol.value,
                    "source_at_ms": source_ms,
                    "bid": str(bid),
                    "ask": str(ask),
                    "bid_size": str(tick.bid_size.as_decimal().to_integral_value()),
                    "ask_size": str(tick.ask_size.as_decimal().to_integral_value()),
                }
            )
        except SchwabError:
            self.rejected_quotes += 1
            return []
        fills: list[dict[str, Any]] = result["fills"]
        return fills


class PaperBackend:
    """The :class:`~rustyschwab.nautilus.backend.Backend` for the paper venue."""

    def __init__(self, venue: PaperVenue) -> None:
        self.venue = venue
        self.account_hash: str | None = venue.config.account
        self._fills: dict[str, tuple[str, list[dict[str, Any]]]] = {}
        # Orders before this ID are all closed; later polls start after it.
        self._settled_through = 0

    async def connect(self) -> str:
        await self.venue.open()
        return "CASH"

    async def close(self) -> None:
        await self.venue.close()

    async def place_order(self, order: Mapping[str, Any]) -> Submitted:
        return await self.venue.broker.place_order(self.venue.config.account, order)

    async def replace_order(self, order_id: str, order: Mapping[str, Any]) -> Submitted:
        return await self.venue.broker.replace_order(self.venue.config.account, order_id, order)

    async def cancel_order(self, order_id: str) -> None:
        await self.venue.broker.cancel_order(self.venue.config.account, order_id)

    async def _with_fills(self, order: Mapping[str, Any]) -> JsonObject:
        order_id = str(order["order_id"])
        filled = str(order["filled_quantity"])
        cached = self._fills.get(order_id)
        if cached is not None and cached[0] == filled:
            fills = cached[1]
        elif Decimal(filled) > 0:
            fills = await self.venue.broker.fills(self.venue.config.account, order_id)
            self._fills[order_id] = (filled, fills)
        else:
            fills = []
        return to_schwab_order(order, fills)

    async def order(self, order_id: str) -> JsonObject:
        order = await self.venue.broker.order(self.venue.config.account, order_id)
        return await self._with_fills(order)

    async def orders(
        self, start: datetime, end: datetime, *, recent: bool = False
    ) -> list[JsonObject]:
        """The venue's orders, in ID order.

        The venue has no time filter, so the window is ignored. A ``recent``
        read starts after the last order known to be closed together with
        everything before it, which keeps the monitor's polls short in a long
        session; any other read pages from the first order.
        """
        del start, end
        collected: list[Mapping[str, Any]] = []
        after = self._settled_through if recent else 0
        while True:
            page = await self.venue.broker.orders(self.venue.config.account, after_id=after, limit=500)
            collected.extend(page)
            if len(page) < 500:
                break
            after = int(page[-1]["order_id"])
        settled = self._settled_through
        for order in collected:
            if order["status"] in ("WORKING", "PENDING_CANCEL"):
                break
            settled = int(order["order_id"])
        result = [await self._with_fills(order) for order in collected]
        self._settled_through = settled
        return result

    async def account(self) -> JsonObject:
        snapshot = await self.venue.broker.snapshot(self.venue.config.account)
        return to_schwab_account(snapshot)


__all__ = ["PaperBackend", "PaperVenue", "iso_time", "to_schwab_account", "to_schwab_order"]
