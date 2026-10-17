use proptest::prelude::*;
use rust_decimal::Decimal;
use rustyschwab_types::{streaming::StreamMessage, trading::OrderRequest};
proptest! {
    #[test]
    fn malformed_wire_input_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let _ = serde_json::from_slice::<OrderRequest>(&bytes);
        let _ = serde_json::from_slice::<StreamMessage>(&bytes);
    }
    #[test]
    fn decimal_execution_price_retains_exact_coefficient(
        coefficient in 1u64..1_000_000_000,
        scale in 0u32..8,
    ) {
        let price = Decimal::new(coefficient as i64, scale);
        let value = serde_json::json!({
            "orderStrategyType": "SINGLE",
            "session": "NORMAL",
            "duration": "DAY",
            "orderType": "LIMIT",
            "price": price.to_string(),
            "orderLegCollection": [{
                "instruction": "BUY",
                "quantity": "1",
                "instrument": {"assetType": "EQUITY", "symbol": "AAPL"},
            }],
        });
        let order: OrderRequest = serde_json::from_value(value).unwrap();
        prop_assert_eq!(order.price, Some(price));
        prop_assert!(order.validate_price_increment(Decimal::new(1, scale)).is_ok());
        let serialized = serde_json::to_value(order).unwrap();
        let round_tripped: Decimal = serialized["price"].as_str().unwrap().parse().unwrap();
        prop_assert_eq!(round_tripped, price);
    }
}

use chrono::NaiveDate;
use rustyschwab_types::{
    common::PutCall,
    streaming::DataContent,
    trading::{OptionSymbol, Order, OrderSpec, Transaction},
};

proptest! {
    /// Every contract OSI can express formats to 21 characters and parses
    /// back to itself.
    #[test]
    fn option_symbols_round_trip(
        root in "[A-Z][A-Z0-9]{0,5}",
        days in 0i64..(365 * 70),
        call in any::<bool>(),
        thousandths in 1i64..100_000_000,
    ) {
        let expiry = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap() + chrono::Duration::days(days);
        let right = if call { PutCall::Call } else { PutCall::Put };
        let symbol = OptionSymbol::new(&root, expiry, right, Decimal::new(thousandths, 3)).unwrap();
        let text = symbol.to_string();
        prop_assert_eq!(text.len(), 21);
        prop_assert_eq!(text.parse::<OptionSymbol>().unwrap(), symbol);
    }

    /// Any string either parses as an OSI symbol that renders back to
    /// exactly that string, or is refused; parsing never panics.
    #[test]
    fn option_symbol_parsing_is_canonical(text in "\\PC{0,24}") {
        if let Ok(symbol) = text.parse::<OptionSymbol>() {
            prop_assert_eq!(symbol.to_string(), text);
        }
    }

    /// A limit order spec expands to a request whose JSON parses back to the
    /// same request.
    #[test]
    fn built_orders_round_trip_through_json(
        quantity in 1u32..10_000,
        cents in 1i64..10_000_000,
        buy in any::<bool>(),
    ) {
        let spec: OrderSpec = serde_json::from_value(serde_json::json!({
            "kind": "limit",
            "leg": {"instruction": if buy { "BUY" } else { "SELL" },
                    "quantity": quantity.to_string(),
                    "instrument": {"asset_type": "EQUITY", "symbol": "AAPL"}},
            "price": Decimal::new(cents, 2).to_string(),
        })).unwrap();
        let order = spec.build().unwrap();
        let json = serde_json::to_value(&order).unwrap();
        let back: OrderRequest = serde_json::from_value(json).unwrap();
        prop_assert_eq!(&back, &order);
        back.validate().unwrap();
    }

    /// Unknown enum spellings on broker responses decode instead of failing
    /// the whole payload.
    #[test]
    fn response_enums_tolerate_unknown_values(value in "[A-Z_]{1,24}") {
        let order = serde_json::json!({
            "orderId": 1, "session": value, "duration": value, "orderType": value,
            "orderStrategyType": value, "status": value, "requestedDestination": value,
            "orderLegCollection": [{"orderLegType": value, "instruction": value,
                "positionEffect": value, "quantity": 1,
                "instrument": {"assetType": value, "symbol": "X"}}]
        });
        prop_assert!(serde_json::from_value::<Order>(order).is_ok());
        let transaction = serde_json::json!({"type": value, "status": value,
            "subAccount": value, "activityType": value});
        prop_assert!(serde_json::from_value::<Transaction>(transaction).is_ok());
    }

    /// Stream items with arbitrary numbered fields always name into an object.
    #[test]
    fn stream_content_names_without_panicking(
        fields in proptest::collection::hash_map("[0-9]{1,3}", any::<i64>(), 0..32),
    ) {
        let mut content = serde_json::Map::new();
        content.insert("key".into(), "AAPL".into());
        for (k, v) in fields {
            content.insert(k, v.into());
        }
        let content: DataContent = serde_json::from_value(content.into()).unwrap();
        let table = rustyschwab_types::streaming::StreamService::LeveloneEquities.fields();
        prop_assert!(content.to_named_value(table).is_object());
    }
}
