//! Pluggable pre-trade risk with durable reservations for outstanding orders.
use crate::{
    coordination::{Lease, lease_held},
    error::{Error, Result},
    execution::ensure_not_halted,
    storage::{StateDatabase, sql},
    types::{
        common::AssetType,
        trading::{OrderInstruction, OrderRequest, OrderStrategyType, OrderType},
    },
};
use async_trait::async_trait;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const SELECT_ACCOUNT_BASELINE: &str =
    "SELECT observed_at,valid,snapshot FROM accounts WHERE account=?1";
const SELECT_HELD_RESERVATION: &str =
    "SELECT quantity,notional FROM reservations WHERE id=?1 AND account=?2 AND state='held'";
/// Every held reservation for the account except (when revalidating) our own.
const SELECT_OTHER_HELD_RESERVATIONS: &str = "SELECT quantity,notional FROM reservations \
     WHERE account=?1 AND state='held' AND (NOT ?2 OR id<>?3)";
/// Broker orders for the account that no held local reservation is linked to.
const SELECT_UNRESERVED_BROKER_ORDERS: &str = "SELECT o.payload FROM orders o \
     WHERE o.account=?1 AND NOT EXISTS(\
       SELECT 1 FROM reservations r JOIN submissions s ON s.id=r.id \
       WHERE r.state='held' AND s.account=o.account \
         AND json_extract(s.outcome,'$.order_id')=o.order_id)";
const INSERT_HELD_RESERVATION: &str = "INSERT INTO reservations VALUES(?1,?2,?3,?4,'held')";

#[async_trait]
pub trait RiskProvider: Send + Sync {
    /// Atomically reserve exposure. Implementations must retain it on unknown outcomes.
    async fn reserve(&self, account: &str, attempt_id: &str, order: &OrderRequest) -> Result<()>;
    /// Recheck the held reservation and current risk baseline after all admission
    /// waits, immediately before transmission. Fail closed if unsupported.
    async fn revalidate(
        &self,
        _account: &str,
        _attempt_id: &str,
        _order: &OrderRequest,
    ) -> Result<()> {
        Err(Error::Config(
            "Risk provider must implement dispatch revalidation".into(),
        ))
    }
    /// Atomically revalidate risk, operator halt and a gateway dispatch fence.
    /// Implementations unable to check the same authority must fail closed.
    async fn revalidate_fenced(
        &self,
        _account: &str,
        _attempt_id: &str,
        _order: &OrderRequest,
        _lease: &Lease,
    ) -> Result<()> {
        Err(Error::Config(
            "Risk provider must implement atomic fenced revalidation".into(),
        ))
    }
    /// Release only after a definite rejection or authoritative terminal broker state.
    async fn release(&self, attempt_id: &str) -> Result<()>;
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortfolioLimits {
    pub max_open_quantity: Decimal,
    pub max_open_notional: Decimal,
    pub max_position_quantity: Decimal,
    #[serde(default = "max_age")]
    pub max_snapshot_age_ms: i64,
}
fn max_age() -> i64 {
    5000
}
pub struct PortfolioRisk {
    pub database: StateDatabase,
    pub accounts: HashMap<String, PortfolioLimits>,
}
fn decimal(value: Option<&serde_json::Value>) -> Option<Decimal> {
    value
        .and_then(|v| {
            v.as_str()
                .map(str::to_owned)
                .or_else(|| v.is_number().then(|| v.to_string()))
        })
        .and_then(|v| v.parse().ok())
}
impl PortfolioRisk {
    async fn validate_and_reserve(
        &self,
        account: &str,
        attempt_id: &str,
        order: &OrderRequest,
        revalidate: bool,
        fence: Option<Lease>,
    ) -> Result<()> {
        order.validate().map_err(Error::InvalidParameter)?;
        let limits = self
            .accounts
            .get(account)
            .cloned()
            .ok_or_else(|| Error::RiskDenied("Account has no portfolio risk policy".into()))?;
        if limits.max_open_quantity <= Decimal::ZERO
            || limits.max_open_notional <= Decimal::ZERO
            || limits.max_position_quantity <= Decimal::ZERO
            || !(1..=60000).contains(&limits.max_snapshot_age_ms)
        {
            return Err(Error::Config("Invalid portfolio risk limits".into()));
        }
        if order.order_strategy_type != OrderStrategyType::Single
            || order.order_leg_collection.len() != 1
            || !matches!(
                order.order_type,
                Some(OrderType::Limit | OrderType::StopLimit)
            )
        {
            return Err(Error::RiskDenied(
                "Built-in portfolio risk requires a single-leg limit order; \
                 use an external risk provider for strategies"
                    .into(),
            ));
        }
        let leg = &order.order_leg_collection[0];
        if leg.instrument.asset_type != AssetType::Equity
            || !matches!(
                leg.instruction,
                OrderInstruction::Buy | OrderInstruction::Sell
            )
        {
            return Err(Error::RiskDenied(
                "Built-in portfolio risk supports long equity exposure; \
                 other assets require an external risk provider"
                    .into(),
            ));
        }
        let quantity = leg.quantity;
        let price = order
            .price
            .ok_or_else(|| Error::InvalidParameter("Limit price is required".into()))?;
        let notional = quantity
            .checked_mul(price)
            .ok_or_else(|| Error::InvalidParameter("Risk arithmetic overflow".into()))?;
        let sell = leg.instruction == OrderInstruction::Sell;
        let symbol = leg.instrument.symbol.clone();
        let account = account.to_owned();
        let id = attempt_id.to_owned();
        self.database
            .run(move |c| {
                use rusqlite::OptionalExtension;
                let tx = c
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                let state: Option<(i64, bool, String)> = tx
                    .query_row(SELECT_ACCOUNT_BASELINE, [&account], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })
                    .optional()
                    .map_err(sql)?;
                let stale = |reason: &str| Error::BaselineStale {
                    account: account.clone(),
                    reason: reason.into(),
                };
                let (at, valid, body) =
                    state.ok_or_else(|| stale("Account requires reconciliation before trading"))?;
                let now = chrono::Utc::now().timestamp_millis();
                if revalidate {
                    ensure_not_halted(&tx)?;
                    if let Some(lease) = &fence
                        && !lease_held(&tx, lease)?
                    {
                        return Err(Error::OwnershipLost);
                    }
                }

                if !valid || now < at || now - at > limits.max_snapshot_age_ms {
                    return Err(stale("Account baseline is stale or unreconciled"));
                }
                let snapshot: serde_json::Value = serde_json::from_str(&body)?;
                let account_value = &snapshot["securitiesAccount"];
                let funds = decimal(account_value.pointer("/currentBalances/availableFunds"))
                    .ok_or_else(|| stale("Available funds missing from risk baseline"))?;
                let position = account_value
                    .get("positions")
                    .and_then(serde_json::Value::as_array)
                    .ok_or_else(|| stale("Positions missing from risk baseline"))?
                    .iter()
                    .filter(|p| {
                        p.pointer("/instrument/symbol")
                            .and_then(serde_json::Value::as_str)
                            == Some(&symbol)
                    })
                    .try_fold(Decimal::ZERO, |sum, p| {
                        let quantity = decimal(p.get("longQuantity"))
                            .ok_or_else(|| stale("Position quantity missing or invalid"))?;
                        sum.checked_add(quantity)
                            .ok_or_else(|| stale("Position arithmetic overflow"))
                    })?;
                if revalidate {
                    let held: Option<(String, String)> = tx
                        .query_row(
                            SELECT_HELD_RESERVATION,
                            rusqlite::params![id, account],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()
                        .map_err(sql)?;
                    if held != Some((quantity.to_string(), notional.to_string())) {
                        return Err(Error::Storage(
                            "Dispatch reservation missing or changed".into(),
                        ));
                    }
                }
                let mut stmt = tx.prepare(SELECT_OTHER_HELD_RESERVATIONS).map_err(sql)?;
                let rows = stmt
                    .query_map(rusqlite::params![account, revalidate, id], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })
                    .map_err(sql)?;
                let mut open_q = Decimal::ZERO;
                let mut open_n = Decimal::ZERO;
                for row in rows {
                    let (q, n) = row.map_err(sql)?;
                    let q: Decimal = q
                        .parse()
                        .map_err(|_| Error::Storage("Invalid risk reservation".into()))?;
                    let n: Decimal = n
                        .parse()
                        .map_err(|_| Error::Storage("Invalid risk reservation".into()))?;
                    open_q = open_q
                        .checked_add(q)
                        .ok_or_else(|| Error::Storage("Risk total overflow".into()))?;
                    open_n = open_n
                        .checked_add(n)
                        .ok_or_else(|| Error::Storage("Risk total overflow".into()))?;
                }
                drop(stmt);
                // Include broker orders from all writers. Locally held, positively
                // linked orders are already reserved; do not count them twice.
                let mut orders = tx.prepare(SELECT_UNRESERVED_BROKER_ORDERS).map_err(sql)?;
                for row in orders
                    .query_map([&account], |r| r.get::<_, String>(0))
                    .map_err(sql)?
                {
                    let order: serde_json::Value = serde_json::from_str(&row.map_err(sql)?)?;
                    let (q, n) = external_exposure(&order)?;
                    open_q = open_q
                        .checked_add(q)
                        .ok_or_else(|| Error::Storage("External quantity overflow".into()))?;
                    open_n = open_n
                        .checked_add(n)
                        .ok_or_else(|| Error::Storage("External notional overflow".into()))?;
                }
                drop(orders);
                let projected_q = open_q
                    .checked_add(quantity)
                    .ok_or_else(|| Error::RiskDenied("Risk quantity overflow".into()))?;
                let projected_n = open_n
                    .checked_add(notional)
                    .ok_or_else(|| Error::RiskDenied("Risk notional overflow".into()))?;
                let buy_exceeds = !sell
                    && (projected_n > funds
                        || position
                            .checked_add(projected_q)
                            .is_none_or(|v| v > limits.max_position_quantity));
                let sell_exceeds = sell && projected_q > position;
                if projected_q > limits.max_open_quantity
                    || projected_n > limits.max_open_notional
                    || buy_exceeds
                    || sell_exceeds
                {
                    return Err(Error::RiskDenied(
                        "Portfolio or outstanding-order limit exceeded".into(),
                    ));
                }
                if !revalidate {
                    tx.execute(
                        INSERT_HELD_RESERVATION,
                        rusqlite::params![id, account, quantity.to_string(), notional.to_string()],
                    )
                    .map_err(sql)?;
                    crate::storage::audit(
                        &tx,
                        "risk_reserved",
                        &id,
                        &serde_json::json!({
                            "quantity": quantity.to_string(),
                            "notional": notional.to_string()
                        }),
                    )?;
                }
                tx.commit().map_err(sql)?;
                Ok(())
            })
            .await
    }
}
#[async_trait]
impl RiskProvider for PortfolioRisk {
    async fn reserve(&self, account: &str, attempt_id: &str, order: &OrderRequest) -> Result<()> {
        self.validate_and_reserve(account, attempt_id, order, false, None)
            .await
    }
    async fn revalidate(
        &self,
        account: &str,
        attempt_id: &str,
        order: &OrderRequest,
    ) -> Result<()> {
        self.validate_and_reserve(account, attempt_id, order, true, None)
            .await
    }
    async fn revalidate_fenced(
        &self,
        account: &str,
        attempt_id: &str,
        order: &OrderRequest,
        lease: &Lease,
    ) -> Result<()> {
        self.validate_and_reserve(account, attempt_id, order, true, Some(lease.clone()))
            .await
    }
    async fn release(&self, attempt_id: &str) -> Result<()> {
        let id = attempt_id.to_owned();
        self.database
            .run(move |c| {
                c.execute(
                    "UPDATE reservations SET state='released' WHERE id=?1",
                    [&id],
                )
                .map_err(sql)?;
                Ok(())
            })
            .await
    }
}

/// Conservative gross exposure for broker orders not covered by local holds.
fn external_exposure(order: &serde_json::Value) -> Result<(Decimal, Decimal)> {
    let deny =
        || Error::RiskDenied("External open order needs a supported, bounded risk model".into());
    if matches!(
        order.get("status").and_then(|v| v.as_str()),
        Some("FILLED" | "CANCELED" | "REJECTED" | "EXPIRED" | "REPLACED")
    ) {
        return Ok((Decimal::ZERO, Decimal::ZERO));
    }
    if order.get("status").and_then(|v| v.as_str()).is_none()
        || order.get("orderStrategyType").and_then(|v| v.as_str()) != Some("SINGLE")
        || !matches!(
            order.get("orderType").and_then(|v| v.as_str()),
            Some("LIMIT" | "STOP_LIMIT")
        )
    {
        return Err(deny());
    }
    let legs = order
        .get("orderLegCollection")
        .and_then(|v| v.as_array())
        .ok_or_else(deny)?;
    if legs.len() != 1
        || legs[0]
            .pointer("/instrument/assetType")
            .and_then(|v| v.as_str())
            != Some("EQUITY")
        || !matches!(
            legs[0].get("instruction").and_then(|v| v.as_str()),
            Some("BUY" | "SELL")
        )
    {
        return Err(deny());
    }
    let total = decimal(order.get("quantity"))
        .or_else(|| decimal(legs[0].get("quantity")))
        .ok_or_else(deny)?;
    let filled = decimal(order.get("filledQuantity")).ok_or_else(deny)?;
    let remaining = decimal(order.get("remainingQuantity"))
        .or_else(|| total.checked_sub(filled))
        .ok_or_else(deny)?;
    let price = decimal(order.get("price")).ok_or_else(deny)?;
    if total < Decimal::ZERO
        || filled < Decimal::ZERO
        || filled > total
        || remaining < Decimal::ZERO
        || remaining > total
        || filled.checked_add(remaining) != Some(total)
        || price <= Decimal::ZERO
    {
        return Err(deny());
    }
    Ok((remaining, remaining.checked_mul(price).ok_or_else(deny)?))
}
