"""Order translation: Nautilus orders to Schwab JSON, and Schwab orders back."""

from __future__ import annotations

from decimal import Decimal
from typing import Any

import pytest

pytest.importorskip("nautilus_trader")

from nautilus_trader.model.enums import (  # noqa: E402
    OrderSide,
    OrderStatus,
    OrderType,
    TimeInForce,
    TriggerType,
)
from nautilus_trader.model.identifiers import ClientOrderId  # noqa: E402
from nautilus_trader.model.instruments import Equity, OptionContract  # noqa: E402
from nautilus_trader.model.objects import Price, Quantity  # noqa: E402
from nautilus_trader.model.orders import Order  # noqa: E402

from rustyschwab.nautilus.orders import (  # noqa: E402
    UnsupportedOrder,
    instruction_for,
    parse_order,
    spec_from_order,
    split_tag,
    tag_for,
    to_schwab_order,
)

from .support import Stack, execution, schwab_order  # noqa: E402


def outgoing(order: Order, instrument: Equity | OptionContract, net: str = "0", **kwargs: Any) -> dict[str, Any]:
    spec = spec_from_order(order, instrument)
    instruction = instruction_for(spec, Decimal(net), kwargs.pop("allow_short", False))
    return to_schwab_order(spec, tag=tag_for(order.client_order_id), instruction=instruction, **kwargs)


# ---- tags -------------------------------------------------------------------------


def test_a_tag_carries_the_client_order_id_and_a_replacement_revision() -> None:
    cid = ClientOrderId("O-20261012-143000-001-001-7")
    assert tag_for(cid) == "O-20261012-143000-001-001-7"
    assert tag_for(cid, 2) == "O-20261012-143000-001-001-7-R2"
    assert split_tag(tag_for(cid, 2)) == ("O-20261012-143000-001-001-7", 2)
    assert split_tag(tag_for(cid)) == ("O-20261012-143000-001-001-7", 0)
    assert split_tag("rs-3a7c") == ("rs-3a7c", 0)


# ---- Nautilus to Schwab -------------------------------------------------------------


def test_a_day_limit_buy(stack: Stack, aapl: Equity) -> None:
    order = stack.orders.limit(
        aapl.id, OrderSide.BUY, Quantity.from_int(10), Price.from_str("189.50"),
        time_in_force=TimeInForce.DAY,
    )
    assert outgoing(order, aapl) == {
        "orderStrategyType": "SINGLE",
        "orderType": "LIMIT",
        "session": "NORMAL",
        "duration": "DAY",
        "price": "189.5",
        "tag": order.client_order_id.value,
        "orderLegCollection": [
            {
                "instruction": "BUY",
                "quantity": 10,
                "instrument": {"symbol": "AAPL", "assetType": "EQUITY"},
            }
        ],
    }


def test_gtc_market_stop_and_stop_limit_orders(stack: Stack, aapl: Equity) -> None:
    gtc = outgoing(
        stack.orders.limit(
            aapl.id, OrderSide.BUY, Quantity.from_int(1), Price.from_str("10.00"),
            time_in_force=TimeInForce.GTC,
        ),
        aapl,
    )
    assert gtc["duration"] == "GOOD_TILL_CANCEL"

    market = outgoing(stack.orders.market(aapl.id, OrderSide.BUY, Quantity.from_int(5)), aapl)
    assert market["orderType"] == "MARKET" and "price" not in market and "stopPrice" not in market

    stop = outgoing(
        stack.orders.stop_market(
            aapl.id, OrderSide.SELL, Quantity.from_int(5), Price.from_str("180.00"),
            trigger_type=TriggerType.LAST_PRICE,
        ),
        aapl,
        net="5",
    )
    assert stop["orderType"] == "STOP" and stop["stopPrice"] == "180" and stop["stopType"] == "LAST"
    assert "price" not in stop

    stop_limit = outgoing(
        stack.orders.stop_limit(
            aapl.id, OrderSide.SELL, Quantity.from_int(5), Price.from_str("179.50"),
            Price.from_str("180.00"),
        ),
        aapl,
        net="5",
    )
    assert stop_limit["orderType"] == "STOP_LIMIT"
    assert (stop_limit["price"], stop_limit["stopPrice"]) == ("179.5", "180")
    assert "stopType" not in stop_limit


def test_an_option_order(stack: Stack, aapl_call: OptionContract) -> None:
    order = stack.orders.limit(
        aapl_call.id, OrderSide.BUY, Quantity.from_int(2), Price.from_str("5.25"),
        time_in_force=TimeInForce.DAY,
    )
    sent = outgoing(order, aapl_call)
    leg = sent["orderLegCollection"][0]
    assert leg["instruction"] == "BUY_TO_OPEN"
    # Schwab gets its own padded OCC symbol, not the compact ID.
    assert leg["instrument"] == {"symbol": "AAPL  261218C00200000", "assetType": "OPTION"}


def test_a_session_is_passed_through(stack: Stack, aapl: Equity) -> None:
    order = stack.orders.limit(aapl.id, OrderSide.BUY, Quantity.from_int(1), Price.from_str("1.00"))
    assert outgoing(order, aapl, session="SEAMLESS")["session"] == "SEAMLESS"


@pytest.mark.parametrize(
    ("side", "quantity", "net", "allow_short", "expected"),
    [
        (OrderSide.BUY, 10, "0", False, "BUY"),
        (OrderSide.BUY, 10, "40", False, "BUY"),
        (OrderSide.BUY, 10, "-10", False, "BUY_TO_COVER"),
        (OrderSide.SELL, 10, "10", False, "SELL"),
        (OrderSide.SELL, 10, "25", False, "SELL"),
        (OrderSide.SELL, 10, "0", True, "SELL_SHORT"),
        (OrderSide.SELL, 10, "-5", True, "SELL_SHORT"),
    ],
)
def test_equity_instructions_follow_the_net_position(
    stack: Stack, aapl: Equity, side: OrderSide, quantity: int, net: str, allow_short: bool, expected: str
) -> None:
    order = stack.orders.limit(aapl.id, side, Quantity.from_int(quantity), Price.from_str("1.00"))
    spec = spec_from_order(order, aapl)
    assert instruction_for(spec, Decimal(net), allow_short) == expected


@pytest.mark.parametrize(
    ("side", "net", "allow_short", "expected"),
    [
        (OrderSide.BUY, "0", False, "BUY_TO_OPEN"),
        (OrderSide.BUY, "-2", False, "BUY_TO_CLOSE"),
        (OrderSide.SELL, "2", False, "SELL_TO_CLOSE"),
        (OrderSide.SELL, "0", True, "SELL_TO_OPEN"),
    ],
)
def test_option_instructions_name_the_opening_or_closing(
    stack: Stack, aapl_call: OptionContract, side: OrderSide, net: str, allow_short: bool, expected: str
) -> None:
    order = stack.orders.limit(aapl_call.id, side, Quantity.from_int(2), Price.from_str("1.00"))
    assert instruction_for(spec_from_order(order, aapl_call), Decimal(net), allow_short) == expected


def test_opening_a_short_or_crossing_zero_is_refused(stack: Stack, aapl: Equity) -> None:
    sell = spec_from_order(
        stack.orders.limit(aapl.id, OrderSide.SELL, Quantity.from_int(10), Price.from_str("1.00")),
        aapl,
    )
    with pytest.raises(UnsupportedOrder, match="open a short"):
        instruction_for(sell, Decimal(0), allow_short=False)
    with pytest.raises(UnsupportedOrder, match="cross"):
        instruction_for(sell, Decimal(4), allow_short=True)
    buy = spec_from_order(
        stack.orders.limit(aapl.id, OrderSide.BUY, Quantity.from_int(10), Price.from_str("1.00")),
        aapl,
    )
    with pytest.raises(UnsupportedOrder, match="cross"):
        instruction_for(buy, Decimal(-4), allow_short=True)


def test_what_schwab_cannot_honour_is_refused_with_a_reason(stack: Stack, aapl: Equity) -> None:
    limit = lambda **kw: stack.orders.limit(  # noqa: E731
        aapl.id, OrderSide.BUY, Quantity.from_int(1), Price.from_str("1.00"), **kw
    )
    cases: list[tuple[Order, str]] = [
        (limit(time_in_force=TimeInForce.IOC), "time in force IOC"),
        (limit(time_in_force=TimeInForce.FOK), "time in force FOK"),
        (limit(reduce_only=True), "reduce-only"),
        (limit(post_only=True), "post-only"),
        (
            stack.orders.trailing_stop_market(
                aapl.id, OrderSide.SELL, Quantity.from_int(1), Decimal("1.00"),
            ),
            "TRAILING_STOP_MARKET",
        ),
        (
            stack.orders.market_if_touched(
                aapl.id, OrderSide.BUY, Quantity.from_int(1), Price.from_str("1.00"),
            ),
            "MARKET_IF_TOUCHED",
        ),
        (
            stack.orders.stop_market(
                aapl.id, OrderSide.SELL, Quantity.from_int(1), Price.from_str("1.00"),
                trigger_type=TriggerType.BID_ASK,
            ),
            "trigger type BID_ASK",
        ),
    ]
    for order, reason in cases:
        with pytest.raises(UnsupportedOrder, match=reason):
            spec_from_order(order, aapl)


def test_an_instrument_that_is_neither_an_equity_nor_an_option_is_refused(stack: Stack) -> None:
    from nautilus_trader.test_kit.providers import TestInstrumentProvider

    pair = TestInstrumentProvider.default_fx_ccy("AUD/USD")
    order = stack.orders.limit(pair.id, OrderSide.BUY, Quantity.from_int(1000), Price.from_str("0.65000"))
    with pytest.raises(UnsupportedOrder, match="not an equity or an equity option"):
        spec_from_order(order, pair)


# ---- Schwab to Nautilus -------------------------------------------------------------


def test_a_working_limit_order_parses() -> None:
    view = parse_order(schwab_order(1001, tag="O-1-1", instruction="BUY"))
    assert view is not None
    assert (view.order_id, view.tag) == ("1001", "O-1-1")
    assert str(view.instrument_id) == "AAPL.SCHWAB"
    assert (view.side, view.order_type, view.time_in_force) == (
        OrderSide.BUY, OrderType.LIMIT, TimeInForce.DAY,
    )
    assert view.status == OrderStatus.ACCEPTED and view.is_open
    assert (view.quantity, view.filled_quantity, view.price) == (10, 0, Decimal("189.5"))
    assert view.fills == ()


@pytest.mark.parametrize(
    ("schwab", "nautilus"),
    [
        ("QUEUED", OrderStatus.ACCEPTED),
        ("AWAITING_STOP_CONDITION", OrderStatus.ACCEPTED),
        ("PENDING_ACTIVATION", OrderStatus.ACCEPTED),
        ("PENDING_CANCEL", OrderStatus.PENDING_CANCEL),
        ("PENDING_REPLACE", OrderStatus.PENDING_UPDATE),
        ("CANCELED", OrderStatus.CANCELED),
        ("REJECTED", OrderStatus.REJECTED),
        ("EXPIRED", OrderStatus.EXPIRED),
        ("FILLED", OrderStatus.FILLED),
    ],
)
def test_schwab_statuses_map_to_nautilus(schwab: str, nautilus: OrderStatus) -> None:
    view = parse_order(schwab_order(1, status=schwab))
    assert view is not None and view.status == nautilus


def test_a_partly_filled_working_order_is_partially_filled() -> None:
    view = parse_order(schwab_order(1, filled=4, status="WORKING"))
    assert view is not None and view.status == OrderStatus.PARTIALLY_FILLED and view.is_open


def test_executions_become_fills_with_stable_ids() -> None:
    activities = [
        execution(900000000001, 4, 189.4, "2026-10-12T14:31:00+0000"),
        execution(900000000002, 6, 189.5, "2026-10-12T14:32:00+0000"),
        {"activityType": "EXECUTION", "executionType": "CANCELED", "executionLegs": []},
        {"activityType": "ORDER_ACTION"},
    ]
    view = parse_order(schwab_order(1001, filled=10, status="FILLED", activities=activities))
    assert view is not None
    assert [(f.trade_id, f.quantity, f.price) for f in view.fills] == [
        ("1001-900000000001", Decimal(4), Decimal("189.4")),
        ("1001-900000000002", Decimal(6), Decimal("189.5")),
    ]
    assert view.fills[0].time_ns == 1_791_815_460_000_000_000
    assert view.average_price == Decimal("189.46")


def test_without_an_activity_id_the_position_names_the_fill() -> None:
    activity = execution(1, 4, 189.4, "2026-10-12T14:31:00+0000")
    del activity["activityId"]
    view = parse_order(schwab_order(7, filled=4, activities=[activity]))
    assert view is not None and view.fills[0].trade_id == "7-0"


def test_an_option_order_parses_to_the_compact_instrument_id() -> None:
    view = parse_order(
        schwab_order(
            5, symbol="AAPL  261218C00200000", asset_type="OPTION",
            instruction="SELL_TO_CLOSE", quantity=2, price=5.3,
        )
    )
    assert view is not None
    assert str(view.instrument_id) == "AAPL261218C00200000.SCHWAB"
    assert view.side == OrderSide.SELL


def test_stop_orders_carry_their_trigger() -> None:
    view = parse_order(
        schwab_order(1, order_type="STOP_LIMIT", price=179.5, stopPrice=180.0, instruction="SELL")
    )
    assert view is not None
    assert (view.order_type, view.price, view.trigger_price) == (
        OrderType.STOP_LIMIT, Decimal("179.5"), Decimal(180),
    )
    stop = parse_order(schwab_order(2, order_type="STOP", price=None, stopPrice=180.0))
    assert stop is not None and stop.price is None and stop.order_type == OrderType.STOP_MARKET


def test_orders_the_adapter_does_not_model_are_left_out() -> None:
    assert parse_order(schwab_order(1, status="REPLACED")) is None
    assert parse_order(schwab_order(1, order_type="TRAILING_STOP")) is None
    assert parse_order(schwab_order(1, status="SOMETHING_NEW")) is None
    assert parse_order(schwab_order(1, quantity=0.5)) is None
    multi = schwab_order(1)
    multi["orderLegCollection"].append(multi["orderLegCollection"][0])
    assert parse_order(multi) is None
    mutual = schwab_order(1, symbol="VFIAX", asset_type="MUTUAL_FUND")
    assert parse_order(mutual) is None
    assert parse_order({"status": "WORKING"}) is None
