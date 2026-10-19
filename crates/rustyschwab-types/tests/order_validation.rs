//! `OrderRequest::validate` refuses orders Schwab would reject or misread,
//! before anything is sent.

use chrono::NaiveDate;
use rustyschwab_types::trading::{
    OptionSymbol, OrderDuration, OrderInstruction, OrderRequest, OrderRequestLeg, OrderSession,
    OrderType, PriceLinkBasis, PriceLinkType,
};
use rustyschwab_types::{Decimal, ValidationErrorKind};
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
    assert!(
        err.to_string().contains(needle),
        "{err:?} does not mention {needle:?}"
    );
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

    // A complex order's quantity is its unit count: a 1x2x1 butterfly of 5
    // has legs of 5, 10 and 5, each a whole multiple of it.
    let leg = |instruction, strike: &str, n: u32| {
        OrderRequestLeg::option(instruction, format!("XYZ   240315C00{strike}000"), n)
    };
    let mut fly = OrderRequest::net_debit(
        vec![
            leg(OrderInstruction::BuyToOpen, "045", 5),
            leg(OrderInstruction::SellToOpen, "050", 10),
            leg(OrderInstruction::BuyToOpen, "055", 5),
        ],
        d("0.50"),
    );
    fly.quantity = Some(d("5"));
    fly.validate().unwrap();
    fly.quantity = Some(d("3"));
    rejects(&fly, "whole multiple");
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

/// Schwab answers 400 to an equity order with any duration but DAY,
/// GOOD_TILL_CANCEL or FILL_OR_KILL; an option order is not judged.
#[test]
fn equity_orders_take_the_durations_schwab_accepts() {
    let equity = OrderRequest::limit(buy(1), d("150"));
    for duration in [
        OrderDuration::Day,
        OrderDuration::Gtc,
        OrderDuration::FillOrKill,
    ] {
        equity.clone().with_duration(duration).validate().unwrap();
    }
    for duration in [
        OrderDuration::ImmediateOrCancel,
        OrderDuration::EndOfWeek,
        OrderDuration::EndOfMonth,
        OrderDuration::NextEndOfMonth,
    ] {
        let err = equity
            .clone()
            .with_duration(duration)
            .validate()
            .unwrap_err();
        assert_eq!(err.kind(), ValidationErrorKind::Structure);
        assert_eq!(err.field(), Some("duration"));
    }
    OrderRequest::limit(
        OrderRequestLeg::option(OrderInstruction::BuyToOpen, "AAPL  240517P00190000", 1),
        d("1.50"),
    )
    .with_duration(OrderDuration::ImmediateOrCancel)
    .validate()
    .unwrap();
}

#[test]
fn instructions_symbols_and_quantities_fit_the_asset() {
    let equity_with_option_instruction = OrderRequest::market(OrderRequestLeg::equity(
        OrderInstruction::BuyToOpen,
        "AAPL",
        1,
    ));
    let wrong = equity_with_option_instruction.validate().unwrap_err();
    assert_eq!(wrong.kind(), ValidationErrorKind::NotAllowed);
    assert_eq!(wrong.field(), Some("orderLegCollection[0].instruction"));

    let option_with_equity_instruction = OrderRequest::market(OrderRequestLeg::option(
        OrderInstruction::Buy,
        "AAPL  240517P00190000",
        1,
    ));
    let wrong = option_with_equity_instruction.validate().unwrap_err();
    assert_eq!(wrong.field(), Some("orderLegCollection[0].instruction"));
    assert!(wrong.to_string().contains("BUY_TO_OPEN"), "{wrong}");

    // Asset types this SDK does not submit are refused, not sent unchecked.
    let mut index = OrderRequestLeg::equity(OrderInstruction::Buy, "$SPX", 1);
    index.instrument.asset_type = rustyschwab_types::common::AssetType::Index;
    let refused = OrderRequest::market(index).validate().unwrap_err();
    assert_eq!(refused.kind(), ValidationErrorKind::Unsupported);
    assert_eq!(
        refused.field(),
        Some("orderLegCollection[0].instrument.assetType")
    );

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

/// Schwab's order examples always send `quantity` as a JSON number, and
/// prices as strings or numbers: quantities go out as numbers, exactly.
#[test]
fn quantities_are_sent_as_exact_json_numbers() {
    let mut order = OrderRequest::limit(buy(10), d("150.25"));
    order.quantity = Some(d("10"));
    let wire = serde_json::to_value(&order).unwrap();
    assert_eq!(wire["quantity"], json!(10));
    assert_eq!(wire["orderLegCollection"][0]["quantity"], json!(10));
    assert_eq!(wire["price"], json!("150.25"));
    // Fund dollar amounts are fractional; they survive the float spelling.
    let mut fund = buy(1);
    fund.quantity = d("1000.05");
    assert_eq!(
        serde_json::to_value(&fund).unwrap()["quantity"].to_string(),
        "1000.05"
    );
    // More digits than a float carries is refused, never rounded.
    fund.quantity = d("0.1234567890123456789");
    assert!(serde_json::to_value(&fund).is_err());
    // Either spelling reads back to the same order.
    assert_eq!(parse(wire).unwrap(), order);
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

fn option_leg(instruction: &str, symbol: &str, quantity: u32) -> serde_json::Value {
    json!({"quantity": quantity, "instruction": instruction,
        "instrument": {"symbol": symbol, "assetType": "OPTION"}})
}

/// Rules over the legs together: what Schwab answers 400 for, refused with
/// the field that is wrong.
#[test]
fn legs_are_judged_together() {
    let call = |strike: &str| format!("AAPL  301218C00{strike}000");
    let spread = |order_type: &str, legs: Vec<serde_json::Value>, strategy: &str| {
        parse(json!({
            "orderStrategyType": "SINGLE", "orderType": order_type, "session": "NORMAL",
            "duration": "DAY", "price": "1.00", "complexOrderStrategyType": strategy,
            "orderLegCollection": legs
        }))
        .unwrap()
    };
    let vertical = vec![
        option_leg("BUY_TO_OPEN", &call("150"), 1),
        option_leg("SELL_TO_OPEN", &call("160"), 1),
    ];
    spread("NET_DEBIT", vertical.clone(), "VERTICAL")
        .validate()
        .unwrap();
    // Several legs are priced as one.
    rejects(
        &spread("LIMIT", vertical.clone(), "VERTICAL"),
        "priced together",
    );
    // A named strategy has its number of legs, on one underlying.
    let mut three = vertical.clone();
    three.push(option_leg("SELL_TO_OPEN", &call("170"), 1));
    rejects(&spread("NET_DEBIT", three, "VERTICAL"), "takes 2 legs");
    let mixed = vec![
        option_leg("BUY_TO_OPEN", &call("150"), 1),
        option_leg("SELL_TO_OPEN", "MSFT  301218C00400000", 1),
    ];
    rejects(&spread("NET_DEBIT", mixed, "VERTICAL"), "one underlying");
    // No leg repeats another.
    let repeated = vec![
        option_leg("BUY_TO_OPEN", &call("150"), 1),
        option_leg("BUY_TO_OPEN", &call("150"), 1),
    ];
    let err = spread("NET_DEBIT", repeated, "CUSTOM")
        .validate()
        .unwrap_err();
    assert_eq!(err.field(), Some("orderLegCollection[1]"));
    // Options trade only in the regular session.
    let single_option = parse(json!({
        "orderStrategyType": "SINGLE", "orderType": "LIMIT", "session": "AM",
        "duration": "DAY", "price": "1.00",
        "orderLegCollection": [option_leg("BUY_TO_OPEN", &call("150"), 1)]
    }))
    .unwrap();
    rejects(&single_option, "NORMAL session");
}

/// A trailing offset in ticks is a whole number of ticks.
#[test]
fn a_tick_offset_is_whole() {
    let order = parse(json!({
        "orderStrategyType": "SINGLE", "orderType": "TRAILING_STOP", "session": "NORMAL",
        "duration": "DAY", "stopPriceLinkBasis": "LAST", "stopPriceLinkType": "TICK",
        "stopPriceOffset": "2.5",
        "orderLegCollection": [{"quantity": 1, "instruction": "SELL",
            "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}}]
    }))
    .unwrap();
    let err = order.validate().unwrap_err();
    assert_eq!(err.kind(), ValidationErrorKind::NotWhole);
    assert_eq!(err.field(), Some("stopPriceOffset"));
}

/// A stop entry's trigger is the price a bracket's exits must straddle.
#[test]
fn a_stop_entry_bracket_straddles_its_trigger() {
    let entry = OrderRequest::stop(buy(1), d("155"));
    assert!(entry.clone().bracket(d("160"), d("158")).is_err());
    entry.bracket(d("160"), d("150")).unwrap();
}

/// A reported order becomes the request that would place it again, to
/// edit for a replacement: Schwab's bookkeeping and the tag are left out.
#[test]
fn a_reported_order_becomes_its_request() {
    let order: rustyschwab_types::trading::Order = serde_json::from_value(json!({
        "orderId": 77, "status": "WORKING", "tag": "rs-original",
        "session": "NORMAL", "duration": "DAY", "orderType": "LIMIT",
        "orderStrategyType": "SINGLE", "price": 150.25, "quantity": 10.0,
        "filledQuantity": 0.0, "remainingQuantity": 10.0,
        "releaseTime": "2030-01-02T14:30:00+0000",
        "orderLegCollection": [{"orderLegType": "EQUITY", "legId": 1,
            "instruction": "BUY", "quantity": 10.0,
            "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}}]
    }))
    .unwrap();
    let mut request = OrderRequest::try_from(&order).unwrap();
    assert_eq!(request.price, Some(d("150.25")));
    assert_eq!(request.order_leg_collection[0].quantity, d("10"));
    assert!(request.tag.is_none());
    assert!(request.release_time.is_some());
    request.validate().unwrap();
    request.price = Some(d("149.50"));
    let wire = serde_json::to_value(&request).unwrap();
    assert_eq!(wire["price"], "149.50");
    assert_eq!(wire["releaseTime"], "2030-01-02T14:30:00.000Z");
}
