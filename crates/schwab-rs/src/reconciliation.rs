//! Broker reconciliation is conservative: absence never proves rejection.
use crate::{
    SchwabClient,
    error::{Error, Result},
    storage::{StateDatabase, audit, sql},
    types::trading::OrdersParams,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const INVALIDATE_ACCOUNT: &str = "INSERT INTO accounts VALUES(?1,?2,0,0,'{}') \
     ON CONFLICT(account) DO UPDATE SET valid=0,generation=MAX(generation,excluded.generation)";
const SELECT_ACCOUNT_STATE: &str =
    "SELECT generation,observed_at,valid,snapshot FROM accounts WHERE account=?1";
const SELECT_ACCOUNT_GENERATION: &str =
    "SELECT generation,observed_at FROM accounts WHERE account=?1";
const UPSERT_ACCOUNT_SNAPSHOT: &str = "INSERT INTO accounts VALUES(?1,?2,?3,?4,?5) \
     ON CONFLICT(account) DO UPDATE SET generation=excluded.generation,\
     observed_at=excluded.observed_at,valid=excluded.valid,snapshot=excluded.snapshot";
const UPSERT_ORDER: &str = "INSERT INTO orders VALUES(?1,?2,?3,?4) \
     ON CONFLICT(account,order_id) DO UPDATE SET observed_at=excluded.observed_at,\
     payload=excluded.payload";
const UPDATE_ORDER_PAYLOAD: &str =
    "UPDATE orders SET payload=?1,observed_at=?2 WHERE account=?3 AND order_id=?4";
const INSERT_EXECUTION_IF_NEW: &str = "INSERT OR IGNORE INTO executions VALUES(?1,?2,?3)";
/// Submissions with no outcome yet, or accepted without a broker order ID.
const SELECT_PENDING_SUBMISSIONS: &str = "SELECT id,request FROM submissions \
     WHERE account=?1 AND (outcome IS NULL OR json_extract(outcome,'$.order_id') IS NULL \
     AND json_extract(outcome,'$.status')='accepted')";
const RESOLVE_SUBMISSION_OUTCOME: &str = "UPDATE submissions SET outcome=?1 WHERE id=?2";
/// Submissions whose linked broker order has reached a terminal status.
const SELECT_TERMINAL_SUBMISSIONS: &str = "SELECT s.id FROM submissions s \
     JOIN orders o ON o.account=s.account AND o.order_id=json_extract(s.outcome,'$.order_id') \
     WHERE s.account=?1 AND json_extract(o.payload,'$.status') \
     IN ('FILLED','CANCELED','REJECTED','EXPIRED','REPLACED')";
const RELEASE_HELD_RESERVATION: &str =
    "UPDATE reservations SET state='released' WHERE id=?1 AND state='held'";
/// Held reservations whose submission is linked to a broker order this store
/// has observed, with that order's latest payload.
const SELECT_LINKED_HELD_RESERVATIONS: &str = "SELECT r.id,r.quantity,r.notional,o.payload \
     FROM reservations r JOIN submissions s ON s.id=r.id \
     JOIN orders o ON o.account=s.account AND o.order_id=json_extract(s.outcome,'$.order_id') \
     WHERE r.state='held' AND s.account=?1";
const SHRINK_HELD_RESERVATION: &str =
    "UPDATE reservations SET quantity=?1,notional=?2 WHERE id=?3 AND state='held'";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountState {
    pub account: String,
    pub generation: u64,
    pub observed_at: i64,
    pub reconciled: bool,
    pub snapshot: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ReconciliationReport {
    pub account: String,
    pub generation: u64,
    /// When this pass began (ms since the epoch): the `since` for the next
    /// incremental pass once this one is reconciled.
    #[serde(default)]
    pub started_at: i64,
    pub orders_observed: usize,
    pub newly_observed_activities: usize,
    pub resolved_attempts: Vec<String>,
    pub unresolved_attempts: Vec<String>,
    pub reconciled: bool,
    pub terminal_attempts: Vec<String>,
}
impl StateDatabase {
    pub async fn invalidate_account(&self, account: &str, generation: u64) -> Result<()> {
        let account = account.to_owned();
        self.run(move |c| {
            c.execute(INVALIDATE_ACCOUNT, params![account, generation as i64])
                .map_err(sql)?;
            Ok(())
        })
        .await
    }
    pub async fn account_state(&self, account: &str) -> Result<Option<AccountState>> {
        let account = account.to_owned();
        self.run(move |c| {
            let row: Option<(i64, i64, bool, String)> = c
                .query_row(SELECT_ACCOUNT_STATE, [&account], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })
                .optional()
                .map_err(sql)?;
            row.map(|(generation, observed_at, reconciled, snapshot)| {
                Ok(AccountState {
                    account,
                    generation: generation as u64,
                    observed_at,
                    reconciled,
                    snapshot: serde_json::from_str(&snapshot)?,
                })
            })
            .transpose()
        })
        .await
    }
    /// Commit only if no newer invalidation or reconciliation superseded this read.
    pub async fn reconcile_snapshot(
        &self,
        account: &str,
        generation: u64,
        started_at: i64,
        snapshot: Value,
        orders: Vec<Value>,
    ) -> Result<ReconciliationReport> {
        let account = account.to_owned();
        self.run(move |c| {
            let tx = c
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(sql)?;
            let current: Option<(i64, i64)> = tx
                .query_row(SELECT_ACCOUNT_GENERATION, [&account], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()
                .map_err(sql)?;
            if current.is_some_and(|(g, at)| g != generation as i64 || at > started_at) {
                return Err(Error::Storage(
                    "Reconciliation superseded by a newer account generation".into(),
                ));
            }
            let mut report = ReconciliationReport {
                account: account.clone(),
                generation,
                started_at,
                orders_observed: orders.len(),
                newly_observed_activities: 0,
                resolved_attempts: vec![],
                unresolved_attempts: vec![],
                reconciled: false,
                terminal_attempts: vec![],
            };
            for order in &orders {
                persist_order(
                    &tx,
                    &account,
                    order,
                    started_at,
                    &mut report.newly_observed_activities,
                )?;
            }
            // Direct/top-level results take precedence over the potentially
            // older child copies embedded in a parent response.
            for order in &orders {
                if let Some(id) = broker_id(order.get("orderId")) {
                    tx.execute(
                        UPDATE_ORDER_PAYLOAD,
                        params![serde_json::to_string(order)?, started_at, account, id],
                    )
                    .map_err(sql)?;
                }
            }
            let pending = {
                let mut stmt = tx.prepare(SELECT_PENDING_SUBMISSIONS).map_err(sql)?;
                stmt.query_map([&account], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(sql)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(sql)?
            };
            for (id, request) in pending {
                let request: Value = serde_json::from_str(&request)?;
                let tag = request.get("tag").and_then(Value::as_str);
                let candidates: Vec<_> = orders
                    .iter()
                    .filter(|o| {
                        let same_tag = tag
                            .is_some_and(|tag| o.get("tag").and_then(Value::as_str) == Some(tag));
                        same_tag && matches_request(&request, o)
                    })
                    .collect();
                if candidates.len() == 1
                    && let Some(order_id) = broker_id(candidates[0].get("orderId"))
                {
                    let outcome = crate::execution::SubmissionOutcome::Accepted {
                        order_id: Some(order_id),
                    };
                    tx.execute(
                        RESOLVE_SUBMISSION_OUTCOME,
                        params![serde_json::to_string(&outcome)?, id],
                    )
                    .map_err(sql)?;
                    audit(
                        &tx,
                        "broker_reconciled",
                        &id,
                        &serde_json::to_value(&outcome)?,
                    )?;
                    report.resolved_attempts.push(id);
                } else {
                    // Matching by tag is the only safe resolution; an order
                    // that fits the request but has no tag might be this
                    // intent or an identical manual one. Count it, so the
                    // operator knows evidence exists, and leave it pending.
                    let mut untagged = request.clone();
                    if let Some(fields) = untagged.as_object_mut() {
                        fields.remove("tag");
                    }
                    if candidates.is_empty()
                        && orders.iter().any(|o| {
                            o.get("tag").is_none_or(Value::is_null) && matches_request(&untagged, o)
                        })
                    {
                        metrics::counter!(crate::metrics::UNTAGGED_MATCHES_TOTAL).increment(1);
                        tracing::warn!(
                            attempt = %id,
                            "Broker lists an untagged order matching an unresolved submission; \
                             reconcile it with evidence (schwab-admin)"
                        );
                    }
                    report.unresolved_attempts.push(id);
                }
            }
            report.terminal_attempts = {
                let mut stmt = tx.prepare(SELECT_TERMINAL_SUBMISSIONS).map_err(sql)?;
                stmt.query_map([&account], |r| r.get::<_, String>(0))
                    .map_err(sql)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(sql)?
            };
            for id in &report.terminal_attempts {
                tx.execute(RELEASE_HELD_RESERVATION, [id]).map_err(sql)?;
            }
            shrink_filled_reservations(&tx, &account)?;
            report.reconciled = report.unresolved_attempts.is_empty();
            tx.execute(
                UPSERT_ACCOUNT_SNAPSHOT,
                params![
                    account,
                    generation as i64,
                    started_at,
                    report.reconciled,
                    serde_json::to_string(&snapshot)?
                ],
            )
            .map_err(sql)?;
            audit(
                &tx,
                "account_reconciled",
                &account,
                &serde_json::to_value(&report)?,
            )?;
            tx.commit().map_err(sql)?;
            let outcome = if report.reconciled {
                "ready"
            } else {
                "unresolved"
            };
            metrics::counter!(crate::metrics::RECONCILIATION_TOTAL, "outcome" => outcome)
                .increment(1);
            Ok(report)
        })
        .await
    }
}
/// Cut each held reservation down to what its working order can still fill.
/// A partial fill is already out of the broker's available funds, so a
/// reservation for the full size would count the filled part twice. The
/// reservation's price is its notional over its quantity, so the remainder is
/// valued at the same limit. Unreadable or non-shrinking payloads are left
/// alone: holding too much is the safe error.
fn shrink_filled_reservations(tx: &rusqlite::Transaction<'_>, account: &str) -> Result<()> {
    use rust_decimal::Decimal;
    let rows = {
        let mut stmt = tx.prepare(SELECT_LINKED_HELD_RESERVATIONS).map_err(sql)?;
        stmt.query_map([account], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .map_err(sql)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(sql)?
    };
    let decimal = |v: Option<&Value>| {
        v.and_then(|v| {
            v.as_str()
                .map(str::to_owned)
                .or_else(|| v.is_number().then(|| v.to_string()))
        })
        .and_then(|s| s.parse::<Decimal>().ok())
    };
    for (id, quantity, notional, payload) in rows {
        let (Ok(quantity), Ok(notional), Ok(order)) = (
            quantity.parse::<Decimal>(),
            notional.parse::<Decimal>(),
            serde_json::from_str::<Value>(&payload),
        ) else {
            continue;
        };
        let remaining = decimal(order.get("remainingQuantity")).or_else(|| {
            let total = decimal(order.get("quantity"))?;
            total.checked_sub(decimal(order.get("filledQuantity"))?)
        });
        let Some(remaining) = remaining.filter(|r| *r >= Decimal::ZERO && *r < quantity) else {
            continue;
        };
        let Some(price) = notional.checked_div(quantity) else {
            continue;
        };
        let Some(left) = remaining.checked_mul(price) else {
            continue;
        };
        tx.execute(
            SHRINK_HELD_RESERVATION,
            params![remaining.to_string(), left.to_string(), id],
        )
        .map_err(sql)?;
        audit(
            tx,
            "risk_reservation_filled",
            &id,
            &serde_json::json!({
                "quantity": remaining.to_string(),
                "notional": left.to_string(),
            }),
        )?;
    }
    Ok(())
}
pub(crate) fn broker_id(value: Option<&Value>) -> Option<String> {
    value.and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) if n.is_u64() => Some(n.to_string()),
        _ => None,
    })
}
fn matches_request(request: &Value, order: &Value) -> bool {
    // Match explicit request fields recursively, tolerating broker-added fields
    // and decimal number/string representation, never match on symbol alone.
    match (request, order) {
        (Value::Object(a), Value::Object(b)) => a
            .iter()
            .all(|(k, v)| b.get(k).is_some_and(|other| matches_request(v, other))),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| matches_request(a, b))
        }
        _ if request == order => true,
        _ => {
            let decimal = |v: &Value| {
                v.as_str()
                    .map(str::to_owned)
                    .or_else(|| v.is_number().then(|| v.to_string()))
                    .and_then(|s| s.parse::<rust_decimal::Decimal>().ok())
            };
            matches!((decimal(request),decimal(order)),(Some(a),Some(b)) if a==b)
        }
    }
}
pub(crate) fn persist_order(
    c: &rusqlite::Connection,
    account: &str,
    order: &Value,
    at: i64,
    activities: &mut usize,
) -> Result<()> {
    if let Some(id) = broker_id(order.get("orderId")) {
        c.execute(
            UPSERT_ORDER,
            params![account, id, at, serde_json::to_string(order)?],
        )
        .map_err(sql)?;
        if let Some(items) = order
            .get("orderActivityCollection")
            .and_then(Value::as_array)
        {
            for item in items {
                let body = serde_json::to_string(&crate::utils::canonical_json(item))?;
                let hash = ring::digest::digest(&ring::digest::SHA256, body.as_bytes());
                let key = format!(
                    "{id}:{}",
                    hash.as_ref()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                );
                // Deduplicated observations, not synthetic broker execution IDs.
                *activities += c
                    .execute(INSERT_EXECUTION_IF_NEW, params![account, key, body])
                    .map_err(sql)?;
            }
        }
    }
    if let Some(children) = order.get("childOrderStrategies").and_then(Value::as_array) {
        for child in children {
            persist_order(c, account, child, at, activities)?;
        }
    }
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationOptions {
    pub lookback_days: u32,
    pub max_pages: usize,
    pub max_targeted_orders: usize,
    pub timeout_ms: u64,
    /// When the last complete pass for this account began. The order list
    /// then covers only orders entered since (less [`SINCE_OVERLAP`]),
    /// not the whole lookback: older working orders are still fetched one
    /// by one as targets, and an older unresolved intent still widens the
    /// window. `None` scans the full lookback.
    #[serde(default)]
    pub since: Option<chrono::DateTime<chrono::Utc>>,
}
/// Margin on an incremental window, for clock skew between this host and
/// Schwab's `enteredTime`.
pub const SINCE_OVERLAP: chrono::Duration = chrono::Duration::minutes(5);
impl Default for ReconciliationOptions {
    fn default() -> Self {
        Self {
            lookback_days: 59,
            max_pages: 128,
            max_targeted_orders: 1000,
            timeout_ms: 60000,
            since: None,
        }
    }
}
impl SchwabClient {
    pub async fn reconcile_account(
        &self,
        account: &str,
        generation: u64,
    ) -> Result<ReconciliationReport> {
        self.reconcile_account_with_options(account, generation, ReconciliationOptions::default())
            .await
    }
    /// Bounded interval splitting plus direct lookups of known nonterminal orders.
    /// Missing orders never imply rejection; any incomplete scan fails closed.
    pub async fn reconcile_account_with_options(
        &self,
        account: &str,
        generation: u64,
        options: ReconciliationOptions,
    ) -> Result<ReconciliationReport> {
        if !(1..=3650).contains(&options.lookback_days)
            || !(1..=4096).contains(&options.max_pages)
            || !(1..=10000).contains(&options.max_targeted_orders)
            || !(1..=300000).contains(&options.timeout_ms)
        {
            return Err(Error::InvalidParameter(
                "Invalid reconciliation bounds".into(),
            ));
        }
        let database = self.execution_store();
        if database
            .account_state(account)
            .await?
            .is_none_or(|s| s.generation != generation)
        {
            database.invalidate_account(account, generation).await?;
        }
        let started = chrono::Utc::now();
        let run = async {
            let lookback = started - chrono::Duration::days(i64::from(options.lookback_days));
            let mut from = options
                .since
                .map_or(lookback, |since| (since - SINCE_OVERLAP).max(lookback));
            // An ambiguous older intent extends the history scan. Limits still
            // apply; we never silently drop an old unresolved submission.
            for record in database.records().await? {
                if record.account == account
                    && matches!(
                        record.outcome,
                        None | Some(crate::execution::SubmissionOutcome::Accepted {
                            order_id: None
                        })
                    )
                {
                    let at = chrono::DateTime::from_timestamp_millis(record.created_at)
                        .ok_or_else(|| Error::Storage("Invalid intent timestamp".into()))?;
                    from = from.min(at - chrono::Duration::seconds(1));
                }
            }
            let mut windows = vec![(from, started)];
            let mut orders = std::collections::BTreeMap::new();
            let mut pages = 0;
            while let Some((from, to)) = windows.pop() {
                // One day inside Schwab's 60-day order list horizon. A wider
                // window is answered with what it holds, so splitting keeps
                // each request's span meaningful rather than silently short.
                if to - from > chrono::Duration::days(59) {
                    let split = from + chrono::Duration::days(59);
                    windows.push((split, to));
                    windows.push((from, split));
                    continue;
                }
                pages += 1;
                if pages > options.max_pages {
                    return Err(Error::Storage(
                        "Reconciliation page budget exhausted".into(),
                    ));
                }
                let page: Vec<Value> = self
                    .request_with_query(
                        "reconcile_orders",
                        reqwest::Method::GET,
                        &crate::client::account_path(account, "/orders")?,
                        &OrdersParams {
                            max_results: Some(3000),
                            ..OrdersParams::new(from, to)
                        },
                    )
                    .await?;
                if page.len() >= 3000 {
                    if (to - from).num_milliseconds() <= 1000 {
                        return Err(Error::Storage(
                            "Order history remains truncated at minimum interval".into(),
                        ));
                    }
                    let mid = from + (to - from) / 2;
                    windows.push((mid, to));
                    windows.push((from, mid));
                    continue;
                }
                for order in page {
                    let id = broker_id(order.get("orderId"))
                        .ok_or_else(|| Error::Storage("Broker order ID missing".into()))?;
                    orders.insert(id, order);
                }
            }
            // A target the list just returned is already current; fetching it
            // again would cost one request per working order per pass.
            let targets: Vec<String> = database
                .reconciliation_targets(account)
                .await?
                .into_iter()
                .filter(|id| !orders.contains_key(id))
                .collect();
            if targets.len() > options.max_targeted_orders {
                return Err(Error::Storage(
                    "Reconciliation targeted-order budget exhausted".into(),
                ));
            }
            for id in targets {
                if !id.bytes().all(|b| b.is_ascii_digit()) || id.is_empty() {
                    return Err(Error::Storage("Invalid stored broker order ID".into()));
                }
                let order: Value = self
                    .request(
                        "reconcile_order",
                        reqwest::Method::GET,
                        &crate::client::order_path(account, &id)?,
                    )
                    .await?;
                if broker_id(order.get("orderId")).as_deref() != Some(&id) {
                    return Err(Error::Storage("Targeted broker order ID mismatch".into()));
                }
                orders.insert(id, order);
            }
            // Read positions after orders to reduce the stale pre-fill snapshot
            // window. Broker reads are still not an atomic account snapshot.
            let snapshot: Value = self
                .request_with_query(
                    "reconcile_account",
                    reqwest::Method::GET,
                    &crate::client::account_path(account, "")?,
                    &[("fields", "positions")],
                )
                .await?;
            let report = database
                .reconcile_snapshot(
                    account,
                    generation,
                    started.timestamp_millis(),
                    snapshot,
                    orders.into_values().collect(),
                )
                .await?;
            if let Some(risk) = self.risk_provider() {
                for id in &report.terminal_attempts {
                    risk.release(id).await?;
                }
                risk.observe_baseline(account).await?;
            }
            Ok(report)
        };
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(options.timeout_ms), run)
                .await
                .unwrap_or_else(|_| Err(Error::Storage("Reconciliation deadline exceeded".into())));
        if result.is_err() {
            database.invalidate_account(account, generation).await?;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::trading::OrderStatus;

    /// The SQL's idea of a terminal status is the type's.
    #[test]
    fn terminal_statuses_match_the_type() {
        for status in [
            OrderStatus::AwaitingParentOrder,
            OrderStatus::AwaitingCondition,
            OrderStatus::AwaitingStopCondition,
            OrderStatus::AwaitingManualReview,
            OrderStatus::Accepted,
            OrderStatus::AwaitingUrOut,
            OrderStatus::PendingActivation,
            OrderStatus::Queued,
            OrderStatus::Working,
            OrderStatus::Rejected,
            OrderStatus::PendingCancel,
            OrderStatus::Canceled,
            OrderStatus::PendingReplace,
            OrderStatus::Replaced,
            OrderStatus::Filled,
            OrderStatus::Expired,
            OrderStatus::New,
            OrderStatus::AwaitingReleaseTime,
            OrderStatus::PendingAcknowledgement,
            OrderStatus::PendingRecall,
            OrderStatus::Unknown,
        ] {
            let wire = serde_json::to_string(&status).unwrap();
            let in_sql = SELECT_TERMINAL_SUBMISSIONS.contains(&wire.replace('"', "'"));
            assert_eq!(in_sql, status.is_terminal(), "{status:?}");
        }
    }
}
