//! Acknowledged, replay-safe delivery to an organization's immutable audit sink.
use crate::{
    error::{Error, Result},
    execution::{AuditEvent, OrderJournal},
    remote::RemoteBackend,
    storage::{meta, sql},
};
use serde::{Deserialize, Serialize};

const AUDIT_STREAM_ID_KEY: &str = "audit_stream_id";

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
                let missing = || Error::Storage("Audit delivery metadata is missing".into());
                meta::insert_if_new(&tx, AUDIT_STREAM_ID_KEY, &uuid::Uuid::new_v4().to_string())?;
                let stream = meta::get(&tx, AUDIT_STREAM_ID_KEY)?.ok_or_else(missing)?;
                let pin = format!("{get_key}:endpoint");
                meta::insert_if_new(&tx, &pin, &endpoint)?;
                if meta::get(&tx, &pin)?.ok_or_else(missing)? != endpoint {
                    return Err(Error::Config(
                        "Audit sink ID is pinned to another endpoint".into(),
                    ));
                }
                let cursor: Cursor = meta::get_json(&tx, &get_key)?.unwrap_or_default();
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
                let current: Cursor = meta::get_json(&tx, &key)?.unwrap_or_default();
                if current.through != cursor.through || current.digest != cursor.digest {
                    return Err(Error::Storage(
                        "Audit cursor advanced concurrently; reload before retrying".into(),
                    ));
                }
                meta::put_json(&tx, &key, &new)?;
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
