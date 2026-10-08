//! Market data types for quotes, price history, options, and instruments.
//!
//! This module contains all types related to market data retrieval from the
//! Schwab API, including:
//!
//! - Real-time and delayed quotes
//! - Historical price data (candles)
//! - Options chains and Greeks
//! - Market movers and screeners
//! - Instrument lookups
//!
//! # Field Naming
//!
//! All fields use `camelCase` serialization to match the Schwab API exactly.
//! Rust fields use `snake_case` as per Rust conventions.

use crate::common::{AssetMainType, AssetSubType};
use crate::validation::{ValidationError, ValidationError as VE, ValidationErrorKind as K};
use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::collections::HashMap;

/// Response from `GET /quotes` and `GET /{symbol}/quotes`.
///
/// Schwab keys the response object **by symbol** — `{"AAPL": {...}}` — rather
/// than wrapping quotes in an array. Invalid identifiers come back under a
/// reserved `errors` key alongside the symbol entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub struct QuotesResponse {
    /// Partial-failure report for invalid symbols/cusips/SSIDs in the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<QuoteError>,
    /// Quotes keyed by the symbol they were requested with.
    #[serde(flatten)]
    pub quotes: HashMap<String, QuoteItem>,
}

/// Per-request error report embedded in a quotes response.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct QuoteError {
    pub invalid_cusips: Option<Vec<String>>,
    #[serde(rename = "invalidSSIDs")]
    pub invalid_ssids: Option<Vec<i64>>,
    pub invalid_symbols: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// A single quote item containing various quote data components.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct QuoteItem {
    pub symbol: Option<String>,
    pub asset_main_type: Option<AssetMainType>,
    pub asset_sub_type: Option<AssetSubType>,
    pub quote_type: Option<String>,
    pub realtime: Option<bool>,
    pub ssid: Option<i64>,
    pub quote: Option<QuoteQuote>,
    pub fundamental: Option<QuoteFundamental>,
    pub extended: Option<QuoteExtended>,
    pub reference: Option<QuoteReference>,
    pub regular: Option<QuoteRegular>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Core quote data including bid/ask, last price, and volume.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct QuoteQuote {
    #[serde(rename = "52WeekHigh")]
    pub fifty_two_week_high: Option<f64>,
    #[serde(rename = "52WeekLow")]
    pub fifty_two_week_low: Option<f64>,
    #[serde(rename = "askMICId")]
    pub ask_mic_id: Option<String>,
    pub ask_price: Option<f64>,
    pub ask_size: Option<i32>,
    pub ask_time: Option<i64>,
    #[serde(rename = "bidMICId")]
    pub bid_mic_id: Option<String>,
    pub bid_price: Option<f64>,
    pub bid_size: Option<i32>,
    pub bid_time: Option<i64>,
    pub close_price: Option<f64>,
    pub exchange: Option<String>,
    pub high_price: Option<f64>,
    #[serde(rename = "lastMICId")]
    pub last_mic_id: Option<String>,
    pub last_price: Option<f64>,
    pub last_size: Option<i32>,
    pub low_price: Option<f64>,
    pub mark: Option<f64>,
    pub net_change: Option<f64>,
    pub total_volume: Option<i64>,
    pub open_price: Option<f64>,
    pub quote_time: Option<i64>,
    pub trade_time: Option<i64>,
    pub volatility: Option<f64>,
    pub short_interest: Option<i32>,
    pub security_status: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Dividend, earnings and volume fundamentals attached to a quote.
///
/// This is the `fundamental` section of `GET /quotes`; the richer ratio set
/// (margins, leverage, 52-week range) lives on
/// [`InstrumentFundamental`] via `GET /instruments?projection=fundamental`.
/// Volumes arrive as JSON floats (`37498265.0`), so they are `f64` even
/// though they count shares. Dates are ISO-8601 strings
/// (`"2024-08-15T04:00:00Z"`).
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct QuoteFundamental {
    pub avg10_days_volume: Option<f64>,
    pub avg1_year_volume: Option<f64>,
    pub declaration_date: Option<String>,
    pub div_amount: Option<f64>,
    pub div_ex_date: Option<String>,
    /// Dividends per year (`4` for quarterly, `0` when none).
    pub div_freq: Option<i64>,
    pub div_pay_amount: Option<f64>,
    pub div_pay_date: Option<String>,
    pub div_yield: Option<f64>,
    pub eps: Option<f64>,
    pub fund_leverage_factor: Option<f64>,
    pub fund_strategy: Option<String>,
    pub last_earnings_date: Option<String>,
    pub next_div_ex_date: Option<String>,
    pub next_div_pay_date: Option<String>,
    pub pe_ratio: Option<f64>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Extended hours trading data.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct QuoteExtended {
    pub extended_price: Option<f64>,
    pub extended_change: Option<f64>,
    pub extended_change_percent: Option<f64>,
    pub extended_price_time: Option<i64>,
    pub quote_time: Option<i64>,
    pub total_volume: Option<i64>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Reference data including symbol description and exchange info.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct QuoteReference {
    pub cusip: Option<String>,
    pub description: Option<String>,
    pub exchange: Option<String>,
    pub exchange_name: Option<String>,
    pub asset_type: Option<String>,
    pub symbol: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Regular market session data.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct QuoteRegular {
    pub regular_market_last_price: Option<f64>,
    pub regular_market_net_change: Option<f64>,
    pub regular_market_trade_time: Option<i64>,
    pub regular_market_percent_change: Option<f64>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Historical price data response with OHLCV candles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PriceHistoryResponse {
    pub symbol: String,
    pub empty: bool,
    pub previous_close: Option<f64>,
    pub previous_close_date: Option<i64>,
    pub candles: Vec<Candle>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// A single OHLCV candlestick data point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Candle {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// Daily volume on large-cap names exceeds `i32::MAX`; keep this wide.
    pub volume: i64,
    pub datetime: i64,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Options chain response with calls and puts organized by expiration and strike.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct OptionChainResponse {
    pub symbol: String,
    pub status: Option<String>,
    pub strategy: Option<String>,
    pub interval: Option<f64>,
    pub is_delayed: Option<bool>,
    pub underlying: Option<UnderlyingQuote>,
    pub number_of_contracts: Option<i32>,
    /// Schwab returned only part of the chain asked for: narrow the
    /// request (strike count, range, dates) to see the rest.
    pub is_chain_truncated: Option<bool>,
    pub underlying_price: Option<f64>,
    pub is_index: Option<bool>,
    pub interest_rate: Option<f64>,
    pub volatility: Option<f64>,
    pub days_to_expiration: Option<f64>,
    pub call_exp_date_map: Option<ExpDateStrikeMap>,
    pub put_exp_date_map: Option<ExpDateStrikeMap>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Map of expiration dates to strike prices to option contracts.
pub type ExpDateStrikeMap = HashMap<String, HashMap<String, Vec<OptionContract>>>;

/// Quote data for the underlying security of an options chain.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct UnderlyingQuote {
    pub ask: Option<f64>,
    pub ask_size: Option<i32>,
    pub bid: Option<f64>,
    pub bid_size: Option<i32>,
    pub change: Option<f64>,
    pub close: Option<f64>,
    pub delayed: Option<bool>,
    pub description: Option<String>,
    pub exchange_name: Option<String>,
    pub fifty_two_week_high: Option<f64>,
    pub fifty_two_week_low: Option<f64>,
    pub high_price: Option<f64>,
    pub last: Option<f64>,
    pub low_price: Option<f64>,
    pub mark: Option<f64>,
    pub mark_change: Option<f64>,
    pub mark_percent_change: Option<f64>,
    pub open_price: Option<f64>,
    pub percent_change: Option<f64>,
    pub quote_time: Option<i64>,
    pub symbol: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// A single option contract with pricing and Greeks.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct OptionContract {
    pub put_call: String,
    pub symbol: String,
    pub description: Option<String>,
    pub exchange_name: Option<String>,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub last: Option<f64>,
    pub mark: Option<f64>,
    pub bid_size: Option<i32>,
    pub ask_size: Option<i32>,
    pub last_size: Option<i32>,
    pub high_price: Option<f64>,
    pub low_price: Option<f64>,
    pub open_price: Option<f64>,
    pub close_price: Option<f64>,
    pub total_volume: Option<i64>,
    pub trade_time_in_long: Option<i64>,
    pub quote_time_in_long: Option<i64>,
    pub net_change: Option<f64>,
    pub volatility: Option<f64>,
    pub theoretical_option_value: Option<f64>,
    pub theoretical_volatility: Option<f64>,
    pub time_value: Option<f64>,
    pub open_interest: Option<i32>,
    #[serde(rename = "inTheMoney")]
    pub is_in_the_money: Option<bool>,
    pub is_divident: Option<bool>,
    pub strike_price: f64,
    pub expiration_date: String,
    pub expiration_type: Option<String>,
    pub days_to_expiration: Option<i32>,
    pub last_trading_day: Option<i64>,
    pub multiplier: Option<f64>,
    pub settlement_type: Option<String>,
    #[serde(rename = "optionDeliverablesList")]
    pub deliverables: Option<Vec<serde_json::Value>>,
    #[serde(flatten)]
    pub greeks: Greeks,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Option Greeks for risk assessment.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Greeks {
    pub delta: Option<f64>,
    pub gamma: Option<f64>,
    pub theta: Option<f64>,
    pub vega: Option<f64>,
    pub rho: Option<f64>,
}

/// Response containing available option expirations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ExpirationChainResponse {
    pub symbol: Option<String>,
    #[serde(rename = "expirationList")]
    pub expirations: Vec<ExpirationItem>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// A single option expiration date entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ExpirationItem {
    pub expiration_date: NaiveDate,
    #[serde(default)]
    pub days_to_expiration: i32,
    #[serde(default)]
    pub expiration_type: String,
    #[serde(default)]
    pub standard: bool,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Response containing market movers/screener results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct MoversResponse {
    pub screeners: Vec<MoverItem>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// A single market mover item.
///
/// The live `/movers` payload carries `lastPrice`, `netChange`,
/// `netPercentChange`, `volume`, `marketShare` and `trades` and no
/// `direction`; the developer portal's example instead shows `last`,
/// `change` and `direction`. Both spellings decode, every field but
/// `symbol` is optional, and anything else lands in `extra`.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct MoverItem {
    pub symbol: String,
    pub description: Option<String>,
    pub last_price: Option<f64>,
    pub net_change: Option<f64>,
    /// Net change as a fraction of the previous close.
    pub net_percent_change: Option<f64>,
    /// Shares traded in the mover's own listing.
    pub volume: Option<i64>,
    /// Volume across the index or universe the movers were drawn from.
    pub total_volume: Option<i64>,
    /// This mover's share of `total_volume`, in percent.
    pub market_share: Option<f64>,
    pub trades: Option<i64>,
    /// Portal-example spelling of `last_price`.
    pub last: Option<f64>,
    /// Portal-example spelling of `net_change`.
    pub change: Option<f64>,
    /// Portal-example only; `None` otherwise or for an unknown value.
    #[serde(default, deserialize_with = "crate::common::lenient")]
    pub direction: Option<Direction>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Direction of price movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Direction {
    /// Price moving up
    Up,
    /// Price moving down
    Down,
}

/// Response containing instrument search results.
///
/// Schwab wraps the result list in an `instruments` key:
/// `{"instruments": [...]}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub struct InstrumentsResponse {
    pub instruments: Vec<Instrument>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// An instrument (security) with basic info and optional fundamentals.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Instrument {
    /// Market Data's classification, the same value set as
    /// `assetMainType` on quotes.
    pub asset_type: Option<AssetMainType>,
    pub cusip: Option<String>,
    pub symbol: String,
    pub description: Option<String>,
    pub exchange: Option<String>,
    pub exchange_name: Option<String>,
    pub fundamental: Option<InstrumentFundamental>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Fundamental data returned by `GET /instruments?projection=fundamental`.
///
/// Dates are Schwab's `"2024-08-12 00:00:00.0"` strings, not epoch millis.
/// Volumes are `f64` because Schwab is inconsistent about sending them as
/// integers or floats between endpoints; `f64` decodes both.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct InstrumentFundamental {
    pub symbol: String,
    pub high52: Option<f64>,
    pub low52: Option<f64>,
    pub dividend_amount: Option<f64>,
    pub dividend_yield: Option<f64>,
    pub dividend_date: Option<String>,
    pub pe_ratio: Option<f64>,
    pub peg_ratio: Option<f64>,
    pub pb_ratio: Option<f64>,
    pub pr_ratio: Option<f64>,
    pub pcf_ratio: Option<f64>,
    #[serde(rename = "grossMarginTTM")]
    pub gross_margin_ttm: Option<f64>,
    #[serde(rename = "grossMarginMRQ")]
    pub gross_margin_mrq: Option<f64>,
    #[serde(rename = "netProfitMarginTTM")]
    pub net_profit_margin_ttm: Option<f64>,
    #[serde(rename = "netProfitMarginMRQ")]
    pub net_profit_margin_mrq: Option<f64>,
    #[serde(rename = "operatingMarginTTM")]
    pub operating_margin_ttm: Option<f64>,
    #[serde(rename = "operatingMarginMRQ")]
    pub operating_margin_mrq: Option<f64>,
    pub return_on_equity: Option<f64>,
    pub return_on_assets: Option<f64>,
    pub return_on_investment: Option<f64>,
    pub quick_ratio: Option<f64>,
    pub current_ratio: Option<f64>,
    pub interest_coverage: Option<f64>,
    pub total_debt_to_capital: Option<f64>,
    pub lt_debt_to_equity: Option<f64>,
    pub total_debt_to_equity: Option<f64>,
    #[serde(rename = "epsTTM")]
    pub eps_ttm: Option<f64>,
    #[serde(rename = "epsChangePercentTTM")]
    pub eps_change_percent_ttm: Option<f64>,
    pub eps_change_year: Option<f64>,
    pub eps_change: Option<f64>,
    pub rev_change_year: Option<f64>,
    #[serde(rename = "revChangeTTM")]
    pub rev_change_ttm: Option<f64>,
    pub rev_change_in: Option<f64>,
    pub shares_outstanding: Option<f64>,
    pub market_cap_float: Option<f64>,
    pub market_cap: Option<f64>,
    pub book_value_per_share: Option<f64>,
    pub short_int_to_float: Option<f64>,
    pub short_int_day_to_cover: Option<f64>,
    pub div_growth_rate3_year: Option<f64>,
    pub dividend_pay_amount: Option<f64>,
    pub dividend_pay_date: Option<String>,
    pub beta: Option<f64>,
    pub vol1_day_avg: Option<f64>,
    pub vol10_day_avg: Option<f64>,
    pub vol3_month_avg: Option<f64>,
    pub avg10_days_volume: Option<f64>,
    pub avg1_day_volume: Option<f64>,
    pub avg3_month_volume: Option<f64>,
    pub declaration_date: Option<String>,
    /// Dividends per year (`4` for quarterly, `0` when none).
    pub dividend_freq: Option<i64>,
    pub eps: Option<f64>,
    pub dtn_volume: Option<f64>,
    pub next_dividend_pay_date: Option<String>,
    pub next_dividend_date: Option<String>,
    pub fund_leverage_factor: Option<f64>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Quote sections selectable via the `fields` query parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum QuoteField {
    /// Every section (Schwab's default when `fields` is omitted)
    All,
    /// Bid/ask, last, volume — [`QuoteItem::quote`]
    Quote,
    /// Ratios, dividends, margins — [`QuoteItem::fundamental`]
    Fundamental,
    /// Pre/post-market prints — [`QuoteItem::extended`]
    Extended,
    /// Symbol description and exchange — [`QuoteItem::reference`]
    Reference,
    /// Regular-session last/net change — [`QuoteItem::regular`]
    Regular,
}

impl QuoteField {
    /// Wire name used in the `fields` query parameter.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Quote => "quote",
            Self::Fundamental => "fundamental",
            Self::Extended => "extended",
            Self::Reference => "reference",
            Self::Regular => "regular",
        }
    }
}

impl std::fmt::Display for QuoteField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Period type for price history requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum PeriodType {
    /// Daily period
    Day,
    /// Monthly period
    Month,
    /// Yearly period
    Year,
    /// Year to date
    Ytd,
}

impl PeriodType {
    /// Wire name used in the `periodType` query parameter.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Month => "month",
            Self::Year => "year",
            Self::Ytd => "ytd",
        }
    }

    /// `period` values Schwab accepts for this period type.
    pub const fn valid_periods(self) -> &'static [i32] {
        match self {
            Self::Day => &[1, 2, 3, 4, 5, 10],
            Self::Month => &[1, 2, 3, 6],
            Self::Year => &[1, 2, 3, 5, 10, 15, 20],
            Self::Ytd => &[1],
        }
    }

    /// `frequencyType` values Schwab accepts for this period type.
    pub const fn valid_frequency_types(self) -> &'static [FrequencyType] {
        match self {
            Self::Day => &[FrequencyType::Minute],
            Self::Month => &[FrequencyType::Daily, FrequencyType::Weekly],
            Self::Year => &[
                FrequencyType::Daily,
                FrequencyType::Weekly,
                FrequencyType::Monthly,
            ],
            Self::Ytd => &[FrequencyType::Daily, FrequencyType::Weekly],
        }
    }

    /// The `frequencyType` Schwab assumes when the request omits it.
    pub const fn default_frequency_type(self) -> FrequencyType {
        match self {
            Self::Day => FrequencyType::Minute,
            Self::Month => FrequencyType::Weekly,
            Self::Year => FrequencyType::Monthly,
            Self::Ytd => FrequencyType::Weekly,
        }
    }
}

impl std::fmt::Display for PeriodType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Frequency type for price history candles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum FrequencyType {
    /// Minute bars
    Minute,
    /// Daily bars
    Daily,
    /// Weekly bars
    Weekly,
    /// Monthly bars
    Monthly,
}

impl FrequencyType {
    /// Wire name used in the `frequencyType` query parameter.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Minute => "minute",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
        }
    }

    /// `frequency` values Schwab accepts for this frequency type.
    pub const fn valid_frequencies(self) -> &'static [i32] {
        match self {
            Self::Minute => &[1, 5, 10, 15, 30],
            Self::Daily | Self::Weekly | Self::Monthly => &[1],
        }
    }
}

impl std::fmt::Display for FrequencyType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Option contract type filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum ContractType {
    /// Call options only
    Call,
    /// Put options only
    Put,
    /// Both calls and puts
    All,
}

/// Option strategy type for chain requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum OptionStrategy {
    /// Single leg options
    Single,
    /// Analytical view
    Analytical,
    /// Covered calls/puts
    Covered,
    /// Vertical spreads
    Vertical,
    /// Calendar spreads
    Calendar,
    /// Strangles
    Strangle,
    /// Straddles
    Straddle,
    /// Butterfly spreads
    Butterfly,
    /// Condor spreads
    Condor,
    /// Diagonal spreads
    Diagonal,
    /// Collar strategy
    Collar,
    /// Roll strategy
    Roll,
}

/// Option moneyness range filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum Range {
    /// In the money
    Itm,
    /// Near the money
    Ntm,
    /// Out of the money
    Otm,
    /// Strikes above market
    Sak,
    /// Strikes below market
    Sbk,
    /// Strikes near market
    Snk,
    /// All strikes
    All,
}

/// Expiration month filter for options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum ExpirationMonth {
    /// January
    Jan,
    /// February
    Feb,
    /// March
    Mar,
    /// April
    Apr,
    /// May
    May,
    /// June
    Jun,
    /// July
    Jul,
    /// August
    Aug,
    /// September
    Sep,
    /// October
    Oct,
    /// November
    Nov,
    /// December
    Dec,
    /// All months
    All,
}

/// Sort criteria for market movers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum MoverSort {
    /// Sort by volume
    Volume,
    /// Sort by number of trades
    Trades,
    /// Sort by percent change (gainers)
    PercentChangeUp,
    /// Sort by percent change (losers)
    PercentChangeDown,
}

impl MoverSort {
    /// Wire name used in the `sort` query parameter.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Volume => "VOLUME",
            Self::Trades => "TRADES",
            Self::PercentChangeUp => "PERCENT_CHANGE_UP",
            Self::PercentChangeDown => "PERCENT_CHANGE_DOWN",
        }
    }
}

impl std::fmt::Display for MoverSort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Index or screener universe for `GET /movers/{index}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum MoverIndex {
    /// Dow Jones Industrial Average
    #[serde(rename = "$DJI")]
    Dji,
    /// NASDAQ Composite
    #[serde(rename = "$COMPX")]
    Compx,
    /// S&P 500
    #[serde(rename = "$SPX")]
    Spx,
    /// NYSE-listed equities
    Nyse,
    /// NASDAQ-listed equities
    Nasdaq,
    /// OTC bulletin board
    Otcbb,
    /// Every index
    IndexAll,
    /// Every equity
    EquityAll,
    /// Every option
    OptionAll,
    /// Put options
    OptionPut,
    /// Call options
    OptionCall,
}

impl MoverIndex {
    /// Wire name used as the `{index}` path segment.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dji => "$DJI",
            Self::Compx => "$COMPX",
            Self::Spx => "$SPX",
            Self::Nyse => "NYSE",
            Self::Nasdaq => "NASDAQ",
            Self::Otcbb => "OTCBB",
            Self::IndexAll => "INDEX_ALL",
            Self::EquityAll => "EQUITY_ALL",
            Self::OptionAll => "OPTION_ALL",
            Self::OptionPut => "OPTION_PUT",
            Self::OptionCall => "OPTION_CALL",
        }
    }
}

impl std::fmt::Display for MoverIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Look-back window, in minutes, for `GET /movers/{index}`.
///
/// Serializes as the bare integer Schwab expects (`frequency=5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema_repr))]
#[serde(into = "u8", try_from = "u8")]
#[repr(u8)]
#[non_exhaustive]
pub enum MoverFrequency {
    /// Schwab's default window
    All = 0,
    OneMinute = 1,
    FiveMinutes = 5,
    TenMinutes = 10,
    ThirtyMinutes = 30,
    SixtyMinutes = 60,
}

impl MoverFrequency {
    /// The window length in minutes as sent on the wire.
    pub const fn as_minutes(self) -> u8 {
        self as u8
    }
}

impl From<MoverFrequency> for u8 {
    fn from(frequency: MoverFrequency) -> Self {
        frequency.as_minutes()
    }
}

impl TryFrom<u8> for MoverFrequency {
    type Error = String;

    fn try_from(minutes: u8) -> Result<Self, Self::Error> {
        Ok(match minutes {
            0 => Self::All,
            1 => Self::OneMinute,
            5 => Self::FiveMinutes,
            10 => Self::TenMinutes,
            30 => Self::ThirtyMinutes,
            60 => Self::SixtyMinutes,
            other => {
                return Err(format!(
                    "frequency {other} is not valid; expected one of 0, 1, 5, 10, 30, 60"
                ));
            }
        })
    }
}

/// Projection type for instrument searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Projection {
    /// Symbol search
    SymbolSearch,
    /// Symbol regex search
    SymbolRegex,
    /// Description search
    DescSearch,
    /// Description regex search
    DescRegex,
    /// General search
    Search,
    /// Include fundamental data
    Fundamental,
}

impl Projection {
    /// Wire name used in the `projection` query parameter.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SymbolSearch => "symbol-search",
            Self::SymbolRegex => "symbol-regex",
            Self::DescSearch => "desc-search",
            Self::DescRegex => "desc-regex",
            Self::Search => "search",
            Self::Fundamental => "fundamental",
        }
    }
}

impl std::fmt::Display for Projection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Market whose trading hours are requested via `GET /markets`.
///
/// Also the top-level key of [`MarketsHoursResponse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Market {
    Equity,
    Option,
    Bond,
    Future,
    Forex,
}

impl Market {
    /// Wire name used in the `markets` query parameter and `{marketId}` path.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Equity => "equity",
            Self::Option => "option",
            Self::Bond => "bond",
            Self::Future => "future",
            Self::Forex => "forex",
        }
    }
}

impl std::fmt::Display for Market {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Market-data entitlement of the requesting user, for option chains.
///
/// Schwab applies it only to retail tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub enum Entitlement {
    /// Non-paying professional
    #[serde(rename = "PN")]
    NonPayingPro,
    /// Non-professional
    #[serde(rename = "NP")]
    NonPro,
    /// Paying professional
    #[serde(rename = "PP")]
    PayingPro,
}

/// Contract standardization filter for option chains: standard contracts,
/// non-standard (adjusted after a corporate action) ones, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub enum OptionType {
    /// Standard contracts only
    #[serde(rename = "S")]
    Standard,
    /// Non-standard (adjusted) contracts only
    #[serde(rename = "NS")]
    NonStandard,
    /// Both
    #[serde(rename = "ALL")]
    All,
}

/// Query parameters for `GET /pricehistory`.
///
/// `period`, `frequencyType`, and `frequency` are only valid in specific
/// combinations; [`validate`](Self::validate) checks them against Schwab's
/// documented tables before the request leaves the process.
#[skip_serializing_none]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct PriceHistoryParams {
    pub symbol: String,
    pub period_type: Option<PeriodType>,
    /// Number of `period_type` units; see [`PeriodType::valid_periods`].
    pub period: Option<i32>,
    /// Candle granularity; see [`PeriodType::valid_frequency_types`].
    pub frequency_type: Option<FrequencyType>,
    /// Candles per `frequency_type` unit; see [`FrequencyType::valid_frequencies`].
    pub frequency: Option<i32>,
    /// Inclusive start, milliseconds since the Unix epoch.
    pub start_date: Option<i64>,
    /// Inclusive end, milliseconds since the Unix epoch.
    pub end_date: Option<i64>,
    pub need_extended_hours_data: Option<bool>,
    pub need_previous_close: Option<bool>,
}

impl PriceHistoryParams {
    /// Request with Schwab's defaults for everything but the symbol.
    pub fn new(symbol: impl Into<String>) -> Self {
        Self {
            symbol: symbol.into(),
            ..Self::default()
        }
    }

    /// Check the request against Schwab's period/frequency tables.
    ///
    /// Only combinations Schwab documents are cross-checked: `period`
    /// against `period_type`, `frequency_type` against `period_type`, and
    /// `frequency` against the explicit or defaulted `frequency_type`.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.symbol.trim().is_empty() {
            return Err(VE::new(K::Missing, Some("symbol"), "symbol is required"));
        }
        if let (Some(period_type), Some(period)) = (self.period_type, self.period)
            && !period_type.valid_periods().contains(&period)
        {
            return Err(VE::new(
                K::OutOfRange,
                Some("period"),
                format!(
                    "period {period} is not valid for periodType {period_type}; expected one of {:?}",
                    period_type.valid_periods()
                ),
            ));
        }
        if let (Some(period_type), Some(frequency_type)) = (self.period_type, self.frequency_type)
            && !period_type
                .valid_frequency_types()
                .contains(&frequency_type)
        {
            return Err(VE::new(
                K::OutOfRange,
                Some("frequencyType"),
                format!(
                    "frequencyType {frequency_type} is not valid for periodType {period_type}; \
                     expected one of {}",
                    join_names(period_type.valid_frequency_types())
                ),
            ));
        }
        let frequency_type = self
            .frequency_type
            .or_else(|| self.period_type.map(PeriodType::default_frequency_type));
        if let (Some(frequency_type), Some(frequency)) = (frequency_type, self.frequency)
            && !frequency_type.valid_frequencies().contains(&frequency)
        {
            return Err(VE::new(
                K::OutOfRange,
                Some("frequency"),
                format!(
                    "frequency {frequency} is not valid for frequencyType {frequency_type}; \
                     expected one of {:?}",
                    frequency_type.valid_frequencies()
                ),
            ));
        }
        if let (Some(start), Some(end)) = (self.start_date, self.end_date)
            && start > end
        {
            return Err(VE::new(
                K::OutOfRange,
                Some("startDate"),
                "startDate must not be after endDate",
            ));
        }
        Ok(())
    }
}

fn join_names(types: &[FrequencyType]) -> String {
    types
        .iter()
        .map(|t| t.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Query parameters for `GET /chains`.
#[skip_serializing_none]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct OptionChainParams {
    pub symbol: String,
    pub contract_type: Option<ContractType>,
    /// Number of strikes above and below the at-the-money price (≥ 1).
    pub strike_count: Option<i32>,
    pub include_underlying_quote: Option<bool>,
    pub strategy: Option<OptionStrategy>,
    /// Strike interval for spread strategy chains.
    pub interval: Option<f64>,
    pub strike: Option<f64>,
    pub range: Option<Range>,
    pub from_date: Option<NaiveDate>,
    pub to_date: Option<NaiveDate>,
    /// Volatility used in `ANALYTICAL` calculations.
    pub volatility: Option<f64>,
    /// Underlying price used in `ANALYTICAL` calculations.
    pub underlying_price: Option<f64>,
    /// Interest rate used in `ANALYTICAL` calculations.
    pub interest_rate: Option<f64>,
    /// Days to expiration used in `ANALYTICAL` calculations (≥ 0).
    pub days_to_expiration: Option<i32>,
    pub exp_month: Option<ExpirationMonth>,
    pub option_type: Option<OptionType>,
    pub entitlement: Option<Entitlement>,
}

impl OptionChainParams {
    /// Request with Schwab's defaults for everything but the symbol.
    pub fn new(symbol: impl Into<String>) -> Self {
        Self {
            symbol: symbol.into(),
            ..Self::default()
        }
    }

    /// Check the request against Schwab's documented bounds.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.symbol.trim().is_empty() {
            return Err(VE::new(K::Missing, Some("symbol"), "symbol is required"));
        }
        if self.strike_count.is_some_and(|count| count < 1) {
            return Err(VE::new(
                K::OutOfRange,
                Some("strikeCount"),
                "strikeCount must be at least 1",
            ));
        }
        if self.days_to_expiration.is_some_and(|days| days < 0) {
            return Err(VE::new(
                K::OutOfRange,
                Some("daysToExpiration"),
                "daysToExpiration must not be negative",
            ));
        }
        if let (Some(from), Some(to)) = (self.from_date, self.to_date)
            && from > to
        {
            return Err(VE::new(
                K::OutOfRange,
                Some("fromDate"),
                "fromDate must not be after toDate",
            ));
        }
        Ok(())
    }
}

/// One error from a Schwab error body (`{"errors":[...]}`).
///
/// Schwab's examples spell `status` as a string for some codes (`"400"`)
/// and a number for others (`401`); both decode.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub struct ApiError {
    #[serde(default, deserialize_with = "crate::common::optional_id")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "crate::common::optional_id")]
    pub status: Option<String>,
    pub title: Option<String>,
    pub detail: Option<String>,
    #[serde(default, deserialize_with = "crate::common::lenient")]
    pub source: Option<ErrorSource>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl ApiError {
    /// An error with a status, title and detail.
    pub fn new(
        status: Option<String>,
        title: impl Into<String>,
        detail: impl Into<Option<String>>,
    ) -> Self {
        Self {
            status,
            title: Some(title.into()),
            detail: detail.into(),
            ..Self::default()
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(title) = &self.title {
            write!(f, "{title}")?;
        }
        if let Some(detail) = &self.detail {
            write!(f, ": {detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

/// Source location of an API error.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub struct ErrorSource {
    pub pointer: Option<Vec<String>>,
    pub parameter: Option<String>,
    pub header: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Collection of API errors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub struct ApiErrors {
    pub errors: Vec<ApiError>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Trading session time window.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Session {
    pub start: Option<String>,
    pub end: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Session hours for pre-market, regular, and post-market.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SessionHours {
    pub pre_market: Option<Vec<Session>>,
    pub regular_market: Option<Vec<Session>>,
    pub post_market: Option<Vec<Session>>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Market hours for a specific date and product.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct MarketHours {
    pub date: Option<String>,
    pub category: Option<String>,
    pub product: Option<String>,
    pub is_open: Option<bool>,
    pub session_hours: Option<SessionHours>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// A part of the trading day, as the market hours calendar divides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionPhase {
    PreMarket,
    Regular,
    PostMarket,
    Closed,
}

/// A session boundary: `phase` begins at `at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTransition {
    pub phase: SessionPhase,
    pub at: DateTime<FixedOffset>,
}

impl MarketHours {
    /// The phase in effect at `at`, and this calendar day's next boundary.
    ///
    /// Windows are compared as the instants their RFC 3339 bounds denote, so
    /// the caller's clock and the calendar's offsets need not agree, and a
    /// window that fails to parse is ignored. A day with `isOpen: false` or
    /// no session windows is `Closed` with no transition; so is the time
    /// after the last window, since the next day's calendar is a separate
    /// request.
    pub fn phase_at(&self, at: DateTime<Utc>) -> (SessionPhase, Option<SessionTransition>) {
        // Each window opens its phase at `start` and closes the market at
        // `end`; where two windows meet, the opening wins.
        let mut events: Vec<(DateTime<FixedOffset>, SessionPhase)> = Vec::new();
        if self.is_open != Some(false)
            && let Some(hours) = &self.session_hours
        {
            for (phase, windows) in [
                (SessionPhase::PreMarket, &hours.pre_market),
                (SessionPhase::Regular, &hours.regular_market),
                (SessionPhase::PostMarket, &hours.post_market),
            ] {
                for window in windows.iter().flatten() {
                    let parse = |bound: &Option<String>| {
                        bound
                            .as_deref()
                            .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
                    };
                    if let (Some(start), Some(end)) = (parse(&window.start), parse(&window.end))
                        && start < end
                    {
                        events.push((end, SessionPhase::Closed));
                        events.push((start, phase));
                    }
                }
            }
        }
        events.sort_by_key(|(at, phase)| (*at, *phase != SessionPhase::Closed));
        events.dedup_by(|later, earlier| {
            later.0 == earlier.0 && {
                earlier.1 = later.1;
                true
            }
        });
        let phase = events
            .iter()
            .rev()
            .find(|(start, _)| *start <= at)
            .map_or(SessionPhase::Closed, |(_, phase)| *phase);
        let next = events
            .iter()
            .find(|(start, _)| *start > at)
            .map(|(at, phase)| SessionTransition {
                phase: *phase,
                at: *at,
            });
        (phase, next)
    }
}

/// Map of market type (e.g. `equity`) to product code (e.g. `EQ`) to hours.
///
/// Schwab nests market hours two levels deep:
/// `{"equity": {"EQ": {"date": ..., "sessionHours": ...}}}`.
pub type MarketsHoursResponse = HashMap<String, HashMap<String, MarketHours>>;

/// The equity product's hours in a market hours response.
///
/// Schwab keys a trading day by product code (`EQ`) and a closed day by the
/// market name (`equity`), so either spelling is accepted.
pub fn equity_hours(response: &MarketsHoursResponse) -> Option<&MarketHours> {
    let market = response.get(Market::Equity.as_str())?;
    market
        .get("EQ")
        .or_else(|| market.get(Market::Equity.as_str()))
}
