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

use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::collections::HashMap;

mod builder;
mod spec;
pub use builder::OptionSymbol;
pub use spec::{InstrumentSpec, LegSpec, OrderSettings, OrderSpec};

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
    pub requested_destination: Option<String>,
    pub destination_link_name: Option<String>,
    pub price: Option<f64>,
    #[serde(default)]
    pub order_leg_collection: Vec<OrderLeg>,
    pub order_strategy_type: Option<OrderStrategyType>,
    pub cancelable: Option<bool>,
    pub editable: Option<bool>,
    pub status: Option<OrderStatus>,
    pub entered_time: Option<String>,
    pub close_time: Option<String>,
    pub stop_price: Option<f64>,
    pub stop_price_link_basis: Option<String>,
    pub stop_price_link_type: Option<String>,
    pub stop_price_offset: Option<f64>,
    #[serde(default)]
    pub child_order_strategies: Vec<Order>,
    #[serde(default)]
    pub order_activity_collection: Vec<serde_json::Value>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
    #[serde(default, deserialize_with = "crate::common::optional_id")]
    pub account_number: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct OrderLeg {
    pub order_leg_type: OrderLegType,
    pub leg_id: Option<i64>,
    pub instrument: OrderInstrument,
    pub instruction: OrderInstruction,
    pub position_effect: Option<PositionEffect>,
    pub quantity: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct OrderInstrument {
    pub asset_type: String,
    pub cusip: Option<String>,
    pub symbol: String,
    pub description: Option<String>,
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

#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    pub activity_id: Option<i64>,
    pub time: Option<String>,
    pub user: Option<TransactionUser>,
    pub description: Option<String>,
    #[serde(default, deserialize_with = "crate::common::optional_id")]
    pub account_number: Option<String>,
    #[serde(rename = "type")]
    pub type_: Option<String>,
    pub status: Option<String>,
    pub sub_account: Option<String>,
    pub position_id: Option<i64>,
    pub order_id: Option<i64>,
    pub net_amount: Option<f64>,
    pub principal: Option<f64>,
    #[serde(rename = "accruedInterest")]
    pub accrued_interest: Option<f64>,
    pub fees: Option<HashMap<String, f64>>,
    pub instrument: Option<TransactionInstrument>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct TransactionInstrument {
    pub symbol: String,
    pub underlying_symbol: Option<String>,
    pub option_expiration_date: Option<String>,
    pub option_strike_price: Option<f64>,
    pub put_call: Option<String>,
    pub cusip: Option<String>,
    pub description: Option<String>,
    pub asset_type: Option<String>,
    pub bond_maturity_date: Option<String>,
    pub bond_interest_rate: Option<f64>,
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
    pub asset_type: String,
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
                    || leg.instrument.asset_type.trim().is_empty()
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
