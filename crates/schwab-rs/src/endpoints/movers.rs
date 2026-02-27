#![allow(missing_docs)]

use crate::client::SchwabClient;
use crate::error::Result;
use crate::types::market_data::MoversResponse;
use crate::types::market_data::MoverSort;
use reqwest::Method;

impl SchwabClient {
    pub async fn endpoints_get_movers(&self, index: &str, sort: Option<MoverSort>, frequency: Option<i32>) -> Result<MoversResponse> {
        let mut params = Vec::new();
        if let Some(s) = sort { params.push(("sort", format!("{:?}", s).to_uppercase())); }
        if let Some(f) = frequency { params.push(("frequency", f.to_string())); }
        self.request_with_query(Method::GET, &format!("/marketdata/v1/movers/{}", urlencoding::encode(index)), &params).await
    }
}
