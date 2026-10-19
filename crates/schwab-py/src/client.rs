//! `rustyschwab.Client`: the Schwab REST API as asyncio coroutines.
//!
//! Requests and responses are plain Python data in Schwab's JSON shape. Every
//! method is a coroutine that runs on the shared tokio runtime; none of them
//! holds the GIL while it waits. Orders go through the SDK's journaled
//! submission path, so risk checks, the operator halt, the request budget and
//! the unknown-outcome bookkeeping all apply exactly as in Rust.

use crate::config::{Base, schwab_config, stream_config};
use crate::convert::{Json, from_py, from_value, name_of, to_json};
use crate::errors::Failure;
use crate::runtime::{awaitable, enter};
use crate::stream::Stream;
use crate::submission::{Submitted, journal_window_start, live_attempt, tagged};
use chrono::NaiveDate;
use pyo3::prelude::*;
use rustyschwab::client::{OptionChainParams, PriceHistoryParams};
use rustyschwab::types::market_data::{
    Market, MoverFrequency, MoverIndex, MoverSort, Projection, QuoteField,
};
use rustyschwab::types::trading::{OrderRequest, OrdersParams, TransactionsParams};
use rustyschwab::{Error, SchwabClient, StreamConfig};
use serde_json::Value;
use std::future::Future;
use std::sync::Arc;

#[derive(Debug)]
pub(crate) struct Shared {
    sdk: SchwabClient,
    streaming: Option<StreamConfig>,
    initialized: tokio::sync::OnceCell<()>,
}

impl Shared {
    /// The SDK client, once its token store is loaded. Every call starts
    /// here, so `Client` works without an explicit `init()`.
    pub(crate) async fn ready(&self) -> Result<&SchwabClient, Failure> {
        self.initialized.get_or_try_init(|| self.sdk.init()).await?;
        Ok(&self.sdk)
    }
}

/// An authenticated Schwab client.
///
/// Build one with `Client(config)` or `Client.from_env()`. It needs tokens in
/// the configured token store; see `authorization_url` and
/// `complete_authorization` for creating them.
#[pyclass(frozen, module = "rustyschwab._native")]
#[derive(Debug)]
pub struct Client {
    shared: Arc<Shared>,
}

impl Client {
    fn build(py: Python<'_>, base: Base, config: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        let config = schwab_config(base, config).map_err(|f| crate::errors::to_pyerr(py, f))?;
        let streaming = config.streaming.clone();
        let sdk = py
            .detach(|| {
                let _runtime = enter();
                SchwabClient::new(config)
            })
            .map_err(|e| crate::errors::to_pyerr(py, e.into()))?;
        Ok(Self {
            shared: Arc::new(Shared {
                sdk,
                streaming,
                initialized: tokio::sync::OnceCell::new(),
            }),
        })
    }

    /// Run `work` against the shared state as a coroutine.
    fn run<'py, T, F, Fut>(&self, py: Python<'py>, work: F) -> PyResult<Bound<'py, PyAny>>
    where
        F: FnOnce(Arc<Shared>) -> Fut,
        Fut: Future<Output = Result<T, Failure>> + Send + 'static,
        T: for<'a> IntoPyObject<'a> + Send + 'static,
    {
        awaitable(py, work(self.shared.clone()))
    }

    /// The SDK client behind this one, for other native classes that read
    /// through it.
    pub(crate) fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }
}

/// `names` as the `fields` sections of a quote request.
fn quote_fields(names: Option<Vec<String>>) -> Result<Vec<QuoteField>, Failure> {
    names
        .unwrap_or_default()
        .iter()
        .map(|name| name_of(&name.to_ascii_lowercase(), "quote field"))
        .collect()
}

fn borrowed(symbols: &[String]) -> Vec<&str> {
    symbols.iter().map(String::as_str).collect()
}

#[pymethods]
impl Client {
    /// Build a client from a configuration dict laid over the SDK defaults.
    ///
    /// The dict has the shape of the SDK's `SchwabConfig`: `oauth`, `client`
    /// and `streaming` sections. `oauth.app_key`, `oauth.app_secret` and
    /// `oauth.tokens_file` are the usual minimum. Unknown keys are an error.
    #[new]
    fn new(py: Python<'_>, config: &Bound<'_, PyAny>) -> PyResult<Self> {
        Self::build(py, Base::Defaults, Some(config))
    }

    /// Build a client from the `SCHWAB_*` environment, optionally with
    /// `overrides` (the same dict shape as `Client(config)`) laid on top.
    #[staticmethod]
    #[pyo3(signature = (overrides=None))]
    fn from_env(py: Python<'_>, overrides: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        Self::build(py, Base::Env, overrides)
    }

    fn __repr__(&self) -> String {
        format!(
            "Client(base_url={:?})",
            self.shared.sdk.client_config().base_url
        )
    }

    /// Load the stored tokens and start the refresh task. Idempotent, and
    /// done on first use if you do not call it.
    fn init<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, |shared| async move {
            shared.ready().await?;
            Ok(Json(Value::Null))
        })
    }

    /// Async context manager support: `async with Client.from_env() as client`.
    fn __aenter__<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let shared = slf.get().shared.clone();
        awaitable(py, async move {
            shared.ready().await?;
            Ok(slf)
        })
    }

    #[pyo3(signature = (*_exc))]
    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _exc: &Bound<'py, pyo3::types::PyTuple>,
    ) -> PyResult<Bound<'py, PyAny>> {
        awaitable(py, async { Ok(Json(false)) })
    }

    // ---- authorization ----------------------------------------------------

    /// The Schwab consent URL to open in a browser. Sign in, approve, and
    /// pass the URL Schwab redirects to into `complete_authorization`.
    fn authorization_url<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, |shared| async move {
            let (url, _) = shared
                .sdk
                .auth_manager()
                .authorize()
                .await
                .map_err(Error::Auth)?;
            Ok(Json(url))
        })
    }

    /// Exchange the redirect URL (or its `code` parameter) for tokens and
    /// store them. Needs the `authorization_url` flow to have started in this
    /// process or another one using the same token file.
    fn complete_authorization<'py>(
        &self,
        py: Python<'py>,
        redirect_url_or_code: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, |shared| async move {
            shared
                .sdk
                .auth_manager()
                .exchange_code(redirect_url_or_code)
                .await
                .map_err(Error::Auth)?;
            Ok(Json(Value::Null))
        })
    }

    // ---- market data --------------------------------------------------------

    /// Quotes for several symbols, keyed by symbol.
    ///
    /// `fields` selects sections (`"quote"`, `"fundamental"`, `"extended"`,
    /// `"reference"`, `"regular"`, or `"all"`).
    #[pyo3(signature = (symbols, *, fields=None, indicative=None))]
    fn quotes<'py>(
        &self,
        py: Python<'py>,
        symbols: Vec<String>,
        fields: Option<Vec<String>>,
        indicative: Option<bool>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let fields = quote_fields(fields);
        self.run(py, |shared| async move {
            let fields = fields?;
            let sdk = shared.ready().await?;
            let found = sdk
                .get_quotes_with_options(&borrowed(&symbols), &fields, indicative)
                .await?;
            Ok(Json(found))
        })
    }

    /// The quote for one symbol.
    #[pyo3(signature = (symbol, *, fields=None))]
    fn quote<'py>(
        &self,
        py: Python<'py>,
        symbol: String,
        fields: Option<Vec<String>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let fields = quote_fields(fields);
        self.run(py, |shared| async move {
            let fields = fields?;
            let sdk = shared.ready().await?;
            Ok(Json(sdk.get_quote_with_options(&symbol, &fields).await?))
        })
    }

    /// Candles for a symbol. `params` uses Schwab's query names:
    /// `symbol`, `periodType`, `period`, `frequencyType`, `frequency`,
    /// `startDate` and `endDate` (epoch milliseconds, or an aware
    /// `datetime`), `needExtendedHoursData`,
    /// `needPreviousClose`. Invalid period/frequency pairs are refused before
    /// anything is sent.
    fn price_history<'py>(
        &self,
        py: Python<'py>,
        params: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let params = from_py::<PriceHistoryParams>(params, "price history parameters");
        self.run(py, |shared| async move {
            let params = params?;
            Ok(Json(
                shared.ready().await?.get_price_history(&params).await?,
            ))
        })
    }

    /// An option chain. `params` uses Schwab's query names (`symbol`,
    /// `contractType`, `strikeCount`, `fromDate`, `toDate`, ...).
    fn option_chain<'py>(
        &self,
        py: Python<'py>,
        params: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let params = from_py::<OptionChainParams>(params, "option chain parameters");
        self.run(py, |shared| async move {
            let params = params?;
            Ok(Json(shared.ready().await?.get_option_chain(&params).await?))
        })
    }

    /// The expiration dates available for an underlying.
    fn option_expirations<'py>(
        &self,
        py: Python<'py>,
        symbol: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, |shared| async move {
            let sdk = shared.ready().await?;
            Ok(Json(sdk.get_option_expiration_chain(&symbol).await?))
        })
    }

    /// Market movers for an index or screener (`"$SPX"`, `"NASDAQ"`, ...).
    /// `sort` is `"VOLUME"`, `"TRADES"`, `"PERCENT_CHANGE_UP"` or
    /// `"PERCENT_CHANGE_DOWN"`; `frequency` is a window in minutes.
    #[pyo3(signature = (index, *, sort=None, frequency=None))]
    fn movers<'py>(
        &self,
        py: Python<'py>,
        index: String,
        sort: Option<String>,
        frequency: Option<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let parsed = (|| {
            Ok::<_, Failure>((
                name_of::<MoverIndex>(&index, "movers index")?,
                sort.map(|s| name_of::<MoverSort>(&s, "movers sort"))
                    .transpose()?,
                frequency
                    .map(|f| from_value::<MoverFrequency>(Value::from(f), "movers frequency"))
                    .transpose()?,
            ))
        })();
        self.run(py, |shared| async move {
            let (index, sort, frequency) = parsed?;
            let sdk = shared.ready().await?;
            Ok(Json(sdk.get_movers(index, sort, frequency).await?))
        })
    }

    /// Market hours for `"equity"`, `"option"`, `"bond"`, `"future"` and
    /// `"forex"`, on `date` (a `date` or `"YYYY-MM-DD"`) or today.
    #[pyo3(signature = (markets, *, date=None))]
    fn market_hours<'py>(
        &self,
        py: Python<'py>,
        markets: Vec<String>,
        date: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let parsed = (|| {
            Ok::<_, Failure>((
                markets
                    .iter()
                    .map(|m| name_of::<Market>(&m.to_ascii_lowercase(), "market"))
                    .collect::<Result<Vec<_>, _>>()?,
                date.map(|d| from_value::<NaiveDate>(to_json(d)?, "date"))
                    .transpose()?,
            ))
        })();
        self.run(py, |shared| async move {
            let (markets, date) = parsed?;
            Ok(Json(
                shared.ready().await?.get_markets(&markets, date).await?,
            ))
        })
    }

    /// Instruments matching `symbol`. `projection` is `"symbol-search"`,
    /// `"symbol-regex"`, `"desc-search"`, `"desc-regex"`, `"search"` or
    /// `"fundamental"`.
    #[pyo3(signature = (symbol, projection="symbol-search"))]
    fn instruments<'py>(
        &self,
        py: Python<'py>,
        symbol: String,
        projection: &str,
    ) -> PyResult<Bound<'py, PyAny>> {
        let projection = name_of::<Projection>(projection, "projection");
        self.run(py, |shared| async move {
            let projection = projection?;
            let sdk = shared.ready().await?;
            Ok(Json(sdk.search_instruments(&symbol, projection).await?))
        })
    }

    /// One instrument by CUSIP.
    fn instrument<'py>(&self, py: Python<'py>, cusip: String) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, |shared| async move {
            Ok(Json(shared.ready().await?.get_instrument(&cusip).await?))
        })
    }

    /// `GET` any Schwab path (`"/marketdata/v1/quotes"`) through the same
    /// auth, budget, breaker and retry pipeline, returning the raw JSON.
    /// Reads only: orders have their own methods.
    #[pyo3(signature = (path, query=None))]
    fn get_json<'py>(
        &self,
        py: Python<'py>,
        path: String,
        query: Option<std::collections::BTreeMap<String, String>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, |shared| async move {
            let sdk = shared.ready().await?;
            let query = query.unwrap_or_default();
            let pairs: Vec<(&str, &str)> = query
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            Ok(Json(sdk.get_json(&path, &pairs).await?))
        })
    }

    // ---- accounts ---------------------------------------------------------

    /// The linked accounts' numbers and the `hashValue` every other call uses.
    fn account_numbers<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, |shared| async move {
            Ok(Json(shared.ready().await?.get_account_numbers().await?))
        })
    }

    /// Every linked account, with positions when asked.
    #[pyo3(signature = (*, include_positions=false))]
    fn accounts<'py>(
        &self,
        py: Python<'py>,
        include_positions: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, |shared| async move {
            Ok(Json(
                shared
                    .ready()
                    .await?
                    .get_accounts(include_positions)
                    .await?,
            ))
        })
    }

    /// One account by its hash, with positions when asked.
    #[pyo3(signature = (account_hash, *, include_positions=false))]
    fn account<'py>(
        &self,
        py: Python<'py>,
        account_hash: String,
        include_positions: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, |shared| async move {
            let sdk = shared.ready().await?;
            Ok(Json(
                sdk.get_account(&account_hash, include_positions).await?,
            ))
        })
    }

    /// The user's preferences, including the streamer connection details.
    fn user_preferences<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, |shared| async move {
            Ok(Json(shared.ready().await?.get_user_preferences().await?))
        })
    }

    // ---- orders -----------------------------------------------------------

    /// Orders on one account. `params` needs `fromEnteredTime` and
    /// `toEnteredTime` (timezone-aware `datetime` or RFC 3339 strings) and
    /// takes `maxResults` and `status`. Schwab lists only the last 60 days.
    fn account_orders<'py>(
        &self,
        py: Python<'py>,
        account_hash: String,
        params: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let params = from_py::<OrdersParams>(params, "orders parameters");
        self.run(py, |shared| async move {
            let params = params?;
            let sdk = shared.ready().await?;
            Ok(Json(sdk.get_account_orders(&account_hash, &params).await?))
        })
    }

    /// Orders across all linked accounts; `params` as for `account_orders`.
    fn all_orders<'py>(
        &self,
        py: Python<'py>,
        params: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let params = from_py::<OrdersParams>(params, "orders parameters");
        self.run(py, |shared| async move {
            let params = params?;
            Ok(Json(shared.ready().await?.get_all_orders(&params).await?))
        })
    }

    /// One order.
    fn order<'py>(
        &self,
        py: Python<'py>,
        account_hash: String,
        order_id: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, |shared| async move {
            let sdk = shared.ready().await?;
            Ok(Json(sdk.get_order(&account_hash, &order_id).await?))
        })
    }

    /// Transactions on one account. `params` needs `startDate`, `endDate` and
    /// `types` (a list of transaction types) and takes `symbol`.
    fn transactions<'py>(
        &self,
        py: Python<'py>,
        account_hash: String,
        params: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let params = from_py::<TransactionsParams>(params, "transactions parameters");
        self.run(py, |shared| async move {
            let params = params?;
            let sdk = shared.ready().await?;
            Ok(Json(sdk.get_transactions(&account_hash, &params).await?))
        })
    }

    /// One transaction.
    fn transaction<'py>(
        &self,
        py: Python<'py>,
        account_hash: String,
        transaction_id: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, |shared| async move {
            let sdk = shared.ready().await?;
            Ok(Json(
                sdk.get_transaction(&account_hash, &transaction_id).await?,
            ))
        })
    }

    /// Schwab's preview of an order: validation findings, projected balances
    /// and fees. Nothing is placed. A preview Schwab would refuse still
    /// returns; look for rejections in the result.
    fn preview_order<'py>(
        &self,
        py: Python<'py>,
        account_hash: String,
        order: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let order = from_py::<OrderRequest>(order, "order");
        self.run(py, |shared| async move {
            let order = order?;
            let sdk = shared.ready().await?;
            Ok(Json(sdk.preview_order(&account_hash, &order).await?))
        })
    }

    /// Place an order (Schwab's JSON shape) through the journaled path.
    ///
    /// Returns `{"order_id", "location", "attempt_id", "tag"}`. If the outcome
    /// is uncertain this raises `SubmissionUnknown` with the same
    /// `attempt_id`; reconcile before sending the order again.
    fn place_order<'py>(
        &self,
        py: Python<'py>,
        account_hash: String,
        order: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let order = from_py::<OrderRequest>(order, "order");
        self.run(py, |shared| async move {
            let (order, tag) = tagged(order?);
            let sdk = shared.ready().await?;
            let since = journal_window_start();
            let response = sdk.place_order(&account_hash, &order).await?;
            let attempt = live_attempt(sdk, &account_hash, "place_order", &tag, since).await;
            Ok(Json(Submitted::new(response, tag, attempt)))
        })
    }

    /// Replace a working order; the result is as for `place_order`. Schwab
    /// cancels the old order and creates a new one with a new ID.
    fn replace_order<'py>(
        &self,
        py: Python<'py>,
        account_hash: String,
        order_id: String,
        order: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let order = from_py::<OrderRequest>(order, "order");
        self.run(py, |shared| async move {
            let (order, tag) = tagged(order?);
            let sdk = shared.ready().await?;
            let since = journal_window_start();
            let response = sdk.replace_order(&account_hash, &order_id, &order).await?;
            let attempt = live_attempt(sdk, &account_hash, "replace_order", &tag, since).await;
            Ok(Json(Submitted::new(response, tag, attempt)))
        })
    }

    /// Cancel an order. Allowed while submissions are halted.
    fn cancel_order<'py>(
        &self,
        py: Python<'py>,
        account_hash: String,
        order_id: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, |shared| async move {
            let sdk = shared.ready().await?;
            sdk.cancel_order(&account_hash, &order_id).await?;
            Ok(Json(Value::Null))
        })
    }

    // ---- state and controls ----------------------------------------------------

    /// This process's use of the request budget as
    /// `{"perMinute", "usedLastMinute", "remaining"}`, or `None` when rate
    /// limiting is off.
    fn request_budget(&self) -> Json<Option<rustyschwab::RequestBudgetUsage>> {
        Json(self.shared.sdk.request_budget())
    }

    /// The circuit breaker: `"closed"`, `"open"` or `"half_open"`.
    fn circuit_state(&self) -> Json<rustyschwab::CircuitState> {
        Json(self.shared.sdk.circuit_state())
    }

    /// Whether reads and trading are ready, from local evidence only (tokens,
    /// breaker, pending submissions, reconciled accounts, halts). Returns the
    /// SDK's `Readiness` object. `accounts` are the account hashes to judge;
    /// a baseline older than `max_age_ms` counts as unreconciled.
    #[pyo3(signature = (accounts, *, max_age_ms=60_000))]
    fn readiness<'py>(
        &self,
        py: Python<'py>,
        accounts: Vec<String>,
        max_age_ms: i64,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, |shared| async move {
            let sdk = shared.ready().await?;
            Ok(Json(sdk.readiness(&accounts, max_age_ms).await?))
        })
    }

    /// Halt new order submissions for every client sharing this journal.
    /// Cancellations still work. `operator` is recorded in the audit trail.
    fn halt<'py>(
        &self,
        py: Python<'py>,
        reason: String,
        operator: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, |shared| async move {
            let sdk = shared.ready().await?;
            sdk.halt_submissions(&reason, &operator).await?;
            Ok(Json(Value::Null))
        })
    }

    /// Lift the operator halt.
    fn clear_halt<'py>(&self, py: Python<'py>, operator: String) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, |shared| async move {
            let sdk = shared.ready().await?;
            sdk.clear_operator_halt(&operator).await?;
            Ok(Json(Value::Null))
        })
    }

    /// The reason submissions are halted, or `None`.
    fn halted<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, |shared| async move {
            Ok(Json(shared.ready().await?.operator_halt().await?))
        })
    }

    // ---- streaming --------------------------------------------------------

    /// Connect to the streamer and return a `Stream`.
    ///
    /// The login identity and socket URL come from `user_preferences`.
    /// `overrides` adjusts the streaming configuration (same keys as the
    /// `streaming` section of the client config). With `named_fields` (the
    /// default) data fields arrive under their names (`bidPrice`) instead of
    /// the streamer's field numbers.
    #[pyo3(signature = (overrides=None, *, named_fields=true))]
    fn open_stream<'py>(
        &self,
        py: Python<'py>,
        overrides: Option<&Bound<'py, PyAny>>,
        named_fields: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let base = self.shared.streaming.clone().unwrap_or_default();
        let config = stream_config(&base, overrides);
        let enabled = self.shared.streaming.is_some() || overrides.is_some();
        self.run(py, |shared| async move {
            let config = config?;
            if !enabled {
                return Err(Error::Config(
                    "streaming is disabled in this client's configuration".into(),
                )
                .into());
            }
            let sdk = shared.ready().await?;
            Stream::connect(sdk, config, named_fields).await
        })
    }
}
