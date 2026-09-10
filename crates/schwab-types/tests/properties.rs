use proptest::prelude::*;
use schwab_types::{streaming::StreamMessage, trading::OrderRequest};
proptest! {
    #[test]
    fn malformed_wire_input_never_panics(bytes in proptest::collection::vec(any::<u8>(),0..4096)) {
        let _=serde_json::from_slice::<OrderRequest>(&bytes);
        let _=serde_json::from_slice::<StreamMessage>(&bytes);
    }
    #[test]
    fn decimal_execution_price_retains_exact_coefficient(coefficient in 1u64..1_000_000_000,scale in 0u32..8) {
        let price=rust_decimal::Decimal::new(coefficient as i64,scale);
        let value=serde_json::json!({"orderStrategyType":"SINGLE","session":"NORMAL","duration":"DAY","orderType":"LIMIT","price":price.to_string(),"orderLegCollection":[{"instruction":"BUY","quantity":"1","instrument":{"assetType":"EQUITY","symbol":"AAPL"}}]});
        let order:OrderRequest=serde_json::from_value(value).unwrap();
        prop_assert_eq!(order.price,Some(price));
        prop_assert!(order.validate_price_increment(rust_decimal::Decimal::new(1,scale)).is_ok());
        let serialized=serde_json::to_value(order).unwrap();
        prop_assert_eq!(serialized["price"].as_str().unwrap().parse::<rust_decimal::Decimal>().unwrap(),price);
    }
}
