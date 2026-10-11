"""Nautilus orders to Schwab order JSON, and Schwab orders back.

Both directions live here so the two stay in step. The Schwab side is parsed
into a :class:`SchwabOrder`, a plain value that the report and event code reads;
the live REST responses and the paper venue's orders both arrive in this shape
(the paper mapping is in :mod:`rustyschwab.nautilus.paper`).

Orders the adapter does not model, such as multi-leg strategies or trailing
stops, parse to ``None`` and are left out of reports.
"""

from __future__ import annotations

import re
from collections.abc import Mapping
from dataclasses import dataclass, replace
from datetime import datetime, timezone
from decimal import Decimal
from typing import Any

from nautilus_trader.model.enums import (
    OrderSide,
    OrderStatus,
    OrderType,
    TimeInForce,
    TriggerType,
)
from nautilus_trader.model.identifiers import ClientOrderId, InstrumentId
from nautilus_trader.model.instruments import Equity, Instrument, OptionContract
from nautilus_trader.model.orders import Order

from .symbols import instrument_id_for

# ---- vocabulary ---------------------------------------------------------------

_ORDER_TYPES: dict[OrderType, str] = {
    OrderType.MARKET: "MARKET",
    OrderType.LIMIT: "LIMIT",
    OrderType.STOP_MARKET: "STOP",
    OrderType.STOP_LIMIT: "STOP_LIMIT",
}
_FROM_SCHWAB_TYPE: dict[str, OrderType] = {v: k for k, v in _ORDER_TYPES.items()}

#: Time in force on the way out. Nautilus has no durations Schwab lacks, but
#: only these two are modelled and tested.
_DURATIONS: dict[TimeInForce, str] = {
    TimeInForce.DAY: "DAY",
    TimeInForce.GTC: "GOOD_TILL_CANCEL",
}
#: Time in force on the way in also covers the durations a reported order may
#: carry although the adapter never submits them.
_FROM_SCHWAB_DURATION: dict[str, TimeInForce] = {
    "DAY": TimeInForce.DAY,
    "GOOD_TILL_CANCEL": TimeInForce.GTC,
    "FILL_OR_KILL": TimeInForce.FOK,
    "IMMEDIATE_OR_CANCEL": TimeInForce.IOC,
}

#: ``stopType`` for the trigger types Schwab can honour.
_STOP_TYPES: dict[TriggerType, str | None] = {
    TriggerType.DEFAULT: None,
    TriggerType.NO_TRIGGER: None,
    TriggerType.LAST_PRICE: "LAST",
    TriggerType.MARK_PRICE: "MARK",
}

_BUY_INSTRUCTIONS = {"BUY", "BUY_TO_COVER", "BUY_TO_OPEN", "BUY_TO_CLOSE"}
_SELL_INSTRUCTIONS = {
    "SELL",
    "SELL_SHORT",
    "SELL_SHORT_EXEMPT",
    "SELL_TO_OPEN",
    "SELL_TO_CLOSE",
}

#: Schwab order status to Nautilus. Working-like statuses collapse to ACCEPTED
#: (and PARTIALLY_FILLED once something has filled); ``REPLACED`` and
#: ``UNKNOWN`` are deliberately absent.
_STATUSES: dict[str, OrderStatus] = {
    "AWAITING_PARENT_ORDER": OrderStatus.ACCEPTED,
    "AWAITING_CONDITION": OrderStatus.ACCEPTED,
    "AWAITING_STOP_CONDITION": OrderStatus.ACCEPTED,
    "AWAITING_MANUAL_REVIEW": OrderStatus.ACCEPTED,
    "AWAITING_UR_OUT": OrderStatus.ACCEPTED,
    "AWAITING_RELEASE_TIME": OrderStatus.ACCEPTED,
    "PENDING_ACTIVATION": OrderStatus.ACCEPTED,
    "PENDING_ACKNOWLEDGEMENT": OrderStatus.ACCEPTED,
    "PENDING_RECALL": OrderStatus.ACCEPTED,
    "ACCEPTED": OrderStatus.ACCEPTED,
    "QUEUED": OrderStatus.ACCEPTED,
    "NEW": OrderStatus.ACCEPTED,
    "WORKING": OrderStatus.ACCEPTED,
    "PENDING_CANCEL": OrderStatus.PENDING_CANCEL,
    "PENDING_REPLACE": OrderStatus.PENDING_UPDATE,
    "FILLED": OrderStatus.FILLED,
    "CANCELED": OrderStatus.CANCELED,
    "REJECTED": OrderStatus.REJECTED,
    "EXPIRED": OrderStatus.EXPIRED,
}
_OPEN = {OrderStatus.ACCEPTED, OrderStatus.PARTIALLY_FILLED, OrderStatus.PENDING_CANCEL,
         OrderStatus.PENDING_UPDATE}

# ---- tags ---------------------------------------------------------------------

#: A replacement is a new Schwab order and the journal spends each tag once, so
#: it carries the original's tag with a revision suffix.
_REVISION = re.compile(r"^(?P<base>.+)-R(?P<revision>\d+)$")


def tag_for(client_order_id: ClientOrderId, revision: int = 0) -> str:
    """The Schwab tag that carries a Nautilus client order ID.

    The tag is the order's identity at Schwab, which echoes it on every read, so
    nothing has to be remembered to map an order back after a restart.
    """
    return client_order_id.value if revision == 0 else f"{client_order_id.value}-R{revision}"


def split_tag(tag: str) -> tuple[str, int]:
    """``(client order ID, revision)`` for a tag from :func:`tag_for`."""
    match = _REVISION.match(tag)
    if match is None:
        return tag, 0
    return match.group("base"), int(match.group("revision"))


# ---- Nautilus to Schwab -------------------------------------------------------


class UnsupportedOrder(ValueError):
    """The order cannot be expressed to Schwab. The message is the denial reason."""


@dataclass(frozen=True)
class OrderSpec:
    """What an outgoing order says, independent of how Schwab spells it."""

    symbol: str
    asset_type: str
    side: OrderSide
    quantity: int
    order_type: OrderType
    time_in_force: TimeInForce
    price: Decimal | None = None
    trigger_price: Decimal | None = None
    trigger_type: TriggerType = TriggerType.DEFAULT

    def modified(
        self,
        quantity: int | None = None,
        price: Decimal | None = None,
        trigger_price: Decimal | None = None,
    ) -> OrderSpec:
        return replace(
            self,
            quantity=self.quantity if quantity is None else quantity,
            price=self.price if price is None else price,
            trigger_price=self.trigger_price if trigger_price is None else trigger_price,
        )


def asset_type_of(instrument: Instrument) -> str:
    if isinstance(instrument, Equity):
        return "EQUITY"
    if isinstance(instrument, OptionContract):
        return "OPTION"
    raise UnsupportedOrder(f"{instrument.id} is not an equity or an equity option")


def spec_from_order(order: Order, instrument: Instrument) -> OrderSpec:
    """Check an order against what the adapter can send, and capture it.

    Anything Schwab could not honour is refused here rather than approximated.
    """
    asset_type = asset_type_of(instrument)
    if order.order_type not in _ORDER_TYPES:
        raise UnsupportedOrder(
            f"{order.order_type.name} orders are not supported; use MARKET, LIMIT, "
            "STOP_MARKET or STOP_LIMIT"
        )
    if order.time_in_force not in _DURATIONS:
        raise UnsupportedOrder(
            f"time in force {order.time_in_force.name} is not supported; use DAY or GTC"
        )
    if order.is_reduce_only:
        raise UnsupportedOrder("reduce-only is not supported by Schwab")
    if order.is_post_only:
        raise UnsupportedOrder("post-only is not supported by Schwab")
    if order.is_quote_quantity:
        raise UnsupportedOrder("quote-denominated quantity is not supported by Schwab")
    if getattr(order, "display_qty", None) is not None:
        raise UnsupportedOrder("display quantity (iceberg) is not supported by Schwab")
    if order.contingency_type.name != "NO_CONTINGENCY":
        raise UnsupportedOrder("contingent orders are not supported; submit legs individually")
    quantity = order.quantity.as_decimal()
    if quantity != quantity.to_integral_value():
        raise UnsupportedOrder(f"fractional quantity {quantity} is not supported")
    trigger_type = getattr(order, "trigger_type", TriggerType.DEFAULT)
    if order.order_type in (OrderType.STOP_MARKET, OrderType.STOP_LIMIT) and (
        trigger_type not in _STOP_TYPES
    ):
        raise UnsupportedOrder(
            f"trigger type {trigger_type.name} is not supported; use DEFAULT, LAST_PRICE or MARK_PRICE"
        )
    price = getattr(order, "price", None)
    trigger = getattr(order, "trigger_price", None)
    return OrderSpec(
        symbol=instrument.raw_symbol.value,
        asset_type=asset_type,
        side=order.side,
        quantity=int(quantity),
        order_type=order.order_type,
        time_in_force=order.time_in_force,
        price=price.as_decimal() if price is not None else None,
        trigger_price=trigger.as_decimal() if trigger is not None else None,
        trigger_type=trigger_type,
    )


def instruction_for(spec: OrderSpec, net_position: Decimal, allow_short: bool) -> str:
    """Schwab's ``instruction`` for an order given the account's net position.

    Nautilus nets positions and Schwab does not: it wants a short sale or an
    option opening named as such. The net position decides. An order that
    would cross zero is refused, since it is really a close and an open; and
    opening a short is refused unless ``allow_short`` says the strategy means it.
    """
    quantity = Decimal(spec.quantity)
    option = spec.asset_type == "OPTION"
    if spec.side == OrderSide.BUY:
        if net_position >= 0:
            return "BUY_TO_OPEN" if option else "BUY"
        if quantity > -net_position:
            raise UnsupportedOrder(_crossing(net_position))
        return "BUY_TO_CLOSE" if option else "BUY_TO_COVER"
    if net_position >= quantity:
        return "SELL_TO_CLOSE" if option else "SELL"
    if net_position > 0:
        raise UnsupportedOrder(_crossing(net_position))
    if not allow_short:
        raise UnsupportedOrder(
            "selling here would open a short position; set allow_short_positions=True "
            "on the execution client if the strategy intends it"
        )
    return "SELL_TO_OPEN" if option else "SELL_SHORT"


def _crossing(net_position: Decimal) -> str:
    return (
        f"the order would cross the net position of {net_position}; "
        "close it and open the new position in separate orders"
    )


def _text(value: Decimal) -> str:
    return format(value.normalize(), "f")


def to_schwab_order(
    spec: OrderSpec,
    *,
    tag: str,
    instruction: str,
    session: str = "NORMAL",
) -> dict[str, Any]:
    """Schwab order JSON for ``place_order`` and ``replace_order``.

    Prices go out as exact strings and the quantity as a number, as the SDK
    expects.
    """
    order: dict[str, Any] = {
        "orderStrategyType": "SINGLE",
        "orderType": _ORDER_TYPES[spec.order_type],
        "session": session,
        "duration": _DURATIONS[spec.time_in_force],
        "tag": tag,
        "orderLegCollection": [
            {
                "instruction": instruction,
                "quantity": spec.quantity,
                "instrument": {"symbol": spec.symbol, "assetType": spec.asset_type},
            }
        ],
    }
    if spec.order_type in (OrderType.LIMIT, OrderType.STOP_LIMIT):
        if spec.price is None:
            raise UnsupportedOrder(f"a {spec.order_type.name} order needs a price")
        order["price"] = _text(spec.price)
    if spec.order_type in (OrderType.STOP_MARKET, OrderType.STOP_LIMIT):
        if spec.trigger_price is None:
            raise UnsupportedOrder(f"a {spec.order_type.name} order needs a trigger price")
        order["stopPrice"] = _text(spec.trigger_price)
        stop_type = _STOP_TYPES.get(spec.trigger_type)
        if stop_type is not None:
            order["stopType"] = stop_type
    return order


# ---- Schwab to Nautilus -------------------------------------------------------


@dataclass(frozen=True)
class SchwabFill:
    """One execution of a Schwab order."""

    trade_id: str
    quantity: Decimal
    price: Decimal
    time_ns: int
    #: Commission and fees. Schwab's order JSON carries none, so live fills
    #: report zero; the paper venue reports its per-share fee.
    fee: Decimal = Decimal(0)


@dataclass(frozen=True)
class SchwabOrder:
    """A single-leg Schwab order, as the report and event code reads it."""

    order_id: str
    tag: str | None
    instrument_id: InstrumentId
    side: OrderSide
    order_type: OrderType
    time_in_force: TimeInForce
    status: OrderStatus
    quantity: Decimal
    filled_quantity: Decimal
    price: Decimal | None
    trigger_price: Decimal | None
    entered_ns: int
    closed_ns: int | None
    fills: tuple[SchwabFill, ...]
    schwab_status: str

    @property
    def is_open(self) -> bool:
        return self.status in _OPEN

    @property
    def average_price(self) -> Decimal | None:
        """The volume-weighted fill price, or ``None`` with no fills."""
        total = sum((f.quantity for f in self.fills), Decimal(0))
        if total == 0:
            return None
        return sum((f.quantity * f.price for f in self.fills), Decimal(0)) / total


def _dec(value: Any) -> Decimal | None:
    if value is None or isinstance(value, bool):
        return None
    try:
        return Decimal(str(value))
    except ArithmeticError:
        return None


def time_to_ns(value: Any) -> int | None:
    """Epoch nanoseconds for a Schwab timestamp such as ``2025-01-15T14:35:00+0000``."""
    if not isinstance(value, str) or not value:
        return None
    try:
        moment = datetime.fromisoformat(value)
    except ValueError:
        return None
    if moment.tzinfo is None:
        moment = moment.replace(tzinfo=timezone.utc)
    return int(moment.timestamp()) * 1_000_000_000 + moment.microsecond * 1_000


def side_of(instruction: str) -> OrderSide | None:
    if instruction in _BUY_INSTRUCTIONS:
        return OrderSide.BUY
    if instruction in _SELL_INSTRUCTIONS:
        return OrderSide.SELL
    return None


def parse_fills(order_id: str, activities: Any) -> tuple[SchwabFill, ...]:
    """Executions from an order's ``orderActivityCollection``.

    Schwab gives an execution no ID of its own in the documented fields. Its
    ``activityId`` is used when present, so the same execution keeps the same
    trade ID on every read; otherwise the execution's position in the list is.
    """
    fills: list[SchwabFill] = []
    for index, activity in enumerate(activities or []):
        if activity.get("activityType") != "EXECUTION":
            continue
        if activity.get("executionType", "FILL") != "FILL":
            continue
        base = activity.get("activityId", index)
        legs = activity.get("executionLegs") or []
        for leg_index, leg in enumerate(legs):
            quantity, price = _dec(leg.get("quantity")), _dec(leg.get("price"))
            if quantity is None or price is None or quantity <= 0:
                continue
            suffix = f".{leg_index}" if len(legs) > 1 else ""
            fills.append(
                SchwabFill(
                    trade_id=f"{order_id}-{base}{suffix}"[:36],
                    quantity=quantity,
                    price=price,
                    time_ns=time_to_ns(leg.get("time")) or 0,
                    fee=_dec(leg.get("fee")) or Decimal(0),
                )
            )
    return tuple(fills)


def parse_order(order: Mapping[str, Any]) -> SchwabOrder | None:
    """A :class:`SchwabOrder` from Schwab's order JSON, or ``None`` if out of scope.

    Out of scope are multi-leg and non-single orders, instruments other than
    equities and options, order types the adapter does not map, ``REPLACED``
    orders (superseded by their replacement, which is reported instead), and
    statuses this adapter does not know.
    """
    legs = order.get("orderLegCollection") or []
    order_id = order.get("orderId")
    if (
        len(legs) != 1
        or order_id is None
        or order.get("orderStrategyType", "SINGLE") != "SINGLE"
        or order.get("childOrderStrategies")
    ):
        return None
    leg = legs[0]
    instrument = leg.get("instrument") or {}
    side = side_of(str(leg.get("instruction")))
    symbol = instrument.get("symbol")
    order_type = _FROM_SCHWAB_TYPE.get(str(order.get("orderType")))
    status = _STATUSES.get(str(order.get("status")))
    if (
        side is None
        or not isinstance(symbol, str)
        or instrument.get("assetType") not in ("EQUITY", "OPTION")
        or order_type is None
        or status is None
    ):
        return None
    filled = _dec(order.get("filledQuantity")) or Decimal(0)
    remaining = _dec(order.get("remainingQuantity"))
    quantity = _dec(order.get("quantity")) or _dec(leg.get("quantity")) or Decimal(0)
    if remaining is not None and filled + remaining > quantity:
        quantity = filled + remaining
    # Whole units only: fractional (stock slice) orders are not modelled.
    if quantity <= 0 or quantity != quantity.to_integral_value() or filled != filled.to_integral_value():
        return None
    if filled > 0 and status == OrderStatus.ACCEPTED:
        status = OrderStatus.PARTIALLY_FILLED
    order_id_text = str(order_id)
    return SchwabOrder(
        order_id=order_id_text,
        tag=order.get("tag") if isinstance(order.get("tag"), str) else None,
        instrument_id=instrument_id_for(symbol),
        side=side,
        order_type=order_type,
        time_in_force=_FROM_SCHWAB_DURATION.get(str(order.get("duration")), TimeInForce.DAY),
        status=status,
        quantity=quantity,
        filled_quantity=filled,
        price=_dec(order.get("price")) if order_type in (OrderType.LIMIT, OrderType.STOP_LIMIT) else None,
        trigger_price=_dec(order.get("stopPrice")),
        entered_ns=time_to_ns(order.get("enteredTime")) or 0,
        closed_ns=time_to_ns(order.get("closeTime")),
        fills=parse_fills(order_id_text, order.get("orderActivityCollection")),
        schwab_status=str(order.get("status")),
    )
