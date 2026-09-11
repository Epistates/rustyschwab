//! Real-time streaming WebSocket types for the Schwab Streamer API.
//!
//! This module contains types for the WebSocket streaming interface:
//!
//! - Request types for subscriptions and commands
//! - Response types for acknowledgments
//! - Data types for real-time market data
//! - Service enums, field tables, and typed payloads per service
//!
//! # Services
//!
//! The streaming API supports multiple services:
//! - `LEVELONE_EQUITIES` - Real-time equity quotes
//! - `LEVELONE_OPTIONS` - Real-time option quotes
//! - `CHART_EQUITY` - Real-time chart data
//! - `ACCT_ACTIVITY` - Account activity notifications
//!
//! Data frames arrive with numbered fields
//! ([`DataContent::fields`](crate::streaming::DataContent::fields));
//! [`DataItem::decode`](crate::streaming::DataItem::decode) and the
//! per-service `from_content` constructors turn them into
//! [`StreamPayload`](crate::streaming::StreamPayload) values with named fields.

mod payload;

pub use payload::*;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct StreamRequests {
    pub requests: Vec<StreamRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct StreamParameters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keys: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<String>,
    #[serde(rename = "Authorization", skip_serializing_if = "Option::is_none")]
    pub authorization: Option<String>,
    #[serde(
        rename = "SchwabClientChannel",
        skip_serializing_if = "Option::is_none"
    )]
    pub schwab_client_channel: Option<String>,
    #[serde(
        rename = "SchwabClientFunctionId",
        skip_serializing_if = "Option::is_none"
    )]
    pub schwab_client_function_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum StreamMessage {
    #[serde(skip_deserializing)]
    State(StreamStatus),
    Response(StreamResponse),
    Data(StreamData),
    Notify(StreamNotify),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct StreamResponse {
    pub response: Vec<ResponseItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ResponseContent {
    pub code: i32,
    pub msg: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct StreamData {
    pub data: Vec<DataItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct DataItem {
    pub service: String,
    pub timestamp: i64,
    pub command: String,
    pub content: Vec<DataContent>,
}

/// One item of a data frame as the streamer sends it: the subscription
/// `key`, the service's envelope keys, and the numbered fields.
///
/// Decode into named, typed fields with [`StreamService::decode`] or a
/// payload's `from_content` (for example [`LevelOneEquity::from_content`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DataContent {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delayed: Option<bool>,
    #[serde(rename = "assetMainType", skip_serializing_if = "Option::is_none")]
    pub asset_main_type: Option<String>,
    #[serde(rename = "assetSubType", skip_serializing_if = "Option::is_none")]
    pub asset_sub_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cusip: Option<String>,
    /// The numbered fields (`"1"`, `"2"`, …) plus any other key the service
    /// sends, such as `seq` on chart and account-activity items.
    #[serde(flatten)]
    pub fields: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct StreamNotify {
    pub notify: Vec<NotifyItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct NotifyItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
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

    /// Parse a service from its API wire name (e.g. `LEVELONE_EQUITIES`).
    pub fn from_api_str(s: &str) -> Option<Self> {
        match s {
            "ADMIN" => Some(StreamService::Admin),
            "LEVELONE_EQUITIES" => Some(StreamService::LeveloneEquities),
            "LEVELONE_OPTIONS" => Some(StreamService::LeveloneOptions),
            "LEVELONE_FUTURES" => Some(StreamService::LeveloneFutures),
            "LEVELONE_FUTURES_OPTIONS" => Some(StreamService::LeveloneFuturesOptions),
            "LEVELONE_FOREX" => Some(StreamService::LeveloneForex),
            "NYSE_BOOK" => Some(StreamService::NyseBook),
            "NASDAQ_BOOK" => Some(StreamService::NasdaqBook),
            "OPTIONS_BOOK" => Some(StreamService::OptionsBook),
            "CHART_EQUITY" => Some(StreamService::ChartEquity),
            "CHART_FUTURES" => Some(StreamService::ChartFutures),
            "SCREENER_EQUITY" => Some(StreamService::ScreenerEquity),
            "SCREENER_OPTION" => Some(StreamService::ScreenerOption),
            "ACCT_ACTIVITY" => Some(StreamService::AcctActivity),
            _ => None,
        }
    }

    /// Default subscription field IDs for this service.
    ///
    /// Every field of the service's table ([`fields`](Self::fields)) except
    /// for `LEVELONE_EQUITIES`, which starts with the quote-sized subset in
    /// [`LevelOneEquityFields::default_fields`] since watchlists are large.
    /// Requesting a number beyond a table is rejected by the streamer with a
    /// bad-command error.
    pub fn default_fields(self) -> String {
        match self {
            Self::LeveloneEquities => LevelOneEquityFields::default_fields(),
            service => payload::csv(service.fields().iter().map(|field| field.number)),
        }
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

/// Local lifecycle notification, never accepted from the broker wire.
/// `lost_messages` is cumulative; any increase requires REST reconciliation
/// for account activity. A new session does not recover missed executions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct StreamStatus {
    pub ready: bool,
    pub generation: u64,
    pub lost_messages: u64,
}
