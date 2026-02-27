//! Account types including balances, positions, and user preferences.
//!
//! This module contains types for account-related API responses:
//!
//! - Account details and balances
//! - Position information
//! - User preferences and streamer info

#![allow(missing_docs)] // DTO fields are self-documenting via Schwab API docs

use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountNumberHash {
    pub account_number: String,
    pub hash_value: String,
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub securities_account: SecuritiesAccount,
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecuritiesAccount {
    pub account_number: String,
    pub account_type: Option<String>,
    pub round_trips: Option<i32>,
    pub is_day_trader: Option<bool>,
    pub is_closing_only_restricted: Option<bool>,
    pub positions: Option<Vec<Position>>,
    pub initial_balances: Option<Balance>,
    pub current_balances: Option<Balance>,
    pub projected_balances: Option<Balance>,
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub instrument: Option<PositionInstrument>,
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
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionInstrument {
    pub asset_type: String,
    pub cusip: Option<String>,
    pub symbol: String,
    pub description: Option<String>,
    pub instrument_id: Option<i64>,
    pub net_change: Option<f64>,
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountsResponse {
    pub accounts: Vec<Account>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserPreferences {
    pub accounts: Option<Vec<AccountPreferences>>,
    pub streamer_info: Option<Vec<StreamerInfo>>,
    pub offers: Option<Vec<Offer>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountPreferences {
    pub account_number: String,
    pub primary: bool,
    pub account_type: String,
    pub account_color: String,
    pub display_name: String,
    pub auto_position_effect: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamerInfo {
    pub streamer_socket_url: String,
    pub schwab_client_customer_id: String,
    pub schwab_client_correl_id: String,
    pub schwab_client_channel: String,
    pub schwab_client_function_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Offer {
    pub level2_permissions: bool,
    pub mkt_data_permission: String,
}