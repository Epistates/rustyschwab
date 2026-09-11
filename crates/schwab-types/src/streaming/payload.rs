//! Field tables and typed payloads for the streamer data services.
//!
//! The streamer sends every service's fields under numeric keys whose
//! meaning depends on the service: `"1"` is the bid price of an equity but
//! the description of an option. Each data service has a `*Fields` table
//! here — the constants used to build subscription field lists and the
//! number → name map — plus a payload struct the named fields deserialize
//! into. Field 0 is always the subscription key and arrives as `key`.
//!
//! Names follow the REST types for the same concept
//! ([`QuoteQuote`](crate::market_data::QuoteQuote),
//! [`Candle`](crate::market_data::Candle)) so a stream delta and a REST
//! quote merge by name; prices are `f64` for the same reason.
//!
//! ```
//! use schwab_types::streaming::{DataContent, LevelOneEquity, LevelOneEquityFields};
//!
//! let content: DataContent = serde_json::from_value(serde_json::json!({
//!     "key": "AMD", "delayed": false, "assetMainType": "EQUITY",
//!     "1": 217.86, "2": 217.95, "8": 33292396
//! }))
//! .unwrap();
//!
//! let quote = LevelOneEquity::from_content(&content).unwrap();
//! assert_eq!(quote.bid_price, Some(217.86));
//! assert_eq!(quote.total_volume, Some(33292396));
//! assert_eq!(LevelOneEquityFields::name(LevelOneEquityFields::BID_PRICE), Some("bidPrice"));
//! ```

use super::{DataContent, DataItem, StreamService};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;

/// One row of a service's field table.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Field {
    /// The wire field number.
    pub number: u32,
    /// The name the field decodes to.
    pub name: &'static str,
    /// The table for the elements when the value is an array of numbered
    /// objects (book price levels).
    pub nested: Option<&'static [Field]>,
}

impl Field {
    const fn new(number: u32, name: &'static str) -> Self {
        Self {
            number,
            name,
            nested: None,
        }
    }

    const fn nested(number: u32, name: &'static str, table: &'static [Field]) -> Self {
        Self {
            number,
            name,
            nested: Some(table),
        }
    }
}

/// Field numbers as the comma-separated list a subscription request takes.
pub(super) fn csv(numbers: impl Iterator<Item = u32>) -> String {
    numbers
        .map(|number| number.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Renames the numeric keys of `entries` per `table`; keys the table does
/// not list, and non-numeric keys, pass through unchanged.
fn rename<'a>(
    entries: impl IntoIterator<Item = (&'a String, &'a Value)>,
    table: &[Field],
) -> Map<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| {
            let field = key
                .parse::<u32>()
                .ok()
                .and_then(|number| table.iter().find(|field| field.number == number));
            match field {
                Some(field) => {
                    let value = match field.nested {
                        Some(nested) => rename_nested(value, nested),
                        None => value.clone(),
                    };
                    (field.name.to_owned(), value)
                }
                None => (key.clone(), value.clone()),
            }
        })
        .collect()
}

fn rename_nested(value: &Value, table: &[Field]) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| rename_nested(item, table))
                .collect(),
        ),
        Value::Object(map) => Value::Object(rename(map, table)),
        other => other.clone(),
    }
}

impl DataContent {
    /// The numbered fields renamed per `table` (see [`StreamService::fields`]).
    /// Numbers the table does not list, and non-numeric keys such as `seq`,
    /// pass through unchanged.
    pub fn named_fields(&self, table: &[Field]) -> Map<String, Value> {
        rename(&self.fields, table)
    }

    /// The whole item as one JSON object: the envelope (`key`, `delayed`,
    /// `assetMainType`, …) plus [`named_fields`](Self::named_fields). This is
    /// the shape the typed payloads deserialize from.
    pub fn to_named_value(&self, table: &[Field]) -> Value {
        match serde_json::to_value(self) {
            Ok(Value::Object(map)) => Value::Object(rename(&map, table)),
            _ => Value::Null,
        }
    }
}

/// Declares one service's field table and typed payload from a single row
/// list: `number CONSTANT field_name "wireName": Type`.
///
/// `key` names the field-0 constant. `envelope` lists the non-numbered
/// keys the service sends beside `key`. `default` (optional) narrows
/// `default_fields()` from every field to the listed constants. A row
/// suffixed `[Record]` holds an array of numbered objects described by
/// `Record::FIELDS`.
macro_rules! field_table {
    (
        $(#[$doc:meta])*
        $table:ident => $payload:ident {
            key = $key:ident;
            envelope = { $( $env:ident : $env_ty:ty ),* $(,)? };
            $( default = [ $( $default:ident ),* $(,)? ]; )?
            $(
                $(#[$field_doc:meta])*
                $number:literal $konst:ident $field:ident $name:literal : $ty:ty
                $([ $nested:ident ])? ,
            )*
        }
    ) => {
        #[doc = concat!("Field numbers and names for [`", stringify!($payload), "`].")]
        #[derive(Debug, Clone, Copy)]
        pub struct $table;

        impl $table {
            /// Field 0: the subscription key, delivered as `key`.
            pub const $key: u32 = 0;
            $( pub const $konst: u32 = $number; )*

            /// The table, in field order.
            pub const FIELDS: &'static [Field] = &[
                Field::new(0, "key"),
                $( field_table!(@field $number $name $($nested)?), )*
            ];

            /// The name field `number` decodes to.
            pub fn name(number: u32) -> Option<&'static str> {
                Self::FIELDS
                    .iter()
                    .find(|field| field.number == number)
                    .map(|field| field.name)
            }

            /// Every field number, comma-separated, for a subscription request.
            pub fn all_fields() -> String {
                csv(Self::FIELDS.iter().map(|field| field.number))
            }

            $(
                /// The fields subscribed by default: a quote-sized subset of
                /// [`all_fields`](Self::all_fields).
                pub fn default_fields() -> String {
                    csv([$( Self::$default ),*].into_iter())
                }
            )?
        }

        $(#[$doc])*
        #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
        #[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
        #[serde(rename_all = "camelCase")]
        pub struct $payload {
            /// The subscription key (field 0).
            pub key: String,
            $(
                #[serde(default, skip_serializing_if = "Option::is_none")]
                pub $env: Option<$env_ty>,
            )*
            $(
                $(#[$field_doc])*
                #[serde(rename = $name, default, skip_serializing_if = "Option::is_none")]
                pub $field: Option<$ty>,
            )*
            /// Fields the table does not describe, by wire key.
            #[serde(flatten, default, skip_serializing_if = "HashMap::is_empty")]
            pub extra: HashMap<String, Value>,
        }

        impl $payload {
            #[doc = concat!(
                "Decode a content item using [`", stringify!($table), "::FIELDS`]."
            )]
            pub fn from_content(content: &DataContent) -> Result<Self, serde_json::Error> {
                serde_json::from_value(content.to_named_value($table::FIELDS))
            }
        }
    };
    (@field $number:literal $name:literal) => {
        Field::new($number, $name)
    };
    (@field $number:literal $name:literal $nested:ident) => {
        Field::nested($number, $name, $nested::FIELDS)
    };
}

field_table! {
    /// A `LEVELONE_EQUITIES` quote. Deltas carry only the changed fields.
    LevelOneEquityFields => LevelOneEquity {
        key = SYMBOL;
        envelope = {
            delayed: bool,
            asset_main_type: String,
            asset_sub_type: String,
            cusip: String,
        };
        default = [
            SYMBOL, BID_PRICE, ASK_PRICE, LAST_PRICE, BID_SIZE, ASK_SIZE, TOTAL_VOLUME,
            LAST_SIZE, HIGH_PRICE, LOW_PRICE, CLOSE_PRICE, OPEN_PRICE, NET_CHANGE,
            NET_CHANGE_PERCENT, QUOTE_TIME_MILLIS, TRADE_TIME_MILLIS,
        ];
        1 BID_PRICE bid_price "bidPrice": f64,
        2 ASK_PRICE ask_price "askPrice": f64,
        3 LAST_PRICE last_price "lastPrice": f64,
        4 BID_SIZE bid_size "bidSize": i64,
        5 ASK_SIZE ask_size "askSize": i64,
        /// Exchange code of the ask.
        6 ASK_ID ask_id "askId": String,
        /// Exchange code of the bid.
        7 BID_ID bid_id "bidId": String,
        8 TOTAL_VOLUME total_volume "totalVolume": i64,
        9 LAST_SIZE last_size "lastSize": i64,
        10 HIGH_PRICE high_price "highPrice": f64,
        11 LOW_PRICE low_price "lowPrice": f64,
        /// Previous day's close.
        12 CLOSE_PRICE close_price "closePrice": f64,
        /// Primary listing exchange code.
        13 EXCHANGE_ID exchange_id "exchangeId": String,
        14 MARGINABLE marginable "marginable": bool,
        15 DESCRIPTION description "description": String,
        /// Exchange code of the last trade.
        16 LAST_ID last_id "lastId": String,
        17 OPEN_PRICE open_price "openPrice": f64,
        18 NET_CHANGE net_change "netChange": f64,
        19 HIGH_52_WEEK fifty_two_week_high "52WeekHigh": f64,
        20 LOW_52_WEEK fifty_two_week_low "52WeekLow": f64,
        21 PE_RATIO pe_ratio "peRatio": f64,
        /// Annual dividend amount.
        22 DIV_AMOUNT div_amount "divAmount": f64,
        23 DIV_YIELD div_yield "divYield": f64,
        /// Mutual fund net asset value.
        24 NAV nav "nav": f64,
        25 EXCHANGE_NAME exchange_name "exchangeName": String,
        26 DIV_DATE div_date "divDate": String,
        /// Whether the last quote came from the regular session.
        27 IS_REGULAR_MARKET_QUOTE regular_market_quote "regularMarketQuote": bool,
        /// Whether the last trade came from the regular session.
        28 IS_REGULAR_MARKET_TRADE regular_market_trade "regularMarketTrade": bool,
        29 REGULAR_MARKET_LAST_PRICE regular_market_last_price "regularMarketLastPrice": f64,
        30 REGULAR_MARKET_LAST_SIZE regular_market_last_size "regularMarketLastSize": i64,
        31 REGULAR_MARKET_NET_CHANGE regular_market_net_change "regularMarketNetChange": f64,
        32 SECURITY_STATUS security_status "securityStatus": String,
        33 MARK mark "mark": f64,
        /// Epoch milliseconds.
        34 QUOTE_TIME_MILLIS quote_time "quoteTime": i64,
        /// Epoch milliseconds.
        35 TRADE_TIME_MILLIS trade_time "tradeTime": i64,
        /// Epoch milliseconds.
        36 REGULAR_MARKET_TRADE_MILLIS regular_market_trade_time "regularMarketTradeTime": i64,
        /// Epoch milliseconds.
        37 BID_TIME_MILLIS bid_time "bidTime": i64,
        /// Epoch milliseconds.
        38 ASK_TIME_MILLIS ask_time "askTime": i64,
        39 ASK_MIC_ID ask_mic_id "askMICId": String,
        40 BID_MIC_ID bid_mic_id "bidMICId": String,
        41 LAST_MIC_ID last_mic_id "lastMICId": String,
        42 NET_CHANGE_PERCENT net_percent_change "netPercentChange": f64,
        43 REGULAR_MARKET_CHANGE_PERCENT regular_market_percent_change
            "regularMarketPercentChange": f64,
        44 MARK_CHANGE mark_change "markChange": f64,
        45 MARK_CHANGE_PERCENT mark_percent_change "markPercentChange": f64,
        46 HTB_QUANTITY htb_quantity "htbQuantity": i64,
        47 HTB_RATE htb_rate "htbRate": f64,
        /// `-1` unknown, `0` no, `1` yes.
        48 HARD_TO_BORROW hard_to_borrow "hardToBorrow": i64,
        /// `-1` unknown, `0` no, `1` yes.
        49 IS_SHORTABLE shortable "shortable": i64,
        50 POST_MARKET_NET_CHANGE post_market_change "postMarketChange": f64,
        51 POST_MARKET_CHANGE_PERCENT post_market_percent_change "postMarketPercentChange": f64,
    }
}

field_table! {
    /// A `LEVELONE_OPTIONS` quote. Deltas carry only the changed fields.
    LevelOneOptionFields => LevelOneOption {
        key = SYMBOL;
        envelope = { delayed: bool, asset_main_type: String };
        1 DESCRIPTION description "description": String,
        2 BID_PRICE bid_price "bidPrice": f64,
        3 ASK_PRICE ask_price "askPrice": f64,
        4 LAST_PRICE last_price "lastPrice": f64,
        5 HIGH_PRICE high_price "highPrice": f64,
        6 LOW_PRICE low_price "lowPrice": f64,
        /// Previous day's close.
        7 CLOSE_PRICE close_price "closePrice": f64,
        8 TOTAL_VOLUME total_volume "totalVolume": i64,
        9 OPEN_INTEREST open_interest "openInterest": i64,
        10 VOLATILITY volatility "volatility": f64,
        11 INTRINSIC_VALUE money_intrinsic_value "moneyIntrinsicValue": f64,
        12 EXP_YEAR expiration_year "expirationYear": i64,
        13 MULTIPLIER multiplier "multiplier": f64,
        /// Decimal places in the price.
        14 DIGITS digits "digits": i64,
        15 OPEN_PRICE open_price "openPrice": f64,
        16 BID_SIZE bid_size "bidSize": i64,
        17 ASK_SIZE ask_size "askSize": i64,
        18 LAST_SIZE last_size "lastSize": i64,
        19 NET_CHANGE net_change "netChange": f64,
        20 STRIKE_PRICE strike_price "strikePrice": f64,
        /// `C` or `P`.
        21 CONTRACT_TYPE contract_type "contractType": String,
        22 UNDERLYING underlying "underlying": String,
        23 EXP_MONTH expiration_month "expirationMonth": i64,
        24 DELIVERABLES deliverables "deliverables": String,
        25 TIME_VALUE time_value "timeValue": f64,
        26 EXP_DAY expiration_day "expirationDay": i64,
        27 DAYS_TO_EXP days_to_expiration "daysToExpiration": i64,
        28 DELTA delta "delta": f64,
        29 GAMMA gamma "gamma": f64,
        30 THETA theta "theta": f64,
        31 VEGA vega "vega": f64,
        32 RHO rho "rho": f64,
        33 SECURITY_STATUS security_status "securityStatus": String,
        34 THEORETICAL_VALUE theoretical_option_value "theoreticalOptionValue": f64,
        35 UNDERLYING_PRICE underlying_price "underlyingPrice": f64,
        36 UV_EXPIRATION_TYPE uv_expiration_type "uvExpirationType": String,
        37 MARK mark "mark": f64,
        /// Epoch milliseconds.
        38 QUOTE_TIME_MILLIS quote_time "quoteTime": i64,
        /// Epoch milliseconds.
        39 TRADE_TIME_MILLIS trade_time "tradeTime": i64,
        40 EXCHANGE exchange "exchange": String,
        41 EXCHANGE_NAME exchange_name "exchangeName": String,
        /// Epoch milliseconds.
        42 LAST_TRADING_DAY last_trading_day "lastTradingDay": i64,
        43 SETTLEMENT_TYPE settlement_type "settlementType": String,
        44 NET_PERCENT_CHANGE net_percent_change "netPercentChange": f64,
        45 MARK_NET_CHANGE mark_change "markChange": f64,
        46 MARK_PERCENT_CHANGE mark_percent_change "markPercentChange": f64,
        47 IMPLIED_YIELD implied_yield "impliedYield": f64,
        48 IS_PENNY_PILOT is_penny_pilot "isPennyPilot": bool,
        49 OPTION_ROOT option_root "optionRoot": String,
        50 HIGH_52_WEEK fifty_two_week_high "52WeekHigh": f64,
        51 LOW_52_WEEK fifty_two_week_low "52WeekLow": f64,
        52 INDICATIVE_ASK_PRICE indicative_ask_price "indAskPrice": f64,
        53 INDICATIVE_BID_PRICE indicative_bid_price "indBidPrice": f64,
        /// Epoch milliseconds.
        54 INDICATIVE_QUOTE_TIME indicative_quote_time "indQuoteTime": i64,
        55 EXERCISE_TYPE exercise_type "exerciseType": String,
    }
}

field_table! {
    /// A `LEVELONE_FUTURES` quote. Deltas carry only the changed fields.
    LevelOneFuturesFields => LevelOneFutures {
        key = SYMBOL;
        envelope = { delayed: bool, asset_main_type: String };
        1 BID_PRICE bid_price "bidPrice": f64,
        2 ASK_PRICE ask_price "askPrice": f64,
        3 LAST_PRICE last_price "lastPrice": f64,
        4 BID_SIZE bid_size "bidSize": i64,
        5 ASK_SIZE ask_size "askSize": i64,
        /// Exchange code of the bid.
        6 BID_ID bid_id "bidId": String,
        /// Exchange code of the ask.
        7 ASK_ID ask_id "askId": String,
        8 TOTAL_VOLUME total_volume "totalVolume": f64,
        9 LAST_SIZE last_size "lastSize": i64,
        /// Epoch milliseconds.
        10 QUOTE_TIME quote_time "quoteTime": i64,
        /// Epoch milliseconds.
        11 TRADE_TIME trade_time "tradeTime": i64,
        12 HIGH_PRICE high_price "highPrice": f64,
        13 LOW_PRICE low_price "lowPrice": f64,
        /// Previous day's close.
        14 CLOSE_PRICE close_price "closePrice": f64,
        15 EXCHANGE_ID exchange_id "exchangeId": String,
        16 DESCRIPTION description "description": String,
        /// Exchange code of the last trade.
        17 LAST_ID last_id "lastId": String,
        18 OPEN_PRICE open_price "openPrice": f64,
        19 NET_CHANGE net_change "netChange": f64,
        20 FUTURE_PERCENT_CHANGE future_percent_change "futurePercentChange": f64,
        21 EXCHANGE_NAME exchange_name "exchangeName": String,
        22 SECURITY_STATUS security_status "securityStatus": String,
        23 OPEN_INTEREST open_interest "openInterest": i64,
        24 MARK mark "mark": f64,
        /// Minimum price movement.
        25 TICK tick "tick": f64,
        /// Money value of one tick.
        26 TICK_AMOUNT tick_amount "tickAmount": f64,
        27 PRODUCT product "product": String,
        28 FUTURE_PRICE_FORMAT future_price_format "futurePriceFormat": String,
        29 FUTURE_TRADING_HOURS future_trading_hours "futureTradingHours": String,
        30 FUTURE_IS_TRADABLE future_is_tradable "futureIsTradable": bool,
        31 FUTURE_MULTIPLIER future_multiplier "futureMultiplier": f64,
        32 FUTURE_IS_ACTIVE future_is_active "futureIsActive": bool,
        33 FUTURE_SETTLEMENT_PRICE future_settlement_price "futureSettlementPrice": f64,
        34 FUTURE_ACTIVE_SYMBOL future_active_symbol "futureActiveSymbol": String,
        /// Epoch milliseconds.
        35 FUTURE_EXPIRATION_DATE future_expiration_date "futureExpirationDate": i64,
        36 EXPIRATION_STYLE expiration_style "expirationStyle": String,
        /// Epoch milliseconds.
        37 ASK_TIME ask_time "askTime": i64,
        /// Epoch milliseconds.
        38 BID_TIME bid_time "bidTime": i64,
        39 QUOTED_IN_SESSION quoted_in_session "quotedInSession": bool,
        /// Epoch milliseconds.
        40 SETTLEMENT_DATE settlement_date "settlementDate": i64,
    }
}

field_table! {
    /// A `LEVELONE_FUTURES_OPTIONS` quote. Deltas carry only the changed fields.
    LevelOneFuturesOptionFields => LevelOneFuturesOption {
        key = SYMBOL;
        envelope = { delayed: bool, asset_main_type: String };
        1 BID_PRICE bid_price "bidPrice": f64,
        2 ASK_PRICE ask_price "askPrice": f64,
        3 LAST_PRICE last_price "lastPrice": f64,
        4 BID_SIZE bid_size "bidSize": i64,
        5 ASK_SIZE ask_size "askSize": i64,
        /// Exchange code of the bid.
        6 BID_ID bid_id "bidId": String,
        /// Exchange code of the ask.
        7 ASK_ID ask_id "askId": String,
        8 TOTAL_VOLUME total_volume "totalVolume": i64,
        9 LAST_SIZE last_size "lastSize": i64,
        /// Epoch milliseconds.
        10 QUOTE_TIME quote_time "quoteTime": i64,
        /// Epoch milliseconds.
        11 TRADE_TIME trade_time "tradeTime": i64,
        12 HIGH_PRICE high_price "highPrice": f64,
        13 LOW_PRICE low_price "lowPrice": f64,
        /// Previous day's close.
        14 CLOSE_PRICE close_price "closePrice": f64,
        /// Exchange code of the last trade.
        15 LAST_ID last_id "lastId": String,
        16 DESCRIPTION description "description": String,
        17 OPEN_PRICE open_price "openPrice": f64,
        18 OPEN_INTEREST open_interest "openInterest": f64,
        19 MARK mark "mark": f64,
        /// Minimum price movement.
        20 TICK tick "tick": f64,
        /// Money value of one tick.
        21 TICK_AMOUNT tick_amount "tickAmount": f64,
        22 FUTURE_MULTIPLIER future_multiplier "futureMultiplier": f64,
        23 FUTURE_SETTLEMENT_PRICE future_settlement_price "futureSettlementPrice": f64,
        24 UNDERLYING_SYMBOL underlying_symbol "underlyingSymbol": String,
        25 STRIKE_PRICE strike_price "strikePrice": f64,
        /// Epoch milliseconds.
        26 FUTURE_EXPIRATION_DATE future_expiration_date "futureExpirationDate": i64,
        27 EXPIRATION_STYLE expiration_style "expirationStyle": String,
        /// `C` or `P`.
        28 CONTRACT_TYPE contract_type "contractType": String,
        29 SECURITY_STATUS security_status "securityStatus": String,
        30 EXCHANGE exchange "exchange": String,
        31 EXCHANGE_NAME exchange_name "exchangeName": String,
    }
}

field_table! {
    /// A `LEVELONE_FOREX` quote. Deltas carry only the changed fields.
    LevelOneForexFields => LevelOneForex {
        key = SYMBOL;
        envelope = { delayed: bool, asset_main_type: String };
        1 BID_PRICE bid_price "bidPrice": f64,
        2 ASK_PRICE ask_price "askPrice": f64,
        3 LAST_PRICE last_price "lastPrice": f64,
        4 BID_SIZE bid_size "bidSize": i64,
        5 ASK_SIZE ask_size "askSize": i64,
        6 TOTAL_VOLUME total_volume "totalVolume": f64,
        7 LAST_SIZE last_size "lastSize": i64,
        /// Epoch milliseconds.
        8 QUOTE_TIME quote_time "quoteTime": i64,
        /// Epoch milliseconds.
        9 TRADE_TIME trade_time "tradeTime": i64,
        10 HIGH_PRICE high_price "highPrice": f64,
        11 LOW_PRICE low_price "lowPrice": f64,
        /// Previous day's close.
        12 CLOSE_PRICE close_price "closePrice": f64,
        13 EXCHANGE exchange "exchange": String,
        14 DESCRIPTION description "description": String,
        15 OPEN_PRICE open_price "openPrice": f64,
        16 NET_CHANGE net_change "netChange": f64,
        17 PERCENT_CHANGE percent_change "percentChange": f64,
        18 EXCHANGE_NAME exchange_name "exchangeName": String,
        /// Decimal places in the price.
        19 DIGITS digits "digits": i64,
        20 SECURITY_STATUS security_status "securityStatus": String,
        /// Minimum price movement.
        21 TICK tick "tick": f64,
        /// Money value of one tick.
        22 TICK_AMOUNT tick_amount "tickAmount": f64,
        23 PRODUCT product "product": String,
        24 TRADING_HOURS trading_hours "tradingHours": String,
        25 IS_TRADABLE is_tradable "isTradable": bool,
        26 MARKET_MAKER market_maker "marketMaker": String,
        27 HIGH_52_WEEK fifty_two_week_high "52WeekHigh": f64,
        28 LOW_52_WEEK fifty_two_week_low "52WeekLow": f64,
        29 MARK mark "mark": f64,
    }
}

/// One market maker's quote inside a [`BookLevel`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct BookMarketMaker {
    pub market_maker_id: String,
    pub size: i64,
    /// As sent by the venue; not an epoch timestamp.
    pub quote_time: i64,
}

impl BookMarketMaker {
    /// Field numbers and names of the numbered object on the wire.
    pub const FIELDS: &'static [Field] = &[
        Field::new(0, "marketMakerId"),
        Field::new(1, "size"),
        Field::new(2, "quoteTime"),
    ];
}

/// One price level of a [`Book`] side.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct BookLevel {
    pub price: f64,
    pub aggregate_size: i64,
    pub market_maker_count: i64,
    #[serde(default)]
    pub market_makers: Vec<BookMarketMaker>,
}

impl BookLevel {
    /// Field numbers and names of the numbered object on the wire.
    pub const FIELDS: &'static [Field] = &[
        Field::new(0, "price"),
        Field::new(1, "aggregateSize"),
        Field::new(2, "marketMakerCount"),
        Field::nested(3, "marketMakers", BookMarketMaker::FIELDS),
    ];
}

field_table! {
    /// A `NYSE_BOOK`, `NASDAQ_BOOK` or `OPTIONS_BOOK` depth snapshot. Every
    /// frame carries the whole book.
    BookFields => Book {
        key = SYMBOL;
        envelope = {};
        /// Epoch milliseconds.
        1 MARKET_SNAPSHOT_TIME market_snapshot_time "marketSnapshotTime": i64,
        2 BID_SIDE_LEVELS bid_side_levels "bidSideLevels": Vec<BookLevel> [BookLevel],
        3 ASK_SIDE_LEVELS ask_side_levels "askSideLevels": Vec<BookLevel> [BookLevel],
    }
}

field_table! {
    /// A `CHART_EQUITY` minute candle. `seq` numbers the candles of the
    /// day; Schwab's published field order for this service is wrong, this
    /// table matches the wire.
    ChartEquityFields => ChartEquity {
        key = KEY;
        envelope = { seq: i64 };
        /// Candle index since the pre-market open.
        1 SEQUENCE sequence "sequence": i64,
        2 OPEN_PRICE open "open": f64,
        3 HIGH_PRICE high "high": f64,
        4 LOW_PRICE low "low": f64,
        5 CLOSE_PRICE close "close": f64,
        6 VOLUME volume "volume": f64,
        /// Candle start, epoch milliseconds.
        7 CHART_TIME chart_time "chartTime": i64,
        /// Days since the epoch.
        8 CHART_DAY chart_day "chartDay": i64,
    }
}

field_table! {
    /// A `CHART_FUTURES` minute candle.
    ChartFuturesFields => ChartFutures {
        key = KEY;
        envelope = { seq: i64 };
        /// Candle start, epoch milliseconds.
        1 CHART_TIME chart_time "chartTime": i64,
        2 OPEN_PRICE open "open": f64,
        3 HIGH_PRICE high "high": f64,
        4 LOW_PRICE low "low": f64,
        5 CLOSE_PRICE close "close": f64,
        6 VOLUME volume "volume": f64,
    }
}

/// One ranked symbol in a [`Screener`] result. Named on the wire already.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ScreenerItem {
    pub symbol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_price: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_change: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_percent_change: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub market_share: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_volume: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trades: Option<i64>,
    #[serde(flatten, default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, Value>,
}

field_table! {
    /// A `SCREENER_EQUITY` or `SCREENER_OPTION` ranking. Every frame
    /// carries the whole list.
    ScreenerFields => Screener {
        key = SYMBOL;
        envelope = {};
        /// Epoch milliseconds.
        1 TIMESTAMP timestamp "timestamp": i64,
        2 SORT_FIELD sort_field "sortField": String,
        /// Minutes the ranking covers; `0` is the whole day.
        3 FREQUENCY frequency "frequency": i64,
        4 ITEMS items "items": Vec<ScreenerItem>,
    }
}

field_table! {
    /// An `ACCT_ACTIVITY` order event. `message_data` is JSON serialized
    /// into a string for order events and plain text for errors.
    AcctActivityFields => AccountActivity {
        key = KEY;
        envelope = { seq: i64 };
        1 ACCOUNT account_number "accountNumber": String,
        /// `OrderCreated`, `OrderAccepted`, `OrderFillCompleted`, …
        2 MESSAGE_TYPE message_type "messageType": String,
        3 MESSAGE_DATA message_data "messageData": String,
    }
}

/// A [`DataContent`] decoded for its service.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum StreamPayload {
    LevelOneEquity(LevelOneEquity),
    LevelOneOption(LevelOneOption),
    LevelOneFutures(LevelOneFutures),
    LevelOneFuturesOption(LevelOneFuturesOption),
    LevelOneForex(LevelOneForex),
    Book(Book),
    ChartEquity(ChartEquity),
    ChartFutures(ChartFutures),
    Screener(Screener),
    AccountActivity(AccountActivity),
}

impl StreamService {
    /// The field table for this service; empty for `ADMIN`, which carries
    /// no data.
    pub fn fields(self) -> &'static [Field] {
        match self {
            Self::Admin => &[],
            Self::LeveloneEquities => LevelOneEquityFields::FIELDS,
            Self::LeveloneOptions => LevelOneOptionFields::FIELDS,
            Self::LeveloneFutures => LevelOneFuturesFields::FIELDS,
            Self::LeveloneFuturesOptions => LevelOneFuturesOptionFields::FIELDS,
            Self::LeveloneForex => LevelOneForexFields::FIELDS,
            Self::NyseBook | Self::NasdaqBook | Self::OptionsBook => BookFields::FIELDS,
            Self::ChartEquity => ChartEquityFields::FIELDS,
            Self::ChartFutures => ChartFuturesFields::FIELDS,
            Self::ScreenerEquity | Self::ScreenerOption => ScreenerFields::FIELDS,
            Self::AcctActivity => AcctActivityFields::FIELDS,
        }
    }

    /// Decode one content item of this service.
    pub fn decode(self, content: &DataContent) -> Result<StreamPayload, serde_json::Error> {
        use serde::de::Error as _;
        Ok(match self {
            Self::Admin => {
                return Err(serde_json::Error::custom("ADMIN frames carry no data"));
            }
            Self::LeveloneEquities => {
                StreamPayload::LevelOneEquity(LevelOneEquity::from_content(content)?)
            }
            Self::LeveloneOptions => {
                StreamPayload::LevelOneOption(LevelOneOption::from_content(content)?)
            }
            Self::LeveloneFutures => {
                StreamPayload::LevelOneFutures(LevelOneFutures::from_content(content)?)
            }
            Self::LeveloneFuturesOptions => {
                StreamPayload::LevelOneFuturesOption(LevelOneFuturesOption::from_content(content)?)
            }
            Self::LeveloneForex => {
                StreamPayload::LevelOneForex(LevelOneForex::from_content(content)?)
            }
            Self::NyseBook | Self::NasdaqBook | Self::OptionsBook => {
                StreamPayload::Book(Book::from_content(content)?)
            }
            Self::ChartEquity => StreamPayload::ChartEquity(ChartEquity::from_content(content)?),
            Self::ChartFutures => StreamPayload::ChartFutures(ChartFutures::from_content(content)?),
            Self::ScreenerEquity | Self::ScreenerOption => {
                StreamPayload::Screener(Screener::from_content(content)?)
            }
            Self::AcctActivity => {
                StreamPayload::AccountActivity(AccountActivity::from_content(content)?)
            }
        })
    }
}

impl DataItem {
    /// The service this item belongs to, when it is one this crate knows.
    pub fn service(&self) -> Option<StreamService> {
        StreamService::from_api_str(&self.service)
    }

    /// Decode every content item for the item's service.
    pub fn decode(&self) -> Result<Vec<StreamPayload>, serde_json::Error> {
        use serde::de::Error as _;
        let service = self.service().ok_or_else(|| {
            serde_json::Error::custom(format!("unknown streamer service `{}`", self.service))
        })?;
        self.content
            .iter()
            .map(|content| service.decode(content))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn content(value: Value) -> DataContent {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn tables_are_dense_and_unique() {
        for service in [
            StreamService::LeveloneEquities,
            StreamService::LeveloneOptions,
            StreamService::LeveloneFutures,
            StreamService::LeveloneFuturesOptions,
            StreamService::LeveloneForex,
            StreamService::NyseBook,
            StreamService::ChartEquity,
            StreamService::ChartFutures,
            StreamService::ScreenerEquity,
            StreamService::AcctActivity,
        ] {
            let table = service.fields();
            for (index, field) in table.iter().enumerate() {
                assert_eq!(field.number as usize, index, "{service:?} {}", field.name);
            }
            let mut names: Vec<_> = table.iter().map(|field| field.name).collect();
            names.sort_unstable();
            names.dedup();
            assert_eq!(names.len(), table.len(), "{service:?} names repeat");
        }
        assert_eq!(LevelOneEquityFields::FIELDS.len(), 52);
        assert_eq!(LevelOneOptionFields::FIELDS.len(), 56);
        assert_eq!(LevelOneFuturesFields::FIELDS.len(), 41);
        assert_eq!(LevelOneFuturesOptionFields::FIELDS.len(), 32);
        assert_eq!(LevelOneForexFields::FIELDS.len(), 30);
        assert!(StreamService::Admin.fields().is_empty());
    }

    #[test]
    fn payload_names_match_the_table() {
        // Every table name must be a field the payload deserializes, so a
        // fully-populated wire object round-trips with nothing in `extra`.
        let numbered: Map<String, Value> = LevelOneEquityFields::FIELDS
            .iter()
            .skip(1)
            .map(|field| {
                let value = match field.name {
                    "askId" | "bidId" | "lastId" | "exchangeId" | "description"
                    | "exchangeName" | "divDate" | "securityStatus" | "askMICId" | "bidMICId"
                    | "lastMICId" => json!("x"),
                    "marginable" | "regularMarketQuote" | "regularMarketTrade" => json!(true),
                    _ => json!(1),
                };
                (field.number.to_string(), value)
            })
            .collect();
        let mut wire = numbered.clone();
        wire.insert("key".into(), json!("AAPL"));
        let quote = LevelOneEquity::from_content(&content(Value::Object(wire))).unwrap();
        assert!(quote.extra.is_empty(), "unmapped: {:?}", quote.extra);
        assert_eq!(quote.post_market_percent_change, Some(1.0));

        let named = serde_json::to_value(&quote).unwrap();
        for field in LevelOneEquityFields::FIELDS {
            assert!(
                named.get(field.name).is_some(),
                "{} not serialized",
                field.name
            );
        }
    }

    #[test]
    fn decodes_deltas_and_keeps_unknown_fields() {
        let content = content(json!({
            "key": "AMD", "delayed": false, "assetMainType": "EQUITY",
            "assetSubType": "COE", "cusip": "007903107",
            "1": 217.86, "2": 217.95, "6": "P", "8": 33292396, "99": "future"
        }));
        let quote = LevelOneEquity::from_content(&content).unwrap();
        assert_eq!(quote.key, "AMD");
        assert_eq!(quote.delayed, Some(false));
        assert_eq!(quote.cusip.as_deref(), Some("007903107"));
        assert_eq!(quote.ask_price, Some(217.95));
        assert_eq!(quote.ask_id.as_deref(), Some("P"));
        assert_eq!(quote.last_price, None);
        assert_eq!(quote.extra["99"], "future");

        let named = content.to_named_value(LevelOneEquityFields::FIELDS);
        assert_eq!(named["bidPrice"], 217.86);
        assert_eq!(named["cusip"], "007903107");
        assert_eq!(named["99"], "future");
        assert!(named.get("1").is_none());
    }

    #[test]
    fn option_field_one_is_the_description() {
        let quote = LevelOneOption::from_content(&content(json!({
            "key": "SPXW  251208C06880000", "delayed": false, "assetMainType": "OPTION",
            "1": "SPXW 12/08/2025 6880.00 C", "2": 10.2, "3": 10.6
        })))
        .unwrap();
        assert_eq!(
            quote.description.as_deref(),
            Some("SPXW 12/08/2025 6880.00 C")
        );
        assert_eq!(quote.bid_price, Some(10.2));
    }

    #[test]
    fn book_levels_decode_recursively() {
        let book = Book::from_content(&content(json!({
            "key": "F",
            "1": 1765306322349i64,
            "2": [{"0": 13.12, "1": 31600, "2": 2, "3": [
                {"0": "NYSE", "1": 8800, "2": 49786299},
                {"0": "ARCX", "1": 4200, "2": 49904606}
            ]}],
            "3": [{"0": 13.13, "1": 900, "2": 0, "3": []}]
        })))
        .unwrap();
        let bids = book.bid_side_levels.as_ref().unwrap();
        assert_eq!(bids[0].price, 13.12);
        assert_eq!(bids[0].market_makers[1].market_maker_id, "ARCX");
        assert_eq!(book.ask_side_levels.unwrap()[0].aggregate_size, 900);
    }

    #[test]
    fn charts_screeners_and_activity_decode() {
        let candle = ChartEquity::from_content(&content(json!({
            "seq": 202, "key": "AMD", "1": 425, "2": 221.133, "5": 221.02,
            "6": 11730, "7": 1765307100000i64, "8": 20431
        })))
        .unwrap();
        assert_eq!(candle.seq, Some(202));
        assert_eq!(candle.sequence, Some(425));
        assert_eq!(candle.volume, Some(11730.0));

        let screener = Screener::from_content(&content(json!({
            "key": "NASDAQ_VOLUME_30", "1": 1765307297706i64, "2": "VOLUME", "3": 30,
            "4": [{"symbol": "WBD", "lastPrice": 28.095, "trades": 21302}]
        })))
        .unwrap();
        assert_eq!(screener.items.unwrap()[0].trades, Some(21302));

        let activity = AccountActivity::from_content(&content(json!({
            "seq": 1, "key": "Account Activity",
            "1": "123456789", "2": "OrderCreated", "3": "{\"SchwabOrderID\":\"1\"}"
        })))
        .unwrap();
        assert_eq!(activity.account_number.as_deref(), Some("123456789"));
        assert_eq!(activity.message_type.as_deref(), Some("OrderCreated"));
    }

    #[test]
    fn items_decode_by_service() {
        let item: DataItem = serde_json::from_value(json!({
            "service": "LEVELONE_FOREX", "timestamp": 1, "command": "SUBS",
            "content": [{"key": "EUR/USD", "1": 1.1643, "2": 1.16443}]
        }))
        .unwrap();
        assert_eq!(item.service(), Some(StreamService::LeveloneForex));
        match item.decode().unwrap().as_slice() {
            [StreamPayload::LevelOneForex(quote)] => assert_eq!(quote.ask_price, Some(1.16443)),
            other => panic!("{other:?}"),
        }

        let unknown = DataItem {
            service: "LEVELTWO".into(),
            ..item.clone()
        };
        assert!(
            unknown
                .decode()
                .unwrap_err()
                .to_string()
                .contains("LEVELTWO")
        );
        assert!(StreamService::Admin.decode(&item.content[0]).is_err());

        let wrong_type = content(json!({"key": "EUR/USD", "1": "not a price"}));
        assert!(LevelOneForex::from_content(&wrong_type).is_err());
    }

    #[test]
    fn subscription_field_lists_come_from_the_tables() {
        assert_eq!(
            LevelOneEquityFields::default_fields(),
            "0,1,2,3,4,5,8,9,10,11,12,17,18,42,34,35"
        );
        assert_eq!(LevelOneEquityFields::all_fields(), csv(0..=51));
        assert_eq!(
            StreamService::LeveloneEquities.default_fields(),
            LevelOneEquityFields::default_fields()
        );
        assert_eq!(
            StreamService::LeveloneOptions.default_fields(),
            LevelOneOptionFields::all_fields()
        );
        assert_eq!(StreamService::NasdaqBook.default_fields(), "0,1,2,3");
        assert_eq!(StreamService::AcctActivity.default_fields(), "0,1,2,3");
        assert_eq!(StreamService::Admin.default_fields(), "");
        assert_eq!(LevelOneEquityFields::name(0), Some("key"));
        assert_eq!(LevelOneEquityFields::name(52), None);
    }
}
