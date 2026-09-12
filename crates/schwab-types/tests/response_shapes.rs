//! Deserialization tests against realistic Schwab wire payloads.
//!
//! These fixtures mirror the shapes documented by Schwab and observed from
//! the live API (symbol-keyed quote maps, nested market hours, `Location`
//! based order submission, etc.). They exist so a type change that breaks a
//! wire shape fails in CI rather than on the first production call.

use schwab_types::accounts::{Account, UserPreferences};
use schwab_types::common::{AssetMainType, AssetSubType};
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
    assert_eq!(aapl.asset_main_type, Some(AssetMainType::Equity));
    assert_eq!(aapl.asset_sub_type, Some(AssetSubType::Coe));
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
    assert_eq!(
        parsed.instruments[0].asset_type,
        Some(AssetMainType::Equity)
    );
}

/// Quote fundamentals as Schwab sends them: share counts as JSON floats and
/// dates as ISO-8601 strings. An integer-typed volume field rejects
/// `37498265.0` outright, which is what this pins against.
#[test]
fn quote_fundamentals_decode_float_volumes_and_string_dates() {
    let json = r#"{
        "AAPL": {
            "symbol": "AAPL",
            "fundamental": {
                "avg10DaysVolume": 37498265.0,
                "avg1YearVolume": 58820585.0,
                "declarationDate": "2024-08-01T04:00:00Z",
                "divAmount": 1.0,
                "divExDate": "2024-08-12T04:00:00Z",
                "divFreq": 4,
                "divPayAmount": 0.25,
                "divPayDate": "2024-08-15T04:00:00Z",
                "divYield": 0.43144,
                "eps": 6.13,
                "fundLeverageFactor": 0.0,
                "lastEarningsDate": "2024-08-01T04:00:00Z",
                "nextDivExDate": "2024-11-12T05:00:00Z",
                "nextDivPayDate": "2024-11-15T05:00:00Z",
                "peRatio": 35.35277
            }
        }
    }"#;

    let parsed: QuotesResponse = serde_json::from_str(json).unwrap();
    let fundamental = parsed.quotes["AAPL"].fundamental.as_ref().unwrap();
    assert_eq!(fundamental.avg10_days_volume, Some(37_498_265.0));
    assert_eq!(fundamental.div_freq, Some(4));
    assert_eq!(
        fundamental.next_div_pay_date.as_deref(),
        Some("2024-11-15T05:00:00Z")
    );
    assert!(fundamental.extra.is_empty(), "{:?}", fundamental.extra);
}

/// `projection=fundamental` uses Schwab's other date format and upper-case
/// `TTM`/`MRQ` suffixes that a plain `camelCase` rename would miss.
#[test]
fn instrument_fundamentals_decode_the_documented_projection() {
    let json = r#"{
        "instruments": [{
            "fundamental": {
                "symbol": "AAPL",
                "high52": 237.49,
                "low52": 164.075,
                "dividendAmount": 1.0,
                "dividendYield": 0.43144,
                "dividendDate": "2024-08-12 00:00:00.0",
                "peRatio": 35.35277,
                "pegRatio": 116.555,
                "pbRatio": 48.06189,
                "prRatio": 8.43929,
                "pcfRatio": 23.01545,
                "grossMarginTTM": 45.962,
                "grossMarginMRQ": 46.2571,
                "netProfitMarginTTM": 26.4406,
                "netProfitMarginMRQ": 25.0043,
                "operatingMarginTTM": 26.4406,
                "operatingMarginMRQ": 25.0043,
                "returnOnEquity": 160.5833,
                "returnOnAssets": 22.6119,
                "returnOnInvestment": 50.98106,
                "quickRatio": 0.79752,
                "currentRatio": 0.95298,
                "interestCoverage": 0.0,
                "totalDebtToCapital": 51.3034,
                "ltDebtToEquity": 151.8618,
                "totalDebtToEquity": 129.2138,
                "epsTTM": 6.56667,
                "epsChangePercentTTM": 10.3155,
                "epsChangeYear": 0.0,
                "epsChange": 0.0,
                "revChangeYear": -2.8005,
                "revChangeTTM": 0.4349,
                "revChangeIn": 0.0,
                "sharesOutstanding": 15204137000.0,
                "marketCapFloat": 0.0,
                "marketCap": 3524014873860.0,
                "bookValuePerShare": 4.38227,
                "shortIntToFloat": 0.0,
                "shortIntDayToCover": 0.0,
                "divGrowthRate3Year": 0.0,
                "dividendPayAmount": 0.25,
                "dividendPayDate": "2024-08-15 00:00:00.0",
                "beta": 1.23919,
                "vol1DayAvg": 0.0,
                "vol10DayAvg": 0.0,
                "vol3MonthAvg": 0.0,
                "avg10DaysVolume": 37498265,
                "avg1DayVolume": 58820585,
                "avg3MonthVolume": 51544618,
                "declarationDate": "2024-08-01 00:00:00.0",
                "dividendFreq": 4,
                "eps": 6.13,
                "dtnVolume": 34065076,
                "nextDividendPayDate": "2024-11-15 00:00:00.0",
                "nextDividendDate": "2024-11-12 00:00:00.0",
                "fundLeverageFactor": 0.0
            },
            "cusip": "037833100",
            "symbol": "AAPL",
            "description": "APPLE INC",
            "exchange": "NASDAQ",
            "assetType": "EQUITY"
        }]
    }"#;

    let parsed: InstrumentsResponse = serde_json::from_str(json).unwrap();
    let fundamental = parsed.instruments[0].fundamental.as_ref().unwrap();
    assert_eq!(fundamental.gross_margin_ttm, Some(45.962));
    assert_eq!(fundamental.eps_ttm, Some(6.56667));
    assert_eq!(fundamental.avg10_days_volume, Some(37_498_265.0));
    assert_eq!(
        fundamental.dividend_date.as_deref(),
        Some("2024-08-12 00:00:00.0")
    );
    assert!(fundamental.extra.is_empty(), "{:?}", fundamental.extra);
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
        "user": { "userId": 1234567 }
    }"#;

    let parsed: Transaction = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.activity_id, Some(95512265692));
    let user = parsed.user.unwrap();
    assert_eq!(user.user_id, Some(1234567));
    assert!(user.login.is_none());
}

/// Schwab's documented `transactions` payload: the security leg and each
/// fee are separate `transferItems`, and there is no top-level instrument.
#[test]
fn transactions_decode_transfer_items_with_typed_fees_and_instruments() {
    use schwab_types::common::AssetType;
    use schwab_types::trading::{
        FeeType, PositionEffect, SubAccount, TransactionActivityType, TransactionStatus,
        TransactionType,
    };
    let json = r#"[{
        "activityId": 95512265692,
        "time": "2025-01-15T14:31:22+0000",
        "accountNumber": "12345678",
        "type": "TRADE",
        "status": "VALID",
        "subAccount": "MARGIN",
        "tradeDate": "2025-01-15T14:31:22+0000",
        "positionId": 2700000001,
        "orderId": 1000000000001,
        "netAmount": -1500.5,
        "activityType": "EXECUTION",
        "transferItems": [
            {
                "instrument": {
                    "assetType": "CURRENCY",
                    "status": "ACTIVE",
                    "symbol": "CURRENCY_USD",
                    "description": "USD currency",
                    "instrumentId": 1,
                    "closingPrice": 0.0
                },
                "amount": 0.0,
                "cost": 0.0,
                "feeType": "COMMISSION"
            },
            {
                "instrument": {
                    "assetType": "CURRENCY",
                    "status": "ACTIVE",
                    "symbol": "CURRENCY_USD",
                    "description": "USD currency",
                    "instrumentId": 1,
                    "closingPrice": 0.0
                },
                "amount": 0.0,
                "cost": 0.0,
                "feeType": "TAF_FEE"
            },
            {
                "instrument": {
                    "assetType": "EQUITY",
                    "status": "ACTIVE",
                    "symbol": "NET",
                    "instrumentId": 12345678,
                    "closingPrice": 150.05,
                    "type": "COMMON_STOCK"
                },
                "amount": 10.0,
                "cost": -1500.5,
                "price": 150.05,
                "positionEffect": "OPENING"
            }
        ]
    }]"#;
    let parsed: Vec<Transaction> = serde_json::from_str(json).unwrap();
    let trade = &parsed[0];
    assert_eq!(trade.type_, Some(TransactionType::Trade));
    assert_eq!(trade.status, Some(TransactionStatus::Valid));
    assert_eq!(trade.sub_account, Some(SubAccount::Margin));
    assert_eq!(
        trade.activity_type,
        Some(TransactionActivityType::Execution)
    );
    assert_eq!(trade.order_id.as_deref(), Some("1000000000001"));
    assert!(trade.extra.is_empty(), "{:?}", trade.extra);
    assert_eq!(trade.transfer_items.len(), 3);
    let fees: Vec<FeeType> = trade
        .transfer_items
        .iter()
        .filter_map(|item| item.fee_type)
        .collect();
    assert_eq!(fees, [FeeType::Commission, FeeType::TafFee]);
    let leg = &trade.transfer_items[2];
    assert_eq!(leg.position_effect, Some(PositionEffect::Opening));
    assert_eq!(leg.amount, Some(10.0));
    assert!(leg.extra.is_empty(), "{:?}", leg.extra);
    let instrument = leg.instrument.as_ref().unwrap();
    assert_eq!(instrument.asset_type, AssetType::Equity);
    assert_eq!(instrument.type_.as_deref(), Some("COMMON_STOCK"));
    assert_eq!(instrument.status.as_deref(), Some("ACTIVE"));
    assert!(instrument.extra.is_empty(), "{:?}", instrument.extra);
    // Values outside the documented sets degrade instead of failing.
    let odd: Transaction = serde_json::from_value(serde_json::json!({
        "activityId": 1, "type": "FUTURE_TYPE", "status": "UNKNOWN", "subAccount": "UNKNOWN",
        "transferItems": [{"feeType": "UNKNOWN", "positionEffect": "UNKNOWN",
            "instrument": {"assetType": "PRODUCT", "symbol": "?", "putCall": "UNKNOWN"}}]
    }))
    .unwrap();
    assert_eq!(odd.type_, None);
    assert_eq!(odd.status, Some(TransactionStatus::Unknown));
    assert_eq!(odd.sub_account, Some(SubAccount::Unknown));
    assert_eq!(odd.transfer_items[0].fee_type, Some(FeeType::Unknown));
    assert_eq!(odd.transfer_items[0].position_effect, None);
    let instrument = odd.transfer_items[0].instrument.as_ref().unwrap();
    assert_eq!(instrument.asset_type, AssetType::Product);
    assert_eq!(instrument.put_call, None);
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
    let leg = serde_json::json!({
        "instruction":"BUY","quantity":"1","instrument":{"assetType":"EQUITY","symbol":"AAPL"}
    });
    let request: OrderRequest = serde_json::from_value(serde_json::json!({
        "session":"NORMAL","duration":"DAY","orderType":"MARKET","orderStrategyType":"PAIR",
        "orderLegCollection":[leg]
    }))
    .unwrap();
    assert!(request.validate().is_err());
    // Request-only fields are typed; a misspelled destination is rejected
    // before it reaches the broker.
    assert!(
        serde_json::from_value::<OrderRequest>(serde_json::json!({
            "session":"NORMAL","duration":"DAY","orderType":"MARKET","orderStrategyType":"SINGLE",
            "requestedDestination":"NYSE_ARCA",
            "orderLegCollection":[leg]
        }))
        .is_err()
    );
    // A misspelled asset type is rejected the same way.
    assert!(
        serde_json::from_value::<OrderRequest>(serde_json::json!({
            "session":"NORMAL","duration":"DAY","orderType":"MARKET","orderStrategyType":"SINGLE",
            "orderLegCollection":[{"instruction":"BUY","quantity":"1",
                "instrument":{"assetType":"STOCK","symbol":"AAPL"}}]
        }))
        .is_err()
    );
}

#[test]
fn order_details_decode_the_documented_payload_fully_typed() {
    use schwab_types::common::AssetType;
    use schwab_types::trading::{
        Order, OrderActivityType, OrderLegType, OrderStatus, PositionEffect, RequestedDestination,
    };
    // Schwab's documented `order_details` example, with the placeholder
    // digits replaced by plausible values.
    let json = r#"{
        "session":"NORMAL",
        "duration":"DAY",
        "orderType":"LIMIT",
        "complexOrderStrategyType":"NONE",
        "quantity":1.0,
        "filledQuantity":0.0,
        "remainingQuantity":0.0,
        "requestedDestination":"AUTO",
        "destinationLinkName":"AutoRoute",
        "price":10.0,
        "orderLegCollection":[
            {
                "orderLegType":"EQUITY",
                "legId":1,
                "instrument":{
                    "assetType":"EQUITY",
                    "cusip":"458140100",
                    "symbol":"INTC",
                    "instrumentId":1234567
                },
                "instruction":"BUY",
                "positionEffect":"OPENING",
                "quantity":1.0
            }
        ],
        "orderStrategyType":"SINGLE",
        "orderId":1000000000001,
        "cancelable":false,
        "editable":false,
        "status":"CANCELED",
        "enteredTime":"2025-01-15T14:31:22+0000",
        "closeTime":"2025-01-15T14:35:00+0000",
        "tag":"API_TRADER",
        "accountNumber":12345678,
        "orderActivityCollection":[
            {
                "activityType":"EXECUTION",
                "activityId":900000000001,
                "executionType":"CANCELED",
                "quantity":1.0,
                "orderRemainingQuantity":0.0,
                "executionLegs":[
                    {
                        "legId":1,
                        "quantity":1.0,
                        "mismarkedQuantity":0.0,
                        "price":0.0,
                        "time":"2025-01-15T14:35:00+0000",
                        "instrumentId":1234567
                    }
                ]
            }
        ]
    }"#;
    let order: Order = serde_json::from_str(json).unwrap();
    assert_eq!(order.order_id.as_deref(), Some("1000000000001"));
    assert_eq!(order.account_number.as_deref(), Some("12345678"));
    assert_eq!(
        order.requested_destination,
        Some(RequestedDestination::Auto)
    );
    assert_eq!(order.status, Some(OrderStatus::Canceled));
    assert_eq!(order.tag.as_deref(), Some("API_TRADER"));
    assert!(order.extra.is_empty(), "{:?}", order.extra);
    let leg = &order.order_leg_collection[0];
    assert_eq!(leg.order_leg_type, OrderLegType::Equity);
    assert_eq!(leg.position_effect, Some(PositionEffect::Opening));
    assert_eq!(leg.instrument.asset_type, AssetType::Equity);
    assert_eq!(leg.instrument.instrument_id, Some(1234567));
    assert!(
        leg.instrument.extra.is_empty(),
        "{:?}",
        leg.instrument.extra
    );
    let activity = &order.order_activity_collection[0];
    assert_eq!(activity.activity_type, Some(OrderActivityType::Execution));
    assert_eq!(activity.execution_type.as_deref(), Some("CANCELED"));
    // Beyond the spec, so it stays in `extra` rather than failing.
    assert_eq!(activity.extra["activityId"], 900000000001_i64);
    let fill = &activity.execution_legs[0];
    assert_eq!(fill.leg_id, Some(1));
    assert_eq!(fill.time.as_deref(), Some("2025-01-15T14:35:00+0000"));
    // Serialized orders carry the same names Schwab uses.
    let wire = serde_json::to_value(&order).unwrap();
    assert_eq!(wire["requestedDestination"], "AUTO");
    assert_eq!(
        wire["orderActivityCollection"][0]["activityId"],
        900000000001_i64
    );
    assert!(wire["orderLegCollection"][0].get("quantityType").is_none());
}

#[test]
fn position_instruments_type_the_option_extension_and_tolerate_unknown() {
    use schwab_types::accounts::SecuritiesAccount;
    use schwab_types::common::{AssetType, PutCall};
    let json = r#"{
        "accountNumber": "12345678",
        "type": "MARGIN",
        "positions": [
            {
                "longQuantity": 2.0,
                "instrument": {
                    "assetType": "OPTION",
                    "cusip": "0AAPL.EH50190000",
                    "symbol": "AAPL  250815C00190000",
                    "description": "APPLE INC 08/15/2025 $190 Call",
                    "instrumentId": 98765432,
                    "netChange": 0.35,
                    "type": "VANILLA",
                    "putCall": "CALL",
                    "underlyingSymbol": "AAPL",
                    "optionMultiplier": 100
                }
            },
            {
                "longQuantity": 100.0,
                "instrument": {
                    "assetType": "CASH_EQUIVALENT",
                    "symbol": "SWVXX",
                    "type": "MONEY_MARKET_FUND",
                    "putCall": "UNKNOWN"
                }
            }
        ]
    }"#;
    let account: SecuritiesAccount = serde_json::from_str(json).unwrap();
    let call = account.positions.as_ref().unwrap()[0]
        .instrument
        .as_ref()
        .unwrap();
    assert_eq!(call.asset_type, AssetType::Option);
    assert_eq!(call.put_call, Some(PutCall::Call));
    assert_eq!(call.underlying_symbol.as_deref(), Some("AAPL"));
    assert_eq!(call.option_multiplier, Some(100));
    assert_eq!(call.type_.as_deref(), Some("VANILLA"));
    assert!(call.extra.is_empty(), "{:?}", call.extra);
    let sweep = account.positions.as_ref().unwrap()[1]
        .instrument
        .as_ref()
        .unwrap();
    assert_eq!(sweep.asset_type, AssetType::CashEquivalent);
    // Schwab's spec lists `UNKNOWN` next to `PUT`/`CALL`; it reads as
    // "no right" rather than failing the whole account.
    assert_eq!(sweep.put_call, None);
    assert_eq!(sweep.type_.as_deref(), Some("MONEY_MARKET_FUND"));
}
