//! What a frontend shows: everything the console knows at one moment, as
//! plain serializable data. A terminal UI renders it; a web frontend can
//! send it as JSON unchanged.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rustyschwab_mcp::delegation::{Grant, Mode};
use serde::Serialize;

/// The console's view at `at`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub at: DateTime<Utc>,
    /// `live` or `paper`.
    pub mode: String,
    /// Who commands from this console, as audits record it.
    pub operator: String,
    /// The operator halt's reason while trading is halted.
    pub halt: Option<String>,
    pub principals: Vec<PrincipalView>,
    /// Every principal's tool calls, oldest first, the latest
    /// [`ACTIVITY_SHOWN`](crate::ACTIVITY_SHOWN) of them.
    pub activity: Vec<ActivityView>,
    /// Orders waiting for an operator's approval, oldest first.
    pub approvals: Vec<ApprovalView>,
    pub accounts: Vec<AccountView>,
    /// Working orders first, then the most recent others, per account.
    pub orders: Vec<OrderView>,
    pub quotes: Vec<QuoteView>,
    /// What could not be read this time, in words.
    pub warnings: Vec<String>,
}

impl Snapshot {
    /// The empty view before the first refresh.
    pub fn empty(mode: &str, operator: &str) -> Self {
        Self {
            at: Utc::now(),
            mode: mode.to_owned(),
            operator: operator.to_owned(),
            halt: None,
            principals: Vec::new(),
            activity: Vec::new(),
            approvals: Vec::new(),
            accounts: Vec::new(),
            orders: Vec::new(),
            quotes: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

/// One agent identity from the policy file.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrincipalView {
    pub name: String,
    /// The approval mode in force.
    pub approval: Mode,
    /// The operator's grant, while one is in force.
    pub delegation: Option<Grant>,
    /// The widest mode this console may grant.
    pub ceiling: Mode,
    pub dead_man: Option<DeadManView>,
    pub budget: BudgetView,
    /// Its submissions whose outcome the venue has not answered.
    pub pending_submissions: usize,
    /// When any server last heard from it.
    pub last_call: Option<DateTime<Utc>>,
    /// Whether it has state yet (it has run at least once).
    pub active: bool,
}

/// A principal's dead-man switch.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeadManView {
    pub window_secs: u64,
    /// Until its working orders are cancelled, if it stays silent.
    pub seconds_left: u64,
}

/// A principal's daily budget and what it has used.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetView {
    pub orders_used: u32,
    pub max_orders: Option<u32>,
    pub notional_used: Decimal,
    pub max_notional: Option<Decimal>,
}

/// One tool call.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityView {
    pub sequence: i64,
    pub at: DateTime<Utc>,
    pub principal: String,
    pub tool: String,
    /// `ok`, or the failure code.
    pub outcome: String,
    pub duration_ms: u64,
    /// The call in words: the order an order tool carried, or its main
    /// arguments.
    pub summary: String,
    /// The model's stated reason, for an order tool.
    pub rationale: Option<String>,
    /// The refusal's message.
    pub message: Option<String>,
    pub order_id: Option<String>,
}

/// An order waiting for an operator.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalView {
    pub principal: String,
    pub digest: String,
    pub tool: String,
    /// The order in words, as `rustyschwab-admin show` prints it.
    pub description: String,
    pub rationale: Option<String>,
    pub first_seen: DateTime<Utc>,
    /// How many times the agent asked.
    pub seen: i64,
}

/// One brokerage account.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    /// Schwab's account hash, which commands take.
    pub hash: String,
    /// The account number's last four digits.
    pub number: String,
    pub liquidation_value: Option<f64>,
    pub cash: Option<f64>,
    pub buying_power: Option<f64>,
    pub day_pnl: Option<f64>,
    pub open_pnl: Option<f64>,
    pub positions: Vec<PositionView>,
    /// When these figures were read from the venue.
    pub observed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionView {
    pub symbol: String,
    /// Long less short.
    pub quantity: f64,
    pub average_price: Option<f64>,
    pub market_value: Option<f64>,
    pub day_pnl: Option<f64>,
    pub open_pnl: Option<f64>,
}

/// One order, a child of a strategy flattened beside its parent.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderView {
    pub account: String,
    pub order_id: String,
    /// The principal that placed it, when its journal says so.
    pub principal: Option<String>,
    pub status: String,
    pub working: bool,
    /// `BUY 10 AAPL`, legs joined by `/`.
    pub legs: String,
    pub order_type: String,
    pub price: Option<f64>,
    pub stop_price: Option<f64>,
    pub quantity: Option<f64>,
    pub filled: Option<f64>,
    pub entered: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteView {
    pub symbol: String,
    pub last: Option<f64>,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub change: Option<f64>,
    pub change_percent: Option<f64>,
}
