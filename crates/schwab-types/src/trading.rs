//! Trading types including orders, transactions, and execution reports.
//!
//! This module contains types for trading operations:
//!
//! - Order creation and management
//! - Order status and responses
//! - Transaction history
//! - Execution reports
//!
//! # Order Types
//!
//! - Market, Limit, Stop, StopLimit
//! - TrailingStop, TrailingStopLimit
//! - Various complex strategies (spreads, straddles, etc.)

#![allow(missing_docs)] // DTO fields are self-documenting via Schwab API docs

use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::collections::HashMap;

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Order {
    pub order_id: Option<String>,
    pub session: OrderSession,
    pub duration: OrderDuration,
    pub order_type: OrderType,
    pub complex_order_strategy_type: Option<ComplexOrderStrategyType>,
    pub quantity: Option<f64>,
    pub filled_quantity: Option<f64>,
    pub remaining_quantity: Option<f64>,
    pub requested_destination: Option<String>,
    pub destination_link_name: Option<String>,
    pub price: Option<f64>,
    pub order_leg_collection: Vec<OrderLeg>,
    pub order_strategy_type: Option<OrderStrategyType>,
    pub cancelable: Option<bool>,
    pub editable: Option<bool>,
    pub status: Option<OrderStatus>,
    pub entered_time: Option<String>,
    pub close_time: Option<String>,
    pub account_number: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OrderLeg {
    pub order_leg_type: OrderLegType,
    pub leg_id: Option<i64>,
    pub instrument: OrderInstrument,
    pub instruction: OrderInstruction,
    pub position_effect: Option<PositionEffect>,
    pub quantity: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OrderInstrument {
    pub asset_type: String,
    pub cusip: Option<String>,
    pub symbol: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderSession {
    #[default]
    Normal,
    Am,
    Pm,
    Seamless,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderDuration {
    #[default]
    Day,
    Gtc,
    FillOrKill,
    ImmediateOrCancel,
    EndOfWeek,
    EndOfMonth,
    NextEndOfMonth,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderType {
    #[default]
    Market,
    Limit,
    Stop,
    StopLimit,
    TrailingStop,
    CabinetOrder,
    NonMarketable,
    MarketOnClose,
    Exercise,
    TrailingStopLimit,
    NetDebit,
    NetCredit,
    NetZero,
    LimitOnClose,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    DoubleCalendar,
    UnbalancedButterflySpread,
    UnbalancedCondorSpread,
    UnbalancedIronCondor,
    UnbalancedVerticalRoll,
    MutualFundSwap,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderStrategyType {
    Single,
    Oco,
    Trigger,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderStatus {
    Awaiting,
    AwaitingCondition,
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
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PositionEffect {
    Opening,
    Closing,
    #[default]
    Automatic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderResponse {
    pub order_id: String,
    pub status: String,
    pub account_number: String,
    pub entered_time: String,
    pub close_time: Option<String>,
    pub status_description: Option<String>,
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    pub activity_id: Option<i64>,
    pub time: Option<String>,
    pub user: Option<TransactionUser>,
    pub description: Option<String>,
    pub account_number: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionUser {
    pub cd_domain_id: String,
    pub login: String,
    pub type_: String,
    pub user_id: i64,
    pub system_user_name: String,
    pub first_name: String,
    pub last_name: String,
    pub broker_rep_code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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