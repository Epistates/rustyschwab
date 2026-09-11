use proptest::prelude::*;
use rust_decimal::Decimal;
use schwab_types::{streaming::StreamMessage, trading::OrderRequest};
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
