//! Main Schwab API client for REST endpoints.
//!
//! Provides authenticated access to accounts, trading, market data, and quotes.

#![allow(missing_docs)] // Internal implementation details

use crate::auth::{AuthManager, create_bearer_header};
use crate::config::{ClientConfig, SchwabConfig};
use crate::error::{Error, Result};
use crate::retry::RetryPolicy;
use crate::types::*;
use crate::transport::HttpTransport;
use crate::streaming::SubscriptionManager;
use chrono::NaiveDate;
use governor::{Quota, RateLimiter};
use reqwest::{Client as HttpClient, Method, Response, StatusCode};
use serde::{de::DeserializeOwned, Serialize};
use std::sync::Arc;
use tracing::{debug, info, warn};
use url::Url;
use urlencoding;

#[derive(Clone)]
pub struct SchwabClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    auth_manager: AuthManager,
    config: ClientConfig,
    transport: HttpTransport,
    rate_limiter: Option<Arc<RateLimiter<governor::state::NotKeyed, governor::state::InMemoryState, governor::clock::DefaultClock>>>,
    retry_policy: RetryPolicy,
    #[allow(dead_code)] // Reserved for streaming subscription integration
    subscriptions: Arc<SubscriptionManager>,
}

impl SchwabClient {
    pub fn new(config: SchwabConfig) -> Result<Self> {
        config.validate()?;

        let auth_manager = AuthManager::new(config.oauth)
            .map_err(|e| Error::Auth(e))?;

        // Build transport with validated base URL and timeout
        let _ = Url::parse(&config.client.base_url)
            .map_err(|e| Error::Config(format!("Invalid base URL: {}", e)))?;
        let transport = HttpTransport::new(config.client.base_url.clone(), config.client.timeout)?;

        let rate_limiter = if config.client.rate_limit.enabled {
            use std::num::NonZeroU32;
            let rps = NonZeroU32::new(config.client.rate_limit.requests_per_second)
                .ok_or_else(|| Error::Config("requests_per_second must be > 0".to_string()))?;
            let burst = NonZeroU32::new(config.client.rate_limit.burst_size)
                .ok_or_else(|| Error::Config("burst_size must be > 0".to_string()))?;
            let quota = Quota::per_second(rps).allow_burst(burst);
            Some(Arc::new(RateLimiter::direct(quota)))
        } else {
            None
        };

        let retry_policy = RetryPolicy::new(&config.client.retry);

        Ok(Self {
            inner: Arc::new(ClientInner {
                auth_manager,
                config: config.client,
                transport,
                rate_limiter,
                retry_policy,
                subscriptions: Arc::new(SubscriptionManager::new()),
            }),
        })
    }

    pub fn builder() -> SchwabClientBuilder {
        SchwabClientBuilder::new()
    }

    pub async fn init(&self) -> Result<()> {
        self.inner.auth_manager.start().await.map_err(Error::Auth)?;
        info!("Schwab client initialized successfully");
        Ok(())
    }

    pub(crate) async fn request<T>(&self, method: Method, path: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        self.request_with_options::<T, ()>(method, path, None::<&()>, None::<&()>).await
    }

    pub(crate) async fn request_with_query<T, Q>(&self, method: Method, path: &str, query: &Q) -> Result<T>
    where
        T: DeserializeOwned,
        Q: Serialize + ?Sized,
    {
        self.request_with_options(method, path, Some(query), None::<&()>).await
    }

    pub(crate) async fn request_with_body<T, B>(&self, method: Method, path: &str, body: &B) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize,
    {
        self.request_with_options::<T, ()>(method, path, None::<&()>, Some(body)).await
    }

    pub(crate) async fn request_with_options<T, Q>(
        &self,
        method: Method,
        path: &str,
        query: Option<&Q>,
        body: Option<&(impl Serialize + ?Sized)>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        Q: Serialize + ?Sized,
    {
        // Apply rate limiting
        if let Some(ref rate_limiter) = self.inner.rate_limiter {
            rate_limiter.until_ready().await;
        }

        // Allow a single refresh-and-retry on 401
        let mut attempted_refresh = false;

        loop {
            // Obtain latest access token each attempt
            let access_token = self
                .inner
                .auth_manager
                .get_access_token()
                .await
                .map_err(Error::Auth)?;

            debug!("Making request to {}{}", self.inner.config.base_url, path);

            let result: Result<T> = self
                .inner
                .retry_policy
                .execute(|| {
                    let headers = vec![
                        ("Authorization".to_string(), create_bearer_header(&access_token)),
                        ("Accept".to_string(), "application/json".to_string()),
                    ];
                    async {
                        self.inner
                            .transport
                            .request::<T>(
                                method.clone(),
                                path,
                                headers,
                                query,
                                body,
                            )
                            .await
                    }
                })
                .await;

            match result {
                Ok(value) => return Ok(value),
                Err(Error::Http { status, .. }) if status == reqwest::StatusCode::UNAUTHORIZED && !attempted_refresh => {
                    warn!("401 Unauthorized received, attempting token refresh and retry");
                    self.inner
                        .auth_manager
                        .ensure_valid_tokens()
                        .await
                        .map_err(Error::Auth)?;
                    attempted_refresh = true;
                    continue; // rebuild request with new token
                }
                Err(err) => return Err(err),
            }
        }
    }

    #[allow(dead_code)] // Reserved for future response handling improvements
    async fn handle_response<T>(&self, response: Response) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let status = response.status();
        
        if status.is_success() {
            response.json().await.map_err(Error::from)
        } else {
            let body = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            
            match status {
                StatusCode::UNAUTHORIZED => {
                    warn!("Received 401 Unauthorized, token may be expired");
                    Err(Error::Http {
                        status,
                        message: "Authentication failed".to_string(),
                    })
                }
                StatusCode::TOO_MANY_REQUESTS => {
                    let retry_after = 60; // Default to 60 seconds
                    Err(Error::RateLimit { retry_after })
                }
                _ => {
                    Err(crate::error::parse_api_error(status, &body))
                }
            }
        }
    }

    // ==================== Market Data Endpoints ====================
    
    /// Get quotes for multiple symbols
    pub async fn get_quotes(&self, symbols: &[&str]) -> Result<QuotesResponse> {
        self.get_quotes_with_options(symbols, None, None).await
    }

    /// Get quotes for multiple symbols with advanced options
    pub async fn get_quotes_with_options(&self, symbols: &[&str], fields: Option<&str>, indicative: Option<bool>) -> Result<QuotesResponse> {
        if symbols.is_empty() {
            return Err(Error::InvalidParameter("No symbols provided".to_string()));
        }
        let mut params = vec![("symbols", symbols.join(","))];
        if let Some(f) = fields {
            params.push(("fields", f.to_string()));
        }
        if let Some(i) = indicative {
            params.push(("indicative", i.to_string()));
        }
        self.request_with_query(Method::GET, "/marketdata/v1/quotes", &params).await
    }

    /// Get quote for a single symbol
    pub async fn get_quote(&self, symbol: &str) -> Result<QuoteItem> {
        let encoded_symbol = urlencoding::encode(symbol);
        self.request(Method::GET, &format!("/marketdata/v1/{}/quotes", encoded_symbol)).await
    }

    /// Get quote for a single symbol with advanced options
    pub async fn get_quote_with_options(&self, symbol: &str, fields: Option<&str>) -> Result<QuoteItem> {
        let mut params = Vec::new();
        if let Some(f) = fields {
            params.push(("fields", f.to_string()));
        }
        let encoded_symbol = urlencoding::encode(symbol);
        self.request_with_query(Method::GET, &format!("/marketdata/v1/{}/quotes", encoded_symbol), &params).await
    }

    /// Get price history for a symbol
    pub async fn get_price_history(&self, params: &PriceHistoryParams) -> Result<PriceHistoryResponse> {
        self.request_with_query(Method::GET, "/marketdata/v1/pricehistory", params).await
    }

    /// Get option chain for a symbol
    pub async fn get_option_chain(&self, params: &OptionChainParams) -> Result<OptionChainResponse> {
        self.request_with_query(Method::GET, "/marketdata/v1/chains", params).await
    }

    /// Get option expiration dates for a symbol
    pub async fn get_option_expiration_chain(&self, symbol: &str) -> Result<ExpirationChainResponse> {
        let params = [("symbol", symbol)];
        self.request_with_query(Method::GET, "/marketdata/v1/expirationchain", &params).await
    }

    /// Get market movers for an index
    pub async fn get_movers(&self, index: &str, sort: Option<MoverSort>, frequency: Option<i32>) -> Result<MoversResponse> {
        let mut params = Vec::new();
        if let Some(s) = sort {
            params.push(("sort", format!("{:?}", s).to_uppercase()));
        }
        if let Some(f) = frequency {
            params.push(("frequency", f.to_string()));
        }
        
        let encoded_index = urlencoding::encode(index);
        self.request_with_query(
            Method::GET,
            &format!("/marketdata/v1/movers/{}", encoded_index),
            &params
        ).await
    }

    /// Search for instruments
    pub async fn search_instruments(&self, symbol: &str, projection: Projection) -> Result<InstrumentsResponse> {
        let params = [
            ("symbol", symbol.to_string()),
            ("projection", format!("{:?}", projection).to_uppercase()),
        ];
        self.request_with_query(Method::GET, "/marketdata/v1/instruments", &params).await
    }

    /// Get instrument details by CUSIP
    pub async fn get_instrument(&self, cusip: &str) -> Result<Instrument> {
        self.request(Method::GET, &format!("/marketdata/v1/instruments/{}", cusip)).await
    }

    /// Get market hours for multiple markets
    pub async fn get_markets(&self, markets: &[&str], date: Option<&str>) -> Result<std::collections::HashMap<String, Vec<crate::types::market_data::MarketHours>>> {
        let mut params = vec![("markets", markets.join(","))];
        if let Some(d) = date {
            params.push(("date", d.to_string()));
        }
        self.request_with_query(Method::GET, "/marketdata/v1/markets", &params).await
    }

    /// Get market hours for a single market
    pub async fn get_market(&self, market_id: &str, date: Option<&str>) -> Result<Vec<crate::types::market_data::MarketHours>> {
        let mut params = Vec::new();
        if let Some(d) = date {
            params.push(("date", d.to_string()));
        }
        self.request_with_query(
            Method::GET, 
            &format!("/marketdata/v1/markets/{}", market_id),
            &params
        ).await
    }

    // ==================== Account & Trading Endpoints ====================
    
    /// Get all linked account numbers
    pub async fn get_account_numbers(&self) -> Result<Vec<AccountNumberHash>> {
        self.request(Method::GET, "/trader/v1/accounts/accountNumbers").await
    }

    /// Get all account details
    pub async fn get_accounts(&self, include_positions: bool) -> Result<Vec<Account>> {
        let params = if include_positions {
            vec![("fields", "positions")]
        } else {
            vec![]
        };
        self.request_with_query(Method::GET, "/trader/v1/accounts", &params).await
    }

    /// Get specific account details
    pub async fn get_account(&self, account_hash: &str, include_positions: bool) -> Result<Account> {
        let params = if include_positions {
            vec![("fields", "positions")]
        } else {
            vec![]
        };
        self.request_with_query(
            Method::GET,
            &format!("/trader/v1/accounts/{}", account_hash),
            &params
        ).await
    }

    /// Get orders for an account
    pub async fn get_account_orders(
        &self,
        account_hash: &str,
        from_entered_time: Option<&str>,
        to_entered_time: Option<&str>,
        max_results: Option<i32>,
        status: Option<&str>,
    ) -> Result<Vec<Order>> {
        let mut params = Vec::new();
        if let Some(from) = from_entered_time {
            params.push(("fromEnteredTime", from.to_string()));
        }
        if let Some(to) = to_entered_time {
            params.push(("toEnteredTime", to.to_string()));
        }
        if let Some(max) = max_results {
            params.push(("maxResults", max.to_string()));
        }
        if let Some(s) = status {
            params.push(("status", s.to_string()));
        }
        
        self.request_with_query(
            Method::GET,
            &format!("/trader/v1/accounts/{}/orders", account_hash),
            &params
        ).await
    }

    /// Place an order
    pub async fn place_order(&self, account_hash: &str, order: &Order) -> Result<OrderResponse> {
        self.request_with_body(
            Method::POST,
            &format!("/trader/v1/accounts/{}/orders", account_hash),
            order
        ).await
    }

    /// Get a specific order
    pub async fn get_order(&self, account_hash: &str, order_id: &str) -> Result<Order> {
        self.request(
            Method::GET,
            &format!("/trader/v1/accounts/{}/orders/{}", account_hash, order_id)
        ).await
    }

    /// Cancel an order
    pub async fn cancel_order(&self, account_hash: &str, order_id: &str) -> Result<()> {
        self.request(
            Method::DELETE,
            &format!("/trader/v1/accounts/{}/orders/{}", account_hash, order_id)
        ).await
    }

    /// Replace an order
    pub async fn replace_order(
        &self,
        account_hash: &str,
        order_id: &str,
        order: &Order
    ) -> Result<OrderResponse> {
        self.request_with_body(
            Method::PUT,
            &format!("/trader/v1/accounts/{}/orders/{}", account_hash, order_id),
            order
        ).await
    }

    /// Preview an order
    pub async fn preview_order(
        &self,
        account_hash: &str,
        order: &Order
    ) -> Result<OrderResponse> {
        self.request_with_body(
            Method::POST,
            &format!("/trader/v1/accounts/{}/previewOrder", account_hash),
            order
        ).await
    }

    /// Get all orders across all accounts
    pub async fn get_all_orders(
        &self,
        from_entered_time: Option<&str>,
        to_entered_time: Option<&str>,
        max_results: Option<i32>,
        status: Option<&str>,
    ) -> Result<Vec<Order>> {
        let mut params = Vec::new();
        if let Some(from) = from_entered_time {
            params.push(("fromEnteredTime", from.to_string()));
        }
        if let Some(to) = to_entered_time {
            params.push(("toEnteredTime", to.to_string()));
        }
        if let Some(max) = max_results {
            params.push(("maxResults", max.to_string()));
        }
        if let Some(s) = status {
            params.push(("status", s.to_string()));
        }
        
        self.request_with_query(Method::GET, "/trader/v1/orders", &params).await
    }

    /// Get account transactions
    pub async fn get_transactions(
        &self,
        account_hash: &str,
        transaction_type: Option<&str>,
        symbol: Option<&str>,
        start_date: Option<&str>,
        end_date: Option<&str>,
    ) -> Result<Vec<Transaction>> {
        let mut params = Vec::new();
        if let Some(t) = transaction_type {
            params.push(("type", t.to_string()));
        }
        if let Some(s) = symbol {
            params.push(("symbol", s.to_string()));
        }
        if let Some(start) = start_date {
            params.push(("startDate", start.to_string()));
        }
        if let Some(end) = end_date {
            params.push(("endDate", end.to_string()));
        }
        
        self.request_with_query(
            Method::GET,
            &format!("/trader/v1/accounts/{}/transactions", account_hash),
            &params
        ).await
    }

    /// Get a specific transaction
    pub async fn get_transaction(
        &self,
        account_hash: &str,
        transaction_id: &str
    ) -> Result<Transaction> {
        self.request(
            Method::GET,
            &format!("/trader/v1/accounts/{}/transactions/{}", account_hash, transaction_id)
        ).await
    }

    /// Get user preferences
    pub async fn get_user_preferences(&self) -> Result<UserPreferences> {
        self.request(Method::GET, "/trader/v1/userPreference").await
    }
}

#[derive(Debug, Clone)]
pub struct SchwabClientBuilder {
    config: Option<SchwabConfig>,
    auth_manager: Option<AuthManager>,
    http_client: Option<HttpClient>,
}

impl SchwabClientBuilder {
    pub fn new() -> Self {
        Self {
            config: None,
            auth_manager: None,
            http_client: None,
        }
    }

    pub fn config(mut self, config: SchwabConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn auth_manager(mut self, auth: AuthManager) -> Self {
        self.auth_manager = Some(auth);
        self
    }

    pub fn http_client(mut self, client: HttpClient) -> Self {
        self.http_client = Some(client);
        self
    }

    pub fn build(self) -> Result<SchwabClient> {
        let config = self.config
            .or_else(|| SchwabConfig::from_env().ok())
            .ok_or_else(|| Error::Config("No configuration provided".to_string()))?;

        SchwabClient::new(config)
    }
}

impl Default for SchwabClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// Parameter structs for complex queries

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceHistoryParams {
    pub symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub period_type: Option<PeriodType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub period: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_type: Option<FrequencyType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_date: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_date: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub need_extended_hours_data: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub need_previous_close: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionChainParams {
    pub symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_type: Option<ContractType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strike_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_underlying_quote: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy: Option<OptionStrategy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strike: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_date: Option<NaiveDate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_date: Option<NaiveDate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volatility: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub underlying_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interest_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days_to_expiration: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp_month: Option<ExpirationMonth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub option_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entitlement: Option<String>,
}

#[cfg(test)]
mod tests {
    use wiremock::{MockServer, Mock, ResponseTemplate};
    use wiremock::matchers::{method, path, header};

    #[tokio::test]
    async fn test_get_quotes() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/marketdata/v1/quotes"))
            .and(header("Authorization", "Bearer test_token"))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "quotes": [{
                        "symbol": "AAPL",
                        "quote": {
                            "lastPrice": 150.00
                        }
                    }]
                })))
            .mount(&mock_server)
            .await;

        // Test would continue here with proper setup
    }
}