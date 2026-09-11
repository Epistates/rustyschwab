//! Trading types including orders, transactions, and execution reports.
//!
//! This module contains types for trading operations:
//!
//! - Order creation and management
//! - Order status and responses
//! - Transaction history
//! - Execution reports
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
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::collections::HashMap;

mod builder;
mod spec;
pub use builder::OptionSymbol;
pub use spec::{InstrumentSpec, LegSpec, OrderSettings, OrderSpec};

/// An order as Schwab reports it. Tolerant: enums that only appear on
/// responses carry an `Unknown` catch-all, and undocumented fields land in
/// `extra`. Times are the broker's ISO-8601 strings, kept verbatim.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Order {
    #[serde(default, deserialize_with = "crate::common::optional_id")]
    pub order_id: Option<String>,
    #[serde(default)]
    pub session: OrderSession,
    #[serde(default)]
    pub duration: OrderDuration,
    #[serde(default)]
    pub order_type: OrderType,
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
pub enum OrderActivityType {
    Execution,
    OrderAction,
    #[serde(other)]
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
pub enum QuantityType {
    AllShares,
    Dollars,
    Shares,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderSession {
    #[default]
    Normal,
    Am,
    Pm,
    Seamless,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderType {
    #[default]
    Market,
    Limit,
    Stop,
    StopLimit,
    TrailingStop,
    Cabinet,
    NonMarketable,
    MarketOnClose,
    Exercise,
    TrailingStopLimit,
    NetDebit,
    NetCredit,
    NetZero,
    LimitOnClose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
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
}

/// Only [`Single`](Self::Single), [`Oco`](Self::Oco), and
/// [`Trigger`](Self::Trigger) can be submitted; the rest appear on orders
/// Schwab reports back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderStrategyType {
    #[default]
    Single,
    Cancel,
    Recall,
    Pair,
    Flatten,
    TwoDaySwap,
    BlastAll,
    Oco,
    Trigger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
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
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PositionEffect {
    Opening,
    Closing,
    #[default]
    Automatic,
}

/// What a linked (trailing) price follows: `priceLinkBasis` and
/// `stopPriceLinkBasis`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
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
}

/// How a linked price's offset is expressed: `priceLinkType` and
/// `stopPriceLinkType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PriceLinkType {
    Value,
    Percent,
    Tick,
}

/// Which price a stop order triggers on (`stopType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StopType {
    Standard,
    Bid,
    Ask,
    Last,
    Mark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaxLotMethod {
    Fifo,
    Lifo,
    HighCost,
    LowCost,
    AverageCost,
    SpecificLot,
    LossHarvester,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SpecialInstruction {
    AllOrNone,
    DoNotReduce,
    AllOrNoneDoNotReduce,
}

/// Routing venue for `requestedDestination`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
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
    /// `None` for a value outside [`TransactionType`], which is also the
    /// request-side filter and therefore has no catch-all.
    #[serde(rename = "type", default, deserialize_with = "crate::common::lenient")]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TransactionStatus {
    Valid,
    Invalid,
    Pending,
    #[serde(other)]
    Unknown,
}

/// Ledger a transaction settled against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SubAccount {
    Cash,
    Margin,
    Short,
    Div,
    Income,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TransactionActivityType {
    ActivityCorrection,
    Execution,
    OrderAction,
    Transfer,
    #[serde(other)]
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
    /// `None` when Schwab reports `UNKNOWN`.
    #[serde(default, deserialize_with = "crate::common::lenient")]
    pub position_effect: Option<PositionEffect>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
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

/// Outgoing orders have a strict schema, separate from tolerant broker responses.
/// Unsupported fields fail deserialization rather than silently altering a trade.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrderRequest {
    pub session: Option<OrderSession>,
    pub duration: Option<OrderDuration>,
    pub order_type: Option<OrderType>,
    pub complex_order_strategy_type: Option<ComplexOrderStrategyType>,
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub quantity: Option<rust_decimal::Decimal>,
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub price: Option<rust_decimal::Decimal>,
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub stop_price: Option<rust_decimal::Decimal>,
    pub stop_price_link_basis: Option<PriceLinkBasis>,
    pub stop_price_link_type: Option<PriceLinkType>,
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrderRequestLeg {
    pub instruction: OrderInstruction,
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub quantity: rust_decimal::Decimal,
    pub instrument: OrderRequestInstrument,
    pub position_effect: Option<PositionEffect>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrderRequestInstrument {
    pub symbol: String,
    pub asset_type: AssetType,
}

impl OrderRequest {
    /// Validates structural safety without silently rounding prices. Broker
    /// eligibility, buying power, and asset-specific increments require preview.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_depth(0)
    }
    fn validate_depth(&self, depth: usize) -> Result<(), String> {
        if depth > 8 {
            return Err("Order strategy nesting exceeds eight levels".into());
        }
        for (name, value) in [
            ("quantity", self.quantity),
            ("price", self.price),
            ("stopPrice", self.stop_price),
            ("stopPriceOffset", self.stop_price_offset),
        ] {
            if value.is_some_and(|v| v <= rust_decimal::Decimal::ZERO) {
                return Err(format!("{name} must be finite and positive"));
            }
        }
        if self.order_strategy_type == OrderStrategyType::Oco {
            if self.child_order_strategies.len() != 2
                || !self.order_leg_collection.is_empty()
                || self.order_type.is_some()
                || self.price.is_some()
                || self.stop_price.is_some()
            {
                return Err(
                    "OCO must contain exactly two children and no executable parent".into(),
                );
            }
        } else {
            if !matches!(
                self.order_strategy_type,
                OrderStrategyType::Single | OrderStrategyType::Trigger
            ) {
                return Err("Only SINGLE, OCO, and TRIGGER strategies can be submitted".into());
            }
            let (Some(kind), Some(_), Some(_), false) = (
                self.order_type,
                self.session,
                self.duration,
                self.order_leg_collection.is_empty(),
            ) else {
                return Err(
                    "Executable orders require legs, session, duration, and orderType".into(),
                );
            };
            if matches!(
                kind,
                OrderType::Limit
                    | OrderType::StopLimit
                    | OrderType::TrailingStopLimit
                    | OrderType::NetDebit
                    | OrderType::NetCredit
                    | OrderType::LimitOnClose
            ) && self.price.is_none()
            {
                return Err("This order type requires price".into());
            }
            if matches!(kind, OrderType::Stop | OrderType::StopLimit) && self.stop_price.is_none() {
                return Err("Stop orders require stopPrice".into());
            }
            if matches!(kind, OrderType::TrailingStop | OrderType::TrailingStopLimit)
                && (self.stop_price_offset.is_none()
                    || self.stop_price_link_basis.is_none()
                    || self.stop_price_link_type.is_none())
            {
                return Err("Trailing stops require offset, basis, and link type".into());
            }
            for leg in &self.order_leg_collection {
                if leg.quantity <= rust_decimal::Decimal::ZERO
                    || leg.instrument.symbol.trim().is_empty()
                {
                    return Err(
                        "Leg quantity must be finite/positive and instrument must be specified"
                            .into(),
                    );
                }
            }
            if self.order_strategy_type == OrderStrategyType::Single
                && !self.child_order_strategies.is_empty()
            {
                return Err("SINGLE cannot contain child strategies".into());
            }
            if self.order_strategy_type == OrderStrategyType::Trigger
                && self.child_order_strategies.is_empty()
            {
                return Err("TRIGGER requires child strategies".into());
            }
        }
        for child in &self.child_order_strategies {
            child.validate_depth(depth + 1)?;
        }
        Ok(())
    }

    /// Enforce a caller-supplied broker/asset tick size; never round implicitly.
    pub fn validate_price_increment(&self, tick: rust_decimal::Decimal) -> Result<(), String> {
        self.validate()?;
        if tick <= rust_decimal::Decimal::ZERO {
            return Err("Tick size must be finite and positive".into());
        }
        for price in [self.price, self.stop_price].into_iter().flatten() {
            if price % tick != rust_decimal::Decimal::ZERO {
                return Err("Price does not match the supplied tick size".into());
            }
        }
        for child in &self.child_order_strategies {
            child.validate_price_increment(tick)?;
        }
        Ok(())
    }
}
