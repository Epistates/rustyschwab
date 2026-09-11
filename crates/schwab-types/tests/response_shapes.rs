//! Deserialization tests against realistic Schwab wire payloads.
//!
//! These fixtures mirror the shapes documented by Schwab and observed from
//! the live API (symbol-keyed quote maps, nested market hours, `Location`
//! based order submission, etc.). They exist so a type change that breaks a
//! wire shape fails in CI rather than on the first production call.

use schwab_types::accounts::{Account, UserPreferences};
use schwab_types::market_data::{
    InstrumentsResponse, MarketsHoursResponse, MoversResponse, QuotesResponse,
};
use schwab_types::trading::Transaction;

#[test]
fn quotes_response_is_symbol_keyed_map() {
    let json = r#"{
        "AAPL": {
            "assetMainType": "EQUITY",
            "assetSubType": "COE",
            "quoteType": "NBBO",
            "realtime": true,
            "ssid": 1973757747,
            "symbol": "AAPL",
            "quote": {
                "52WeekHigh": 237.23,
                "52WeekLow": 164.08,
                "askPrice": 228.16,
                "askSize": 2,
                "bidPrice": 228.14,
                "bidSize": 4,
                "lastPrice": 228.15,
                "totalVolume": 38861966,
                "netChange": -0.65,
                "securityStatus": "Normal"
            },
            "reference": {
                "cusip": "037833100",
                "description": "Apple Inc",
                "exchange": "Q",
                "exchangeName": "NASDAQ"
            },
            "regular": {
                "regularMarketLastPrice": 228.15,
                "regularMarketNetChange": -0.65
            }
        },
        "TSLA": {
            "assetMainType": "EQUITY",
            "symbol": "TSLA",
            "quote": { "lastPrice": 339.64 }
        }
    }"#;

    let parsed: QuotesResponse = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.quotes.len(), 2);
    let aapl = &parsed.quotes["AAPL"];
    assert_eq!(aapl.symbol.as_deref(), Some("AAPL"));
    assert_eq!(aapl.asset_main_type.as_deref(), Some("EQUITY"));
    assert_eq!(aapl.quote.as_ref().unwrap().last_price, Some(228.15));
    assert_eq!(
        aapl.quote.as_ref().unwrap().fifty_two_week_high,
        Some(237.23)
    );
    assert!(parsed.errors.is_none());
}

#[test]
fn quotes_response_carries_invalid_symbol_errors() {
    let json = r#"{
        "AAPL": { "symbol": "AAPL", "quote": { "lastPrice": 228.15 } },
        "errors": { "invalidSymbols": ["BADSYM"] }
    }"#;

    let parsed: QuotesResponse = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.quotes.len(), 1);
    assert_eq!(
        parsed.errors.unwrap().invalid_symbols.unwrap(),
        vec!["BADSYM".to_string()]
    );
}

#[test]
fn instruments_response_is_wrapped_in_instruments_key() {
    let json = r#"{
        "instruments": [
            {
                "cusip": "037833100",
                "symbol": "AAPL",
                "description": "Apple Inc",
                "exchange": "NASDAQ",
                "assetType": "EQUITY"
            }
        ]
    }"#;

    let parsed: InstrumentsResponse = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.instruments.len(), 1);
    assert_eq!(parsed.instruments[0].symbol, "AAPL");
}

#[test]
fn market_hours_are_nested_two_levels() {
    let json = r#"{
        "equity": {
            "EQ": {
                "date": "2025-01-15",
                "marketType": "EQUITY",
                "product": "EQ",
                "isOpen": true,
                "sessionHours": {
                    "preMarket": [{
                        "start": "2025-01-15T07:00:00-05:00",
                        "end": "2025-01-15T09:30:00-05:00"
                    }],
                    "regularMarket": [{
                        "start": "2025-01-15T09:30:00-05:00",
                        "end": "2025-01-15T16:00:00-05:00"
                    }]
                }
            }
        },
        "option": {
            "EQO": {
                "date": "2025-01-15",
                "product": "EQO",
                "isOpen": true
            }
        }
    }"#;

    let parsed: MarketsHoursResponse = serde_json::from_str(json).unwrap();
    let eq = &parsed["equity"]["EQ"];
    assert_eq!(eq.is_open, Some(true));
    assert!(
        eq.session_hours
            .as_ref()
            .unwrap()
            .regular_market
            .as_ref()
            .unwrap()[0]
            .start
            .is_some()
    );
    assert_eq!(parsed["option"]["EQO"].product.as_deref(), Some("EQO"));
}

#[test]
fn movers_response_uses_screeners_key() {
    let json = r#"{
        "screeners": [
            {
                "symbol": "NVDA",
                "description": "NVIDIA CORP",
                "direction": "up",
                "change": 4.21,
                "last": 140.11,
                "totalVolume": 198346722
            }
        ]
    }"#;

    let parsed: MoversResponse = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.screeners[0].symbol, "NVDA");
}

#[test]
fn transaction_tolerates_missing_user_identity_fields() {
    let json = r#"{
        "activityId": 95512265692,
        "time": "2025-01-15T14:31:22+0000",
        "type": "TRADE",
        "status": "VALID",
        "accountNumber": "12345678",
        "netAmount": -1500.50,
        "user": { "userId": 1234567 },
        "instrument": { "symbol": "AAPL", "assetType": "EQUITY" }
    }"#;

    let parsed: Transaction = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.activity_id, Some(95512265692));
    let user = parsed.user.unwrap();
    assert_eq!(user.user_id, Some(1234567));
    assert!(user.login.is_none());
}

#[test]
fn accounts_response_is_array_of_securities_accounts() {
    let json = r#"[
        {
            "securitiesAccount": {
                "accountNumber": "12345678",
                "type": "MARGIN",
                "roundTrips": 0,
                "isDayTrader": false,
                "currentBalances": { "liquidationValue": 100000.0, "buyingPower": 50000.0 }
            }
        }
    ]"#;

    let parsed: Vec<Account> = serde_json::from_str(json).unwrap();
    assert_eq!(parsed[0].securities_account.account_number, "12345678");
}

#[test]
fn user_preferences_carry_streamer_info() {
    let json = r#"{
        "accounts": [
            {
                "accountNumber": "12345678",
                "primaryAccount": true,
                "type": "BROKERAGE",
                "nickName": "Individual",
                "accountColor": "Green",
                "displayAcctId": "...678",
                "autoPositionEffect": false
            }
        ],
        "streamerInfo": [
            {
                "streamerSocketUrl": "wss://streamer-api.schwab.com/ws",
                "schwabClientCustomerId": "CUSTOMER_ID",
                "schwabClientCorrelId": "CORREL_ID",
                "schwabClientChannel": "N9",
                "schwabClientFunctionId": "APIAPP"
            }
        ],
        "offers": [{ "level2Permissions": true, "mktDataPermission": "realtime" }]
    }"#;

    let parsed: UserPreferences = serde_json::from_str(json).unwrap();
    let streamer = &parsed.streamer_info.unwrap()[0];
    assert_eq!(streamer.schwab_client_channel, "N9");
    assert!(streamer.streamer_socket_url.starts_with("wss://"));
}

#[test]
fn protective_order_tree_survives_serialization_and_unknown_inputs_fail() {
    use schwab_types::trading::OrderRequest;
    let leg = serde_json::json!({"instruction":"SELL","quantity":"1","instrument":{"assetType":"EQUITY","symbol":"AAPL"}});
    let stop = serde_json::json!({"session":"NORMAL","duration":"GOOD_TILL_CANCEL","orderType":"STOP_LIMIT","price":"9.99","stopPrice":"10.00","orderStrategyType":"SINGLE","orderLegCollection":[leg.clone()]});
    let limit = serde_json::json!({"session":"NORMAL","duration":"GOOD_TILL_CANCEL","orderType":"LIMIT","price":"12.34","orderStrategyType":"SINGLE","orderLegCollection":[leg]});
    let value = serde_json::json!({"orderStrategyType":"OCO","childOrderStrategies":[limit,stop]});
    let order: OrderRequest = serde_json::from_value(value.clone()).unwrap();
    order.validate().unwrap();
    order
        .validate_price_increment("0.01".parse().unwrap())
        .unwrap();
    assert_eq!(serde_json::to_value(&order).unwrap(), value);
    assert!(
        order
            .validate_price_increment("0.05".parse().unwrap())
            .is_err()
    );
    let mut malformed = value;
    malformed["mysteryExecutionFlag"] = true.into();
    assert!(serde_json::from_value::<OrderRequest>(malformed).is_err());
}

#[test]
fn incomplete_stops_and_invalid_quantities_are_rejected_before_transport() {
    use schwab_types::trading::OrderRequest;
    for value in [
        serde_json::json!({"orderStrategyType":"SINGLE"}),
        serde_json::json!({"session":"NORMAL","duration":"DAY","orderType":"STOP","orderStrategyType":"SINGLE","orderLegCollection":[{"instruction":"SELL","quantity":"1","instrument":{"assetType":"EQUITY","symbol":"AAPL"}}]}),
        serde_json::json!({"session":"NORMAL","duration":"DAY","orderType":"LIMIT","price":"1","orderStrategyType":"SINGLE","orderLegCollection":[{"instruction":"BUY","quantity":"-1","instrument":{"assetType":"EQUITY","symbol":"AAPL"}}]}),
    ] {
        assert!(
            serde_json::from_value::<OrderRequest>(value)
                .unwrap()
                .validate()
                .is_err()
        );
    }
}

#[test]
fn triggered_trailing_stop_and_partial_fill_activity_are_preserved() {
    use schwab_types::trading::{Order, OrderRequest};
    let leg = serde_json::json!({"instruction":"SELL","quantity":"1","instrument":{"assetType":"EQUITY","symbol":"AAPL"}});
    let trailing = serde_json::json!({"session":"NORMAL","duration":"GOOD_TILL_CANCEL","orderType":"TRAILING_STOP","stopPriceLinkBasis":"BID","stopPriceLinkType":"VALUE","stopPriceOffset":"0.25","orderStrategyType":"SINGLE","orderLegCollection":[leg]});
    let value = serde_json::json!({"session":"NORMAL","duration":"DAY","orderType":"LIMIT","price":"10.00","orderStrategyType":"TRIGGER","orderLegCollection":[{"instruction":"BUY","quantity":"1","instrument":{"assetType":"EQUITY","symbol":"AAPL"}}],"childOrderStrategies":[trailing]});
    let request: OrderRequest = serde_json::from_value(value.clone()).unwrap();
    request.validate().unwrap();
    assert_eq!(serde_json::to_value(request).unwrap(), value);
    let activity = serde_json::json!([{"activityType":"EXECUTION","executionType":"FILL","quantity":0.5,"executionLegs":[{"legId":1,"quantity":0.5,"price":10.0}]}]);
    let order: Order = serde_json::from_value(serde_json::json!({"orderId":123,"orderStrategyType":"SINGLE","status":"NEW_BROKER_STATE","orderActivityCollection":activity})).unwrap();
    assert_eq!(
        serde_json::to_value(order).unwrap()["orderActivityCollection"],
        activity
    );
}

#[test]
fn order_enums_use_the_trader_api_spellings() {
    use schwab_types::trading::{
        ComplexOrderStrategyType, Order, OrderRequest, OrderStatus, OrderStrategyType, OrderType,
        PriceLinkBasis, RequestedDestination, SpecialInstruction,
    };
    fn wire<T: serde::Serialize>(value: T) -> serde_json::Value {
        serde_json::to_value(value).unwrap()
    }
    assert_eq!(wire(OrderType::Cabinet), "CABINET");
    assert_eq!(
        wire(ComplexOrderStrategyType::DoubleDiagonal),
        "DOUBLE_DIAGONAL"
    );
    assert_eq!(
        wire(ComplexOrderStrategyType::UnbalancedButterfly),
        "UNBALANCED_BUTTERFLY"
    );
    assert_eq!(
        wire(ComplexOrderStrategyType::UnbalancedCondor),
        "UNBALANCED_CONDOR"
    );
    assert_eq!(wire(PriceLinkBasis::AskBid), "ASK_BID");
    assert_eq!(wire(RequestedDestination::C2), "C2");
    assert_eq!(wire(RequestedDestination::EcnArca), "ECN_ARCA");
    assert_eq!(
        wire(SpecialInstruction::AllOrNoneDoNotReduce),
        "ALL_OR_NONE_DO_NOT_REDUCE"
    );
    assert_eq!(
        wire(OrderStatus::AwaitingStopCondition),
        "AWAITING_STOP_CONDITION"
    );
    // Schwab reports strategy types that cannot be submitted.
    let reported: Order = serde_json::from_value(
        serde_json::json!({"orderId":1,"orderStrategyType":"PAIR","status":"AWAITING_PARENT_ORDER"}),
    )
    .unwrap();
    assert_eq!(reported.order_strategy_type, Some(OrderStrategyType::Pair));
    assert_eq!(reported.status, Some(OrderStatus::AwaitingParentOrder));
    let request: OrderRequest = serde_json::from_value(serde_json::json!({
        "session":"NORMAL","duration":"DAY","orderType":"MARKET","orderStrategyType":"PAIR",
        "orderLegCollection":[{"instruction":"BUY","quantity":"1","instrument":{"assetType":"EQUITY","symbol":"AAPL"}}]
    }))
    .unwrap();
    assert!(request.validate().is_err());
    // Request-only fields are typed; a misspelled destination is rejected
    // before it reaches the broker.
    assert!(
        serde_json::from_value::<OrderRequest>(serde_json::json!({
            "session":"NORMAL","duration":"DAY","orderType":"MARKET","orderStrategyType":"SINGLE",
            "requestedDestination":"NYSE_ARCA",
            "orderLegCollection":[{"instruction":"BUY","quantity":"1","instrument":{"assetType":"EQUITY","symbol":"AAPL"}}]
        }))
        .is_err()
    );
}
