//! The execution backend behind every tool: live Schwab, or the paper venue.
//!
//! Tools ask the backend for accounts, orders and quotes instead of holding a
//! `SchwabClient`, so a paper server answers them from the venue's durable
//! state and never reaches a Schwab trading endpoint. A paper backend may
//! carry a read-only Schwab client for market data; it is never asked for an
//! account or an order.

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use schwab_rs::{
    CircuitState, Error, RequestBudgetUsage, SchwabClient,
    broker::TradingBackend,
    paper::{PaperBroker, PaperFill, PaperOrder, PaperSnapshot},
    readiness::Readiness,
};
use schwab_types::{
    accounts::{Account, AccountNumberHash},
    market_data::{QuoteField, QuoteItem, QuotesResponse},
    trading::{Order, OrderRequest, OrderStatus, OrderSubmitResponse, OrdersParams},
};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::contract::{Source, ToolFailure};
use crate::server;

/// Where orders go and where account state comes from.
#[derive(Clone)]
pub enum Backend {
    Live(Arc<SchwabClient>),
    Paper(Paper),
}

/// The paper venue, with Schwab market data when the operator configured
/// credentials for it. Without market data the venue's own quotes are the
/// only prices, and the market-data tools beyond quotes are unavailable.
#[derive(Clone)]
pub struct Paper {
    pub broker: Arc<PaperBroker>,
    pub market_data: Option<Arc<SchwabClient>>,
}

/// Per-account paper figures that Schwab's account shape has no field for.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaperLedger {
    pub realized_pnl: f64,
    pub fees: f64,
    pub model_version: String,
    /// Venue time of the read: replay time in replay mode, wall time otherwise.
    pub at_ms: i64,
    pub event_sequence: u64,
}

impl Backend {
    /// `live` or `paper`; what `get_server_status` and every mutation report.
    pub fn mode(&self) -> &'static str {
        self.trading().mode()
    }

    pub fn trading(&self) -> &dyn TradingBackend {
        match self {
            Self::Live(client) => client.as_ref(),
            Self::Paper(paper) => paper.broker.as_ref(),
        }
    }

    /// Reads served by the backend itself carry this source.
    pub fn source(&self) -> Source {
        match self {
            Self::Live(_) => Source::SchwabRest,
            Self::Paper(_) => Source::Paper,
        }
    }

    /// The Schwab client for market data, streaming and anything else the
    /// paper venue does not model. In paper mode it is the optional
    /// read-only client.
    pub fn market_data(&self, what: &str) -> Result<&Arc<SchwabClient>, ToolFailure> {
        match self {
            Self::Live(client) => Ok(client),
            Self::Paper(Paper {
                market_data: Some(client),
                ..
            }) => Ok(client),
            Self::Paper(_) => Err(ToolFailure::not_permitted(format!(
                "{what} is not available on the paper backend without Schwab market data; \
                 the operator must configure read-only Schwab credentials for it"
            ))),
        }
    }

    /// What the paper venue does not model at all.
    fn paper_unavailable(what: &str, instead: &str) -> ToolFailure {
        ToolFailure::not_permitted(format!(
            "{what} is not available on the paper backend; {instead}"
        ))
    }

    // ---- account reads ---------------------------------------------------

    pub async fn account_numbers(&self) -> Result<Vec<AccountNumberHash>, ToolFailure> {
        match self {
            Self::Live(client) => Ok(client.get_account_numbers().await?),
            Self::Paper(paper) => Ok(paper
                .accounts()
                .map(|name| AccountNumberHash {
                    account_number: name.clone(),
                    hash_value: name.clone(),
                })
                .collect()),
        }
    }

    pub async fn accounts(&self, include_positions: bool) -> Result<Vec<Account>, ToolFailure> {
        match self {
            Self::Live(client) => Ok(client.get_accounts(include_positions).await?),
            Self::Paper(paper) => {
                let mut accounts = Vec::new();
                for name in paper.accounts() {
                    accounts.push(paper.account(name, include_positions).await?);
                }
                Ok(accounts)
            }
        }
    }

    pub async fn account(
        &self,
        account_hash: &str,
        include_positions: bool,
    ) -> Result<Account, ToolFailure> {
        match self {
            Self::Live(client) => Ok(client.get_account(account_hash, include_positions).await?),
            Self::Paper(paper) => paper.account(account_hash, include_positions).await,
        }
    }

    /// The venue's per-account figures; none on the live backend.
    pub async fn paper_ledger(
        &self,
        account_hash: &str,
    ) -> Result<Option<PaperLedger>, ToolFailure> {
        match self {
            Self::Live(_) => Ok(None),
            Self::Paper(paper) => {
                let snapshot = paper.broker.snapshot(account_hash).await?;
                Ok(Some(PaperLedger {
                    realized_pnl: money(snapshot.ledger.realized_pnl),
                    fees: money(snapshot.ledger.fees),
                    model_version: snapshot.model_version,
                    at_ms: snapshot.at_ms,
                    event_sequence: snapshot.event_sequence,
                }))
            }
        }
    }

    // ---- order reads -----------------------------------------------------

    pub async fn order(&self, account_hash: &str, order_id: &str) -> Result<Order, ToolFailure> {
        match self {
            Self::Live(client) => Ok(client.get_order(account_hash, order_id).await?),
            Self::Paper(paper) => {
                let order = paper.broker.get_order(account_hash, order_id).await?;
                paper.order(order).await
            }
        }
    }

    pub async fn account_orders(
        &self,
        account_hash: &str,
        params: &OrdersParams,
    ) -> Result<Vec<Order>, ToolFailure> {
        match self {
            Self::Live(client) => Ok(client.get_account_orders(account_hash, params).await?),
            Self::Paper(paper) => paper.orders(account_hash, params).await,
        }
    }

    pub async fn all_orders(&self, params: &OrdersParams) -> Result<Vec<Order>, ToolFailure> {
        match self {
            Self::Live(client) => Ok(client.get_all_orders(params).await?),
            Self::Paper(paper) => {
                let mut orders = Vec::new();
                for name in paper.accounts() {
                    orders.extend(paper.orders(name, params).await?);
                }
                Ok(orders)
            }
        }
    }

    // ---- quotes ----------------------------------------------------------

    /// Live: Schwab's quotes. Paper: the venue's, the ones fills are judged
    /// against, with `fresh` telling whether an order would fill against
    /// them now.
    pub async fn quotes(
        &self,
        symbols: &[&str],
        fields: &[QuoteField],
        indicative: Option<bool>,
    ) -> Result<QuotesResponse, ToolFailure> {
        match self {
            Self::Live(client) => Ok(client
                .get_quotes_with_options(symbols, fields, indicative)
                .await?),
            Self::Paper(paper) => {
                let mut quotes = Map::new();
                let mut invalid = Vec::new();
                for symbol in symbols {
                    match paper.broker.quote(symbol).await? {
                        Some(quote) => {
                            quotes.insert(symbol.to_string(), paper_quote(quote));
                        }
                        None => invalid.push(symbol.to_string()),
                    }
                }
                if !invalid.is_empty() {
                    quotes.insert("errors".into(), json!({"invalidSymbols": invalid}));
                }
                shaped("paper quotes", Value::Object(quotes))
            }
        }
    }

    pub async fn quote(
        &self,
        symbol: &str,
        fields: &[QuoteField],
    ) -> Result<QuoteItem, ToolFailure> {
        match self {
            Self::Live(client) => Ok(client.get_quote_with_options(symbol, fields).await?),
            Self::Paper(paper) => match paper.broker.quote(symbol).await? {
                Some(quote) => shaped("paper quote", paper_quote(quote)),
                None => Err(ToolFailure::new(
                    crate::contract::FailureCode::NotFound,
                    format!("The paper venue has never seen a quote for {symbol}"),
                )),
            },
        }
    }

    // ---- what the venue does not model -----------------------------------

    pub async fn user_preferences(
        &self,
    ) -> Result<schwab_types::accounts::UserPreferences, ToolFailure> {
        match self {
            Self::Live(client) => Ok(client.get_user_preferences().await?),
            Self::Paper(_) => Err(Self::paper_unavailable(
                "get_user_preferences",
                "paper accounts are listed by get_account_numbers",
            )),
        }
    }

    pub async fn transactions(
        &self,
        account_hash: &str,
        params: &schwab_types::trading::TransactionsParams,
    ) -> Result<Vec<schwab_types::trading::Transaction>, ToolFailure> {
        match self {
            Self::Live(client) => Ok(client.get_transactions(account_hash, params).await?),
            Self::Paper(_) => Err(Self::paper_unavailable(
                "get_transactions",
                "paper fills are on each order: get_order_status and get_account_orders",
            )),
        }
    }

    pub async fn transaction(
        &self,
        account_hash: &str,
        transaction_id: &str,
    ) -> Result<schwab_types::trading::Transaction, ToolFailure> {
        match self {
            Self::Live(client) => Ok(client.get_transaction(account_hash, transaction_id).await?),
            Self::Paper(_) => Err(Self::paper_unavailable(
                "get_transaction",
                "paper fills are on each order: get_order_status",
            )),
        }
    }

    pub async fn preview_order(
        &self,
        account_hash: &str,
        order: &OrderRequest,
    ) -> Result<Value, ToolFailure> {
        match self {
            Self::Live(client) => Ok(client.preview_order(account_hash, order).await?),
            Self::Paper(_) => Err(Self::paper_unavailable(
                "preview_order",
                "check_order is the paper pre-flight",
            )),
        }
    }

    // ---- mutations -------------------------------------------------------

    pub async fn place_order(
        &self,
        account_hash: &str,
        order: &OrderRequest,
    ) -> Result<OrderSubmitResponse, ToolFailure> {
        match self {
            Self::Live(client) => Ok(client.place_order(account_hash, order).await?),
            Self::Paper(paper) => Ok(paper.broker.place_order(account_hash, order).await?),
        }
    }

    pub async fn replace_order(
        &self,
        account_hash: &str,
        order_id: &str,
        order: &OrderRequest,
    ) -> Result<OrderSubmitResponse, ToolFailure> {
        match self {
            Self::Live(client) => Ok(client.replace_order(account_hash, order_id, order).await?),
            Self::Paper(paper) => Ok(paper
                .broker
                .replace_order(account_hash, order_id, order)
                .await?),
        }
    }

    pub async fn cancel_order(
        &self,
        account_hash: &str,
        order_id: &str,
    ) -> Result<(), ToolFailure> {
        Ok(self.trading().cancel_order(account_hash, order_id).await?)
    }

    // ---- admission and health --------------------------------------------

    /// Local admission for a new submission, checked again on every attempt.
    pub async fn submission_admission(&self) -> Result<(), ToolFailure> {
        match self {
            Self::Live(client) => Ok(client.submission_admission().await?),
            Self::Paper(paper) => {
                if paper.broker.operator_halt().await?.is_some() {
                    return Err(Error::Halted("Operator trading halt is active".into()).into());
                }
                if paper.broker.is_draining() {
                    return Err(Error::Halted("Paper broker is draining".into()).into());
                }
                Ok(())
            }
        }
    }

    /// Suspend live submissions while a required stream recovers. The paper
    /// venue takes no stream, so it has nothing to suspend.
    pub fn suspend_submissions(&self, reason: &str) {
        if let Self::Live(client) = self {
            client.suspend_submissions(reason);
        }
    }

    pub fn submissions_suspended(&self) -> bool {
        match self {
            Self::Live(client) => client.submissions_suspended(),
            Self::Paper(paper) => paper.broker.is_draining(),
        }
    }

    pub fn circuit_state(&self) -> CircuitState {
        match self {
            Self::Live(client) => client.circuit_state(),
            Self::Paper(_) => CircuitState::Closed,
        }
    }

    pub fn request_budget(&self) -> Option<RequestBudgetUsage> {
        self.schwab().and_then(SchwabClient::request_budget)
    }

    pub fn has_valid_tokens(&self) -> bool {
        self.schwab()
            .is_some_and(|client| client.auth_manager().has_valid_tokens())
    }

    /// Whether reads and trading can be served now. Live readiness is judged
    /// over the recovery accounts; paper readiness over the venue's.
    pub async fn readiness(&self, max_age_ms: i64) -> Result<Readiness, ToolFailure> {
        let accounts: Vec<String> = match self {
            Self::Live(_) => server::recovery_accounts(),
            Self::Paper(paper) => paper.accounts().cloned().collect(),
        };
        Ok(self.trading().readiness(&accounts, max_age_ms).await?)
    }

    /// Whatever Schwab client exists, for figures that are about Schwab
    /// itself rather than the venue.
    fn schwab(&self) -> Option<&SchwabClient> {
        match self {
            Self::Live(client) => Some(client),
            Self::Paper(paper) => paper.market_data.as_deref(),
        }
    }
}

impl Paper {
    fn accounts(&self) -> impl Iterator<Item = &String> {
        self.broker.config().accounts.keys()
    }

    /// The venue's snapshot in Schwab's account shape. Positions are marked
    /// at the venue's fresh bid, as its equity is; a position without a
    /// fresh quote has no market value.
    async fn account(&self, name: &str, include_positions: bool) -> Result<Account, ToolFailure> {
        let snapshot = self.broker.snapshot(name).await?;
        let mut long_market_value = Some(0.0);
        let mut positions = Vec::new();
        for (symbol, position) in &snapshot.ledger.positions {
            if position.quantity.is_zero() {
                continue;
            }
            let market = match self.broker.quote(symbol).await? {
                Some(quote) if quote.fresh => Some(quote.bid * position.quantity),
                _ => None,
            };
            long_market_value = match (long_market_value, market) {
                (Some(total), Some(value)) => Some(total + money(value)),
                _ => None,
            };
            positions.push(json!({
                "shortQuantity": 0.0,
                "longQuantity": position.quantity.to_f64(),
                "settledLongQuantity": position.quantity.to_f64(),
                "averagePrice": (position.cost_basis / position.quantity).to_f64(),
                "marketValue": market.map(money),
                "longOpenProfitLoss": market.map(|value| money(value - position.cost_basis)),
                "instrument": {"assetType": "EQUITY", "symbol": symbol},
            }));
        }
        let PaperSnapshot {
            ledger,
            available_cash,
            unsettled_cash,
            equity,
            ..
        } = snapshot;
        let mut account = json!({
            "type": "CASH",
            "accountNumber": name,
            "roundTrips": 0,
            "isDayTrader": false,
            "isClosingOnlyRestricted": false,
            "currentBalances": {
                "cashBalance": money(ledger.cash),
                "cashAvailableForTrading": money(available_cash),
                "unsettledCash": money(unsettled_cash),
                "longMarketValue": long_market_value,
                "liquidationValue": equity.map(money),
                "equity": equity.map(money),
            },
        });
        if include_positions {
            account["positions"] = Value::Array(positions);
        }
        shaped("paper account", json!({"securitiesAccount": account}))
    }

    /// The account's orders entered inside the window, in Schwab's shape,
    /// oldest first.
    async fn orders(&self, name: &str, params: &OrdersParams) -> Result<Vec<Order>, ToolFailure> {
        const PAGE: u32 = 1000;
        let from = params.from_entered_time.timestamp_millis();
        let to = params.to_entered_time.timestamp_millis();
        let mut orders = Vec::new();
        let mut after = 0;
        loop {
            let page = self.broker.orders(name, after, PAGE).await?;
            let last = page.len() < PAGE as usize;
            for order in page {
                after = order
                    .order_id
                    .parse()
                    .map_err(|e| ToolFailure::internal("paper order id", e))?;
                if order.accepted_at_ms < from || order.accepted_at_ms > to {
                    continue;
                }
                let status: OrderStatus = shaped("paper order status", json!(order.status))?;
                if params.status.is_some_and(|wanted| wanted != status) {
                    continue;
                }
                orders.push(self.order(order).await?);
                if params
                    .max_results
                    .is_some_and(|max| orders.len() >= max as usize)
                {
                    return Ok(orders);
                }
            }
            if last {
                return Ok(orders);
            }
        }
    }

    /// A paper order in Schwab's shape, its fills as execution activities.
    async fn order(&self, order: PaperOrder) -> Result<Order, ToolFailure> {
        let fills = if order.filled_quantity.is_zero() {
            Vec::new()
        } else {
            self.broker.fills(&order.account, &order.order_id).await?
        };
        shaped("paper order", order_json(&order, &fills))
    }
}

/// A `PaperOrder` as Schwab would report the same order.
fn order_json(order: &PaperOrder, fills: &[PaperFill]) -> Value {
    let request = &order.request;
    let legs: Vec<Value> = request
        .order_leg_collection
        .iter()
        .enumerate()
        .map(|(index, leg)| {
            json!({
                "orderLegType": leg.instrument.asset_type,
                "legId": index as i64 + 1,
                "instrument": {"assetType": leg.instrument.asset_type, "symbol": leg.instrument.symbol},
                "instruction": leg.instruction,
                "positionEffect": leg.position_effect,
                "quantity": leg.quantity.to_f64(),
            })
        })
        .collect();
    let activities: Vec<Value> = fills
        .iter()
        .map(|fill| {
            json!({
                "activityType": "EXECUTION",
                "executionType": "FILL",
                "quantity": fill.quantity.to_f64(),
                "executionLegs": [{
                    "legId": 1,
                    "quantity": fill.quantity.to_f64(),
                    "price": fill.price.to_f64(),
                    "time": schwab_time(fill.at_ms),
                }],
            })
        })
        .collect();
    let open = order.status.is_open();
    json!({
        "orderId": order.order_id,
        "accountNumber": order.account,
        "session": request.session,
        "duration": request.duration,
        "orderType": request.order_type,
        "orderStrategyType": request.order_strategy_type,
        "price": request.price.and_then(|p| p.to_f64()),
        "stopPrice": request.stop_price.and_then(|p| p.to_f64()),
        "quantity": (order.filled_quantity + order.remaining_quantity).to_f64(),
        "filledQuantity": order.filled_quantity.to_f64(),
        "remainingQuantity": order.remaining_quantity.to_f64(),
        "status": order.status,
        "cancelable": open,
        "editable": open,
        "enteredTime": schwab_time(order.accepted_at_ms),
        "tag": request.tag,
        "orderLegCollection": legs,
        "orderActivityCollection": activities,
    })
}

/// The venue's quote in the shape of a Schwab `QuoteItem`, plus `fresh`:
/// whether an order would fill against it now.
fn paper_quote(quote: schwab_rs::paper::PaperQuote) -> Value {
    json!({
        "symbol": quote.symbol,
        "assetMainType": "EQUITY",
        "fresh": quote.fresh,
        "quote": {
            "bidPrice": quote.bid.to_f64(),
            "askPrice": quote.ask.to_f64(),
            "bidTime": quote.source_at_ms,
            "askTime": quote.source_at_ms,
            "quoteTime": quote.source_at_ms,
        },
    })
}

/// Schwab's order timestamps: `2025-01-15T14:31:22+0000`.
fn schwab_time(ms: i64) -> Option<String> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|t: DateTime<Utc>| t.format("%Y-%m-%dT%H:%M:%S%z").to_string())
}

/// Cents, as Schwab reports money.
fn money(value: Decimal) -> f64 {
    value.round_dp(2).to_f64().unwrap_or(f64::NAN)
}

/// Parse a venue answer through the Schwab type it imitates, so a paper
/// read can never carry a shape the live one could not.
fn shaped<T: serde::de::DeserializeOwned>(
    what: &'static str,
    value: Value,
) -> Result<T, ToolFailure> {
    serde_json::from_value(value).map_err(|e| ToolFailure::internal(what, e))
}
