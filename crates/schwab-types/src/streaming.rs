//! Real-time streaming WebSocket types for the Schwab Streamer API.
//!
//! This module contains types for the WebSocket streaming interface:
//!
//! - Request types for subscriptions and commands
//! - Response types for acknowledgments
//! - Data types for real-time market data
//! - Service enums and field constants
//!
//! # Services
//!
//! The streaming API supports multiple services:
//! - `LEVELONE_EQUITIES` - Real-time equity quotes
//! - `LEVELONE_OPTIONS` - Real-time option quotes
//! - `CHART_EQUITY` - Real-time chart data
//! - `ACCT_ACTIVITY` - Account activity notifications

#![allow(missing_docs)] // DTO fields are self-documenting via Schwab API docs

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamRequest {
    pub requestid: String,
    pub service: String,
    pub command: StreamCommand,
    #[serde(rename = "SchwabClientCustomerId")]
    pub schwab_client_customer_id: String,
    #[serde(rename = "SchwabClientCorrelId")]
    pub schwab_client_correl_id: String,
    pub parameters: StreamParameters,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamRequests {
    pub requests: Vec<StreamRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum StreamCommand {
    Login,
    Logout,
    Subs,
    Add,
    Unsubs,
    View,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamParameters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keys: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<String>,
    #[serde(rename = "Authorization", skip_serializing_if = "Option::is_none")]
    pub authorization: Option<String>,
    #[serde(rename = "SchwabClientChannel", skip_serializing_if = "Option::is_none")]
    pub schwab_client_channel: Option<String>,
    #[serde(rename = "SchwabClientFunctionId", skip_serializing_if = "Option::is_none")]
    pub schwab_client_function_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamMessage {
    Response(StreamResponse),
    Data(StreamData),
    Notify(StreamNotify),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamResponse {
    pub response: Vec<ResponseItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseItem {
    pub service: String,
    pub command: String,
    pub requestid: String,
    #[serde(rename = "SchwabClientCorrelId")]
    pub schwab_client_correl_id: String,
    pub timestamp: i64,
    pub content: ResponseContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseContent {
    pub code: i32,
    pub msg: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamData {
    pub data: Vec<DataItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataItem {
    pub service: String,
    pub timestamp: i64,
    pub command: String,
    pub content: Vec<DataContent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataContent {
    pub key: String,
    pub delayed: bool,
    #[serde(rename = "assetMainType", skip_serializing_if = "Option::is_none")]
    pub asset_main_type: Option<String>,
    #[serde(rename = "assetSubType", skip_serializing_if = "Option::is_none")]
    pub asset_sub_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cusip: Option<String>,
    #[serde(flatten)]
    pub fields: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamNotify {
    pub notify: Vec<NotifyItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StreamService {
    Admin,
    LeveloneEquities,
    LeveloneOptions,
    LeveloneFutures,
    LeveloneFuturesOptions,
    LeveloneForex,
    NyseBook,
    NasdaqBook,
    OptionsBook,
    ChartEquity,
    ChartFutures,
    ScreenerEquity,
    ScreenerOption,
    AcctActivity,
}

impl StreamService {
    pub fn as_str(&self) -> &str {
        match self {
            StreamService::Admin => "ADMIN",
            StreamService::LeveloneEquities => "LEVELONE_EQUITIES",
            StreamService::LeveloneOptions => "LEVELONE_OPTIONS",
            StreamService::LeveloneFutures => "LEVELONE_FUTURES",
            StreamService::LeveloneFuturesOptions => "LEVELONE_FUTURES_OPTIONS",
            StreamService::LeveloneForex => "LEVELONE_FOREX",
            StreamService::NyseBook => "NYSE_BOOK",
            StreamService::NasdaqBook => "NASDAQ_BOOK",
            StreamService::OptionsBook => "OPTIONS_BOOK",
            StreamService::ChartEquity => "CHART_EQUITY",
            StreamService::ChartFutures => "CHART_FUTURES",
            StreamService::ScreenerEquity => "SCREENER_EQUITY",
            StreamService::ScreenerOption => "SCREENER_OPTION",
            StreamService::AcctActivity => "ACCT_ACTIVITY",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LevelOneEquityFields;

impl LevelOneEquityFields {
    pub const SYMBOL: u32 = 0;
    pub const BID_PRICE: u32 = 1;
    pub const ASK_PRICE: u32 = 2;
    pub const LAST_PRICE: u32 = 3;
    pub const BID_SIZE: u32 = 4;
    pub const ASK_SIZE: u32 = 5;
    pub const ASK_ID: u32 = 6;
    pub const BID_ID: u32 = 7;
    pub const TOTAL_VOLUME: u32 = 8;
    pub const LAST_SIZE: u32 = 9;
    pub const HIGH_PRICE: u32 = 10;
    pub const LOW_PRICE: u32 = 11;
    pub const CLOSE_PRICE: u32 = 12;
    pub const EXCHANGE_ID: u32 = 13;
    pub const MARGINABLE: u32 = 14;
    pub const DESCRIPTION: u32 = 15;
    pub const LAST_ID: u32 = 16;
    pub const OPEN_PRICE: u32 = 17;
    pub const NET_CHANGE: u32 = 18;
    pub const HIGH_52_WEEK: u32 = 19;
    pub const LOW_52_WEEK: u32 = 20;
    pub const PE_RATIO: u32 = 21;
    pub const DIV_AMOUNT: u32 = 22;
    pub const DIV_YIELD: u32 = 23;
    pub const NAV: u32 = 24;
    pub const EXCHANGE_NAME: u32 = 25;
    pub const DIV_DATE: u32 = 26;
    pub const IS_REGULAR_MARKET_QUOTE: u32 = 27;
    pub const IS_REGULAR_MARKET_TRADE: u32 = 28;
    pub const REGULAR_MARKET_LAST_PRICE: u32 = 29;
    pub const REGULAR_MARKET_LAST_SIZE: u32 = 30;
    pub const REGULAR_MARKET_NET_CHANGE: u32 = 31;
    pub const SECURITY_STATUS: u32 = 32;
    pub const MARK: u32 = 33;
    pub const QUOTE_TIME_MILLIS: u32 = 34;
    pub const TRADE_TIME_MILLIS: u32 = 35;
    pub const REGULAR_MARKET_TRADE_MILLIS: u32 = 36;
    pub const BID_TIME_MILLIS: u32 = 37;
    pub const ASK_TIME_MILLIS: u32 = 38;
    pub const ASK_MIC_ID: u32 = 39;
    pub const BID_MIC_ID: u32 = 40;
    pub const LAST_MIC_ID: u32 = 41;
    pub const NET_CHANGE_PERCENT: u32 = 42;
    pub const REGULAR_MARKET_CHANGE_PERCENT: u32 = 43;
    pub const MARK_CHANGE: u32 = 44;
    pub const MARK_CHANGE_PERCENT: u32 = 45;
    pub const HTB_QUANTITY: u32 = 46;
    pub const HTB_RATE: u32 = 47;
    pub const HARD_TO_BORROW: u32 = 48;
    pub const IS_SHORTABLE: u32 = 49;
    pub const POST_MARKET_NET_CHANGE: u32 = 50;
    pub const POST_MARKET_CHANGE_PERCENT: u32 = 51;

    pub fn all_fields() -> String {
        (0..=51).map(|i| i.to_string()).collect::<Vec<_>>().join(",")
    }

    pub fn default_fields() -> String {
        vec![
            Self::SYMBOL,
            Self::BID_PRICE,
            Self::ASK_PRICE,
            Self::LAST_PRICE,
            Self::BID_SIZE,
            Self::ASK_SIZE,
            Self::TOTAL_VOLUME,
            Self::LAST_SIZE,
            Self::HIGH_PRICE,
            Self::LOW_PRICE,
            Self::CLOSE_PRICE,
            Self::OPEN_PRICE,
            Self::NET_CHANGE,
            Self::NET_CHANGE_PERCENT,
            Self::QUOTE_TIME_MILLIS,
            Self::TRADE_TIME_MILLIS,
        ]
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
    }
}

#[derive(Debug, Clone)]
pub struct LevelOneOptionFields;

impl LevelOneOptionFields {
    pub const SYMBOL: u32 = 0;
    pub const DESCRIPTION: u32 = 1;
    pub const BID_PRICE: u32 = 2;
    pub const ASK_PRICE: u32 = 3;
    pub const LAST_PRICE: u32 = 4;
    pub const HIGH_PRICE: u32 = 5;
    pub const LOW_PRICE: u32 = 6;
    pub const CLOSE_PRICE: u32 = 7;
    pub const TOTAL_VOLUME: u32 = 8;
    pub const OPEN_INTEREST: u32 = 9;
    pub const VOLATILITY: u32 = 10;
    pub const INTRINSIC_VALUE: u32 = 11;
    pub const EXP_YEAR: u32 = 12;
    pub const MULTIPLIER: u32 = 13;
    pub const DIGITS: u32 = 14;
    pub const OPEN_PRICE: u32 = 15;
    pub const BID_SIZE: u32 = 16;
    pub const ASK_SIZE: u32 = 17;
    pub const LAST_SIZE: u32 = 18;
    pub const NET_CHANGE: u32 = 19;
    pub const STRIKE_PRICE: u32 = 20;
    pub const CONTRACT_TYPE: u32 = 21;
    pub const UNDERLYING: u32 = 22;
    pub const EXP_MONTH: u32 = 23;
    pub const DELIVERABLES: u32 = 24;
    pub const TIME_VALUE: u32 = 25;
    pub const EXP_DAY: u32 = 26;
    pub const DAYS_TO_EXP: u32 = 27;
    pub const DELTA: u32 = 28;
    pub const GAMMA: u32 = 29;
    pub const THETA: u32 = 30;
    pub const VEGA: u32 = 31;
    pub const RHO: u32 = 32;
    pub const SECURITY_STATUS: u32 = 33;
    pub const THEORETICAL_VALUE: u32 = 34;
    pub const UNDERLYING_PRICE: u32 = 35;
    pub const MARK: u32 = 37;
    pub const QUOTE_TIME_MILLIS: u32 = 38;
    pub const TRADE_TIME_MILLIS: u32 = 39;

    pub fn default_fields() -> String {
        (0..=39).map(|i| i.to_string()).collect::<Vec<_>>().join(",")
    }
}

#[derive(Debug, Clone)]
pub struct ChartEquityFields;

impl ChartEquityFields {
    pub const KEY: u32 = 0;
    pub const SEQUENCE: u32 = 1;
    pub const OPEN_PRICE: u32 = 2;
    pub const HIGH_PRICE: u32 = 3;
    pub const LOW_PRICE: u32 = 4;
    pub const CLOSE_PRICE: u32 = 5;
    pub const VOLUME: u32 = 6;
    pub const CHART_TIME: u32 = 7;
    pub const CHART_DAY: u32 = 8;

    pub fn default_fields() -> String {
        (0..=8).map(|i| i.to_string()).collect::<Vec<_>>().join(",")
    }
}

#[derive(Debug, Clone)]
pub struct AcctActivityFields;

impl AcctActivityFields {
    pub const SEQUENCE: &str = "seq";
    pub const KEY: &str = "key";
    pub const ACCOUNT: u32 = 1;
    pub const MESSAGE_TYPE: u32 = 2;
    pub const MESSAGE_DATA: u32 = 3;

    pub fn default_fields() -> String {
        "0,1,2,3".to_string()
    }
}

#[derive(Debug, Clone)]
pub enum StreamResponseCode {
    Success = 0,
    LoginDenied = 3,
    UnknownFailure = 9,
    ServiceNotAvailable = 11,
    CloseConnection = 12,
    ReachedSymbolLimit = 19,
    StreamConnNotFound = 20,
    BadCommandFormat = 21,
    FailedCommandSubs = 22,
    FailedCommandUnsubs = 23,
    FailedCommandAdd = 24,
    FailedCommandView = 25,
    SucceededCommandSubs = 26,
    SucceededCommandUnsubs = 27,
    SucceededCommandAdd = 28,
    SucceededCommandView = 29,
    StopStreaming = 30,
}

impl StreamResponseCode {
    pub fn from_code(code: i32) -> Option<Self> {
        match code {
            0 => Some(Self::Success),
            3 => Some(Self::LoginDenied),
            9 => Some(Self::UnknownFailure),
            11 => Some(Self::ServiceNotAvailable),
            12 => Some(Self::CloseConnection),
            19 => Some(Self::ReachedSymbolLimit),
            20 => Some(Self::StreamConnNotFound),
            21 => Some(Self::BadCommandFormat),
            22 => Some(Self::FailedCommandSubs),
            23 => Some(Self::FailedCommandUnsubs),
            24 => Some(Self::FailedCommandAdd),
            25 => Some(Self::FailedCommandView),
            26 => Some(Self::SucceededCommandSubs),
            27 => Some(Self::SucceededCommandUnsubs),
            28 => Some(Self::SucceededCommandAdd),
            29 => Some(Self::SucceededCommandView),
            30 => Some(Self::StopStreaming),
            _ => None,
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(
            self,
            Self::Success
                | Self::SucceededCommandSubs
                | Self::SucceededCommandUnsubs
                | Self::SucceededCommandAdd
                | Self::SucceededCommandView
        )
    }

    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::UnknownFailure | Self::ServiceNotAvailable | Self::StreamConnNotFound
        )
    }
}