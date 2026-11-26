//! Broker reconciliation is conservative: absence never proves rejection.
use crate::{
    SchwabClient,
    client::Op,
    error::{Error, Result},
    execution::is_broker_order_id,
    storage::{StateDatabase, audit, sql},
    types::trading::OrdersParams,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cmp::Ordering;

const INVALIDATE_ACCOUNT: &str = "INSERT INTO accounts VALUES(?1,?2,0,0,'{}') \
     ON CONFLICT(account) DO UPDATE SET valid=0,generation=MAX(generation,excluded.generation)";
const SELECT_ACCOUNT_STATE: &str =
    "SELECT generation,observed_at,valid,snapshot FROM accounts WHERE account=?1";
const SELECT_ACCOUNT_GENERATION: &str =
    "SELECT generation,observed_at,valid FROM accounts WHERE account=?1";
const UPSERT_ACCOUNT_SNAPSHOT: &str = "INSERT INTO accounts VALUES(?1,?2,?3,?4,?5) \
     ON CONFLICT(account) DO UPDATE SET generation=excluded.generation,\
     observed_at=excluded.observed_at,valid=excluded.valid,snapshot=excluded.snapshot";
const UPSERT_ORDER: &str = "INSERT INTO orders VALUES(?1,?2,?3,?4) \
     ON CONFLICT(account,order_id) DO UPDATE SET observed_at=excluded.observed_at,\
     payload=excluded.payload";
const UPDATE_ORDER_PAYLOAD: &str =
    "UPDATE orders SET payload=?1,observed_at=?2 WHERE account=?3 AND order_id=?4";
const INSERT_EXECUTION_IF_NEW: &str = "INSERT OR IGNORE INTO executions VALUES(?1,?2,?3)";
/// Submissions with no outcome yet, or accepted without a broker order ID,
/// with whether each was armed and when it began.
const SELECT_PENDING_SUBMISSIONS: &str = "SELECT id,request,outcome IS NULL,armed,created_at \
     FROM submissions WHERE account=?1 AND (outcome IS NULL \
       OR (json_extract(outcome,'$.status')='accepted' \
         AND json_extract(outcome,'$.order_id') IS NULL))";
const RESOLVE_SUBMISSION_OUTCOME: &str = "UPDATE submissions SET outcome=?1 WHERE id=?2";
/// Frees the tag of an intent resolved as never dispatched, as
/// [`OrderJournal::finish`](crate::execution::OrderJournal::finish) does
/// for any rejection.
const RELEASE_NEVER_SENT_TAG: &str = "DELETE FROM used_order_tags WHERE account=?1 AND tag=?2 \
     AND NOT EXISTS(SELECT 1 FROM orders o WHERE o.account=?1 \
       AND json_extract(o.payload,'$.tag')=?2)";
const INSERT_USED_ORDER_TAG: &str = "INSERT OR IGNORE INTO used_order_tags VALUES(?1,?2)";
/// Submissions still holding a reservation whose linked broker order has
/// reached a terminal status: what this pass releases.
const SELECT_TERMINAL_SUBMISSIONS: &str = "SELECT s.id FROM reservations r \
     JOIN submissions s ON s.id=r.id \
     JOIN orders o ON o.account=s.account AND o.order_id=json_extract(s.outcome,'$.order_id') \
     WHERE r.state='held' AND r.account=?1 AND json_extract(o.payload,'$.status') \
     IN ('FILLED','CANCELED','REJECTED','EXPIRED','REPLACED')";
const RELEASE_HELD_RESERVATION: &str =
    "UPDATE reservations SET state='released' WHERE id=?1 AND state='held'";
/// Held reservations whose submission resolved without a live linked order
/// (refused, or settled by an operator) before releases moved into
/// `finish`: swept so they no longer count against the account.
const RELEASE_ORPHANED_RESERVATIONS: &str = "UPDATE reservations SET state='released' \
     WHERE state='held' AND account=?1 AND id IN (SELECT id FROM submissions \
       WHERE account=?1 AND json_extract(outcome,'$.status') IN ('rejected','reconciled'))";
/// How long an unarmed intent may stay unresolved before recovery decides it
/// was never sent: longer than any submission's own deadline, so the task
/// that began it has either armed it or given up. Arming is conditional on
/// the intent being unresolved, so even a late dispatcher cannot send what
/// recovery resolved.
pub const NEVER_DISPATCHED_AFTER: chrono::Duration = chrono::Duration::minutes(10);
/// How far ahead of this store's clock a pass's start may be.
const MAX_CLOCK_LEAD_MS: i64 = 5_000;
/// Held reservations whose submission is linked to a broker order this store
/// has observed, with that order's latest payload.
const SELECT_LINKED_HELD_RESERVATIONS: &str = "SELECT r.id,r.quantity,r.notional,o.payload \
     FROM reservations r JOIN submissions s ON s.id=r.id \
     JOIN orders o ON o.account=s.account AND o.order_id=json_extract(s.outcome,'$.order_id') \
     WHERE r.state='held' AND s.account=?1";
const SHRINK_HELD_RESERVATION: &str =
    "UPDATE reservations SET quantity=?1,notional=?2 WHERE id=?3 AND state='held'";
const SELECT_ORDER_ROW: &str =
    "SELECT observed_at,payload FROM orders WHERE account=?1 AND order_id=?2";
/// Accepted submissions on `?1` that name broker order `?2`.
const SELECT_ACCEPTED_FOR_ORDER: &str = "SELECT id FROM submissions WHERE account=?1 \
     AND json_extract(outcome,'$.status')='accepted' AND json_extract(outcome,'$.order_id')=?2 \
     ORDER BY created_at,id";
/// Gives order `?2`'s observed activities order `?3`'s ID, keeping one of
/// each: an activity's key is its order's ID and a digest of its content,
/// so the same activity observed under both IDs has one key once moved.
const MOVE_ORDER_EXECUTIONS: &str = "INSERT OR IGNORE INTO executions(account,execution_id,payload) \
     SELECT account,?3 || substr(execution_id,length(?2)+1),payload FROM executions \
     WHERE account=?1 AND execution_id>=?2 || ':' AND execution_id<?2 || ';'";
/// Reservations on `?1` for a replacement of order `?2` name order `?3`.
const RENAME_REPLACED_ORDER: &str =
    "UPDATE reservations SET replaces=?3 WHERE account=?1 AND replaces=?2";
/// Reservations on `?1` for a replacement of `?2`, which names no order, no
/// longer name the order they replace.
const FORGET_REPLACED_ORDER: &str =
    "UPDATE reservations SET replaces=NULL WHERE account=?1 AND replaces=?2";

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
    /// Attempts whose reservation this pass released because their broker
    /// order reached a terminal status.
    pub terminal_attempts: Vec<String>,
    /// Unresolved intents this pass resolved as rejected because they were
    /// never armed: nothing was sent (see
    /// [`NEVER_DISPATCHED_AFTER`]).
    #[serde(default)]
    pub never_dispatched_attempts: Vec<String>,
    /// A newer pass or invalidation committed first, so this one wrote
    /// nothing; the stored account state is the newer one's.
    #[serde(default)]
    pub superseded: bool,
}
/// A stored order ID that is not a broker order ID (the store's invariant,
/// which [`Error::NonCanonicalOrderId`] reports), and what the repair does
/// with it: rewrites it as `canonical`, the same order in its own digits,
/// or sets it aside when it names no order at all. Listed by
/// [`StateDatabase::order_id_repairs`] and made by
/// [`StateDatabase::repair_order_ids`]. Displayed as one line:
/// `orders (account …1234): "007" -> "7"`, or `-> set aside`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OrderIdRepair {
    /// The table that holds it.
    pub table: String,
    /// Its row, as [`Error::NonCanonicalOrderId`] names it.
    pub row: String,
    /// The account it is stored under.
    pub account: String,
    /// The text as stored.
    pub order_id: String,
    /// The order ID it becomes; `None` when it is set aside.
    pub canonical: Option<String>,
}
impl std::fmt::Display for OrderIdRepair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {:?} -> ", self.table, self.row, self.order_id)?;
        match &self.canonical {
            Some(canonical) => write!(f, "{canonical:?}"),
            None => f.write_str("set aside"),
        }
    }
}
impl StateDatabase {
    pub async fn invalidate_account(&self, account: &str, generation: u64) -> Result<()> {
        let account = account.to_owned();
        self.write(move |c| {
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
    /// The accounts reconciliation has a baseline for.
    pub async fn known_accounts(&self) -> Result<Vec<String>> {
        self.run(|c| {
            let mut stmt = c
                .prepare("SELECT account FROM accounts ORDER BY account")
                .map_err(sql)?;
            let rows = stmt.query_map([], |r| r.get(0)).map_err(sql)?;
            rows.collect::<std::result::Result<_, _>>().map_err(sql)
        })
        .await
    }
    /// Up to `limit` of the orders reconciliation last read for `account`,
    /// as Schwab reported them, most recently observed first: a view of the
    /// account's orders that costs no request.
    pub async fn known_orders(&self, account: &str, limit: u32) -> Result<Vec<Value>> {
        let account = account.to_owned();
        self.run(move |c| {
            let mut stmt = c
                .prepare(
                    "SELECT payload FROM orders WHERE account=?1 \
                     ORDER BY observed_at DESC, order_id DESC LIMIT ?2",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map(params![account, limit.clamp(1, 10_000)], |r| {
                    r.get::<_, String>(0)
                })
                .map_err(sql)?;
            rows.map(|row| Ok(serde_json::from_str(&row.map_err(sql)?)?))
                .collect()
        })
        .await
    }
    /// Commit only if no newer invalidation or reconciliation superseded this
    /// read; a superseded pass writes nothing and says so in its report.
    pub async fn reconcile_snapshot(
        &self,
        account: &str,
        generation: u64,
        started_at: i64,
        snapshot: Value,
        orders: Vec<Value>,
    ) -> Result<ReconciliationReport> {
        let account = account.to_owned();
        self.write(move |c| {
            let tx = c
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(sql)?;
            // The pass's start becomes the baseline's age.
            let started_at =
                baseline_observed_at(started_at, chrono::Utc::now().timestamp_millis())?;
            let current: Option<(i64, i64, bool)> = tx
                .query_row(SELECT_ACCOUNT_GENERATION, [&account], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()
                .map_err(sql)?;
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
                never_dispatched_attempts: vec![],
                superseded: false,
            };
            if current.is_some_and(|(g, at, _)| g != generation as i64 || at > started_at) {
                report.superseded = true;
                return Ok(report);
            }
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
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, bool>(2)?,
                        r.get::<_, bool>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                })
                .map_err(sql)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(sql)?
            };
            let never_sent_before = started_at - NEVER_DISPATCHED_AFTER.num_milliseconds();
            for (id, request, unresolved, armed, created_at) in pending {
                let request: Value = serde_json::from_str(&request)?;
                let tag = request.get("tag").and_then(Value::as_str);
                if unresolved && !armed && created_at < never_sent_before {
                    // Never armed, so never sent; arming now fails.
                    let outcome = crate::execution::SubmissionOutcome::Rejected;
                    tx.execute(
                        RESOLVE_SUBMISSION_OUTCOME,
                        params![serde_json::to_string(&outcome)?, id],
                    )
                    .map_err(sql)?;
                    if let Some(tag) = tag {
                        tx.execute(RELEASE_NEVER_SENT_TAG, params![account, tag])
                            .map_err(sql)?;
                    }
                    tx.execute(RELEASE_HELD_RESERVATION, [&id]).map_err(sql)?;
                    audit(
                        &tx,
                        "never_dispatched",
                        &id,
                        &serde_json::json!({"outcome": outcome}),
                    )?;
                    report.never_dispatched_attempts.push(id);
                    continue;
                }
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
                } else if unresolved && !armed {
                    // Still on its way to the broker (waiting for its turn
                    // at the execution lease, say) but not armed, so not
                    // sent: Schwab cannot have it, and its reservation still
                    // counts against the risk limits. It leaves the baseline
                    // valid, so it is not refused at its own revalidation.
                } else {
                    // Matching by tag is the only safe resolution; an order
                    // that fits the request without our tag (none, or one
                    // the broker stamped itself) might be this intent or an
                    // identical manual one. Count it, so the operator knows
                    // evidence exists, and leave it pending.
                    let mut untagged = request.clone();
                    if let Some(fields) = untagged.as_object_mut() {
                        fields.remove("tag");
                    }
                    if candidates.is_empty()
                        && orders.iter().any(|o| {
                            o.get("tag").and_then(Value::as_str) != tag
                                && matches_request(&untagged, o)
                        })
                    {
                        metrics::counter!(crate::metrics::UNTAGGED_MATCHES_TOTAL).increment(1);
                        tracing::warn!(
                            attempt = %id,
                            "Broker lists an order matching an unresolved submission but \
                             without its tag; reconcile it with evidence (rustyschwab-admin)"
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
            let orphaned = tx
                .execute(RELEASE_ORPHANED_RESERVATIONS, [&account])
                .map_err(sql)?;
            let shrunk = shrink_filled_reservations(&tx, &account)?;
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
            // A pass that changed nothing the journal tracks is not an
            // event: auditing every pass would grow the trail by one row
            // per account per interval forever.
            let changed = !report.resolved_attempts.is_empty()
                || !report.terminal_attempts.is_empty()
                || !report.never_dispatched_attempts.is_empty()
                || report.newly_observed_activities > 0
                || orphaned > 0
                || shrunk > 0
                || current.is_none_or(|(_, _, valid)| valid != report.reconciled);
            if changed {
                audit(
                    &tx,
                    "account_reconciled",
                    &account,
                    &serde_json::to_value(&report)?,
                )?;
            }
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
    /// Every change [`repair_order_ids`](Self::repair_order_ids) would make
    /// to this file as it stands, in table and then row order. Changes
    /// nothing. Like the repair, it reads a file that fails to open with
    /// [`Error::NonCanonicalOrderId`]; an empty list means there is nothing
    /// to repair.
    pub async fn order_id_repairs(&self) -> Result<Vec<OrderIdRepair>> {
        self.repairing(|c| stored_order_id_repairs(c)).await
    }
    /// Repair a file that fails to open with [`Error::NonCanonicalOrderId`]
    /// (edited by hand or damaged after its migration), in `operator`'s
    /// name: the migration's canonicalization, run on demand. Under the init
    /// lock and in one transaction, every stored order ID that is not a
    /// broker order ID is rewritten in its own digits, merging with the
    /// records for the same order, or set aside when it names none, each
    /// audited as the migration audits it; then `order_ids_repaired` records
    /// the operator and the changes. Afterwards the file opens. Returns the
    /// changes made, as [`order_id_repairs`](Self::order_id_repairs) listed
    /// them; with nothing to repair it writes nothing.
    ///
    /// The file must be at this release's schema version: opening one that
    /// is behind migrates it, which makes the same repair.
    pub async fn repair_order_ids(&self, operator: &str) -> Result<Vec<OrderIdRepair>> {
        if operator.trim().is_empty() {
            return Err(Error::InvalidParameter("Operator is required".into()));
        }
        let operator = operator.to_owned();
        self.repairing(move |c| {
            let tx = c
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(sql)?;
            let repairs = stored_order_id_repairs(&tx)?;
            if repairs.is_empty() {
                return Ok(repairs);
            }
            repair_stored_order_ids(&tx, &repairs)?;
            check_stored_order_ids(&tx)?;
            audit(
                &tx,
                "order_ids_repaired",
                &operator,
                &serde_json::json!({ "changes": repairs }),
            )?;
            tx.commit().map_err(sql)?;
            Ok(repairs)
        })
        .await
    }
}
/// One order ID as this store holds it outside its audit trail: where it is
/// (`table`, and the row as an error names it), its account and its text.
struct StoredOrderId {
    table: &'static str,
    row: String,
    account: String,
    order_id: String,
}
/// Visit every order ID this store holds outside its audit trail (see
/// [`check_stored_order_ids`] for what that leaves out, and why): each
/// stored order and reference, each observed activity (keyed by its
/// order's ID and a digest of its content), each accepted submission's
/// order, and each reservation's replaced order, table by table in that
/// order and each table in its key's order. Reads row by row.
fn for_each_stored_order_id(
    c: &rusqlite::Connection,
    mut visit: impl FnMut(StoredOrderId) -> Result<()>,
) -> Result<()> {
    let masked = crate::utils::mask_account;
    // Each query gives the account, the row's key and the order ID's text.
    let mut scan = |table: &'static str,
                    query: &str,
                    describe: &dyn Fn(&str, &str, String) -> (String, String)|
     -> Result<()> {
        let mut statement = c.prepare(query).map_err(sql)?;
        let mut rows = statement.query([]).map_err(sql)?;
        while let Some(r) = rows.next().map_err(sql)? {
            let account: String = r.get(0).map_err(sql)?;
            let key: String = r.get(1).map_err(sql)?;
            let (row, order_id) = describe(&account, &key, r.get(2).map_err(sql)?);
            visit(StoredOrderId {
                table,
                row,
                account,
                order_id,
            })?;
        }
        Ok(())
    };
    scan(
        "orders",
        "SELECT account,order_id,order_id FROM orders ORDER BY account,order_id",
        &|account, _, order_id| (format!("account {}", masked(account)), order_id),
    )?;
    scan(
        "executions",
        "SELECT account,execution_id,execution_id FROM executions \
         ORDER BY account,execution_id",
        &|account, key, _| {
            let order_id = key.rsplit_once(':').map_or(key, |(order_id, _)| order_id);
            (
                format!("account {}, activity {key}", masked(account)),
                order_id.to_owned(),
            )
        },
    )?;
    scan(
        "submissions",
        "SELECT account,id,json_extract(outcome,'$.order_id') FROM submissions \
         WHERE json_extract(outcome,'$.status')='accepted' \
           AND json_extract(outcome,'$.order_id') IS NOT NULL ORDER BY id",
        &|_, id, order_id| (format!("submission {id}"), order_id),
    )?;
    scan(
        "reservations",
        "SELECT account,id,replaces FROM reservations WHERE replaces IS NOT NULL ORDER BY id",
        &|_, id, order_id| (format!("reservation {id}"), order_id),
    )
}
/// The store's invariant on order IDs: every one it holds outside its audit
/// trail is a broker order ID ([`is_broker_order_id`]), so each order has
/// exactly one text form in every table that names it. Checked each time
/// the store opens, after migrating (`StateDatabase`); new input is held to
/// the same rule where it comes in. A file that breaks it was edited by
/// hand or damaged after its migration, so this fails closed with
/// [`Error::NonCanonicalOrderId`] rather than repair it silently.
///
/// What is left out, on purpose:
/// - The audit trail. Its rows are append-only evidence, hash-chained
///   (`audit_chain`), of what was true when each was written; an ID an
///   earlier release stored stays in them as it was written, and the
///   `order_id_canonicalized` entry the migration wrote records the
///   mapping. Rewriting them would falsify that evidence and break the
///   chain.
/// - What was asked for rather than what this store follows: each
///   submission's request as submitted (for an MCP principal, the input of
///   the approval digest it consumed), the intents awaiting approval, and
///   the activity log.
/// - The text inside an order's payload, the broker's view as it reported
///   it (a parent order's copies of its children's IDs, say). The migration
///   rewrites the `orderId` of an order it moves; the rest is as reported.
pub(crate) fn check_stored_order_ids(c: &rusqlite::Connection) -> Result<()> {
    for_each_stored_order_id(c, |stored| {
        if is_broker_order_id(&stored.order_id) {
            return Ok(());
        }
        Err(Error::NonCanonicalOrderId {
            table: stored.table,
            row: stored.row,
            order_id: stored.order_id,
        })
    })
}
/// The one-time migration to canonical order IDs (schema version 7), run in
/// the transaction that records it: every order ID this store holds outside
/// its audit trail, finished history included, is rewritten in its own
/// digits when it names an order written another way, merging the records
/// for the same order ([`canonicalize_order_id`]), and set aside when it
/// names no order at all ([`set_aside_order_id`]). Each change is audited;
/// the audit rows already written are not touched (see
/// [`check_stored_order_ids`]). Afterwards the store meets its invariant.
/// An operator's repair ([`StateDatabase::repair_order_ids`]) makes the
/// same changes on demand.
pub(crate) fn canonicalize_stored_order_ids(c: &rusqlite::Connection) -> Result<()> {
    repair_stored_order_ids(c, &stored_order_id_repairs(c)?)
}
/// Every stored order ID that breaks the invariant
/// ([`check_stored_order_ids`]), in the order the store holds them, and
/// what the canonicalization makes of it: the same order in its own
/// digits, or `None` when it names no order and is set aside.
fn stored_order_id_repairs(c: &rusqlite::Connection) -> Result<Vec<OrderIdRepair>> {
    let mut repairs = Vec::new();
    for_each_stored_order_id(c, |stored| {
        if !is_broker_order_id(&stored.order_id) {
            repairs.push(OrderIdRepair {
                canonical: crate::execution::canonical_order_id(&stored.order_id),
                table: stored.table.to_owned(),
                row: stored.row,
                account: stored.account,
                order_id: stored.order_id,
            });
        }
        Ok(())
    })?;
    Ok(repairs)
}
/// Make `repairs`, which [`stored_order_id_repairs`] listed on `c`, once
/// for each stored ID on each account however many rows name it: rewrite
/// ([`canonicalize_order_id`]) or set aside ([`set_aside_order_id`]).
fn repair_stored_order_ids(c: &rusqlite::Connection, repairs: &[OrderIdRepair]) -> Result<()> {
    let ids: std::collections::BTreeSet<_> = repairs
        .iter()
        .map(|r| (&r.account, &r.order_id, &r.canonical))
        .collect();
    for (account, id, canonical) in ids {
        match canonical {
            Some(canonical) => canonicalize_order_id(c, account, id, canonical)?,
            None => set_aside_order_id(c, account, id)?,
        }
    }
    Ok(())
}
/// The accepted submissions on `account` that name order `id`.
fn accepted_for_order(c: &rusqlite::Connection, account: &str, id: &str) -> Result<Vec<String>> {
    let mut stmt = c.prepare(SELECT_ACCEPTED_FOR_ORDER).map_err(sql)?;
    stmt.query_map(params![account, id], |r| r.get(0))
        .map_err(sql)?
        .collect::<std::result::Result<_, _>>()
        .map_err(sql)
}
/// When order `id` was last observed and how, if this store holds it.
fn order_row(c: &rusqlite::Connection, account: &str, id: &str) -> Result<Option<(i64, String)>> {
    c.query_row(SELECT_ORDER_ROW, params![account, id], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })
    .optional()
    .map_err(sql)
}
/// Rewrite stored ID `stored`, which names broker order `canonical` written
/// another way, in its own digits, merging it with whatever this store
/// already holds under `canonical`. Both describe the same broker order,
/// so the merge loses nothing about it:
/// - the order's record is the later of the two observations, the
///   broker's latest view (on a tie, the record under its own digits);
/// - each observed activity is kept once: activities are keyed by their
///   content, so one observed under both IDs is the same activity;
/// - every submission accepted under `stored` is accepted under
///   `canonical`, and a reservation for a replacement of `stored` names
///   `canonical`, so risk and reconciliation link them to the order as
///   before.
///
/// Audited as `order_id_canonicalized`, with `stored` as the subject.
fn canonicalize_order_id(
    tx: &rusqlite::Connection,
    account: &str,
    stored: &str,
    canonical: &str,
) -> Result<()> {
    let existing = order_row(tx, account, canonical)?;
    if let Some((observed_at, payload)) = order_row(tx, account, stored)? {
        if existing
            .as_ref()
            .is_none_or(|(existing_at, _)| observed_at > *existing_at)
        {
            let mut order: Value = serde_json::from_str(&payload)?;
            if let Some(fields) = order.as_object_mut() {
                fields.insert("orderId".into(), Value::String(canonical.to_owned()));
            }
            tx.execute(
                UPSERT_ORDER,
                params![
                    account,
                    canonical,
                    observed_at,
                    serde_json::to_string(&order)?
                ],
            )
            .map_err(sql)?;
        }
        tx.execute(crate::execution::DELETE_ORDER, params![account, stored])
            .map_err(sql)?;
        tx.execute(MOVE_ORDER_EXECUTIONS, params![account, stored, canonical])
            .map_err(sql)?;
        tx.execute(
            crate::execution::DELETE_ORDER_EXECUTIONS,
            params![account, stored],
        )
        .map_err(sql)?;
    }
    let attempts = accepted_for_order(tx, account, stored)?;
    let accepted = serde_json::to_string(&crate::execution::SubmissionOutcome::Accepted {
        order_id: Some(canonical.to_owned()),
    })?;
    for attempt in &attempts {
        tx.execute(RESOLVE_SUBMISSION_OUTCOME, params![accepted, attempt])
            .map_err(sql)?;
    }
    tx.execute(RENAME_REPLACED_ORDER, params![account, stored, canonical])
        .map_err(sql)?;
    tracing::warn!(
        order_id = ?stored,
        canonical,
        account = %crate::utils::mask_account(account),
        "A stored order ID was written another way; it was rewritten in its own digits \
         (audited as order_id_canonicalized)"
    );
    audit(
        tx,
        "order_id_canonicalized",
        stored,
        &serde_json::json!({
            "account": account,
            "order_id": canonical,
            "merged": existing.is_some(),
            "attempts": attempts,
        }),
    )
}
/// Set aside stored ID `id`, which names no broker order at all, so nothing
/// real is given up: the order stored under it and its observed activity
/// are dropped, a submission accepted under it goes back to being placed by
/// its tag (keeping its reservation), and a reservation for a replacement
/// of it no longer names one, which can only count more against the
/// account. Audited as `order_id_set_aside` with what was stored.
fn set_aside_order_id(tx: &rusqlite::Connection, account: &str, id: &str) -> Result<()> {
    let order: Option<Value> = order_row(tx, account, id)?
        .map(|(_, payload)| serde_json::from_str(&payload))
        .transpose()?;
    tx.execute(crate::execution::DELETE_ORDER, params![account, id])
        .map_err(sql)?;
    tx.execute(
        crate::execution::DELETE_ORDER_EXECUTIONS,
        params![account, id],
    )
    .map_err(sql)?;
    let attempts = accepted_for_order(tx, account, id)?;
    let unplaced =
        serde_json::to_string(&crate::execution::SubmissionOutcome::Accepted { order_id: None })?;
    for attempt in &attempts {
        tx.execute(RESOLVE_SUBMISSION_OUTCOME, params![unplaced, attempt])
            .map_err(sql)?;
    }
    tx.execute(FORGET_REPLACED_ORDER, params![account, id])
        .map_err(sql)?;
    tracing::error!(
        order_id = ?id,
        account = %crate::utils::mask_account(account),
        attempts = attempts.len(),
        "A stored order ID cannot name a broker order; it was set aside \
         (audited as order_id_set_aside)"
    );
    audit(
        tx,
        "order_id_set_aside",
        id,
        &serde_json::json!({"account": account, "order": order, "attempts": attempts}),
    )
}
/// When the baseline of a pass that began at `started_at` counts as
/// observed, by a store whose clock reads `now` (both in ms since the
/// epoch). A start ahead of this store's clock (a remote worker whose clock
/// runs fast) would make the baseline look fresher than it is: up to
/// [`MAX_CLOCK_LEAD_MS`] ahead it counts from now, and further ahead it is
/// refused.
fn baseline_observed_at(started_at: i64, now: i64) -> Result<i64> {
    if started_at > now + MAX_CLOCK_LEAD_MS {
        return Err(Error::Storage(
            "Reconciliation started ahead of the state store's clock".into(),
        ));
    }
    Ok(started_at.min(now))
}
/// Cut each held reservation down to what its working order can still fill.
/// A partial fill is already out of the broker's available funds, so a
/// reservation for the full size would count the filled part twice. The
/// reservation's price is its notional over its quantity, so the remainder is
/// valued at the same limit. Unreadable or non-shrinking payloads are left
/// alone: holding too much is the safe error.
fn shrink_filled_reservations(tx: &rusqlite::Transaction<'_>, account: &str) -> Result<usize> {
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
    let mut shrunk = 0;
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
        shrunk += 1;
    }
    Ok(shrunk)
}
/// Drop the placeholder for one broker order under each text it was stored
/// as (`stored`, one ID written one or more ways); whether any was dropped.
async fn forget_placeholders(
    store: &dyn crate::execution::ExecutionStore,
    account: &str,
    stored: &[String],
) -> Result<bool> {
    let mut forgot = false;
    for id in stored {
        if store.forget_placeholder(account, id).await? {
            forgot = true;
        }
    }
    Ok(forgot)
}
/// The broker order ID an order payload's `orderId` holds, as a JSON
/// string or a JSON number: one rule ([`is_broker_order_id`]) for both, so
/// `"7"` and `7` name the same order and neither form reaches past
/// `u64::MAX`.
pub(crate) fn broker_id(value: Option<&Value>) -> Option<String> {
    let id = match value? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    is_broker_order_id(&id).then_some(id)
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
        // The broker holds an order under this tag: no new intent may use
        // it, even if this store lost the submission that did.
        if let Some(tag) = order.get("tag").and_then(Value::as_str) {
            c.execute(INSERT_USED_ORDER_TAG, params![account, tag])
                .map_err(sql)?;
        }
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
/// An account's net cash transfers into it on one trading day, as read.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DayTransfers {
    date: chrono::NaiveDate,
    read_at: tokio::time::Instant,
    net: rust_decimal::Decimal,
}
impl DayTransfers {
    /// The net transfers read, while the reading is of trading day `today`
    /// and younger than [`DAY_TRANSFERS_TTL`].
    fn current(&self, today: chrono::NaiveDate) -> Option<rust_decimal::Decimal> {
        (self.date == today && self.read_at.elapsed() < DAY_TRANSFERS_TTL).then_some(self.net)
    }
}
/// How long a reading of the day's transfers is reused: they are rare, and
/// each reading costs one request per transfer type.
const DAY_TRANSFERS_TTL: std::time::Duration = std::time::Duration::from_secs(300);
/// The transaction types that move cash into or out of an account.
const TRANSFER_TYPES: [crate::types::trading::TransactionType; 8] = {
    use crate::types::trading::TransactionType as T;
    [
        T::AchReceipt,
        T::AchDisbursement,
        T::CashReceipt,
        T::CashDisbursement,
        T::ElectronicFund,
        T::WireIn,
        T::WireOut,
        T::Journal,
    ]
};

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
    /// The account's net cash transfers in (deposits less withdrawals) since
    /// the start of the New York trading day, reused for
    /// [`DAY_TRANSFERS_TTL`]. `None` when it cannot be read: an older
    /// reading could predate a deposit that hides a loss, so the daily-loss
    /// control treats the baseline as stale instead.
    async fn day_net_transfers(&self, account: &str) -> Option<rust_decimal::Decimal> {
        let now = chrono::Utc::now();
        let today = crate::paper::feed::trading_date(now);
        let cached = self.day_transfers().lock().get(account).copied();
        if let Some(net) = cached.and_then(|reading| reading.current(today)) {
            return Some(net);
        }
        let read = async {
            let midnight = crate::paper::feed::trading_day_start(now);
            let mut net = rust_decimal::Decimal::ZERO;
            // One type per request: Schwab documents `types` as a single value.
            for kind in TRANSFER_TYPES {
                let params = crate::types::trading::TransactionsParams::new(midnight, now, [kind]);
                for transaction in self.get_transactions(account, &params).await.ok()? {
                    let amount = transaction.net_amount.unwrap_or(0.0);
                    net = net.checked_add(rust_decimal::Decimal::try_from(amount).ok()?)?;
                }
            }
            Some(net)
        };
        let net = read.await?;
        self.day_transfers().lock().insert(
            account.to_owned(),
            DayTransfers {
                date: today,
                read_at: tokio::time::Instant::now(),
                net,
            },
        );
        Some(net)
    }

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
            for record in database.unresolved_records(account).await? {
                let at = chrono::DateTime::from_timestamp_millis(record.created_at)
                    .ok_or_else(|| Error::Storage("Invalid intent timestamp".into()))?;
                from = from.min(at - chrono::Duration::seconds(1));
            }
            let mut windows = vec![(from, started)];
            let mut orders = std::collections::BTreeMap::new();
            let mut pages = 0;
            // Each turn reads one page and counts it against the budget, so
            // the loop ends within the budget however its windows are cut.
            while let Some((from, to)) = windows.pop() {
                pages += 1;
                if pages > options.max_pages {
                    return Err(Error::Storage(
                        "Reconciliation page budget exhausted".into(),
                    ));
                }
                // One day inside Schwab's 60-day order list horizon. A wider
                // window is answered with what it holds, so the rest of it is
                // read next rather than silently missed.
                let end = to.min(from + chrono::Duration::days(59));
                if end < to {
                    windows.push((end, to));
                }
                let page: Vec<Value> = self
                    .request_with_query(
                        Op::recovery("reconcile_orders"),
                        reqwest::Method::GET,
                        &crate::client::account_path(account, "/orders")?,
                        &OrdersParams {
                            max_results: Some(3000),
                            ..OrdersParams::new(from, end)
                        },
                    )
                    .await?;
                if page.len() >= 3000 {
                    if (end - from).num_milliseconds() <= 1000 {
                        return Err(Error::Storage(
                            "Order history remains truncated at minimum interval".into(),
                        ));
                    }
                    let mid = from + (end - from) / 2;
                    windows.push((mid, end));
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
            // again would cost one request per working order per pass. This
            // store holds only broker order IDs, but one that was never
            // migrated (an authority on an earlier release, say) can return
            // one written another way ("007"): it is read as the order it
            // names (7), once however many ways it is stored. One that names
            // no order at all has nothing to fetch.
            let mut targets = std::collections::BTreeMap::<String, Vec<String>>::new();
            for stored in database.reconciliation_targets(account).await? {
                match crate::execution::canonical_order_id(&stored) {
                    Some(id) if !orders.contains_key(&id) => {
                        targets.entry(id).or_default().push(stored);
                    }
                    Some(_) => {}
                    None => tracing::warn!(
                        order_id = ?stored,
                        account = %crate::utils::mask_account(account),
                        "A stored order ID cannot name a broker order; not fetched"
                    ),
                }
            }
            if targets.len() > options.max_targeted_orders {
                return Err(Error::Storage(
                    "Reconciliation targeted-order budget exhausted".into(),
                ));
            }
            for (id, stored) in targets {
                let order: Value = match self
                    .request(
                        Op::recovery("reconcile_order"),
                        reqwest::Method::GET,
                        &crate::client::order_path(account, &id)?,
                    )
                    .await
                {
                    Ok(order) => order,
                    // The broker has no such order. That ends a reference
                    // this store never saw answered, whatever text it was
                    // stored as; an order it did observe vanishing still
                    // needs an operator.
                    Err(error)
                        if error.http_status() == Some(reqwest::StatusCode::NOT_FOUND)
                            && forget_placeholders(database.as_ref(), account, &stored).await? =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if broker_id(order.get("orderId")).as_deref() != Some(&id) {
                    return Err(Error::Storage("Targeted broker order ID mismatch".into()));
                }
                orders.insert(id, order);
            }
            // Read positions after orders to reduce the stale pre-fill snapshot
            // window. Broker reads are still not an atomic account snapshot.
            let mut snapshot: Value = self
                .request_with_query(
                    Op::recovery("reconcile_account"),
                    reqwest::Method::GET,
                    &crate::client::account_path(account, "")?,
                    &[("fields", "positions")],
                )
                .await?;
            if self
                .risk_provider()
                .is_some_and(|risk| risk.needs_day_transfers(account))
                && let Some(net) = self.day_net_transfers(account).await
                && let Some(fields) = snapshot.as_object_mut()
            {
                fields.insert(
                    crate::risk::DAY_TRANSFERS_KEY.into(),
                    Value::String(net.to_string()),
                );
            }
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
        // A superseded pass wrote nothing and returned Ok: the newer state
        // stands. A pass that could not finish judges the baseline it finds
        // by that baseline's generation:
        // - a newer generation's baseline is no business of this pass, and
        //   stands;
        // - a baseline of its own generation stands only if a pass that
        //   began after this one took it, and is invalidated otherwise;
        // - an older generation cannot be stored, since this pass raised the
        //   account to its own when it began. One found anyway means that
        //   assumption broke, so the baseline is invalidated, not trusted.
        if result.is_err() {
            let state = database.account_state(account).await?;
            let stands = match &state {
                Some(state) => match state.generation.cmp(&generation) {
                    Ordering::Greater => true,
                    Ordering::Equal => state.observed_at > started.timestamp_millis(),
                    Ordering::Less => false,
                },
                None => false,
            };
            if !stands {
                database.invalidate_account(account, generation).await?;
            }
            // Only after invalidating: the assertion reports a broken
            // assumption once the store has already failed closed.
            let stored = state.map(|state| state.generation);
            debug_assert!(
                stored.is_none_or(|stored| stored >= generation),
                "the account's stored generation is older than the generation {generation} \
                 of a pass that raised it when it began"
            );
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

    /// A baseline is never dated ahead of the store's clock: a pass that
    /// began earlier keeps its start, one that began up to
    /// `MAX_CLOCK_LEAD_MS` ahead counts from now, and one further ahead is
    /// refused.
    #[test]
    fn a_baseline_is_dated_no_later_than_the_store_clock() {
        let now = 1_764_000_000_000;
        assert_eq!(baseline_observed_at(now - 1, now).unwrap(), now - 1);
        assert_eq!(baseline_observed_at(now, now).unwrap(), now);
        assert_eq!(baseline_observed_at(now + 1, now).unwrap(), now);
        assert_eq!(
            baseline_observed_at(now + MAX_CLOCK_LEAD_MS, now).unwrap(),
            now
        );
        assert!(matches!(
            baseline_observed_at(now + MAX_CLOCK_LEAD_MS + 1, now),
            Err(Error::Storage(_))
        ));
    }

    /// A reading of the day's transfers is reused while it is of the same
    /// trading day and younger than `DAY_TRANSFERS_TTL`; at that age, or on
    /// the next day, it is read again.
    #[tokio::test(start_paused = true)]
    async fn a_transfers_reading_is_current_for_its_ttl_on_its_own_day() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 11, 25).unwrap();
        let reading = DayTransfers {
            date: day,
            read_at: tokio::time::Instant::now(),
            net: 7.into(),
        };
        assert_eq!(reading.current(day), Some(7.into()));
        assert_eq!(reading.current(day.succ_opt().unwrap()), None);
        let just_under = DAY_TRANSFERS_TTL - std::time::Duration::from_millis(1);
        tokio::time::advance(just_under).await;
        assert_eq!(reading.current(day), Some(7.into()));
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        assert_eq!(reading.current(day), None);
    }
}
