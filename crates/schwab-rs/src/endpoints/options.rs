#![allow(missing_docs)]

use crate::client::SchwabClient;
use crate::client::OptionChainParams;
use crate::error::Result;
use crate::types::market_data::OptionChainResponse;
use reqwest::Method;

impl SchwabClient {
    pub async fn endpoints_get_option_chain(&self, params: &OptionChainParams) -> Result<OptionChainResponse> {
        self.request_with_query(Method::GET, "/marketdata/v1/chains", params).await
    }
}
