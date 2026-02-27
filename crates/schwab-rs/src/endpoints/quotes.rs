#![allow(missing_docs)]

use crate::client::SchwabClient;
use crate::error::Result;
use crate::types::market_data::*;
use reqwest::Method;

impl SchwabClient {
    pub async fn endpoints_get_quotes(&self, symbols: &[&str]) -> Result<QuotesResponse> {
        let params = [("symbols", symbols.join(","))];
        self.request_with_query(Method::GET, "/marketdata/v1/quotes", &params).await
    }
}
