//! `SchwabMcp` — the aggregate MCP server impl.
//!
//! All `#[tool]` methods must live in a single `impl` block so the
//! `#[server]` macro can generate the handler. Sections are ordered by
//! category (market data → accounts → transactions → trading → streaming)
//! to keep the wall of tools navigable.

use std::sync::Arc;

use schwab_rs::config::StreamConfig;
use schwab_rs::streaming::StreamClient;
use schwab_rs::SchwabClient;
use schwab_rs::client::{OptionChainParams, PriceHistoryParams};
use schwab_types::market_data::{MoverSort, Projection};
use schwab_types::streaming::StreamService;
use schwab_types::trading::Order;
use tokio::sync::RwLock as AsyncRwLock;
use turbomcp::prelude::*;

use crate::stream_cache::{StreamCache, spawn_cache_feeder};

/// Env var that unlocks the write-side order tools
/// (`place_order`, `cancel_order`, `replace_order`).
///
/// Set to `1` / `true` to allow real trades. Unset or any other value keeps
/// them disabled — an LLM can still call `preview_order` to see the effect.
const ALLOW_TRADING_ENV: &str = "SCHWAB_MCP_ALLOW_TRADING";

fn trading_enabled() -> bool {
    matches!(
        std::env::var(ALLOW_TRADING_ENV).as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

fn require_trading_enabled(op: &str) -> McpResult<()> {
    if trading_enabled() {
        Ok(())
    } else {
        Err(McpError::invalid_params(format!(
            "{op} is disabled. Set {ALLOW_TRADING_ENV}=1 to enable real order submission."
        )))
    }
}

/// MCP server wrapping a configured `SchwabClient`.
///
/// Holds optional streaming state — a `StreamClient` is built lazily on the
/// first `stream_connect` call so that read-only MCP clients never pay the
/// WebSocket setup cost.
#[derive(Clone)]
pub struct SchwabMcp {
    client: Arc<SchwabClient>,
    stream: Arc<AsyncRwLock<Option<StreamClient>>>,
    cache: StreamCache,
    feeder: Arc<AsyncRwLock<Option<tokio::task::JoinHandle<()>>>>,
    stream_config: StreamConfig,
}

impl SchwabMcp {
    /// Construct the MCP server. Uses `StreamConfig::default()` — override
    /// the websocket URL via `SCHWAB_WEBSOCKET_URL` in the env before
    /// constructing the client if needed.
    pub fn new(client: Arc<SchwabClient>) -> Self {
        Self {
            client,
            stream: Arc::new(AsyncRwLock::new(None)),
            cache: StreamCache::new(),
            feeder: Arc::new(AsyncRwLock::new(None)),
            stream_config: StreamConfig::default(),
        }
    }
}

/// Convert any `schwab_rs::Error` into an `McpError` with the upstream
/// message attached as context.
fn map_err(e: schwab_rs::Error) -> McpError {
    McpError::internal(format!("schwab api error: {e}"))
}

#[turbomcp::server(
    name = "schwab-mcp",
    version = "0.1.0",
    description = "Charles Schwab Trading + Market Data — full API surface as MCP tools"
)]
impl SchwabMcp {
    // ============================================================
    // Market Data
    // ============================================================

    /// Get quotes for one or more symbols (equities, options, etc.).
    /// Optional `fields` is a comma-separated subset like "quote,fundamental".
    /// Set `indicative=true` to include pre/post-market indicative quotes.
    #[tool]
    async fn get_quotes(
        &self,
        symbols: Vec<String>,
        fields: Option<String>,
        indicative: Option<bool>,
    ) -> McpResult<Json<serde_json::Value>> {
        let refs: Vec<&str> = symbols.iter().map(String::as_str).collect();
        let resp = self
            .client
            .get_quotes_with_options(&refs, fields.as_deref(), indicative)
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    /// Get a quote for a single symbol. Optional `fields` restricts sections.
    #[tool]
    async fn get_quote(
        &self,
        symbol: String,
        fields: Option<String>,
    ) -> McpResult<Json<serde_json::Value>> {
        let resp = self
            .client
            .get_quote_with_options(&symbol, fields.as_deref())
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    /// Get OHLCV price history candles for a symbol.
    /// See `PriceHistoryParams` for period/frequency options.
    #[tool]
    async fn get_price_history(
        &self,
        params: PriceHistoryParams,
    ) -> McpResult<Json<serde_json::Value>> {
        let resp = self
            .client
            .get_price_history(&params)
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    /// Get the options chain for a symbol — calls and puts across expirations.
    #[tool]
    async fn get_option_chain(
        &self,
        params: OptionChainParams,
    ) -> McpResult<Json<serde_json::Value>> {
        let resp = self
            .client
            .get_option_chain(&params)
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    /// Get the list of option expiration dates for a symbol.
    #[tool]
    async fn get_option_expiration_chain(
        &self,
        symbol: String,
    ) -> McpResult<Json<serde_json::Value>> {
        let resp = self
            .client
            .get_option_expiration_chain(&symbol)
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    /// Get top market movers for an index (e.g. `$SPX`, `$COMPX`, `$DJI`).
    /// `frequency` filters to movers over the last N minutes (1, 5, 10, 30, 60).
    #[tool]
    async fn get_movers(
        &self,
        index: String,
        sort: Option<MoverSort>,
        frequency: Option<i32>,
    ) -> McpResult<Json<serde_json::Value>> {
        let resp = self
            .client
            .get_movers(&index, sort, frequency)
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    /// Search instruments by symbol, regex, description, or fundamentals.
    #[tool]
    async fn search_instruments(
        &self,
        symbol: String,
        projection: Projection,
    ) -> McpResult<Json<serde_json::Value>> {
        let resp = self
            .client
            .search_instruments(&symbol, projection)
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    /// Look up an instrument by its CUSIP.
    #[tool]
    async fn get_instrument(&self, cusip: String) -> McpResult<Json<serde_json::Value>> {
        let resp = self.client.get_instrument(&cusip).await.map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    /// Get market hours for multiple markets (`equity,option,future,bond,forex`).
    /// `date` is `YYYY-MM-DD`; omit for today.
    #[tool]
    async fn get_markets(
        &self,
        markets: Vec<String>,
        date: Option<String>,
    ) -> McpResult<Json<serde_json::Value>> {
        let refs: Vec<&str> = markets.iter().map(String::as_str).collect();
        let resp = self
            .client
            .get_markets(&refs, date.as_deref())
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    /// Get market hours for a single market.
    #[tool]
    async fn get_market(
        &self,
        market_id: String,
        date: Option<String>,
    ) -> McpResult<Json<serde_json::Value>> {
        let resp = self
            .client
            .get_market(&market_id, date.as_deref())
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    // ============================================================
    // Accounts
    // ============================================================

    /// List all linked account numbers + their opaque account hashes
    /// (required by every other account/trading endpoint).
    #[tool]
    async fn get_account_numbers(&self) -> McpResult<Json<serde_json::Value>> {
        let resp = self.client.get_account_numbers().await.map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    /// Get full details for every linked account.
    /// Set `include_positions=true` to include current positions.
    #[tool]
    async fn get_accounts(&self, include_positions: bool) -> McpResult<Json<serde_json::Value>> {
        let resp = self
            .client
            .get_accounts(include_positions)
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    /// Get details for a single account by its hashed ID.
    #[tool]
    async fn get_account(
        &self,
        account_hash: String,
        include_positions: bool,
    ) -> McpResult<Json<serde_json::Value>> {
        let resp = self
            .client
            .get_account(&account_hash, include_positions)
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    /// Get user preferences including streaming credentials
    /// (`schwabClientCustomerId`, `schwabClientCorrelId`).
    #[tool]
    async fn get_user_preferences(&self) -> McpResult<Json<serde_json::Value>> {
        let resp = self.client.get_user_preferences().await.map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    // ============================================================
    // Orders (read-only)
    // ============================================================

    /// Fetch a specific order by ID.
    #[tool]
    async fn get_order(
        &self,
        account_hash: String,
        order_id: String,
    ) -> McpResult<Json<serde_json::Value>> {
        let resp = self
            .client
            .get_order(&account_hash, &order_id)
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    /// List orders for a specific account.
    /// Times are ISO-8601 (`2025-01-15T00:00:00.000Z`).
    /// `status` values: `AWAITING_PARENT_ORDER`, `WORKING`, `FILLED`, `CANCELED`, etc.
    #[tool]
    async fn get_account_orders(
        &self,
        account_hash: String,
        from_entered_time: Option<String>,
        to_entered_time: Option<String>,
        max_results: Option<i32>,
        status: Option<String>,
    ) -> McpResult<Json<serde_json::Value>> {
        let resp = self
            .client
            .get_account_orders(
                &account_hash,
                from_entered_time.as_deref(),
                to_entered_time.as_deref(),
                max_results,
                status.as_deref(),
            )
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    /// List orders across all linked accounts in a time window.
    #[tool]
    async fn get_all_orders(
        &self,
        from_entered_time: Option<String>,
        to_entered_time: Option<String>,
        max_results: Option<i32>,
        status: Option<String>,
    ) -> McpResult<Json<serde_json::Value>> {
        let resp = self
            .client
            .get_all_orders(
                from_entered_time.as_deref(),
                to_entered_time.as_deref(),
                max_results,
                status.as_deref(),
            )
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    // ============================================================
    // Transactions
    // ============================================================

    /// List transactions for an account, optionally filtered.
    /// `transaction_type`: `TRADE`, `DIVIDEND_OR_INTEREST`, `CASH_RECEIPT`, `ELECTRONIC_FUND`, etc.
    /// Dates are `YYYY-MM-DD`.
    #[tool]
    async fn get_transactions(
        &self,
        account_hash: String,
        transaction_type: Option<String>,
        symbol: Option<String>,
        start_date: Option<String>,
        end_date: Option<String>,
    ) -> McpResult<Json<serde_json::Value>> {
        let resp = self
            .client
            .get_transactions(
                &account_hash,
                transaction_type.as_deref(),
                symbol.as_deref(),
                start_date.as_deref(),
                end_date.as_deref(),
            )
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    /// Fetch a specific transaction by ID.
    #[tool]
    async fn get_transaction(
        &self,
        account_hash: String,
        transaction_id: String,
    ) -> McpResult<Json<serde_json::Value>> {
        let resp = self
            .client
            .get_transaction(&account_hash, &transaction_id)
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    // ============================================================
    // Orders (write) — always available: preview
    // ============================================================

    /// Dry-run an order to see fees, buying-power impact, and validation
    /// errors *without* submitting it. Always available — does not touch
    /// real positions and not gated by the trading guardrail.
    #[tool]
    async fn preview_order(
        &self,
        account_hash: String,
        order: Order,
    ) -> McpResult<Json<serde_json::Value>> {
        let resp = self
            .client
            .preview_order(&account_hash, &order)
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    // ============================================================
    // Orders (write) — gated by SCHWAB_MCP_ALLOW_TRADING=1
    // ============================================================

    /// Submit an order for execution. **Requires** `SCHWAB_MCP_ALLOW_TRADING=1`
    /// in the server's environment; otherwise returns an error.
    #[tool]
    async fn place_order(
        &self,
        account_hash: String,
        order: Order,
    ) -> McpResult<Json<serde_json::Value>> {
        require_trading_enabled("place_order")?;
        let resp = self
            .client
            .place_order(&account_hash, &order)
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    /// Cancel a working order. **Requires** `SCHWAB_MCP_ALLOW_TRADING=1`.
    #[tool]
    async fn cancel_order(
        &self,
        account_hash: String,
        order_id: String,
    ) -> McpResult<Json<serde_json::Value>> {
        require_trading_enabled("cancel_order")?;
        self.client
            .cancel_order(&account_hash, &order_id)
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::json!({
            "status": "canceled",
            "account_hash": account_hash,
            "order_id": order_id,
        })))
    }

    /// Replace a working order with a new order spec (Schwab atomically
    /// cancels the old one and places the new one). **Requires**
    /// `SCHWAB_MCP_ALLOW_TRADING=1`.
    #[tool]
    async fn replace_order(
        &self,
        account_hash: String,
        order_id: String,
        order: Order,
    ) -> McpResult<Json<serde_json::Value>> {
        require_trading_enabled("replace_order")?;
        let resp = self
            .client
            .replace_order(&account_hash, &order_id, &order)
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::to_value(resp).unwrap_or_default()))
    }

    // ============================================================
    // Streaming
    // ============================================================

    /// Connect to the Schwab streaming WebSocket.
    ///
    /// Uses the user's `streamerInfo` (fetched via `get_user_preferences`)
    /// for customer/correl IDs, then authenticates and spawns a background
    /// task that feeds incoming messages into the in-memory snapshot cache.
    /// Idempotent — if already connected, returns the current status.
    #[tool]
    async fn stream_connect(&self) -> McpResult<Json<serde_json::Value>> {
        let mut guard = self.stream.write().await;
        if guard.is_some() {
            return Ok(Json(serde_json::json!({
                "status": "already_connected",
            })));
        }

        let prefs = self.client.get_user_preferences().await.map_err(map_err)?;
        let streamer = prefs
            .streamer_info
            .as_ref()
            .and_then(|v| v.first())
            .ok_or_else(|| {
                McpError::internal(
                    "no streamerInfo on user preferences — cannot build StreamClient",
                )
            })?;

        let mut cfg = self.stream_config.clone();
        if !streamer.streamer_socket_url.is_empty() {
            cfg.websocket_url = streamer.streamer_socket_url.clone();
        }

        let client = StreamClient::new(
            cfg,
            self.client.auth_manager(),
            streamer.schwab_client_customer_id.clone(),
            streamer.schwab_client_correl_id.clone(),
        )
        .map_err(map_err)?;
        client.connect().await.map_err(map_err)?;

        let rx = client.get_receiver().ok_or_else(|| {
            McpError::internal("StreamClient receiver already taken — cannot attach cache feeder")
        })?;
        let handle = spawn_cache_feeder(rx, self.cache.clone());
        *self.feeder.write().await = Some(handle);

        *guard = Some(client);
        Ok(Json(serde_json::json!({
            "status": "connected",
            "websocket_url": streamer.streamer_socket_url,
        })))
    }

    /// Disconnect the streaming client, abort the cache feeder, and drop
    /// cached snapshots. Idempotent.
    #[tool]
    async fn stream_disconnect(&self) -> McpResult<Json<serde_json::Value>> {
        let existed = self.stream.write().await.take().is_some();
        if let Some(handle) = self.feeder.write().await.take() {
            handle.abort();
        }
        self.cache.clear();
        Ok(Json(serde_json::json!({
            "status": if existed { "disconnected" } else { "was_not_connected" },
        })))
    }

    /// Subscribe to a streaming service for the given keys (symbols, contracts,
    /// or screener IDs depending on service). Calls `stream_connect` implicitly
    /// if not yet connected. For `ACCT_ACTIVITY` pass an empty `keys` list.
    #[tool]
    async fn stream_subscribe(
        &self,
        service: StreamService,
        keys: Vec<String>,
    ) -> McpResult<Json<serde_json::Value>> {
        self.ensure_connected().await?;
        let guard = self.stream.read().await;
        let client = guard
            .as_ref()
            .ok_or_else(|| McpError::internal("stream client missing after connect"))?;
        client
            .subscribe(service.clone(), keys.clone())
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::json!({
            "status": "subscribed",
            "service": service.as_str(),
            "keys": keys,
        })))
    }

    /// Unsubscribe from a streaming service for specific keys.
    #[tool]
    async fn stream_unsubscribe(
        &self,
        service: StreamService,
        keys: Vec<String>,
    ) -> McpResult<Json<serde_json::Value>> {
        let guard = self.stream.read().await;
        let client = guard
            .as_ref()
            .ok_or_else(|| McpError::invalid_params("not connected — call stream_connect first"))?;
        client
            .unsubscribe(service.clone(), keys.clone())
            .await
            .map_err(map_err)?;
        Ok(Json(serde_json::json!({
            "status": "unsubscribed",
            "service": service.as_str(),
            "keys": keys,
        })))
    }

    /// Override the field bitmap (comma-separated field IDs) sent on the
    /// *next* subscription for `service`. See Schwab's streaming docs for
    /// valid field IDs per service.
    #[tool]
    async fn stream_set_service_fields(
        &self,
        service: StreamService,
        fields_csv: String,
    ) -> McpResult<Json<serde_json::Value>> {
        let guard = self.stream.read().await;
        let client = guard
            .as_ref()
            .ok_or_else(|| McpError::invalid_params("not connected — call stream_connect first"))?;
        client.set_service_fields(service.clone(), fields_csv.clone());
        Ok(Json(serde_json::json!({
            "status": "updated",
            "service": service.as_str(),
            "fields": fields_csv,
        })))
    }

    /// Read the most recent streaming snapshot for `service`. If `keys` is
    /// omitted, returns every cached key for the service; otherwise filters
    /// to the listed keys. Returns an object keyed by `key`.
    #[tool]
    async fn stream_snapshot(
        &self,
        service: StreamService,
        keys: Option<Vec<String>>,
    ) -> McpResult<Json<serde_json::Value>> {
        let snap = self.cache.snapshot(service.as_str(), keys.as_deref());
        Ok(Json(serde_json::json!({
            "service": service.as_str(),
            "count": snap.len(),
            "data": snap,
        })))
    }

    /// Report whether the stream is currently connected.
    #[tool]
    async fn stream_status(&self) -> McpResult<Json<serde_json::Value>> {
        let connected = self.stream.read().await.is_some();
        Ok(Json(serde_json::json!({
            "connected": connected,
        })))
    }
}

impl SchwabMcp {
    /// Internal helper: connect if not yet connected.
    async fn ensure_connected(&self) -> McpResult<()> {
        if self.stream.read().await.is_some() {
            return Ok(());
        }
        // Reuse the tool's logic by locking and running the connect path inline.
        let mut guard = self.stream.write().await;
        if guard.is_some() {
            return Ok(());
        }

        let prefs = self.client.get_user_preferences().await.map_err(map_err)?;
        let streamer = prefs
            .streamer_info
            .as_ref()
            .and_then(|v| v.first())
            .ok_or_else(|| McpError::internal("no streamerInfo on user preferences"))?;

        let mut cfg = self.stream_config.clone();
        if !streamer.streamer_socket_url.is_empty() {
            cfg.websocket_url = streamer.streamer_socket_url.clone();
        }

        let client = StreamClient::new(
            cfg,
            self.client.auth_manager(),
            streamer.schwab_client_customer_id.clone(),
            streamer.schwab_client_correl_id.clone(),
        )
        .map_err(map_err)?;
        client.connect().await.map_err(map_err)?;

        let rx = client
            .get_receiver()
            .ok_or_else(|| McpError::internal("StreamClient receiver already taken"))?;
        let handle = spawn_cache_feeder(rx, self.cache.clone());
        *self.feeder.write().await = Some(handle);

        *guard = Some(client);
        Ok(())
    }
}
