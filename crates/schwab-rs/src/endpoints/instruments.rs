#![allow(missing_docs)]

use crate::client::SchwabClient;
use crate::error::Result;
use crate::types::market_data::{InstrumentsResponse, Instrument, Projection};
use reqwest::Method;

impl SchwabClient {
    pub async fn endpoints_search_instruments(&self, symbol: &str, projection: Projection) -> Result<InstrumentsResponse> {
        let params = [("symbol", symbol.to_string()), ("projection", format!("{:?}", projection).to_uppercase())];
        self.request_with_query(Method::GET, "/marketdata/v1/instruments", &params).await
    }

    pub async fn endpoints_get_instrument(&self, cusip: &str) -> Result<Instrument> {
        self.request(Method::GET, &format!("/marketdata/v1/instruments/{}", cusip)).await
    }
}
