//! Model-facing parameters for the time-windowed read tools. The SDK
//! parameter types are Schwab's strict wire shape; these accept what a
//! model is likely to write (a calendar date, RFC 3339 or epoch
//! milliseconds; an omitted window) and resolve to the SDK types, so the
//! request Schwab receives is still built and validated by the SDK.
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc};
use rustyschwab_types::market_data::{FrequencyType, PeriodType, PriceHistoryParams};
use rustyschwab_types::trading::{OrderStatus, OrdersParams, TransactionType, TransactionsParams};
use schemars::JsonSchema;
use serde::Deserialize;

/// Width of an omitted orders or transactions window, ending now.
pub const DEFAULT_WINDOW_DAYS: i64 = 7;

/// A point in time as a model is likely to write it.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum FlexibleTime {
    /// Milliseconds since the Unix epoch.
    EpochMillis(i64),
    /// RFC 3339 with an offset (`2025-01-15T09:30:00-05:00`), a date-time
    /// without one, taken as UTC (`2025-01-15T14:30:00`), or a calendar
    /// date (`YYYY-MM-DD`), which covers that whole UTC day: its start as
    /// a window start, its end as a window end.
    Text(String),
}

/// Which end of a window a time bounds, which decides what a calendar
/// date means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    Start,
    End,
}

impl FlexibleTime {
    /// The instant this stands for as the `bound` of a window.
    pub fn resolve(&self, bound: Bound) -> Result<DateTime<Utc>, String> {
        match self {
            Self::EpochMillis(ms) => DateTime::from_timestamp_millis(*ms)
                .ok_or_else(|| format!("{ms} is out of range for milliseconds since the epoch")),
            Self::Text(text) => {
                let text = text.trim();
                if let Ok(at) = DateTime::parse_from_rfc3339(text) {
                    return Ok(at.with_timezone(&Utc));
                }
                if let Ok(naive) = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f") {
                    return Ok(naive.and_utc());
                }
                if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
                    let start = date
                        .and_hms_opt(0, 0, 0)
                        .expect("midnight exists")
                        .and_utc();
                    return match bound {
                        Bound::Start => Ok(start),
                        Bound::End => shift(start, Duration::days(1) - Duration::milliseconds(1)),
                    };
                }
                Err(format!(
                    "{text:?} is not a calendar date (YYYY-MM-DD), an RFC 3339 timestamp or \
                     milliseconds since the epoch"
                ))
            }
        }
    }
}

/// Resolve a window whose end defaults to `now` and whose start defaults
/// to [`DEFAULT_WINDOW_DAYS`] before the end.
fn window(
    start: Option<&FlexibleTime>,
    end: Option<&FlexibleTime>,
    now: DateTime<Utc>,
) -> Result<(DateTime<Utc>, DateTime<Utc>), String> {
    window_of(start, end, now, DEFAULT_WINDOW_DAYS)
}

/// [`window`] with the omitted start `days` before the end.
pub(crate) fn window_of(
    start: Option<&FlexibleTime>,
    end: Option<&FlexibleTime>,
    now: DateTime<Utc>,
    days: i64,
) -> Result<(DateTime<Utc>, DateTime<Utc>), String> {
    let end = end.map_or(Ok(now), |t| t.resolve(Bound::End))?;
    let start = start.map_or_else(
        || shift(end, -Duration::days(days)),
        |t| t.resolve(Bound::Start),
    )?;
    Ok((start, end))
}

/// `at + by`, refused rather than panicking at the edge of chrono's range
/// (`+262142-12-31` is a valid date to parse).
fn shift(at: DateTime<Utc>, by: Duration) -> Result<DateTime<Utc>, String> {
    at.checked_add_signed(by)
        .ok_or_else(|| format!("{at} is too far from the present for a query window"))
}

/// Parameters of `get_account_orders` and `get_all_orders`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrdersQuery {
    /// Earliest entry time; default seven days before `toEnteredTime`.
    /// Schwab returns only orders entered in the last 60 days, whatever
    /// this asks for.
    #[serde(default, alias = "from_entered_time")]
    pub from_entered_time: Option<FlexibleTime>,
    /// Latest entry time; default now.
    #[serde(default, alias = "to_entered_time")]
    pub to_entered_time: Option<FlexibleTime>,
    /// Page size (at least 1); Schwab's default is 3000.
    #[serde(default, alias = "max_results")]
    pub max_results: Option<u32>,
    /// Only orders in this status.
    #[serde(default)]
    pub status: Option<OrderStatus>,
}

impl OrdersQuery {
    /// The SDK request, with the window defaulted relative to `now`.
    pub fn into_params(self, now: DateTime<Utc>) -> Result<OrdersParams, String> {
        let (from_entered_time, to_entered_time) = window(
            self.from_entered_time.as_ref(),
            self.to_entered_time.as_ref(),
            now,
        )?;
        Ok(OrdersParams {
            from_entered_time,
            to_entered_time,
            max_results: self.max_results,
            status: self.status,
        })
    }
}

/// Parameters of `get_transactions`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransactionsQuery {
    /// Earliest transaction time; default seven days before `endDate`.
    /// Schwab caps the window at one year — transaction history reaches
    /// back further than the 60-day order list.
    #[serde(default, alias = "start_date")]
    pub start_date: Option<FlexibleTime>,
    /// Latest transaction time; default now.
    #[serde(default, alias = "end_date")]
    pub end_date: Option<FlexibleTime>,
    /// Categories to include; default `["TRADE"]`.
    #[serde(default)]
    pub types: Vec<TransactionType>,
    /// Only activity in this symbol.
    #[serde(default)]
    pub symbol: Option<String>,
}

impl TransactionsQuery {
    /// The SDK request, with the window defaulted relative to `now` and
    /// the types defaulted to trades.
    pub fn into_params(self, now: DateTime<Utc>) -> Result<TransactionsParams, String> {
        let (start_date, end_date) = window(self.start_date.as_ref(), self.end_date.as_ref(), now)?;
        let types = if self.types.is_empty() {
            vec![TransactionType::Trade]
        } else {
            self.types
        };
        Ok(TransactionsParams {
            start_date,
            end_date,
            types,
            symbol: self.symbol,
        })
    }
}

/// Parameters of `get_price_history`. Schwab's period tables apply:
/// `periodType` `day` takes `period` 1, 2, 3, 4, 5 or 10 of `minute`
/// candles (`frequency` 1, 5, 10, 15 or 30); `month` takes 1, 2, 3 or 6
/// of `daily` or `weekly` candles; `year` takes 1, 2, 3, 5, 10, 15 or 20
/// of `daily`, `weekly` or `monthly` candles; `ytd` takes 1 of `daily` or
/// `weekly`. An explicit `startDate`/`endDate` window replaces `period`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PriceHistoryQuery {
    pub symbol: String,
    /// Unit of `period`; default `day`.
    #[serde(default, alias = "period_type")]
    pub period_type: Option<PeriodType>,
    /// Number of `periodType` units back from `endDate`.
    #[serde(default)]
    pub period: Option<i32>,
    /// Candle granularity; default depends on `periodType`.
    #[serde(default, alias = "frequency_type")]
    pub frequency_type: Option<FrequencyType>,
    /// Candles per `frequencyType` unit; default 1.
    #[serde(default)]
    pub frequency: Option<i32>,
    /// Inclusive start of the candle window.
    #[serde(default, alias = "start_date")]
    pub start_date: Option<FlexibleTime>,
    /// Inclusive end of the candle window; default now.
    #[serde(default, alias = "end_date")]
    pub end_date: Option<FlexibleTime>,
    /// Include pre- and post-market candles.
    #[serde(default, alias = "need_extended_hours_data")]
    pub need_extended_hours_data: Option<bool>,
    /// Include the previous session's close.
    #[serde(default, alias = "need_previous_close")]
    pub need_previous_close: Option<bool>,
}

impl PriceHistoryQuery {
    /// The SDK request, with the window resolved to epoch milliseconds.
    pub fn into_params(self) -> Result<PriceHistoryParams, String> {
        let millis = |time: Option<FlexibleTime>, bound: Bound| {
            time.map(|t| t.resolve(bound).map(|at| at.timestamp_millis()))
                .transpose()
        };
        Ok(PriceHistoryParams {
            symbol: self.symbol,
            period_type: self.period_type,
            period: self.period,
            frequency_type: self.frequency_type,
            frequency: self.frequency,
            start_date: millis(self.start_date, Bound::Start)?,
            end_date: millis(self.end_date, Bound::End)?,
            need_extended_hours_data: self.need_extended_hours_data,
            need_previous_close: self.need_previous_close,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn times_resolve_from_every_accepted_spelling() {
        let noon = at("2025-01-15T12:00:00Z");
        let resolve = |v: serde_json::Value, bound| {
            serde_json::from_value::<FlexibleTime>(v)
                .unwrap()
                .resolve(bound)
        };
        assert_eq!(
            resolve(json!(noon.timestamp_millis()), Bound::Start).unwrap(),
            noon
        );
        assert_eq!(
            resolve(json!("2025-01-15T07:00:00-05:00"), Bound::End).unwrap(),
            noon
        );
        assert_eq!(
            resolve(json!("2025-01-15T12:00:00"), Bound::Start).unwrap(),
            noon
        );
        assert_eq!(
            resolve(json!("2025-01-15T12:00:00.250"), Bound::Start).unwrap(),
            noon + Duration::milliseconds(250)
        );
        assert_eq!(
            resolve(json!("2025-01-15"), Bound::Start).unwrap(),
            at("2025-01-15T00:00:00Z")
        );
        assert_eq!(
            resolve(json!(" 2025-01-15 "), Bound::End).unwrap(),
            at("2025-01-15T23:59:59.999Z")
        );
        assert!(resolve(json!("yesterday"), Bound::Start).is_err());
        assert!(resolve(json!("2025-13-01"), Bound::Start).is_err());
        assert!(resolve(json!(i64::MAX), Bound::Start).is_err());
        assert!(serde_json::from_value::<FlexibleTime>(json!(true)).is_err());
        // Chrono's last and first dates parse: the last day's end is its
        // final millisecond, and a default window before the first is refused.
        let last = resolve(json!(NaiveDate::MAX.to_string()), Bound::End).unwrap();
        assert_eq!(last.date_naive(), NaiveDate::MAX);
        let earliest = FlexibleTime::Text(NaiveDate::MIN.to_string());
        assert!(window(None, Some(&earliest), Utc::now()).is_err());
    }

    #[test]
    fn orders_and_transactions_default_to_the_last_seven_days() {
        let now = at("2025-01-15T12:00:00Z");
        let orders: OrdersQuery = serde_json::from_value(json!({})).unwrap();
        let params = orders.into_params(now).unwrap();
        assert_eq!(params.to_entered_time, now);
        assert_eq!(params.from_entered_time, at("2025-01-08T12:00:00Z"));
        assert_eq!(params.max_results, None);
        params.validate().unwrap();

        // A lone end anchors the default window; a lone start runs to now.
        let ending: OrdersQuery =
            serde_json::from_value(json!({"toEnteredTime": "2025-01-10"})).unwrap();
        let params = ending.into_params(now).unwrap();
        assert_eq!(params.to_entered_time, at("2025-01-10T23:59:59.999Z"));
        assert_eq!(params.from_entered_time, at("2025-01-03T23:59:59.999Z"));
        let starting: OrdersQuery = serde_json::from_value(
            json!({"from_entered_time": "2025-01-01", "status": "FILLED", "maxResults": 5}),
        )
        .unwrap();
        let params = starting.into_params(now).unwrap();
        assert_eq!(params.from_entered_time, at("2025-01-01T00:00:00Z"));
        assert_eq!(params.to_entered_time, now);
        assert_eq!(params.status, Some(OrderStatus::Filled));
        assert_eq!(params.max_results, Some(5));

        let transactions: TransactionsQuery =
            serde_json::from_value(json!({"symbol": "AAPL"})).unwrap();
        let params = transactions.into_params(now).unwrap();
        assert_eq!(params.types, [TransactionType::Trade]);
        assert_eq!(params.symbol.as_deref(), Some("AAPL"));
        assert_eq!(params.end_date, now);
        params.validate().unwrap();
        let typed: TransactionsQuery = serde_json::from_value(
            json!({"types": ["DIVIDEND_OR_INTEREST"], "startDate": "2025-01-14", "endDate": "2025-01-14"}),
        )
        .unwrap();
        let params = typed.into_params(now).unwrap();
        assert_eq!(params.types, [TransactionType::DividendOrInterest]);
        assert_eq!(params.start_date, at("2025-01-14T00:00:00Z"));
        assert_eq!(params.end_date, at("2025-01-14T23:59:59.999Z"));

        // Typos are refused rather than silently widening the window.
        assert!(serde_json::from_value::<OrdersQuery>(json!({"fromTime": "2025-01-01"})).is_err());
        // An inverted window is still the SDK's call to refuse.
        let inverted: OrdersQuery = serde_json::from_value(
            json!({"fromEnteredTime": "2025-01-10", "toEnteredTime": "2025-01-01"}),
        )
        .unwrap();
        assert!(inverted.into_params(now).unwrap().validate().is_err());
    }

    #[test]
    fn price_history_windows_become_epoch_milliseconds() {
        let query: PriceHistoryQuery = serde_json::from_value(json!({
            "symbol": "AAPL", "periodType": "day", "period": 5,
            "frequencyType": "minute", "frequency": 5,
            "startDate": "2025-01-13", "endDate": 1736985600000i64,
            "needExtendedHoursData": false
        }))
        .unwrap();
        let params = query.into_params().unwrap();
        assert_eq!(
            params.start_date,
            Some(at("2025-01-13T00:00:00Z").timestamp_millis())
        );
        assert_eq!(params.end_date, Some(1_736_985_600_000));
        assert_eq!(params.period_type, Some(PeriodType::Day));
        assert_eq!(params.frequency, Some(5));
        assert_eq!(params.need_extended_hours_data, Some(false));
        params.validate().unwrap();

        let bare: PriceHistoryQuery = serde_json::from_value(json!({"symbol": "AAPL"})).unwrap();
        let params = bare.into_params().unwrap();
        assert_eq!((params.start_date, params.end_date), (None, None));
        assert!(
            serde_json::from_value::<PriceHistoryQuery>(json!({"symbol": "AAPL", "days": 5}))
                .is_err()
        );
    }
}
