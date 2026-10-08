//! Main Schwab API client for REST endpoints.
//!
//! Provides authenticated access to accounts, trading, market data, and quotes.

use crate::auth::{AuthManager, create_bearer_header};
use crate::circuit_breaker::CircuitBreaker;
use crate::config::{ClientConfig, SchwabConfig};
use crate::error::{Error, Result};
use crate::request_budget::RequestBudget;
use crate::retry::RetryPolicy;
use crate::transport::HttpTransport;
use crate::types::accounts::{Account, AccountNumberHash, UserPreferences};
use crate::types::market_data::{
    ExpirationChainResponse, Instrument, InstrumentsResponse, Market, MarketsHoursResponse,
    MoverFrequency, MoverIndex, MoverSort, MoversResponse, OptionChainResponse,
    PriceHistoryResponse, Projection, QuoteField, QuoteItem, QuotesResponse,
};
use crate::types::trading::{
    Order, OrderRequest, OrderSubmitResponse, OrdersParams, Transaction, TransactionsParams,
};
use chrono::NaiveDate;
use reqwest::{Method, StatusCode};
use serde::{Serialize, de::DeserializeOwned};
use std::sync::Arc;
use tracing::{debug, info};
use url::Url;
use urlencoding;

pub use crate::types::market_data::{OptionChainParams, PriceHistoryParams};

#[derive(Clone)]
pub struct SchwabClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    mode_verified: tokio::sync::OnceCell<()>,
    journal: crate::execution::OrderJournal,
    execution: Arc<dyn crate::execution::ExecutionStore>,
    coordinator: Arc<dyn crate::coordination::Coordinator>,
    quota_key: String,
    risk: Option<Arc<dyn crate::risk::RiskProvider>>,
    submission_gate: parking_lot::RwLock<Option<String>>,
    draining: std::sync::atomic::AtomicBool,
    auth_manager: AuthManager,
    config: ClientConfig,
    transport: HttpTransport,
    rate_limiter: Option<Arc<RequestBudget>>,
    retry_policy: RetryPolicy,
    circuit_breaker: Arc<CircuitBreaker>,
    /// Wakes a [`RecoveryWorker`](crate::recovery::RecoveryWorker) early.
    reconciliation_wanted: Arc<tokio::sync::Notify>,
}

impl SchwabClient {
    pub fn new(config: SchwabConfig) -> Result<Self> {
        config.validate()?;

        let journal = crate::execution::OrderJournal::new(
            config.oauth.tokens_file.with_extension("orders.json"),
        );
        let origin = Url::parse(&config.client.base_url)
            .map_err(|e| Error::Config(format!("Invalid base URL: {e}")))?
            .origin()
            .ascii_serialization();
        let budget_key = format!("{}:{origin}", config.oauth.app_key);
        let auth_manager = AuthManager::new(config.oauth).map_err(Error::Auth)?;
        let transport = HttpTransport::new(
            config.client.base_url.clone(),
            config.client.timeout,
            config.client.user_agent.clone(),
        )?;

        let rate_limiter = if config.client.rate_limit.enabled {
            Some(RequestBudget::shared(
                budget_key.clone(),
                &config.client.rate_limit,
            )?)
        } else {
            None
        };

        let retry_policy = RetryPolicy::new(&config.client.retry);
        let circuit_breaker = Arc::new(CircuitBreaker::new(config.client.circuit_breaker.clone()));

        Ok(Self {
            inner: Arc::new(ClientInner {
                mode_verified: tokio::sync::OnceCell::new(),
                coordinator: Arc::new(journal.database().clone()),
                quota_key: budget_key.clone(),
                risk: None,
                submission_gate: parking_lot::RwLock::new(None),
                draining: std::sync::atomic::AtomicBool::new(false),
                execution: Arc::new(journal.clone()),
                journal,
                auth_manager,
                config: config.client,
                transport,
                rate_limiter,
                retry_policy,
                circuit_breaker,
                reconciliation_wanted: Arc::new(tokio::sync::Notify::new()),
            }),
        })
    }

    pub fn builder() -> SchwabClientBuilder {
        SchwabClientBuilder::new()
    }

    /// Configure a deployment-owned shared coordinator before cloning the client.
    /// Install a coherent set of remote enterprise services before cloning.
    pub fn with_enterprise_backends(
        mut self,
        execution: Arc<dyn crate::execution::ExecutionStore>,
        coordinator: Arc<dyn crate::coordination::Coordinator>,
        risk: Arc<dyn crate::risk::RiskProvider>,
    ) -> Result<Self> {
        let inner = Arc::get_mut(&mut self.inner)
            .ok_or_else(|| Error::Config("Configure enterprise services before cloning".into()))?;
        inner.execution = execution;
        inner.coordinator = coordinator;
        inner.risk = Some(risk);
        Ok(self)
    }
    /// Suspend new submissions while a required data source is recovering.
    /// Cancellations and reads remain available.
    pub fn suspend_submissions(&self, reason: &str) {
        *self.inner.submission_gate.write() = Some(reason.to_owned());
    }
    /// Trusted application recovery must establish readiness before clearing this gate.
    pub fn resume_submissions(&self) {
        *self.inner.submission_gate.write() = None;
    }
    /// Ask the recovery worker for a reconciliation pass now rather than at
    /// its next interval: account activity arrived, so orders or balances
    /// changed. Requests made during a pass start another one after it.
    pub fn request_reconciliation(&self) {
        self.inner.reconciliation_wanted.notify_one();
    }
    pub(crate) async fn reconciliation_requested(&self) {
        self.inner.reconciliation_wanted.notified().await
    }
    /// Permanently close submission admission on this client and all its clones.
    /// Reconciliation and cancellation remain available during process drain.
    pub fn begin_drain(&self) {
        self.inner
            .draining
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub fn submissions_suspended(&self) -> bool {
        self.inner
            .draining
            .load(std::sync::atomic::Ordering::Acquire)
            || self.inner.submission_gate.read().is_some()
    }
    /// Persist an operator halt shared by every client using this execution store.
    /// Recovery cannot clear it; in-flight broker writes still require reconciliation.
    pub async fn halt_submissions(&self, reason: &str, operator: &str) -> Result<()> {
        self.inner
            .execution
            .set_operator_halt(Some(reason.to_owned()), operator)
            .await
    }
    pub async fn clear_operator_halt(&self, operator: &str) -> Result<()> {
        self.inner.execution.set_operator_halt(None, operator).await
    }
    pub async fn operator_halt(&self) -> Result<Option<String>> {
        self.inner.execution.operator_halt().await
    }
    /// Local admission for a new submission: an operator halt or a
    /// data-recovery suspension refuses it before anything reaches the
    /// broker. Checked again on every attempt, so a pre-flight answer is
    /// advisory.
    pub async fn submission_admission(&self) -> Result<()> {
        if self.inner.execution.operator_halt().await?.is_some() {
            return Err(Error::Halted("Operator trading halt is active".into()));
        }
        if self.submissions_suspended() {
            return Err(Error::Suspended(
                "Required data recovery is incomplete, or the process is draining".into(),
            ));
        }
        Ok(())
    }
    pub fn execution_store(&self) -> Arc<dyn crate::execution::ExecutionStore> {
        self.inner.execution.clone()
    }
    pub fn with_coordinator(
        mut self,
        coordinator: Arc<dyn crate::coordination::Coordinator>,
    ) -> Result<Self> {
        Arc::get_mut(&mut self.inner)
            .ok_or_else(|| {
                Error::Config("Configure coordination before cloning the client".into())
            })?
            .coordinator = coordinator;
        Ok(self)
    }
    pub fn with_risk_provider(mut self, risk: Arc<dyn crate::risk::RiskProvider>) -> Result<Self> {
        Arc::get_mut(&mut self.inner)
            .ok_or_else(|| Error::Config("Configure risk before cloning the client".into()))?
            .risk = Some(risk);
        Ok(self)
    }
    pub(crate) fn risk_provider(&self) -> Option<Arc<dyn crate::risk::RiskProvider>> {
        self.inner.risk.clone()
    }
    pub fn circuit_state(&self) -> crate::circuit_breaker::CircuitState {
        self.inner.circuit_breaker.state()
    }
    /// This process's use of the credential's request budget, `None` when
    /// rate limiting is disabled.
    pub fn request_budget(&self) -> Option<crate::RequestBudgetUsage> {
        self.inner
            .rate_limiter
            .as_ref()
            .map(|budget| budget.usage())
    }
    pub fn coordinator(&self) -> Arc<dyn crate::coordination::Coordinator> {
        self.inner.coordinator.clone()
    }

    async fn ensure_live_state(&self) -> Result<()> {
        self.inner
            .mode_verified
            .get_or_try_init(|| async {
                self.inner
                    .journal
                    .database()
                    .run(|c| {
                        use crate::storage::{meta, sql};
                        let tx = c
                            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                            .map_err(sql)?;
                        let paper = meta::exists(&tx, "paper_config")?;
                        let mode = meta::get(&tx, "execution_mode")?;
                        if paper || mode.as_deref().is_some_and(|m| m != "live") {
                            return Err(Error::Config(
                                "Live client cannot use paper execution state".into(),
                            ));
                        }
                        meta::insert_if_new(&tx, "execution_mode", "live")?;
                        tx.commit().map_err(sql)
                    })
                    .await
            })
            .await
            .map(|_| ())
    }
    pub async fn init(&self) -> Result<()> {
        self.ensure_live_state().await?;
        self.inner.auth_manager.start().await.map_err(Error::Auth)?;
        info!("Schwab client initialized successfully");
        Ok(())
    }

    /// Borrow the client's shared `AuthManager` (Arc-backed).
    ///
    /// Useful when building ancillary clients — e.g. a `StreamClient` — that
    /// need to reuse the same token refresh loop and on-disk token store.
    pub fn order_journal(&self) -> &crate::execution::OrderJournal {
        &self.inner.journal
    }

    pub fn auth_manager(&self) -> AuthManager {
        self.inner.auth_manager.clone()
    }

    /// Borrow the HTTP client config (base URL, retry policy, etc.).
    pub fn client_config(&self) -> &ClientConfig {
        &self.inner.config
    }

    pub(crate) async fn request<T>(&self, op: &'static str, method: Method, path: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        self.request_with_options::<T, ()>(op, method, path, None::<&()>, None::<&()>)
            .await
    }

    pub(crate) async fn request_with_query<T, Q>(
        &self,
        op: &'static str,
        method: Method,
        path: &str,
        query: &Q,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        Q: Serialize + ?Sized + Sync,
    {
        self.request_with_options(op, method, path, Some(query), None::<&()>)
            .await
    }

    pub(crate) async fn request_with_body<T, B>(
        &self,
        op: &'static str,
        method: Method,
        path: &str,
        body: &B,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + Sync,
    {
        self.request_with_options::<T, ()>(op, method, path, None::<&()>, Some(body))
            .await
    }

    pub(crate) async fn request_with_options<T, Q>(
        &self,
        op: &'static str,
        method: Method,
        path: &str,
        query: Option<&Q>,
        body: Option<&(impl Serialize + ?Sized + Sync)>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        Q: Serialize + ?Sized + Sync,
    {
        let method_label = method_label(&method);
        debug!(op, "Making API request");
        let method = &method;
        self.execute_resilient(op, method_label, None, None, move |token: String| {
            Box::pin(async move {
                let headers = vec![
                    ("Authorization".to_string(), create_bearer_header(&token)),
                    ("Accept".to_string(), "application/json".to_string()),
                ];
                self.inner
                    .transport
                    .request::<T>(method.clone(), path, headers, query, body)
                    .await
            })
        })
        .await
    }

    /// `GET` any Schwab REST path and return the raw JSON body.
    ///
    /// Escape hatch for endpoints without typed bindings and for shape
    /// conformance checks. The request goes through the same auth, request
    /// budget, circuit breaker and retry pipeline as the typed methods.
    /// Deliberately `GET`-only: writes must use the order methods so they
    /// are journaled and never replayed.
    ///
    /// `path` is relative to the configured base URL (for example
    /// `/marketdata/v1/quotes`); `query` pairs are URL-encoded for you.
    pub async fn get_json(&self, path: &str, query: &[(&str, &str)]) -> Result<serde_json::Value> {
        if !path.starts_with('/') {
            return Err(Error::InvalidParameter("path must start with '/'".into()));
        }
        self.request_with_options::<serde_json::Value, [(&str, &str)]>(
            "get_json",
            Method::GET,
            path,
            (!query.is_empty()).then_some(query),
            None::<&()>,
        )
        .await
    }

    /// Submit an order (`POST`/`PUT`) whose success payload is the
    /// `Location` response header — Schwab returns 201 with an empty body
    /// for order placement/replacement, carrying the new order's URL in
    /// `Location`. See [`OrderSubmitResponse`].
    ///
    /// The submission runs on its own task. A caller that stops waiting (a
    /// dropped future, a client that went away, a deadline above this one)
    /// abandons only the answer: the journaled intent still reaches its
    /// outcome, rather than being left unresolved with the account blocked
    /// behind it.
    async fn submit_order_request(
        &self,
        op: &'static str,
        method: Method,
        path: &str,
        order: &OrderRequest,
        fence: Option<&crate::coordination::Lease>,
    ) -> Result<OrderSubmitResponse> {
        let client = self.clone();
        let path = path.to_owned();
        let order = order.clone();
        let fence = fence.cloned();
        tokio::spawn(async move {
            client
                .submit_detached(op, method, &path, &order, fence.as_ref())
                .await
        })
        .await
        .map_err(|e| Error::Storage(format!("Submission task failed: {e}")))?
    }

    async fn submit_detached(
        &self,
        op: &'static str,
        method: Method,
        path: &str,
        order: &OrderRequest,
        fence: Option<&crate::coordination::Lease>,
    ) -> Result<OrderSubmitResponse> {
        self.ensure_live_state().await?;
        order.validate()?;
        let mut tagged_order = order.clone();
        if tagged_order.tag.is_none() {
            tagged_order.tag = Some(format!("rs-{}", uuid::Uuid::new_v4()));
        }
        let order = &tagged_order;
        let account = path.split('/').nth(4).unwrap_or_default();
        let replaces = (op == "replace_order").then(|| path.rsplit('/').next().unwrap_or_default());
        if let Some(order_id) = replaces {
            // Risk judges a replacement against the order it replaces, and
            // one this client has not reconciled yet has no exposure it can
            // judge: read it first. Nothing is recorded unless Schwab has
            // it, so a replacement of an unknown order is refused before
            // anything is sent and leaves no trace.
            let current = self.get_order(account, order_id).await?;
            self.inner
                .execution
                .record_order(account, serde_json::to_value(current)?)
                .await?;
        }
        let attempt_id = self
            .inner
            .execution
            .begin_once(account, op, serde_json::to_value(order)?, None)
            .await?;
        let reserved = match (&self.inner.risk, replaces) {
            (None, _) => Ok(()),
            (Some(risk), None) => risk.reserve(account, &attempt_id, order).await,
            (Some(risk), Some(replaces)) => {
                risk.reserve_replacement(account, &attempt_id, order, replaces)
                    .await
            }
        };
        if let Err(error) = reserved {
            self.inner
                .execution
                .finish(&attempt_id, crate::execution::SubmissionOutcome::Rejected)
                .await?;
            return Err(error);
        }
        let method_label = method_label(&method);
        debug!(op, "Submitting order");
        let method = &method;
        let result = self
            .execute_resilient(
                op,
                method_label,
                Some((account, attempt_id.as_str(), order)),
                fence,
                move |token: String| {
                    Box::pin(async move {
                        let headers = vec![
                            ("Authorization".to_string(), create_bearer_header(&token)),
                            ("Accept".to_string(), "application/json".to_string()),
                        ];
                        self.inner
                            .transport
                            .request_for_location(method.clone(), path, headers, Some(order))
                            .await
                    })
                },
            )
            .await;

        let location = match result {
            Ok(location) => location,
            Err(Error::SubmissionUnknown { source, .. }) => {
                return Err(Error::SubmissionUnknown { attempt_id, source });
            }
            Err(error) => {
                // A definite rejection or failure before transmission resolves
                // the intent. Persistence failure leaves it pending, safely.
                self.inner
                    .execution
                    .finish(&attempt_id, crate::execution::SubmissionOutcome::Rejected)
                    .await?;
                if let Some(risk) = &self.inner.risk {
                    risk.release(&attempt_id).await?;
                }
                return Err(error);
            }
        };
        let order_id = location.as_deref().and_then(order_id_from_location);

        self.inner
            .execution
            .finish(
                &attempt_id,
                crate::execution::SubmissionOutcome::Accepted {
                    order_id: order_id.clone(),
                },
            )
            .await
            .map_err(|e| Error::SubmissionUnknown {
                attempt_id: attempt_id.clone(),
                source: Box::new(e),
            })?;
        Ok(OrderSubmitResponse { order_id, location })
    }

    /// Run one logical API call through the full resilience stack:
    /// rate limiter → (token fetch → circuit breaker → retry policy) with a
    /// single refresh-and-retry on 401, recording request metrics either way.
    ///
    /// `call` receives the current access token and performs the actual
    /// transport request; it is re-invoked on each retry attempt. The
    /// future is boxed (`BoxFuture`) so its `Send` bound is explicit —
    /// opaque `AsyncFn` futures trip rustc's "implementation of `Send` is
    /// not general enough" limitation once embedded in spawned handlers.
    async fn execute_resilient<'a, T>(
        &'a self,
        op: &'static str,
        method_label: &'static str,
        risk_context: Option<(&str, &str, &OrderRequest)>,
        fence: Option<&crate::coordination::Lease>,
        call: impl Fn(String) -> futures_util::future::BoxFuture<'a, Result<T>> + 'a,
    ) -> Result<T> {
        self.ensure_live_state().await?;
        let start = std::time::Instant::now();
        let deadline = tokio::time::Instant::now() + self.inner.config.timeout;
        let submission = matches!(op, "place_order" | "replace_order");
        // A cancel only ever reduces risk: it must not be refused because
        // read traffic tripped the breaker, and it is never retried, since
        // a retry after a cancel that landed answers 4xx and hides it.
        let protective = op == "cancel_order";
        let attempt_id = uuid::Uuid::new_v4().to_string();
        let sent = std::sync::atomic::AtomicBool::new(false);
        let operation = async {
            let mut refreshed = false;
            loop {
                let token = self
                    .inner
                    .auth_manager
                    .get_access_token()
                    .await
                    .map_err(Error::Auth)?;
                let attempt = || async {
                    if let Some(budget) = &self.inner.rate_limiter {
                        budget.admit(self.inner.coordinator.as_ref()).await?;
                    }
                    if submission {
                        self.submission_admission().await?;
                    }
                    if let Some((account, attempt, order)) = risk_context {
                        if let Some(risk) = &self.inner.risk {
                            if let Some(lease) = fence {
                                risk.revalidate_fenced(account, attempt, order, lease)
                                    .await?;
                            } else {
                                risk.revalidate(account, attempt, order).await?;
                            }
                        } else if let Some(lease) = fence
                            && !self.inner.coordinator.valid(lease).await?
                        {
                            return Err(Error::OwnershipLost);
                        }
                        // Last step before the request leaves: from here
                        // on the intent may have reached the broker.
                        self.inner.execution.arm(attempt).await?;
                    }
                    sent.store(true, std::sync::atomic::Ordering::Relaxed);
                    call(token.clone()).await
                };
                let result = if protective {
                    attempt().await
                } else {
                    self.inner
                        .circuit_breaker
                        .call(|| async {
                            if submission {
                                attempt().await
                            } else {
                                self.inner
                                    .retry_policy
                                    .execute_until(Some(deadline), attempt)
                                    .await
                            }
                        })
                        .await
                };
                // A write never replays, even on 401. Caller can reconcile and
                // explicitly submit a new intent after credentials recover.
                if !submission
                    && result
                        .as_ref()
                        .err()
                        .is_some_and(|e| e.http_status() == Some(StatusCode::UNAUTHORIZED))
                    && !refreshed
                {
                    self.inner
                        .auth_manager
                        .refresh_rejected_token(&token)
                        .await
                        .map_err(Error::Auth)?;
                    refreshed = true;
                    continue;
                }
                return result;
            }
        };
        let guarded = async {
            if submission {
                // Submissions sharing the credential take turns; the overall
                // deadline below bounds the wait.
                let mut ownership = crate::coordination::LeaseSession::acquire_waiting(
                    self.inner.coordinator.clone(),
                    &format!("execution:{}", self.inner.quota_key),
                    self.inner.config.timeout,
                )
                .await?;
                ownership.run(operation).await
            } else {
                operation.await
            }
        };
        let result = tokio::time::timeout_at(deadline, guarded)
            .await
            .unwrap_or_else(|_| {
                // A request in flight when the deadline fell went
                // unanswered: that is the breaker's business, though the
                // dropped call never told it.
                if sent.load(std::sync::atomic::Ordering::Relaxed) && !protective {
                    self.inner.circuit_breaker.record_unanswered();
                }
                Err(Error::Timeout {
                    duration: self.inner.config.timeout.as_secs(),
                })
            });
        let result = result.map_err(|e| {
            let ambiguous = sent.load(std::sync::atomic::Ordering::Relaxed)
                && ((e.is_retryable() && !matches!(e, Error::RateLimit { .. }))
                    || matches!(e, Error::OwnershipLost));
            if submission && ambiguous {
                metrics::counter!(crate::metrics::SUBMISSION_UNKNOWN_TOTAL).increment(1);
                Error::SubmissionUnknown {
                    attempt_id,
                    source: Box::new(e),
                }
            } else {
                e
            }
        });
        let status = match &result {
            Ok(_) => "2xx",
            Err(e) => e
                .http_status()
                .map(|s| crate::metrics::status_class(s.as_u16()))
                .unwrap_or("error"),
        };
        metrics::counter!(
            crate::metrics::HTTP_REQUESTS_TOTAL,
            "op" => op,
            "method" => method_label,
            "status" => status
        )
        .increment(1);
        metrics::histogram!(
            crate::metrics::HTTP_REQUEST_DURATION,
            "op" => op,
            "method" => method_label
        )
        .record(start.elapsed().as_secs_f64());
        result
    }

    // ==================== Market Data Endpoints ====================

    /// Get quotes for multiple symbols
    pub async fn get_quotes(&self, symbols: &[&str]) -> Result<QuotesResponse> {
        self.get_quotes_with_options(symbols, &[], None).await
    }

    /// Get quotes for multiple symbols with advanced options.
    ///
    /// An empty `fields` slice leaves the parameter off, which Schwab
    /// treats as [`QuoteField::All`].
    pub async fn get_quotes_with_options(
        &self,
        symbols: &[&str],
        fields: &[QuoteField],
        indicative: Option<bool>,
    ) -> Result<QuotesResponse> {
        if symbols.is_empty() {
            return Err(Error::InvalidParameter("No symbols provided".to_string()));
        }
        if let Some(symbol) = symbols.iter().find(|s| s.trim().is_empty()) {
            return Err(Error::InvalidParameter(format!(
                "Blank symbol {symbol:?} in request"
            )));
        }
        let mut params = vec![("symbols", symbols.join(","))];
        if let Some(fields) = quote_fields_csv(fields) {
            params.push(("fields", fields));
        }
        if let Some(i) = indicative {
            params.push(("indicative", i.to_string()));
        }
        self.request_with_query(
            "get_quotes_with_options",
            Method::GET,
            "/marketdata/v1/quotes",
            &params,
        )
        .await
    }

    /// Get quote for a single symbol.
    ///
    /// Like `/quotes`, the single-symbol endpoint returns a response keyed
    /// by symbol — `{"AAPL": {...}}` — so we fetch the map and extract the
    /// requested entry.
    pub async fn get_quote(&self, symbol: &str) -> Result<QuoteItem> {
        self.get_quote_with_options(symbol, &[]).await
    }

    /// Get quote for a single symbol with advanced options.
    ///
    /// An empty `fields` slice leaves the parameter off, which Schwab
    /// treats as [`QuoteField::All`].
    pub async fn get_quote_with_options(
        &self,
        symbol: &str,
        fields: &[QuoteField],
    ) -> Result<QuoteItem> {
        if symbol.trim().is_empty() {
            return Err(Error::InvalidParameter("symbol is required".into()));
        }
        let mut params = Vec::new();
        if let Some(fields) = quote_fields_csv(fields) {
            params.push(("fields", fields));
        }
        let encoded_symbol = urlencoding::encode(symbol);
        let response: QuotesResponse = self
            .request_with_query(
                "get_quote_with_options",
                Method::GET,
                &format!("/marketdata/v1/{}/quotes", encoded_symbol),
                &params,
            )
            .await?;

        // The requested symbol should be the only key; fall back to the
        // first entry in case Schwab normalizes the symbol spelling.
        response
            .quotes
            .get(symbol)
            .cloned()
            .or_else(|| response.quotes.into_values().next())
            .ok_or_else(|| Error::Api {
                code: "NOT_FOUND".to_string(),
                message: format!("no quote returned for symbol {symbol}"),
            })
    }

    /// Get price history for a symbol.
    ///
    /// Rejects period/frequency combinations Schwab does not accept before
    /// the request is sent; see [`PriceHistoryParams::validate`].
    pub async fn get_price_history(
        &self,
        params: &PriceHistoryParams,
    ) -> Result<PriceHistoryResponse> {
        params.validate()?;
        self.request_with_query(
            "get_price_history",
            Method::GET,
            "/marketdata/v1/pricehistory",
            params,
        )
        .await
    }

    /// Get option chain for a symbol.
    ///
    /// Rejects out-of-range parameters before the request is sent; see
    /// [`OptionChainParams::validate`].
    pub async fn get_option_chain(
        &self,
        params: &OptionChainParams,
    ) -> Result<OptionChainResponse> {
        params.validate()?;
        self.request_with_query(
            "get_option_chain",
            Method::GET,
            "/marketdata/v1/chains",
            params,
        )
        .await
    }

    /// Get option expiration dates for a symbol
    pub async fn get_option_expiration_chain(
        &self,
        symbol: &str,
    ) -> Result<ExpirationChainResponse> {
        if symbol.trim().is_empty() {
            return Err(Error::InvalidParameter("symbol is required".into()));
        }
        let params = [("symbol", symbol)];
        self.request_with_query(
            "get_option_expiration_chain",
            Method::GET,
            "/marketdata/v1/expirationchain",
            &params,
        )
        .await
    }

    /// Get market movers for an index or screener universe.
    pub async fn get_movers(
        &self,
        index: MoverIndex,
        sort: Option<MoverSort>,
        frequency: Option<MoverFrequency>,
    ) -> Result<MoversResponse> {
        let mut params = Vec::new();
        if let Some(sort) = sort {
            params.push(("sort", sort.as_str().to_owned()));
        }
        if let Some(frequency) = frequency {
            params.push(("frequency", frequency.as_minutes().to_string()));
        }

        let encoded_index = urlencoding::encode(index.as_str());
        self.request_with_query(
            "get_movers",
            Method::GET,
            &format!("/marketdata/v1/movers/{}", encoded_index),
            &params,
        )
        .await
    }

    /// Search for instruments
    pub async fn search_instruments(
        &self,
        symbol: &str,
        projection: Projection,
    ) -> Result<InstrumentsResponse> {
        if symbol.trim().is_empty() {
            return Err(Error::InvalidParameter("symbol is required".into()));
        }
        let params = [("symbol", symbol), ("projection", projection.as_str())];
        self.request_with_query(
            "search_instruments",
            Method::GET,
            "/marketdata/v1/instruments",
            &params,
        )
        .await
    }

    /// Get instrument details by CUSIP
    pub async fn get_instrument(&self, cusip: &str) -> Result<Instrument> {
        if cusip.trim().is_empty() {
            return Err(Error::InvalidParameter("cusip is required".into()));
        }
        let encoded_cusip = urlencoding::encode(cusip);
        let response: InstrumentsResponse = self
            .request(
                "get_instrument",
                Method::GET,
                &format!("/marketdata/v1/instruments/{}", encoded_cusip),
            )
            .await?;
        response
            .instruments
            .into_iter()
            .next()
            .ok_or_else(|| Error::Api {
                code: "NOT_FOUND".into(),
                message: "No instrument returned".into(),
            })
    }

    /// Get market hours for multiple markets.
    ///
    /// Schwab nests the response two levels deep — market type to product
    /// code to hours: `{"equity": {"EQ": {...}}}`.
    pub async fn get_markets(
        &self,
        markets: &[Market],
        date: Option<NaiveDate>,
    ) -> Result<MarketsHoursResponse> {
        if markets.is_empty() {
            return Err(Error::InvalidParameter("No markets provided".into()));
        }
        let markets = markets
            .iter()
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let mut params = vec![("markets", markets)];
        if let Some(date) = date {
            params.push(("date", date.to_string()));
        }
        self.request_with_query(
            "get_markets",
            Method::GET,
            "/marketdata/v1/markets",
            &params,
        )
        .await
    }

    /// Get market hours for a single market.
    ///
    /// The Schwab `/markets/{marketId}` endpoint returns the same nested
    /// shape as `/markets` — market type to product code to hours — with
    /// just the requested market populated.
    pub async fn get_market(
        &self,
        market: Market,
        date: Option<NaiveDate>,
    ) -> Result<MarketsHoursResponse> {
        let mut params = Vec::new();
        if let Some(date) = date {
            params.push(("date", date.to_string()));
        }
        self.request_with_query(
            "get_market",
            Method::GET,
            &format!("/marketdata/v1/markets/{}", market.as_str()),
            &params,
        )
        .await
    }

    // ==================== Account & Trading Endpoints ====================

    /// Get all linked account numbers
    pub async fn get_account_numbers(&self) -> Result<Vec<AccountNumberHash>> {
        self.request(
            "get_account_numbers",
            Method::GET,
            "/trader/v1/accounts/accountNumbers",
        )
        .await
    }

    /// Get all account details
    pub async fn get_accounts(&self, include_positions: bool) -> Result<Vec<Account>> {
        let params = if include_positions {
            vec![("fields", "positions")]
        } else {
            vec![]
        };
        self.request_with_query("get_accounts", Method::GET, "/trader/v1/accounts", &params)
            .await
    }

    /// Get specific account details
    pub async fn get_account(
        &self,
        account_hash: &str,
        include_positions: bool,
    ) -> Result<Account> {
        let params = if include_positions {
            vec![("fields", "positions")]
        } else {
            vec![]
        };
        self.request_with_query(
            "get_account",
            Method::GET,
            &account_path(account_hash, "")?,
            &params,
        )
        .await
    }

    /// Get orders for an account.
    ///
    /// Rejects an inverted window or a zero page size before the request is
    /// sent; see [`OrdersParams::validate`].
    pub async fn get_account_orders(
        &self,
        account_hash: &str,
        params: &OrdersParams,
    ) -> Result<Vec<Order>> {
        params.validate()?;
        self.request_with_query(
            "get_account_orders",
            Method::GET,
            &account_path(account_hash, "/orders")?,
            params,
        )
        .await
    }

    /// Place an order.
    ///
    /// On success Schwab returns 201 with an empty body; the new order's ID
    /// comes back in the `Location` header (see [`OrderSubmitResponse`]).
    pub async fn place_order(
        &self,
        account_hash: &str,
        order: &OrderRequest,
    ) -> Result<OrderSubmitResponse> {
        self.submit_order_request(
            "place_order",
            Method::POST,
            &account_path(account_hash, "/orders")?,
            order,
            None,
        )
        .await
    }

    /// Dispatch through a gateway-owned lease, checked after queue/auth waits.
    pub async fn place_order_fenced(
        &self,
        account: &str,
        order: &OrderRequest,
        lease: &crate::coordination::Lease,
    ) -> Result<OrderSubmitResponse> {
        self.submit_order_request(
            "place_order",
            Method::POST,
            &account_path(account, "/orders")?,
            order,
            Some(lease),
        )
        .await
    }
    pub async fn replace_order_fenced(
        &self,
        account: &str,
        id: &str,
        order: &OrderRequest,
        lease: &crate::coordination::Lease,
    ) -> Result<OrderSubmitResponse> {
        self.submit_order_request(
            "replace_order",
            Method::PUT,
            &order_path(account, id)?,
            order,
            Some(lease),
        )
        .await
    }

    /// Get a specific order
    pub async fn get_order(&self, account_hash: &str, order_id: &str) -> Result<Order> {
        self.request(
            "get_order",
            Method::GET,
            &order_path(account_hash, order_id)?,
        )
        .await
    }

    /// Cancel an order.
    ///
    /// Unlike placement, a cancellation creates no exposure and asking twice
    /// is harmless, so it is retried like a read, is allowed while
    /// submissions are halted, and opens no intent in the execution journal.
    /// Record who cancelled what in the calling layer: the MCP server and the
    /// authority audit every cancellation per principal.
    pub async fn cancel_order(&self, account_hash: &str, order_id: &str) -> Result<()> {
        self.request(
            "cancel_order",
            Method::DELETE,
            &order_path(account_hash, order_id)?,
        )
        .await
    }

    /// Replace an order.
    ///
    /// Schwab atomically cancels the old order and creates a new one; like
    /// [`place_order`](Self::place_order), the new order's ID is returned
    /// via the `Location` header.
    pub async fn replace_order(
        &self,
        account_hash: &str,
        order_id: &str,
        order: &OrderRequest,
    ) -> Result<OrderSubmitResponse> {
        self.submit_order_request(
            "replace_order",
            Method::PUT,
            &order_path(account_hash, order_id)?,
            order,
            None,
        )
        .await
    }

    /// Preview an order without submitting it.
    ///
    /// Returns the raw JSON validation result (fees, buying-power impact,
    /// warnings/rejects) — the shape varies by order type and account, so
    /// it is not modeled as a fixed struct.
    pub async fn preview_order(
        &self,
        account_hash: &str,
        order: &OrderRequest,
    ) -> Result<serde_json::Value> {
        order.validate()?;
        self.request_with_body(
            "preview_order",
            Method::POST,
            &account_path(account_hash, "/previewOrder")?,
            order,
        )
        .await
    }

    /// Get all orders across all accounts.
    ///
    /// Rejects an inverted window or a zero page size before the request is
    /// sent; see [`OrdersParams::validate`].
    pub async fn get_all_orders(&self, params: &OrdersParams) -> Result<Vec<Order>> {
        params.validate()?;
        self.request_with_query("get_all_orders", Method::GET, "/trader/v1/orders", params)
            .await
    }

    /// Get account transactions.
    ///
    /// Rejects an inverted window or an empty type list before the request
    /// is sent; see [`TransactionsParams::validate`].
    pub async fn get_transactions(
        &self,
        account_hash: &str,
        params: &TransactionsParams,
    ) -> Result<Vec<Transaction>> {
        params.validate()?;
        self.request_with_query(
            "get_transactions",
            Method::GET,
            &account_path(account_hash, "/transactions")?,
            params,
        )
        .await
    }

    /// Get a specific transaction
    pub async fn get_transaction(
        &self,
        account_hash: &str,
        transaction_id: &str,
    ) -> Result<Transaction> {
        path_segment("transaction_id", transaction_id)?;
        self.request(
            "get_transaction",
            Method::GET,
            &account_path(account_hash, &format!("/transactions/{transaction_id}"))?,
        )
        .await
    }

    /// Get user preferences
    pub async fn get_user_preferences(&self) -> Result<UserPreferences> {
        self.request(
            "get_user_preferences",
            Method::GET,
            "/trader/v1/userPreference",
        )
        .await
    }
}

/// Comma-separated `fields` value, or `None` when the caller wants Schwab's
/// default (every section).
fn quote_fields_csv(fields: &[QuoteField]) -> Option<String> {
    if fields.is_empty() {
        return None;
    }
    Some(
        fields
            .iter()
            .map(|f| f.as_str())
            .collect::<Vec<_>>()
            .join(","),
    )
}

/// Reject values that cannot be a Schwab identifier before they reach a
/// URL path. Schwab identifiers are URL-safe (`[A-Za-z0-9_-]`), so anything
/// else is a caller bug that would otherwise surface as an opaque 4xx or,
/// for an embedded `/`, as a request for a different resource.
pub(crate) fn path_segment(name: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(Error::InvalidParameter(format!("{name} must not be empty")));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(Error::InvalidParameter(format!(
            "{name} contains characters that are not valid in a Schwab identifier"
        )));
    }
    Ok(())
}

/// Build a `/trader/v1/accounts/{account_hash}{suffix}` path.
///
/// Besides the [`path_segment`] rules, an all-digit value is refused: that
/// is an account *number*, and Schwab expects the opaque `hashValue` from
/// `get_account_numbers` here.
pub(crate) fn account_path(account_hash: &str, suffix: &str) -> Result<String> {
    path_segment("account_hash", account_hash)?;
    if account_hash.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::InvalidParameter(
            "account_hash looks like an account number; use the hashValue from get_account_numbers"
                .into(),
        ));
    }
    Ok(format!("/trader/v1/accounts/{account_hash}{suffix}"))
}

/// Build a `/trader/v1/accounts/{account_hash}/orders/{order_id}` path.
pub(crate) fn order_path(account_hash: &str, order_id: &str) -> Result<String> {
    path_segment("order_id", order_id)?;
    account_path(account_hash, &format!("/orders/{order_id}"))
}

/// Extract the order ID from a `Location` header value — the last
/// non-empty path segment of the order resource URL.
fn order_id_from_location(location: &str) -> Option<String> {
    location
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Static label for the request method, used in metrics.
fn method_label(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::PATCH => "PATCH",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        _ => "OTHER",
    }
}

#[derive(Debug, Clone)]
pub struct SchwabClientBuilder {
    config: Option<SchwabConfig>,
}

impl SchwabClientBuilder {
    pub fn new() -> Self {
        Self { config: None }
    }

    pub fn config(mut self, config: SchwabConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn build(self) -> Result<SchwabClient> {
        let config = self
            .config
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_path_accepts_hashes_and_rejects_numbers_and_separators() {
        assert_eq!(
            account_path("A1B2-c3_D4", "/orders").unwrap(),
            "/trader/v1/accounts/A1B2-c3_D4/orders"
        );
        for bad in ["", "12345678", "abc/def", "abc?x=1", "a b", "héllo"] {
            let err = account_path(bad, "").unwrap_err();
            assert!(
                matches!(err, Error::InvalidParameter(_)),
                "{bad:?} produced {err:?}"
            );
        }
        assert!(
            account_path("12345678", "")
                .unwrap_err()
                .to_string()
                .contains("account number")
        );
    }

    #[test]
    fn order_path_validates_both_segments() {
        assert_eq!(
            order_path("hash", "123456").unwrap(),
            "/trader/v1/accounts/hash/orders/123456"
        );
        assert!(order_path("hash", "").is_err());
        assert!(order_path("hash", "1/2").is_err());
        assert!(order_path("", "1").is_err());
    }

    #[test]
    fn quote_fields_csv_omits_empty_and_joins_wire_names() {
        assert_eq!(quote_fields_csv(&[]), None);
        assert_eq!(
            quote_fields_csv(&[QuoteField::Quote, QuoteField::Fundamental]).as_deref(),
            Some("quote,fundamental")
        );
    }

    #[test]
    fn order_id_from_location_parses_last_segment() {
        assert_eq!(
            super::order_id_from_location(
                "https://api.schwabapi.com/trader/v1/accounts/ABC123/orders/456789"
            ),
            Some("456789".to_string())
        );
        assert_eq!(
            super::order_id_from_location(
                "https://api.schwabapi.com/trader/v1/accounts/ABC123/orders/456789/"
            ),
            Some("456789".to_string())
        );
        assert_eq!(super::order_id_from_location(""), None);
    }
}
