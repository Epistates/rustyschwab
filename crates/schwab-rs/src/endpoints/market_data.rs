#![allow(missing_docs)]

use crate::client::SchwabClient;
use crate::error::Result;
use reqwest::Method;
use crate::types::market_data::{MarketsHoursResponse, MarketHours};

impl SchwabClient {
    pub async fn endpoints_get_markets(&self, markets: &[&str], date: Option<&str>) -> Result<MarketsHoursResponse> {
        let mut params = vec![("markets", markets.join(","))];
        if let Some(d) = date { params.push(("date", d.to_string())); }
        self.request_with_query(Method::GET, "/marketdata/v1/markets", &params).await
    }

    pub async fn endpoints_get_market(&self, market_id: &str, date: Option<&str>) -> Result<Vec<MarketHours>> {
        let mut params = Vec::new();
        if let Some(d) = date { params.push(("date", d.to_string())); }
        self.request_with_query(Method::GET, &format!("/marketdata/v1/markets/{}", market_id), &params).await
    }
}
