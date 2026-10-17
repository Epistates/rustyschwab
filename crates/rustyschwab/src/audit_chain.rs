//! Tamper evidence for the `audit` table.
//!
//! Every audit row carries `prev_hash` (the previous row's `hash`) and its own
//! `hash`, a SHA-256 over the row's content and its predecessor. Editing,
//! deleting or reordering a row leaves a later link that no longer matches,
//! and [`OrderJournal::verify_audit`](crate::execution::OrderJournal::verify_audit)
//! names the first sequence where that happens.
//!
//! The chain proves the history is internally consistent, not that it is
//! complete: someone with write access can drop the newest rows, or rewrite
//! the whole chain consistently. Record [`AuditChainReport::head_hash`] (or
//! have the external sink hold the events' hashes) and compare later to close
//! that gap.
//!
//! A hash covers, in order, each field prefixed by its length as a big-endian
//! `u64` so no two field layouts share an encoding: the domain tag
//! `rustyschwab.audit.v1`, `prev_hash` (hex text), `sequence` and `at` (big-endian `i64`),
//! `kind`, `subject` and `payload` (the JSON text exactly as stored). The
//! first row's `prev_hash` is [`GENESIS_HASH`]. If rows before some sequence
//! were deleted on purpose, `audit_anchor` holds the last deleted row's
//! sequence and hash and verification starts from it.
use crate::{
    error::{Error, Result},
    storage::sql,
};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// Separates this hash from any other use of SHA-256 over the same bytes;
/// bump the suffix if the encoding ever changes.
const DOMAIN: &[u8] = b"rustyschwab.audit.v1";
/// The `prev_hash` of the first row: 64 hex zeros.
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const BACKFILL_PAGE: i64 = 1000;

/// What a successful chain verification found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AuditChainReport {
    /// Rows whose link and hash were checked (after the anchor, if any).
    pub rows: u64,
    /// Sequence of the first row checked; `None` when there are none.
    pub first_sequence: Option<i64>,
    /// Sequence of the newest row.
    pub last_sequence: Option<i64>,
    /// Hash of the newest row (the anchor's, or [`GENESIS_HASH`], when no row
    /// follows). Record it elsewhere as a checkpoint.
    pub head_hash: String,
    /// Sequence of the retention anchor verification started from, if one is
    /// recorded.
    pub anchor_sequence: Option<i64>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The hash of one row; see the module docs for the encoding.
fn row_hash(
    prev_hash: &str,
    sequence: i64,
    at: i64,
    kind: &str,
    subject: &str,
    payload: &str,
) -> String {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    for field in [
        DOMAIN,
        prev_hash.as_bytes(),
        &sequence.to_be_bytes(),
        &at.to_be_bytes(),
        kind.as_bytes(),
        subject.as_bytes(),
        payload.as_bytes(),
    ] {
        context.update(&(field.len() as u64).to_be_bytes());
        context.update(field);
    }
    hex(context.finish().as_ref())
}

/// The hash the next row builds on: the newest row's, else the anchor's,
/// else the genesis.
fn head_hash(conn: &Connection) -> Result<String> {
    let newest = conn
        .query_row(
            "SELECT hash FROM audit ORDER BY sequence DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql)?;
    match newest {
        Some(hash) => Ok(hash),
        None => Ok(anchor(conn)?.map_or_else(|| GENESIS_HASH.to_owned(), |(_, hash)| hash)),
    }
}

fn anchor(conn: &Connection) -> Result<Option<(i64, String)>> {
    conn.query_row("SELECT sequence,hash FROM audit_anchor", [], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })
    .optional()
    .map_err(sql)
}

/// Appends one row linked to the chain. The caller holds the write lock (see
/// [`crate::storage::audit`]), so the head read here is still the head when
/// the insert lands.
pub(crate) fn append(
    conn: &Connection,
    at: i64,
    kind: &str,
    subject: &str,
    payload: &str,
) -> Result<()> {
    let prev_hash = head_hash(conn)?;
    // The next sequence is derived from what SQLite last handed out, so a
    // row deleted from the tail leaves a visible gap instead of being
    // reissued under a valid-looking link.
    let sequence: i64 = conn
        .query_row(
            "SELECT MAX(COALESCE((SELECT seq FROM sqlite_sequence WHERE name='audit'),0), \
                        COALESCE((SELECT MAX(sequence) FROM audit),0), \
                        COALESCE((SELECT sequence FROM audit_anchor),0)) + 1",
            [],
            |r| r.get(0),
        )
        .map_err(sql)?;
    let hash = row_hash(&prev_hash, sequence, at, kind, subject, payload);
    conn.execute(
        "INSERT INTO audit(sequence,at,kind,subject,payload,prev_hash,hash) \
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![sequence, at, kind, subject, payload, prev_hash, hash],
    )
    .map_err(sql)?;
    Ok(())
}

/// Chains the rows that predate the hash columns, in sequence order. Runs
/// inside the migration's transaction, in pages so a long history is not
/// held in memory.
pub(crate) fn backfill(conn: &Connection) -> Result<()> {
    let mut prev_hash = GENESIS_HASH.to_owned();
    let mut after = 0_i64;
    loop {
        let page = {
            let mut statement = conn
                .prepare(
                    "SELECT sequence,at,kind,subject,payload FROM audit \
                     WHERE sequence>?1 ORDER BY sequence LIMIT ?2",
                )
                .map_err(sql)?;
            statement
                .query_map([after, BACKFILL_PAGE], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                })
                .map_err(sql)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(sql)?
        };
        let Some(&(last, ..)) = page.last() else {
            return Ok(());
        };
        for (sequence, at, kind, subject, payload) in page {
            let hash = row_hash(&prev_hash, sequence, at, &kind, &subject, &payload);
            conn.execute(
                "UPDATE audit SET prev_hash=?1, hash=?2 WHERE sequence=?3",
                rusqlite::params![prev_hash, hash, sequence],
            )
            .map_err(sql)?;
            prev_hash = hash;
        }
        after = last;
    }
}

fn broken(sequence: i64, reason: &str) -> Error {
    Error::Storage(format!(
        "Audit chain broken at sequence {sequence}: {reason}"
    ))
}

/// Walks the chain from the anchor (or the genesis) to the newest row and
/// returns the first break as an [`Error::Storage`] naming its sequence. The
/// walk reads one snapshot, so concurrent appends do not disturb it.
pub(crate) fn verify(conn: &mut Connection) -> Result<AuditChainReport> {
    let tx = conn.transaction().map_err(sql)?;
    let anchor = anchor(&tx)?;
    let mut expected_prev = anchor
        .as_ref()
        .map_or_else(|| GENESIS_HASH.to_owned(), |(_, h)| h.clone());
    let mut previous_sequence = anchor.as_ref().map(|(s, _)| *s);
    let mut report = AuditChainReport {
        rows: 0,
        first_sequence: None,
        last_sequence: None,
        head_hash: expected_prev.clone(),
        anchor_sequence: previous_sequence,
    };
    let mut statement = tx
        .prepare(
            "SELECT sequence,at,kind,subject,payload,prev_hash,hash FROM audit \
             WHERE sequence>?1 ORDER BY sequence",
        )
        .map_err(sql)?;
    let mut rows = statement
        .query([previous_sequence.unwrap_or(0)])
        .map_err(sql)?;
    while let Some(row) = rows.next().map_err(sql)? {
        let sequence: i64 = row.get(0).map_err(sql)?;
        let prev_hash: String = row.get(5).map_err(sql)?;
        let hash: String = row.get(6).map_err(sql)?;
        if prev_hash != expected_prev {
            return Err(broken(sequence, "link to the previous row does not match"));
        }
        if previous_sequence.is_some_and(|p| sequence != p + 1) {
            return Err(broken(sequence, "preceding rows are missing"));
        }
        let computed = row_hash(
            &prev_hash,
            sequence,
            row.get(1).map_err(sql)?,
            &row.get::<_, String>(2).map_err(sql)?,
            &row.get::<_, String>(3).map_err(sql)?,
            &row.get::<_, String>(4).map_err(sql)?,
        );
        if computed != hash {
            return Err(broken(sequence, "row content does not match its hash"));
        }
        report.rows += 1;
        report.first_sequence.get_or_insert(sequence);
        report.last_sequence = Some(sequence);
        previous_sequence = Some(sequence);
        expected_prev = hash;
    }
    report.head_hash = expected_prev;
    Ok(report)
}
