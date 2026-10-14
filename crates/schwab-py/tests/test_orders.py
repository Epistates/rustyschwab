"""Order templates and option symbols: exact output, errors and the venues.

The templates are Python over the SDK's order builder, so these tests pin the
JSON the builder produces for each family and check that a bad order is
refused with the right exception before anything is sent.
"""

from __future__ import annotations

import datetime
import json
from decimal import Decimal
from pathlib import Path
from typing import Any, Callable

import pytest
from conftest import ACCOUNT, limit_order, paper_config

import rustyschwab
from rustyschwab import PaperBroker, ValidationError, orders, paper

EXPIRY = "2026-12-18"
CALL_190 = "AAPL  261218C00190000"
CALL_195 = "AAPL  261218C00195000"
CALL_200 = "AAPL  261218C00200000"
PUT_180 = "AAPL  261218P00180000"
PUT_185 = "AAPL  261218P00185000"
PUT_190 = "AAPL  261218P00190000"


def equity_leg(instruction: str, quantity: int, symbol: str = "AAPL") -> dict[str, Any]:
    return {
        "instruction": instruction,
        "quantity": quantity,
        "instrument": {"symbol": symbol, "assetType": "EQUITY"},
    }


def option_leg(instruction: str, symbol: str, quantity: int = 1) -> dict[str, Any]:
    return {
        "instruction": instruction,
        "quantity": quantity,
        "instrument": {"symbol": symbol, "assetType": "OPTION"},
    }


def single(order_type: str, legs: list[dict[str, Any]], **fields: Any) -> dict[str, Any]:
    return {
        "orderType": order_type,
        "session": "NORMAL",
        "duration": "DAY",
        "orderStrategyType": "SINGLE",
        "orderLegCollection": legs,
        **fields,
    }


# ---- equities ---------------------------------------------------------------


def test_a_limit_order_is_exactly_schwabs_shape() -> None:
    assert orders.equity_buy_limit("AAPL", 10, "189.50") == single(
        "LIMIT", [equity_leg("BUY", 10)], price="189.50"
    )
    assert orders.equity_buy_market("AAPL", 10) == single("MARKET", [equity_leg("BUY", 10)])


def test_keyword_options_reach_the_order() -> None:
    order = orders.equity_sell_limit(
        "AAPL",
        3,
        "201",
        session="SEAMLESS",
        duration="GOOD_TILL_CANCEL",
        tag="exit-1",
        special_instruction="ALL_OR_NONE",
        destination="NYSE",
        tax_lot_method="HIGH_COST",
    )
    assert order == single(
        "LIMIT",
        [equity_leg("SELL", 3)],
        price="201",
        session="SEAMLESS",
        duration="GOOD_TILL_CANCEL",
        tag="exit-1",
        specialInstruction="ALL_OR_NONE",
        requestedDestination="NYSE",
        taxLotMethod="HIGH_COST",
    )
    assert orders.equity_buy_market("AAPL", 1, tag=None) == single(
        "MARKET", [equity_leg("BUY", 1)]
    )


@pytest.mark.parametrize(
    ("market", "limit", "instruction"),
    [
        (orders.equity_buy_market, orders.equity_buy_limit, "BUY"),
        (orders.equity_sell_market, orders.equity_sell_limit, "SELL"),
        (orders.equity_sell_short_market, orders.equity_sell_short_limit, "SELL_SHORT"),
        (orders.equity_buy_to_cover_market, orders.equity_buy_to_cover_limit, "BUY_TO_COVER"),
    ],
)
def test_each_equity_side_has_its_instruction(
    market: Callable[..., Any], limit: Callable[..., Any], instruction: str
) -> None:
    assert market("XYZ", 2) == single("MARKET", [equity_leg(instruction, 2, "XYZ")])
    assert limit("XYZ", 2, "5.25") == single(
        "LIMIT", [equity_leg(instruction, 2, "XYZ")], price="5.25"
    )


@pytest.mark.parametrize(
    ("stop", "stop_limit", "trailing", "instruction"),
    [
        (
            orders.equity_buy_stop,
            orders.equity_buy_stop_limit,
            orders.equity_buy_trailing_stop,
            "BUY",
        ),
        (
            orders.equity_sell_stop,
            orders.equity_sell_stop_limit,
            orders.equity_sell_trailing_stop,
            "SELL",
        ),
        (
            orders.equity_sell_short_stop,
            orders.equity_sell_short_stop_limit,
            orders.equity_sell_short_trailing_stop,
            "SELL_SHORT",
        ),
        (
            orders.equity_buy_to_cover_stop,
            orders.equity_buy_to_cover_stop_limit,
            orders.equity_buy_to_cover_trailing_stop,
            "BUY_TO_COVER",
        ),
    ],
)
def test_stops_and_trailing_stops_for_each_side(
    stop: Callable[..., Any],
    stop_limit: Callable[..., Any],
    trailing: Callable[..., Any],
    instruction: str,
) -> None:
    leg = [equity_leg(instruction, 4)]
    assert stop("AAPL", 4, "95") == single("STOP", leg, stopPrice="95")
    assert stop_limit("AAPL", 4, "95", "94.50") == single(
        "STOP_LIMIT", leg, stopPrice="95", price="94.50"
    )
    assert trailing("AAPL", 4, amount="2.50") == single(
        "TRAILING_STOP",
        leg,
        stopPriceLinkBasis="LAST",
        stopPriceLinkType="VALUE",
        stopPriceOffset="2.50",
    )
    assert trailing("AAPL", 4, percent=5, basis="BID") == single(
        "TRAILING_STOP",
        leg,
        stopPriceLinkBasis="BID",
        stopPriceLinkType="PERCENT",
        stopPriceOffset="5",
    )


def test_a_trailing_stop_takes_exactly_one_of_amount_and_percent() -> None:
    for kwargs in ({}, {"amount": 1, "percent": 2}):
        with pytest.raises(ValidationError, match="exactly one of amount"):
            orders.equity_sell_trailing_stop("AAPL", 1, **kwargs)
    with pytest.raises(ValidationError, match="below 100"):
        orders.equity_sell_trailing_stop("AAPL", 1, percent=100)


def test_extended_hours_are_limit_only() -> None:
    assert orders.equity_buy_limit("AAPL", 1, "1", session="PM")["session"] == "PM"
    with pytest.raises(ValidationError, match="LIMIT"):
        orders.equity_buy_market("AAPL", 1, session="PM")


# ---- amounts ----------------------------------------------------------------


def test_amounts_keep_their_digits_in_every_accepted_type() -> None:
    for price in ("189.50", Decimal("189.50")):
        assert orders.equity_buy_limit("AAPL", 1, price)["price"] == "189.50"
    assert orders.equity_buy_limit("AAPL", 1, 190)["price"] == "190"
    assert orders.equity_buy_limit("AAPL", Decimal(2), "1")["orderLegCollection"][0][
        "quantity"
    ] == 2


def test_a_float_is_its_shortest_spelling_not_a_binary_expansion() -> None:
    assert orders.equity_buy_limit("AAPL", 1, 189.5)["price"] == "189.5"
    assert orders.equity_buy_limit("AAPL", 1, 0.1)["price"] == "0.1"
    # A computed float is sent as written; build prices from str or Decimal.
    assert orders.equity_buy_limit("AAPL", 1, 0.1 + 0.2)["price"] == "0.30000000000000004"


@pytest.mark.parametrize("bad", [True, None, "1e3", "1_000", " 5", "", float("nan"), "abc"])
def test_malformed_prices_are_refused(bad: Any) -> None:
    with pytest.raises(ValidationError):
        orders.equity_buy_limit("AAPL", 1, bad)


@pytest.mark.parametrize(
    "make",
    [
        lambda: orders.equity_buy_limit("AAPL", 1, "0"),
        lambda: orders.equity_buy_limit("AAPL", 1, "-1"),
        lambda: orders.equity_buy_market("AAPL", 0),
        lambda: orders.equity_buy_market("AAPL", "1.5"),
        lambda: orders.equity_buy_market("aapl", 1),
        lambda: orders.equity_buy_market("AAPL ", 1),
        lambda: orders.equity_buy_market("", 1),
        lambda: orders.equity_buy_market("AAPL", 1, duration="FOREVER"),  # type: ignore[arg-type]
        lambda: orders.equity_buy_market("AAPL", 1, durration="DAY"),  # type: ignore[call-arg]
    ],
)
def test_invalid_orders_raise_validation_error(make: Callable[[], Any]) -> None:
    with pytest.raises(ValidationError) as caught:
        make()
    assert isinstance(caught.value, rustyschwab.InvalidParameter)
    assert caught.value.kind == "invalid_parameter"


def test_a_zero_price_on_a_stop_limit_is_refused() -> None:
    with pytest.raises(ValidationError, match="positive"):
        orders.equity_buy_to_cover_stop_limit("AAPL", 1, "5", "0")


# ---- single options ---------------------------------------------------------


@pytest.mark.parametrize(
    ("market", "limit", "instruction"),
    [
        (orders.option_buy_to_open_market, orders.option_buy_to_open_limit, "BUY_TO_OPEN"),
        (orders.option_sell_to_open_market, orders.option_sell_to_open_limit, "SELL_TO_OPEN"),
        (orders.option_buy_to_close_market, orders.option_buy_to_close_limit, "BUY_TO_CLOSE"),
        (orders.option_sell_to_close_market, orders.option_sell_to_close_limit, "SELL_TO_CLOSE"),
    ],
)
def test_single_options_by_instruction(
    market: Callable[..., Any], limit: Callable[..., Any], instruction: str
) -> None:
    assert market(PUT_190, 3) == single("MARKET", [option_leg(instruction, PUT_190, 3)])
    assert limit(PUT_190, 3, "2.50") == single(
        "LIMIT", [option_leg(instruction, PUT_190, 3)], price="2.50"
    )


def test_options_trade_in_whole_contracts_in_the_regular_session() -> None:
    with pytest.raises(ValidationError):
        orders.option_buy_to_open_limit(PUT_190, "1.5", "2.50")
    with pytest.raises(ValidationError, match="NORMAL"):
        orders.option_buy_to_open_limit(PUT_190, 1, "2.50", session="PM")
    with pytest.raises(ValidationError):
        orders.option_buy_to_open_limit("AAPL", 1, "2.50")


# ---- vertical spreads -------------------------------------------------------


def vertical(order_type: str, legs: list[dict[str, Any]], price: str) -> dict[str, Any]:
    return single(order_type, legs, complexOrderStrategyType="VERTICAL", price=price)


def test_bull_call_spread_open_is_a_debit_and_close_a_credit() -> None:
    assert orders.bull_call_vertical_open(CALL_190, CALL_195, 2, "1.20") == vertical(
        "NET_DEBIT",
        [option_leg("BUY_TO_OPEN", CALL_190, 2), option_leg("SELL_TO_OPEN", CALL_195, 2)],
        "1.20",
    )
    assert orders.bull_call_vertical_close(CALL_190, CALL_195, 2, "1.50") == vertical(
        "NET_CREDIT",
        [option_leg("SELL_TO_CLOSE", CALL_190, 2), option_leg("BUY_TO_CLOSE", CALL_195, 2)],
        "1.50",
    )


def test_bear_call_spread_open_is_a_credit_and_close_a_debit() -> None:
    assert orders.bear_call_vertical_open(CALL_190, CALL_195, 1, "1.10") == vertical(
        "NET_CREDIT",
        [option_leg("SELL_TO_OPEN", CALL_190), option_leg("BUY_TO_OPEN", CALL_195)],
        "1.10",
    )
    assert orders.bear_call_vertical_close(CALL_190, CALL_195, 1, "0.40") == vertical(
        "NET_DEBIT",
        [option_leg("BUY_TO_CLOSE", CALL_190), option_leg("SELL_TO_CLOSE", CALL_195)],
        "0.40",
    )


def test_bull_put_spread_open_is_a_credit_and_close_a_debit() -> None:
    assert orders.bull_put_vertical_open(PUT_185, PUT_190, 1, "1.30") == vertical(
        "NET_CREDIT",
        [option_leg("BUY_TO_OPEN", PUT_185), option_leg("SELL_TO_OPEN", PUT_190)],
        "1.30",
    )
    assert orders.bull_put_vertical_close(PUT_185, PUT_190, 1, "0.30") == vertical(
        "NET_DEBIT",
        [option_leg("SELL_TO_CLOSE", PUT_185), option_leg("BUY_TO_CLOSE", PUT_190)],
        "0.30",
    )


def test_bear_put_spread_open_is_a_debit_and_close_a_credit() -> None:
    assert orders.bear_put_vertical_open(PUT_190, PUT_185, 1, "1.60") == vertical(
        "NET_DEBIT",
        [option_leg("BUY_TO_OPEN", PUT_190), option_leg("SELL_TO_OPEN", PUT_185)],
        "1.60",
    )
    assert orders.bear_put_vertical_close(PUT_190, PUT_185, 1, "2.10") == vertical(
        "NET_CREDIT",
        [option_leg("SELL_TO_CLOSE", PUT_190), option_leg("BUY_TO_CLOSE", PUT_185)],
        "2.10",
    )


@pytest.mark.parametrize(
    ("make", "message"),
    [
        # Strikes the wrong way round for the strategy.
        (lambda: orders.bull_call_vertical_open(CALL_195, CALL_190, 1, "1"), "below the strike"),
        (lambda: orders.bear_call_vertical_open(CALL_195, CALL_190, 1, "1"), "below the strike"),
        (lambda: orders.bull_put_vertical_open(PUT_190, PUT_185, 1, "1"), "below the strike"),
        (lambda: orders.bear_put_vertical_open(PUT_185, PUT_190, 1, "1"), "below the strike"),
        (lambda: orders.bull_call_vertical_open(CALL_190, CALL_190, 1, "1"), "below the strike"),
        # A put where a call belongs, and the reverse.
        (lambda: orders.bull_call_vertical_open(PUT_185, CALL_195, 1, "1"), "must be a call"),
        (lambda: orders.bull_put_vertical_open(PUT_185, CALL_195, 1, "1"), "must be a put"),
        # Different underlying or expiration.
        (
            lambda: orders.bull_call_vertical_open(CALL_190, "MSFT  261218C00195000", 1, "1"),
            "one underlying",
        ),
        (
            lambda: orders.bull_call_vertical_open(CALL_190, "AAPL  270115C00195000", 1, "1"),
            "one expiration",
        ),
        # Not an option symbol at all.
        (lambda: orders.bull_call_vertical_open("AAPL", CALL_195, 1, "1"), "OSI"),
    ],
)
def test_spreads_reject_legs_that_do_not_make_the_strategy(
    make: Callable[[], Any], message: str
) -> None:
    with pytest.raises(ValidationError, match=message):
        make()


# ---- other multi-leg strategies ---------------------------------------------


def test_straddles_and_strangles() -> None:
    def strategy(name: str, order_type: str, instruction: str, put: str, price: str) -> Any:
        return single(
            order_type,
            [option_leg(instruction, CALL_190, 2), option_leg(instruction, put, 2)],
            complexOrderStrategyType=name,
            price=price,
        )

    assert orders.long_straddle_open(CALL_190, PUT_190, 2, "9.80") == strategy(
        "STRADDLE", "NET_DEBIT", "BUY_TO_OPEN", PUT_190, "9.80"
    )
    assert orders.long_straddle_close(CALL_190, PUT_190, 2, "11") == strategy(
        "STRADDLE", "NET_CREDIT", "SELL_TO_CLOSE", PUT_190, "11"
    )
    assert orders.short_straddle_open(CALL_190, PUT_190, 2, "9.80") == strategy(
        "STRADDLE", "NET_CREDIT", "SELL_TO_OPEN", PUT_190, "9.80"
    )
    assert orders.short_straddle_close(CALL_190, PUT_190, 2, "5") == strategy(
        "STRADDLE", "NET_DEBIT", "BUY_TO_CLOSE", PUT_190, "5"
    )
    assert orders.long_strangle_open(CALL_190, PUT_185, 2, "6") == strategy(
        "STRANGLE", "NET_DEBIT", "BUY_TO_OPEN", PUT_185, "6"
    )
    assert orders.long_strangle_close(CALL_190, PUT_185, 2, "7") == strategy(
        "STRANGLE", "NET_CREDIT", "SELL_TO_CLOSE", PUT_185, "7"
    )
    assert orders.short_strangle_open(CALL_190, PUT_185, 2, "6") == strategy(
        "STRANGLE", "NET_CREDIT", "SELL_TO_OPEN", PUT_185, "6"
    )
    assert orders.short_strangle_close(CALL_190, PUT_185, 2, "3") == strategy(
        "STRANGLE", "NET_DEBIT", "BUY_TO_CLOSE", PUT_185, "3"
    )


def test_a_straddle_needs_one_strike_and_a_strangle_two() -> None:
    with pytest.raises(ValidationError, match="use a strangle"):
        orders.long_straddle_open(CALL_190, PUT_185, 1, "1")
    with pytest.raises(ValidationError, match="use a straddle"):
        orders.long_strangle_open(CALL_190, PUT_190, 1, "1")
    with pytest.raises(ValidationError, match="must be a put"):
        orders.long_strangle_open(CALL_190, CALL_195, 1, "1")


def test_iron_condor_sells_the_inner_strikes_for_a_credit() -> None:
    put_wing, put_body, call_body, call_wing = PUT_180, PUT_185, CALL_195, CALL_200
    expected_legs = [
        ("BUY_TO_OPEN", put_wing),
        ("SELL_TO_OPEN", put_body),
        ("SELL_TO_OPEN", call_body),
        ("BUY_TO_OPEN", call_wing),
    ]
    assert orders.iron_condor_open(put_wing, put_body, call_body, call_wing, 1, "1.80") == single(
        "NET_CREDIT",
        [option_leg(instruction, symbol) for instruction, symbol in expected_legs],
        complexOrderStrategyType="IRON_CONDOR",
        price="1.80",
    )
    closing = orders.iron_condor_close(put_wing, put_body, call_body, call_wing, 1, "0.60")
    assert closing["orderType"] == "NET_DEBIT"
    assert [leg["instruction"] for leg in closing["orderLegCollection"]] == [
        "SELL_TO_CLOSE",
        "BUY_TO_CLOSE",
        "BUY_TO_CLOSE",
        "SELL_TO_CLOSE",
    ]
    with pytest.raises(ValidationError, match="below the strike"):
        orders.iron_condor_open(put_body, put_wing, call_body, call_wing, 1, "1")


def test_calendar_spreads_sell_the_near_month_and_buy_the_far() -> None:
    near, far = CALL_190, "AAPL  270115C00190000"
    opened = orders.long_call_calendar_open(near, far, 1, "2.20")
    assert opened == single(
        "NET_DEBIT",
        [option_leg("SELL_TO_OPEN", near), option_leg("BUY_TO_OPEN", far)],
        complexOrderStrategyType="CALENDAR",
        price="2.20",
    )
    closed = orders.long_call_calendar_close(near, far, 1, "2.50")
    assert closed["orderType"] == "NET_CREDIT"
    assert [leg["instruction"] for leg in closed["orderLegCollection"]] == [
        "BUY_TO_CLOSE",
        "SELL_TO_CLOSE",
    ]
    put_near, put_far = PUT_190, "AAPL  270115P00190000"
    assert orders.long_put_calendar_open(put_near, put_far, 1, "2")["orderType"] == "NET_DEBIT"
    assert orders.long_put_calendar_close(put_near, put_far, 1, "2")["orderType"] == "NET_CREDIT"
    with pytest.raises(ValidationError, match="expire before"):
        orders.long_call_calendar_open(far, near, 1, "2")
    with pytest.raises(ValidationError, match="one strike"):
        orders.long_call_calendar_open(near, "AAPL  270115C00195000", 1, "2")


# ---- composite orders -------------------------------------------------------


def test_one_cancels_other_wraps_two_orders() -> None:
    target = orders.equity_sell_limit("XYZ", 2, "45.97")
    stop = orders.equity_sell_stop_limit("XYZ", 2, "37.03", "37.00")
    assert orders.one_cancels_other(target, stop) == {
        "orderStrategyType": "OCO",
        "childOrderStrategies": [target, stop],
    }


def test_first_triggers_second_makes_a_trigger_parent() -> None:
    entry = orders.equity_buy_limit("XYZ", 5, "14.97")
    exit_order = orders.equity_sell_limit("XYZ", 5, "15.27", duration="GOOD_TILL_CANCEL")
    assert orders.first_triggers_second(entry, exit_order) == {
        **entry,
        "orderStrategyType": "TRIGGER",
        "childOrderStrategies": [exit_order],
    }


def test_a_trigger_can_start_an_oco() -> None:
    entry = orders.equity_buy_limit("XYZ", 5, "14.97")
    exits = orders.one_cancels_other(
        orders.equity_sell_limit("XYZ", 5, "15.27"),
        orders.equity_sell_stop_limit("XYZ", 5, "11.30", "11.27"),
    )
    order = orders.first_triggers_second(entry, exits)
    assert order["childOrderStrategies"][0]["orderStrategyType"] == "OCO"
    rustyschwab.validate_order(order)


def test_a_bracket_adds_a_gtc_take_profit_and_stop_loss() -> None:
    entry = orders.equity_buy_limit("AAPL", 10, "150.25", tag="swing-1")
    bracketed = orders.bracket(entry, take_profit="160", stop_loss=145)
    exits = bracketed.pop("childOrderStrategies")
    assert bracketed == {**entry, "orderStrategyType": "TRIGGER"}
    assert exits == [
        {
            "orderStrategyType": "OCO",
            "childOrderStrategies": [
                single(
                    "LIMIT",
                    [equity_leg("SELL", 10)],
                    price="160",
                    duration="GOOD_TILL_CANCEL",
                ),
                single(
                    "STOP",
                    [equity_leg("SELL", 10)],
                    stopPrice="145",
                    duration="GOOD_TILL_CANCEL",
                ),
            ],
        }
    ]


def test_a_short_option_bracket_closes_with_the_covering_instruction() -> None:
    entry = orders.option_sell_to_open_limit(PUT_190, 2, "3.00")
    exits = orders.bracket(entry, take_profit="1.00", stop_loss="5.00")["childOrderStrategies"]
    legs = [child["orderLegCollection"][0] for child in exits[0]["childOrderStrategies"]]
    assert [leg["instruction"] for leg in legs] == ["BUY_TO_CLOSE", "BUY_TO_CLOSE"]
    assert [leg["quantity"] for leg in legs] == [2, 2]


def test_bracket_matches_the_builder_through_a_spec() -> None:
    entry = orders.equity_buy_limit("AAPL", 10, "150.25")
    spec = {
        "kind": "bracket",
        "entry": {
            "kind": "limit",
            "leg": {
                "instruction": "BUY",
                "quantity": 10,
                "instrument": {"asset_type": "EQUITY", "symbol": "AAPL"},
            },
            "price": "150.25",
        },
        "take_profit": "160",
        "stop_loss": "145",
    }
    assert orders.bracket(entry, "160", "145") == rustyschwab.build_order(spec)


@pytest.mark.parametrize(
    ("entry", "take_profit", "stop_loss", "message"),
    [
        (orders.equity_buy_limit("AAPL", 1, "150"), "140", "145", "inverted"),
        (orders.equity_buy_limit("AAPL", 1, "150"), "160", "155", "straddle"),
        (orders.equity_sell_limit("AAPL", 1, "150"), "140", "145", "does not open"),
        (orders.bull_call_vertical_open(CALL_190, CALL_195, 1, "1"), "2", "1", "exactly one leg"),
    ],
)
def test_bad_brackets_are_refused(
    entry: dict[str, Any], take_profit: str, stop_loss: str, message: str
) -> None:
    with pytest.raises(ValidationError, match=message):
        orders.bracket(entry, take_profit, stop_loss)


def test_composites_take_hand_written_orders_too() -> None:
    hand = limit_order("BUY", 2, "11", symbol="ABC")
    other = orders.equity_sell_limit("ABC", 2, "12")
    assert orders.one_cancels_other(hand, other)["childOrderStrategies"] == [hand, other]
    with pytest.raises(ValidationError):
        orders.one_cancels_other(hand, {"orderType": "LIMIT"})
    with pytest.raises(ValidationError, match="invalid"):
        orders.one_cancels_other(hand, {"ordertype": "LIMIT"})


# ---- build_order and validate_order -----------------------------------------


def test_build_order_expands_the_sdk_spec() -> None:
    spec = {
        "kind": "net_credit",
        "legs": [
            {
                "instruction": "SELL_TO_OPEN",
                "quantity": 1,
                "instrument": {
                    "asset_type": "OPTION",
                    "underlying": "aapl",
                    "expiration": datetime.date(2026, 12, 18),
                    "right": "PUT",
                    "strike": Decimal("190"),
                },
            },
            {
                "instruction": "BUY_TO_OPEN",
                "quantity": 1,
                "instrument": {"asset_type": "OPTION", "symbol": PUT_185},
            },
        ],
        "price": Decimal("1.05"),
        "settings": {"complex_strategy": "VERTICAL", "tag": "spec-1"},
    }
    assert rustyschwab.build_order(spec) == single(
        "NET_CREDIT",
        [option_leg("SELL_TO_OPEN", PUT_190), option_leg("BUY_TO_OPEN", PUT_185)],
        complexOrderStrategyType="VERTICAL",
        price="1.05",
        tag="spec-1",
    )


@pytest.mark.parametrize(
    "spec",
    [
        {"kind": "iceberg"},
        {"kind": "limit"},
        {"kind": "market", "leg": {"instruction": "BUY", "quantity": 1}},
        {"kind": "trigger", "entry": {"kind": "market"}, "children": []},
        "market",
        [],
    ],
)
def test_build_order_refuses_a_bad_spec(spec: Any) -> None:
    with pytest.raises(ValidationError, match="invalid order spec"):
        rustyschwab.build_order(spec)


def test_build_order_applies_the_sdk_validation() -> None:
    spec = {
        "kind": "market",
        "leg": {
            "instruction": "BUY_TO_OPEN",
            "quantity": 1,
            "instrument": {"asset_type": "EQUITY", "symbol": "AAPL"},
        },
    }
    with pytest.raises(ValidationError):
        rustyschwab.build_order(spec)


def test_validate_order_accepts_what_the_templates_build() -> None:
    for order in (
        orders.equity_buy_limit("AAPL", 10, "189.50"),
        orders.bull_call_vertical_open(CALL_190, CALL_195, 1, "1.20"),
        orders.bracket(orders.equity_buy_limit("AAPL", 10, "150.25"), 160, 145),
        limit_order("BUY", 2, "11"),
    ):
        assert rustyschwab.validate_order(order) is None


def test_validate_order_names_the_failing_field() -> None:
    order = limit_order("BUY", 2, "11")
    order["stopPrice"] = "10"
    with pytest.raises(ValidationError) as caught:
        rustyschwab.validate_order(order)
    assert caught.value.field == "stopPrice"

    order = limit_order("BUY", 2, "11")
    order["orderLegCollection"][0]["quantity"] = 0
    with pytest.raises(ValidationError) as caught:
        rustyschwab.validate_order(order)
    assert caught.value.field == "orderLegCollection[0].quantity"

    with pytest.raises(ValidationError, match="invalid order"):
        rustyschwab.validate_order({"ordertype": "LIMIT"})


def test_built_orders_are_plain_json() -> None:
    order = orders.bracket(orders.equity_buy_limit("AAPL", 10, "150.25"), 160, 145)
    assert json.loads(json.dumps(order)) == order
    assert type(order) is dict


# ---- option symbols ---------------------------------------------------------


def test_option_symbols_follow_the_osi_layout() -> None:
    assert rustyschwab.option_symbol("AAPL", datetime.date(2024, 5, 17), "PUT", 190) == (
        "AAPL  240517P00190000"
    )
    assert rustyschwab.option_symbol("AAPL", "2024-05-17", "PUT", "190") == "AAPL  240517P00190000"
    assert rustyschwab.option_symbol("spy", "2026-01-16", "CALL", Decimal("592.5")) == (
        "SPY   260116C00592500"
    )
    assert rustyschwab.option_symbol("BRKB", "2026-01-16", "CALL", "0.5") == "BRKB  260116C00000500"
    assert rustyschwab.option_symbol("TSLA", "2026-01-16", "CALL", 99999.999) == (
        "TSLA  260116C99999999"
    )


@pytest.mark.parametrize(
    "symbol",
    [
        "AAPL  240517P00190000",
        "SPY   260116C00592500",
        "BRKB  260116C00000500",
        "ABCDEF991231C00001001",
    ],
)
def test_option_symbols_round_trip(symbol: str) -> None:
    parts = rustyschwab.parse_option_symbol(symbol)
    rebuilt = rustyschwab.option_symbol(
        parts["underlying"],
        datetime.date.fromisoformat(parts["expiration"]),
        parts["put_call"],
        Decimal(parts["strike"]),
    )
    assert rebuilt == symbol


def test_parse_option_symbol_splits_the_fields() -> None:
    assert rustyschwab.parse_option_symbol("AAPL  240517P00190000") == {
        "underlying": "AAPL",
        "expiration": "2024-05-17",
        "put_call": "PUT",
        "strike": "190",
    }
    assert rustyschwab.parse_option_symbol("SPY   260116C00592500") == {
        "underlying": "SPY",
        "expiration": "2026-01-16",
        "put_call": "CALL",
        "strike": "592.5",
    }


@pytest.mark.parametrize(
    "args",
    [
        ("AAPL", "2024-05-17", "put", 190),
        ("AAPL", "2024-05-17", "P", 190),
        ("AAPL", "2024-05-17", "CALL", 0),
        ("AAPL", "2024-05-17", "CALL", -5),
        ("AAPL", "2024-05-17", "CALL", "190.0005"),
        ("AAPL", "2024-05-17", "CALL", 100000),
        ("AAPL", "2024-05-17", "CALL", True),
        ("TOOLONG", "2024-05-17", "CALL", 190),
        ("", "2024-05-17", "CALL", 190),
        ("AA PL", "2024-05-17", "CALL", 190),
        ("AAPL", "2024-13-01", "CALL", 190),
        ("AAPL", "05/17/2024", "CALL", 190),
        ("AAPL", "1999-12-17", "CALL", 190),
        ("AAPL", None, "CALL", 190),
        ("AAPL", 20240517, "CALL", 190),
        ("AAPL", datetime.datetime(2024, 5, 17), "CALL", 190),
        ("AAPL", datetime.datetime(2024, 5, 17, tzinfo=datetime.timezone.utc), "CALL", 190),
    ],
)
def test_option_symbol_rejects_what_the_format_cannot_carry(args: tuple[Any, ...]) -> None:
    with pytest.raises(ValidationError):
        rustyschwab.option_symbol(*args)


@pytest.mark.parametrize(
    "symbol",
    [
        "",
        "AAPL",
        "AAPL 240517P00190000",
        "aapl  240517P00190000",
        "AAPL  240517X00190000",
        "AAPL  241317P00190000",
        "AAPL  240517P0019000",
        "AAPL  240517P001900000",
        "AAPL  240517p00190000",
        "AAPL  240517P0019000A",
        "AAPL  240517P00000000",
    ],
)
def test_parse_option_symbol_accepts_only_the_canonical_form(symbol: str) -> None:
    with pytest.raises(ValidationError):
        rustyschwab.parse_option_symbol(symbol)


def test_templates_accept_built_option_symbols() -> None:
    long_call = rustyschwab.option_symbol("AAPL", datetime.date(2026, 12, 18), "CALL", 190)
    short_call = rustyschwab.option_symbol("AAPL", datetime.date(2026, 12, 18), "CALL", 195)
    assert orders.bull_call_vertical_open(long_call, short_call, 1, "1.20") == (
        orders.bull_call_vertical_open(CALL_190, CALL_195, 1, "1.20")
    )


def test_the_package_exports_the_builders() -> None:
    assert rustyschwab.orders is orders
    for name in ("build_order", "validate_order", "option_symbol", "parse_option_symbol", "orders"):
        assert name in rustyschwab.__all__
    for name in orders.__all__:
        assert hasattr(orders, name), name


# ---- the venues take what the builders make ---------------------------------


@pytest.fixture
async def broker(tmp_path: Path) -> PaperBroker:
    venue = await PaperBroker.open(tmp_path / "state" / "paper.sqlite3", paper_config())
    await venue.apply(paper.session(1, 0, "session-1"))
    await venue.apply(
        paper.quote(2, 100, "ABC", bid="10", ask="10.01", bid_size="100", ask_size="100")
    )
    return venue


async def test_the_paper_venue_accepts_a_built_equity_order(broker: PaperBroker) -> None:
    order = orders.equity_buy_limit("ABC", 2, "11", tag="built-1")
    placed = await broker.place_order(ACCOUNT, order)
    assert placed["tag"] == "built-1"

    result = await broker.apply(
        paper.quote(3, 200, "ABC", bid="10", ask="10.01", bid_size="100", ask_size="100")
    )
    assert len(result["fills"]) == 1
    filled = await broker.order(ACCOUNT, placed["order_id"])
    assert filled["status"] == "FILLED"
    assert Decimal(filled["filled_quantity"]) == 2


async def test_a_template_matches_the_hand_written_order(broker: PaperBroker) -> None:
    assert orders.equity_buy_limit("ABC", 2, "11") == limit_order("BUY", 2, "11")
    placed = await broker.place_order(ACCOUNT, orders.equity_buy_limit("ABC", 2, "9"))
    assert placed["order_id"]
    working = await broker.snapshot(ACCOUNT)
    assert [o["order_id"] for o in working["working_orders"]] == [placed["order_id"]]
