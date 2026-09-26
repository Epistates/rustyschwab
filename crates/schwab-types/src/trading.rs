//! Trading types including orders, transactions, and execution reports.
//!
//! This module contains types for trading operations:
//!
//! - Order creation and management
//! - Order status and responses
//! - Transaction history
//! - Execution reports
//! - Query parameters for the order and transaction listings
//!   ([`OrdersParams`](crate::trading::OrdersParams),
//!   [`TransactionsParams`](crate::trading::TransactionsParams))
//!
//! # Building orders
//!
//! [`OrderRequest`](crate::trading::OrderRequest) is the strict outbound
//! shape. Its constructors assemble it the way Schwab's order examples are
//! written, so callers never hand-nest `childOrderStrategies`: single orders
//! start from a leg
//! ([`OrderRequestLeg::equity`](crate::trading::OrderRequestLeg::equity),
//! [`OrderRequestLeg::option`](crate::trading::OrderRequestLeg::option)) and
//! an order type ([`OrderRequest::market`](crate::trading::OrderRequest::market),
//! [`OrderRequest::limit`](crate::trading::OrderRequest::limit), …), default
//! to the `NORMAL` session and `DAY` duration, and take `with_*` setters;
//! [`OrderRequest::one_cancels_other`](crate::trading::OrderRequest::one_cancels_other),
//! [`OrderRequest::triggers`](crate::trading::OrderRequest::triggers) and
//! [`OrderRequest::bracket`](crate::trading::OrderRequest::bracket) compose
//! them. [`OptionSymbol`](crate::trading::OptionSymbol) formats and parses
//! OSI option symbols.
//! [`OrderRequest::validate`](crate::trading::OrderRequest::validate) checks
//! structure before submission.
//!
//! ```
//! use schwab_types::Decimal;
//! use schwab_types::trading::{OrderDuration, OrderInstruction, OrderRequest, OrderRequestLeg};
//!
//! let entry = OrderRequest::limit(
//!     OrderRequestLeg::equity(OrderInstruction::Buy, "AAPL", 10),
//!     Decimal::new(15025, 2),
//! )
//! .with_duration(OrderDuration::Gtc)
//! .with_tag("swing-1");
//! // Buy at 150.25; once filled, sell at 160 or stop out at 145.
//! let bracket = entry.bracket(Decimal::from(160), Decimal::from(145)).unwrap();
//! bracket.validate().unwrap();
//! ```

use crate::common::{AccountsInstrument, AssetType, PutCall};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::collections::HashMap;

mod builder;
mod spec;
pub use builder::OptionSymbol;
pub use spec::{InstrumentSpec, LegSpec, OrderSettings, OrderSpec};

/// An order as Schwab reports it. Tolerant: every enum decodes a value this
/// release does not know as `Unknown` (see [`Wire`](crate::common::Wire)),
/// and undocumented fields land in `extra`. Times are the broker's ISO-8601
/// strings, kept verbatim.
///
/// `session`, `duration` and `order_type` are `None` when Schwab omits
/// them, as it does on an `OCO` parent: nothing is inferred.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Order {
    #[serde(default, deserialize_with = "crate::common::optional_id")]
    pub order_id: Option<String>,
    pub session: Option<OrderSession>,
    pub duration: Option<OrderDuration>,
    pub order_type: Option<OrderType>,
    pub complex_order_strategy_type: Option<ComplexOrderStrategyType>,
    pub quantity: Option<f64>,
    pub filled_quantity: Option<f64>,
    pub remaining_quantity: Option<f64>,
    pub requested_destination: Option<RequestedDestination>,
    pub destination_link_name: Option<String>,
    pub price: Option<f64>,
    #[serde(default)]
    pub order_leg_collection: Vec<OrderLeg>,
    pub order_strategy_type: Option<OrderStrategyType>,
    pub cancelable: Option<bool>,
    pub editable: Option<bool>,
    pub status: Option<OrderStatus>,
    pub status_description: Option<String>,
    pub entered_time: Option<String>,
    pub close_time: Option<String>,
    pub cancel_time: Option<String>,
    pub release_time: Option<String>,
    pub stop_price: Option<f64>,
    pub stop_price_link_basis: Option<PriceLinkBasis>,
    pub stop_price_link_type: Option<PriceLinkType>,
    pub stop_price_offset: Option<f64>,
    pub stop_type: Option<StopType>,
    pub price_link_basis: Option<PriceLinkBasis>,
    pub price_link_type: Option<PriceLinkType>,
    pub activation_price: Option<f64>,
    pub tax_lot_method: Option<TaxLotMethod>,
    pub special_instruction: Option<SpecialInstruction>,
    pub tag: Option<String>,
    #[serde(default)]
    pub child_order_strategies: Vec<Order>,
    #[serde(default)]
    pub replacing_order_collection: Vec<Order>,
    #[serde(default)]
    pub order_activity_collection: Vec<OrderActivity>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
    #[serde(default, deserialize_with = "crate::common::optional_id")]
    pub account_number: Option<String>,
}

#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct OrderLeg {
    pub order_leg_type: OrderLegType,
    pub leg_id: Option<i64>,
    pub instrument: AccountsInstrument,
    pub instruction: OrderInstruction,
    pub position_effect: Option<PositionEffect>,
    pub quantity: f64,
    pub quantity_type: Option<QuantityType>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// One entry of `orderActivityCollection`: an execution (partial or full
/// fill) or an order action. The real payload carries more than the spec
/// documents (an `activityId`, `executionType: CANCELED`), so
/// `execution_type` stays a string and the rest lands in `extra`.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct OrderActivity {
    pub activity_type: Option<OrderActivityType>,
    /// `FILL` is the only documented value; `CANCELED` has been observed.
    pub execution_type: Option<String>,
    pub quantity: Option<f64>,
    pub order_remaining_quantity: Option<f64>,
    #[serde(default)]
    pub execution_legs: Vec<ExecutionLeg>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum OrderActivityType {
    Execution,
    OrderAction,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

/// Per-leg detail of an [`OrderActivity`] execution.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ExecutionLeg {
    pub leg_id: Option<i64>,
    pub price: Option<f64>,
    pub quantity: Option<f64>,
    pub mismarked_quantity: Option<f64>,
    pub instrument_id: Option<i64>,
    pub time: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// How an order leg's `quantity` is denominated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum QuantityType {
    AllShares,
    Dollars,
    Shares,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum OrderSession {
    #[default]
    Normal,
    Am,
    Pm,
    Seamless,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum OrderDuration {
    #[default]
    Day,
    #[serde(rename = "GOOD_TILL_CANCEL")]
    Gtc,
    FillOrKill,
    ImmediateOrCancel,
    EndOfWeek,
    EndOfMonth,
    NextEndOfMonth,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

/// `CABINET`, `NON_MARKETABLE` and `EXERCISE` appear on orders Schwab
/// reports back but cannot be submitted, so the request schema leaves them
/// out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum OrderType {
    Market,
    Limit,
    Stop,
    StopLimit,
    TrailingStop,
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Cabinet,
    #[cfg_attr(feature = "schemars", schemars(skip))]
    NonMarketable,
    MarketOnClose,
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Exercise,
    TrailingStopLimit,
    NetDebit,
    NetCredit,
    NetZero,
    LimitOnClose,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum ComplexOrderStrategyType {
    None,
    Covered,
    Vertical,
    BackRatio,
    Calendar,
    Diagonal,
    Straddle,
    Strangle,
    CollarSynthetic,
    Butterfly,
    Condor,
    IronCondor,
    VerticalRoll,
    CollarWithStock,
    DoubleDiagonal,
    UnbalancedButterfly,
    UnbalancedCondor,
    UnbalancedIronCondor,
    UnbalancedVerticalRoll,
    MutualFundSwap,
    Custom,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

/// Only [`Single`](Self::Single), [`Oco`](Self::Oco), and
/// [`Trigger`](Self::Trigger) can be submitted; the rest appear on orders
/// Schwab reports back, and are left out of the request schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum OrderStrategyType {
    #[default]
    Single,
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Cancel,
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Recall,
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Pair,
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Flatten,
    #[cfg_attr(feature = "schemars", schemars(skip))]
    TwoDaySwap,
    #[cfg_attr(feature = "schemars", schemars(skip))]
    BlastAll,
    Oco,
    Trigger,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum OrderStatus {
    AwaitingParentOrder,
    AwaitingCondition,
    AwaitingStopCondition,
    AwaitingManualReview,
    Accepted,
    AwaitingUrOut,
    PendingActivation,
    Queued,
    Working,
    Rejected,
    PendingCancel,
    Canceled,
    PendingReplace,
    Replaced,
    Filled,
    Expired,
    New,
    AwaitingReleaseTime,
    PendingAcknowledgement,
    PendingRecall,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

impl OrderStatus {
    /// Schwab will not change the order again: it filled, was canceled,
    /// rejected, expired or replaced. Every other status, including one
    /// this crate does not know, may still fill.
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Filled | Self::Canceled | Self::Rejected | Self::Expired | Self::Replaced
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum OrderLegType {
    #[default]
    Equity,
    Option,
    Index,
    MutualFund,
    CashEquivalent,
    FixedIncome,
    Currency,
    CollectiveInvestment,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum OrderInstruction {
    #[default]
    Buy,
    Sell,
    BuyToCover,
    SellShort,
    BuyToOpen,
    BuyToClose,
    SellToOpen,
    SellToClose,
    Exchange,
    SellShortExempt,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum PositionEffect {
    Opening,
    Closing,
    #[default]
    Automatic,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

/// What a linked (trailing) price follows: `priceLinkBasis` and
/// `stopPriceLinkBasis`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum PriceLinkBasis {
    Manual,
    Base,
    Trigger,
    Last,
    Bid,
    Ask,
    AskBid,
    Mark,
    Average,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

/// How a linked price's offset is expressed: `priceLinkType` and
/// `stopPriceLinkType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum PriceLinkType {
    Value,
    Percent,
    Tick,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

/// Which price a stop order triggers on (`stopType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum StopType {
    Standard,
    Bid,
    Ask,
    Last,
    Mark,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum TaxLotMethod {
    Fifo,
    Lifo,
    HighCost,
    LowCost,
    AverageCost,
    SpecificLot,
    LossHarvester,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum SpecialInstruction {
    AllOrNone,
    DoNotReduce,
    AllOrNoneDoNotReduce,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

/// Routing venue for `requestedDestination`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum RequestedDestination {
    Inet,
    EcnArca,
    Cboe,
    Amex,
    Phlx,
    Ise,
    Box,
    Nyse,
    Nasdaq,
    Bats,
    C2,
    Auto,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

/// Result of submitting an order via `POST .../orders` or `PUT .../orders/{id}`.
///
/// Schwab returns **201 Created with an empty body**; the only payload is a
/// `Location` header pointing at the new order resource. The order ID is the
/// last path segment of that URL. If the order filled immediately, Schwab may
/// omit the header entirely — hence both fields are optional.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct OrderSubmitResponse {
    /// Order ID parsed from the `Location` header's last path segment.
    pub order_id: Option<String>,
    /// Full `Location` header value, when present.
    pub location: Option<String>,
}

/// Query parameters for `GET .../orders` (one account) and `GET /orders`
/// (every linked account).
///
/// Both bounds are required by Schwab, which formats them as
/// [`schwab_timestamp`](crate::common::schwab_timestamp) on the wire and
/// lists only orders entered in the last 60 days, however far back the
/// window reaches. Callers that want the whole horizon should stay just
/// inside it; the SDK's own reads use 59 days.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct OrdersParams {
    /// No orders entered before this instant are returned.
    #[serde(serialize_with = "crate::common::serialize_timestamp")]
    pub from_entered_time: DateTime<Utc>,
    /// No orders entered after this instant are returned.
    #[serde(serialize_with = "crate::common::serialize_timestamp")]
    pub to_entered_time: DateTime<Utc>,
    /// Page size (≥ 1); Schwab's default is 3000.
    pub max_results: Option<u32>,
    /// Only orders in this status.
    pub status: Option<OrderStatus>,
}

impl OrdersParams {
    /// Every order entered in the window, in Schwab's default page size.
    pub fn new(from_entered_time: DateTime<Utc>, to_entered_time: DateTime<Utc>) -> Self {
        Self {
            from_entered_time,
            to_entered_time,
            max_results: None,
            status: None,
        }
    }

    /// Check the request against Schwab's documented bounds.
    pub fn validate(&self) -> Result<(), String> {
        if self.from_entered_time > self.to_entered_time {
            return Err("fromEnteredTime must not be after toEnteredTime".into());
        }
        if self.max_results == Some(0) {
            return Err("maxResults must be at least 1".into());
        }
        crate::common::known("status", self.status.as_ref())
    }
}

/// A transaction as Schwab reports it: header fields plus one
/// [`TransferItem`] per instrument moved and per fee charged. There is no
/// top-level instrument or fee map; a trade's security leg and each of its
/// fees are separate items, and the fees are the ones whose
/// [`fee_type`](TransferItem::fee_type) is set.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    pub activity_id: Option<i64>,
    pub time: Option<String>,
    pub user: Option<TransactionUser>,
    pub description: Option<String>,
    #[serde(default, deserialize_with = "crate::common::optional_id")]
    pub account_number: Option<String>,
    /// `Unknown` for a category this release does not know.
    #[serde(rename = "type")]
    pub type_: Option<TransactionType>,
    pub status: Option<TransactionStatus>,
    pub sub_account: Option<SubAccount>,
    pub trade_date: Option<String>,
    pub settlement_date: Option<String>,
    pub position_id: Option<i64>,
    #[serde(default, deserialize_with = "crate::common::optional_id")]
    pub order_id: Option<String>,
    pub net_amount: Option<f64>,
    pub activity_type: Option<TransactionActivityType>,
    #[serde(default)]
    pub transfer_items: Vec<TransferItem>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Transaction category; the `type` of a [`Transaction`] and the value set
/// of the `types` filter on `GET .../transactions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum TransactionType {
    Trade,
    ReceiveAndDeliver,
    DividendOrInterest,
    AchReceipt,
    AchDisbursement,
    CashReceipt,
    CashDisbursement,
    ElectronicFund,
    WireOut,
    WireIn,
    Journal,
    Memorandum,
    MarginCall,
    MoneyMarket,
    SmaAdjustment,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

impl TransactionType {
    /// Wire name used in the `types` query parameter.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trade => "TRADE",
            Self::ReceiveAndDeliver => "RECEIVE_AND_DELIVER",
            Self::DividendOrInterest => "DIVIDEND_OR_INTEREST",
            Self::AchReceipt => "ACH_RECEIPT",
            Self::AchDisbursement => "ACH_DISBURSEMENT",
            Self::CashReceipt => "CASH_RECEIPT",
            Self::CashDisbursement => "CASH_DISBURSEMENT",
            Self::ElectronicFund => "ELECTRONIC_FUND",
            Self::WireOut => "WIRE_OUT",
            Self::WireIn => "WIRE_IN",
            Self::Journal => "JOURNAL",
            Self::Memorandum => "MEMORANDUM",
            Self::MarginCall => "MARGIN_CALL",
            Self::MoneyMarket => "MONEY_MARKET",
            Self::SmaAdjustment => "SMA_ADJUSTMENT",
            Self::Unknown => "UNKNOWN",
        }
    }
}

impl std::fmt::Display for TransactionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum TransactionStatus {
    Valid,
    Invalid,
    Pending,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

/// Ledger a transaction settled against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum SubAccount {
    Cash,
    Margin,
    Short,
    Div,
    Income,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum TransactionActivityType {
    ActivityCorrection,
    Execution,
    OrderAction,
    Transfer,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

/// One leg of a [`Transaction`]: a security or cash movement, or a fee.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct TransferItem {
    pub instrument: Option<TransactionInstrument>,
    /// Quantity moved (shares, contracts, or currency units).
    pub amount: Option<f64>,
    pub cost: Option<f64>,
    pub price: Option<f64>,
    /// Set on fee items only.
    pub fee_type: Option<FeeType>,
    /// `Unknown` when Schwab reports `UNKNOWN`.
    pub position_effect: Option<PositionEffect>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum FeeType {
    Commission,
    SecFee,
    StrFee,
    RFee,
    CdscFee,
    OptRegFee,
    AdditionalFee,
    MiscellaneousFee,
    Ftt,
    FuturesClearingFee,
    FuturesDeskOfficeFee,
    FuturesExchangeFee,
    FuturesGlobexFee,
    FuturesNfaFee,
    FuturesPitBrokerageFee,
    FuturesTransactionFee,
    LowProceedsCommission,
    BaseCharge,
    GeneralCharge,
    GstFee,
    TafFee,
    IndexOptionFee,
    TefraTax,
    StateTax,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct TransactionUser {
    pub cd_domain_id: Option<String>,
    pub login: Option<String>,
    #[serde(rename = "type")]
    pub type_: Option<String>,
    pub user_id: Option<i64>,
    pub system_user_name: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub broker_rep_code: Option<String>,
}

/// The instrument on a [`TransferItem`] (`TransactionInstrument` in the
/// Trader API spec). The base fields plus the option, fixed-income and
/// future extensions are typed; the rest lands in `extra`.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct TransactionInstrument {
    pub asset_type: AssetType,
    pub symbol: String,
    pub cusip: Option<String>,
    pub description: Option<String>,
    pub instrument_id: Option<i64>,
    pub net_change: Option<f64>,
    /// Subtype whose value set depends on `asset_type` (`COMMON_STOCK`,
    /// `SWEEP_VEHICLE`, `VANILLA`, …).
    #[serde(rename = "type")]
    pub type_: Option<String>,
    /// Observed on live payloads (`ACTIVE`), undocumented in the spec.
    pub status: Option<String>,
    /// Observed on live payloads, undocumented in the spec.
    pub closing_price: Option<f64>,
    /// Options and futures.
    pub expiration_date: Option<String>,
    /// Options only. `None` when Schwab reports `UNKNOWN`.
    #[serde(default, deserialize_with = "crate::common::lenient")]
    pub put_call: Option<PutCall>,
    /// Options only.
    pub strike_price: Option<f64>,
    /// Options only.
    pub option_premium_multiplier: Option<i64>,
    /// Options only.
    pub underlying_symbol: Option<String>,
    /// Options only.
    pub underlying_cusip: Option<String>,
    /// Fixed income only.
    pub maturity_date: Option<String>,
    /// Fixed income only.
    pub factor: Option<f64>,
    /// Fixed income only.
    pub variable_rate: Option<f64>,
    /// Fixed income and futures.
    pub multiplier: Option<f64>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Query parameters for `GET .../transactions`.
///
/// Both bounds and at least one type are required by Schwab, which formats
/// the bounds as [`schwab_timestamp`](crate::common::schwab_timestamp) on
/// the wire, joins the types with commas, and caps the window at one year.
/// Transaction history reaches back that far; the order list
/// ([`OrdersParams`]) reaches back only 60 days. The two limits are
/// different, and neither is enforced here: Schwab answers the request.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct TransactionsParams {
    /// No transactions before this instant are returned.
    #[serde(serialize_with = "crate::common::serialize_timestamp")]
    pub start_date: DateTime<Utc>,
    /// No transactions after this instant are returned.
    #[serde(serialize_with = "crate::common::serialize_timestamp")]
    pub end_date: DateTime<Utc>,
    /// Transaction categories to include (at least one). Serializes as the
    /// comma-joined list the query string takes; deserializes from that or
    /// from a JSON array.
    #[serde(
        serialize_with = "serialize_transaction_types",
        deserialize_with = "deserialize_transaction_types"
    )]
    pub types: Vec<TransactionType>,
    /// Only activity in this symbol.
    pub symbol: Option<String>,
}

impl TransactionsParams {
    /// Every transaction of the given types in the window.
    pub fn new(
        start_date: DateTime<Utc>,
        end_date: DateTime<Utc>,
        types: impl IntoIterator<Item = TransactionType>,
    ) -> Self {
        Self {
            start_date,
            end_date,
            types: types.into_iter().collect(),
            symbol: None,
        }
    }

    /// Check the request against Schwab's documented bounds.
    pub fn validate(&self) -> Result<(), String> {
        if self.start_date > self.end_date {
            return Err("startDate must not be after endDate".into());
        }
        if self.types.is_empty() {
            return Err("types must name at least one transaction type".into());
        }
        for t in &self.types {
            crate::common::known("types", Some(t))?;
        }
        if self.symbol.as_deref().is_some_and(|s| s.trim().is_empty()) {
            return Err("symbol must not be blank".into());
        }
        Ok(())
    }
}

/// Comma-joined wire names, the only list encoding the query string takes.
fn serialize_transaction_types<S: serde::Serializer>(
    types: &[TransactionType],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let csv = types
        .iter()
        .map(|t| t.as_str())
        .collect::<Vec<_>>()
        .join(",");
    serializer.serialize_str(&csv)
}

/// Accept the comma-joined wire form back, so serialized params round-trip.
fn deserialize_transaction_types<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<TransactionType>, D::Error> {
    use serde::de::IntoDeserializer;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Types {
        List(Vec<TransactionType>),
        Csv(String),
    }

    match Types::deserialize(deserializer)? {
        Types::List(types) => Ok(types),
        Types::Csv(csv) => csv
            .split(',')
            .map(|name| TransactionType::deserialize(name.trim().into_deserializer()))
            .collect(),
    }
}

/// Outgoing orders have a strict schema, separate from tolerant broker responses.
/// Unsupported fields fail deserialization rather than silently altering a trade,
/// and decimals parse exactly (see [`exact_decimal`](crate::common::exact_decimal)).
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrderRequest {
    pub session: Option<OrderSession>,
    pub duration: Option<OrderDuration>,
    pub order_type: Option<OrderType>,
    pub complex_order_strategy_type: Option<ComplexOrderStrategyType>,
    /// When present, must equal every leg's quantity. Sent as a JSON number.
    #[serde(
        default,
        deserialize_with = "crate::common::exact_decimal::option::deserialize",
        serialize_with = "crate::common::exact_decimal::option::serialize_number"
    )]
    #[cfg_attr(
        feature = "schemars",
        schemars(with = "Option<crate::common::QuantityNumber>")
    )]
    pub quantity: Option<rust_decimal::Decimal>,
    #[serde(
        default,
        deserialize_with = "crate::common::exact_decimal::option::deserialize"
    )]
    #[cfg_attr(
        feature = "schemars",
        schemars(with = "Option<crate::common::DecimalString>")
    )]
    pub price: Option<rust_decimal::Decimal>,
    #[serde(
        default,
        deserialize_with = "crate::common::exact_decimal::option::deserialize"
    )]
    #[cfg_attr(
        feature = "schemars",
        schemars(with = "Option<crate::common::DecimalString>")
    )]
    pub stop_price: Option<rust_decimal::Decimal>,
    pub stop_price_link_basis: Option<PriceLinkBasis>,
    pub stop_price_link_type: Option<PriceLinkType>,
    #[serde(
        default,
        deserialize_with = "crate::common::exact_decimal::option::deserialize"
    )]
    #[cfg_attr(
        feature = "schemars",
        schemars(with = "Option<crate::common::DecimalString>")
    )]
    pub stop_price_offset: Option<rust_decimal::Decimal>,
    pub stop_type: Option<StopType>,
    pub price_link_basis: Option<PriceLinkBasis>,
    pub price_link_type: Option<PriceLinkType>,
    pub tax_lot_method: Option<TaxLotMethod>,
    pub tag: Option<String>,
    pub special_instruction: Option<SpecialInstruction>,
    pub requested_destination: Option<RequestedDestination>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order_leg_collection: Vec<OrderRequestLeg>,
    pub order_strategy_type: OrderStrategyType,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_order_strategies: Vec<OrderRequest>,
}

#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrderRequestLeg {
    pub instruction: OrderInstruction,
    /// Shares or contracts; whole numbers for equities and options. For a
    /// mutual fund, denominated as `quantity_type` says. Sent as a JSON number.
    #[serde(
        deserialize_with = "crate::common::exact_decimal::deserialize",
        serialize_with = "crate::common::exact_decimal::serialize_number"
    )]
    #[cfg_attr(feature = "schemars", schemars(with = "crate::common::QuantityNumber"))]
    pub quantity: rust_decimal::Decimal,
    pub instrument: OrderRequestInstrument,
    pub position_effect: Option<PositionEffect>,
    /// Mutual funds only: whether `quantity` is shares, dollars or the whole
    /// position.
    #[serde(default)]
    pub quantity_type: Option<QuantityType>,
    /// Mutual funds only: what happens to dividends and capital gains.
    #[serde(default)]
    pub div_cap_gains: Option<DivCapGains>,
    /// Mutual funds only: the fund an `EXCHANGE` moves into.
    #[serde(default)]
    pub to_symbol: Option<String>,
}

/// Dividend and capital-gain handling on a mutual-fund leg (`divCapGains`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum DivCapGains {
    Reinvest,
    Payout,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrderRequestInstrument {
    pub symbol: String,
    pub asset_type: AssetType,
}

crate::common::wire_enum!(
    QuantityType,
    OrderSession,
    OrderDuration,
    OrderType,
    ComplexOrderStrategyType,
    OrderStrategyType,
    OrderStatus,
    OrderLegType,
    OrderInstruction,
    PositionEffect,
    PriceLinkBasis,
    PriceLinkType,
    StopType,
    TaxLotMethod,
    SpecialInstruction,
    RequestedDestination,
    OrderActivityType,
    TransactionType,
    TransactionStatus,
    SubAccount,
    TransactionActivityType,
    FeeType,
    DivCapGains,
);

/// Strategies nest at most this many levels, the root counting as one.
const MAX_STRATEGY_DEPTH: usize = 8;

/// What an executable order type takes. `true` means required, `false`
/// forbidden; the trailing link fields are required together.
struct TypeRules {
    price: bool,
    stop_price: bool,
    trailing: bool,
    /// `priceLinkBasis`/`priceLinkType` (a linked limit) are allowed.
    price_link: bool,
    /// `stopType` is meaningful.
    stop_type: bool,
    /// Net-priced: needs two or more legs.
    net: bool,
}

impl OrderType {
    /// Submission rules for this type; `None` for types Schwab reports but
    /// does not accept on a new order.
    fn rules(self) -> Option<TypeRules> {
        let base = TypeRules {
            price: false,
            stop_price: false,
            trailing: false,
            price_link: false,
            stop_type: false,
            net: false,
        };
        Some(match self {
            Self::Market | Self::MarketOnClose => base,
            Self::Limit | Self::LimitOnClose => TypeRules {
                price: true,
                ..base
            },
            Self::Stop => TypeRules {
                stop_price: true,
                stop_type: true,
                ..base
            },
            Self::StopLimit => TypeRules {
                price: true,
                stop_price: true,
                stop_type: true,
                ..base
            },
            Self::TrailingStop => TypeRules {
                trailing: true,
                stop_type: true,
                ..base
            },
            Self::TrailingStopLimit => TypeRules {
                price: true,
                trailing: true,
                price_link: true,
                stop_type: true,
                ..base
            },
            Self::NetDebit | Self::NetCredit => TypeRules {
                price: true,
                net: true,
                ..base
            },
            Self::NetZero => TypeRules { net: true, ..base },
            Self::Cabinet | Self::NonMarketable | Self::Exercise | Self::Unknown => return None,
        })
    }
}

impl OrderRequest {
    /// Validates structure without silently rounding prices: every field
    /// the order type takes and none it does not, instructions that fit the
    /// asset, whole shares and contracts, canonical symbols, extended hours
    /// only for limit orders. Broker eligibility, buying power and
    /// asset-specific increments still require a preview.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_depth(1)
    }

    fn validate_depth(&self, depth: usize) -> Result<(), String> {
        use crate::common::known;
        if depth > MAX_STRATEGY_DEPTH {
            return Err(format!(
                "Order strategies nest at most {MAX_STRATEGY_DEPTH} levels"
            ));
        }
        known("session", self.session.as_ref())?;
        known("duration", self.duration.as_ref())?;
        known("orderType", self.order_type.as_ref())?;
        known(
            "complexOrderStrategyType",
            self.complex_order_strategy_type.as_ref(),
        )?;
        known("stopPriceLinkBasis", self.stop_price_link_basis.as_ref())?;
        known("stopPriceLinkType", self.stop_price_link_type.as_ref())?;
        known("stopType", self.stop_type.as_ref())?;
        known("priceLinkBasis", self.price_link_basis.as_ref())?;
        known("priceLinkType", self.price_link_type.as_ref())?;
        known("taxLotMethod", self.tax_lot_method.as_ref())?;
        known("specialInstruction", self.special_instruction.as_ref())?;
        known("requestedDestination", self.requested_destination.as_ref())?;
        known("orderStrategyType", Some(&self.order_strategy_type))?;
        for (name, value) in [
            ("quantity", self.quantity),
            ("price", self.price),
            ("stopPrice", self.stop_price),
            ("stopPriceOffset", self.stop_price_offset),
        ] {
            if value.is_some_and(|v| v <= rust_decimal::Decimal::ZERO) {
                return Err(format!("{name} must be positive"));
            }
        }
        match self.order_strategy_type {
            OrderStrategyType::Oco => self.validate_oco_parent()?,
            OrderStrategyType::Single | OrderStrategyType::Trigger => self.validate_executable()?,
            _ => return Err("Only SINGLE, OCO, and TRIGGER strategies can be submitted".into()),
        }
        for child in &self.child_order_strategies {
            child.validate_depth(depth + 1)?;
        }
        Ok(())
    }

    /// An OCO parent only groups its two children; anything executable on
    /// it would be ignored or misread.
    fn validate_oco_parent(&self) -> Result<(), String> {
        let executable = self.session.is_some()
            || self.duration.is_some()
            || self.order_type.is_some()
            || self.complex_order_strategy_type.is_some()
            || self.quantity.is_some()
            || self.price.is_some()
            || self.stop_price.is_some()
            || self.stop_price_link_basis.is_some()
            || self.stop_price_link_type.is_some()
            || self.stop_price_offset.is_some()
            || self.stop_type.is_some()
            || self.price_link_basis.is_some()
            || self.price_link_type.is_some()
            || self.tax_lot_method.is_some()
            || self.special_instruction.is_some()
            || self.requested_destination.is_some()
            || !self.order_leg_collection.is_empty();
        if self.child_order_strategies.len() != 2 || executable {
            return Err("OCO must contain exactly two children and no executable parent".into());
        }
        Ok(())
    }

    fn validate_executable(&self) -> Result<(), String> {
        let (Some(kind), Some(session), Some(duration), false) = (
            self.order_type,
            self.session,
            self.duration,
            self.order_leg_collection.is_empty(),
        ) else {
            return Err("Executable orders require legs, session, duration, and orderType".into());
        };
        let name = serde_json::to_value(kind).unwrap_or_default();
        let name = name.as_str().unwrap_or("this order type");
        let rules = kind
            .rules()
            .ok_or_else(|| format!("{name} orders cannot be submitted"))?;
        let field = |present: bool, required: bool, field: &str| -> Result<(), String> {
            match (present, required) {
                (false, true) => Err(format!("{name} requires {field}")),
                (true, false) => Err(format!("{name} does not take {field}")),
                _ => Ok(()),
            }
        };
        field(self.price.is_some(), rules.price, "price")?;
        field(self.stop_price.is_some(), rules.stop_price, "stopPrice")?;
        for (present, f) in [
            (self.stop_price_offset.is_some(), "stopPriceOffset"),
            (self.stop_price_link_basis.is_some(), "stopPriceLinkBasis"),
            (self.stop_price_link_type.is_some(), "stopPriceLinkType"),
        ] {
            field(present, rules.trailing, f)?;
        }
        if self.price_link_basis.is_some() != self.price_link_type.is_some() {
            return Err("priceLinkBasis and priceLinkType go together".into());
        }
        if self.price_link_basis.is_some() && !rules.price_link {
            return Err(format!("{name} does not take priceLinkBasis"));
        }
        if self.stop_type.is_some() && !rules.stop_type {
            return Err(format!("{name} does not take stopType"));
        }
        if rules.trailing {
            if self.stop_price_link_basis == Some(PriceLinkBasis::Manual) {
                return Err("A trailing stop cannot trail a MANUAL basis".into());
            }
            if self.stop_price_link_type == Some(PriceLinkType::Percent)
                && self
                    .stop_price_offset
                    .is_some_and(|o| o >= rust_decimal::Decimal::ONE_HUNDRED)
            {
                return Err("A percentage trailing offset must be below 100".into());
            }
        }
        if rules.net && self.order_leg_collection.len() < 2 {
            return Err(format!("{name} needs two or more legs"));
        }
        if session != OrderSession::Normal && kind != OrderType::Limit {
            return Err("Extended-hours sessions accept only LIMIT orders".into());
        }
        if matches!(kind, OrderType::MarketOnClose | OrderType::LimitOnClose)
            && (session != OrderSession::Normal || duration != OrderDuration::Day)
        {
            return Err(format!(
                "{name} requires the NORMAL session and DAY duration"
            ));
        }
        for leg in &self.order_leg_collection {
            leg.validate()?;
            if self.quantity.is_some_and(|q| q != leg.quantity) {
                return Err(
                    "quantity must equal every leg's quantity; size orders on the legs".into(),
                );
            }
        }
        match (
            self.order_strategy_type,
            self.child_order_strategies.is_empty(),
        ) {
            (OrderStrategyType::Single, false) => {
                Err("SINGLE cannot contain child strategies".into())
            }
            (OrderStrategyType::Trigger, true) => Err("TRIGGER requires child strategies".into()),
            _ => Ok(()),
        }
    }

    /// Enforce a caller-supplied broker/asset tick size on every price and
    /// on a trailing offset expressed as a value; never round implicitly.
    pub fn validate_price_increment(&self, tick: rust_decimal::Decimal) -> Result<(), String> {
        self.validate()?;
        self.validate_ticks(tick)
    }

    fn validate_ticks(&self, tick: rust_decimal::Decimal) -> Result<(), String> {
        if tick <= rust_decimal::Decimal::ZERO {
            return Err("Tick size must be finite and positive".into());
        }
        let value_offset = self
            .stop_price_offset
            .filter(|_| self.stop_price_link_type == Some(PriceLinkType::Value));
        for price in [self.price, self.stop_price, value_offset]
            .into_iter()
            .flatten()
        {
            if price % tick != rust_decimal::Decimal::ZERO {
                return Err("Price does not match the supplied tick size".into());
            }
        }
        for child in &self.child_order_strategies {
            child.validate_ticks(tick)?;
        }
        Ok(())
    }
}

impl OrderRequestLeg {
    /// The leg-level rules [`OrderRequest::validate`] applies to each leg.
    pub fn validate(&self) -> Result<(), String> {
        use crate::common::known;
        use OrderInstruction as I;
        known("instruction", Some(&self.instruction))?;
        known("positionEffect", self.position_effect.as_ref())?;
        known("assetType", Some(&self.instrument.asset_type))?;
        known("quantityType", self.quantity_type.as_ref())?;
        known("divCapGains", self.div_cap_gains.as_ref())?;
        let symbol = &self.instrument.symbol;
        if symbol.trim().is_empty()
            || symbol.trim() != symbol
            || symbol.bytes().any(|b| b.is_ascii_lowercase())
        {
            return Err(format!(
                "Instrument symbol {symbol:?} must be non-empty, upper case and unpadded"
            ));
        }
        if self.quantity <= rust_decimal::Decimal::ZERO {
            return Err("Leg quantity must be positive".into());
        }
        let whole = self.quantity.fract().is_zero();
        match self.instrument.asset_type {
            AssetType::Equity => {
                if !matches!(
                    self.instruction,
                    I::Buy | I::Sell | I::SellShort | I::BuyToCover | I::SellShortExempt
                ) {
                    return Err(format!(
                        "{:?} is an option instruction; equities take BUY, SELL, \
                         SELL_SHORT, SELL_SHORT_EXEMPT or BUY_TO_COVER",
                        self.instruction
                    ));
                }
                if !whole {
                    return Err("Equity orders are for whole shares".into());
                }
            }
            AssetType::Option => {
                if !matches!(
                    self.instruction,
                    I::BuyToOpen | I::BuyToClose | I::SellToOpen | I::SellToClose
                ) {
                    return Err(format!(
                        "{:?} is not an option instruction; options take BUY_TO_OPEN, \
                         BUY_TO_CLOSE, SELL_TO_OPEN or SELL_TO_CLOSE",
                        self.instruction
                    ));
                }
                symbol.parse::<OptionSymbol>()?;
                if !whole {
                    return Err("Option orders are for whole contracts".into());
                }
            }
            _ => {}
        }
        let fund = self.instrument.asset_type == AssetType::MutualFund;
        if !fund
            && (self.quantity_type.is_some()
                || self.div_cap_gains.is_some()
                || self.to_symbol.is_some())
        {
            return Err("quantityType, divCapGains and toSymbol apply to mutual funds only".into());
        }
        if (self.instruction == I::Exchange) != self.to_symbol.is_some() {
            return Err("EXCHANGE takes a toSymbol, and only EXCHANGE does".into());
        }
        Ok(())
    }
}
