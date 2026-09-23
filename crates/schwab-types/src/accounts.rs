//! Account types including balances, positions, and user preferences.
//!
//! This module contains types for account-related API responses:
//!
//! - Account details and balances
//! - Position information
//! - User preferences and streamer info

//!
//! Response types keep fields this crate does not model in `extra` (Schwab
//! adds them, for example `aggregatedBalance`), except
//! [`StreamerInfo`](crate::accounts::StreamerInfo),
//! whose unmodelled fields could be credentials.

use crate::common::AccountsInstrument;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::collections::HashMap;

type Extra = HashMap<String, serde_json::Value>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct AccountNumberHash {
    pub account_number: String,
    pub hash_value: String,
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub securities_account: SecuritiesAccount,
    #[serde(flatten)]
    pub extra: Extra,
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct SecuritiesAccount {
    pub account_number: String,
    #[serde(rename = "type")]
    pub account_type: Option<String>,
    pub round_trips: Option<i32>,
    pub is_day_trader: Option<bool>,
    pub is_closing_only_restricted: Option<bool>,
    pub positions: Option<Vec<Position>>,
    pub initial_balances: Option<Balance>,
    pub current_balances: Option<Balance>,
    pub projected_balances: Option<Balance>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub short_quantity: Option<f64>,
    pub average_price: Option<f64>,
    pub current_day_cost: Option<f64>,
    pub current_day_profit_loss: Option<f64>,
    pub current_day_profit_loss_percentage: Option<f64>,
    pub long_quantity: Option<f64>,
    pub settled_long_quantity: Option<f64>,
    pub settled_short_quantity: Option<f64>,
    pub instrument: Option<AccountsInstrument>,
    pub market_value: Option<f64>,
    pub maintenance_requirement: Option<f64>,
    pub average_long_price: Option<f64>,
    pub average_short_price: Option<f64>,
    pub tax_lot_average_long_price: Option<f64>,
    pub tax_lot_average_short_price: Option<f64>,
    pub long_open_profit_loss: Option<f64>,
    pub short_open_profit_loss: Option<f64>,
    pub previous_session_long_quantity: Option<f64>,
    pub previous_session_short_quantity: Option<f64>,
    pub current_cost: Option<f64>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// One of an account's three balance blocks. Margin and cash accounts
/// report different fields, and the initial, current and projected blocks
/// each carry a subset, so everything is optional: a cash account's
/// spending power is `cash_available_for_trading`, a margin account's is
/// `buying_power` and `available_funds`.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Balance {
    pub accrued_interest: Option<f64>,
    pub cash_balance: Option<f64>,
    pub cash_receipts: Option<f64>,
    pub long_option_market_value: Option<f64>,
    pub liquidation_value: Option<f64>,
    pub long_market_value: Option<f64>,
    pub money_market_fund: Option<f64>,
    pub savings: Option<f64>,
    pub short_market_value: Option<f64>,
    pub pending_deposits: Option<f64>,
    pub available_funds: Option<f64>,
    pub available_funds_non_marginable_trade: Option<f64>,
    pub buying_power: Option<f64>,
    pub buying_power_non_marginable_trade: Option<f64>,
    pub day_trading_buying_power: Option<f64>,
    pub day_trading_buying_power_call: Option<f64>,
    pub equity: Option<f64>,
    pub equity_percentage: Option<f64>,
    pub long_margin_value: Option<f64>,
    pub maintenance_call: Option<f64>,
    pub maintenance_requirement: Option<f64>,
    pub margin: Option<f64>,
    pub margin_equity: Option<f64>,
    pub reg_t_call: Option<f64>,
    pub short_balance: Option<f64>,
    pub short_margin_value: Option<f64>,
    pub short_option_market_value: Option<f64>,
    pub sma: Option<f64>,
    pub mutual_fund_value: Option<f64>,
    pub bond_value: Option<f64>,
    pub margin_balance: Option<f64>,
    pub long_stock_value: Option<f64>,
    pub short_stock_value: Option<f64>,
    pub account_value: Option<f64>,
    pub day_trading_equity_call: Option<f64>,
    /// Projected block of a margin account.
    pub stock_buying_power: Option<f64>,
    /// Projected block: the account is in a margin or cash call.
    pub is_in_call: Option<bool>,
    /// Cash accounts (and a margin account's initial block).
    pub cash_available_for_trading: Option<f64>,
    /// Cash accounts.
    pub cash_available_for_withdrawal: Option<f64>,
    /// Cash accounts.
    pub cash_call: Option<f64>,
    /// Cash accounts.
    pub cash_debit_call_value: Option<f64>,
    /// Cash accounts.
    pub long_non_marginable_market_value: Option<f64>,
    /// Cash accounts.
    pub total_cash: Option<f64>,
    /// Cash accounts.
    pub unsettled_cash: Option<f64>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct AccountsResponse {
    pub accounts: Vec<Account>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct UserPreferences {
    pub accounts: Option<Vec<AccountPreferences>>,
    pub streamer_info: Option<Vec<StreamerInfo>>,
    pub offers: Option<Vec<Offer>>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct AccountPreferences {
    pub account_number: Option<String>,
    pub primary_account: Option<bool>,
    #[serde(rename = "type")]
    pub account_type: Option<String>,
    pub nick_name: Option<String>,
    pub account_color: Option<String>,
    pub display_acct_id: Option<String>,
    pub auto_position_effect: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct StreamerInfo {
    pub streamer_socket_url: String,
    pub schwab_client_customer_id: String,
    pub schwab_client_correl_id: String,
    pub schwab_client_channel: String,
    pub schwab_client_function_id: String,
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Offer {
    pub level2_permissions: Option<bool>,
    pub mkt_data_permission: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}
