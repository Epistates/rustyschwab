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

#![allow(missing_docs)] // DTO fields are self-documenting via Schwab API docs

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::collections::HashMap;

/// Response containing multiple quote items.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotesResponse {
    pub quotes: Vec<QuoteItem>,
}

/// A single quote item containing various quote data components.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteItem {
    pub symbol: String,
    pub quote: Option<QuoteQuote>,
    pub fundamental: Option<QuoteFundamental>,
    pub extended: Option<QuoteExtended>,
    pub reference: Option<QuoteReference>,
    pub regular: Option<QuoteRegular>,
}

/// Core quote data including bid/ask, last price, and volume.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
    pub total_volume: Option<i32>,
    pub open_price: Option<f64>,
    pub quote_time: Option<i64>,
    pub trade_time: Option<i64>,
    pub volatility: Option<f64>,
    pub short_interest: Option<i32>,
    pub security_status: Option<String>,
}

/// Fundamental data including ratios, dividends, and financial metrics.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteFundamental {
    pub avg10_days_volume: Option<i32>,
    pub avg30_days_volume: Option<i32>,
    pub avg90_days_volume: Option<i32>,
    pub div_amount: Option<f64>,
    pub div_yield: Option<f64>,
    pub div_pay_amount: Option<f64>,
    pub div_pay_date: Option<i64>,
    pub pe_ratio: Option<f64>,
    pub peg_ratio: Option<f64>,
    pub pb_ratio: Option<f64>,
    pub pr_ratio: Option<f64>,
    pub pcf_ratio: Option<f64>,
    pub gross_margin_ttm: Option<f64>,
    pub gross_margin_mrq: Option<f64>,
    pub net_profit_margin_ttm: Option<f64>,
    pub net_profit_margin_mrq: Option<f64>,
    pub operating_margin_ttm: Option<f64>,
    pub operating_margin_mrq: Option<f64>,
    pub return_on_equity: Option<f64>,
    pub return_on_assets: Option<f64>,
    pub return_on_investment: Option<f64>,
    pub beta: Option<f64>,
    pub market_cap: Option<f64>,
    pub shares_outstanding: Option<f64>,
    pub eps_ttm: Option<f64>,
    pub eps_change_percent_ttm: Option<f64>,
    pub eps_change_year: Option<f64>,
    pub eps_change: Option<f64>,
    pub book_value_per_share: Option<f64>,
    pub high52: Option<f64>,
    pub low52: Option<f64>,
    pub fundamental_currency_code: Option<String>,
}

/// Extended hours trading data.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteExtended {
    pub extended_price: Option<f64>,
    pub extended_change: Option<f64>,
    pub extended_change_percent: Option<f64>,
    pub extended_price_time: Option<i64>,
    pub quote_time: Option<i64>,
    pub total_volume: Option<i32>,
}

/// Reference data including symbol description and exchange info.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteReference {
    pub cusip: Option<String>,
    pub description: Option<String>,
    pub exchange: Option<String>,
    pub exchange_name: Option<String>,
    pub asset_type: Option<String>,
    pub symbol: Option<String>,
}

/// Regular market session data.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteRegular {
    pub regular_market_last_price: Option<f64>,
    pub regular_market_net_change: Option<f64>,
    pub regular_market_trade_time: Option<i64>,
    pub regular_market_percent_change: Option<f64>,
}

/// Historical price data response with OHLCV candles.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceHistoryResponse {
    pub symbol: String,
    pub empty: bool,
    pub previous_close: Option<f64>,
    pub previous_close_date: Option<i64>,
    pub candles: Vec<Candle>,
}

/// A single OHLCV candlestick data point.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candle {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i32,
    pub datetime: i64,
}

/// Options chain response with calls and puts organized by expiration and strike.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionChainResponse {
    pub symbol: String,
    pub status: Option<String>,
    pub strategy: Option<String>,
    pub interval: Option<f64>,
    pub is_delayed: Option<bool>,
    pub underlying: Option<UnderlyingQuote>,
    pub number_of_contracts: Option<i32>,
    pub call_exp_date_map: Option<ExpDateStrikeMap>,
    pub put_exp_date_map: Option<ExpDateStrikeMap>,
}

/// Map of expiration dates to strike prices to option contracts.
pub type ExpDateStrikeMap = HashMap<String, HashMap<String, Vec<OptionContract>>>;

/// Quote data for the underlying security of an options chain.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
}

/// A single option contract with pricing and Greeks.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
    pub total_volume: Option<i32>,
    pub trade_time_in_long: Option<i64>,
    pub quote_time_in_long: Option<i64>,
    pub net_change: Option<f64>,
    pub volatility: Option<f64>,
    pub theoretical_option_value: Option<f64>,
    pub theoretical_volatility: Option<f64>,
    pub time_value: Option<f64>,
    pub open_interest: Option<i32>,
    pub is_in_the_money: Option<bool>,
    pub is_divident: Option<bool>,
    pub strike_price: f64,
    pub expiration_date: i64,
    pub expiration_type: Option<String>,
    pub days_to_expiration: Option<i32>,
    pub last_trading_day: Option<i64>,
    pub multiplier: Option<String>,
    pub settlement_type: Option<String>,
    pub deliverables: Option<Vec<String>>,
    pub greeks: Option<Greeks>,
}

/// Option Greeks for risk assessment.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Greeks {
    pub delta: Option<f64>,
    pub gamma: Option<f64>,
    pub theta: Option<f64>,
    pub vega: Option<f64>,
    pub rho: Option<f64>,
}

/// Response containing available option expirations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpirationChainResponse {
    pub symbol: String,
    pub expirations: Vec<ExpirationItem>,
}

/// A single option expiration date entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpirationItem {
    pub expiration_date: NaiveDate,
    pub days_to_expiration: i32,
    pub expiration_type: String,
    pub standard: bool,
}

/// Response containing market movers/screener results.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoversResponse {
    pub screeners: Vec<MoverItem>,
}

/// A single market mover item.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoverItem {
    pub symbol: String,
    pub description: Option<String>,
    pub direction: Direction,
    pub change: Option<f64>,
    pub last: Option<f64>,
    pub total_volume: Option<i32>,
}

/// Direction of price movement.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Price moving up
    Up,
    /// Price moving down
    Down,
}

/// Response containing instrument search results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrumentsResponse(pub Vec<Instrument>);

/// An instrument (security) with basic info and optional fundamentals.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Instrument {
    pub asset_type: Option<String>,
    pub cusip: Option<String>,
    pub symbol: String,
    pub description: Option<String>,
    pub exchange: Option<String>,
    pub exchange_name: Option<String>,
    pub fundamental: Option<InstrumentFundamental>,
}

/// Fundamental data for an instrument.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstrumentFundamental {
    pub symbol: String,
    pub high52: Option<f64>,
    pub low52: Option<f64>,
    pub dividend_amount: Option<f64>,
    pub dividend_yield: Option<f64>,
    pub dividend_date: Option<i64>,
    pub pe_ratio: Option<f64>,
    pub peg_ratio: Option<f64>,
    pub pb_ratio: Option<f64>,
    pub pr_ratio: Option<f64>,
    pub pct_change: Option<f64>,
    pub market_cap: Option<f64>,
    pub trade_time_in_long: Option<i64>,
    pub exchange: Option<String>,
}

/// Period type for price history requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
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

/// Frequency type for price history candles.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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

/// Option contract type filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ContractType {
    /// Call options only
    Call,
    /// Put options only
    Put,
    /// Both calls and puts
    All,
}

/// Option strategy type for chain requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
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

/// Projection type for instrument searches.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
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

/// API error response item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub id: Option<String>,
    pub status: Option<String>,
    pub title: Option<String>,
    pub detail: Option<String>,
    pub source: Option<ErrorSource>,
}

/// Source location of an API error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorSource {
    pub pointer: Option<Vec<String>>,
    pub parameter: Option<String>,
    pub header: Option<String>,
}

/// Collection of API errors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiErrors {
    pub errors: Vec<ApiError>,
}

/// Trading session time window.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub start: Option<String>,
    pub end: Option<String>,
}

/// Session hours for pre-market, regular, and post-market.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHours {
    pub pre_market: Option<Vec<Session>>,
    pub regular_market: Option<Vec<Session>>,
    pub post_market: Option<Vec<Session>>,
}

/// Market hours for a specific date and product.
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketHours {
    pub date: Option<String>,
    pub category: Option<String>,
    pub product: Option<String>,
    pub is_open: Option<bool>,
    pub session_hours: Option<SessionHours>,
}

/// Map of market names to their hours entries.
pub type MarketsHoursResponse = HashMap<String, Vec<MarketHours>>;
