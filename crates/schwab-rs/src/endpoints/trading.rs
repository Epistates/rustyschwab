#![allow(missing_docs)]

use crate::client::SchwabClient;
use crate::error::Result;
use crate::types::trading::*;
use reqwest::Method;

impl SchwabClient {
    pub async fn endpoints_place_order(&self, account_hash: &str, order: &Order) -> Result<OrderResponse> {
        self.request_with_body(Method::POST, &format!("/trader/v1/accounts/{}/orders", account_hash), order).await
    }

    pub async fn endpoints_cancel_order(&self, account_hash: &str, order_id: &str) -> Result<()> {
        self.request(Method::DELETE, &format!("/trader/v1/accounts/{}/orders/{}", account_hash, order_id)).await
    }
}
