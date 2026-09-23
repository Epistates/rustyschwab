//! Synthetic best bid and offer from OHLCV candles, so the paper venue can be
//! driven over price history.
//!
//! This is a declared modeling assumption, not broker conformance. A candle
//! says where a bar opened, ranged and closed; it does not say what was bid or
//! offered, in what size, or in which order the extremes printed. The model
//! here fixes each of those by rule, and every session it emits carries the
//! model's name in its id, so a run built on synthetic quotes is never read as
//! one fed by observed ones.
//!
//! The rules, all of them assumptions:
//!
//! * A candle becomes four marks a quarter of a candle apart: the open, then
//!   the low and the high (low first when the bar closed at or above its open,
//!   high first when it closed below), then the close.
//! * Around each mark the bid and ask sit half the configured spread below
//!   and above it, the bid rounded down to the cent and the ask rounded up.
//! * Each mark offers, on each side, the configured share of the candle's
//!   volume divided across its four marks, in whole shares. A candle without
//!   volume quotes prices nobody trades against.
//! * A candle belongs to the New York calendar date it opened on, and each
//!   date is one venue session across every symbol: it opens at the date's
//!   first candle and closes when its last candle ends, or when the next
//!   date's first candle opens if that comes first.
//!
//! The venue's own rules then apply unchanged: an order accepted at one mark
//! fills at a later mark's synthetic bid or ask (plus configured slippage),
//! never at its own limit price, and only up to the liquidity that mark
//! offers.
use super::{
    PaperEvent, PaperEventKind,
    engine::{MAX_AT_MS, MAX_QUOTE_SIZE, positive_bounded, symbol_valid},
};
use crate::{Error, Result};
use chrono::{NaiveDate, TimeZone};
use chrono_tz::America::New_York;
use rust_decimal::{Decimal, RoundingStrategy};
use schwab_types::market_data::{Candle, PriceHistoryResponse};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

/// Name of the quote model, carried in every session id the model emits.
pub const SYNTHETIC_MODEL: &str = "candle-bbo-v1";
const MARKS_PER_CANDLE: i64 = 4;
const MAX_CANDLE_MS: i64 = 31 * 86_400_000;

/// How candles become quotes. Every field is a modeling choice; see the
/// module documentation for what each one asserts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyntheticBbo {
    /// Width of each candle in milliseconds; the four marks fall a quarter of
    /// it apart and a session closes this long after its last candle opens.
    pub candle_ms: i64,
    /// Full bid-ask spread as basis points of the mark, 0 to 1000.
    pub spread_bps: Decimal,
    /// Share of a candle's volume offered on each side across its marks, as
    /// basis points, 0 to 10000.
    pub participation_bps: Decimal,
}

struct Item<'a> {
    at_ms: i64,
    /// Order at one instant: the old session closes, the new one opens, then
    /// quotes print in symbol order.
    rank: u8,
    symbol: &'a str,
    kind: PaperEventKind,
}

impl SyntheticBbo {
    pub fn validate(&self) -> Result<()> {
        let deny = |what: &str| Error::InvalidParameter(format!("Synthetic BBO {what}"));
        if !(MARKS_PER_CANDLE..=MAX_CANDLE_MS).contains(&self.candle_ms) {
            return Err(deny("candle_ms must be between 4 ms and 31 days"));
        }
        if self.spread_bps < Decimal::ZERO || self.spread_bps > Decimal::from(1000) {
            return Err(deny("spread_bps must be between 0 and 1000"));
        }
        if self.participation_bps < Decimal::ZERO || self.participation_bps > Decimal::from(10_000)
        {
            return Err(deny("participation_bps must be between 0 and 10000"));
        }
        Ok(())
    }

    /// One time-ordered event stream for all the histories, numbered
    /// contiguously from `first_sequence`, ready for a replay-mode venue.
    ///
    /// Each history must carry a venue-valid symbol, no symbol may appear
    /// twice, and each symbol's candles must be in time order at least one
    /// candle apart. A mark whose synthetic bid would fall below the venue's
    /// one-unit minimum, or whose size would exceed its bound, is an error
    /// rather than a silently clipped quote.
    pub fn events(
        &self,
        histories: &[PriceHistoryResponse],
        first_sequence: u64,
    ) -> Result<Vec<PaperEvent>> {
        self.validate()?;
        if first_sequence == 0 {
            return Err(Error::InvalidParameter(
                "Synthetic BBO first_sequence must be at least 1".into(),
            ));
        }
        let mut items = Vec::new();
        let mut sessions: BTreeMap<NaiveDate, (i64, i64)> = BTreeMap::new();
        let mut symbols = HashSet::new();
        for history in histories {
            let symbol = history.symbol.as_str();
            if !symbol_valid(symbol) || !symbols.insert(symbol) {
                return Err(Error::InvalidParameter(format!(
                    "Synthetic BBO history symbol {symbol:?} is invalid or repeated"
                )));
            }
            let mut previous_open = None;
            for candle in &history.candles {
                self.check(symbol, candle, previous_open)?;
                previous_open = Some(candle.datetime);
                let date = New_York
                    .timestamp_millis_opt(candle.datetime)
                    .single()
                    .ok_or_else(|| self.reject(symbol, candle, "has no New York date"))?
                    .date_naive();
                let end = candle.datetime + self.candle_ms;
                sessions
                    .entry(date)
                    .and_modify(|(open, close)| {
                        *open = (*open).min(candle.datetime);
                        *close = (*close).max(end);
                    })
                    .or_insert((candle.datetime, end));
                let size = self.size(symbol, candle)?;
                for (k, mark) in path(candle).into_iter().enumerate() {
                    let (bid, ask) = self.quote(symbol, candle, mark)?;
                    let at_ms = candle.datetime + k as i64 * (self.candle_ms / MARKS_PER_CANDLE);
                    items.push(Item {
                        at_ms,
                        rank: 2,
                        symbol,
                        kind: PaperEventKind::Quote {
                            symbol: symbol.into(),
                            source_at_ms: at_ms,
                            bid,
                            ask,
                            bid_size: size,
                            ask_size: size,
                        },
                    });
                }
            }
        }
        let mut dates = sessions.iter().peekable();
        while let Some((date, (open, close))) = dates.next() {
            let session_id = format!("{SYNTHETIC_MODEL}:{date}");
            let close = dates
                .peek()
                .map_or(*close, |(_, (next_open, _))| (*close).min(*next_open));
            items.push(Item {
                at_ms: *open,
                rank: 1,
                symbol: "",
                kind: PaperEventKind::Session {
                    session_id: session_id.clone(),
                    open: true,
                },
            });
            items.push(Item {
                at_ms: close,
                rank: 0,
                symbol: "",
                kind: PaperEventKind::Session {
                    session_id,
                    open: false,
                },
            });
        }
        items.sort_by(|a, b| (a.at_ms, a.rank, a.symbol).cmp(&(b.at_ms, b.rank, b.symbol)));
        if first_sequence
            .checked_add(items.len() as u64)
            .is_none_or(|end| end > i64::MAX as u64 + 1)
        {
            return Err(Error::InvalidParameter(
                "Synthetic BBO sequence numbers would exceed the venue's range".into(),
            ));
        }
        Ok(items
            .into_iter()
            .zip(first_sequence..)
            .map(|(item, sequence)| PaperEvent {
                sequence,
                at_ms: item.at_ms,
                kind: item.kind,
            })
            .collect())
    }

    fn check(&self, symbol: &str, candle: &Candle, previous_open: Option<i64>) -> Result<()> {
        let prices = [candle.open, candle.high, candle.low, candle.close];
        // The venue's own price bound (`positive_bounded`): anything above it
        // could never be quoted, and keeps the spread arithmetic in range.
        if prices
            .iter()
            .any(|p| !p.is_finite() || *p <= 0.0 || *p > 1e12)
            || candle.low > candle.open.min(candle.close)
            || candle.high < candle.open.max(candle.close)
        {
            return Err(self.reject(symbol, candle, "is not a well-formed OHLC bar"));
        }
        if candle.volume < 0 {
            return Err(self.reject(symbol, candle, "has negative volume"));
        }
        if candle.datetime < 0 || candle.datetime > MAX_AT_MS - self.candle_ms {
            return Err(self.reject(symbol, candle, "opens outside the venue's clock"));
        }
        if previous_open.is_some_and(|previous| candle.datetime < previous + self.candle_ms) {
            return Err(self.reject(
                symbol,
                candle,
                "is out of time order or closer than one candle to the one before it",
            ));
        }
        Ok(())
    }

    fn size(&self, symbol: &str, candle: &Candle) -> Result<Decimal> {
        let size = (Decimal::from(candle.volume) * self.participation_bps
            / Decimal::from(10_000 * MARKS_PER_CANDLE))
        .floor();
        if size > Decimal::from(MAX_QUOTE_SIZE) {
            return Err(self.reject(
                symbol,
                candle,
                "offers more than the venue's size bound; lower participation_bps",
            ));
        }
        Ok(size)
    }

    fn quote(&self, symbol: &str, candle: &Candle, mark: f64) -> Result<(Decimal, Decimal)> {
        let mid: Decimal = mark
            .to_string()
            .parse()
            .map_err(|_| self.reject(symbol, candle, "has a price the venue cannot represent"))?;
        let unrepresentable =
            || self.reject(symbol, candle, "has a price the venue cannot represent");
        let half = mid
            .checked_mul(self.spread_bps)
            .and_then(|v| v.checked_div(Decimal::from(20_000)))
            .ok_or_else(unrepresentable)?;
        let bid = mid
            .checked_sub(half)
            .ok_or_else(unrepresentable)?
            .round_dp_with_strategy(2, RoundingStrategy::ToNegativeInfinity);
        let ask = mid
            .checked_add(half)
            .ok_or_else(unrepresentable)?
            .round_dp_with_strategy(2, RoundingStrategy::ToPositiveInfinity);
        if bid < Decimal::ONE {
            return Err(self.reject(
                symbol,
                candle,
                "would bid below the venue's one-unit minimum",
            ));
        }
        if !positive_bounded(ask) {
            return Err(self.reject(symbol, candle, "would ask above the venue's price bound"));
        }
        Ok((bid, ask))
    }

    fn reject(&self, symbol: &str, candle: &Candle, what: &str) -> Error {
        Error::InvalidParameter(format!(
            "Synthetic BBO candle for {symbol} at {} {what}",
            candle.datetime
        ))
    }
}

/// The order the four marks print in: the low prints first on a bar that
/// closed at or above its open, the high first on one that closed below.
fn path(candle: &Candle) -> [f64; 4] {
    if candle.close >= candle.open {
        [candle.open, candle.low, candle.high, candle.close]
    } else {
        [candle.open, candle.high, candle.low, candle.close]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn d(v: &str) -> Decimal {
        v.parse().unwrap()
    }
    fn candle(datetime: i64, open: f64, high: f64, low: f64, close: f64, volume: i64) -> Candle {
        Candle {
            open,
            high,
            low,
            close,
            volume,
            datetime,
            extra: HashMap::new(),
        }
    }
    fn history(symbol: &str, candles: Vec<Candle>) -> PriceHistoryResponse {
        PriceHistoryResponse {
            symbol: symbol.into(),
            empty: candles.is_empty(),
            previous_close: None,
            previous_close_date: None,
            candles,
            extra: HashMap::new(),
        }
    }
    fn model() -> SyntheticBbo {
        SyntheticBbo {
            candle_ms: 60_000,
            spread_bps: d("10"),
            participation_bps: d("1000"),
        }
    }
    fn quotes(events: &[PaperEvent]) -> Vec<(i64, &str, Decimal, Decimal, Decimal)> {
        events
            .iter()
            .filter_map(|e| match &e.kind {
                PaperEventKind::Quote {
                    symbol,
                    source_at_ms,
                    bid,
                    ask,
                    bid_size,
                    ask_size,
                } => {
                    assert_eq!(*source_at_ms, e.at_ms);
                    assert_eq!(bid_size, ask_size);
                    Some((e.at_ms, symbol.as_str(), *bid, *ask, *bid_size))
                }
                _ => None,
            })
            .collect()
    }
    fn sessions(events: &[PaperEvent]) -> Vec<(i64, &str, bool)> {
        events
            .iter()
            .filter_map(|e| match &e.kind {
                PaperEventKind::Session { session_id, open } => {
                    Some((e.at_ms, session_id.as_str(), *open))
                }
                _ => None,
            })
            .collect()
    }
    // 2024-05-17 09:30:00 America/New_York, EDT.
    const OPEN: i64 = 1_715_952_600_000;

    #[test]
    fn marks_follow_the_bar_path_and_round_the_spread_outward() {
        let events = model()
            .events(
                &[history(
                    "ABC",
                    vec![
                        candle(OPEN, 100.0, 101.0, 99.0, 100.5, 4000),
                        candle(OPEN + 60_000, 100.5, 102.0, 100.0, 100.25, 0),
                    ],
                )],
                7,
            )
            .unwrap();
        assert_eq!(
            events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            (7..17).collect::<Vec<_>>()
        );
        let quotes = quotes(&events);
        // Up bar: open, low, high, close. Down bar: open, high, low, close.
        assert_eq!(
            quotes
                .iter()
                .map(|(at, _, bid, ask, size)| (at - OPEN, *bid, *ask, *size))
                .collect::<Vec<_>>(),
            vec![
                (0, d("99.95"), d("100.05"), d("100")),
                (15_000, d("98.95"), d("99.05"), d("100")),
                (30_000, d("100.94"), d("101.06"), d("100")),
                (45_000, d("100.44"), d("100.56"), d("100")),
                (60_000, d("100.44"), d("100.56"), d("0")),
                (75_000, d("101.94"), d("102.06"), d("0")),
                (90_000, d("99.95"), d("100.05"), d("0")),
                (105_000, d("100.19"), d("100.31"), d("0")),
            ]
        );
        assert_eq!(
            sessions(&events),
            vec![
                (OPEN, "candle-bbo-v1:2024-05-17", true),
                (OPEN + 120_000, "candle-bbo-v1:2024-05-17", false),
            ]
        );
        assert!(
            events
                .windows(2)
                .all(|w| w[0].at_ms <= w[1].at_ms && w[0].sequence + 1 == w[1].sequence)
        );
    }

    #[test]
    fn zero_spread_meets_at_the_cent_and_participation_floors_shares() {
        let model = SyntheticBbo {
            candle_ms: 1000,
            spread_bps: Decimal::ZERO,
            participation_bps: d("2500"),
        };
        let events = model
            .events(
                &[history(
                    "ABC",
                    vec![candle(OPEN, 10.001, 10.001, 10.001, 10.001, 15)],
                )],
                1,
            )
            .unwrap();
        // Quarter of 15 shares across four marks is 0.9375: no whole share.
        for (at, _, bid, ask, size) in quotes(&events) {
            assert_eq!((bid, ask, size), (d("10.00"), d("10.01"), Decimal::ZERO));
            assert!((OPEN..OPEN + 1000).contains(&at));
        }
    }

    #[test]
    fn sessions_are_new_york_dates_shared_across_symbols() {
        // 2024-01-05 15:59 EST, then 2024-01-08 04:00 EST (pre-market), which
        // is 09:00 UTC: a UTC date would be right, a fixed EDT offset wrong.
        let friday_last = 1_704_488_340_000;
        let monday_first = 1_704_704_400_000;
        let events = model()
            .events(
                &[
                    history(
                        "ABC",
                        vec![
                            candle(friday_last, 10.0, 10.0, 10.0, 10.0, 400),
                            candle(monday_first, 11.0, 11.0, 11.0, 11.0, 400),
                        ],
                    ),
                    history(
                        "AAA",
                        vec![
                            candle(friday_last - 60_000, 20.0, 20.0, 20.0, 20.0, 400),
                            candle(friday_last, 20.0, 20.0, 20.0, 20.0, 400),
                        ],
                    ),
                ],
                1,
            )
            .unwrap();
        assert_eq!(
            sessions(&events),
            vec![
                (friday_last - 60_000, "candle-bbo-v1:2024-01-05", true),
                (friday_last + 60_000, "candle-bbo-v1:2024-01-05", false),
                (monday_first, "candle-bbo-v1:2024-01-08", true),
                (monday_first + 60_000, "candle-bbo-v1:2024-01-08", false),
            ]
        );
        // The Friday session opens before AAA's first quote, then symbols
        // print in name order at a shared instant.
        let first: Vec<_> = events.iter().take(2).map(|e| &e.kind).collect();
        assert!(matches!(
            first[0],
            PaperEventKind::Session { open: true, .. }
        ));
        assert!(matches!(first[1], PaperEventKind::Quote { symbol, .. } if symbol == "AAA"));
        let at_friday_last: Vec<_> = quotes(&events)
            .into_iter()
            .filter(|(at, ..)| *at == friday_last)
            .map(|(_, symbol, ..)| symbol)
            .collect();
        assert_eq!(at_friday_last, vec!["AAA", "ABC"]);
    }

    #[test]
    fn daily_candles_across_the_spring_forward_keep_their_new_york_dates() {
        // Daily candles at New York midnight: Friday 2024-03-08 (EST) and
        // Monday 2024-03-11 (EDT). A candle is a fixed 86,400,000 ms wide,
        // and the Friday session closes when its candle ends, before Monday.
        let model = SyntheticBbo {
            candle_ms: 86_400_000,
            ..model()
        };
        let friday = 1_709_874_000_000;
        let monday = 1_710_129_600_000;
        let events = model
            .events(
                &[history(
                    "ABC",
                    vec![
                        candle(friday, 10.0, 10.0, 10.0, 10.0, 0),
                        candle(monday, 10.0, 10.0, 10.0, 10.0, 0),
                    ],
                )],
                1,
            )
            .unwrap();
        assert_eq!(
            sessions(&events),
            vec![
                (friday, "candle-bbo-v1:2024-03-08", true),
                (friday + 86_400_000, "candle-bbo-v1:2024-03-08", false),
                (monday, "candle-bbo-v1:2024-03-11", true),
                (monday + 86_400_000, "candle-bbo-v1:2024-03-11", false),
            ]
        );
    }

    #[test]
    fn a_session_closes_when_the_next_date_opens_if_a_candle_runs_past_it() {
        // ABC's last Friday candle is a full day wide, so it would end after
        // AAA opens Saturday's date; the venue closes Friday at AAA's open.
        let model = SyntheticBbo {
            candle_ms: 86_400_000,
            ..model()
        };
        let friday_noon = 1_709_917_200_000;
        let saturday = 1_709_960_400_000;
        let events = model
            .events(
                &[
                    history("ABC", vec![candle(friday_noon, 10.0, 10.0, 10.0, 10.0, 0)]),
                    history("AAA", vec![candle(saturday, 10.0, 10.0, 10.0, 10.0, 0)]),
                ],
                1,
            )
            .unwrap();
        assert_eq!(
            sessions(&events),
            vec![
                (friday_noon, "candle-bbo-v1:2024-03-08", true),
                (saturday, "candle-bbo-v1:2024-03-08", false),
                (saturday, "candle-bbo-v1:2024-03-09", true),
                (saturday + 86_400_000, "candle-bbo-v1:2024-03-09", false),
            ]
        );
        let at_saturday: Vec<_> = events
            .iter()
            .filter(|e| e.at_ms == saturday)
            .map(|e| match &e.kind {
                PaperEventKind::Session { open, .. } => {
                    if *open {
                        "open"
                    } else {
                        "close"
                    }
                }
                PaperEventKind::Quote { .. } => "quote",
                _ => unreachable!(),
            })
            .collect();
        // ABC's midnight mark lands on the boundary and prints in the new session.
        assert_eq!(at_saturday, vec!["close", "open", "quote", "quote"]);
    }

    #[test]
    fn malformed_input_is_rejected_by_name() {
        let model = model();
        let err = |histories: &[PriceHistoryResponse]| {
            model.events(histories, 1).unwrap_err().to_string()
        };
        let one = |c: Candle| vec![history("ABC", vec![c])];
        assert!(err(&one(candle(OPEN, 100.0, 99.0, 99.0, 99.5, 1))).contains("well-formed"));
        assert!(err(&one(candle(OPEN, 100.0, 100.0, 100.0, 100.0, -1))).contains("negative"));
        assert!(err(&one(candle(-1, 100.0, 100.0, 100.0, 100.0, 1))).contains("clock"));
        assert!(err(&one(candle(OPEN, 0.5, 0.5, 0.5, 0.5, 1))).contains("one-unit"));
        assert!(err(&one(candle(OPEN, 100.0, 100.0, 100.0, 100.0, i64::MAX))).contains("bound"));
        // Representable but absurd prices are refused, not an arithmetic panic.
        let wide = SyntheticBbo {
            spread_bps: d("1000"),
            ..model.clone()
        };
        let huge = candle(OPEN, 8e25, 8e25, 8e25, 8e25, 1);
        assert!(
            wide.events(&[history("ABC", vec![huge])], 1)
                .unwrap_err()
                .to_string()
                .contains("well-formed")
        );
        assert!(
            err(&[history(
                "ABC",
                vec![
                    candle(OPEN, 100.0, 100.0, 100.0, 100.0, 1),
                    candle(OPEN + 59_999, 100.0, 100.0, 100.0, 100.0, 1),
                ],
            )])
            .contains("time order")
        );
        assert!(err(&[history("abc", vec![])]).contains("invalid or repeated"));
        assert!(err(&[history("ABC", vec![]), history("ABC", vec![])]).contains("repeated"));
        assert!(
            model
                .events(&[history("ABC", vec![])], 0)
                .unwrap_err()
                .to_string()
                .contains("first_sequence")
        );
        for bad in [
            SyntheticBbo {
                candle_ms: 3,
                ..model.clone()
            },
            SyntheticBbo {
                spread_bps: d("1000.5"),
                ..model.clone()
            },
            SyntheticBbo {
                participation_bps: d("-1"),
                ..model.clone()
            },
        ] {
            assert!(bad.validate().is_err());
        }
        assert!(model.events(&[], 1).unwrap().is_empty());
    }
}
