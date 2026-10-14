//! A durable local cache of price history and recorded stream data.
//!
//! A backtest over weeks of minute bars is many price-history requests, and
//! the same window is usually read again on the next run. A [`HistoryStore`]
//! fetches each span once: it records which spans of which series it holds,
//! asks Schwab only for the gaps, in bounded chunks, and serves every later
//! read from its SQLite file.
//!
//! A span is marked complete only up to the start of the current New York
//! date, because the day's bars are still arriving. Bars from today are
//! stored but fetched again on the next read.
//!
//! The store also keeps what Schwab has no history API for: level-one quotes,
//! book snapshots and any other stream payload a recorder chooses to write,
//! keyed by kind, symbol and time, so a backtest can replay them.
//!
//! ```no_run
//! # async fn example(client: schwab_rs::SchwabClient) -> schwab_rs::Result<()> {
//! use schwab_rs::history::{BarSize, HistoryRequest, HistoryStore};
//!
//! let store = HistoryStore::open("candles.sqlite3".into()).await?;
//! let request = HistoryRequest::new("AAPL", BarSize::MINUTE, 1_767_225_600_000, 1_768_435_200_000);
//! let history = store.load(&client, &request).await?;
//! println!("{} candles", history.candles.len());
//! # Ok(()) }
//! ```
use crate::client::SchwabClient;
use crate::error::{Error, Result};
use crate::storage::sql;
use chrono::{TimeZone, Utc};
use chrono_tz::America::New_York;
use rusqlite::{Connection, params};
use schwab_types::market_data::{
    Candle, FrequencyType, PeriodType, PriceHistoryParams, PriceHistoryResponse,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DAY_MS: i64 = 86_400_000;
/// Schema version kept in `PRAGMA user_version`.
const SCHEMA: i64 = 1;

/// The width of one candle in a series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BarSize {
    pub frequency_type: FrequencyType,
    pub frequency: i32,
}

impl BarSize {
    /// One-minute bars, the finest Schwab serves.
    pub const MINUTE: Self = Self {
        frequency_type: FrequencyType::Minute,
        frequency: 1,
    };
    pub const DAILY: Self = Self {
        frequency_type: FrequencyType::Daily,
        frequency: 1,
    };
    pub const WEEKLY: Self = Self {
        frequency_type: FrequencyType::Weekly,
        frequency: 1,
    };
    pub const MONTHLY: Self = Self {
        frequency_type: FrequencyType::Monthly,
        frequency: 1,
    };

    /// Bars of `frequency` units, checked against the frequencies Schwab
    /// accepts for `frequency_type` (1, 5, 10, 15 or 30 for minutes).
    pub fn new(frequency_type: FrequencyType, frequency: i32) -> Result<Self> {
        if !frequency_type.valid_frequencies().contains(&frequency) {
            return Err(Error::InvalidParameter(format!(
                "frequency {frequency} is not valid for frequencyType {}; expected one of {:?}",
                frequency_type.as_str(),
                frequency_type.valid_frequencies()
            )));
        }
        Ok(Self {
            frequency_type,
            frequency,
        })
    }

    /// The period type Schwab pairs with this frequency type.
    fn period_type(self) -> PeriodType {
        match self.frequency_type {
            FrequencyType::Minute => PeriodType::Day,
            _ => PeriodType::Year,
        }
    }

    /// The longest span one request asks for. Minute history is fetched ten
    /// days at a time; coarser bars a decade at a time.
    fn chunk_ms(self) -> i64 {
        match self.frequency_type {
            FrequencyType::Minute => 10 * DAY_MS,
            _ => 3_653 * DAY_MS,
        }
    }

    /// The width of one candle in milliseconds, when it is fixed. Weekly and
    /// monthly bars have none.
    pub fn duration_ms(self) -> Option<i64> {
        match self.frequency_type {
            FrequencyType::Minute => Some(i64::from(self.frequency) * 60_000),
            FrequencyType::Daily => Some(DAY_MS),
            _ => None,
        }
    }

    fn key(self) -> String {
        format!("{}:{}", self.frequency_type.as_str(), self.frequency)
    }
}

/// A span of one series: a symbol's bars of one size, with or without
/// extended-hours bars. Times are epoch milliseconds, both ends inclusive,
/// matching Schwab's `startDate` and `endDate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRequest {
    pub symbol: String,
    pub bars: BarSize,
    pub start_ms: i64,
    pub end_ms: i64,
    #[serde(default)]
    pub extended_hours: bool,
}

impl HistoryRequest {
    /// Regular-hours bars of `symbol` from `start_ms` through `end_ms`.
    pub fn new(symbol: impl Into<String>, bars: BarSize, start_ms: i64, end_ms: i64) -> Self {
        Self {
            symbol: symbol.into(),
            bars,
            start_ms,
            end_ms,
            extended_hours: false,
        }
    }

    pub fn with_extended_hours(mut self, extended_hours: bool) -> Self {
        self.extended_hours = extended_hours;
        self
    }

    fn validate(&self) -> Result<()> {
        if self.symbol.trim().is_empty() || self.symbol.len() > 64 {
            return Err(Error::InvalidParameter(
                "history symbol must be 1 to 64 characters".into(),
            ));
        }
        if self.start_ms < 0 || self.start_ms > self.end_ms {
            return Err(Error::InvalidParameter(
                "history start must be non-negative and not after the end".into(),
            ));
        }
        BarSize::new(self.bars.frequency_type, self.bars.frequency).map(|_| ())
    }

    fn series(&self) -> String {
        let session = if self.extended_hours { "eth" } else { "rth" };
        format!("{}|{}|{session}", self.symbol, self.bars.key())
    }

    fn params(&self, start_ms: i64, end_ms: i64) -> PriceHistoryParams {
        PriceHistoryParams {
            period_type: Some(self.bars.period_type()),
            frequency_type: Some(self.bars.frequency_type),
            frequency: Some(self.bars.frequency),
            start_date: Some(start_ms),
            end_date: Some(end_ms),
            need_extended_hours_data: Some(self.extended_hours),
            ..PriceHistoryParams::new(self.symbol.clone())
        }
    }
}

/// Where a [`HistoryStore`] fetches the spans it does not hold.
/// [`SchwabClient`] is the production source.
#[async_trait::async_trait]
pub trait HistorySource: Send + Sync {
    async fn price_history(&self, params: &PriceHistoryParams) -> Result<PriceHistoryResponse>;
}

#[async_trait::async_trait]
impl HistorySource for SchwabClient {
    async fn price_history(&self, params: &PriceHistoryParams) -> Result<PriceHistoryResponse> {
        self.get_price_history(params).await
    }
}

/// One recorded stream payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recorded {
    /// What was recorded, such as `LEVELONE_EQUITIES` or `NASDAQ_BOOK`.
    pub kind: String,
    pub symbol: String,
    pub at_ms: i64,
    pub payload: serde_json::Value,
}

/// The cache. Clones share one connection; reads and writes run on blocking
/// threads.
#[derive(Clone)]
pub struct HistoryStore {
    path: PathBuf,
    connection: Arc<parking_lot::Mutex<Connection>>,
}

impl std::fmt::Debug for HistoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl HistoryStore {
    /// Open or create the cache at `path`, creating its directory.
    pub async fn open(path: PathBuf) -> Result<Self> {
        let opened = path.clone();
        let connection = tokio::task::spawn_blocking(move || initialize(&opened))
            .await
            .map_err(|_| Error::Storage("History worker failed".into()))??;
        Ok(Self {
            path,
            connection: Arc::new(parking_lot::Mutex::new(connection)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || f(&mut connection.lock()))
            .await
            .map_err(|_| Error::Storage("History worker failed".into()))?
    }

    /// The candles of `request`'s span that are in the cache, in time
    /// order. Nothing is fetched.
    pub async fn candles(&self, request: &HistoryRequest) -> Result<Vec<Candle>> {
        request.validate()?;
        let (series, start, end) = (request.series(), request.start_ms, request.end_ms);
        self.run(move |c| read_candles(c, &series, start, end))
            .await
    }

    /// The parts of `request`'s span the cache does not hold as complete,
    /// in time order.
    pub async fn missing(&self, request: &HistoryRequest) -> Result<Vec<(i64, i64)>> {
        request.validate()?;
        let (series, start, end) = (request.series(), request.start_ms, request.end_ms);
        self.run(move |c| Ok(gaps(&read_spans(c, &series)?, start, end)))
            .await
    }

    /// Store candles for `request`'s series, replacing any at the same
    /// times, and mark `complete` (inclusive) as held in full. Candles come
    /// from a fetch or from a recorder; only mark a span complete when every
    /// bar in it is present.
    pub async fn put_candles(
        &self,
        request: &HistoryRequest,
        candles: Vec<Candle>,
        complete: Option<(i64, i64)>,
    ) -> Result<()> {
        request.validate()?;
        if let Some((start, end)) = complete
            && start > end
        {
            return Err(Error::InvalidParameter(
                "complete span start is after its end".into(),
            ));
        }
        let series = request.series();
        self.run(move |c| write_candles(c, &series, &candles, complete))
            .await
    }

    /// `request`'s candles, fetching the spans the cache lacks from
    /// `source` first, at most one chunk per request. Fetched bars before
    /// the current New York date are marked complete; today's are stored
    /// and fetched again next time. The span's end is clamped to now.
    pub async fn load(
        &self,
        source: &dyn HistorySource,
        request: &HistoryRequest,
    ) -> Result<PriceHistoryResponse> {
        self.load_at(source, request, Utc::now().timestamp_millis())
            .await
    }

    async fn load_at(
        &self,
        source: &dyn HistorySource,
        request: &HistoryRequest,
        now_ms: i64,
    ) -> Result<PriceHistoryResponse> {
        request.validate()?;
        let mut bounded = request.clone();
        bounded.end_ms = bounded.end_ms.min(now_ms);
        let settled = new_york_midnight(now_ms)?;
        if bounded.start_ms <= bounded.end_ms {
            for (gap_start, gap_end) in self.missing(&bounded).await? {
                let mut start = gap_start;
                while start <= gap_end {
                    let end = start
                        .saturating_add(bounded.bars.chunk_ms() - 1)
                        .min(gap_end);
                    let fetched = source.price_history(&bounded.params(start, end)).await?;
                    let complete = (start < settled).then(|| (start, end.min(settled - 1)));
                    self.put_candles(&bounded, fetched.candles, complete)
                        .await?;
                    start = end + 1;
                }
            }
        }
        let candles = self.candles(request).await?;
        Ok(PriceHistoryResponse::new(request.symbol.clone(), candles))
    }

    /// Append one recorded stream payload.
    pub async fn record(&self, entry: Recorded) -> Result<()> {
        self.record_all(vec![entry]).await
    }

    /// Append recorded stream payloads in one transaction.
    pub async fn record_all(&self, entries: Vec<Recorded>) -> Result<()> {
        if let Some(bad) = entries
            .iter()
            .find(|e| e.kind.is_empty() || e.symbol.is_empty() || e.at_ms < 0)
        {
            return Err(Error::InvalidParameter(format!(
                "recorded entry needs a kind, a symbol and a non-negative time: {:?}/{:?}",
                bad.kind, bad.symbol
            )));
        }
        self.run(move |c| {
            let tx = c.transaction().map_err(sql)?;
            {
                let mut insert = tx
                    .prepare_cached(
                        "INSERT INTO recorded(kind, symbol, at_ms, payload) VALUES (?1, ?2, ?3, ?4)",
                    )
                    .map_err(sql)?;
                for entry in &entries {
                    insert
                        .execute(params![
                            entry.kind,
                            entry.symbol,
                            entry.at_ms,
                            entry.payload.to_string()
                        ])
                        .map_err(sql)?;
                }
            }
            tx.commit().map_err(sql)
        })
        .await
    }

    /// Recorded payloads of `kind` for `symbols` from `start_ms` through
    /// `end_ms`, in time order and, at one instant, in recording order.
    pub async fn recorded(
        &self,
        kind: &str,
        symbols: &[String],
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<Recorded>> {
        let kind = kind.to_owned();
        let symbols = symbols.to_vec();
        self.run(move |c| {
            let mut statement = c
                .prepare_cached(
                    "SELECT symbol, at_ms, payload FROM recorded \
                     WHERE kind = ?1 AND symbol = ?2 AND at_ms BETWEEN ?3 AND ?4 ORDER BY at_ms, id",
                )
                .map_err(sql)?;
            let mut out = Vec::new();
            for symbol in &symbols {
                let rows = statement
                    .query_map(params![kind, symbol, start_ms, end_ms], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    })
                    .map_err(sql)?;
                for row in rows {
                    let (symbol, at_ms, payload) = row.map_err(sql)?;
                    out.push(Recorded {
                        kind: kind.clone(),
                        symbol,
                        at_ms,
                        payload: serde_json::from_str(&payload)?,
                    });
                }
            }
            // Stable, so one instant keeps each symbol's recording order.
            out.sort_by_key(|r| r.at_ms);
            Ok(out)
        })
        .await
    }
}

fn initialize(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let c = Connection::open(path).map_err(sql)?;
    c.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(sql)?;
    c.pragma_update(None, "journal_mode", "WAL").map_err(sql)?;
    let version: i64 = c
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(sql)?;
    if version > SCHEMA {
        return Err(Error::Storage(format!(
            "History store {} has schema {version}, newer than this SDK's {SCHEMA}",
            path.display()
        )));
    }
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS candles(series TEXT NOT NULL, at_ms INTEGER NOT NULL, \
             open REAL NOT NULL, high REAL NOT NULL, low REAL NOT NULL, close REAL NOT NULL, \
             volume INTEGER NOT NULL, PRIMARY KEY(series, at_ms)) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS spans(series TEXT NOT NULL, start_ms INTEGER NOT NULL, \
             end_ms INTEGER NOT NULL, PRIMARY KEY(series, start_ms));
         CREATE TABLE IF NOT EXISTS recorded(id INTEGER PRIMARY KEY AUTOINCREMENT, \
             kind TEXT NOT NULL, symbol TEXT NOT NULL, at_ms INTEGER NOT NULL, payload TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS recorded_by_time ON recorded(kind, symbol, at_ms);
         PRAGMA user_version = 1;",
    )
    .map_err(sql)?;
    Ok(c)
}

/// Midnight in New York on the date `at_ms` falls on, in epoch milliseconds.
fn new_york_midnight(at_ms: i64) -> Result<i64> {
    let date = New_York
        .timestamp_millis_opt(at_ms)
        .single()
        .ok_or_else(|| Error::InvalidParameter(format!("time {at_ms} is out of range")))?
        .date_naive();
    New_York
        .from_local_datetime(&date.and_time(chrono::NaiveTime::MIN))
        .earliest()
        .map(|midnight| midnight.timestamp_millis())
        .ok_or_else(|| Error::InvalidParameter(format!("no New York midnight on {date}")))
}

fn read_candles(c: &Connection, series: &str, start: i64, end: i64) -> Result<Vec<Candle>> {
    let mut statement = c
        .prepare_cached(
            "SELECT at_ms, open, high, low, close, volume FROM candles \
             WHERE series = ?1 AND at_ms BETWEEN ?2 AND ?3 ORDER BY at_ms",
        )
        .map_err(sql)?;
    let rows = statement
        .query_map(params![series, start, end], |r| {
            Ok(Candle::new(
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .map_err(sql)?;
    rows.collect::<rusqlite::Result<_>>().map_err(sql)
}

fn read_spans(c: &Connection, series: &str) -> Result<Vec<(i64, i64)>> {
    let mut statement = c
        .prepare_cached("SELECT start_ms, end_ms FROM spans WHERE series = ?1 ORDER BY start_ms")
        .map_err(sql)?;
    let rows = statement
        .query_map(params![series], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(sql)?;
    rows.collect::<rusqlite::Result<_>>().map_err(sql)
}

fn write_candles(
    c: &mut Connection,
    series: &str,
    candles: &[Candle],
    complete: Option<(i64, i64)>,
) -> Result<()> {
    let tx = c.transaction().map_err(sql)?;
    {
        let mut insert = tx
            .prepare_cached(
                "INSERT OR REPLACE INTO candles(series, at_ms, open, high, low, close, volume) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .map_err(sql)?;
        for candle in candles {
            let prices = [candle.open, candle.high, candle.low, candle.close];
            if prices.iter().any(|p| !p.is_finite()) || candle.volume < 0 {
                return Err(Error::InvalidParameter(format!(
                    "candle at {} has a non-finite price or negative volume",
                    candle.datetime
                )));
            }
            insert
                .execute(params![
                    series,
                    candle.datetime,
                    candle.open,
                    candle.high,
                    candle.low,
                    candle.close,
                    candle.volume
                ])
                .map_err(sql)?;
        }
    }
    if let Some(span) = complete {
        let mut spans = read_spans(&tx, series)?;
        spans.push(span);
        tx.execute("DELETE FROM spans WHERE series = ?1", params![series])
            .map_err(sql)?;
        for (start, end) in merge(spans) {
            tx.execute(
                "INSERT INTO spans(series, start_ms, end_ms) VALUES (?1, ?2, ?3)",
                params![series, start, end],
            )
            .map_err(sql)?;
        }
    }
    tx.commit().map_err(sql)
}

/// Sorted, non-overlapping, non-adjacent spans covering the same times.
fn merge(mut spans: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    spans.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match merged.last_mut() {
            Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// The parts of `start..=end` that sorted, merged `spans` leave uncovered.
fn gaps(spans: &[(i64, i64)], start: i64, end: i64) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    let mut cursor = start;
    for &(span_start, span_end) in spans {
        if span_end < cursor {
            continue;
        }
        if span_start > end {
            break;
        }
        if span_start > cursor {
            out.push((cursor, span_start - 1));
        }
        cursor = span_end.saturating_add(1);
        if cursor > end {
            return out;
        }
    }
    out.push((cursor, end));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    const MINUTE: i64 = 60_000;
    /// 2026-01-05 09:30 New York, a Monday.
    const OPEN: i64 = 1_767_623_400_000;

    fn candle(at: i64, close: f64) -> Candle {
        Candle::new(at, close, close + 0.5, close - 0.5, close, 1_000)
    }

    /// Serves a minute bar for every minute of each requested span and
    /// records the spans asked for.
    #[derive(Default)]
    struct Fake {
        asked: Mutex<Vec<(i64, i64)>>,
    }

    #[async_trait::async_trait]
    impl HistorySource for Fake {
        async fn price_history(&self, params: &PriceHistoryParams) -> Result<PriceHistoryResponse> {
            params.validate()?;
            let (start, end) = (params.start_date.unwrap(), params.end_date.unwrap());
            self.asked.lock().push((start, end));
            let first = start.div_euclid(MINUTE) * MINUTE;
            let candles = (0..)
                .map(|i| first + i * MINUTE)
                .skip_while(|at| *at < start)
                .take_while(|at| *at <= end)
                .map(|at| candle(at, 100.0))
                .collect();
            Ok(PriceHistoryResponse::new(params.symbol.clone(), candles))
        }
    }

    async fn store() -> (tempfile::TempDir, HistoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(dir.path().join("cache/history.sqlite3"))
            .await
            .unwrap();
        (dir, store)
    }

    #[test]
    fn spans_merge_and_leave_gaps() {
        assert_eq!(
            merge(vec![(10, 20), (0, 5), (6, 8), (19, 30)]),
            vec![(0, 8), (10, 30)]
        );
        let spans = [(0, 8), (10, 30)];
        assert_eq!(gaps(&spans, 0, 30), vec![(9, 9)]);
        assert_eq!(gaps(&spans, 5, 40), vec![(9, 9), (31, 40)]);
        assert_eq!(gaps(&spans, 12, 20), vec![]);
        assert_eq!(gaps(&[], 3, 4), vec![(3, 4)]);
    }

    #[test]
    fn bar_sizes_follow_schwabs_tables() {
        assert!(BarSize::new(FrequencyType::Minute, 5).is_ok());
        assert!(BarSize::new(FrequencyType::Minute, 2).is_err());
        assert!(BarSize::new(FrequencyType::Daily, 5).is_err());
        assert_eq!(BarSize::MINUTE.duration_ms(), Some(MINUTE));
        assert_eq!(BarSize::WEEKLY.duration_ms(), None);
    }

    #[tokio::test]
    async fn a_settled_span_is_fetched_once() {
        let (_dir, store) = store().await;
        let fake = Fake::default();
        let request = HistoryRequest::new("ABC", BarSize::MINUTE, OPEN, OPEN + 59 * MINUTE);
        let now = OPEN + 3 * DAY_MS;
        let first = store.load_at(&fake, &request, now).await.unwrap();
        assert_eq!(first.candles.len(), 60);
        assert_eq!(fake.asked.lock().len(), 1);
        let again = store.load_at(&fake, &request, now).await.unwrap();
        assert_eq!(again.candles, first.candles);
        assert_eq!(
            fake.asked.lock().len(),
            1,
            "a held span is not fetched again"
        );
        assert!(store.missing(&request).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn only_the_gap_is_fetched_and_long_spans_are_chunked() {
        let (_dir, store) = store().await;
        let fake = Fake::default();
        let now = OPEN + 40 * DAY_MS;
        let early = HistoryRequest::new("ABC", BarSize::MINUTE, OPEN, OPEN + 9 * MINUTE);
        store.load_at(&fake, &early, now).await.unwrap();
        let wide = HistoryRequest::new("ABC", BarSize::MINUTE, OPEN, OPEN + 25 * DAY_MS);
        store.load_at(&fake, &wide, now).await.unwrap();
        let asked = fake.asked.lock().clone();
        assert_eq!(
            asked[1].0,
            OPEN + 9 * MINUTE + 1,
            "starts just after the held span"
        );
        assert_eq!(asked.len(), 4, "25 days in chunks of at most 10");
        assert!(asked[1..].iter().all(|(s, e)| e - s < 10 * DAY_MS));
        assert_eq!(asked.last().unwrap().1, OPEN + 25 * DAY_MS);
    }

    #[tokio::test]
    async fn todays_bars_are_stored_but_fetched_again() {
        let (_dir, store) = store().await;
        let fake = Fake::default();
        let now = OPEN + 30 * MINUTE;
        let request = HistoryRequest::new("ABC", BarSize::MINUTE, OPEN - DAY_MS, OPEN + DAY_MS);
        let first = store.load_at(&fake, &request, now).await.unwrap();
        assert_eq!(
            first.candles.last().unwrap().datetime,
            now.div_euclid(MINUTE) * MINUTE,
            "the end is clamped to now"
        );
        let missing = store.missing(&request).await.unwrap();
        assert_eq!(missing[0].0, new_york_midnight(now).unwrap());
        store.load_at(&fake, &request, now).await.unwrap();
        let asked = fake.asked.lock().clone();
        assert_eq!(asked.len(), 2);
        assert_eq!(asked[1].0, new_york_midnight(now).unwrap());
    }

    #[tokio::test]
    async fn series_are_kept_apart() {
        let (_dir, store) = store().await;
        let fake = Fake::default();
        let now = OPEN + 3 * DAY_MS;
        let regular = HistoryRequest::new("ABC", BarSize::MINUTE, OPEN, OPEN + 4 * MINUTE);
        store.load_at(&fake, &regular, now).await.unwrap();
        let extended = regular.clone().with_extended_hours(true);
        assert!(store.candles(&extended).await.unwrap().is_empty());
        let other = HistoryRequest::new("XYZ", BarSize::MINUTE, OPEN, OPEN + 4 * MINUTE);
        assert_eq!(
            store.missing(&other).await.unwrap(),
            vec![(other.start_ms, other.end_ms)]
        );
    }

    #[tokio::test]
    async fn bad_input_is_refused() {
        let (_dir, store) = store().await;
        let backwards = HistoryRequest::new("ABC", BarSize::MINUTE, 10, 5);
        assert!(store.candles(&backwards).await.is_err());
        let request = HistoryRequest::new("ABC", BarSize::MINUTE, 0, 10);
        let mut nan = candle(0, 1.0);
        nan.close = f64::NAN;
        assert!(store.put_candles(&request, vec![nan], None).await.is_err());
        let blank = Recorded {
            kind: String::new(),
            symbol: "ABC".into(),
            at_ms: 0,
            payload: serde_json::json!({}),
        };
        assert!(store.record(blank).await.is_err());
    }

    #[tokio::test]
    async fn recordings_replay_in_time_order() {
        let (dir, store) = store().await;
        let entry = |symbol: &str, at_ms: i64, bid: f64| Recorded {
            kind: "LEVELONE_EQUITIES".into(),
            symbol: symbol.into(),
            at_ms,
            payload: serde_json::json!({ "bid": bid }),
        };
        store
            .record_all(vec![
                entry("B", 20, 2.0),
                entry("A", 10, 1.0),
                entry("A", 20, 1.5),
            ])
            .await
            .unwrap();
        store.record(entry("A", 30, 1.7)).await.unwrap();
        let reopened = HistoryStore::open(dir.path().join("cache/history.sqlite3"))
            .await
            .unwrap();
        let replay = reopened
            .recorded("LEVELONE_EQUITIES", &["A".into(), "B".into()], 0, 25)
            .await
            .unwrap();
        let seen: Vec<_> = replay
            .iter()
            .map(|r| (r.symbol.as_str(), r.at_ms))
            .collect();
        assert_eq!(seen, vec![("A", 10), ("A", 20), ("B", 20)]);
        assert_eq!(replay[0].payload["bid"], 1.0);
        assert!(
            reopened
                .recorded("NASDAQ_BOOK", &["A".into()], 0, 100)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite3");
        let c = Connection::open(&path).unwrap();
        c.pragma_update(None, "user_version", 99).unwrap();
        drop(c);
        assert!(HistoryStore::open(path).await.is_err());
    }

    #[test]
    fn midnight_is_new_york_local() {
        // 2026-01-05 09:30 EST is 14:30 UTC; midnight EST is 05:00 UTC.
        assert_eq!(
            new_york_midnight(OPEN).unwrap(),
            OPEN - (9 * 60 + 30) * MINUTE
        );
    }
}
