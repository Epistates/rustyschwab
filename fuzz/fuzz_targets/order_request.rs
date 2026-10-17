//! Order requests come from callers, and over MCP from models: parsing,
//! validating and re-serializing one must never panic, and what validates
//! must survive its own wire form.
#![no_main]
use libfuzzer_sys::fuzz_target;
use rustyschwab_types::trading::OrderRequest;

fuzz_target!(|data: &[u8]| {
    let Ok(order) = serde_json::from_slice::<OrderRequest>(data) else {
        return;
    };
    if order.validate().is_ok() {
        let wire = serde_json::to_vec(&order).expect("a valid order serializes");
        let again: OrderRequest =
            serde_json::from_slice(&wire).expect("a valid order reads back from its wire form");
        assert_eq!(again, order);
        again.validate().expect("a valid order stays valid on its wire form");
    }
});
