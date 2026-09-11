//! Acknowledged, replay-safe delivery to an organization's immutable audit sink.
use crate::{
    error::{Error, Result},
    execution::{AuditEvent, OrderJournal},
    remote::RemoteBackend,
    storage::sql,
};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

const INSERT_AUDIT_STREAM_ID: &str = "INSERT OR IGNORE INTO meta VALUES('audit_stream_id',?1)";
const SELECT_AUDIT_STREAM_ID: &str = "SELECT value FROM meta WHERE key='audit_stream_id'";
const INSERT_META_IF_NEW: &str = "INSERT OR IGNORE INTO meta VALUES(?1,?2)";
const SELECT_META: &str = "SELECT value FROM meta WHERE key=?1";
const UPSERT_META: &str =
    "INSERT INTO meta VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditBatch {
    pub stream_id: String,
    pub after_sequence: i64,
    pub previous_digest: String,
    pub events: Vec<AuditEvent>,
    pub batch_sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AuditAcknowledgement {
    pub stream_id: String,
    pub through_sequence: i64,
    pub batch_sha256: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Cursor {
    through: i64,
    digest: String,
}
impl OrderJournal {
    /// Send one bounded batch; advance the cursor only after an exact durable ack.
    /// The sink must deduplicate by stream/sequence/digest and enforce append-only
    /// retention. A restored/forked history must be rejected by the sink.
    pub async fn deliver_audit(
        &self,
        sink_id: &str,
        endpoint: &str,
        token: secrecy::SecretString,
        limit: u32,
    ) -> Result<Option<AuditAcknowledgement>> {
        if sink_id.trim().is_empty() || sink_id.len() > 128 {
            return Err(Error::InvalidParameter(
                "Stable audit sink ID required".into(),
            ));
        }
        let sink = RemoteBackend::new(endpoint, token)?;
        let key = format!("audit_delivery:{sink_id}");
        let get_key = key.clone();
        let endpoint = endpoint.to_owned();
        let (stream, cursor) = self
            .database()
            .run(move |c| {
                let tx = c
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                tx.execute(INSERT_AUDIT_STREAM_ID, [uuid::Uuid::new_v4().to_string()])
                    .map_err(sql)?;
                let stream: String = tx
                    .query_row(SELECT_AUDIT_STREAM_ID, [], |r| r.get(0))
                    .map_err(sql)?;
                let pin = format!("{get_key}:endpoint");
                tx.execute(INSERT_META_IF_NEW, rusqlite::params![pin, endpoint])
                    .map_err(sql)?;
                let pinned: String = tx
                    .query_row(SELECT_META, [pin], |r| r.get(0))
                    .map_err(sql)?;
                if pinned != endpoint {
                    return Err(Error::Config(
                        "Audit sink ID is pinned to another endpoint".into(),
                    ));
                }
                let cursor: Option<String> = tx
                    .query_row(SELECT_META, [get_key], |r| r.get(0))
                    .optional()
                    .map_err(sql)?;
                let cursor: Cursor = cursor
                    .map(|v| serde_json::from_str(&v))
                    .transpose()?
                    .unwrap_or_default();
                tx.commit().map_err(sql)?;
                Ok((stream, cursor))
            })
            .await?;
        let events = self.audit_page(cursor.through, limit.min(1000)).await?;
        if events.is_empty() {
            return Ok(None);
        }
        let last = events.last().unwrap().sequence;
        let mut batch = AuditBatch {
            stream_id: stream.clone(),
            after_sequence: cursor.through,
            previous_digest: cursor.digest.clone(),
            events,
            batch_sha256: String::new(),
        };
        batch.batch_sha256 = audit_digest(&batch)?;
        let expected = AuditAcknowledgement {
            stream_id: stream,
            through_sequence: last,
            batch_sha256: batch.batch_sha256.clone(),
        };
        let ack: AuditAcknowledgement = sink.post(&batch).await?;
        if ack != expected {
            return Err(Error::Storage(
                "Audit sink acknowledgement does not match batch".into(),
            ));
        }
        let new = Cursor {
            through: last,
            digest: batch.batch_sha256,
        };
        self.database()
            .run(move |c| {
                let tx = c
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                let current: Option<String> = tx
                    .query_row(SELECT_META, [&key], |r| r.get(0))
                    .optional()
                    .map_err(sql)?;
                let current: Cursor = current
                    .map(|v| serde_json::from_str(&v))
                    .transpose()?
                    .unwrap_or_default();
                if current.through != cursor.through || current.digest != cursor.digest {
                    return Err(Error::Storage(
                        "Audit cursor advanced concurrently; reload before retrying".into(),
                    ));
                }
                tx.execute(
                    UPSERT_META,
                    rusqlite::params![key, serde_json::to_string(&new)?],
                )
                .map_err(sql)?;
                tx.commit().map_err(sql)
            })
            .await?;
        Ok(Some(ack))
    }
}
/// Canonical digest for this protocol: serialize with an empty digest field.
pub fn audit_digest(batch: &AuditBatch) -> Result<String> {
    let mut body = batch.clone();
    body.batch_sha256.clear();
    let hash = ring::digest::digest(
        &ring::digest::SHA256,
        &serde_json::to_vec(&crate::utils::canonical_json(&serde_json::to_value(&body)?))?,
    );
    Ok(hash.as_ref().iter().map(|b| format!("{b:02x}")).collect())
}
