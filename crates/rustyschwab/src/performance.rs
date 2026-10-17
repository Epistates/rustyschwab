//! Performance analytics over fills and mark-to-market equity curves.
//!
//! [`analyze`] is a pure function: it reads the fills a strategy made and,
//! optionally, the account value curve it ran through, and reports what the
//! run earned and what it cost to earn it. It performs no I/O and reads no
//! clock, so the same inputs always give the same report. Paper venue fills
//! and live account transactions feed it alike, once they carry a symbol and a
//! side.
//!
//! The accounting rules, all of them fixed by this module:
//!
//! * Fills are matched first-in first-out at the share level, long and short.
//!   A sell with no long lots open opens a short lot, a buy closes short lots
//!   before it opens a long one, and a fill that crosses through flat closes
//!   the old position and opens the new one with the remainder.
//! * Every match of one entry lot against one closing fill is one
//!   [`RoundTrip`], so a partial fill, a scale-in and a scale-out are each
//!   their own round trip. Prices on a round trip are therefore the exact
//!   fill prices, not averages.
//! * A fill's fee is spread over the quantities it opens and closes in
//!   proportion to quantity, the last piece taking the exact remainder, so
//!   the pieces always add back to the fee. A round trip carries the entry
//!   fee portion of its lot and the exit fee portion of its closing fill.
//! * A round trip wins when its P&L after fees is positive, loses when it is
//!   negative, and is breakeven at zero.
//! * Profit and loss are scaled by each fill's `multiplier` (100 for equity
//!   options). A symbol must use one multiplier throughout.
//! * Open lots are valued at the supplied mark for their symbol, or at the
//!   symbol's last fill price when no mark is given. That is an assumption,
//!   not an observation; pass `marks` for a real valuation.
//!
//! Ratios are computed from the equity curve resampled to one value per New
//! York calendar date (the last point of the date). Drawdown uses the curve at
//! full resolution. When no curve is supplied, the curve is built from the
//! fills: one point per fill at the realized P&L less fees paid so far, with
//! the final point carrying the open lots' valuation. The report says which
//! curve it used in [`PerformanceReport::equity_source`].
//!
//! Money is [`Decimal`] throughout and serializes as a decimal string, as it
//! does on the paper venue. Ratios and fractions are `f64`.

use crate::types::trading::OrderInstruction;
use crate::{Error, Result};
use chrono::{DateTime, NaiveDate};
use chrono_tz::America::New_York;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::de::{self, IntoDeserializer};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, VecDeque};

/// Trading periods per year when none is given.
pub const DEFAULT_PERIODS_PER_YEAR: u32 = 252;

/// A deviation below this is zero: the series is constant up to rounding.
const MIN_DEVIATION: f64 = 1e-12;

/// Which way a fill moves the position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Side {
    Buy,
    Sell,
}

impl<'de> Deserialize<'de> for Side {
    /// Reads `BUY` or `SELL`, or any Schwab order instruction that buys
    /// (`BUY_TO_COVER`, `BUY_TO_OPEN`, ...) or sells (`SELL_SHORT`,
    /// `SELL_TO_CLOSE`, ...), in any case.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let upper = text.trim().to_ascii_uppercase();
        let instruction = OrderInstruction::deserialize(upper.as_str().into_deserializer())
            .map_err(|_: de::value::Error| unsupported_side::<D::Error>(&text))?;
        match instruction.buys() {
            Some(true) => Ok(Self::Buy),
            Some(false) => Ok(Self::Sell),
            None => Err(unsupported_side(&text)),
        }
    }
}

fn unsupported_side<E: de::Error>(text: &str) -> E {
    E::custom(format!(
        "unsupported side {text:?}; expected BUY, SELL or a Schwab buy or sell instruction"
    ))
}

fn one() -> Decimal {
    Decimal::ONE
}

/// One execution: a quantity of a symbol bought or sold at a price.
///
/// A paper venue fill (`order_id`, `event_sequence`, `at_ms`, `quantity`,
/// `price`, `fee`) becomes one by adding the `symbol` and `side` of its order;
/// the two paper-only fields are accepted and ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fill {
    pub symbol: String,
    pub side: Side,
    /// Shares or contracts, positive.
    pub quantity: Decimal,
    /// Price per share or contract, positive.
    pub price: Decimal,
    /// Commission and fees for the whole fill, not negative. Default 0.
    #[serde(default)]
    pub fee: Decimal,
    /// Execution time in epoch milliseconds.
    pub at_ms: i64,
    /// Units of currency per price point per contract: 1 for shares, 100 for
    /// equity options. Default 1.
    #[serde(default = "one")]
    pub multiplier: Decimal,
    /// Carried by paper venue fills; not used.
    #[serde(default, skip_serializing)]
    pub order_id: Option<String>,
    /// Carried by paper venue fills; not used.
    #[serde(default, skip_serializing)]
    pub event_sequence: Option<u64>,
}

/// The account's mark-to-market value at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EquityPoint {
    pub at_ms: i64,
    pub equity: Decimal,
}

/// What [`analyze`] is told besides the fills and the curve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerformanceOptions {
    /// Account value before the first fill, positive. Returns are measured
    /// against it, and the first daily return runs from it.
    pub starting_equity: Decimal,
    /// Annual risk-free rate as a fraction (0.04 for 4%). Default 0.
    #[serde(default)]
    pub risk_free_rate: f64,
    /// Daily returns per year, used to annualize. Default 252.
    #[serde(default = "default_periods_per_year")]
    pub periods_per_year: u32,
    /// Per symbol, the `(first, last)` price of a buy-and-hold comparison.
    /// The benchmark return is the equal-weight mean of the symbols' returns.
    #[serde(default)]
    pub benchmark: BTreeMap<String, (Decimal, Decimal)>,
    /// Per symbol, the price that values its open lots. A symbol without one
    /// is valued at its last fill price.
    #[serde(default)]
    pub marks: BTreeMap<String, Decimal>,
}

fn default_periods_per_year() -> u32 {
    DEFAULT_PERIODS_PER_YEAR
}

impl PerformanceOptions {
    /// Options with the given starting equity and every other field default.
    pub fn new(starting_equity: Decimal) -> Self {
        Self {
            starting_equity,
            risk_free_rate: 0.0,
            periods_per_year: DEFAULT_PERIODS_PER_YEAR,
            benchmark: BTreeMap::new(),
            marks: BTreeMap::new(),
        }
    }
}

/// Which way a round trip was exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Long,
    Short,
}

/// Where the equity curve behind the ratios came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EquitySource {
    /// The caller's mark-to-market curve.
    Provided,
    /// Built from the fills; see the module documentation.
    Fills,
}

/// One entry lot closed, in whole or in part, by one fill.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RoundTrip {
    pub symbol: String,
    pub direction: Direction,
    pub quantity: Decimal,
    pub multiplier: Decimal,
    pub entry_price: Decimal,
    pub exit_price: Decimal,
    pub entry_at_ms: i64,
    pub exit_at_ms: i64,
    /// P&L before fees, multiplier applied.
    pub gross_pnl: Decimal,
    /// The entry and exit fee portions.
    pub fees: Decimal,
    pub net_pnl: Decimal,
    pub holding_ms: i64,
}

/// Statistics of a set of round trips.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TradeStats {
    pub round_trips: usize,
    pub wins: usize,
    pub losses: usize,
    pub breakeven: usize,
    /// Wins over round trips; `None` without round trips.
    pub win_rate: Option<f64>,
    /// Net profit of the winners over net loss of the losers; `None` without
    /// a loser.
    pub profit_factor: Option<f64>,
    pub gross_pnl: Decimal,
    /// Fees carried by the round trips (open lots' fees are not included).
    pub fees: Decimal,
    pub net_pnl: Decimal,
    /// Sum of the winners' net P&L.
    pub gross_profit: Decimal,
    /// Sum of the losers' net P&L, zero or negative.
    pub gross_loss: Decimal,
    pub avg_win: Option<Decimal>,
    /// Mean net P&L of the losers, negative.
    pub avg_loss: Option<Decimal>,
    /// Mean net P&L per round trip.
    pub expectancy: Option<Decimal>,
    pub largest_win: Option<Decimal>,
    /// The most negative net P&L, negative.
    pub largest_loss: Option<Decimal>,
    /// Mean holding time, truncated to a millisecond.
    pub avg_holding_ms: Option<i64>,
}

/// One symbol's results.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SymbolReport {
    pub trades: TradeStats,
    /// Realized P&L before fees.
    pub realized_pnl: Decimal,
    /// Every fee paid on the symbol's fills, including those still in open lots.
    pub fees: Decimal,
    /// Open lots valued at `mark`, before fees.
    pub unrealized_pnl: Decimal,
    /// `realized_pnl + unrealized_pnl - fees`.
    pub net_pnl: Decimal,
    /// Signed open quantity: positive long, negative short, zero flat.
    pub open_quantity: Decimal,
    /// The price the open lots were valued at; `None` when flat.
    pub mark: Option<Decimal>,
}

/// The last equity of one New York calendar date and the return from the
/// date before (or from the starting equity, for the first).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DailyPoint {
    pub date: NaiveDate,
    pub equity: Decimal,
    /// `None` when the previous equity was not positive.
    pub daily_return: Option<f64>,
}

/// The full report of [`analyze`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PerformanceReport {
    pub equity_source: EquitySource,
    pub start_equity: Decimal,
    /// The last curve point, or the starting equity plus `net_pnl` from fills.
    pub end_equity: Decimal,
    /// `end_equity - start_equity` on a provided curve; on a curve built from
    /// the fills, `realized_pnl + unrealized_pnl - fees`.
    pub net_pnl: Decimal,
    /// `net_pnl` over the starting equity.
    pub net_return: f64,
    /// Realized P&L before fees.
    pub realized_pnl: Decimal,
    /// Realized P&L after the fees the round trips carry.
    pub realized_pnl_net: Decimal,
    /// Open lots valued at their marks, before fees.
    pub unrealized_pnl: Decimal,
    /// Every fee paid.
    pub fees: Decimal,
    /// Deepest peak-to-trough fall as a fraction of the peak, from 0.
    pub max_drawdown: f64,
    pub max_drawdown_amount: Decimal,
    pub max_drawdown_peak_at_ms: Option<i64>,
    pub max_drawdown_trough_at_ms: Option<i64>,
    /// From the deepest drawdown's peak to its recovery, or to the last point
    /// when it never recovered.
    pub max_drawdown_duration_ms: i64,
    /// The longest time spent below a previous peak, over every drawdown.
    pub longest_drawdown_ms: i64,
    /// Compound annual growth from the daily series.
    pub annualized_return: Option<f64>,
    /// Annualized standard deviation of daily returns.
    pub volatility: Option<f64>,
    pub sharpe: Option<f64>,
    pub sortino: Option<f64>,
    /// Annualized return over `max_drawdown`.
    pub calmar: Option<f64>,
    pub daily: Vec<DailyPoint>,
    pub trades: TradeStats,
    pub symbols: BTreeMap<String, SymbolReport>,
    pub benchmark_return: Option<f64>,
    /// `net_return - benchmark_return`.
    pub excess_return: Option<f64>,
    pub round_trips: Vec<RoundTrip>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidParameter(format!("performance: {}", message.into()))
}

fn overflow(operation: &str) -> Error {
    invalid(format!("decimal overflow in {operation}"))
}

fn add(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_add(b).ok_or_else(|| overflow("addition"))
}

fn sub(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_sub(b).ok_or_else(|| overflow("subtraction"))
}

fn mul(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_mul(b).ok_or_else(|| overflow("multiplication"))
}

fn div(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_div(b).ok_or_else(|| overflow("division"))
}

fn float(value: Decimal) -> Result<f64> {
    value
        .to_f64()
        .ok_or_else(|| invalid(format!("{value} does not fit a float")))
}

/// `numerator / denominator` as a float.
fn ratio(numerator: Decimal, denominator: Decimal) -> Result<f64> {
    float(div(numerator, denominator)?)
}

fn new_york_date(at_ms: i64) -> Result<NaiveDate> {
    DateTime::from_timestamp_millis(at_ms)
        .map(|time| time.with_timezone(&New_York).date_naive())
        .ok_or_else(|| invalid(format!("timestamp {at_ms} is out of range")))
}

/// Check the inputs and return each symbol's multiplier.
fn validate(
    fills: &[Fill],
    equity: &[EquityPoint],
    options: &PerformanceOptions,
) -> Result<BTreeMap<String, Decimal>> {
    if options.starting_equity <= Decimal::ZERO {
        return Err(invalid("starting_equity must be positive"));
    }
    if !options.risk_free_rate.is_finite() {
        return Err(invalid("risk_free_rate must be finite"));
    }
    if options.periods_per_year == 0 {
        return Err(invalid("periods_per_year must be positive"));
    }
    for (symbol, (first, last)) in &options.benchmark {
        if *first <= Decimal::ZERO || *last <= Decimal::ZERO {
            return Err(invalid(format!(
                "benchmark prices for {symbol} must be positive"
            )));
        }
    }
    for (symbol, mark) in &options.marks {
        if *mark <= Decimal::ZERO {
            return Err(invalid(format!("mark for {symbol} must be positive")));
        }
    }

    let mut multipliers = BTreeMap::new();
    let mut previous: Option<i64> = None;
    for (index, fill) in fills.iter().enumerate() {
        let at = |message: &str| invalid(format!("fills[{index}]: {message}"));
        if fill.symbol.trim().is_empty() {
            return Err(at("symbol must not be empty"));
        }
        if fill.quantity <= Decimal::ZERO {
            return Err(at("quantity must be positive"));
        }
        if fill.price <= Decimal::ZERO {
            return Err(at("price must be positive"));
        }
        if fill.fee < Decimal::ZERO {
            return Err(at("fee must not be negative"));
        }
        if fill.multiplier <= Decimal::ZERO {
            return Err(at("multiplier must be positive"));
        }
        if previous.is_some_and(|previous| fill.at_ms < previous) {
            return Err(at(
                "at_ms is earlier than the fill before it; fills must be in time order",
            ));
        }
        previous = Some(fill.at_ms);
        new_york_date(fill.at_ms).map_err(|_| at("at_ms is out of range"))?;
        let multiplier = *multipliers
            .entry(fill.symbol.clone())
            .or_insert(fill.multiplier);
        if multiplier != fill.multiplier {
            return Err(at(&format!(
                "multiplier {} differs from {multiplier} used earlier for {}",
                fill.multiplier, fill.symbol
            )));
        }
    }

    let mut previous: Option<i64> = None;
    for (index, point) in equity.iter().enumerate() {
        if previous.is_some_and(|previous| point.at_ms < previous) {
            return Err(invalid(format!(
                "equity[{index}]: at_ms is earlier than the point before it; \
                 equity points must be in time order"
            )));
        }
        previous = Some(point.at_ms);
        new_york_date(point.at_ms)
            .map_err(|_| invalid(format!("equity[{index}]: at_ms is out of range")))?;
    }
    Ok(multipliers)
}

/// A lot still open: the unmatched part of one fill.
#[derive(Debug, Clone)]
struct Lot {
    long: bool,
    quantity: Decimal,
    price: Decimal,
    /// Fee not yet carried by a round trip.
    fee: Decimal,
    at_ms: i64,
}

/// The result of matching every fill.
struct Matched {
    round_trips: Vec<RoundTrip>,
    open: BTreeMap<String, VecDeque<Lot>>,
    last_price: BTreeMap<String, Decimal>,
    symbol_fees: BTreeMap<String, Decimal>,
    /// After each fill: realized P&L before fees less every fee paid so far.
    cash_curve: Vec<(i64, Decimal)>,
    realized: Decimal,
    fees: Decimal,
}

/// Take the share of `fee` that `matched` of `quantity` carries, leaving the
/// rest. The whole fee goes when the whole quantity does, so the portions of
/// one fee always add up to it.
fn take_fee(fee: &mut Decimal, quantity: Decimal, matched: Decimal) -> Result<Decimal> {
    let portion = if matched >= quantity {
        *fee
    } else {
        div(mul(*fee, matched)?, quantity)?
    };
    *fee = sub(*fee, portion)?;
    Ok(portion)
}

fn match_fills(fills: &[Fill], multipliers: &BTreeMap<String, Decimal>) -> Result<Matched> {
    let mut matched = Matched {
        round_trips: Vec::new(),
        open: BTreeMap::new(),
        last_price: BTreeMap::new(),
        symbol_fees: BTreeMap::new(),
        cash_curve: Vec::with_capacity(fills.len()),
        realized: Decimal::ZERO,
        fees: Decimal::ZERO,
    };
    for fill in fills {
        let buy = fill.side == Side::Buy;
        let multiplier = multipliers
            .get(&fill.symbol)
            .copied()
            .unwrap_or(fill.multiplier);
        let lots = matched.open.entry(fill.symbol.clone()).or_default();
        let mut remaining = fill.quantity;
        let mut remaining_fee = fill.fee;
        while remaining > Decimal::ZERO {
            let Some(lot) = lots.front_mut().filter(|lot| lot.long != buy) else {
                break;
            };
            let size = remaining.min(lot.quantity);
            let entry_fee = take_fee(&mut lot.fee, lot.quantity, size)?;
            let exit_fee = take_fee(&mut remaining_fee, remaining, size)?;
            let points = if lot.long {
                sub(fill.price, lot.price)?
            } else {
                sub(lot.price, fill.price)?
            };
            let gross_pnl = mul(mul(points, size)?, multiplier)?;
            let fees = add(entry_fee, exit_fee)?;
            matched.round_trips.push(RoundTrip {
                symbol: fill.symbol.clone(),
                direction: if lot.long {
                    Direction::Long
                } else {
                    Direction::Short
                },
                quantity: size,
                multiplier,
                entry_price: lot.price,
                exit_price: fill.price,
                entry_at_ms: lot.at_ms,
                exit_at_ms: fill.at_ms,
                gross_pnl,
                fees,
                net_pnl: sub(gross_pnl, fees)?,
                holding_ms: fill
                    .at_ms
                    .checked_sub(lot.at_ms)
                    .ok_or_else(|| overflow("holding time"))?,
            });
            matched.realized = add(matched.realized, gross_pnl)?;
            lot.quantity = sub(lot.quantity, size)?;
            remaining = sub(remaining, size)?;
            if lot.quantity.is_zero() {
                lots.pop_front();
            }
        }
        if remaining > Decimal::ZERO {
            lots.push_back(Lot {
                long: buy,
                quantity: remaining,
                price: fill.price,
                fee: remaining_fee,
                at_ms: fill.at_ms,
            });
        }
        matched.fees = add(matched.fees, fill.fee)?;
        let symbol_fees = matched
            .symbol_fees
            .entry(fill.symbol.clone())
            .or_insert(Decimal::ZERO);
        *symbol_fees = add(*symbol_fees, fill.fee)?;
        matched.last_price.insert(fill.symbol.clone(), fill.price);
        matched
            .cash_curve
            .push((fill.at_ms, sub(matched.realized, matched.fees)?));
    }
    Ok(matched)
}

/// Open lots of one symbol valued at `mark`: `(signed quantity, unrealized)`.
fn value_open(
    lots: &VecDeque<Lot>,
    mark: Decimal,
    multiplier: Decimal,
) -> Result<(Decimal, Decimal)> {
    let mut quantity = Decimal::ZERO;
    let mut unrealized = Decimal::ZERO;
    for lot in lots {
        let points = if lot.long {
            sub(mark, lot.price)?
        } else {
            sub(lot.price, mark)?
        };
        unrealized = add(unrealized, mul(mul(points, lot.quantity)?, multiplier)?)?;
        quantity = if lot.long {
            add(quantity, lot.quantity)?
        } else {
            sub(quantity, lot.quantity)?
        };
    }
    Ok((quantity, unrealized))
}

fn trade_stats(trips: &[&RoundTrip]) -> Result<TradeStats> {
    let mut stats = TradeStats {
        round_trips: trips.len(),
        wins: 0,
        losses: 0,
        breakeven: 0,
        win_rate: None,
        profit_factor: None,
        gross_pnl: Decimal::ZERO,
        fees: Decimal::ZERO,
        net_pnl: Decimal::ZERO,
        gross_profit: Decimal::ZERO,
        gross_loss: Decimal::ZERO,
        avg_win: None,
        avg_loss: None,
        expectancy: None,
        largest_win: None,
        largest_loss: None,
        avg_holding_ms: None,
    };
    let mut holding: i128 = 0;
    for trip in trips {
        stats.gross_pnl = add(stats.gross_pnl, trip.gross_pnl)?;
        stats.fees = add(stats.fees, trip.fees)?;
        stats.net_pnl = add(stats.net_pnl, trip.net_pnl)?;
        holding += i128::from(trip.holding_ms);
        if trip.net_pnl > Decimal::ZERO {
            stats.wins += 1;
            stats.gross_profit = add(stats.gross_profit, trip.net_pnl)?;
            stats.largest_win = Some(
                stats
                    .largest_win
                    .map_or(trip.net_pnl, |w| w.max(trip.net_pnl)),
            );
        } else if trip.net_pnl < Decimal::ZERO {
            stats.losses += 1;
            stats.gross_loss = add(stats.gross_loss, trip.net_pnl)?;
            stats.largest_loss = Some(
                stats
                    .largest_loss
                    .map_or(trip.net_pnl, |l| l.min(trip.net_pnl)),
            );
        } else {
            stats.breakeven += 1;
        }
    }
    if trips.is_empty() {
        return Ok(stats);
    }
    let count = Decimal::from(trips.len());
    stats.win_rate = Some(stats.wins as f64 / trips.len() as f64);
    if stats.losses > 0 {
        stats.profit_factor = Some(ratio(stats.gross_profit, -stats.gross_loss)?);
        stats.avg_loss = Some(div(stats.gross_loss, Decimal::from(stats.losses))?);
    }
    if stats.wins > 0 {
        stats.avg_win = Some(div(stats.gross_profit, Decimal::from(stats.wins))?);
    }
    stats.expectancy = Some(div(stats.net_pnl, count)?);
    stats.avg_holding_ms = i64::try_from(holding / trips.len() as i128).ok();
    Ok(stats)
}

/// Drawdown of a curve that starts at `start`.
#[derive(Debug, Default)]
struct Drawdown {
    fraction: Decimal,
    amount: Decimal,
    peak_at_ms: Option<i64>,
    trough_at_ms: Option<i64>,
    duration_ms: i64,
    longest_ms: i64,
}

fn drawdown(start: Decimal, curve: &[EquityPoint]) -> Result<Drawdown> {
    let mut result = Drawdown::default();
    let Some(first) = curve.first() else {
        return Ok(result);
    };
    let mut peak = start;
    let mut peak_at = first.at_ms;
    let mut underwater = false;
    // Whether the drawdown in progress is the deepest so far.
    let mut deepest = false;
    for point in curve {
        if point.equity >= peak {
            if underwater {
                let held = point.at_ms - peak_at;
                result.longest_ms = result.longest_ms.max(held);
                if deepest {
                    result.duration_ms = held;
                }
                underwater = false;
                deepest = false;
            }
            peak = point.equity;
            peak_at = point.at_ms;
            continue;
        }
        underwater = true;
        let amount = sub(peak, point.equity)?;
        let fraction = div(amount, peak)?;
        if fraction > result.fraction {
            result.fraction = fraction;
            result.amount = amount;
            result.peak_at_ms = Some(peak_at);
            result.trough_at_ms = Some(point.at_ms);
            result.duration_ms = 0;
            deepest = true;
        }
    }
    if underwater && let Some(last) = curve.last() {
        let held = last.at_ms - peak_at;
        result.longest_ms = result.longest_ms.max(held);
        if deepest {
            result.duration_ms = held;
        }
    }
    Ok(result)
}

/// The last equity of each New York date, with the return from the date
/// before.
fn daily_series(start: Decimal, curve: &[EquityPoint]) -> Result<Vec<DailyPoint>> {
    let mut closes: Vec<(NaiveDate, Decimal)> = Vec::new();
    for point in curve {
        let date = new_york_date(point.at_ms)?;
        match closes.last_mut() {
            Some((last, equity)) if *last == date => *equity = point.equity,
            _ => closes.push((date, point.equity)),
        }
    }
    let mut previous = start;
    let mut daily = Vec::with_capacity(closes.len());
    for (date, equity) in closes {
        let daily_return = if previous > Decimal::ZERO {
            Some(ratio(sub(equity, previous)?, previous)?)
        } else {
            None
        };
        daily.push(DailyPoint {
            date,
            equity,
            daily_return,
        });
        previous = equity;
    }
    Ok(daily)
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

/// Sample standard deviation (n - 1); needs two values.
fn sample_deviation(values: &[f64]) -> f64 {
    let center = mean(values);
    let squares: f64 = values.iter().map(|v| (v - center).powi(2)).sum();
    (squares / (values.len() - 1) as f64).sqrt()
}

/// Ratios of a daily return series.
struct Ratios {
    annualized_return: Option<f64>,
    volatility: Option<f64>,
    sharpe: Option<f64>,
    sortino: Option<f64>,
}

fn ratios(
    returns: &[f64],
    options: &PerformanceOptions,
    start: Decimal,
    end: Decimal,
) -> Result<Ratios> {
    let mut result = Ratios {
        annualized_return: None,
        volatility: None,
        sharpe: None,
        sortino: None,
    };
    if returns.len() < 2 {
        return Ok(result);
    }
    let periods = f64::from(options.periods_per_year);
    let annualize = periods.sqrt();
    let target = options.risk_free_rate / periods;
    let excess: Vec<f64> = returns.iter().map(|r| r - target).collect();
    let deviation = sample_deviation(&excess);
    result.volatility = Some(deviation * annualize);
    if deviation >= MIN_DEVIATION {
        result.sharpe = Some(mean(&excess) / deviation * annualize);
    }
    let downside =
        (excess.iter().map(|r| r.min(0.0).powi(2)).sum::<f64>() / excess.len() as f64).sqrt();
    if downside >= MIN_DEVIATION {
        result.sortino = Some(mean(&excess) / downside * annualize);
    }
    if end > Decimal::ZERO {
        let growth = ratio(end, start)?;
        let annual = growth.powf(periods / returns.len() as f64) - 1.0;
        result.annualized_return = annual.is_finite().then_some(annual);
    }
    Ok(result)
}

fn benchmark_return(options: &PerformanceOptions) -> Result<Option<f64>> {
    if options.benchmark.is_empty() {
        return Ok(None);
    }
    let mut total = Decimal::ZERO;
    for (first, last) in options.benchmark.values() {
        total = add(total, sub(div(*last, *first)?, Decimal::ONE)?)?;
    }
    ratio(total, Decimal::from(options.benchmark.len())).map(Some)
}

/// Analyze `fills` and, when given, the `equity` curve they ran through.
///
/// Fills must be in time order, with a positive quantity and price and a fee
/// of zero or more; equity points must be in time order. A violation is an
/// [`Error::InvalidParameter`] naming the offending index. Arithmetic is
/// checked, so values large enough to overflow a [`Decimal`] are refused
/// rather than wrapped.
///
/// With no curve (an empty slice) the curve is built from the fills; see the
/// module documentation. With no fills the report is flat: the starting
/// equity, no round trips and no ratios.
pub fn analyze(
    fills: &[Fill],
    equity: &[EquityPoint],
    options: &PerformanceOptions,
) -> Result<PerformanceReport> {
    let multipliers = validate(fills, equity, options)?;
    let matched = match_fills(fills, &multipliers)?;
    let start = options.starting_equity;

    let mut symbols = BTreeMap::new();
    let mut unrealized_pnl = Decimal::ZERO;
    for (symbol, lots) in &matched.open {
        let multiplier = multipliers.get(symbol).copied().unwrap_or(Decimal::ONE);
        let last = matched.last_price.get(symbol).copied();
        let mark = options.marks.get(symbol).copied().or(last);
        let (open_quantity, unrealized) = match mark {
            Some(mark) if !lots.is_empty() => value_open(lots, mark, multiplier)?,
            _ => (Decimal::ZERO, Decimal::ZERO),
        };
        let trips: Vec<&RoundTrip> = matched
            .round_trips
            .iter()
            .filter(|trip| &trip.symbol == symbol)
            .collect();
        let trades = trade_stats(&trips)?;
        let fees = matched
            .symbol_fees
            .get(symbol)
            .copied()
            .unwrap_or(Decimal::ZERO);
        unrealized_pnl = add(unrealized_pnl, unrealized)?;
        symbols.insert(
            symbol.clone(),
            SymbolReport {
                realized_pnl: trades.gross_pnl,
                net_pnl: sub(add(trades.gross_pnl, unrealized)?, fees)?,
                trades,
                fees,
                unrealized_pnl: unrealized,
                open_quantity,
                mark: if lots.is_empty() { None } else { mark },
            },
        );
    }

    let fill_net_pnl = sub(add(matched.realized, unrealized_pnl)?, matched.fees)?;
    let (curve, equity_source) = if equity.is_empty() {
        let mut points: Vec<EquityPoint> = Vec::with_capacity(matched.cash_curve.len());
        for (at_ms, cash_pnl) in &matched.cash_curve {
            points.push(EquityPoint {
                at_ms: *at_ms,
                equity: add(start, *cash_pnl)?,
            });
        }
        if let Some(last) = points.last_mut() {
            last.equity = add(start, fill_net_pnl)?;
        }
        (points, EquitySource::Fills)
    } else {
        (equity.to_vec(), EquitySource::Provided)
    };

    let end_equity = curve.last().map_or(start, |point| point.equity);
    let net_pnl = match equity_source {
        EquitySource::Provided => sub(end_equity, start)?,
        EquitySource::Fills => fill_net_pnl,
    };
    let net_return = ratio(net_pnl, start)?;

    let drawdown = drawdown(start, &curve)?;
    let daily = daily_series(start, &curve)?;
    let returns: Vec<f64> = daily.iter().filter_map(|p| p.daily_return).collect();
    let ratios = ratios(&returns, options, start, end_equity)?;
    let max_drawdown = float(drawdown.fraction)?;
    let calmar = match ratios.annualized_return {
        Some(annual) if max_drawdown > 0.0 => Some(annual / max_drawdown),
        _ => None,
    };
    let benchmark = benchmark_return(options)?;

    let all: Vec<&RoundTrip> = matched.round_trips.iter().collect();
    let trades = trade_stats(&all)?;
    Ok(PerformanceReport {
        equity_source,
        start_equity: start,
        end_equity,
        net_pnl,
        net_return,
        realized_pnl: matched.realized,
        realized_pnl_net: trades.net_pnl,
        unrealized_pnl,
        fees: matched.fees,
        max_drawdown,
        max_drawdown_amount: drawdown.amount,
        max_drawdown_peak_at_ms: drawdown.peak_at_ms,
        max_drawdown_trough_at_ms: drawdown.trough_at_ms,
        max_drawdown_duration_ms: drawdown.duration_ms,
        longest_drawdown_ms: drawdown.longest_ms,
        annualized_return: ratios.annualized_return,
        volatility: ratios.volatility,
        sharpe: ratios.sharpe,
        sortino: ratios.sortino,
        calmar,
        daily,
        trades,
        symbols,
        benchmark_return: benchmark,
        excess_return: benchmark.map(|b| net_return - b),
        round_trips: matched.round_trips,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn d(v: &str) -> Decimal {
        v.parse().unwrap()
    }

    fn fill(symbol: &str, side: Side, quantity: &str, price: &str, fee: &str, at_ms: i64) -> Fill {
        Fill {
            symbol: symbol.into(),
            side,
            quantity: d(quantity),
            price: d(price),
            fee: d(fee),
            at_ms,
            multiplier: Decimal::ONE,
            order_id: None,
            event_sequence: None,
        }
    }

    fn options() -> PerformanceOptions {
        PerformanceOptions::new(d("1000"))
    }

    fn run(fills: &[Fill]) -> PerformanceReport {
        analyze(fills, &[], &options()).unwrap()
    }

    fn close(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{actual} is not within {tolerance} of {expected}"
        );
    }

    /// Noon in New York on 2026-01-05 (a Monday), plus `days`.
    fn noon(days: i64) -> i64 {
        Utc.with_ymd_and_hms(2026, 1, 5, 17, 0, 0)
            .unwrap()
            .timestamp_millis()
            + days * 86_400_000
    }

    const HOUR: i64 = 3_600_000;

    fn point(at_ms: i64, equity: &str) -> EquityPoint {
        EquityPoint {
            at_ms,
            equity: d(equity),
        }
    }

    fn invalid_message(result: Result<PerformanceReport>) -> String {
        match result {
            Err(Error::InvalidParameter(message)) => message,
            other => panic!("expected InvalidParameter, got {other:?}"),
        }
    }

    /// Buys 10 at 100 and 10 at 110, sells 15 at 120, then 5 at 90.
    fn scaled_fills() -> Vec<Fill> {
        vec![
            fill("ABC", Side::Buy, "10", "100", "1", 1000),
            fill("ABC", Side::Buy, "10", "110", "1", 2000),
            fill("ABC", Side::Sell, "15", "120", "3", 3000),
            fill("ABC", Side::Sell, "5", "90", "1", 5000),
        ]
    }

    #[test]
    fn scale_in_and_scale_out_are_matched_first_in_first_out() {
        let report = run(&scaled_fills());
        let trips = &report.round_trips;
        assert_eq!(trips.len(), 3);

        // The 15-share sell closes the 10 at 100, then 5 of the 10 at 110.
        assert_eq!(trips[0].quantity, d("10"));
        assert_eq!(
            (trips[0].entry_price, trips[0].exit_price),
            (d("100"), d("120"))
        );
        assert_eq!(trips[0].gross_pnl, d("200"));
        // Entry fee 1 whole; exit fee 3 * 10/15 = 2.
        assert_eq!(trips[0].fees, d("3"));
        assert_eq!(trips[0].net_pnl, d("197"));
        assert_eq!(trips[0].holding_ms, 2000);

        assert_eq!(trips[1].quantity, d("5"));
        assert_eq!(
            (trips[1].entry_price, trips[1].exit_price),
            (d("110"), d("120"))
        );
        assert_eq!(trips[1].gross_pnl, d("50"));
        // Entry fee 1 * 5/10 = 0.5; exit fee is the 1 left of the 3.
        assert_eq!(trips[1].fees, d("1.5"));
        assert_eq!(trips[1].net_pnl, d("48.5"));
        assert_eq!(trips[1].holding_ms, 1000);

        // The last 5 at 110 close against the 5-share sell at 90.
        assert_eq!(trips[2].gross_pnl, d("-100"));
        assert_eq!(trips[2].fees, d("1.5"));
        assert_eq!(trips[2].net_pnl, d("-101.5"));
        assert_eq!((trips[2].entry_at_ms, trips[2].exit_at_ms), (2000, 5000));
        assert!(trips.iter().all(|t| t.direction == Direction::Long));

        let stats = &report.trades;
        assert_eq!((stats.round_trips, stats.wins, stats.losses), (3, 2, 1));
        close(stats.win_rate.unwrap(), 2.0 / 3.0, 1e-12);
        close(stats.profit_factor.unwrap(), 245.5 / 101.5, 1e-12);
        assert_eq!(stats.gross_profit, d("245.5"));
        assert_eq!(stats.gross_loss, d("-101.5"));
        assert_eq!(stats.avg_win, Some(d("122.75")));
        assert_eq!(stats.avg_loss, Some(d("-101.5")));
        assert_eq!(stats.expectancy, Some(d("48")));
        assert_eq!(stats.largest_win, Some(d("197")));
        assert_eq!(stats.largest_loss, Some(d("-101.5")));
        assert_eq!(stats.avg_holding_ms, Some(2000));

        assert_eq!(report.realized_pnl, d("150"));
        assert_eq!(report.realized_pnl_net, d("144"));
        assert_eq!(report.fees, d("6"));
        assert_eq!(report.unrealized_pnl, d("0"));
        assert_eq!(report.net_pnl, d("144"));
        assert_eq!(report.end_equity, d("1144"));
        close(report.net_return, 0.144, 1e-12);
    }

    #[test]
    fn a_short_round_trip_profits_when_the_price_falls() {
        let report = run(&[
            fill("ABC", Side::Sell, "10", "50", "2", 1000),
            fill("ABC", Side::Buy, "10", "45", "2", 4000),
        ]);
        let trip = &report.round_trips[0];
        assert_eq!(trip.direction, Direction::Short);
        assert_eq!((trip.entry_price, trip.exit_price), (d("50"), d("45")));
        assert_eq!(trip.gross_pnl, d("50"));
        assert_eq!(trip.fees, d("4"));
        assert_eq!(trip.net_pnl, d("46"));
        assert_eq!(trip.holding_ms, 3000);
        assert_eq!(report.net_pnl, d("46"));
    }

    #[test]
    fn a_fill_through_flat_closes_one_position_and_opens_the_other() {
        let report = run(&[
            fill("ABC", Side::Buy, "10", "100", "2", 1000),
            fill("ABC", Side::Sell, "25", "110", "5", 2000),
            fill("ABC", Side::Buy, "15", "105", "3", 3000),
        ]);
        assert_eq!(report.round_trips.len(), 2);

        // 10 of the 25 sold close the long: exit fee 5 * 10/25 = 2.
        let long = &report.round_trips[0];
        assert_eq!(long.direction, Direction::Long);
        assert_eq!(long.quantity, d("10"));
        assert_eq!(long.gross_pnl, d("100"));
        assert_eq!(long.fees, d("4"));
        assert_eq!(long.net_pnl, d("96"));

        // The other 15 open a short carrying the remaining fee 3.
        let short = &report.round_trips[1];
        assert_eq!(short.direction, Direction::Short);
        assert_eq!(short.quantity, d("15"));
        assert_eq!((short.entry_price, short.exit_price), (d("110"), d("105")));
        assert_eq!(short.entry_at_ms, 2000);
        assert_eq!(short.gross_pnl, d("75"));
        assert_eq!(short.fees, d("6"));
        assert_eq!(short.net_pnl, d("69"));

        assert_eq!(report.fees, d("10"));
        assert_eq!(report.trades.fees, d("10"));
        assert_eq!(report.symbols["ABC"].open_quantity, d("0"));
        assert_eq!(report.symbols["ABC"].mark, None);
    }

    #[test]
    fn option_contracts_scale_profit_by_the_multiplier() {
        let mut open = fill("ABC  260116C00100000", Side::Buy, "2", "1.50", "1.30", 1000);
        let mut close_ = fill(
            "ABC  260116C00100000",
            Side::Sell,
            "2",
            "2.00",
            "1.30",
            2000,
        );
        open.multiplier = d("100");
        close_.multiplier = d("100");
        let report = run(&[open, close_]);
        let trip = &report.round_trips[0];
        assert_eq!(trip.multiplier, d("100"));
        assert_eq!(trip.gross_pnl, d("100"));
        assert_eq!(trip.fees, d("2.60"));
        assert_eq!(trip.net_pnl, d("97.40"));
    }

    #[test]
    fn the_portions_of_one_fee_add_back_to_it() {
        let report = run(&[
            fill("ABC", Side::Buy, "3", "10", "1", 1000),
            fill("ABC", Side::Sell, "1", "11", "0.1", 2000),
            fill("ABC", Side::Sell, "1", "11", "0.1", 3000),
            fill("ABC", Side::Sell, "1", "11", "0.1", 4000),
        ]);
        assert_eq!(report.round_trips.len(), 3);
        // A third of 1 cannot be written exactly, but the thirds sum to 1.
        assert_eq!(report.trades.fees, d("1.3"));
        assert_eq!(report.fees, d("1.3"));
    }

    #[test]
    fn open_lots_are_valued_at_the_mark_or_the_last_fill_price() {
        let fills = [
            fill("LONG", Side::Buy, "10", "100", "1", 1000),
            fill("SHORT", Side::Sell, "4", "50", "0", 2000),
        ];
        let unmarked = run(&fills);
        assert_eq!(unmarked.unrealized_pnl, d("0"));
        assert_eq!(unmarked.net_pnl, d("-1"));
        assert_eq!(unmarked.symbols["LONG"].mark, Some(d("100")));

        let mut marked = options();
        marked.marks.insert("LONG".into(), d("105"));
        marked.marks.insert("SHORT".into(), d("45"));
        let report = analyze(&fills, &[], &marked).unwrap();
        assert_eq!(report.symbols["LONG"].unrealized_pnl, d("50"));
        assert_eq!(report.symbols["LONG"].open_quantity, d("10"));
        assert_eq!(report.symbols["LONG"].net_pnl, d("49"));
        assert_eq!(report.symbols["SHORT"].unrealized_pnl, d("20"));
        assert_eq!(report.symbols["SHORT"].open_quantity, d("-4"));
        assert_eq!(report.symbols["SHORT"].mark, Some(d("45")));
        assert_eq!(report.unrealized_pnl, d("70"));
        // Unrealized 70 less the 1 of fees.
        assert_eq!(report.net_pnl, d("69"));
        assert_eq!(report.end_equity, d("1069"));
        assert_eq!(report.trades.round_trips, 0);
    }

    #[test]
    fn the_curve_is_built_from_fills_when_none_is_given() {
        let report = run(&scaled_fills());
        assert_eq!(report.equity_source, EquitySource::Fills);
        // After each fill: 999, 998, 1000 + 250 - 5, then 1000 + 150 - 6.
        // The deepest drawdown is 1245 -> 1144, from the peak at the third
        // fill to the last, which is still under water at the end.
        close(report.max_drawdown, 101.0 / 1245.0, 1e-12);
        assert_eq!(report.max_drawdown_amount, d("101"));
        assert_eq!(report.max_drawdown_peak_at_ms, Some(3000));
        assert_eq!(report.max_drawdown_trough_at_ms, Some(5000));
        assert_eq!(report.max_drawdown_duration_ms, 2000);
        // The first dip, 1000 -> 998, lasted from the first fill to the third.
        assert_eq!(report.longest_drawdown_ms, 2000);
        assert_eq!(report.daily.len(), 1);
        assert_eq!(report.daily[0].equity, d("1144"));
    }

    #[test]
    fn drawdown_reports_depth_peak_trough_and_duration() {
        let t = noon(0);
        let curve = [
            point(t, "1000"),
            point(t + HOUR, "1100"),
            point(t + 2 * HOUR, "990"),
            point(t + 3 * HOUR, "1045"),
            point(t + 4 * HOUR, "1200"),
            point(t + 5 * HOUR, "1020"),
            point(t + 6 * HOUR, "1100"),
        ];
        let report = analyze(&[], &curve, &options()).unwrap();
        // 1100 -> 990 is 10% and recovers; 1200 -> 1020 is 15% and does not.
        close(report.max_drawdown, 0.15, 1e-12);
        assert_eq!(report.max_drawdown_amount, d("180"));
        assert_eq!(report.max_drawdown_peak_at_ms, Some(t + 4 * HOUR));
        assert_eq!(report.max_drawdown_trough_at_ms, Some(t + 5 * HOUR));
        assert_eq!(report.max_drawdown_duration_ms, 2 * HOUR);
        // The first drawdown, 1h to 4h, is the longer.
        assert_eq!(report.longest_drawdown_ms, 3 * HOUR);
        assert_eq!(report.equity_source, EquitySource::Provided);
        assert_eq!(report.end_equity, d("1100"));
        assert_eq!(report.net_pnl, d("100"));
        close(report.net_return, 0.1, 1e-12);
    }

    #[test]
    fn a_curve_that_only_rises_has_no_drawdown() {
        let t = noon(0);
        let report = analyze(
            &[],
            &[point(t, "1000"), point(t + HOUR, "1010")],
            &options(),
        )
        .unwrap();
        assert_eq!(report.max_drawdown, 0.0);
        assert_eq!(report.max_drawdown_peak_at_ms, None);
        assert_eq!(report.max_drawdown_duration_ms, 0);
        assert_eq!(report.longest_drawdown_ms, 0);
        assert_eq!(report.calmar, None);
    }

    /// 100 -> 110 -> 99 -> 108.9: daily returns of +10%, -10% and +10%.
    fn known_series() -> Vec<EquityPoint> {
        vec![
            point(noon(0), "110"),
            point(noon(1), "99"),
            point(noon(2), "108.9"),
        ]
    }

    #[test]
    fn sharpe_sortino_and_volatility_follow_the_formulas() {
        let mut opts = PerformanceOptions::new(d("100"));
        let report = analyze(&[], &known_series(), &opts).unwrap();
        let returns: Vec<_> = report
            .daily
            .iter()
            .map(|p| p.daily_return.unwrap())
            .collect();
        for (actual, expected) in returns.iter().zip([0.1, -0.1, 0.1]) {
            close(*actual, expected, 1e-12);
        }
        // mean 0.1/3; sample deviation sqrt(0.04/3) = 0.115470;
        // downside deviation sqrt(0.01/3) = 0.057735; annualized by sqrt(252).
        close(report.volatility.unwrap(), 1.83303, 1e-4);
        close(report.sharpe.unwrap(), 4.58258, 1e-4);
        close(report.sortino.unwrap(), 9.16515, 1e-4);

        // A risk-free rate of 25.2% a year is 0.1% a day off every return.
        opts.risk_free_rate = 0.252;
        let report = analyze(&[], &known_series(), &opts).unwrap();
        close(report.sharpe.unwrap(), 4.44510, 1e-4);
    }

    #[test]
    fn calmar_divides_the_annualized_return_by_the_drawdown() {
        let report = analyze(&[], &known_series(), &PerformanceOptions::new(d("100"))).unwrap();
        // 1.089 over three days, compounded out to 252.
        let annual = 1.089_f64.powf(252.0 / 3.0) - 1.0;
        close(report.annualized_return.unwrap() / annual, 1.0, 1e-9);
        close(report.max_drawdown, 0.1, 1e-12);
        close(report.calmar.unwrap() / (annual / 0.1), 1.0, 1e-9);
    }

    #[test]
    fn ratios_need_two_returns_and_some_deviation() {
        let one_day = analyze(&[], &[point(noon(0), "1010")], &options()).unwrap();
        assert_eq!(one_day.daily.len(), 1);
        assert_eq!(one_day.sharpe, None);
        assert_eq!(one_day.sortino, None);
        assert_eq!(one_day.volatility, None);
        assert_eq!(one_day.calmar, None);

        // Equal daily growth has no deviation; with no losing day there is
        // no downside either.
        let flat: Vec<_> = (0..4).map(|day| point(noon(day), "1000")).collect();
        let report = analyze(&[], &flat, &options()).unwrap();
        assert_eq!(report.sharpe, None);
        assert_eq!(report.sortino, None);
        assert_eq!(report.volatility, Some(0.0));
    }

    #[test]
    fn daily_returns_group_by_new_york_date() {
        let utc = |day: u32, hour: u32| {
            Utc.with_ymd_and_hms(2026, 1, day, hour, 0, 0)
                .unwrap()
                .timestamp_millis()
        };
        let curve = [
            point(utc(5, 17), "1010"),
            // 03:00 UTC on the 6th is 22:00 on the 5th in New York.
            point(utc(6, 3), "1020"),
            point(utc(6, 15), "1030"),
        ];
        let report = analyze(&[], &curve, &options()).unwrap();
        assert_eq!(report.daily.len(), 2);
        assert_eq!(report.daily[0].date.to_string(), "2026-01-05");
        assert_eq!(report.daily[0].equity, d("1020"));
        close(report.daily[0].daily_return.unwrap(), 0.02, 1e-12);
        assert_eq!(report.daily[1].date.to_string(), "2026-01-06");
        close(
            report.daily[1].daily_return.unwrap(),
            1030.0 / 1020.0 - 1.0,
            1e-12,
        );
    }

    #[test]
    fn the_benchmark_is_an_equal_weight_buy_and_hold() {
        let mut opts = options();
        opts.benchmark.insert("AAA".into(), (d("100"), d("120")));
        opts.benchmark.insert("BBB".into(), (d("50"), d("45")));
        let report = analyze(&[], &[point(noon(0), "1100")], &opts).unwrap();
        // (+20% and -10%) / 2
        close(report.benchmark_return.unwrap(), 0.05, 1e-12);
        close(report.excess_return.unwrap(), 0.05, 1e-12);

        let report = analyze(&[], &[point(noon(0), "1100")], &options()).unwrap();
        assert_eq!(report.benchmark_return, None);
        assert_eq!(report.excess_return, None);
    }

    #[test]
    fn a_provided_curve_decides_net_pnl_over_the_fills() {
        let report = analyze(&scaled_fills(), &[point(noon(0), "1500")], &options()).unwrap();
        assert_eq!(report.equity_source, EquitySource::Provided);
        assert_eq!(report.net_pnl, d("500"));
        assert_eq!(report.end_equity, d("1500"));
        // The fills still give their own accounting.
        assert_eq!(report.realized_pnl_net, d("144"));
    }

    #[test]
    fn symbols_are_broken_down_with_their_own_stats() {
        let report = run(&[
            fill("AAA", Side::Buy, "10", "10", "0", 1000),
            fill("BBB", Side::Sell, "5", "20", "0", 1500),
            fill("AAA", Side::Sell, "10", "12", "0", 2000),
            fill("BBB", Side::Buy, "5", "22", "0", 2500),
        ]);
        let names: Vec<_> = report.symbols.keys().cloned().collect();
        assert_eq!(names, ["AAA", "BBB"]);
        assert_eq!(report.symbols["AAA"].net_pnl, d("20"));
        assert_eq!(report.symbols["AAA"].trades.wins, 1);
        assert_eq!(report.symbols["AAA"].trades.profit_factor, None);
        assert_eq!(report.symbols["BBB"].net_pnl, d("-10"));
        assert_eq!(report.symbols["BBB"].trades.losses, 1);
        assert_eq!(report.symbols["BBB"].trades.win_rate, Some(0.0));
        assert_eq!(report.symbols["BBB"].trades.profit_factor, Some(0.0));
        assert_eq!(report.trades.net_pnl, d("10"));
        close(report.trades.win_rate.unwrap(), 0.5, 1e-12);
        close(report.trades.profit_factor.unwrap(), 2.0, 1e-12);
    }

    #[test]
    fn a_trade_that_breaks_even_is_neither_a_win_nor_a_loss() {
        let report = run(&[
            fill("ABC", Side::Buy, "1", "10", "0", 1000),
            fill("ABC", Side::Sell, "1", "10", "0", 2000),
        ]);
        let stats = &report.trades;
        assert_eq!((stats.wins, stats.losses, stats.breakeven), (0, 0, 1));
        assert_eq!(stats.win_rate, Some(0.0));
        assert_eq!(stats.profit_factor, None);
        assert_eq!(stats.avg_win, None);
        assert_eq!(stats.avg_loss, None);
        assert_eq!(stats.expectancy, Some(d("0")));
    }

    #[test]
    fn empty_input_reports_a_flat_account() {
        let report = run(&[]);
        assert_eq!(report.start_equity, d("1000"));
        assert_eq!(report.end_equity, d("1000"));
        assert_eq!(report.net_pnl, d("0"));
        assert_eq!(report.net_return, 0.0);
        assert_eq!(report.fees, d("0"));
        assert_eq!(report.max_drawdown, 0.0);
        assert!(report.daily.is_empty() && report.symbols.is_empty());
        assert!(report.round_trips.is_empty());
        assert_eq!(report.trades.round_trips, 0);
        assert_eq!(report.trades.win_rate, None);
        assert_eq!(report.trades.expectancy, None);
        assert_eq!(report.trades.avg_holding_ms, None);
        assert_eq!(report.sharpe, None);
        assert_eq!(report.annualized_return, None);
    }

    #[test]
    fn bad_inputs_are_refused_with_the_offending_index() {
        let ok = fill("ABC", Side::Buy, "1", "10", "0", 1000);
        let with = |change: &dyn Fn(&mut Fill)| {
            let mut bad = ok.clone();
            change(&mut bad);
            invalid_message(analyze(&[ok.clone(), bad], &[], &options()))
        };

        let message = with(&|f| f.at_ms = 999);
        assert!(
            message.contains("fills[1]") && message.contains("time order"),
            "{message}"
        );
        assert!(with(&|f| f.quantity = d("0")).contains("quantity must be positive"));
        assert!(with(&|f| f.price = d("-1")).contains("price must be positive"));
        assert!(with(&|f| f.fee = d("-0.01")).contains("fee must not be negative"));
        assert!(with(&|f| f.symbol = " ".into()).contains("symbol must not be empty"));
        assert!(with(&|f| f.multiplier = d("100")).contains("multiplier"));
        assert!(with(&|f| f.at_ms = i64::MAX).contains("out of range"));

        let curve = [point(2000, "1000"), point(1000, "1000")];
        let message = invalid_message(analyze(&[], &curve, &options()));
        assert!(
            message.contains("equity[1]") && message.contains("time order"),
            "{message}"
        );
    }

    #[test]
    fn bad_options_are_refused() {
        let check = |change: &dyn Fn(&mut PerformanceOptions), expected: &str| {
            let mut opts = options();
            change(&mut opts);
            let message = invalid_message(analyze(&[], &[], &opts));
            assert!(message.contains(expected), "{message}");
        };
        check(&|o| o.starting_equity = d("0"), "starting_equity");
        check(&|o| o.periods_per_year = 0, "periods_per_year");
        check(&|o| o.risk_free_rate = f64::NAN, "risk_free_rate");
        check(
            &|o| o.marks = BTreeMap::from([("A".into(), d("0"))]),
            "mark for A",
        );
        check(
            &|o| o.benchmark = BTreeMap::from([("A".into(), (d("0"), d("1")))]),
            "benchmark",
        );
    }

    #[test]
    fn overflow_is_an_error_not_a_panic() {
        let result = analyze(
            &[
                fill("ABC", Side::Buy, "100000000000000000000", "1", "0", 1000),
                fill(
                    "ABC",
                    Side::Sell,
                    "100000000000000000000",
                    "1000000000",
                    "0",
                    2000,
                ),
            ],
            &[],
            &options(),
        );
        assert!(invalid_message(result).contains("overflow"));
    }

    #[test]
    fn sides_read_schwab_instructions_in_any_case() {
        let side = |text: &str| serde_json::from_value::<Side>(serde_json::json!(text));
        for buy in ["BUY", "buy", "BUY_TO_COVER", "BUY_TO_OPEN", "BUY_TO_CLOSE"] {
            assert_eq!(side(buy).unwrap(), Side::Buy, "{buy}");
        }
        for sell in [
            "SELL",
            "Sell",
            "SELL_SHORT",
            "SELL_TO_OPEN",
            "SELL_TO_CLOSE",
        ] {
            assert_eq!(side(sell).unwrap(), Side::Sell, "{sell}");
        }
        for bad in ["EXCHANGE", "HOLD", ""] {
            let error = side(bad).unwrap_err().to_string();
            assert!(error.contains("unsupported side"), "{error}");
        }
    }

    #[test]
    fn a_paper_fill_with_its_symbol_and_side_deserializes_as_is() {
        let paper: Fill = serde_json::from_value(serde_json::json!({
            "order_id": "1", "event_sequence": 7, "at_ms": 60000,
            "quantity": "5", "price": "10.61", "fee": "0.05",
            "symbol": "ABC", "side": "BUY",
        }))
        .unwrap();
        assert_eq!(paper.fee, d("0.05"));
        assert_eq!(paper.multiplier, Decimal::ONE);
        assert_eq!(paper.order_id.as_deref(), Some("1"));

        let bare: Fill = serde_json::from_value(serde_json::json!({
            "symbol": "ABC", "side": "SELL", "quantity": 2, "price": 3.5, "at_ms": 1,
        }))
        .unwrap();
        assert_eq!(bare.fee, Decimal::ZERO);

        let typo = serde_json::from_value::<Fill>(serde_json::json!({
            "symbol": "ABC", "side": "SELL", "quantity": 2, "price": 3.5, "at_ms": 1,
            "fees": "1",
        }));
        assert!(typo.is_err());
    }

    #[test]
    fn the_report_serializes_money_as_strings() {
        let report = run(&scaled_fills());
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["fees"], "6");
        assert_eq!(value["equity_source"], "fills");
        assert_eq!(value["round_trips"][0]["direction"], "long");
        assert_eq!(value["round_trips"][0]["net_pnl"], "197");
        assert_eq!(value["daily"][0]["date"], "1969-12-31");
        assert!(value["net_return"].is_f64());
        assert!(value["sharpe"].is_null());
        assert_eq!(value["symbols"]["ABC"]["trades"]["wins"], 2);
    }
}
