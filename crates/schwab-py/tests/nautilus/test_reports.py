"""Reports and account state built from recorded Schwab JSON."""

from __future__ import annotations

from decimal import Decimal

import pytest

pytest.importorskip("nautilus_trader")

from nautilus_trader.model.enums import (  # noqa: E402
    LiquiditySide,
    OrderSide,
    OrderStatus,
    OrderType,
    PositionSide,
    TimeInForce,
    TriggerType,
)
from nautilus_trader.model.identifiers import AccountId, ClientOrderId  # noqa: E402

from rustyschwab.nautilus.orders import parse_order  # noqa: E402
from rustyschwab.nautilus.reports import (  # noqa: E402
    account_balances,
    fill_reports,
    order_status_report,
    position_reports,
)

from .support import OPTION_SYMBOL, account_json, execution, position_json, schwab_order  # noqa: E402

ACCOUNT = AccountId("SCHWAB-HASH123")

# An order as Schwab returns it after two fills.
FILLED = schwab_order(
    1000000000001,
    tag="O-20261012-143000-001-001-1",
    quantity=10,
    filled=10,
    status="FILLED",
    closeTime="2026-10-12T14:32:00+0000",
    activities=[
        execution(900000000001, 4, 189.4, "2026-10-12T14:31:00+0000"),
        execution(900000000002, 6, 189.5, "2026-10-12T14:32:00+0000"),
    ],
)


def test_an_order_status_report_carries_everything_nautilus_reconciles_on() -> None:
    view = parse_order(FILLED)
    assert view is not None
    cid = ClientOrderId("O-20261012-143000-001-001-1")
    report = order_status_report(
        view, account_id=ACCOUNT, client_order_id=cid, ts_init=5, price_precision=2
    )
    assert report.account_id == ACCOUNT and report.client_order_id == cid
    assert report.venue_order_id.value == "1000000000001"
    assert str(report.instrument_id) == "AAPL.SCHWAB"
    assert (report.order_side, report.order_type, report.time_in_force) == (
        OrderSide.BUY, OrderType.LIMIT, TimeInForce.DAY,
    )
    assert report.order_status == OrderStatus.FILLED
    assert (int(report.quantity), int(report.filled_qty)) == (10, 10)
    assert str(report.price) == "189.50" and report.trigger_type == TriggerType.NO_TRIGGER
    assert report.avg_px == Decimal("189.46")
    assert report.ts_accepted == 1_791_815_400_000_000_000  # enteredTime
    assert report.ts_last == 1_791_815_520_000_000_000  # the last fill and the close


def test_a_stop_limit_report_has_both_prices_and_a_trigger_type() -> None:
    view = parse_order(
        schwab_order(2, order_type="STOP_LIMIT", price=179.5, stopPrice=180.0, instruction="SELL")
    )
    assert view is not None
    report = order_status_report(view, account_id=ACCOUNT, client_order_id=None, ts_init=5)
    assert (str(report.price), str(report.trigger_price)) == ("179.50", "180.00")
    assert report.trigger_type == TriggerType.DEFAULT and report.client_order_id is None


def test_fill_reports_one_per_execution() -> None:
    view = parse_order(FILLED)
    assert view is not None
    reports = fill_reports(view, account_id=ACCOUNT, client_order_id=None, ts_init=5, price_precision=2)
    assert [(r.trade_id.value, int(r.last_qty), str(r.last_px)) for r in reports] == [
        ("1000000000001-900000000001", 4, "189.40"),
        ("1000000000001-900000000002", 6, "189.50"),
    ]
    first = reports[0]
    assert first.order_side == OrderSide.BUY and first.liquidity_side == LiquiditySide.NO_LIQUIDITY_SIDE
    assert first.commission.as_double() == 0 and first.commission.currency.code == "USD"
    assert first.ts_event == 1_791_815_460_000_000_000


def test_positions_net_long_and_short_and_skip_what_is_not_modelled() -> None:
    account = account_json(
        positions=[
            position_json("AAPL", long=100, averageLongPrice=180.5),
            position_json(OPTION_SYMBOL, short=3, averageShortPrice=5.2),
            position_json("MSFT", long=10, short=10),
            position_json("VFIAX", long=5, instrument={"assetType": "MUTUAL_FUND", "symbol": "VFIAX"}),
        ]
    )
    reports = position_reports(account, account_id=ACCOUNT, ts_init=5)
    by_id = {str(r.instrument_id): r for r in reports}
    assert sorted(by_id) == ["AAPL.SCHWAB", "AAPL261218C00200000.SCHWAB", "MSFT.SCHWAB"]
    assert by_id["AAPL.SCHWAB"].position_side == PositionSide.LONG
    assert by_id["AAPL.SCHWAB"].avg_px_open == Decimal("180.5")
    assert by_id["AAPL261218C00200000.SCHWAB"].position_side == PositionSide.SHORT
    assert int(by_id["AAPL261218C00200000.SCHWAB"].quantity) == 3
    assert by_id["MSFT.SCHWAB"].position_side == PositionSide.FLAT


def test_a_margin_account_spends_available_funds() -> None:
    (balance,), info = account_balances(account_json(), "MARGIN")
    assert (float(balance.total), float(balance.free), float(balance.locked)) == (25_000, 18_000, 7_000)
    assert info["buyingPower"] == "36000.0"


def test_a_cash_account_spends_cash_available_for_trading() -> None:
    # Cash accounts have no availableFunds or buyingPower, as Schwab sends them.
    account = account_json(account_type="CASH")
    balances = account["securitiesAccount"]["currentBalances"]
    del balances["availableFunds"], balances["buyingPower"]
    (balance,), _ = account_balances(account, "CASH")
    assert (float(balance.total), float(balance.free), float(balance.locked)) == (25_000, 9_000, 16_000)


def test_free_funds_never_exceed_the_total() -> None:
    account = account_json()
    account["securitiesAccount"]["currentBalances"]["availableFunds"] = 99_999.0
    (balance,), _ = account_balances(account, "MARGIN")
    assert float(balance.free) == float(balance.total) == 25_000 and float(balance.locked) == 0


def test_balances_without_a_value_are_an_error_not_a_zero() -> None:
    account = account_json()
    account["securitiesAccount"]["currentBalances"] = {"cashBalance": 1.0}
    with pytest.raises(ValueError, match="lack"):
        account_balances(account, "MARGIN")
