#![allow(missing_docs)]

use crate::client::SchwabClient;
use crate::error::Result;
use crate::types::accounts::*;
use reqwest::Method;

impl SchwabClient {
    pub async fn endpoints_get_accounts(&self, include_positions: bool) -> Result<Vec<Account>> {
        let params = if include_positions { vec![("fields", "positions")] } else { vec![] };
        self.request_with_query(Method::GET, "/trader/v1/accounts", &params).await
    }

    pub async fn endpoints_get_account(&self, account_hash: &str, include_positions: bool) -> Result<Account> {
        let params = if include_positions { vec![("fields", "positions")] } else { vec![] };
        self.request_with_query(Method::GET, &format!("/trader/v1/accounts/{}", account_hash), &params).await
    }
}
