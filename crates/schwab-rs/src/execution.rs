//! Durable submission intent/outcome journal. Pending intents survive task
//! cancellation and process failure, and block new submissions for that account.
use crate::error::{Error, Result};
use crate::storage::{audit, meta, sql};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const SELECT_APPROVAL_CONSUMED: &str =
    "SELECT EXISTS(SELECT 1 FROM consumed_approvals WHERE key=?1)";
const SELECT_PENDING_SUBMISSION: &str =
    "SELECT id FROM submissions WHERE account=?1 AND outcome IS NULL";
const SELECT_APPROVAL_EXISTS: &str = "SELECT EXISTS(SELECT 1 FROM approvals WHERE digest=?1)";
const SELECT_APPROVAL_LIVE: &str = "SELECT EXISTS(SELECT 1 FROM approvals \
     WHERE digest=?1 AND principal=?2 AND expires_at>?3 AND consumed=0)";
const CONSUME_APPROVAL: &str = "UPDATE approvals SET consumed=1 \
     WHERE digest=?1 AND principal=?2 AND expires_at>?3 AND consumed=0";
/// A digest is approved once at a time: a new grant may replace only one
/// that lapsed unused. A live, spent or revoked approval stays as it is.
const INSERT_APPROVAL: &str = "INSERT INTO approvals(digest,principal,expires_at,operator) \
     VALUES(?1,?2,?3,?4) ON CONFLICT(digest) DO UPDATE SET principal=excluded.principal,\
     expires_at=excluded.expires_at,operator=excluded.operator \
     WHERE approvals.consumed=0 AND approvals.expires_at<=?5 \
       AND NOT EXISTS(SELECT 1 FROM consumed_approvals WHERE key=approvals.digest)";
const INSERT_CONSUMED_APPROVAL: &str = "INSERT INTO consumed_approvals VALUES(?1)";
const INSERT_CONSUMED_APPROVAL_IF_NEW: &str = "INSERT OR IGNORE INTO consumed_approvals VALUES(?1)";
const INSERT_USED_ORDER_TAG: &str = "INSERT OR IGNORE INTO used_order_tags VALUES(?1,?2)";
/// A rejected submission frees its tag: the broker holds no order for it, so
/// the identical request may be retried under the same tag. A tag the broker
/// is known to hold on some order stays spent whatever this intent's fate.
const RELEASE_ORDER_TAG: &str = "DELETE FROM used_order_tags WHERE EXISTS(\
     SELECT 1 FROM submissions s WHERE s.id=?1 AND s.account=used_order_tags.account \
       AND json_extract(s.request,'$.tag')=used_order_tags.tag) \
     AND NOT EXISTS(SELECT 1 FROM orders o WHERE o.account=used_order_tags.account \
       AND json_extract(o.payload,'$.tag')=used_order_tags.tag)";
const INSERT_SUBMISSION: &str = "INSERT INTO \
     submissions(id,account,operation,created_at,request,outcome,dedup_key,armed) \
     VALUES(?1,?2,?3,?4,?5,NULL,?6,0)";
const RESOLVE_SUBMISSION: &str =
    "UPDATE submissions SET outcome=?1 WHERE id=?2 AND outcome IS NULL";
/// Only an unresolved intent may be armed: one that recovery already
/// resolved as never dispatched must not be sent.
const ARM_SUBMISSION: &str = "UPDATE submissions SET armed=1 WHERE id=?1 AND outcome IS NULL";
/// A resolution that says the broker holds no live order for the intent
/// (refused, or settled by an operator) frees the exposure reserved for it.
const RELEASE_RESERVATION: &str =
    "UPDATE reservations SET state='released' WHERE id=?1 AND state='held'";
const SELECT_SUBMISSION_OUTCOME: &str = "SELECT outcome FROM submissions WHERE id=?1";
const SELECT_SUBMISSION_DEDUP: &str =
    "SELECT dedup_key,outcome IS NOT NULL FROM submissions WHERE id=?1";
const SELECT_APPROVAL_REVOKED: &str =
    "SELECT EXISTS(SELECT 1 FROM audit WHERE kind='approval_revoked' AND subject=?1)";
const SELECT_SUBMISSIONS: &str = "SELECT id,account,operation,created_at,request,outcome,\
     dedup_key,armed FROM submissions ORDER BY created_at,id";
/// Intents on `?1` that reconciliation must still place: unresolved, or
/// accepted without the broker's order ID.
const SELECT_UNRESOLVED_SUBMISSIONS: &str = "SELECT id,account,operation,created_at,request,\
     outcome,dedup_key,armed FROM submissions WHERE account=?1 AND (outcome IS NULL \
       OR (json_extract(outcome,'$.status')='accepted' \
         AND json_extract(outcome,'$.order_id') IS NULL)) ORDER BY created_at,id";
const SELECT_SUBMISSION_BY_TAG: &str = "SELECT id,account,operation,created_at,request,outcome,\
     dedup_key,armed FROM submissions WHERE account=?1 AND json_extract(request,'$.tag')=?2 \
     ORDER BY created_at DESC,id LIMIT 1";
const SELECT_SUBMISSION: &str = "SELECT id,account,operation,created_at,request,outcome,\
     dedup_key,armed FROM submissions WHERE id=?1";
/// Submissions on `?1` created at or after `?2`, through the
/// `submissions_account` index.
const SELECT_SUBMISSIONS_SINCE: &str = "SELECT id,account,operation,created_at,request,outcome,\
     dedup_key,armed FROM submissions WHERE account=?1 AND created_at>=?2 \
     ORDER BY created_at,id";
/// The order this store recorded only as a reference, never as the broker
/// answered it (see [`ExecutionStore::forget_placeholder`]).
const DELETE_ORDER_PLACEHOLDER: &str = "DELETE FROM orders WHERE account=?1 AND order_id=?2 \
     AND json_extract(payload,'$.status')='UNKNOWN' AND json_extract(payload,'$.orderType') IS NULL";
/// Terminal broker orders last observed before `?1` that no held
/// reservation still refers to. Their tags stay spent in `used_order_tags`.
const SELECT_PRUNABLE_ORDERS: &str = "SELECT account,order_id FROM orders WHERE observed_at<?1 \
     AND json_extract(payload,'$.status') IN ('FILLED','CANCELED','REJECTED','EXPIRED','REPLACED') \
     AND NOT EXISTS(SELECT 1 FROM reservations r JOIN submissions s ON s.id=r.id \
       WHERE r.state='held' AND s.account=orders.account \
         AND json_extract(s.outcome,'$.order_id')=orders.order_id)";
const DELETE_ORDER: &str = "DELETE FROM orders WHERE account=?1 AND order_id=?2";
/// An order's observed activities are keyed `{order_id}:{digest}`; `;`
/// sorts right after `:`, so this is exactly that order's range.
const DELETE_ORDER_EXECUTIONS: &str = "DELETE FROM executions WHERE account=?1 \
     AND execution_id>=?2 || ':' AND execution_id<?2 || ';'";
/// Released reservations whose submission is pruned or resolved before `?1`.
const PRUNE_RELEASED_RESERVATIONS: &str = "DELETE FROM reservations WHERE state='released' \
     AND NOT EXISTS(SELECT 1 FROM submissions s WHERE s.id=reservations.id \
       AND (s.outcome IS NULL OR s.created_at>=?1))";
const SELECT_AUDIT_PAGE: &str = "SELECT sequence,at,kind,subject,payload FROM audit \
     WHERE sequence>?1 ORDER BY sequence LIMIT ?2";
/// Resolved submissions older than `?1` whose audit trail is exported through
/// `?2`, hold no reservation, and are rejected/reconciled or whose broker
/// order reached a terminal status.
const PRUNE_RESOLVED_SUBMISSIONS: &str = "DELETE FROM submissions \
     WHERE outcome IS NOT NULL AND created_at<?1 \
       AND NOT EXISTS(SELECT 1 FROM audit WHERE subject=submissions.id AND sequence>?2) \
       AND NOT EXISTS(SELECT 1 FROM reservations r WHERE r.id=submissions.id AND r.state='held') \
       AND (json_extract(outcome,'$.status') IN ('rejected','reconciled') \
         OR EXISTS(SELECT 1 FROM orders o \
           WHERE o.account=submissions.account \
             AND o.order_id=json_extract(submissions.outcome,'$.order_id') \
             AND json_extract(o.payload,'$.status') \
               IN ('FILLED','CANCELED','REJECTED','EXPIRED','REPLACED')))";
/// Open broker orders plus accepted submissions whose order is not yet known
/// to be terminal.
const SELECT_RECONCILIATION_TARGETS: &str = "SELECT order_id FROM orders \
     WHERE account=?1 AND COALESCE(json_extract(payload,'$.status'),'UNKNOWN') \
       NOT IN ('FILLED','CANCELED','REJECTED','EXPIRED','REPLACED') \
     UNION \
     SELECT json_extract(s.outcome,'$.order_id') FROM submissions s \
     WHERE s.account=?1 AND json_extract(s.outcome,'$.order_id') IS NOT NULL \
       AND NOT EXISTS(SELECT 1 FROM orders o \
         WHERE o.account=s.account AND o.order_id=json_extract(s.outcome,'$.order_id') \
           AND json_extract(o.payload,'$.status') \
             IN ('FILLED','CANCELED','REJECTED','EXPIRED','REPLACED'))";
const OPERATOR_HALT_KEY: &str = "operator_halt";

/// Admission check shared by every dispatch path, live and paper: fail closed
/// while an operator halt is recorded in the same state file.
pub(crate) fn ensure_not_halted(c: &rusqlite::Connection) -> Result<()> {
    if meta::exists(c, OPERATOR_HALT_KEY)? {
        return Err(Error::Halted("Operator trading halt is active".into()));
    }
    Ok(())
}

/// Records the halt (`Some`) or clears it (`None`) inside `tx` and audits the
/// change under `operator`: the person who asked for it, or the automatic
/// control that tripped it.
pub(crate) fn record_operator_halt(
    tx: &rusqlite::Connection,
    reason: Option<&str>,
    operator: &str,
) -> Result<()> {
    match reason {
        Some(reason) => meta::put(tx, OPERATOR_HALT_KEY, reason)?,
        None => meta::delete(tx, OPERATOR_HALT_KEY)?,
    }
    audit(
        tx,
        "operator_halt_changed",
        operator,
        &serde_json::json!({"reason":reason}),
    )
}

/// Trips the halt for an automatic control unless one is already recorded,
/// so a standing operator halt keeps its reason; either way the caller must
/// fail closed with `reason`.
pub(crate) fn trip_halt(tx: &rusqlite::Connection, reason: &str, control: &str) -> Result<()> {
    if meta::exists(tx, OPERATOR_HALT_KEY)? {
        return Ok(());
    }
    record_operator_halt(tx, Some(reason), control)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmissionRecord {
    pub attempt_id: String,
    pub account: String,
    pub operation: String,
    pub request: serde_json::Value,
    pub created_at: i64,
    pub outcome: Option<SubmissionOutcome>,
    #[serde(default)]
    pub dedup_key: Option<String>,
    /// Whether the request may have left this host: set just before it is
    /// transmitted. An unresolved intent that was never armed provably
    /// reached no broker, and recovery resolves it as rejected; records
    /// from before this marker existed read as armed.
    #[serde(default = "armed_by_default")]
    pub armed: bool,
}
fn armed_by_default() -> bool {
    true
}
type SubmissionRow = (
    String,
    String,
    String,
    i64,
    String,
    Option<String>,
    Option<String>,
    bool,
);
fn submission_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SubmissionRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
    ))
}
fn submission_record(row: SubmissionRow) -> Result<SubmissionRecord> {
    let (attempt_id, account, operation, created_at, request, outcome, dedup_key, armed) = row;
    Ok(SubmissionRecord {
        attempt_id,
        account,
        operation,
        created_at,
        request: serde_json::from_str(&request)?,
        outcome: outcome.map(|s| serde_json::from_str(&s)).transpose()?,
        dedup_key,
        armed,
    })
}
fn query_submissions(
    c: &rusqlite::Connection,
    sql_text: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<SubmissionRecord>> {
    let mut query = c.prepare(sql_text).map_err(sql)?;
    let rows = query.query_map(params, submission_row).map_err(sql)?;
    rows.map(|row| submission_record(row.map_err(sql)?))
        .collect()
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SubmissionOutcome {
    Accepted {
        order_id: Option<String>,
    },
    /// The broker holds no order for this intent: Schwab refused it, or it
    /// was refused locally (risk, halt, deadline) before anything was sent.
    /// The caller's error says which; either way the tag is free again.
    Rejected,
    /// Explicit reconciliation by a trusted caller, after querying the broker.
    Reconciled {
        evidence: String,
    },
}
#[derive(Debug, Clone)]
pub struct OrderJournal {
    database: crate::storage::StateDatabase,
}
impl OrderJournal {
    pub fn new(path: PathBuf) -> Self {
        Self {
            database: crate::storage::StateDatabase::new(path),
        }
    }
    pub fn database(&self) -> &crate::storage::StateDatabase {
        &self.database
    }
    pub fn path(&self) -> &Path {
        self.database.path()
    }
    pub async fn begin(
        &self,
        account: &str,
        operation: &str,
        request: serde_json::Value,
    ) -> Result<String> {
        self.begin_once(account, operation, request, None).await
    }
    pub async fn begin_once(
        &self,
        account: &str,
        operation: &str,
        request: serde_json::Value,
        dedup_key: Option<String>,
    ) -> Result<String> {
        let account = account.to_owned();
        let operation = operation.to_owned();
        self.database
            .run(move |conn| {
                use rusqlite::{OptionalExtension, params};
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                if let Some(key) = &dedup_key {
                    let used: bool = tx
                        .query_row(SELECT_APPROVAL_CONSUMED, [key], |r| r.get(0))
                        .map_err(sql)?;
                    if used {
                        return Err(Error::Duplicate("Execution intent already consumed".into()));
                    }
                }
                let pending: Option<String> = tx
                    .query_row(SELECT_PENDING_SUBMISSION, [&account], |r| r.get(0))
                    .optional()
                    .map_err(sql)?;
                if let Some(attempt_id) = pending {
                    return Err(Error::ReconciliationRequired { attempt_id });
                }
                if let Some(key) = &dedup_key {
                    let configured: bool = tx
                        .query_row(SELECT_APPROVAL_EXISTS, [key], |r| r.get(0))
                        .map_err(sql)?;
                    if configured
                        && tx
                            .execute(
                                CONSUME_APPROVAL,
                                params![key, account, chrono::Utc::now().timestamp()],
                            )
                            .map_err(sql)?
                            != 1
                    {
                        return Err(Error::ApprovalInvalid(
                            "Approval expired, revoked or belongs to another principal".into(),
                        ));
                    }
                }
                if let Some(tag) = request.get("tag").and_then(serde_json::Value::as_str) {
                    if tag.trim().is_empty() {
                        return Err(Error::InvalidParameter(
                            "Order tag must not be empty".into(),
                        ));
                    }
                    if tx
                        .execute(INSERT_USED_ORDER_TAG, params![account, tag])
                        .map_err(sql)?
                        != 1
                    {
                        return Err(Error::Duplicate(
                            "Order tag already belongs to an accepted or unresolved submission \
                             for this account"
                                .into(),
                        ));
                    }
                }
                let id = uuid::Uuid::new_v4().to_string();
                let record = SubmissionRecord {
                    attempt_id: id.clone(),
                    account,
                    operation,
                    request,
                    created_at: chrono::Utc::now().timestamp_millis(),
                    outcome: None,
                    dedup_key,
                    armed: false,
                };
                tx.execute(
                    INSERT_SUBMISSION,
                    params![
                        record.attempt_id,
                        record.account,
                        record.operation,
                        record.created_at,
                        serde_json::to_string(&record.request)?,
                        record.dedup_key
                    ],
                )
                .map_err(sql)?;
                if let Some(key) = &record.dedup_key {
                    tx.execute(INSERT_CONSUMED_APPROVAL, [key]).map_err(sql)?;
                }
                audit(&tx, "intent", &id, &serde_json::to_value(&record)?)?;
                tx.commit().map_err(sql)?;
                Ok(id)
            })
            .await
    }
    /// Resolve a pending submission. `Rejected` asserts the broker holds no
    /// order for it and releases its tag for a retry; `Accepted` and
    /// `Reconciled` keep the tag consumed. `Rejected` and `Reconciled` also
    /// release the risk reserved for it in the same transaction: no live
    /// order is linked to the intent any more, and any order an operator
    /// found is counted from the broker's own order list. `Accepted` also
    /// succeeds when reconciliation already accepted the submission under
    /// the same order ID (or this call has none), and `Rejected` when
    /// recovery already resolved it as never dispatched.
    pub async fn finish(&self, id: &str, outcome: SubmissionOutcome) -> Result<()> {
        if let SubmissionOutcome::Reconciled { evidence } = &outcome
            && evidence.trim().is_empty()
        {
            return Err(Error::InvalidParameter(
                "Reconciliation evidence is required".into(),
            ));
        }
        let id = id.to_owned();
        self.database
            .run(move |conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                let changed = tx
                    .execute(
                        RESOLVE_SUBMISSION,
                        rusqlite::params![serde_json::to_string(&outcome)?, id],
                    )
                    .map_err(sql)?;
                if changed != 1 {
                    use rusqlite::OptionalExtension;
                    let recorded: Option<String> = tx
                        .query_row(SELECT_SUBMISSION_OUTCOME, [&id], |r| r.get(0))
                        .optional()
                        .map_err(sql)?
                        .flatten();
                    let Some(recorded) = recorded else {
                        return Err(Error::InvalidParameter("Submission is unknown".into()));
                    };
                    // Reconciliation can match the order by its tag between
                    // the broker's answer and this call; the same answer is
                    // not a conflict.
                    let recorded: SubmissionOutcome = serde_json::from_str(&recorded)?;
                    return match (&recorded, &outcome) {
                        (
                            SubmissionOutcome::Accepted { order_id: Some(a) },
                            SubmissionOutcome::Accepted { order_id: b },
                        ) if b.as_ref().is_none_or(|b| b == a) => Ok(()),
                        (SubmissionOutcome::Rejected, SubmissionOutcome::Rejected) => Ok(()),
                        _ => Err(Error::Duplicate("Submission is already resolved".into())),
                    };
                }
                if matches!(outcome, SubmissionOutcome::Rejected) {
                    tx.execute(RELEASE_ORDER_TAG, [&id]).map_err(sql)?;
                }
                if !matches!(outcome, SubmissionOutcome::Accepted { .. })
                    && tx.execute(RELEASE_RESERVATION, [&id]).map_err(sql)? == 1
                {
                    audit(&tx, "risk_released", &id, &serde_json::json!({}))?;
                }
                audit(&tx, "outcome", &id, &serde_json::to_value(&outcome)?)?;
                tx.commit().map_err(sql)?;
                Ok(())
            })
            .await
    }
    pub async fn records(&self) -> Result<Vec<SubmissionRecord>> {
        self.database
            .run(|conn| query_submissions(conn, SELECT_SUBMISSIONS, []))
            .await
    }
    /// Submissions on `account` created at or after `since` (Unix
    /// milliseconds), oldest first.
    pub async fn records_since(&self, account: &str, since: i64) -> Result<Vec<SubmissionRecord>> {
        let account = account.to_owned();
        self.database
            .run(move |conn| {
                query_submissions(
                    conn,
                    SELECT_SUBMISSIONS_SINCE,
                    rusqlite::params![account, since],
                )
            })
            .await
    }
    /// Append an operator-visible event to the audit trail (for actions the
    /// journal does not otherwise record, such as stream control).
    pub async fn record_event(
        &self,
        kind: &str,
        subject: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        let kind = kind.to_owned();
        let subject = subject.to_owned();
        self.database
            .run(move |c| audit(c, &kind, &subject, &payload))
            .await
    }
    /// One submission by attempt ID.
    pub async fn record(&self, id: &str) -> Result<Option<SubmissionRecord>> {
        let id = id.to_owned();
        self.database
            .run(move |conn| Ok(query_submissions(conn, SELECT_SUBMISSION, [id])?.pop()))
            .await
    }
    /// Mark the intent as possibly transmitted. Call it after every local
    /// admission check and immediately before the request leaves: an
    /// unresolved intent that was never armed is provably unsent, and
    /// recovery resolves it as rejected. Fails without arming when the
    /// intent is already resolved, so a recovered intent is never sent.
    pub async fn arm(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.database
            .run(move |c| {
                let tx = c
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                if tx.execute(ARM_SUBMISSION, [&id]).map_err(sql)? != 1 {
                    return Err(Error::Duplicate(
                        "Submission was resolved before dispatch; it was not sent".into(),
                    ));
                }
                audit(&tx, "armed", &id, &serde_json::json!({}))?;
                tx.commit().map_err(sql)
            })
            .await
    }
    /// Delete terminal broker orders (and their observed activities) last
    /// seen before `before` (ms since the epoch), and released risk
    /// reservations of submissions resolved before it. The tags of pruned
    /// orders stay spent; working orders, held reservations and audit
    /// events are kept. Returns the number of orders removed.
    pub async fn prune_history(&self, before: i64) -> Result<usize> {
        self.database
            .run(move |c| {
                let tx = c
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                let orders = {
                    let mut stmt = tx.prepare(SELECT_PRUNABLE_ORDERS).map_err(sql)?;
                    stmt.query_map([before], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })
                    .map_err(sql)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(sql)?
                };
                for (account, order_id) in &orders {
                    tx.execute(DELETE_ORDER, [account, order_id]).map_err(sql)?;
                    tx.execute(DELETE_ORDER_EXECUTIONS, [account, order_id])
                        .map_err(sql)?;
                }
                let reservations = tx
                    .execute(PRUNE_RELEASED_RESERVATIONS, [before])
                    .map_err(sql)?;
                audit(
                    &tx,
                    "history_pruned",
                    "state",
                    &serde_json::json!({
                        "before": before,
                        "orders": orders.len(),
                        "reservations": reservations,
                    }),
                )?;
                tx.commit().map_err(sql)?;
                Ok(orders.len())
            })
            .await
    }
    /// Ordered, bounded pages for an external immutable audit sink.
    pub async fn audit_page(&self, after: i64, limit: u32) -> Result<Vec<AuditEvent>> {
        self.database
            .run(move |conn| {
                let mut stmt = conn.prepare(SELECT_AUDIT_PAGE).map_err(sql)?;
                let rows = stmt
                    .query_map(rusqlite::params![after, limit.clamp(1, 10000)], |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get::<_, String>(4)?,
                        ))
                    })
                    .map_err(sql)?;
                rows.map(|row| {
                    let (sequence, at, kind, subject, payload) = row.map_err(sql)?;
                    Ok(AuditEvent {
                        sequence,
                        at,
                        kind,
                        subject,
                        payload: serde_json::from_str(&payload)?,
                    })
                })
                .collect()
            })
            .await
    }
    /// Explicit retention after durable export acknowledgement. Pending records,
    /// consumed approval tombstones and all audit events remain intact.
    pub async fn prune_resolved(&self, before: i64, exported_through: i64) -> Result<usize> {
        self.database
            .run(move |conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                let pruned = tx
                    .execute(
                        PRUNE_RESOLVED_SUBMISSIONS,
                        rusqlite::params![before, exported_through],
                    )
                    .map_err(sql)?;
                audit(
                    &tx,
                    "submissions_pruned",
                    "state",
                    &serde_json::json!({
                        "before": before,
                        "exported_through": exported_through,
                        "submissions": pruned,
                    }),
                )?;
                tx.commit().map_err(sql)?;
                Ok(pruned)
            })
            .await
    }
}
/// An intent a principal asked for that needed an operator's approval, as
/// the operator reviews it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct IntentRecord {
    pub digest: String,
    pub principal: String,
    /// The tool (`place_order` or `replace_order`).
    pub tool: String,
    /// The arguments as last sent, rationale included.
    pub arguments: serde_json::Value,
    pub first_seen: i64,
    pub last_seen: i64,
    /// How many times it was asked for.
    pub seen: i64,
    pub state: IntentState,
}

/// Where an intent stands with the operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum IntentState {
    /// Not approved yet.
    Awaiting,
    /// Approved, usable until `expires_at` (Unix seconds).
    Approved { operator: String, expires_at: i64 },
    /// Its approval was used on an order.
    Spent,
    /// Its approval was revoked.
    Revoked,
    /// Its approval lapsed unused; it can be approved again.
    Lapsed,
}

const UPSERT_INTENT: &str = "INSERT INTO intents(digest,principal,tool,arguments,first_seen,last_seen) \
     VALUES(?1,?2,?3,?4,?5,?5) ON CONFLICT(digest) DO UPDATE SET \
     arguments=excluded.arguments,last_seen=excluded.last_seen,seen=intents.seen+1 RETURNING seen";
const SELECT_INTENTS: &str = "SELECT i.digest,i.principal,i.tool,i.arguments,i.first_seen,\
     i.last_seen,i.seen,a.operator,a.expires_at,a.consumed,\
     EXISTS(SELECT 1 FROM audit WHERE kind='approval_revoked' AND subject=i.digest) \
     FROM intents i LEFT JOIN approvals a ON a.digest=i.digest \
     WHERE (?1 IS NULL OR i.digest=?1) AND i.last_seen>=?2 ORDER BY i.last_seen DESC LIMIT ?3";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub sequence: i64,
    pub at: i64,
    pub kind: String,
    pub subject: String,
    pub payload: serde_json::Value,
}

impl OrderJournal {
    pub async fn approve(
        &self,
        principal: &str,
        digest: &str,
        operator: &str,
        expires_at: i64,
    ) -> Result<()> {
        if principal == operator
            || operator.trim().is_empty()
            || digest.len() != 64
            || !digest.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(Error::InvalidParameter(
                "Approval requires a distinct operator and a SHA-256 intent digest".into(),
            ));
        }
        let now = chrono::Utc::now().timestamp();
        if expires_at <= now || expires_at > now + 3600 {
            return Err(Error::InvalidParameter(
                "Approval must expire within one hour".into(),
            ));
        }
        let principal = principal.to_owned();
        let digest = digest.to_owned();
        let operator = operator.to_owned();
        self.database
            .run(move |c| {
                let tx = c
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                let now = chrono::Utc::now().timestamp();
                if tx
                    .execute(
                        INSERT_APPROVAL,
                        rusqlite::params![digest, principal, expires_at, operator, now],
                    )
                    .map_err(sql)?
                    != 1
                {
                    return Err(Error::Duplicate(
                        "This intent is already approved, was spent on an order, or was \
                         revoked; only a lapsed approval can be granted again"
                            .into(),
                    ));
                }
                audit(
                    &tx,
                    "approval_granted",
                    &digest,
                    &serde_json::json!({
                        "principal": principal,
                        "operator": operator,
                        "expires_at": expires_at
                    }),
                )?;
                tx.commit().map_err(sql)?;
                Ok(())
            })
            .await
    }
    /// Note that `principal` asked for intent `digest` (`tool` with
    /// `arguments`) and it needs an operator's approval, so the operator
    /// can review what it is: see [`intents`](Self::intents). Returns how
    /// many times it has now been asked for, so a caller can tell the first.
    pub async fn record_intent(
        &self,
        principal: &str,
        digest: &str,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> Result<i64> {
        let values = (
            digest.to_owned(),
            principal.to_owned(),
            tool.to_owned(),
            serde_json::to_string(arguments)?,
        );
        self.database
            .run(move |c| {
                c.query_row(
                    UPSERT_INTENT,
                    rusqlite::params![
                        values.0,
                        values.1,
                        values.2,
                        values.3,
                        chrono::Utc::now().timestamp_millis()
                    ],
                    |r| r.get(0),
                )
                .map_err(sql)
            })
            .await
    }
    /// Intents asked for since `since` (ms since the epoch), newest first,
    /// at most `limit`, each with where it stands; only `digest` if given.
    pub async fn intents(
        &self,
        digest: Option<&str>,
        since: i64,
        limit: u32,
    ) -> Result<Vec<IntentRecord>> {
        let digest = digest.map(str::to_owned);
        self.database
            .run(move |c| {
                let now = chrono::Utc::now().timestamp();
                let mut stmt = c.prepare(SELECT_INTENTS).map_err(sql)?;
                let rows = stmt
                    .query_map(rusqlite::params![digest, since, limit], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, i64>(4)?,
                            r.get::<_, i64>(5)?,
                            r.get::<_, i64>(6)?,
                            r.get::<_, Option<String>>(7)?,
                            r.get::<_, Option<i64>>(8)?,
                            r.get::<_, Option<bool>>(9)?,
                            r.get::<_, bool>(10)?,
                        ))
                    })
                    .map_err(sql)?;
                rows.map(|row| {
                    let (
                        digest,
                        principal,
                        tool,
                        arguments,
                        first_seen,
                        last_seen,
                        seen,
                        operator,
                        expires_at,
                        consumed,
                        revoked,
                    ) = row.map_err(sql)?;
                    let state = match (operator, expires_at, consumed) {
                        _ if revoked => IntentState::Revoked,
                        (Some(_), Some(_), Some(true)) => IntentState::Spent,
                        (Some(operator), Some(expires_at), _) if expires_at > now => {
                            IntentState::Approved {
                                operator,
                                expires_at,
                            }
                        }
                        (Some(_), Some(_), _) => IntentState::Lapsed,
                        _ => IntentState::Awaiting,
                    };
                    Ok(IntentRecord {
                        digest,
                        principal,
                        tool,
                        arguments: serde_json::from_str(&arguments)?,
                        first_seen,
                        last_seen,
                        seen,
                        state,
                    })
                })
                .collect()
            })
            .await
    }
    pub async fn approval_operator(&self, digest: &str) -> Result<Option<String>> {
        let digest = digest.to_owned();
        self.database
            .run(move |c| {
                use rusqlite::OptionalExtension;
                c.query_row(
                    "SELECT operator FROM approvals WHERE digest=?1",
                    [digest],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)
            })
            .await
    }
    pub async fn is_approved(&self, principal: &str, digest: &str) -> Result<bool> {
        let principal = principal.to_owned();
        let digest = digest.to_owned();
        self.database
            .run(move |c| {
                c.query_row(
                    SELECT_APPROVAL_LIVE,
                    rusqlite::params![digest, principal, chrono::Utc::now().timestamp()],
                    |r| r.get(0),
                )
                .map_err(sql)
            })
            .await
    }
    /// Give back the one-use approval that submission `id` consumed, when the
    /// broker never saw it, so the same intent can be submitted again while
    /// the approval lasts. The caller vouches that nothing was sent (the SDK
    /// reports anything else as `SubmissionUnknown`), and `id`'s dedup key
    /// must be an approval digest, not an idempotency key. The submission
    /// must be resolved; its order tag is released with the approval, and a
    /// revoked approval stays revoked. Audited as `approval_refunded`.
    pub async fn refund_approval(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.database
            .run(move |c| {
                use rusqlite::OptionalExtension;
                let tx = c
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                let row: Option<(Option<String>, bool)> = tx
                    .query_row(SELECT_SUBMISSION_DEDUP, [&id], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })
                    .optional()
                    .map_err(sql)?;
                let (digest, resolved) =
                    row.ok_or_else(|| Error::InvalidParameter("Submission is unknown".into()))?;
                let digest = digest.ok_or_else(|| {
                    Error::InvalidParameter("Submission consumed no approval".into())
                })?;
                if !resolved {
                    return Err(Error::InvalidParameter(
                        "Submission is unresolved; reconcile it first".into(),
                    ));
                }
                let revoked: bool = tx
                    .query_row(SELECT_APPROVAL_REVOKED, [&digest], |r| r.get(0))
                    .map_err(sql)?;
                if !revoked {
                    tx.execute("DELETE FROM consumed_approvals WHERE key=?1", [&digest])
                        .map_err(sql)?;
                    tx.execute("UPDATE approvals SET consumed=0 WHERE digest=?1", [&digest])
                        .map_err(sql)?;
                }
                tx.execute(RELEASE_ORDER_TAG, [&id]).map_err(sql)?;
                // The digest is unique across submissions; the retry takes it.
                tx.execute("UPDATE submissions SET dedup_key=NULL WHERE id=?1", [&id])
                    .map_err(sql)?;
                audit(
                    &tx,
                    "approval_refunded",
                    &id,
                    &serde_json::json!({"digest": digest, "revoked": revoked}),
                )?;
                tx.commit().map_err(sql)
            })
            .await
    }
    /// Release the risk reserved for resolved submission `id`, on an
    /// operator's evidence (its broker order is gone from view, say).
    /// Refused while the submission is unresolved: its order may be live.
    /// Audited as `risk_released_by_operator`.
    pub async fn release_reservation(
        &self,
        id: &str,
        operator: &str,
        evidence: &str,
    ) -> Result<()> {
        if operator.trim().is_empty() || evidence.trim().is_empty() {
            return Err(Error::InvalidParameter(
                "Operator and evidence are required".into(),
            ));
        }
        let (id, operator, evidence) = (id.to_owned(), operator.to_owned(), evidence.to_owned());
        self.database
            .run(move |c| {
                use rusqlite::OptionalExtension;
                let tx = c
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                let resolved: Option<bool> = tx
                    .query_row(
                        "SELECT outcome IS NOT NULL FROM submissions WHERE id=?1",
                        [&id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(sql)?;
                match resolved {
                    None => return Err(Error::InvalidParameter("Submission is unknown".into())),
                    Some(false) => {
                        return Err(Error::InvalidParameter(
                            "Submission is unresolved; its order may be live. Reconcile it first"
                                .into(),
                        ));
                    }
                    Some(true) => {}
                }
                if tx.execute(RELEASE_RESERVATION, [&id]).map_err(sql)? != 1 {
                    return Err(Error::InvalidParameter(
                        "Submission holds no reservation".into(),
                    ));
                }
                audit(
                    &tx,
                    "risk_released_by_operator",
                    &id,
                    &serde_json::json!({"operator": operator, "evidence": evidence}),
                )?;
                tx.commit().map_err(sql)
            })
            .await
    }
    pub async fn revoke_approval(&self, digest: &str, operator: &str) -> Result<()> {
        let digest = digest.to_owned();
        let operator = operator.to_owned();
        self.database
            .run(move |c| {
                let tx = c
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                tx.execute("UPDATE approvals SET consumed=1 WHERE digest=?1", [&digest])
                    .map_err(sql)?;
                // Tombstone also revokes an approval originally supplied in policy.
                tx.execute(INSERT_CONSUMED_APPROVAL_IF_NEW, [&digest])
                    .map_err(sql)?;
                audit(
                    &tx,
                    "approval_revoked",
                    &digest,
                    &serde_json::json!({"operator":operator}),
                )?;
                tx.commit().map_err(sql)?;
                Ok(())
            })
            .await
    }
}

/// Atomic storage boundary for deployments using a remote transactional ledger.
/// Implementations must enforce one pending intent per account and permanent
/// deduplication tombstones in the same transaction as intent insertion.
#[async_trait::async_trait]
pub trait ExecutionStore: Send + Sync {
    async fn begin_once(
        &self,
        account: &str,
        operation: &str,
        request: serde_json::Value,
        dedup_key: Option<String>,
    ) -> Result<String>;
    async fn finish(&self, id: &str, outcome: SubmissionOutcome) -> Result<()>;
    /// Mark an unresolved intent as possibly transmitted, immediately before
    /// it leaves; fail, without arming, if it is already resolved. See
    /// [`OrderJournal::arm`].
    async fn arm(&self, id: &str) -> Result<()>;
    async fn records(&self) -> Result<Vec<SubmissionRecord>>;
    /// The account's intents reconciliation must still place: unresolved,
    /// or accepted without a broker order ID. The default filters
    /// [`records`](Self::records); stores that can query should.
    async fn unresolved_records(&self, account: &str) -> Result<Vec<SubmissionRecord>> {
        Ok(self
            .records()
            .await?
            .into_iter()
            .filter(|r| {
                r.account == account
                    && matches!(
                        r.outcome,
                        None | Some(SubmissionOutcome::Accepted { order_id: None })
                    )
            })
            .collect())
    }
    /// The latest submission on `account` whose request carried order tag
    /// `tag`. The default scans [`records`](Self::records).
    async fn submission_by_tag(
        &self,
        account: &str,
        tag: &str,
    ) -> Result<Option<SubmissionRecord>> {
        Ok(self
            .records()
            .await?
            .into_iter()
            .filter(|r| {
                r.account == account
                    && r.request.get("tag").and_then(serde_json::Value::as_str) == Some(tag)
            })
            .max_by_key(|r| r.created_at))
    }
    /// Drop `order_id` if this store holds it only as a reference it never
    /// saw the broker answer for (the broker then said it has no such
    /// order). Returns whether one was dropped; an observed order is kept.
    async fn forget_placeholder(&self, account: &str, order_id: &str) -> Result<bool>;
    /// Whether order tag `tag` is spent on `account`: an accepted or
    /// unresolved submission carries it, or the broker holds an order
    /// under it. A tag that is not spent reached no venue through this
    /// store.
    async fn tag_spent(&self, account: &str, tag: &str) -> Result<bool>;
    async fn invalidate_account(&self, account: &str, generation: u64) -> Result<()>;
    async fn account_state(
        &self,
        account: &str,
    ) -> Result<Option<crate::reconciliation::AccountState>>;
    async fn reconcile_snapshot(
        &self,
        account: &str,
        generation: u64,
        started_at: i64,
        snapshot: serde_json::Value,
        orders: Vec<serde_json::Value>,
    ) -> Result<crate::reconciliation::ReconciliationReport>;
    async fn audit_page(&self, after: i64, limit: u32) -> Result<Vec<AuditEvent>>;
    async fn healthcheck(&self) -> Result<()>;
    async fn pending_count(&self) -> Result<usize>;
    /// Unresolved submissions on these accounts: what blocks their next
    /// submission. The default counts every account, which can only
    /// over-report; stores that can filter should.
    async fn pending_count_for(&self, accounts: &[String]) -> Result<usize> {
        let _ = accounts;
        self.pending_count().await
    }
    async fn reconciliation_targets(&self, account: &str) -> Result<Vec<String>>;
    async fn track_order(&self, account: &str, order_id: &str) -> Result<()>;
    /// Store the broker's current view of one order (its payload as
    /// `GET .../orders/{id}` returns it), as a reconciliation pass would.
    async fn record_order(&self, account: &str, order: serde_json::Value) -> Result<()>;
    async fn operator_halt(&self) -> Result<Option<String>>;
    async fn set_operator_halt(&self, reason: Option<String>, operator: &str) -> Result<()>;
}
#[async_trait::async_trait]
impl ExecutionStore for OrderJournal {
    async fn reconciliation_targets(&self, account: &str) -> Result<Vec<String>> {
        let account = account.to_owned();
        self.database
            .run(move |c| {
                let mut stmt = c.prepare(SELECT_RECONCILIATION_TARGETS).map_err(sql)?;
                stmt.query_map([account], |r| r.get(0))
                    .map_err(sql)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(sql)
            })
            .await
    }
    async fn track_order(&self, account: &str, order_id: &str) -> Result<()> {
        if order_id.is_empty() || !order_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::InvalidParameter(
                "Numeric broker order ID required".into(),
            ));
        }
        let account = account.to_owned();
        let id = order_id.to_owned();
        self.database
            .run(move |c| {
                c.execute(
                    "INSERT OR IGNORE INTO orders VALUES(?1,?2,0,?3)",
                    rusqlite::params![
                        account,
                        id,
                        serde_json::json!({"orderId":id,"status":"UNKNOWN"}).to_string()
                    ],
                )
                .map_err(sql)?;
                Ok(())
            })
            .await
    }
    async fn record_order(&self, account: &str, order: serde_json::Value) -> Result<()> {
        if crate::reconciliation::broker_id(order.get("orderId")).is_none() {
            return Err(Error::InvalidParameter(
                "Broker order payload has no order ID".into(),
            ));
        }
        let account = account.to_owned();
        self.database
            .run(move |c| {
                let tx = c
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                let mut activities = 0;
                crate::reconciliation::persist_order(
                    &tx,
                    &account,
                    &order,
                    chrono::Utc::now().timestamp_millis(),
                    &mut activities,
                )?;
                tx.commit().map_err(sql)
            })
            .await
    }
    async fn operator_halt(&self) -> Result<Option<String>> {
        self.database.run(|c| meta::get(c, OPERATOR_HALT_KEY)).await
    }
    async fn set_operator_halt(&self, reason: Option<String>, operator: &str) -> Result<()> {
        if operator.trim().is_empty() || reason.as_ref().is_some_and(|r| r.trim().is_empty()) {
            return Err(Error::InvalidParameter(
                "Operator and halt reason must be nonempty".into(),
            ));
        }
        let operator = operator.to_owned();
        self.database
            .run(move |c| {
                let tx = c
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                record_operator_halt(&tx, reason.as_deref(), &operator)?;
                tx.commit().map_err(sql)
            })
            .await
    }
    async fn begin_once(
        &self,
        account: &str,
        operation: &str,
        request: serde_json::Value,
        dedup_key: Option<String>,
    ) -> Result<String> {
        OrderJournal::begin_once(self, account, operation, request, dedup_key).await
    }
    async fn finish(&self, id: &str, outcome: SubmissionOutcome) -> Result<()> {
        OrderJournal::finish(self, id, outcome).await
    }
    async fn arm(&self, id: &str) -> Result<()> {
        OrderJournal::arm(self, id).await
    }
    async fn records(&self) -> Result<Vec<SubmissionRecord>> {
        OrderJournal::records(self).await
    }
    async fn unresolved_records(&self, account: &str) -> Result<Vec<SubmissionRecord>> {
        let account = account.to_owned();
        self.database
            .run(move |c| query_submissions(c, SELECT_UNRESOLVED_SUBMISSIONS, [account]))
            .await
    }
    async fn submission_by_tag(
        &self,
        account: &str,
        tag: &str,
    ) -> Result<Option<SubmissionRecord>> {
        let account = account.to_owned();
        let tag = tag.to_owned();
        self.database
            .run(move |c| Ok(query_submissions(c, SELECT_SUBMISSION_BY_TAG, [account, tag])?.pop()))
            .await
    }
    async fn tag_spent(&self, account: &str, tag: &str) -> Result<bool> {
        let account = account.to_owned();
        let tag = tag.to_owned();
        self.database
            .run(move |c| {
                c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM used_order_tags WHERE account=?1 AND tag=?2)",
                    [account, tag],
                    |r| r.get(0),
                )
                .map_err(sql)
            })
            .await
    }
    async fn forget_placeholder(&self, account: &str, order_id: &str) -> Result<bool> {
        let account = account.to_owned();
        let order_id = order_id.to_owned();
        self.database
            .run(move |c| {
                let tx = c
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                let dropped = tx
                    .execute(DELETE_ORDER_PLACEHOLDER, [&account, &order_id])
                    .map_err(sql)?
                    == 1;
                if dropped {
                    audit(
                        &tx,
                        "order_placeholder_dropped",
                        &order_id,
                        &serde_json::json!({"account": account}),
                    )?;
                }
                tx.commit().map_err(sql)?;
                Ok(dropped)
            })
            .await
    }
    async fn invalidate_account(&self, account: &str, generation: u64) -> Result<()> {
        self.database.invalidate_account(account, generation).await
    }
    async fn account_state(
        &self,
        account: &str,
    ) -> Result<Option<crate::reconciliation::AccountState>> {
        self.database.account_state(account).await
    }
    async fn reconcile_snapshot(
        &self,
        account: &str,
        generation: u64,
        started_at: i64,
        snapshot: serde_json::Value,
        orders: Vec<serde_json::Value>,
    ) -> Result<crate::reconciliation::ReconciliationReport> {
        self.database
            .reconcile_snapshot(account, generation, started_at, snapshot, orders)
            .await
    }
    async fn audit_page(&self, after: i64, limit: u32) -> Result<Vec<AuditEvent>> {
        OrderJournal::audit_page(self, after, limit).await
    }
    async fn pending_count(&self) -> Result<usize> {
        self.database
            .run(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM submissions WHERE outcome IS NULL",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .map(|n| n as usize)
                .map_err(sql)
            })
            .await
    }
    async fn pending_count_for(&self, accounts: &[String]) -> Result<usize> {
        let accounts = serde_json::to_string(accounts)?;
        self.database
            .run(move |c| {
                c.query_row(
                    "SELECT COUNT(*) FROM submissions WHERE outcome IS NULL \
                     AND account IN (SELECT value FROM json_each(?1))",
                    [accounts],
                    |r| r.get::<_, i64>(0),
                )
                .map(|n| n as usize)
                .map_err(sql)
            })
            .await
    }
    async fn healthcheck(&self) -> Result<()> {
        self.database
            .run(|c| {
                let tx = c
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                tx.commit().map_err(sql)
            })
            .await
    }
}
