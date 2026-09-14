//! Versioned inputs and exact-decimal paper account reports.
use crate::risk::PortfolioLimits;
use crate::types::trading::OrderRequest;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Fill semantics are pinned in durable state; changing them requires a new run.
pub const MODEL_VERSION: &str = "equity-bbo-v2";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaperAccountConfig {
    pub initial_cash: Decimal,
    pub limits: PortfolioLimits,
}

/// Replay is driven only by events; wall mode rejects stopped/stale live feeds.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PaperClockMode {
    #[default]
    Replay,
    Wall,
}

/// All amounts are decimal strings in JSON. There are no broker credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaperConfig {
    pub model_version: String,
    #[serde(default)]
    pub clock: PaperClockMode,
    pub accounts: BTreeMap<String, PaperAccountConfig>,
    pub execution_latency_ms: i64,
    pub cancel_latency_ms: i64,
    pub quote_max_age_ms: i64,
    pub slippage_bps: Decimal,
    pub market_collar_bps: Decimal,
    pub fee_per_share: Decimal,
    pub max_working_orders: usize,
    /// If enabled, every trading session needs an explicit settlement timestamp.
    #[serde(default)]
    pub settlement_required: bool,
    #[serde(default)]
    pub settlement_calendar: BTreeMap<String, i64>,
}

/// Attribution supplied by trusted application code, never by a gateway event body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum PaperEventSource {
    Local,
    SchwabRest,
    Authority { subject: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaperEvent {
    /// Contiguous, globally ordered, starting at one. Exact retries are idempotent.
    pub sequence: u64,
    /// Virtual receive time. Never moves backwards.
    pub at_ms: i64,
    pub kind: PaperEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PaperEventKind {
    /// Explicit exchange session/calendar supplied by the replay/feed adapter.
    Session { session_id: String, open: bool },
    Quote {
        symbol: String,
        source_at_ms: i64,
        bid: Decimal,
        ask: Decimal,
        bid_size: Decimal,
        ask_size: Decimal,
    },
    /// Capture entitlement from holdings at this event, with explicit payment time.
    Dividend {
        action_id: String,
        symbol: String,
        cash_per_share: Decimal,
        pay_at_ms: i64,
    },
    /// Cancel working orders for the symbol, adjust holdings/cost basis, and pay
    /// fractional entitlements at the supplied cash-in-lieu price and time.
    Split {
        action_id: String,
        symbol: String,
        numerator: u32,
        denominator: u32,
        cash_in_lieu_price: Decimal,
        pay_at_ms: i64,
    },
    /// Advances time and pending cancels without inventing fresh liquidity.
    Clock,
    /// Invalidates all quotes. A subsequent quote is required to submit again.
    Disconnect,
    /// One-shot fault, persisted until the next new/replace order.
    Fault { fault: PaperFault },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PaperFault {
    RejectNext,
    LoseNextAcknowledgement,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PaperOrderStatus {
    Working,
    PendingCancel,
    Filled,
    Canceled,
    Replaced,
    Expired,
    Rejected,
}
impl PaperOrderStatus {
    pub fn is_open(self) -> bool {
        matches!(self, Self::Working | Self::PendingCancel)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperOrder {
    pub order_id: String,
    pub account: String,
    pub request: OrderRequest,
    pub status: PaperOrderStatus,
    pub filled_quantity: Decimal,
    pub remaining_quantity: Decimal,
    pub accepted_at_ms: i64,
    pub accepted_sequence: u64,
    pub eligible_at_ms: i64,
    pub session_id: String,
    pub cancel_at_ms: Option<i64>,
    /// Buy maximum / sell minimum for protected simulated market orders.
    pub price_bound: Decimal,
    pub reserved_cash: Decimal,
    pub replaces: Option<String>,
    #[serde(default)]
    pub triggered: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PaperPosition {
    pub quantity: Decimal,
    /// Total remaining acquisition cost, including simulated buy fees.
    pub cost_basis: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingCash {
    pub source: String,
    pub amount: Decimal,
    pub available_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperAccount {
    pub cash: Decimal,
    pub realized_pnl: Decimal,
    pub fees: Decimal,
    pub positions: BTreeMap<String, PaperPosition>,
    #[serde(default)]
    pub pending_cash: Vec<PendingCash>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperFill {
    pub order_id: String,
    pub event_sequence: u64,
    pub at_ms: i64,
    pub quantity: Decimal,
    pub price: Decimal,
    pub fee: Decimal,
}

/// The venue's latest best bid and offer for a symbol: the quote paper fills
/// are judged against, which is not a Schwab quote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaperQuote {
    pub symbol: String,
    pub source_at_ms: i64,
    pub bid: Decimal,
    pub ask: Decimal,
    /// Whether an order would fill against this quote now: the session is
    /// open and the quote is within `quote_max_age_ms` of the venue clock.
    pub fresh: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperSnapshot {
    pub mode: String,
    pub clock: PaperClockMode,
    pub model_version: String,
    pub account: String,
    pub at_ms: i64,
    pub event_sequence: u64,
    pub ledger: PaperAccount,
    pub reserved_cash: Decimal,
    pub available_cash: Decimal,
    pub unsettled_cash: Decimal,
    /// Conservative bid marking; absent if any position lacks a fresh quote.
    pub equity: Option<Decimal>,
    pub unrealized_pnl: Option<Decimal>,
    pub working_orders: Vec<PaperOrder>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperEventResult {
    pub mode: String,
    pub duplicate: bool,
    pub sequence: u64,
    pub fills: Vec<PaperFill>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(super) struct Clock {
    pub sequence: u64,
    pub at_ms: i64,
    pub session_id: String,
    pub open: bool,
    pub fault: Option<PaperFault>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Quote {
    pub source_at_ms: i64,
    pub bid: Decimal,
    pub ask: Decimal,
}

/// Replay input: preserve command/event ordering and use a new database to
/// reproduce a complete run. Resubmitted order tags deliberately fail closed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum PaperCommand {
    Archive {
        destination: std::path::PathBuf,
        through_sequence: u64,
    },
    Verify,
    Event {
        event: PaperEvent,
    },
    Place {
        account: String,
        order: OrderRequest,
    },
    Replace {
        account: String,
        order_id: String,
        order: OrderRequest,
    },
    Cancel {
        account: String,
        order_id: String,
    },
    Snapshot {
        account: String,
    },
    Halt {
        reason: String,
    },
    Resume,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperIntegrityReport {
    pub model_version: String,
    pub accounts: usize,
    pub orders: usize,
    pub event_sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperArchiveReport {
    pub through_sequence: u64,
    pub compacted: usize,
    pub archive_path: std::path::PathBuf,
    pub sha256: String,
}

/// A committed execution mutation, atomically captured in the audit journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaperCapturedCommand {
    pub index: u64,
    pub command: PaperCommand,
    pub outcome: PaperCapturedOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum PaperCapturedOutcome {
    Success { result: serde_json::Value },
    InjectedRejection,
    LostAcknowledgement { order_id: String },
}
