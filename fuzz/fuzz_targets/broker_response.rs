//! Broker responses are decoded as typed values, after the integral-float
//! normalization the SDK applies: neither may panic on any input.
#![no_main]
use libfuzzer_sys::fuzz_target;
use schwab_types::{accounts::Account, market_data, trading};

fuzz_target!(|data: &[u8]| {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    schwab_types::common::normalize_integral_numbers(&mut value);
    let _ = serde_json::from_value::<Vec<trading::Order>>(value.clone());
    let _ = serde_json::from_value::<Vec<Account>>(value.clone());
    let _ = serde_json::from_value::<market_data::QuotesResponse>(value.clone());
    let _ = serde_json::from_value::<market_data::OptionChainResponse>(value.clone());
    let _ = serde_json::from_value::<trading::OrderPreview>(value);
});
