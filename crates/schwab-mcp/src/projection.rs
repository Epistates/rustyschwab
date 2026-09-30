//! Compact, bounded projections over Schwab's raw responses for
//! `get_candles`, `get_option_contracts` and `get_positions`.
//!
//! The raw tools keep Schwab's shape; these answer the common questions in
//! a fraction of the bytes. Rows are arrays under a `columns` header, in a
//! deterministic order, and a page stops at the response budget or the
//! caller's `limit` with `truncated: true` and a `nextCursor` that resumes
//! after the last row, so nothing is dropped silently. Each page re-reads
//! the query from Schwab; the cursor is the last row's ordering key, not a
//! position, so rows added or repriced between pages are never skipped
//! or duplicated.

use std::fmt::{self, Display};
use std::str::FromStr;

use schwab_types::accounts::{Account, Position};
use schwab_types::market_data::{
    OptionChainParams, OptionChainResponse, OptionContract, OptionStrategy, PriceHistoryResponse,
};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::contract::ToolFailure;

/// Env var bounding the encoded rows of one projected page, in bytes.
pub const BUDGET_ENV: &str = "SCHWAB_MCP_RESPONSE_BUDGET_BYTES";

/// Rows per page when [`BUDGET_ENV`] is unset: about a third of a day of
/// minute candles, or a few hundred option contracts.
pub const DEFAULT_BUDGET_BYTES: usize = 64 * 1024;

/// Smallest budget honoured; a page always carries at least one row.
const MIN_BUDGET_BYTES: usize = 4 * 1024;

/// The page budget from the environment, clamped to what the transport
/// can carry; unset or unparsable means the default.
pub fn budget() -> usize {
    parse_budget(std::env::var(BUDGET_ENV).ok().as_deref())
}

fn parse_budget(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_BUDGET_BYTES)
        .clamp(MIN_BUDGET_BYTES, turbomcp_core::MAX_MESSAGE_SIZE)
}

/// One projected row with the key that orders it and resumes after it.
pub struct Row<K> {
    pub key: K,
    pub value: Value,
}

/// A page of rows, plus the tool's own header fields.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    #[serde(flatten)]
    pub header: Map<String, Value>,
    pub columns: &'static [&'static str],
    pub rows: Vec<Value>,
    /// Rows on this page.
    pub count: usize,
    /// Rows the query has in all, before the cursor and the budget.
    pub total: usize,
    /// Rows remain after this page; pass `nextCursor` as `cursor` to
    /// continue from where it stopped.
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

impl Page {
    pub fn into_payload(self) -> Result<Value, ToolFailure> {
        serde_json::to_value(self).map_err(|e| ToolFailure::internal("Projection serialization", e))
    }
}

/// Order `rows` by key, drop those at or before `cursor`, and take rows
/// until `limit` or `budget` bytes of encoded rows, whichever comes first.
pub fn page<K: Ord + Display + FromStr>(
    mut rows: Vec<Row<K>>,
    columns: &'static [&'static str],
    header: Map<String, Value>,
    cursor: Option<&str>,
    limit: Option<u32>,
    budget: usize,
) -> Result<Page, ToolFailure> {
    let limit = match limit {
        Some(0) => {
            return Err(ToolFailure::invalid_arguments(
                "limit must be at least 1; omit it to fill the response budget",
            ));
        }
        Some(n) => n as usize,
        None => usize::MAX,
    };
    let after = cursor
        .map(|c| {
            K::from_str(c.trim()).map_err(|_| {
                ToolFailure::invalid_arguments(format!(
                    "cursor {c:?} is not one this tool issued; pass back nextCursor unchanged"
                ))
            })
        })
        .transpose()?;
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    let total = rows.len();
    let mut remaining = rows
        .into_iter()
        .skip_while(|row| after.as_ref().is_some_and(|after| row.key <= *after))
        .peekable();
    let mut taken = Vec::new();
    let mut used = 0;
    let mut last_key = None;
    while let Some(row) = remaining.peek() {
        let size = row.value.to_string().len() + 1;
        if taken.len() >= limit || (!taken.is_empty() && used + size > budget) {
            break;
        }
        let row = remaining.next().expect("peeked");
        used += size;
        last_key = Some(row.key.to_string());
        taken.push(row.value);
    }
    let truncated = remaining.peek().is_some();
    Ok(Page {
        header,
        columns,
        count: taken.len(),
        rows: taken,
        total,
        truncated,
        next_cursor: if truncated { last_key } else { None },
    })
}

// ---------------------------------------------------------------- candles

pub const CANDLE_COLUMNS: &[&str] = &["t", "o", "h", "l", "c", "v"];

/// Orders candles by time, then by position among candles sharing a
/// timestamp, so a page boundary between two such candles skips neither.
/// The cursor text is the epoch milliseconds, suffixed `#n` for the n-th
/// repeat of a timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CandleKey {
    datetime: i64,
    repeat: u32,
}

impl Display for CandleKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.repeat {
            0 => write!(f, "{}", self.datetime),
            n => write!(f, "{}#{n}", self.datetime),
        }
    }
}

impl FromStr for CandleKey {
    type Err = ();
    fn from_str(text: &str) -> Result<Self, ()> {
        let (datetime, repeat) = text.split_once('#').unwrap_or((text, "0"));
        Ok(Self {
            datetime: datetime.parse().map_err(|_| ())?,
            repeat: repeat.parse().map_err(|_| ())?,
        })
    }
}

/// `[t, o, h, l, c, v]` rows in time order, with `t` in epoch
/// milliseconds as Schwab sends it.
pub fn candles(history: PriceHistoryResponse) -> (Map<String, Value>, Vec<Row<CandleKey>>) {
    let mut seen: std::collections::HashMap<i64, u32> = std::collections::HashMap::new();
    let rows = history
        .candles
        .into_iter()
        .map(|c| {
            let repeat = seen.entry(c.datetime).or_insert(0);
            let key = CandleKey {
                datetime: c.datetime,
                repeat: *repeat,
            };
            *repeat += 1;
            Row {
                key,
                value: json!([c.datetime, c.open, c.high, c.low, c.close, c.volume]),
            }
        })
        .collect();
    let mut header = Map::new();
    header.insert("symbol".into(), json!(history.symbol));
    header.insert("empty".into(), json!(history.empty));
    if let Some(close) = history.previous_close {
        header.insert("previousClose".into(), json!(close));
    }
    if let Some(date) = history.previous_close_date {
        header.insert("previousCloseDate".into(), json!(date));
    }
    (header, rows)
}

// ------------------------------------------------------- option contracts

pub const CONTRACT_COLUMNS: &[&str] = &[
    "symbol",
    "putCall",
    "expiration",
    "strike",
    "bid",
    "ask",
    "mark",
    "last",
    "volume",
    "openInterest",
    "delta",
    "gamma",
    "theta",
    "vega",
    "iv",
    "itm",
    "dte",
];

/// Orders contracts by expiration, strike, calls before puts, then contract
/// symbol, and survives a round trip through the cursor text
/// `2025-01-17|150.000|C|AAPL  250117C00150000`. The symbol breaks ties:
/// after a corporate action a chain can list an adjusted contract beside
/// the standard one at the same expiration, strike and side, and without it
/// a page boundary between the two would skip the second.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContractKey {
    expiration: String,
    /// Strike in thousandths, so ordering is numeric.
    strike_milli: i64,
    put_call: u8,
    symbol: String,
}

impl ContractKey {
    fn new(expiration: &str, contract: &OptionContract) -> Self {
        Self {
            expiration: expiration.to_owned(),
            strike_milli: (contract.strike_price * 1000.0).round() as i64,
            put_call: u8::from(!contract.put_call.eq_ignore_ascii_case("CALL")),
            symbol: contract.symbol.clone(),
        }
    }
}

impl Display for ContractKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}|{}.{:03}|{}|{}",
            self.expiration,
            self.strike_milli / 1000,
            self.strike_milli % 1000,
            if self.put_call == 0 { 'C' } else { 'P' },
            self.symbol
        )
    }
}

impl FromStr for ContractKey {
    type Err = ();
    fn from_str(text: &str) -> Result<Self, ()> {
        let mut parts = text.splitn(4, '|');
        let expiration = parts.next().filter(|e| !e.is_empty()).ok_or(())?;
        let strike: f64 = parts.next().and_then(|s| s.parse().ok()).ok_or(())?;
        let put_call = match parts.next() {
            Some("C") => 0,
            Some("P") => 1,
            _ => return Err(()),
        };
        let symbol = parts.next().ok_or(())?;
        Ok(Self {
            expiration: expiration.to_owned(),
            strike_milli: (strike * 1000.0).round() as i64,
            put_call,
            symbol: symbol.to_owned(),
        })
    }
}

/// Only single-leg chains have contracts to tabulate; spread strategies
/// come back as Schwab's `monthlyStrategyList` and need the raw tool.
pub fn check_contract_params(params: &OptionChainParams) -> Result<(), ToolFailure> {
    params.validate().map_err(ToolFailure::from)?;
    match params.strategy {
        None | Some(OptionStrategy::Single) | Some(OptionStrategy::Analytical) => Ok(()),
        Some(_) => Err(ToolFailure::invalid_arguments(
            "get_option_contracts tabulates single-leg chains; use get_option_chain for \
             spread strategies",
        )),
    }
}

/// One row per contract from both expiration maps.
pub fn option_contracts(chain: OptionChainResponse) -> (Map<String, Value>, Vec<Row<ContractKey>>) {
    let mut rows = Vec::new();
    for map in [chain.call_exp_date_map, chain.put_exp_date_map]
        .into_iter()
        .flatten()
    {
        for (expiration_key, strikes) in map {
            // Keys are `2025-01-17:3` (date, days to expiration).
            let expiration = expiration_key
                .split(':')
                .next()
                .unwrap_or(&expiration_key)
                .to_owned();
            for contract in strikes.into_values().flatten() {
                rows.push(Row {
                    key: ContractKey::new(&expiration, &contract),
                    value: json!([
                        contract.symbol,
                        contract.put_call,
                        expiration,
                        contract.strike_price,
                        contract.bid,
                        contract.ask,
                        contract.mark,
                        contract.last,
                        contract.total_volume,
                        contract.open_interest,
                        contract.greeks.delta,
                        contract.greeks.gamma,
                        contract.greeks.theta,
                        contract.greeks.vega,
                        contract.volatility,
                        contract.is_in_the_money,
                        contract.days_to_expiration,
                    ]),
                });
            }
        }
    }
    let mut header = Map::new();
    header.insert("symbol".into(), json!(chain.symbol));
    if let Some(status) = chain.status {
        header.insert("status".into(), json!(status));
    }
    if let Some(delayed) = chain.is_delayed {
        header.insert("isDelayed".into(), json!(delayed));
    }
    if let Some(u) = chain.underlying {
        header.insert(
            "underlying".into(),
            json!({
                "last": u.last, "bid": u.bid, "ask": u.ask, "mark": u.mark,
                "quoteTime": u.quote_time,
            }),
        );
    }
    (header, rows)
}

// -------------------------------------------------------------- positions

pub const POSITION_COLUMNS: &[&str] = &[
    "symbol",
    "assetType",
    "longQuantity",
    "shortQuantity",
    "averagePrice",
    "marketValue",
    "currentDayProfitLoss",
    "longOpenProfitLoss",
    "shortOpenProfitLoss",
];

/// Orders positions by symbol; the ordinal keeps two entries in the same
/// symbol distinct across a page boundary. Cursor text is `AAPL#0`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PositionKey {
    symbol: String,
    ordinal: u32,
}

impl Display for PositionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.symbol, self.ordinal)
    }
}

impl FromStr for PositionKey {
    type Err = ();
    /// The symbol may be empty: a position without an instrument sorts first
    /// under `#0`.
    fn from_str(text: &str) -> Result<Self, ()> {
        let (symbol, ordinal) = text.rsplit_once('#').ok_or(())?;
        Ok(Self {
            symbol: symbol.to_owned(),
            ordinal: ordinal.parse().map_err(|_| ())?,
        })
    }
}

/// One row per position; the account number is masked by redaction.
pub fn positions(account: Account) -> (Map<String, Value>, Vec<Row<PositionKey>>) {
    let account = account.securities_account;
    let rows = position_rows(account.positions.unwrap_or_default());
    let mut header = Map::new();
    header.insert("accountNumber".into(), json!(account.account_number));
    if let Some(kind) = account.account_type {
        header.insert("type".into(), json!(kind));
    }
    (header, rows)
}

/// The rows of [`positions`], for callers that keep the account.
pub fn position_rows(mut positions: Vec<Position>) -> Vec<Row<PositionKey>> {
    positions.sort_by(|a, b| symbol_of(a).cmp(symbol_of(b)));
    let mut rows: Vec<Row<PositionKey>> = Vec::with_capacity(positions.len());
    for position in positions {
        let symbol = symbol_of(&position).to_owned();
        let ordinal = rows
            .last()
            .filter(|last| last.key.symbol == symbol)
            .map_or(0, |last| last.key.ordinal + 1);
        let asset_type = position.instrument.as_ref().map(|i| i.asset_type);
        rows.push(Row {
            key: PositionKey {
                symbol: symbol.clone(),
                ordinal,
            },
            value: json!([
                symbol,
                asset_type,
                position.long_quantity,
                position.short_quantity,
                position.average_price,
                position.market_value,
                position.current_day_profit_loss,
                position.long_open_profit_loss,
                position.short_open_profit_loss,
            ]),
        });
    }
    rows
}

fn symbol_of(position: &Position) -> &str {
    position
        .instrument
        .as_ref()
        .map_or("", |i| i.symbol.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(n: i64) -> Vec<Row<i64>> {
        (1..=n)
            .map(|i| Row {
                key: i,
                value: json!([i, "row"]),
            })
            .collect()
    }

    #[test]
    fn pages_stop_at_the_budget_or_the_limit_and_resume_after_the_cursor() {
        // Each row encodes to 9 bytes plus a separator.
        let all = page(numbered(5), &["i", "x"], Map::new(), None, None, 1 << 20).unwrap();
        assert_eq!((all.count, all.total, all.truncated), (5, 5, false));
        assert!(all.next_cursor.is_none());

        let two = page(numbered(5), &["i", "x"], Map::new(), None, Some(2), 1 << 20).unwrap();
        assert_eq!(two.rows, vec![json!([1, "row"]), json!([2, "row"])]);
        assert_eq!(
            (two.truncated, two.next_cursor.as_deref()),
            (true, Some("2"))
        );

        let rest = page(
            numbered(5),
            &["i", "x"],
            Map::new(),
            Some("2"),
            None,
            1 << 20,
        )
        .unwrap();
        assert_eq!(rest.rows.len(), 3);
        assert_eq!(rest.rows[0], json!([3, "row"]));
        assert_eq!(rest.total, 5);
        assert!(!rest.truncated);

        // Three rows fit in 30 bytes; a budget below one row still yields one.
        let budgeted = page(numbered(5), &["i", "x"], Map::new(), None, None, 30).unwrap();
        assert_eq!((budgeted.count, budgeted.truncated), (3, true));
        let tiny = page(numbered(5), &["i", "x"], Map::new(), None, None, 1).unwrap();
        assert_eq!((tiny.count, tiny.next_cursor.as_deref()), (1, Some("1")));

        // Rows arrive unsorted; the cursor beyond the end yields nothing.
        let mut shuffled = numbered(3);
        shuffled.reverse();
        let sorted = page(shuffled, &["i", "x"], Map::new(), None, None, 1 << 20).unwrap();
        assert_eq!(sorted.rows[0], json!([1, "row"]));
        let past = page(
            numbered(3),
            &["i", "x"],
            Map::new(),
            Some("9"),
            None,
            1 << 20,
        )
        .unwrap();
        assert_eq!((past.count, past.truncated), (0, false));

        let bad = page(
            numbered(3),
            &["i", "x"],
            Map::new(),
            Some("x"),
            None,
            1 << 20,
        );
        assert_eq!(
            bad.unwrap_err().code,
            crate::contract::FailureCode::InvalidArguments
        );
        let zero = page(numbered(3), &["i", "x"], Map::new(), None, Some(0), 1 << 20);
        assert!(zero.is_err());
    }

    /// An adjusted contract shares expiration, strike and side with the
    /// standard one; paging one row at a time must still return both.
    #[test]
    fn contracts_sharing_a_strike_survive_page_boundaries() {
        let chain: OptionChainResponse = serde_json::from_value(json!({
            "symbol": "AAPL",
            "callExpDateMap": {"2025-01-17:3": {"150.0": [
                {"putCall": "CALL", "symbol": "AAPL  250117C00150000", "strikePrice": 150.0,
                 "expirationDate": "2025-01-17T00:00:00.000+00:00"},
                {"putCall": "CALL", "symbol": "AAPL1 250117C00150000", "strikePrice": 150.0,
                 "expirationDate": "2025-01-17T00:00:00.000+00:00"}
            ]}}
        }))
        .unwrap();
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let (header, rows) = option_contracts(chain.clone());
            let page = page(
                rows,
                CONTRACT_COLUMNS,
                header,
                cursor.as_deref(),
                Some(1),
                1 << 20,
            )
            .unwrap();
            assert_eq!(page.total, 2);
            seen.extend(page.rows.iter().map(|r| r[0].as_str().unwrap().to_owned()));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(seen, ["AAPL  250117C00150000", "AAPL1 250117C00150000"]);
    }

    /// Two candles at one timestamp are both returned across a boundary.
    #[test]
    fn candles_sharing_a_timestamp_survive_page_boundaries() {
        let history: PriceHistoryResponse = serde_json::from_value(json!({
            "symbol": "AAPL", "empty": false, "candles": [
                {"datetime": 1, "open": 1.0, "high": 1.0, "low": 1.0, "close": 1.0, "volume": 1},
                {"datetime": 2, "open": 2.0, "high": 2.0, "low": 2.0, "close": 2.0, "volume": 2},
                {"datetime": 2, "open": 3.0, "high": 3.0, "low": 3.0, "close": 3.0, "volume": 3}
            ]
        }))
        .unwrap();
        let mut opens = Vec::new();
        let mut cursor = None;
        loop {
            let (header, rows) = candles(history.clone());
            let page = page(
                rows,
                CANDLE_COLUMNS,
                header,
                cursor.as_deref(),
                Some(1),
                1 << 20,
            )
            .unwrap();
            opens.extend(page.rows.iter().map(|r| r[1].as_f64().unwrap()));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(opens, [1.0, 2.0, 3.0]);
        assert_eq!(CandleKey::from_str("2#1").unwrap().to_string(), "2#1");
        assert_eq!(CandleKey::from_str("2").unwrap().to_string(), "2");
    }

    #[test]
    fn contract_keys_order_and_round_trip() {
        let chain: OptionChainResponse = serde_json::from_value(json!({
            "symbol": "AAPL", "status": "SUCCESS", "isDelayed": false,
            "underlying": {"last": 150.1, "bid": 150.0, "ask": 150.2, "mark": 150.1, "quoteTime": 5},
            "callExpDateMap": {
                "2025-01-24:10": {"150.0": [{"putCall": "CALL", "symbol": "AAPL  250124C00150000",
                    "strikePrice": 150.0, "expirationDate": "2025-01-24T00:00:00.000+00:00",
                    "bid": 2.0, "ask": 2.1, "delta": 0.5, "daysToExpiration": 10}]},
                "2025-01-17:3": {
                    "155.0": [{"putCall": "CALL", "symbol": "AAPL  250117C00155000",
                        "strikePrice": 155.0, "expirationDate": "2025-01-17T00:00:00.000+00:00"}],
                    "150.0": [{"putCall": "CALL", "symbol": "AAPL  250117C00150000",
                        "strikePrice": 150.0, "expirationDate": "2025-01-17T00:00:00.000+00:00"}]}
            },
            "putExpDateMap": {
                "2025-01-17:3": {"150.0": [{"putCall": "PUT", "symbol": "AAPL  250117P00150000",
                    "strikePrice": 150.0, "expirationDate": "2025-01-17T00:00:00.000+00:00",
                    "inTheMoney": false}]}
            }
        }))
        .unwrap();
        let (header, rows) = option_contracts(chain);
        assert_eq!(header["underlying"]["last"], 150.1);
        let page = page(rows, CONTRACT_COLUMNS, header, None, None, 1 << 20).unwrap();
        let symbols: Vec<&str> = page.rows.iter().map(|r| r[0].as_str().unwrap()).collect();
        assert_eq!(
            symbols,
            [
                "AAPL  250117C00150000",
                "AAPL  250117P00150000",
                "AAPL  250117C00155000",
                "AAPL  250124C00150000",
            ]
        );
        assert_eq!(page.rows[0][2], "2025-01-17");
        assert_eq!(page.rows[3][10], 0.5);
        assert_eq!(page.rows[1][15], false);
        assert_eq!(page.columns.len(), page.rows[0].as_array().unwrap().len());

        let key = ContractKey::from_str("2025-01-17|150.000|P|AAPL  250117P00150000").unwrap();
        assert_eq!(
            key.to_string(),
            "2025-01-17|150.000|P|AAPL  250117P00150000"
        );
        assert_eq!(
            ContractKey::from_str("2025-01-17|152.5|C|X")
                .unwrap()
                .to_string(),
            "2025-01-17|152.500|C|X"
        );
        assert!(ContractKey::from_str("2025-01-17|abc|C|X").is_err());
        assert!(ContractKey::from_str("2025-01-17|150|X|X").is_err());
        assert!(ContractKey::from_str("|150|C|X").is_err());
        // The symbol is part of the key: a cursor without one was not issued here.
        assert!(ContractKey::from_str("2025-01-17|150|C").is_err());

        let spread = OptionChainParams {
            strategy: Some(OptionStrategy::Vertical),
            ..OptionChainParams::new("AAPL")
        };
        assert!(check_contract_params(&spread).is_err());
        assert!(check_contract_params(&OptionChainParams::new("AAPL")).is_ok());
    }

    #[test]
    fn positions_are_tabulated_by_symbol() {
        let account = || -> Account {
            serde_json::from_value(json!({"securitiesAccount": {
                "accountNumber": "123456789", "type": "MARGIN",
                "positions": [
                    {"longQuantity": 10.0, "shortQuantity": 0.0, "averagePrice": 100.0,
                     "marketValue": 1500.0, "currentDayProfitLoss": 5.0,
                     "longOpenProfitLoss": 500.0,
                     "instrument": {"assetType": "EQUITY", "symbol": "MSFT"}},
                    {"longQuantity": 1.0, "shortQuantity": 0.0,
                     "instrument": {"assetType": "OPTION", "symbol": "AAPL  250117C00150000",
                         "putCall": "CALL", "underlyingSymbol": "AAPL"}},
                    {"longQuantity": 2.0, "shortQuantity": 0.0,
                     "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}},
                    {"longQuantity": 3.0, "shortQuantity": 0.0,
                     "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}}
                ]
            }}))
            .unwrap()
        };
        let (header, rows) = positions(account());
        assert_eq!(header["accountNumber"], "123456789");
        assert_eq!(header["type"], "MARGIN");
        let first = page(rows, POSITION_COLUMNS, header, None, Some(2), 1 << 20).unwrap();
        assert_eq!(first.rows[0][0], "AAPL");
        assert_eq!(first.rows[1][0], "AAPL");
        assert_eq!(first.next_cursor.as_deref(), Some("AAPL#1"));

        let (header, rows) = positions(account());
        let rest = page(
            rows,
            POSITION_COLUMNS,
            header,
            Some("AAPL#1"),
            None,
            1 << 20,
        )
        .unwrap();
        let symbols: Vec<&str> = rest.rows.iter().map(|r| r[0].as_str().unwrap()).collect();
        assert_eq!(symbols, ["AAPL  250117C00150000", "MSFT"]);
        assert_eq!(rest.rows[1][1], "EQUITY");
        assert_eq!(rest.rows[1][6], 5.0);
        assert!(PositionKey::from_str("AAPL").is_err());

        // A position without an instrument sorts first; paging goes past it.
        let bare = || -> Account {
            serde_json::from_value(json!({"securitiesAccount": {
                "accountNumber": "123456789",
                "positions": [
                    {"longQuantity": 2.0, "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}},
                    {"longQuantity": 1.0}
                ]
            }}))
            .unwrap()
        };
        let (header, rows) = positions(bare());
        let first = page(rows, POSITION_COLUMNS, header, None, Some(1), 1 << 20).unwrap();
        assert_eq!(first.next_cursor.as_deref(), Some("#0"));
        let (header, rows) = positions(bare());
        let rest = page(rows, POSITION_COLUMNS, header, Some("#0"), None, 1 << 20).unwrap();
        assert_eq!(rest.rows.len(), 1);
        assert_eq!(rest.rows[0][0], "AAPL");
    }

    #[test]
    fn candles_are_six_column_rows() {
        let history: PriceHistoryResponse = serde_json::from_value(json!({
            "symbol": "AAPL", "empty": false, "previousClose": 149.5, "previousCloseDate": 1,
            "candles": [
                {"datetime": 2000, "open": 1.0, "high": 2.0, "low": 0.5, "close": 1.5, "volume": 10},
                {"datetime": 1000, "open": 1.0, "high": 2.0, "low": 0.5, "close": 1.5, "volume": 10}
            ]
        }))
        .unwrap();
        let (header, rows) = candles(history);
        assert_eq!(header["previousClose"], 149.5);
        let page = page(rows, CANDLE_COLUMNS, header, None, None, 1 << 20).unwrap();
        assert_eq!(page.rows[0], json!([1000, 1.0, 2.0, 0.5, 1.5, 10]));
        let value = page.into_payload().unwrap();
        assert_eq!(value["symbol"], "AAPL");
        assert_eq!(value["columns"], json!(["t", "o", "h", "l", "c", "v"]));
        assert_eq!(value["count"], 2);
        assert!(value.get("nextCursor").is_none());
    }

    #[test]
    fn budget_is_parsed_leniently_and_clamped() {
        assert_eq!(parse_budget(None), DEFAULT_BUDGET_BYTES);
        assert_eq!(parse_budget(Some(" 100000 ")), 100_000);
        assert_eq!(parse_budget(Some("lots")), DEFAULT_BUDGET_BYTES);
        assert_eq!(parse_budget(Some("1")), MIN_BUDGET_BYTES);
        assert_eq!(
            parse_budget(Some("99999999999")),
            turbomcp_core::MAX_MESSAGE_SIZE
        );
    }
}
