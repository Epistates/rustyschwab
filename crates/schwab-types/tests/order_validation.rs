//! `OrderRequest::validate` refuses orders Schwab would reject or misread,
//! before anything is sent.

use chrono::NaiveDate;
use schwab_types::Decimal;
use schwab_types::trading::{
    OptionSymbol, OrderDuration, OrderInstruction, OrderRequest, OrderRequestLeg, OrderSession,
    OrderType, PriceLinkBasis, PriceLinkType,
};
use serde_json::json;

fn d(s: &str) -> Decimal {
    s.parse().unwrap()
}

fn buy(n: u32) -> OrderRequestLeg {
    OrderRequestLeg::equity(OrderInstruction::Buy, "AAPL", n)
}

fn parse(value: serde_json::Value) -> Result<OrderRequest, String> {
    serde_json::from_value::<OrderRequest>(value).map_err(|e| e.to_string())
}

fn rejects(order: &OrderRequest, needle: &str) {
    let err = order.validate().expect_err("order should be rejected");
    assert!(err.contains(needle), "{err:?} does not mention {needle:?}");
}

#[test]
fn bracket_exits_must_straddle_a_priced_entry() {
    let entry = OrderRequest::limit(buy(10), d("150"));
    // Both exits above a long entry: the stop would fire at once.
    assert!(entry.clone().bracket(d("170"), d("160")).is_err());
    // Both below: the take-profit is marketable at once.
    assert!(entry.clone().bracket(d("140"), d("130")).is_err());
    entry.clone().bracket(d("160"), d("140")).unwrap();
    // Shorts mirror it.
    let short = OrderRequest::limit(
        OrderRequestLeg::equity(OrderInstruction::SellShort, "AAPL", 10),
        d("150"),
    );
    assert!(short.clone().bracket(d("155"), d("160")).is_err());
    short.bracket(d("140"), d("160")).unwrap();
    // A market entry has no price to compare.
    OrderRequest::market(buy(10))
        .bracket(d("170"), d("160"))
        .unwrap();
}

#[test]
fn top_level_quantity_must_match_the_legs() {
    let mut order = OrderRequest::limit(buy(10), d("150"));
    order.quantity = Some(d("999"));
    rejects(&order, "quantity must equal");
    order.quantity = Some(d("10"));
    order.validate().unwrap();
}

#[test]
fn each_order_type_takes_exactly_its_fields() {
    let mut market = OrderRequest::market(buy(1));
    market.price = Some(d("1"));
    rejects(&market, "does not take price");

    let mut limit = OrderRequest::limit(buy(1), d("1"));
    limit.stop_price = Some(d("1"));
    rejects(&limit, "does not take stopPrice");

    let mut limit = OrderRequest::limit(buy(1), d("1"));
    limit.stop_price_offset = Some(d("1"));
    limit.stop_price_link_basis = Some(PriceLinkBasis::Bid);
    limit.stop_price_link_type = Some(PriceLinkType::Value);
    rejects(&limit, "does not take stopPriceOffset");

    let mut stop = OrderRequest::stop(buy(1), d("1"));
    stop.price = Some(d("1"));
    rejects(&stop, "does not take price");

    for kind in [
        OrderType::Cabinet,
        OrderType::Exercise,
        OrderType::NonMarketable,
    ] {
        let mut order = OrderRequest::market(buy(1));
        order.order_type = Some(kind);
        rejects(&order, "cannot be submitted");
    }

    let single_leg_debit = OrderRequest::net_debit(
        vec![OrderRequestLeg::option(
            OrderInstruction::BuyToOpen,
            "XYZ   240315P00045000",
            1,
        )],
        d("0.10"),
    );
    rejects(&single_leg_debit, "two or more legs");
}

#[test]
fn trailing_stops_trail_a_real_basis_by_a_sane_amount() {
    let trailing = |basis, link, offset: &str| {
        OrderRequest::trailing_stop(
            OrderRequestLeg::equity(OrderInstruction::Sell, "AAPL", 1),
            basis,
            link,
            d(offset),
        )
    };
    trailing(PriceLinkBasis::Bid, PriceLinkType::Percent, "5")
        .validate()
        .unwrap();
    rejects(
        &trailing(PriceLinkBasis::Bid, PriceLinkType::Percent, "150"),
        "below 100",
    );
    rejects(
        &trailing(PriceLinkBasis::Manual, PriceLinkType::Value, "1"),
        "MANUAL",
    );
    // A value offset is a price and must respect the tick.
    let value = trailing(PriceLinkBasis::Bid, PriceLinkType::Value, "0.005");
    value.validate().unwrap();
    assert!(value.validate_price_increment(d("0.01")).is_err());
}

#[test]
fn sessions_and_durations_fit_the_order_type() {
    rejects(
        &OrderRequest::market(buy(1)).with_session(OrderSession::Pm),
        "only LIMIT",
    );
    OrderRequest::limit(buy(1), d("1"))
        .with_session(OrderSession::Pm)
        .validate()
        .unwrap();
    let mut moc = OrderRequest::market(buy(1));
    moc.order_type = Some(OrderType::MarketOnClose);
    moc.validate().unwrap();
    rejects(&moc.with_duration(OrderDuration::Gtc), "DAY duration");
}

#[test]
fn instructions_symbols_and_quantities_fit_the_asset() {
    let equity_with_option_instruction = OrderRequest::market(OrderRequestLeg::equity(
        OrderInstruction::BuyToOpen,
        "AAPL",
        1,
    ));
    rejects(&equity_with_option_instruction, "option instruction");

    let option_with_equity_instruction = OrderRequest::market(OrderRequestLeg::option(
        OrderInstruction::Buy,
        "AAPL  240517P00190000",
        1,
    ));
    rejects(&option_with_equity_instruction, "not an option instruction");

    let not_osi = OrderRequest::market(OrderRequestLeg::option(
        OrderInstruction::BuyToOpen,
        "AAPL",
        1,
    ));
    rejects(&not_osi, "OSI");

    let fractional_contracts = OrderRequest::market(OrderRequestLeg::option(
        OrderInstruction::BuyToOpen,
        "AAPL  240517P00190000",
        d("1.5"),
    ));
    rejects(&fractional_contracts, "whole contracts");
    rejects(
        &OrderRequest::market(OrderRequestLeg::equity(
            OrderInstruction::Buy,
            "AAPL",
            d("0.001"),
        )),
        "whole shares",
    );
    for symbol in [" aapl ", "aapl", "AAPL "] {
        rejects(
            &OrderRequest::market(OrderRequestLeg::equity(OrderInstruction::Buy, symbol, 1)),
            "upper case",
        );
    }
}

#[test]
fn oco_parents_carry_nothing_executable() {
    let mut oco = OrderRequest::one_cancels_other(
        OrderRequest::limit(buy(1), d("1")),
        OrderRequest::limit(buy(1), d("2")),
    );
    oco.validate().unwrap();
    oco.session = Some(OrderSession::Normal);
    rejects(&oco, "no executable parent");
    oco.session = None;
    oco.stop_price_offset = Some(d("1"));
    rejects(&oco, "no executable parent");
}

#[test]
fn strategies_nest_at_most_eight_levels() {
    let leaf = || OrderRequest::limit(buy(1), d("1"));
    let nest = |levels: usize| {
        let mut order = leaf();
        for _ in 1..levels {
            order = leaf().triggers(order);
        }
        order
    };
    nest(8).validate().unwrap();
    rejects(&nest(9), "at most 8 levels");
}

#[test]
fn unknown_enum_values_decode_but_are_never_sent() {
    let order = parse(json!({
        "orderType": "ICEBERG", "session": "NORMAL", "duration": "DAY",
        "price": "1", "orderStrategyType": "SINGLE",
        "orderLegCollection": [{"instruction": "BUY", "quantity": "1",
            "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}}]
    }))
    .unwrap();
    assert_eq!(order.order_type, Some(OrderType::Unknown));
    rejects(&order, "orderType is not a value Schwab accepts");

    let leg = parse(json!({
        "orderType": "MARKET", "session": "NORMAL", "duration": "DAY",
        "orderStrategyType": "SINGLE",
        "orderLegCollection": [{"instruction": "BUY", "quantity": "1",
            "instrument": {"symbol": "AAPL", "assetType": "CRYPTO"}}]
    }))
    .unwrap();
    rejects(&leg, "assetType");
}

#[test]
fn request_decimals_parse_exactly_or_not_at_all() {
    let order = |price: serde_json::Value| {
        parse(json!({
            "orderType": "LIMIT", "session": "NORMAL", "duration": "DAY",
            "price": price, "orderStrategyType": "SINGLE",
            "orderLegCollection": [{"instruction": "BUY", "quantity": "1",
                "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}}]
        }))
    };
    assert_eq!(order(json!("150.25")).unwrap().price, Some(d("150.25")));
    assert_eq!(order(json!(150)).unwrap().price, Some(d("150")));
    for bad in [
        json!("1_000"),
        json!(" 1"),
        json!("1e3"),
        json!("1.00000000000000000000000000001"),
        json!(""),
    ] {
        assert!(order(bad.clone()).is_err(), "{bad} should be rejected");
    }
}

#[test]
fn osi_symbols_parse_only_their_canonical_form() {
    // 2070 is 2070, not 1970.
    let late: OptionSymbol = "AAPL  700117C00100000".parse().unwrap();
    assert_eq!(
        late.expiration(),
        NaiveDate::from_ymd_opt(2070, 1, 17).unwrap()
    );
    for padded in ["  AAPL240517P00190000", "aapl  240517P00190000"] {
        assert!(padded.parse::<OptionSymbol>().is_err(), "{padded}");
    }
}
