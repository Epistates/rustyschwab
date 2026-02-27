#![allow(missing_docs)]

use crate::client::SchwabClient;
use crate::error::Result;
use crate::client::PriceHistoryParams;
use crate::types::market_data::PriceHistoryResponse;
use reqwest::Method;

impl SchwabClient {
    pub async fn endpoints_get_price_history(&self, params: &PriceHistoryParams) -> Result<PriceHistoryResponse> {
        self.request_with_query(Method::GET, "/marketdata/v1/pricehistory", params).await
    }
}
