"""Nautilus execution reports and account state from Schwab's JSON.

These are the values Nautilus reconciles against: order status, fill and
position reports, and balances. They are built from :class:`SchwabOrder` and
from the account JSON, never from the venue's raw order dicts, so Schwab and
the paper venue produce identical reports for identical state.
"""

from __future__ import annotations

from collections.abc import Mapping
from decimal import Decimal
from typing import Any

from nautilus_trader.core.uuid import UUID4
from nautilus_trader.execution.reports import FillReport, OrderStatusReport, PositionStatusReport
from nautilus_trader.model.enums import LiquiditySide, PositionSide, TriggerType
from nautilus_trader.model.identifiers import (
    AccountId,
    ClientOrderId,
    TradeId,
    VenueOrderId,
)
from nautilus_trader.model.objects import AccountBalance, Money, Price, Quantity

from .constants import SCHWAB_CURRENCY
from .orders import SchwabOrder
from .parsing import decimals_of
from .symbols import instrument_id_for

def _precision(value: Decimal, loaded: int | None) -> int:
    if loaded is not None:
        return loaded
    return max(2, decimals_of(value))


def make_price(value: Decimal, loaded: int | None) -> Price:
    return Price(value, _precision(value, loaded))


def make_quantity(value: Decimal) -> Quantity:
    return Quantity(int(value), 0)


def order_status_report(
    order: SchwabOrder,
    *,
    account_id: AccountId,
    client_order_id: ClientOrderId | None,
    ts_init: int,
    price_precision: int | None = None,
) -> OrderStatusReport:
    last_fill = max((f.time_ns for f in order.fills), default=0)
    ts_last = max(order.closed_ns or 0, last_fill, order.entered_ns) or ts_init
    return OrderStatusReport(
        account_id=account_id,
        instrument_id=order.instrument_id,
        client_order_id=client_order_id,
        venue_order_id=VenueOrderId(order.order_id),
        order_side=order.side,
        order_type=order.order_type,
        time_in_force=order.time_in_force,
        order_status=order.status,
        quantity=make_quantity(order.quantity),
        filled_qty=make_quantity(order.filled_quantity),
        price=make_price(order.price, price_precision) if order.price is not None else None,
        trigger_price=(
            make_price(order.trigger_price, price_precision)
            if order.trigger_price is not None
            else None
        ),
        trigger_type=TriggerType.DEFAULT if order.trigger_price is not None else TriggerType.NO_TRIGGER,
        avg_px=order.average_price,
        report_id=UUID4(),
        ts_accepted=order.entered_ns or ts_init,
        ts_last=ts_last,
        ts_init=ts_init,
    )


def fill_reports(
    order: SchwabOrder,
    *,
    account_id: AccountId,
    client_order_id: ClientOrderId | None,
    ts_init: int,
    price_precision: int | None = None,
) -> list[FillReport]:
    return [
        FillReport(
            account_id=account_id,
            instrument_id=order.instrument_id,
            client_order_id=client_order_id,
            venue_order_id=VenueOrderId(order.order_id),
            trade_id=TradeId(fill.trade_id),
            order_side=order.side,
            last_qty=make_quantity(fill.quantity),
            last_px=make_price(fill.price, price_precision),
            commission=Money(fill.fee, SCHWAB_CURRENCY),
            liquidity_side=LiquiditySide.NO_LIQUIDITY_SIDE,
            report_id=UUID4(),
            ts_event=fill.time_ns or order.entered_ns or ts_init,
            ts_init=ts_init,
        )
        for fill in order.fills
    ]


def _quantity(value: Any) -> Decimal:
    try:
        return Decimal(str(value or 0))
    except ArithmeticError:
        return Decimal(0)


def position_reports(
    account: Mapping[str, Any],
    *,
    account_id: AccountId,
    ts_init: int,
) -> list[PositionStatusReport]:
    """One report per equity or option position in a Schwab account.

    Nautilus nets, so a position is long or short by the larger side. Other
    asset types (mutual funds, bonds, cash equivalents) are not modelled and
    are left out.
    """
    reports: list[PositionStatusReport] = []
    positions = account.get("securitiesAccount", {}).get("positions") or []
    for position in positions:
        instrument = position.get("instrument") or {}
        symbol = instrument.get("symbol")
        if instrument.get("assetType") not in ("EQUITY", "OPTION") or not isinstance(symbol, str):
            continue
        long_qty, short_qty = _quantity(position.get("longQuantity")), _quantity(position.get("shortQuantity"))
        net = long_qty - short_qty
        if net > 0:
            side, average = PositionSide.LONG, position.get("averageLongPrice") or position.get("averagePrice")
        elif net < 0:
            side, average = PositionSide.SHORT, position.get("averageShortPrice") or position.get("averagePrice")
        else:
            side, average = PositionSide.FLAT, None
        avg_px = _quantity(average) if average is not None else None
        reports.append(
            PositionStatusReport(
                account_id=account_id,
                instrument_id=instrument_id_for(symbol),
                position_side=side,
                quantity=make_quantity(abs(net)),
                report_id=UUID4(),
                ts_last=ts_init,
                ts_init=ts_init,
                avg_px_open=avg_px if avg_px else None,
            )
        )
    return reports


def _first(values: Mapping[str, Any], *names: str) -> Decimal | None:
    for name in names:
        value = values.get(name)
        if value is not None and not isinstance(value, bool):
            return Decimal(str(value))
    return None


def account_balances(
    account: Mapping[str, Any], account_type: str
) -> tuple[list[AccountBalance], dict[str, Any]]:
    """The balance and the extra account information for an account state.

    Total is the liquidation value (cash plus what the positions are worth).
    Free is what the account can spend: ``cashAvailableForTrading`` for a cash
    account, ``availableFunds`` for margin. Locked is the difference, and free
    is held between zero and the total because Nautilus requires it.
    """
    balances = account.get("securitiesAccount", {}).get("currentBalances") or {}
    total = _first(balances, "liquidationValue", "equity", "accountValue", "cashBalance")
    spend = (
        ("cashAvailableForTrading", "availableFunds")
        if account_type == "CASH"
        else ("availableFunds", "cashAvailableForTrading", "buyingPower")
    )
    free = _first(balances, *spend)
    if total is None or free is None:
        raise ValueError("Schwab's account balances lack a liquidation value or available funds")
    total = max(total, Decimal(0))
    free = min(max(free, Decimal(0)), total)
    balance = AccountBalance(
        total=Money(total, SCHWAB_CURRENCY),
        locked=Money(total - free, SCHWAB_CURRENCY),
        free=Money(free, SCHWAB_CURRENCY),
    )
    info = {
        name: str(balances[name])
        for name in ("cashBalance", "buyingPower", "maintenanceRequirement", "marginBalance")
        if balances.get(name) is not None
    }
    return [balance], info
