//! `get_performance`: an account's round trips and profit and loss, worked
//! out from its own fills, so a model evaluates its trading from verified
//! numbers instead of doing the arithmetic.
//!
//! The orders entered in the window are read through the backend (live
//! Schwab or the paper venue, which renders its fills in Schwab's shape),
//! their executions become [`Fill`]s and [`schwab_rs::performance::analyze`]
//! does the accounting. Only fills in the window's orders are seen:
//! a position opened before the window has no entry to match, and the call
//! says so when the account's holdings disagree with what the fills leave.
//!
//! The SDK needs a starting equity to measure returns against. It is the
//! account's current liquidation value less the window's net profit and
//! loss, which holds only while nothing else moved the balance (deposits,
//! withdrawals, dividends); without an account value the return and ratio
//! fields are null and `warnings` says why.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use schwab_rs::performance::{self, Fill, PerformanceOptions, PerformanceReport, Side};
use schwab_types::accounts::Position;
use schwab_types::common::{AssetType, parse_schwab_time};
use schwab_types::market_data::{QuoteField, QuoteItem};
use schwab_types::trading::{ExecutionLeg, Order, OrderActivityType, OrderLeg, OrdersParams};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::backend::Backend;
use crate::contract::ToolFailure;
use crate::portfolio::ORDER_LOOKBACK_DAYS;
use crate::query::{FlexibleTime, window_of};

/// Width of an omitted window, ending at its end.
pub const DEFAULT_WINDOW_DAYS: i64 = 30;

/// The most round trips listed, the most recent by exit time; the rest are
/// counted but not listed, and every statistic covers all of them.
pub const MAX_ROUND_TRIPS: usize = 100;

/// The most open symbols quoted for marks; the rest are valued at their
/// last fill price.
const MAX_MARKED_SYMBOLS: usize = 100;

/// Contracts per equity option unless the operator or Schwab says otherwise.
const OPTION_MULTIPLIER: i64 = 100;

/// Schwab's default page of an order list: a list this long may be cut off.
const ORDER_PAGE: usize = 3000;

/// Open quantity this close to the account's own is the same quantity.
const QUANTITY_TOLERANCE: f64 = 1e-6;

/// The operator-verified contract multipliers by option symbol: a
/// principal's `option_multipliers`.
pub type Multipliers = HashMap<String, Decimal>;

const NOTES: &str = "Covers the fills of orders entered in the window, matched first in first \
     out; a position opened before the window has no entry here, so its closing fills open a \
     position instead (warnings name any symbol whose holdings disagree). Schwab's order data \
     carries no commissions or fees, so live fees are zero and net equals gross (the paper \
     venue reports its own). Unrealized profit \
     and loss values open lots at the quote in `marks` (source quote) or, failing that, the \
     last fill price (source last_fill). The equity curve is built from the fills and the \
     ratios (sharpe, sortino, volatility) use only the days with fills: indicative, not a \
     substitute for a daily account history. Fills of futures, forex and fixed income are \
     skipped (`skippedExecutions`).";

/// `get_performance` arguments, the shape of the tool's signature, so the
/// authorized transport extracts them from raw arguments exactly as the
/// `#[tool]` macro does.
#[derive(Debug, Deserialize)]
pub struct Request {
    pub account_hash: String,
    #[serde(default)]
    pub since: Option<FlexibleTime>,
    #[serde(default)]
    pub until: Option<FlexibleTime>,
    #[serde(default)]
    pub symbols: Option<Vec<String>>,
}

impl Request {
    pub fn parse(args: Value) -> Result<Self, ToolFailure> {
        serde_json::from_value(args).map_err(|e| {
            ToolFailure::invalid_arguments(format!("Invalid get_performance arguments: {e}"))
        })
    }
}

/// The span of orders read: where the request put it and whether Schwab's
/// list horizon cut it short.
#[derive(Debug, PartialEq)]
struct Window {
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    clamped: bool,
}

impl Window {
    /// The request's window, by default the last [`DEFAULT_WINDOW_DAYS`]
    /// ending `now`. With `horizon`, a start older than Schwab's order list
    /// reaches is moved up to it.
    fn resolve(request: &Request, now: DateTime<Utc>, horizon: bool) -> Result<Self, ToolFailure> {
        let (mut since, until) = window_of(
            request.since.as_ref(),
            request.until.as_ref(),
            now,
            DEFAULT_WINDOW_DAYS,
        )
        .map_err(ToolFailure::invalid_arguments)?;
        let mut clamped = false;
        let oldest = now - Duration::days(ORDER_LOOKBACK_DAYS);
        if horizon && since < oldest {
            since = oldest;
            clamped = true;
        }
        if since > until {
            return Err(ToolFailure::invalid_arguments(if clamped {
                format!(
                    "The window ends before the {ORDER_LOOKBACK_DAYS} days of orders Schwab \
                     lists; `until` must be later than {}",
                    oldest.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                )
            } else {
                "`since` must not be after `until`".to_owned()
            }));
        }
        Ok(Self {
            since,
            until,
            clamped,
        })
    }
}

/// The symbols a request narrows to, upper-cased; an option matches by its
/// own symbol or its underlying's.
struct SymbolFilter(Option<HashSet<String>>);

impl SymbolFilter {
    fn new(symbols: Option<&[String]>) -> Self {
        Self(symbols.filter(|s| !s.is_empty()).map(|symbols| {
            symbols
                .iter()
                .map(|s| s.trim().to_ascii_uppercase())
                .collect()
        }))
    }

    fn allows(&self, leg: &OrderLeg) -> bool {
        let Some(wanted) = &self.0 else {
            return true;
        };
        let instrument = &leg.instrument;
        wanted.contains(&instrument.symbol.trim().to_ascii_uppercase())
            || instrument
                .underlying_symbol
                .as_deref()
                .is_some_and(|u| wanted.contains(&u.trim().to_ascii_uppercase()))
    }

    fn names(&self) -> Option<Vec<&str>> {
        self.0.as_ref().map(|set| {
            let mut names: Vec<&str> = set.iter().map(String::as_str).collect();
            names.sort_unstable();
            names
        })
    }
}

/// The fills found in a set of orders.
#[derive(Debug, Default, PartialEq)]
struct Extraction {
    /// In time order; fills with the same time keep their orders' id order.
    fills: Vec<Fill>,
    /// Orders that contributed at least one fill.
    orders_with_fills: usize,
    /// Executions that could not become a fill: no price, quantity, time or
    /// leg, an instruction that is neither a buy nor a sell, or an asset
    /// this tool does not value.
    skipped: usize,
}

/// `order` and its children, each order once: Schwab lists the legs of a
/// trigger, bracket or one-cancels-other order nested under their parent.
fn flatten<'a>(order: &'a Order, seen: &mut HashSet<&'a str>, out: &mut Vec<&'a Order>) {
    if order.order_id.as_deref().is_some_and(|id| !seen.insert(id)) {
        return;
    }
    out.push(order);
    for child in &order.child_order_strategies {
        flatten(child, seen, out);
    }
}

/// The leg an execution leg belongs to: the one with its `legId`; Schwab
/// numbers legs from 1 in collection order, so a collection without ids is
/// read that way; and a single-leg order has only one leg to give.
fn resolve_leg(order: &Order, leg_id: Option<i64>) -> Option<&OrderLeg> {
    let legs = &order.order_leg_collection;
    if let Some(id) = leg_id {
        if let Some(leg) = legs.iter().find(|leg| leg.leg_id == Some(id)) {
            return Some(leg);
        }
        if legs.iter().all(|leg| leg.leg_id.is_none()) {
            return usize::try_from(id - 1).ok().and_then(|i| legs.get(i));
        }
    }
    match legs.as_slice() {
        [only] => Some(only),
        _ => None,
    }
}

/// Currency per price point per contract: shares and fund units are 1, an
/// option's the operator's verified multiplier, else Schwab's own, else 100.
/// `None` for an asset whose price is not per unit (futures, forex, bonds).
fn multiplier(leg: &OrderLeg, multipliers: &Multipliers) -> Option<Decimal> {
    match leg.instrument.asset_type {
        AssetType::Equity
        | AssetType::MutualFund
        | AssetType::CashEquivalent
        | AssetType::CollectiveInvestment => Some(Decimal::ONE),
        AssetType::Option => Some(
            multipliers
                .get(&leg.instrument.symbol)
                .copied()
                .filter(|m| *m > Decimal::ZERO)
                .or_else(|| {
                    leg.instrument
                        .option_multiplier
                        .filter(|m| *m > 0)
                        .map(Decimal::from)
                })
                .unwrap_or_else(|| Decimal::from(OPTION_MULTIPLIER)),
        ),
        _ => None,
    }
}

/// A Schwab number as a decimal, to the six places a price or share count
/// is quoted in.
fn decimal(value: f64) -> Option<Decimal> {
    Decimal::from_f64(value).map(|d| d.round_dp(6).normalize())
}

/// The fee on an execution leg: Schwab's order data carries none, but the
/// paper venue puts its fee there as `fee`. Absent or negative, zero.
fn fee_of(execution: &ExecutionLeg) -> Decimal {
    execution
        .extra
        .get("fee")
        .and_then(Value::as_f64)
        .filter(|fee| *fee >= 0.0)
        .and_then(decimal)
        .unwrap_or(Decimal::ZERO)
}

/// The fills of every execution in `orders` (children included) that the
/// filter allows. An execution counts when its activity is an `EXECUTION`
/// whose `executionType` is `FILL` or absent: `CANCELED` has been observed
/// there, with a zero price.
fn extract(orders: &[Order], multipliers: &Multipliers, filter: &SymbolFilter) -> Extraction {
    let mut seen = HashSet::new();
    let mut flat = Vec::new();
    for order in orders {
        flatten(order, &mut seen, &mut flat);
    }
    // (time, order id, position among the order's fills) fixes the order of
    // fills that share a second, which is all the time Schwab reports.
    let mut keyed: Vec<((i64, u64, usize), Fill)> = Vec::new();
    let mut extraction = Extraction::default();
    for order in flat {
        let entered = order
            .entered_time
            .as_deref()
            .and_then(|t| parse_schwab_time(t).ok());
        let order_key = order
            .order_id
            .as_deref()
            .and_then(|id| id.parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        let before = keyed.len();
        let executions = order
            .order_activity_collection
            .iter()
            .filter(|activity| {
                activity.activity_type == Some(OrderActivityType::Execution)
                    && activity
                        .execution_type
                        .as_deref()
                        .is_none_or(|kind| kind.eq_ignore_ascii_case("FILL"))
            })
            .flat_map(|activity| &activity.execution_legs);
        for execution in executions {
            let Some(leg) = resolve_leg(order, execution.leg_id) else {
                extraction.skipped += 1;
                continue;
            };
            if !filter.allows(leg) {
                continue;
            }
            let at = execution
                .time
                .as_deref()
                .and_then(|t| parse_schwab_time(t).ok())
                .or(entered);
            let fill = (|| {
                Some(Fill {
                    symbol: leg.instrument.symbol.clone(),
                    side: if leg.instruction.buys()? {
                        Side::Buy
                    } else {
                        Side::Sell
                    },
                    quantity: decimal(execution.quantity?).filter(|q| *q > Decimal::ZERO)?,
                    price: decimal(execution.price?).filter(|p| *p > Decimal::ZERO)?,
                    fee: fee_of(execution),
                    at_ms: at?.timestamp_millis(),
                    multiplier: multiplier(leg, multipliers)?,
                    order_id: order.order_id.clone(),
                    event_sequence: None,
                })
            })();
            match fill {
                Some(fill) => keyed.push(((fill.at_ms, order_key, keyed.len()), fill)),
                None => extraction.skipped += 1,
            }
        }
        if keyed.len() > before {
            extraction.orders_with_fills += 1;
        }
    }
    keyed.sort_by_key(|(key, _)| *key);
    extraction.fills = keyed.into_iter().map(|(_, fill)| fill).collect();
    extraction
}

/// Where a symbol's open lots were valued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkSource {
    Quote,
    LastFill,
}

impl MarkSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Quote => "quote",
            Self::LastFill => "last_fill",
        }
    }
}

/// The price a quote values a position at: the mark, else the midpoint of a
/// two-sided market, else the last trade. A paper quote the venue calls
/// stale (`fresh: false`) is no price.
fn mark_of(item: &QuoteItem) -> Option<Decimal> {
    if item.extra.get("fresh") == Some(&Value::Bool(false)) {
        return None;
    }
    let quote = item.quote.as_ref()?;
    let positive = |price: Option<f64>| price.filter(|p| *p > 0.0);
    let midpoint = match (positive(quote.bid_price), positive(quote.ask_price)) {
        (Some(bid), Some(ask)) => Some((bid + ask) / 2.0),
        _ => None,
    };
    positive(quote.mark)
        .or(midpoint)
        .or_else(|| positive(quote.last_price))
        .and_then(decimal)
}

/// Quotes for the open `symbols`; those the venue would not quote are
/// absent, and a failed read is a warning.
async fn marks(
    backend: &Backend,
    symbols: &[String],
    warnings: &mut Vec<String>,
) -> BTreeMap<String, Decimal> {
    let mut marks = BTreeMap::new();
    if symbols.is_empty() {
        return marks;
    }
    let quoted = &symbols[..symbols.len().min(MAX_MARKED_SYMBOLS)];
    if quoted.len() < symbols.len() {
        warnings.push(format!(
            "Only the first {MAX_MARKED_SYMBOLS} open symbols were quoted; the rest are valued \
             at their last fill price"
        ));
    }
    let refs: Vec<&str> = quoted.iter().map(String::as_str).collect();
    match backend.quotes(&refs, &[QuoteField::Quote], None).await {
        Ok(response) => {
            for symbol in quoted {
                if let Some(mark) = response.quotes.get(symbol).and_then(mark_of) {
                    marks.insert(symbol.clone(), mark);
                }
            }
        }
        Err(failure) => warnings.push(format!(
            "Open positions were not marked to market ({}); they are valued at their last fill \
             price",
            failure.message
        )),
    }
    marks
}

/// Net signed quantity by symbol across the account's positions.
fn held(positions: &[Position]) -> BTreeMap<String, f64> {
    let mut held: BTreeMap<String, f64> = BTreeMap::new();
    for position in positions {
        if let Some(instrument) = &position.instrument {
            *held.entry(instrument.symbol.clone()).or_default() +=
                position.long_quantity.unwrap_or(0.0) - position.short_quantity.unwrap_or(0.0);
        }
    }
    held
}

/// One warning for each symbol the window's fills leave open in a quantity
/// the account does not hold: the position, or part of it, predates the
/// window or moved without an order (an expiration, an assignment).
fn reconcile(report: &PerformanceReport, held: &BTreeMap<String, f64>) -> Vec<String> {
    report
        .symbols
        .iter()
        .filter_map(|(symbol, stats)| {
            let from_fills = stats.open_quantity.to_f64()?;
            let account = held.get(symbol).copied().unwrap_or(0.0);
            ((account - from_fills).abs() > QUANTITY_TOLERANCE).then(|| {
                format!(
                    "{symbol}: the account holds {account} but the window's fills leave \
                     {from_fills}; the position, or part of it, predates the window or moved \
                     without an order, so its profit and loss here is incomplete"
                )
            })
        })
        .collect()
}

fn analyze(
    fills: &[Fill],
    marks: &BTreeMap<String, Decimal>,
    starting_equity: Decimal,
) -> Result<PerformanceReport, ToolFailure> {
    let mut options = PerformanceOptions::new(starting_equity);
    options.marks = marks.clone();
    performance::analyze(fills, &[], &options)
        .map_err(|e| ToolFailure::internal("Performance analysis failed", e))
}

/// `snake_case` to `camelCase`, the casing of every other tool's payload.
fn camel_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut upper = false;
    for c in key.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// `value` with its object keys camel-cased, except the keys of any
/// `symbols` object, which are instrument symbols.
fn camel_case(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let value = match (key.as_str(), value) {
                        ("symbols", Value::Object(by_symbol)) => Value::Object(
                            by_symbol
                                .into_iter()
                                .map(|(symbol, report)| (symbol, camel_case(report)))
                                .collect(),
                        ),
                        (_, value) => camel_case(value),
                    };
                    (camel_key(&key), value)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(camel_case).collect()),
        other => other,
    }
}

/// Fields measured against the starting equity.
const EQUITY_FIELDS: &[&str] = &[
    "start_equity",
    "end_equity",
    "net_return",
    "max_drawdown",
    "annualized_return",
    "volatility",
    "sharpe",
    "sortino",
    "calmar",
];

/// The report as the tool returns it: the SDK's figures, camel-cased, with
/// the daily series and the benchmark (nothing to compare against) left out
/// and the round trips cut to the most recent [`MAX_ROUND_TRIPS`].
fn payload(
    report: &PerformanceReport,
    equity_known: bool,
) -> Result<Map<String, Value>, ToolFailure> {
    let Value::Object(mut report_json) = serde_json::to_value(report)
        .map_err(|e| ToolFailure::internal("Performance report serialization", e))?
    else {
        unreachable!("the report serializes as an object")
    };
    for field in ["daily", "benchmark_return", "excess_return"] {
        report_json.remove(field);
    }
    if !equity_known {
        for field in EQUITY_FIELDS {
            report_json.insert((*field).to_owned(), Value::Null);
        }
    }
    let total = report.round_trips.len();
    let listed = &report.round_trips[total.saturating_sub(MAX_ROUND_TRIPS)..];
    report_json.insert(
        "round_trips".into(),
        serde_json::to_value(listed)
            .map_err(|e| ToolFailure::internal("Round trip serialization", e))?,
    );
    let Value::Object(mut out) = camel_case(Value::Object(report_json)) else {
        unreachable!("camel_case keeps an object an object")
    };
    out.insert("roundTripsTotal".into(), total.into());
    out.insert("roundTripsOmitted".into(), (total - listed.len()).into());
    Ok(out)
}

/// The tool's answer, enveloped with the backend's provenance.
///
/// `multipliers` are the principal's verified option multipliers; the
/// unauthorized transport has none.
pub async fn collect(
    backend: &Backend,
    request: Request,
    multipliers: &Multipliers,
) -> Result<Value, ToolFailure> {
    let now = backend.now().await?;
    // The paper venue keeps every order, so only Schwab's list has a horizon.
    let window = Window::resolve(&request, now, matches!(backend, Backend::Live(_)))?;
    let params = OrdersParams::new(window.since, window.until);
    params.validate()?;
    let orders = backend
        .account_orders(&request.account_hash, &params)
        .await?;

    let mut warnings = Vec::new();
    if orders.len() >= ORDER_PAGE {
        warnings.push(format!(
            "Schwab returned {} orders, its page size: older orders in the window may be missing; \
             narrow `since`",
            orders.len()
        ));
    }
    let filter = SymbolFilter::new(request.symbols.as_deref());
    let extraction = extract(&orders, multipliers, &filter);

    // The account is read for what the report is measured against and
    // checked with; the orders alone still answer without it.
    let account = match backend.account(&request.account_hash, true).await {
        Ok(account) => Some(account.securities_account),
        Err(failure) => {
            warnings.push(format!("The account was not read: {}", failure.message));
            None
        }
    };

    // A first pass finds what is open, a second values it at quotes, and
    // the third measures it against the equity the window started from.
    let unmarked = analyze(&extraction.fills, &BTreeMap::new(), Decimal::ONE)?;
    let open: Vec<String> = unmarked
        .symbols
        .iter()
        .filter(|(_, stats)| !stats.open_quantity.is_zero())
        .map(|(symbol, _)| symbol.clone())
        .collect();
    let quotes = marks(backend, &open, &mut warnings).await;
    let marked = analyze(&extraction.fills, &quotes, Decimal::ONE)?;

    let liquidation = account
        .as_ref()
        .and_then(|a| a.current_balances.as_ref())
        .and_then(|b| b.liquidation_value)
        .and_then(decimal);
    let starting = liquidation
        .and_then(|value| value.checked_sub(marked.net_pnl))
        .filter(|start| *start > Decimal::ZERO);
    if starting.is_none() {
        warnings.push(
            "No starting equity could be derived (the account's liquidation value was missing \
             or less than the window's profit): returns, drawdown fraction and ratios are null"
                .to_owned(),
        );
    }
    let report = match starting {
        Some(start) => analyze(&extraction.fills, &quotes, start)?,
        None => marked,
    };

    if let Some(account) = &account {
        warnings.extend(reconcile(
            &report,
            &held(account.positions.as_deref().unwrap_or_default()),
        ));
    }

    let mark_sources: Map<String, Value> = report
        .symbols
        .iter()
        .filter(|(_, stats)| stats.mark.is_some())
        .map(|(symbol, stats)| {
            let source = if quotes.contains_key(symbol) {
                MarkSource::Quote
            } else {
                MarkSource::LastFill
            };
            (
                symbol.clone(),
                json!({"price": stats.mark, "source": source.as_str()}),
            )
        })
        .collect();

    let mut out = payload(&report, starting.is_some())?;
    out.insert(
        "window".into(),
        json!({
            "since": window.since.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "until": window.until.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "clampedToOrderHorizon": window.clamped,
        }),
    );
    out.insert("symbolsFilter".into(), json!(filter.names()));
    out.insert("ordersRead".into(), orders.len().into());
    out.insert(
        "ordersWithFills".into(),
        extraction.orders_with_fills.into(),
    );
    out.insert("fills".into(), extraction.fills.len().into());
    out.insert("skippedExecutions".into(), extraction.skipped.into());
    out.insert("marks".into(), Value::Object(mark_sources));
    out.insert("warnings".into(), json!(warnings));
    out.insert("notes".into(), NOTES.into());
    Ok(crate::contract::envelope(
        backend.source(),
        Value::Object(out),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(v: &str) -> Decimal {
        v.parse().unwrap()
    }

    fn orders(value: Value) -> Vec<Order> {
        serde_json::from_value(value).unwrap()
    }

    fn all() -> SymbolFilter {
        SymbolFilter::new(None)
    }

    fn execution(legs: Value) -> Value {
        json!({"activityType": "EXECUTION", "executionType": "FILL", "executionLegs": legs})
    }

    #[test]
    fn executions_become_fills_mapped_to_their_legs_in_time_order() {
        let found = extract(
            &orders(json!([
                {"orderId": 12, "enteredTime": "2026-01-05T15:00:00+0000",
                 "orderLegCollection": [
                    {"legId": 1, "instruction": "SELL_TO_CLOSE", "quantity": 4,
                     "instrument": {"assetType": "EQUITY", "symbol": "ABC"}}],
                 "orderActivityCollection": [execution(json!([
                    {"legId": 1, "quantity": 4, "price": 12.5, "fee": 0.04,
                     "time": "2026-01-05T15:00:01+0000"}]))]},
                {"orderId": 11, "enteredTime": "2026-01-05T14:00:00+0000",
                 "orderLegCollection": [
                    {"legId": 1, "instruction": "BUY", "quantity": 4,
                     "instrument": {"assetType": "EQUITY", "symbol": "ABC"}}],
                 "orderActivityCollection": [
                    execution(json!([{"legId": 1, "quantity": 1, "price": 10.01,
                                      "time": "2026-01-05T14:00:05+0000"}])),
                    execution(json!([{"legId": 1, "quantity": 3, "price": 10.02,
                                      "time": "2026-01-05T14:00:09+0000"}]))]},
            ])),
            &Multipliers::new(),
            &all(),
        );
        assert_eq!(found.orders_with_fills, 2);
        assert_eq!(found.skipped, 0);
        let summary: Vec<_> = found
            .fills
            .iter()
            .map(|f| (f.side, f.quantity, f.price, f.order_id.as_deref()))
            .collect();
        assert_eq!(
            summary,
            [
                (Side::Buy, d("1"), d("10.01"), Some("11")),
                (Side::Buy, d("3"), d("10.02"), Some("11")),
                (Side::Sell, d("4"), d("12.5"), Some("12")),
            ]
        );
        assert!(found.fills.iter().all(|f| f.multiplier == Decimal::ONE));
        // Schwab sends no fee; the paper venue's rides on the execution leg.
        let fees: Vec<_> = found.fills.iter().map(|f| f.fee).collect();
        assert_eq!(fees, [d("0"), d("0"), d("0.04")]);
        assert!(found.fills[0].at_ms < found.fills[1].at_ms);
    }

    #[test]
    fn a_fill_of_every_leg_of_a_spread_and_of_nested_children_is_found_once() {
        let spread = json!({
            "orderId": 20, "orderStrategyType": "TRIGGER",
            "orderLegCollection": [
                {"legId": 1, "instruction": "BUY_TO_OPEN", "quantity": 2,
                 "instrument": {"assetType": "OPTION", "symbol": "ABC  260116C00100000",
                                "underlyingSymbol": "ABC"}},
                {"legId": 2, "instruction": "SELL_TO_OPEN", "quantity": 2,
                 "instrument": {"assetType": "OPTION", "symbol": "ABC  260116C00110000",
                                "underlyingSymbol": "ABC"}}],
            "orderActivityCollection": [execution(json!([
                {"legId": 2, "quantity": 2, "price": 1.0, "time": "2026-01-05T14:00:00+0000"},
                {"legId": 1, "quantity": 2, "price": 3.5, "time": "2026-01-05T14:00:00+0000"}]))],
            "childOrderStrategies": [
                {"orderId": 21,
                 "orderLegCollection": [
                    {"legId": 1, "instruction": "SELL_TO_CLOSE", "quantity": 2,
                     "instrument": {"assetType": "OPTION", "symbol": "ABC  260116C00100000"}}],
                 "orderActivityCollection": [execution(json!([
                    {"legId": 1, "quantity": 2, "price": 4.0,
                     "time": "2026-01-06T14:00:00+0000"}]))]}]
        });
        // The same child also listed on its own must not be counted twice.
        let listed = orders(json!([spread, {"orderId": 21}]));
        let found = extract(&listed, &Multipliers::new(), &all());
        assert_eq!(found.fills.len(), 3);
        assert_eq!(found.orders_with_fills, 2);
        // Options carry the standard multiplier by default.
        assert!(found.fills.iter().all(|f| f.multiplier == d("100")));
        // The spread's two fills share a time and stay in the order Schwab
        // listed them, each on the leg its id names.
        assert_eq!(found.fills[0].symbol, "ABC  260116C00110000");
        assert_eq!(found.fills[0].side, Side::Sell);
        assert_eq!(found.fills[1].symbol, "ABC  260116C00100000");
        assert_eq!(found.fills[1].side, Side::Buy);
        assert_eq!(found.fills[2].side, Side::Sell);

        // A verified multiplier wins over Schwab's, which wins over 100.
        let verified = Multipliers::from([("ABC  260116C00100000".to_owned(), d("10"))]);
        let found = extract(&listed, &verified, &all());
        assert_eq!(found.fills[0].multiplier, d("100"));
        assert_eq!(found.fills[1].multiplier, d("10"));
        let adjusted = orders(json!([{
            "orderId": 30,
            "orderLegCollection": [{"legId": 1, "instruction": "BUY_TO_OPEN", "quantity": 1,
                "instrument": {"assetType": "OPTION", "symbol": "XYZ1 260116C00050000",
                               "optionMultiplier": 150}}],
            "orderActivityCollection": [execution(json!([
                {"legId": 1, "quantity": 1, "price": 2.0, "time": "2026-01-05T14:00:00+0000"}]))]
        }]));
        let found = extract(&adjusted, &Multipliers::new(), &all());
        assert_eq!(found.fills[0].multiplier, d("150"));
    }

    #[test]
    fn a_missing_leg_id_falls_back_to_the_only_or_the_numbered_leg() {
        let one = json!({"instruction": "BUY", "quantity": 1,
                         "instrument": {"assetType": "EQUITY", "symbol": "ONE"}});
        let found = extract(
            &orders(json!([
                {"orderId": 1, "orderLegCollection": [one],
                 "orderActivityCollection": [execution(json!([
                    {"quantity": 1, "price": 5.0, "time": "2026-01-05T14:00:00+0000"}]))]},
                {"orderId": 2,
                 "orderLegCollection": [
                    {"instruction": "BUY", "quantity": 1,
                     "instrument": {"assetType": "EQUITY", "symbol": "A"}},
                    {"instruction": "SELL", "quantity": 1,
                     "instrument": {"assetType": "EQUITY", "symbol": "B"}}],
                 "orderActivityCollection": [execution(json!([
                    {"legId": 2, "quantity": 1, "price": 6.0,
                     "time": "2026-01-05T14:00:01+0000"},
                    // No id on a two-leg order says nothing about which leg.
                    {"quantity": 1, "price": 7.0, "time": "2026-01-05T14:00:02+0000"}]))]},
            ])),
            &Multipliers::new(),
            &all(),
        );
        let symbols: Vec<_> = found.fills.iter().map(|f| f.symbol.as_str()).collect();
        assert_eq!(symbols, ["ONE", "B"]);
        assert_eq!(found.skipped, 1);
    }

    #[test]
    fn only_filled_executions_with_usable_numbers_count() {
        let leg = json!([{"legId": 1, "instruction": "BUY", "quantity": 5,
                          "instrument": {"assetType": "EQUITY", "symbol": "ABC"}}]);
        let at = "2026-01-05T14:00:00+0000";
        let found = extract(
            &orders(json!([{
                "orderId": 1, "enteredTime": at, "orderLegCollection": leg,
                "orderActivityCollection": [
                    {"activityType": "ORDER_ACTION", "executionType": "FILL",
                     "executionLegs": [{"legId": 1, "quantity": 5, "price": 9.0, "time": at}]},
                    execution(json!([{"legId": 1, "quantity": 5, "price": 0.0, "time": at}])),
                    {"activityType": "EXECUTION", "executionType": "CANCELED",
                     "executionLegs": [{"legId": 1, "quantity": 5, "price": 9.0, "time": at}]},
                    execution(json!([{"legId": 1, "price": 9.0, "time": at}])),
                    // No `executionType` is a fill, as `get_order_status` reads it.
                    {"activityType": "EXECUTION",
                     "executionLegs": [{"legId": 1, "quantity": 2, "price": 9.5, "time": at}]},
                    // No time of its own: the order's entry time stands in.
                    execution(json!([{"legId": 1, "quantity": 3, "price": 9.25}])),
                ]
            }])),
            &Multipliers::new(),
            &all(),
        );
        let fills: Vec<_> = found.fills.iter().map(|f| (f.quantity, f.price)).collect();
        assert_eq!(fills, [(d("2"), d("9.5")), (d("3"), d("9.25"))]);
        // The zero price and the missing quantity; the other two never counted.
        assert_eq!(found.skipped, 2);
    }

    #[test]
    fn unvalued_assets_unknown_instructions_and_other_symbols_are_set_aside() {
        let book = orders(json!([
            {"orderId": 1,
             "orderLegCollection": [
                {"legId": 1, "instruction": "BUY", "quantity": 1,
                 "instrument": {"assetType": "FUTURE", "symbol": "/ES"}}],
             "orderActivityCollection": [execution(json!([
                {"legId": 1, "quantity": 1, "price": 5000.0, "time": "2026-01-05T14:00:00+0000"}]))]},
            {"orderId": 2,
             "orderLegCollection": [
                {"legId": 1, "instruction": "EXCHANGE", "quantity": 1,
                 "instrument": {"assetType": "EQUITY", "symbol": "ABC"}}],
             "orderActivityCollection": [execution(json!([
                {"legId": 1, "quantity": 1, "price": 5.0, "time": "2026-01-05T14:00:00+0000"}]))]},
            {"orderId": 3,
             "orderLegCollection": [
                {"legId": 1, "instruction": "BUY", "quantity": 1,
                 "instrument": {"assetType": "OPTION", "symbol": "ABC  260116C00100000",
                                "underlyingSymbol": "ABC"}},
                {"legId": 2, "instruction": "BUY", "quantity": 1,
                 "instrument": {"assetType": "EQUITY", "symbol": "XYZ"}}],
             "orderActivityCollection": [execution(json!([
                {"legId": 1, "quantity": 1, "price": 2.0, "time": "2026-01-05T14:00:00+0000"},
                {"legId": 2, "quantity": 1, "price": 8.0, "time": "2026-01-05T14:00:00+0000"}]))]},
        ]));
        let found = extract(&book, &Multipliers::new(), &all());
        assert_eq!(found.skipped, 2);
        assert_eq!(found.fills.len(), 2);

        // A symbol filter matches an option by its underlying, ignoring case.
        let found = extract(
            &book,
            &Multipliers::new(),
            &SymbolFilter::new(Some(&["abc ".into()])),
        );
        let symbols: Vec<_> = found.fills.iter().map(|f| f.symbol.as_str()).collect();
        assert_eq!(symbols, ["ABC  260116C00100000"]);
        let found = extract(
            &book,
            &Multipliers::new(),
            &SymbolFilter::new(Some(&["XYZ".into()])),
        );
        assert_eq!(found.fills.len(), 1);
        assert!(SymbolFilter::new(Some(&[])).0.is_none());
    }

    #[test]
    fn the_window_defaults_to_thirty_days_and_stays_inside_schwabs_horizon() {
        let at = |text: &str| {
            DateTime::parse_from_rfc3339(text)
                .unwrap()
                .with_timezone(&Utc)
        };
        let now = at("2026-03-01T12:00:00Z");
        let request = |extra: Value| {
            let mut args = json!({"account_hash": "A"});
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            Request::parse(args).unwrap()
        };

        let bare = Window::resolve(&request(json!({})), now, true).unwrap();
        assert_eq!(bare.until, now);
        assert_eq!(bare.since, at("2026-01-30T12:00:00Z"));
        assert!(!bare.clamped);

        let long = request(json!({"since": "2025-06-01"}));
        let clamped = Window::resolve(&long, now, true).unwrap();
        assert_eq!(clamped.since, now - Duration::days(ORDER_LOOKBACK_DAYS));
        assert!(clamped.clamped);
        // The paper venue has no such horizon.
        let paper = Window::resolve(&long, now, false).unwrap();
        assert_eq!(paper.since, at("2025-06-01T00:00:00Z"));
        assert!(!paper.clamped);

        let ending = request(json!({"until": "2026-02-10"}));
        let w = Window::resolve(&ending, now, true).unwrap();
        assert_eq!(w.until, at("2026-02-10T23:59:59.999Z"));
        assert_eq!(w.since, w.until - Duration::days(DEFAULT_WINDOW_DAYS));

        let inverted = request(json!({"since": "2026-02-10", "until": "2026-02-01"}));
        assert!(Window::resolve(&inverted, now, true).is_err());
        let ancient = request(json!({"since": "2025-01-01", "until": "2025-02-01"}));
        let refused = Window::resolve(&ancient, now, true).unwrap_err();
        assert!(refused.message.contains("until"), "{}", refused.message);
        assert!(Request::parse(json!({"since": "2026-02-10"})).is_err());
    }

    #[test]
    fn keys_are_camel_cased_except_instrument_symbols() {
        let cased = camel_case(json!({
            "net_pnl": 1,
            "symbols": {"BRK_B": {"open_quantity": 2, "trades": {"win_rate": 0.5}}},
            "round_trips": [{"entry_at_ms": 1}],
        }));
        assert_eq!(
            cased,
            json!({
                "netPnl": 1,
                "symbols": {"BRK_B": {"openQuantity": 2, "trades": {"winRate": 0.5}}},
                "roundTrips": [{"entryAtMs": 1}],
            })
        );
    }

    #[test]
    fn a_report_lists_the_latest_round_trips_and_nulls_what_needs_equity() {
        let at = |i: i64| 1_767_625_200_000 + i * 60_000;
        let mut fills = Vec::new();
        for i in 0..(MAX_ROUND_TRIPS as i64 + 5) {
            for (side, price, offset) in [(Side::Buy, "10", 0), (Side::Sell, "11", 1)] {
                fills.push(Fill {
                    symbol: "ABC".into(),
                    side,
                    quantity: Decimal::ONE,
                    price: d(price),
                    fee: Decimal::ZERO,
                    at_ms: at(i * 2 + offset),
                    multiplier: Decimal::ONE,
                    order_id: None,
                    event_sequence: None,
                });
            }
        }
        let report = analyze(&fills, &BTreeMap::new(), d("1000")).unwrap();
        let known = payload(&report, true).unwrap();
        assert_eq!(known["roundTripsTotal"], 105);
        assert_eq!(known["roundTripsOmitted"], 5);
        let listed = known["roundTrips"].as_array().unwrap();
        assert_eq!(listed.len(), MAX_ROUND_TRIPS);
        assert_eq!(listed[0]["entryAtMs"], at(10));
        assert_eq!(known["realizedPnl"], "105");
        assert_eq!(known["trades"]["winRate"], 1.0);
        assert_eq!(known["startEquity"], "1000");
        assert!(known.get("daily").is_none() && known.get("excessReturn").is_none());
        assert_eq!(known["symbols"]["ABC"]["openQuantity"], "0");

        let unknown = payload(&report, false).unwrap();
        for field in [
            "startEquity",
            "netReturn",
            "maxDrawdown",
            "sharpe",
            "calmar",
        ] {
            assert_eq!(unknown[field], Value::Null, "{field}");
        }
        assert_eq!(unknown["realizedPnl"], "105");
    }

    #[test]
    fn holdings_the_fills_do_not_explain_are_reported() {
        let fill = |side, quantity: &str| Fill {
            symbol: "ABC".into(),
            side,
            quantity: d(quantity),
            price: d("10"),
            fee: Decimal::ZERO,
            at_ms: 1_767_625_200_000,
            multiplier: Decimal::ONE,
            order_id: None,
            event_sequence: None,
        };
        let report = analyze(&[fill(Side::Buy, "5")], &BTreeMap::new(), d("100")).unwrap();
        assert!(reconcile(&report, &BTreeMap::from([("ABC".to_owned(), 5.0)])).is_empty());
        let off = reconcile(&report, &BTreeMap::from([("ABC".to_owned(), 8.0)]));
        assert_eq!(off.len(), 1);
        assert!(off[0].starts_with("ABC: the account holds 8"), "{off:?}");
        assert_eq!(reconcile(&report, &BTreeMap::new()).len(), 1);

        let positions: Vec<Position> = serde_json::from_value(json!([
            {"longQuantity": 5.0, "instrument": {"assetType": "EQUITY", "symbol": "ABC"}},
            {"shortQuantity": 2.0, "instrument": {"assetType": "EQUITY", "symbol": "XYZ"}},
        ]))
        .unwrap();
        assert_eq!(
            held(&positions),
            BTreeMap::from([("ABC".to_owned(), 5.0), ("XYZ".to_owned(), -2.0)])
        );
    }

    #[test]
    fn a_quote_marks_by_mark_then_midpoint_then_last_and_a_stale_one_not_at_all() {
        let item = |value: Value| serde_json::from_value::<QuoteItem>(value).unwrap();
        assert_eq!(
            mark_of(&item(
                json!({"quote": {"mark": 12.5, "bidPrice": 1.0, "askPrice": 3.0}})
            )),
            Some(d("12.5"))
        );
        assert_eq!(
            mark_of(&item(
                json!({"quote": {"bidPrice": 10.0, "askPrice": 10.02, "lastPrice": 9.0}})
            )),
            Some(d("10.01"))
        );
        assert_eq!(
            mark_of(&item(
                json!({"quote": {"bidPrice": 0.0, "askPrice": 10.0, "lastPrice": 9.0}})
            )),
            Some(d("9"))
        );
        assert_eq!(mark_of(&item(json!({"quote": {"mark": 0.0}}))), None);
        assert_eq!(
            mark_of(&item(json!({"fresh": false, "quote": {"mark": 12.5}}))),
            None
        );
    }
}
