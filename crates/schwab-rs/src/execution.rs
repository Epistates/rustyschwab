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
const INSERT_APPROVAL: &str =
    "INSERT INTO approvals(digest,principal,expires_at,operator) VALUES(?1,?2,?3,?4)";
const INSERT_CONSUMED_APPROVAL: &str = "INSERT INTO consumed_approvals VALUES(?1)";
const INSERT_CONSUMED_APPROVAL_IF_NEW: &str = "INSERT OR IGNORE INTO consumed_approvals VALUES(?1)";
const INSERT_USED_ORDER_TAG: &str = "INSERT OR IGNORE INTO used_order_tags VALUES(?1,?2)";
/// A rejected submission frees its tag: the broker holds no order for it, so
/// the identical request may be retried under the same tag.
const RELEASE_ORDER_TAG: &str = "DELETE FROM used_order_tags WHERE EXISTS(\
     SELECT 1 FROM submissions s WHERE s.id=?1 AND s.account=used_order_tags.account \
       AND json_extract(s.request,'$.tag')=used_order_tags.tag)";
const INSERT_SUBMISSION: &str = "INSERT INTO submissions VALUES(?1,?2,?3,?4,?5,NULL,?6)";
const RESOLVE_SUBMISSION: &str =
    "UPDATE submissions SET outcome=?1 WHERE id=?2 AND outcome IS NULL";
const SELECT_SUBMISSION_OUTCOME: &str = "SELECT outcome FROM submissions WHERE id=?1";
const SELECT_SUBMISSIONS: &str = "SELECT id,account,operation,created_at,request,outcome,dedup_key \
     FROM submissions ORDER BY created_at,id";
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
    /// `Reconciled` keep the tag consumed. `Accepted` also succeeds when
    /// reconciliation already accepted the submission under the same order
    /// ID (or this call has none).
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
                        _ => Err(Error::Duplicate("Submission is already resolved".into())),
                    };
                }
                if matches!(outcome, SubmissionOutcome::Rejected) {
                    tx.execute(RELEASE_ORDER_TAG, [&id]).map_err(sql)?;
                }
                audit(&tx, "outcome", &id, &serde_json::to_value(&outcome)?)?;
                tx.commit().map_err(sql)?;
                Ok(())
            })
            .await
    }
    pub async fn records(&self) -> Result<Vec<SubmissionRecord>> {
        self.database
            .run(|conn| {
                let mut query = conn.prepare(SELECT_SUBMISSIONS).map_err(sql)?;
                let rows = query
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, Option<String>>(5)?,
                            r.get::<_, Option<String>>(6)?,
                        ))
                    })
                    .map_err(sql)?;
                rows.map(|row| {
                    let (attempt_id, account, operation, created_at, request, outcome, dedup_key) =
                        row.map_err(sql)?;
                    Ok(SubmissionRecord {
                        attempt_id,
                        account,
                        operation,
                        created_at,
                        request: serde_json::from_str(&request)?,
                        outcome: outcome.map(|s| serde_json::from_str(&s)).transpose()?,
                        dedup_key,
                    })
                })
                .collect()
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
                conn.execute(
                    PRUNE_RESOLVED_SUBMISSIONS,
                    rusqlite::params![before, exported_through],
                )
                .map_err(sql)
            })
            .await
    }
}
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
                tx.execute(
                    INSERT_APPROVAL,
                    rusqlite::params![digest, principal, expires_at, operator],
                )
                .map_err(sql)?;
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
    async fn records(&self) -> Result<Vec<SubmissionRecord>>;
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
    async fn records(&self) -> Result<Vec<SubmissionRecord>> {
        OrderJournal::records(self).await
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
