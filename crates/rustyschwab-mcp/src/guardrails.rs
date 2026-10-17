//! A principal's trading guardrails beyond its per-order caps: which
//! symbols it may trade, how far through the market it may price a limit
//! order, and how much it may submit in a trading day.
//!
//! - **Symbols.** `allowed_symbols` (empty: any) and `denied_symbols` name
//!   equities, and options by their underlying or their own OSI symbol.
//!   Every leg of every order in the strategy is judged; a denial wins.
//! - **Price collar.** `price_collar_bps` refuses a limit order priced more
//!   than that many basis points through the far side of the market: a buy
//!   above the ask, or a sell below the bid, by more than the collar. A
//!   passive price is never refused. The quote is read when the order is
//!   judged; with no two-sided market the mark, then the last trade, stands
//!   in, and with neither the order is refused.
//! - **Daily budget.** `max_daily_orders` and `max_daily_notional` bound the
//!   submissions (`place_order` and `replace_order`) a principal starts in
//!   a New York trading day, by the wall clock, with each order's bounded
//!   notional as the policy computes it. A submission that produced no
//!   order (refused here or by the venue) does not count. The budget is judged after the
//!   submission's audit record opens: a principal has one submission in
//!   flight at a time, so two cannot both take the last of the budget.
use crate::contract::{FailureCode, ToolFailure};
use crate::journal::Dispatch;
use crate::policy::{self, PrincipalPolicy};
use rust_decimal::Decimal;
use rustyschwab::broker::TradingBackend;
use rustyschwab::execution::{OrderJournal, SubmissionOutcome, SubmissionRecord};
use rustyschwab::{Error, Result};
use rustyschwab_types::common::AssetType;
use rustyschwab_types::trading::{OptionSymbol, OrderRequest, OrderRequestLeg, OrderType};
use serde::Serialize;
use serde_json::Value;

/// Every leg of `order` and of its child strategies.
fn legs(order: &OrderRequest) -> Vec<&OrderRequestLeg> {
    let mut legs: Vec<&OrderRequestLeg> = order.order_leg_collection.iter().collect();
    for child in &order.child_order_strategies {
        legs.extend(self::legs(child));
    }
    legs
}

/// The names a symbol list entry may give `leg` by: its own symbol, and an
/// option's underlying.
fn names(leg: &OrderRequestLeg) -> (&str, Option<String>) {
    let symbol = leg.instrument.symbol.as_str();
    let underlying = (leg.instrument.asset_type == AssetType::Option)
        .then(|| symbol.parse::<OptionSymbol>().ok())
        .flatten()
        .map(|option| option.underlying().to_owned());
    (symbol, underlying)
}

/// The first leg of `order` the principal's symbol lists refuse.
pub(crate) fn symbol_denial(policy: &PrincipalPolicy, order: &OrderRequest) -> Option<ToolFailure> {
    legs(order).into_iter().find_map(|leg| {
        let (symbol, underlying) = names(leg);
        let listed = |list: &std::collections::HashSet<String>| {
            list.contains(symbol) || underlying.as_ref().is_some_and(|u| list.contains(u))
        };
        if listed(&policy.denied_symbols) {
            Some(ToolFailure::not_permitted(format!(
                "{symbol} is on this principal's denied symbols"
            )))
        } else if !policy.allowed_symbols.is_empty() && !listed(&policy.allowed_symbols) {
            Some(ToolFailure::not_permitted(format!(
                "{symbol} is not among this principal's allowed symbols"
            )))
        } else {
            None
        }
    })
}

/// Validate a symbol list: upper-case symbols without surrounding space.
pub(crate) fn validate_symbols(
    what: &str,
    symbols: &std::collections::HashSet<String>,
) -> anyhow::Result<()> {
    for symbol in symbols {
        anyhow::ensure!(
            !symbol.is_empty() && symbol.trim() == symbol && symbol.to_ascii_uppercase() == *symbol,
            "{what} entries must be upper-case symbols, got {symbol:?}"
        );
    }
    Ok(())
}

/// The principal's price collar on the order in `args`, when it sets one
/// and the order is valid (an invalid one is refused for that). A refusal
/// is [`Error::RiskDenied`]; a venue that cannot quote fails as it does.
pub(crate) async fn check_collar(
    policy: &PrincipalPolicy,
    venue: &dyn TradingBackend,
    args: &Value,
) -> Result<()> {
    let Some(bps) = policy.price_collar_bps else {
        return Ok(());
    };
    match policy::order_request(args) {
        Ok(order) => collar(venue, &order, bps.get()).await,
        Err(_) => Ok(()),
    }
}

/// Refuse a limit order in `order` priced more than `bps` basis points
/// through the far side of the market, reading the quotes from `venue`.
async fn collar(venue: &dyn TradingBackend, order: &OrderRequest, bps: u32) -> Result<()> {
    // Basis points: ten thousandths.
    let band = Decimal::new(i64::from(bps), 4);
    for node in limit_orders(order) {
        let [leg] = node.order_leg_collection.as_slice() else {
            return Err(Error::RiskDenied(
                "The price collar judges single-leg limit orders only".into(),
            ));
        };
        let (Some(price), Some(buys)) = (node.price, leg.instruction.buys()) else {
            continue;
        };
        let symbol = leg.instrument.symbol.as_str();
        let touch = venue.touch(symbol).await.inspect_err(|e| {
            tracing::warn!(%symbol, error = %e, "the price collar could not read a quote");
        })?;
        let Some(reference) = touch.and_then(|touch| touch.reference(buys)) else {
            return Err(Error::RiskDenied(format!(
                "The price collar has no market for {symbol} to judge the price against"
            )));
        };
        let (bound, through) = if buys {
            let bound = reference * (Decimal::ONE + band);
            (bound, price > bound)
        } else {
            let bound = reference * (Decimal::ONE - band);
            (bound, price < bound)
        };
        if through {
            return Err(Error::RiskDenied(format!(
                "Limit {price} to {side} {symbol} is more than {bps} bps through the market \
                     ({reference} {touch}); the collar allows {limit} {bound}",
                side = if buys { "buy" } else { "sell" },
                touch = if buys { "ask" } else { "bid" },
                limit = if buys { "at most" } else { "at least" },
                bound = bound.round_dp(4).normalize(),
            )));
        }
    }
    Ok(())
}

/// The limit orders in `order` the collar judges: the order and its child
/// strategies, at every depth. A child waits for its trigger, but it then
/// prices against a market near this one.
fn limit_orders(order: &OrderRequest) -> Vec<&OrderRequest> {
    let mut found = Vec::new();
    if order.order_type == Some(OrderType::Limit) {
        found.push(order);
    }
    for child in &order.child_order_strategies {
        found.extend(limit_orders(child));
    }
    found
}

/// What a principal has submitted in the current trading day.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayUsage {
    pub orders: u32,
    pub notional: Decimal,
}

/// The principal's daily budget and what it has used, as `check_order` and
/// `get_server_status` report it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyBudget {
    pub max_orders: Option<u32>,
    pub max_notional: Option<Decimal>,
    pub used: DayUsage,
}

/// Whether `policy` sets a daily budget.
pub(crate) fn has_budget(policy: &PrincipalPolicy) -> bool {
    policy.max_daily_orders.is_some() || policy.max_daily_notional.is_some()
}

/// Whether a submission record counts against the budget: anything that
/// may have produced an order. A submission the MCP path answered with a
/// definite failure (anything but `reconciliation_required` or an internal
/// fault), or the gateway recorded as rejected, produced none. The
/// dead-man switch reads the same records for the orders to cancel.
pub(crate) fn counts(record: &SubmissionRecord) -> bool {
    if !matches!(record.operation.as_str(), "place_order" | "replace_order") {
        return false;
    }
    match &record.outcome {
        Some(SubmissionOutcome::Rejected) => false,
        Some(SubmissionOutcome::Reconciled { evidence }) => !matches!(
            Dispatch::parse(evidence),
            Some(Dispatch::Failed { code: Some(code) })
                if !matches!(code, FailureCode::ReconciliationRequired | FailureCode::Internal)
        ),
        _ => true,
    }
}

/// The principal's submissions in the current New York trading day, less
/// `except` (the submission being judged).
pub(crate) async fn day_usage(
    audit: &OrderJournal,
    policy: &PrincipalPolicy,
    except: Option<&str>,
) -> Result<DayUsage> {
    let since = rustyschwab::paper::feed::trading_day_start(chrono::Utc::now());
    let records = audit
        .records_since(&policy.name, since.timestamp_millis())
        .await?;
    let mut usage = DayUsage::default();
    for record in records
        .iter()
        .filter(|r| Some(r.attempt_id.as_str()) != except && counts(r))
    {
        usage.orders = usage.orders.saturating_add(1);
        usage.notional = usage
            .notional
            .saturating_add(notional(policy, &record.request["arguments"]));
    }
    Ok(usage)
}

/// The bounded notional of the order in `args`, as the policy computes it;
/// zero for one it cannot price (which it would not have dispatched).
pub(crate) fn notional(policy: &PrincipalPolicy, args: &Value) -> Decimal {
    policy::order_request(args)
        .ok()
        .and_then(|order| policy::exposure(&order, &policy.option_multipliers).ok())
        .map_or(Decimal::ZERO, |(_, notional)| notional)
}

/// Refuse a submission of `notional` that would take the principal past
/// its daily budget, given what it has `used`.
pub(crate) fn within_budget(
    policy: &PrincipalPolicy,
    used: DayUsage,
    notional: Decimal,
) -> Result<()> {
    if let Some(max) = policy.max_daily_orders
        && used.orders >= max.get()
    {
        return Err(Error::RiskDenied(format!(
            "This principal has submitted its {max} orders for the trading day; \
             the budget resets at midnight New York time"
        )));
    }
    if let Some(max) = policy.max_daily_notional
        && used.notional.saturating_add(notional) > max
    {
        return Err(Error::RiskDenied(format!(
            "Order notional {notional} would take this principal past its daily \
             {max} (used {used}); the budget resets at midnight New York time",
            used = used.notional
        )));
    }
    Ok(())
}

/// Refuse submission `attempt` of `operation` with `args` if it would take
/// the principal past its daily budget. Call it with the submission's audit
/// record open, so no other submission of the principal is in flight.
pub(crate) async fn check_budget(
    policy: &PrincipalPolicy,
    audit: &OrderJournal,
    operation: &str,
    args: &Value,
    attempt: &str,
) -> Result<()> {
    if !matches!(operation, "place_order" | "replace_order") || !has_budget(policy) {
        return Ok(());
    }
    let used = day_usage(audit, policy, Some(attempt)).await?;
    within_budget(policy, used, notional(policy, args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(extra: Value) -> PrincipalPolicy {
        let mut base = json!({"name": "desk", "token_sha256": "a".repeat(64),
            "tools": ["place_order"], "accounts": ["A"], "max_quantity": "100",
            "max_notional": "100000", "option_multipliers": {"AAPL  250117C00150000": "100"}});
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    fn order(symbol: &str, asset: &str, instruction: &str, price: &str) -> OrderRequest {
        serde_json::from_value(json!({"orderStrategyType": "SINGLE", "orderType": "LIMIT",
            "session": "NORMAL", "duration": "DAY", "price": price,
            "orderLegCollection": [{"quantity": "1", "instruction": instruction,
                "instrument": {"symbol": symbol, "assetType": asset}}]}))
        .unwrap()
    }

    #[test]
    fn symbol_lists_match_equities_and_option_underlyings() {
        let allow = policy(json!({"allowed_symbols": ["AAPL"], "denied_symbols": ["TSLA"]}));
        assert!(symbol_denial(&allow, &order("AAPL", "EQUITY", "BUY", "1")).is_none());
        let option = order("AAPL  250117C00150000", "OPTION", "BUY_TO_OPEN", "1");
        assert!(
            symbol_denial(&allow, &option).is_none(),
            "an option matches by its underlying"
        );
        let other = symbol_denial(&allow, &order("MSFT", "EQUITY", "BUY", "1")).unwrap();
        assert_eq!(other.code, FailureCode::NotPermitted);
        assert!(other.message.contains("not among"), "{}", other.message);
        let denied = symbol_denial(&allow, &order("TSLA", "EQUITY", "BUY", "1")).unwrap();
        assert!(denied.message.contains("denied"), "{}", denied.message);

        // A denial wins over an allowance, and reaches child strategies.
        let both = policy(json!({"allowed_symbols": ["AAPL"], "denied_symbols": ["AAPL"]}));
        assert!(symbol_denial(&both, &order("AAPL", "EQUITY", "BUY", "1")).is_some());
        let mut bracket = order("AAPL", "EQUITY", "BUY", "1");
        bracket.order_strategy_type = rustyschwab_types::trading::OrderStrategyType::Trigger;
        bracket.child_order_strategies = vec![order("TSLA", "EQUITY", "SELL", "1")];
        assert!(symbol_denial(&allow, &bracket).is_some());
        assert!(symbol_denial(&policy(json!({})), &bracket).is_none());
    }

    #[test]
    fn symbol_lists_must_be_upper_case() {
        let ok = ["AAPL".to_owned()].into_iter().collect();
        assert!(validate_symbols("allowed_symbols", &ok).is_ok());
        for bad in ["aapl", " AAPL", ""] {
            let set = [bad.to_owned()].into_iter().collect();
            assert!(
                validate_symbols("allowed_symbols", &set).is_err(),
                "{bad:?}"
            );
        }
    }

    fn record(
        operation: &str,
        args: Value,
        outcome: Option<SubmissionOutcome>,
    ) -> SubmissionRecord {
        SubmissionRecord {
            attempt_id: uuid::Uuid::new_v4().to_string(),
            account: "desk".into(),
            operation: operation.into(),
            request: json!({"arguments": args}),
            created_at: 0,
            outcome,
            dedup_key: None,
            armed: true,
        }
    }

    #[test]
    fn only_submissions_that_may_have_reached_the_venue_count() {
        let failed = |code| {
            Some(SubmissionOutcome::Reconciled {
                evidence: Dispatch::Failed { code: Some(code) }.evidence(),
            })
        };
        assert!(counts(&record("place_order", json!({}), None)));
        assert!(counts(&record(
            "replace_order",
            json!({}),
            Some(SubmissionOutcome::Reconciled {
                evidence: Dispatch::Completed { order_id: None }.evidence()
            })
        )));
        assert!(!counts(&record(
            "place_order",
            json!({}),
            failed(FailureCode::RiskDenied)
        )));
        assert!(!counts(&record(
            "place_order",
            json!({}),
            failed(FailureCode::InvalidArguments)
        )));
        assert!(!counts(&record(
            "place_order",
            json!({}),
            Some(SubmissionOutcome::Rejected)
        )));
        assert!(counts(&record(
            "place_order",
            json!({}),
            Some(SubmissionOutcome::Reconciled {
                evidence: Dispatch::Failed { code: None }.evidence()
            })
        )));
        assert!(counts(&record(
            "place_order",
            json!({}),
            failed(FailureCode::ReconciliationRequired)
        )));
        assert!(!counts(&record("cancel_order", json!({}), None)));
    }

    #[test]
    fn the_budget_bounds_orders_and_notional() {
        let capped = policy(json!({"max_daily_orders": 2, "max_daily_notional": "1000"}));
        let used = DayUsage {
            orders: 1,
            notional: Decimal::from(600),
        };
        assert!(within_budget(&capped, used, Decimal::from(400)).is_ok());
        let over = within_budget(&capped, used, Decimal::from(401)).unwrap_err();
        assert!(matches!(over, Error::RiskDenied(_)), "{over}");
        let spent = DayUsage {
            orders: 2,
            notional: Decimal::ZERO,
        };
        assert!(within_budget(&capped, spent, Decimal::ONE).is_err());
        assert!(within_budget(&policy(json!({})), spent, Decimal::from(u32::MAX)).is_ok());
    }
}
