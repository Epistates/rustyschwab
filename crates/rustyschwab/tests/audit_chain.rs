use rustyschwab::{
    Error,
    audit_chain::GENESIS_HASH,
    execution::{AuditEvent, ExecutionStore, OrderJournal, SubmissionOutcome},
    remote::BackendRequest,
};
use serde_json::json;
use std::path::PathBuf;

async fn journal_with_events(dir: &tempfile::TempDir, count: i64) -> OrderJournal {
    let journal = OrderJournal::new(dir.path().join("chain.json"));
    for n in 1..=count {
        journal
            .record_event("note", "test", json!({"n": n}))
            .await
            .unwrap();
    }
    journal
}

fn raw(journal: &OrderJournal) -> rusqlite::Connection {
    rusqlite::Connection::open(journal.database().path()).unwrap()
}

fn broken_at(error: Error) -> String {
    match error {
        Error::Storage(message) => message,
        other => panic!("expected a storage error, got {other:?}"),
    }
}

#[tokio::test]
async fn chain_links_every_event_to_the_one_before() {
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_with_events(&dir, 60).await;
    let report = journal.verify_audit().await.unwrap();
    let events = journal.audit_page(0, 1000).await.unwrap();
    assert_eq!(report.rows, events.len() as u64);
    assert_eq!(report.first_sequence, Some(events[0].sequence));
    assert_eq!(report.last_sequence, Some(events.last().unwrap().sequence));
    assert_eq!(report.anchor_sequence, None);
    assert_eq!(report.head_hash, events.last().unwrap().hash);
    assert_eq!(events[0].prev_hash, GENESIS_HASH);
    for pair in events.windows(2) {
        assert_eq!(pair[1].prev_hash, pair[0].hash);
        assert_eq!(pair[1].sequence, pair[0].sequence + 1);
        assert_eq!(pair[1].hash.len(), 64);
    }
    // The ExecutionStore trait reaches the same check.
    let store: &dyn ExecutionStore = &journal;
    assert_eq!(store.verify_audit().await.unwrap(), report);
}

#[tokio::test]
async fn empty_chain_verifies_at_the_genesis() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("empty.json"));
    let report = journal.verify_audit().await.unwrap();
    assert_eq!(report.rows, 0);
    assert_eq!(report.first_sequence, None);
    assert_eq!(report.head_hash, GENESIS_HASH);
}

#[tokio::test]
async fn edited_payload_is_found_at_its_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_with_events(&dir, 10).await;
    raw(&journal)
        .execute(
            "UPDATE audit SET payload='{\"n\":999}' WHERE sequence=3",
            [],
        )
        .unwrap();
    let message = broken_at(journal.verify_audit().await.unwrap_err());
    assert!(message.contains("sequence 3:"), "{message}");
}

#[tokio::test]
async fn edited_kind_subject_and_time_are_each_found() {
    for edit in [
        "UPDATE audit SET kind='other' WHERE sequence=4",
        "UPDATE audit SET subject='other' WHERE sequence=4",
        "UPDATE audit SET at=at+1 WHERE sequence=4",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal_with_events(&dir, 8).await;
        raw(&journal).execute(edit, []).unwrap();
        let message = broken_at(journal.verify_audit().await.unwrap_err());
        assert!(message.contains("sequence 4:"), "{edit}: {message}");
    }
}

#[tokio::test]
async fn deleted_middle_row_is_found_at_the_row_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_with_events(&dir, 10).await;
    raw(&journal)
        .execute("DELETE FROM audit WHERE sequence=5", [])
        .unwrap();
    let message = broken_at(journal.verify_audit().await.unwrap_err());
    assert!(message.contains("sequence 6:"), "{message}");
}

#[tokio::test]
async fn deleted_oldest_rows_without_an_anchor_are_found() {
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_with_events(&dir, 10).await;
    raw(&journal)
        .execute("DELETE FROM audit WHERE sequence<=3", [])
        .unwrap();
    let message = broken_at(journal.verify_audit().await.unwrap_err());
    assert!(message.contains("sequence 4:"), "{message}");
}

#[tokio::test]
async fn row_deleted_from_the_tail_leaves_a_gap_the_next_append_exposes() {
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_with_events(&dir, 6).await;
    raw(&journal)
        .execute("DELETE FROM audit WHERE sequence=6", [])
        .unwrap();
    // Nothing follows the deleted row yet, so only a recorded head hash
    // would reveal it.
    journal.verify_audit().await.unwrap();
    journal
        .record_event("note", "test", json!({"n": 7}))
        .await
        .unwrap();
    let message = broken_at(journal.verify_audit().await.unwrap_err());
    assert!(message.contains("sequence 7:"), "{message}");
}

#[tokio::test]
async fn reordered_rows_are_found_at_the_first_moved_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_with_events(&dir, 10).await;
    raw(&journal)
        .execute_batch(
            "UPDATE audit SET sequence=-1 WHERE sequence=3;
             UPDATE audit SET sequence=3 WHERE sequence=4;
             UPDATE audit SET sequence=4 WHERE sequence=-1;",
        )
        .unwrap();
    let message = broken_at(journal.verify_audit().await.unwrap_err());
    assert!(message.contains("sequence 3:"), "{message}");
}

#[tokio::test]
async fn pruning_keeps_the_chain_valid_and_is_itself_chained() {
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_with_events(&dir, 5).await;
    let id = journal.begin("A", "place_order", json!({})).await.unwrap();
    journal
        .finish(&id, SubmissionOutcome::Rejected)
        .await
        .unwrap();
    let before = journal.verify_audit().await.unwrap();
    let through = before.last_sequence.unwrap();
    assert_eq!(journal.prune_resolved(i64::MAX, through).await.unwrap(), 1);
    journal.prune_history(i64::MAX).await.unwrap();
    let after = journal.verify_audit().await.unwrap();
    assert_eq!(after.rows, before.rows + 2);
    let kinds: Vec<_> = journal
        .audit_page(through, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(kinds, ["submissions_pruned", "history_pruned"]);
}

#[tokio::test]
async fn verification_starts_from_the_anchor_after_old_rows_are_removed() {
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_with_events(&dir, 10).await;
    let anchor_hash = journal.audit_page(4, 1).await.unwrap()[0].prev_hash.clone();
    let connection = raw(&journal);
    connection
        .execute_batch(&format!(
            "DELETE FROM audit WHERE sequence<=4;
             INSERT INTO audit_anchor VALUES(1,4,'{anchor_hash}');"
        ))
        .unwrap();
    let report = journal.verify_audit().await.unwrap();
    assert_eq!(report.rows, 6);
    assert_eq!(report.first_sequence, Some(5));
    assert_eq!(report.anchor_sequence, Some(4));
    // Appending continues the chain, and so does a table emptied entirely.
    journal
        .record_event("note", "test", json!({}))
        .await
        .unwrap();
    assert_eq!(journal.verify_audit().await.unwrap().rows, 7);
    let head = journal.verify_audit().await.unwrap();
    connection
        .execute_batch(&format!(
            "DELETE FROM audit;
             UPDATE audit_anchor SET sequence={},hash='{}';",
            head.last_sequence.unwrap(),
            head.head_hash
        ))
        .unwrap();
    let empty = journal.verify_audit().await.unwrap();
    assert_eq!(empty.rows, 0);
    assert_eq!(empty.head_hash, head.head_hash);
    journal
        .record_event("note", "test", json!({}))
        .await
        .unwrap();
    let resumed = journal.verify_audit().await.unwrap();
    assert_eq!(resumed.rows, 1);
    assert_eq!(
        resumed.first_sequence,
        Some(head.last_sequence.unwrap() + 1)
    );
    // A wrong anchor hash is a break at the first row it should have linked.
    connection
        .execute("UPDATE audit_anchor SET hash=?1", [GENESIS_HASH])
        .unwrap();
    let message = broken_at(journal.verify_audit().await.unwrap_err());
    assert!(
        message.contains(&format!("sequence {}:", resumed.first_sequence.unwrap())),
        "{message}"
    );
}

#[tokio::test]
async fn upgrading_a_pre_chain_database_backfills_a_valid_chain() {
    let dir = tempfile::tempdir().unwrap();
    let journal = journal_with_events(&dir, 25).await;
    let path = journal.database().path().to_owned();
    {
        // Turn the file back into what version 4 left: no hash columns.
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "DROP TABLE audit_anchor;
                 ALTER TABLE audit DROP COLUMN prev_hash;
                 ALTER TABLE audit DROP COLUMN hash;
                 DROP TABLE activity;
                 DELETE FROM schema_migrations WHERE version>=5;
                 UPDATE meta SET value='4' WHERE key='schema';",
            )
            .unwrap();
    }
    let upgraded = OrderJournal::new(path.clone());
    let report = upgraded.verify_audit().await.unwrap();
    assert_eq!(report.rows, 25);
    assert_eq!(report.first_sequence, Some(1));
    let events = upgraded.audit_page(0, 100).await.unwrap();
    assert_eq!(events[0].prev_hash, GENESIS_HASH);
    assert_eq!(events.last().unwrap().hash, report.head_hash);
    upgraded
        .record_event("note", "test", json!({}))
        .await
        .unwrap();
    assert_eq!(upgraded.verify_audit().await.unwrap().rows, 26);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_on_one_file_keep_a_single_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("shared.json");
    // Separate handles share nothing but the file, as two processes would.
    let handles = [OrderJournal::new(path.clone()), OrderJournal::new(path)];
    let tasks: Vec<_> = (0..8)
        .map(|task| {
            let journal = handles[task % 2].clone();
            tokio::spawn(async move {
                for n in 0..20 {
                    journal
                        .record_event("note", &format!("task{task}"), json!({"n": n}))
                        .await
                        .unwrap();
                }
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    let report = handles[0].verify_audit().await.unwrap();
    assert_eq!(report.rows, 160);
    assert_eq!(report.last_sequence, Some(160));
    assert_eq!(handles[1].verify_audit().await.unwrap(), report);
}

#[test]
fn events_and_requests_from_older_peers_still_decode() {
    let event: AuditEvent = serde_json::from_value(json!({
        "sequence": 1, "at": 2, "kind": "k", "subject": "s", "payload": {}
    }))
    .unwrap();
    assert!(event.hash.is_empty() && event.prev_hash.is_empty());
    assert!(matches!(
        serde_json::from_value(json!({"operation": "verify_audit"})),
        Ok(BackendRequest::VerifyAudit)
    ));
}

/// The activity log pages by sequence, keeps only its newest entries, and
/// stays out of the audit chain.
#[tokio::test]
async fn activity_pages_in_order_and_keeps_only_the_newest() {
    use rustyschwab::execution::ACTIVITY_RETAINED;
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("activity.json"));
    let first = journal
        .record_activity("desk", "tool_call", json!({"tool": "get_quote"}))
        .await
        .unwrap();
    let second = journal
        .record_activity("desk", "tool_call", json!({"tool": "place_order"}))
        .await
        .unwrap();
    let page = journal.activity_page(0, 10).await.unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].sequence, first);
    assert_eq!(page[1].payload["tool"], "place_order");
    assert_eq!(
        journal.activity_page(first, 10).await.unwrap()[0].sequence,
        second
    );
    assert!(journal.audit_page(0, 10).await.unwrap().is_empty());

    // Past the cap, the oldest entries go.
    raw(&journal)
        .execute(
            "UPDATE sqlite_sequence SET seq=?1 WHERE name='activity'",
            [ACTIVITY_RETAINED + 5],
        )
        .unwrap();
    let latest = journal
        .record_activity("desk", "tool_call", json!({}))
        .await
        .unwrap();
    assert_eq!(latest, ACTIVITY_RETAINED + 6);
    let kept = journal.activity_page(0, 10).await.unwrap();
    assert_eq!(
        kept.iter().map(|a| a.sequence).collect::<Vec<_>>(),
        vec![latest]
    );
}

/// A setting is set and cleared with an audited `setting_changed` event,
/// in a namespace that cannot touch the journal's own entries.
#[tokio::test]
async fn settings_are_audited_and_namespaced() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("settings.json"));
    assert_eq!(journal.setting("delegation").await.unwrap(), None);
    journal
        .set_setting("delegation", Some(json!({"mode": "autonomous"})), "alice")
        .await
        .unwrap();
    assert_eq!(
        journal.setting("delegation").await.unwrap(),
        Some(json!({"mode": "autonomous"}))
    );
    journal
        .set_setting("operator_halt", Some(json!("not a halt")), "alice")
        .await
        .unwrap();
    assert_eq!(journal.operator_halt().await.unwrap(), None);
    journal
        .set_setting("delegation", None, "bob")
        .await
        .unwrap();
    assert_eq!(journal.setting("delegation").await.unwrap(), None);
    let changes: Vec<_> = journal
        .audit_page(0, 10)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "setting_changed")
        .map(|e| (e.subject, e.payload))
        .collect();
    assert_eq!(changes.len(), 3);
    assert_eq!(
        changes[2],
        (
            "bob".to_string(),
            json!({"key": "delegation", "value": null})
        )
    );
    journal.verify_audit().await.unwrap();
}
