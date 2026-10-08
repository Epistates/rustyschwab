use schwab_rs::{
    coordination::Coordinator,
    execution::{ExecutionStore, OrderJournal, SubmissionOutcome},
    risk::{PortfolioLimits, PortfolioRisk, RiskProvider},
    storage::StateDatabase,
};
use serde_json::json;
use std::{collections::HashMap, time::Duration};

#[tokio::test]
async fn legacy_migration_backup_restore_and_retention_preserve_safety() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.json");
    let record = schwab_rs::execution::SubmissionRecord {
        attempt_id: "legacy".into(),
        account: "A".into(),
        operation: "place_order".into(),
        request: json!({}),
        created_at: 1,
        outcome: None,
        dedup_key: Some("once".into()),
        armed: true,
    };
    schwab_rs::security::secure_file_write(
        &path,
        &serde_json::to_vec(&json!({"legacy":record})).unwrap(),
    )
    .unwrap();
    let journal = OrderJournal::new(path);
    assert_eq!(journal.records().await.unwrap().len(), 1);
    assert!(journal.begin("A", "place_order", json!({})).await.is_err());
    journal
        .finish("legacy", SubmissionOutcome::Rejected)
        .await
        .unwrap();
    let backup = dir.path().join("backup/state.sqlite3");
    journal.database().backup(backup.clone()).await.unwrap();
    let restored = StateDatabase::restore(backup, dir.path().join("restored.json"))
        .await
        .unwrap();
    let restored = OrderJournal::new(restored.path().to_owned());
    assert_eq!(restored.records().await.unwrap().len(), 1);
    let last = restored
        .audit_page(0, 100)
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence;
    assert_eq!(restored.prune_resolved(i64::MAX, last).await.unwrap(), 1);
    assert!(
        restored
            .begin_once("B", "place_order", json!({}), Some("once".into()))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn reconciliation_matches_tag_deduplicates_activity_and_rejects_old_generation() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let request = json!({"tag":"unique","orderType":"LIMIT","price":"10.00"});
    let id = journal.begin("A", "place_order", request).await.unwrap();
    let db = journal.database();
    db.invalidate_account("A", 1).await.unwrap();
    let absent = db
        .reconcile_snapshot("A", 1, 100, json!({}), vec![])
        .await
        .unwrap();
    assert!(!absent.reconciled);
    let order = json!({"tag":"unique","orderId":123,"orderType":"LIMIT","price":10.0,"status":"WORKING","orderActivityCollection":[{"activityType":"EXECUTION","quantity":1}]});
    let first = db
        .reconcile_snapshot("A", 1, 101, json!({}), vec![order.clone()])
        .await
        .unwrap();
    assert!(first.reconciled);
    assert_eq!(first.resolved_attempts, vec![id.clone()]);
    assert_eq!(first.newly_observed_activities, 1);
    // The submitter's own answer arriving after reconciliation's agrees;
    // a different order ID does not.
    let accepted = |order_id: Option<&str>| SubmissionOutcome::Accepted {
        order_id: order_id.map(str::to_owned),
    };
    journal.finish(&id, accepted(Some("123"))).await.unwrap();
    journal.finish(&id, accepted(None)).await.unwrap();
    assert!(journal.finish(&id, accepted(Some("124"))).await.is_err());
    assert!(
        journal
            .finish(&id, SubmissionOutcome::Rejected)
            .await
            .is_err()
    );
    assert_eq!(
        db.reconcile_snapshot("A", 1, 102, json!({}), vec![order])
            .await
            .unwrap()
            .newly_observed_activities,
        0
    );
    db.invalidate_account("A", 2).await.unwrap();
    // An older generation's pass writes nothing, and says so.
    assert!(
        db.reconcile_snapshot("A", 1, 103, json!({}), vec![])
            .await
            .unwrap()
            .superseded
    );
    assert!(!db.account_state("A").await.unwrap().unwrap().reconciled);
}

#[tokio::test]
async fn shared_quota_and_fencing_survive_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let a = StateDatabase::new(dir.path().join("state.json"));
    let b = StateDatabase::new(a.path().to_owned());
    assert!(a.quota("credential", 1, 1).await.unwrap().is_zero());
    assert!(!b.quota("credential", 1, 1).await.unwrap().is_zero());
    let lease = a
        .acquire("stream", "A", Duration::from_secs(10))
        .await
        .unwrap();
    assert!(
        b.acquire("stream", "B", Duration::from_secs(10))
            .await
            .is_err()
    );
    a.release(&lease).await.unwrap();
    let next = b
        .acquire("stream", "B", Duration::from_secs(10))
        .await
        .unwrap();
    assert!(next.epoch > lease.epoch);
    assert!(!a.valid(&lease).await.unwrap());
    assert!(a.renew(&lease, Duration::from_secs(10)).await.is_err());
}

#[tokio::test]
async fn approvals_expire_revoke_and_cannot_be_self_issued() {
    let dir = tempfile::tempdir().unwrap();
    let j = OrderJournal::new(dir.path().join("audit.json"));
    let digest = "a".repeat(64);
    let expires = chrono::Utc::now().timestamp() + 60;
    assert!(j.approve("desk", &digest, "desk", expires).await.is_err());
    j.approve("desk", &digest, "operator", expires)
        .await
        .unwrap();
    assert!(j.is_approved("desk", &digest).await.unwrap());
    assert!(!j.is_approved("other", &digest).await.unwrap());
    j.revoke_approval(&digest, "operator").await.unwrap();
    assert!(!j.is_approved("desk", &digest).await.unwrap());
    assert!(
        j.begin_once("desk", "place_order", json!({}), Some(digest))
            .await
            .is_err()
    );
}

/// An approval spent on a submission the broker never saw can be given back
/// once the submission is resolved; a revoked one cannot.
#[tokio::test]
async fn approvals_refund_only_for_resolved_unsent_submissions() {
    let dir = tempfile::tempdir().unwrap();
    let j = OrderJournal::new(dir.path().join("audit.json"));
    let digest = "b".repeat(64);
    let expires = chrono::Utc::now().timestamp() + 60;
    j.approve("desk", &digest, "operator", expires)
        .await
        .unwrap();
    let request = json!({"tag": "intent-tag"});
    let first = j
        .begin_once("desk", "place_order", request.clone(), Some(digest.clone()))
        .await
        .unwrap();
    assert!(!j.is_approved("desk", &digest).await.unwrap());
    // Pending: it may still reach the broker.
    assert!(j.refund_approval(&first).await.is_err());
    j.finish(&first, SubmissionOutcome::Rejected).await.unwrap();
    j.refund_approval(&first).await.unwrap();
    assert!(j.is_approved("desk", &digest).await.unwrap());
    // The same intent, tag included, goes again, and spends it again.
    let second = j
        .begin_once("desk", "place_order", request.clone(), Some(digest.clone()))
        .await
        .unwrap();
    assert!(!j.is_approved("desk", &digest).await.unwrap());
    j.finish(&second, SubmissionOutcome::Rejected)
        .await
        .unwrap();
    j.revoke_approval(&digest, "operator").await.unwrap();
    j.refund_approval(&second).await.unwrap();
    assert!(!j.is_approved("desk", &digest).await.unwrap());
    assert!(
        j.begin_once("desk", "place_order", request, Some(digest))
            .await
            .is_err()
    );
    // A submission without an approval has nothing to give back.
    let plain = j
        .begin_once("other", "place_order", json!({}), None)
        .await
        .unwrap();
    j.finish(&plain, SubmissionOutcome::Rejected).await.unwrap();
    assert!(j.refund_approval(&plain).await.is_err());
}

#[tokio::test]
async fn risk_reserves_pending_exposure_and_releases_only_after_terminal_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let db = journal.database().clone();
    let now = chrono::Utc::now().timestamp_millis();
    db.invalidate_account("A", 1).await.unwrap();
    db.reconcile_snapshot(
        "A",
        1,
        now,
        json!({"securitiesAccount":{"currentBalances":{"availableFunds":"100"},"positions":[]}}),
        vec![],
    )
    .await
    .unwrap();
    let limits = PortfolioLimits {
        max_open_quantity: "2".parse().unwrap(),
        max_open_notional: "100".parse().unwrap(),
        max_position_quantity: "2".parse().unwrap(),
        max_snapshot_age_ms: 5000,
        max_daily_loss: None,
    };
    let risk = PortfolioRisk {
        database: db,
        accounts: HashMap::from([("A".into(), limits)]),
    };
    let order=serde_json::from_value(json!({"orderStrategyType":"SINGLE","session":"NORMAL","duration":"DAY","orderType":"LIMIT","price":"60","orderLegCollection":[{"quantity":"1","instruction":"BUY","instrument":{"symbol":"AAPL","assetType":"EQUITY"}}]})).unwrap();
    risk.reserve("A", "first", &order).await.unwrap();
    assert!(risk.reserve("A", "second", &order).await.is_err());
    risk.release("first").await.unwrap();
    risk.reserve("A", "second", &order).await.unwrap();
}

#[tokio::test]
async fn daily_loss_past_the_cap_halts_at_reservation_and_on_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let db = journal.database().clone();
    let baseline = |start: &str, current: &str| {
        json!({"securitiesAccount":{
            "initialBalances":{"liquidationValue":start},
            "currentBalances":{"availableFunds":"100","liquidationValue":current},
            "positions":[]}})
    };
    let now = chrono::Utc::now().timestamp_millis();
    db.invalidate_account("A", 1).await.unwrap();
    db.reconcile_snapshot("A", 1, now, baseline("10000", "9990"), vec![])
        .await
        .unwrap();
    let risk = PortfolioRisk {
        database: db.clone(),
        accounts: HashMap::from([(
            "A".into(),
            PortfolioLimits {
                max_open_quantity: 2.into(),
                max_open_notional: 100.into(),
                max_position_quantity: 2.into(),
                max_snapshot_age_ms: 5000,
                max_daily_loss: Some(50.into()),
            },
        )]),
    };
    let order=serde_json::from_value(json!({"orderStrategyType":"SINGLE","session":"NORMAL","duration":"DAY","orderType":"LIMIT","price":"60","orderLegCollection":[{"quantity":"1","instruction":"BUY","instrument":{"symbol":"AAPL","assetType":"EQUITY"}}]})).unwrap();
    // Ten down against a cap of fifty: reserved, and nothing to observe.
    risk.reserve("A", "first", &order).await.unwrap();
    risk.observe_baseline("A").await.unwrap();
    assert!(journal.operator_halt().await.unwrap().is_none());
    // Sixty down: the next reservation trips the halt and fails closed, and
    // so does the revalidation of the one already held.
    db.reconcile_snapshot("A", 1, now + 1, baseline("10000", "9940"), vec![])
        .await
        .unwrap();
    let denied = risk.reserve("A", "second", &order).await.unwrap_err();
    let schwab_rs::Error::Halted(reason) = &denied else {
        panic!("{denied:?}");
    };
    assert!(
        reason.starts_with("Daily loss 60 exceeds the 50 limit for account A: liquidation value"),
        "{reason}"
    );
    assert_eq!(
        journal.operator_halt().await.unwrap().as_deref(),
        Some(reason.as_str())
    );
    assert!(matches!(
        risk.revalidate("A", "first", &order).await,
        Err(schwab_rs::Error::Halted(_))
    ));
    let trips: Vec<_> = journal
        .audit_page(0, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|entry| entry.kind == "operator_halt_changed")
        .collect();
    assert_eq!(trips.len(), 1);
    assert_eq!(trips[0].subject, "daily-loss-control");
    // Clearing the halt does not reset the day: reconciliation trips it again.
    journal.set_operator_halt(None, "operator").await.unwrap();
    risk.observe_baseline("A").await.unwrap();
    assert!(journal.operator_halt().await.unwrap().is_some());
    // A baseline without liquidation values cannot be measured: observation
    // leaves the halt alone and the reservation fails closed as stale.
    journal.set_operator_halt(None, "operator").await.unwrap();
    db.reconcile_snapshot(
        "A",
        1,
        now + 2,
        json!({"securitiesAccount":{"currentBalances":{"availableFunds":"100"},"positions":[]}}),
        vec![],
    )
    .await
    .unwrap();
    risk.observe_baseline("A").await.unwrap();
    assert!(journal.operator_halt().await.unwrap().is_none());
    assert!(matches!(
        risk.reserve("A", "third", &order).await,
        Err(schwab_rs::Error::BaselineStale { .. })
    ));
}

#[tokio::test]
async fn unfinished_transaction_is_rolled_back_after_process_kill() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("crash.json"));
    let id = journal.begin("A", "place_order", json!({})).await.unwrap();
    let ready = dir.path().join("ready");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_transaction_child"])
        .env("SCHWAB_TEST_CRASH_DB", journal.path())
        .env("SCHWAB_TEST_CRASH_ID", &id)
        .env("SCHWAB_TEST_CRASH_READY", &ready)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    let records = journal.records().await.unwrap();
    assert_eq!(records.len(), 1);
    assert!(records[0].outcome.is_none());
    assert!(journal.begin("A", "place_order", json!({})).await.is_err());
}

#[tokio::test]
async fn crash_transaction_child() {
    if let Some(path) = std::env::var_os("SCHWAB_TEST_CRASH_DB") {
        let id = std::env::var("SCHWAB_TEST_CRASH_ID").unwrap();
        let ready = std::env::var_os("SCHWAB_TEST_CRASH_READY").unwrap();
        StateDatabase::new(path.into())
            .run(move |c| {
                c.execute_batch("BEGIN IMMEDIATE").unwrap();
                c.execute("UPDATE submissions SET outcome='{}' WHERE id=?1", [id])
                    .unwrap();
                std::fs::write(ready, b"ready").unwrap();
                std::thread::sleep(Duration::from_secs(30));
                Ok(())
            })
            .await
            .unwrap();
    } else {
        // Normal test invocation also checks SQLite rollback without process kill.
        let dir = tempfile::tempdir().unwrap();
        let db = StateDatabase::new(dir.path().join("rollback.json"));
        let result: schwab_rs::Result<()> = db
            .run(|c| {
                let tx = c.transaction().unwrap();
                tx.execute("INSERT INTO meta VALUES('uncommitted','1')", [])
                    .unwrap();
                Err(schwab_rs::Error::Storage("synthetic failure".into()))
            })
            .await;
        assert!(result.is_err());
        let count = db
            .run(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM meta WHERE key='uncommitted'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap())
            })
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
}

/// A rejection means the broker holds nothing, so the same tag may carry the
/// retry; once a submission is accepted its tag is consumed for good.
#[tokio::test]
async fn rejected_tags_are_released_and_accepted_tags_outlive_retention() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("tags.json"));
    let rejected = journal
        .begin("A", "place_order", json!({"tag":"caller-tag"}))
        .await
        .unwrap();
    assert!(matches!(
        journal
            .begin("A", "place_order", json!({"tag":"caller-tag"}))
            .await,
        Err(schwab_rs::Error::ReconciliationRequired { .. })
    ));
    journal
        .finish(&rejected, SubmissionOutcome::Rejected)
        .await
        .unwrap();
    let accepted = journal
        .begin("A", "place_order", json!({"tag":"caller-tag"}))
        .await
        .expect("a rejected submission releases its tag");
    journal
        .finish(
            &accepted,
            SubmissionOutcome::Accepted {
                order_id: Some("1".into()),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        journal
            .begin("A", "place_order", json!({"tag":"caller-tag"}))
            .await,
        Err(schwab_rs::Error::Duplicate(_))
    ));
    // Retire both submissions: the accepted one needs its broker order to be
    // terminal before retention may drop it.
    journal
        .database()
        .run(|c| {
            c.execute(
                "INSERT INTO orders VALUES('A','1',0,'{\"status\":\"FILLED\"}')",
                [],
            )
            .map(|_| ())
            .map_err(|e| schwab_rs::Error::Storage(e.to_string()))
        })
        .await
        .unwrap();
    let exported = journal
        .audit_page(0, 100)
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence;
    assert_eq!(journal.prune_resolved(i64::MAX, exported).await.unwrap(), 2);
    assert!(matches!(
        journal
            .begin("A", "place_order", json!({"tag":"caller-tag"}))
            .await,
        Err(schwab_rs::Error::Duplicate(_))
    ));
}

#[tokio::test]
async fn restore_rejects_missing_corrupt_and_foreign_backups_without_creating_state() {
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("restored.json");
    for (name, bytes) in [
        ("missing.sqlite3", None),
        ("corrupt.sqlite3", Some(b"not a database".as_slice())),
    ] {
        let source = dir.path().join(name);
        if let Some(bytes) = bytes {
            schwab_rs::security::secure_file_write(&source, bytes).unwrap();
        }
        assert!(
            StateDatabase::restore(source.clone(), destination.clone())
                .await
                .is_err()
        );
        assert!(!StateDatabase::new(destination.clone()).path().exists());
        if let Some(bytes) = bytes {
            assert_eq!(std::fs::read(source).unwrap(), bytes);
        } else {
            assert!(!source.exists());
        }
    }
    let source = dir.path().join("foreign.sqlite3");
    schwab_rs::security::secure_file_write(&source, &[]).unwrap();
    rusqlite::Connection::open(&source)
        .unwrap()
        .execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT); \
             INSERT INTO meta VALUES('schema','1'); \
             CREATE TABLE unrelated(value TEXT);",
        )
        .unwrap();
    assert!(
        StateDatabase::restore(source, destination.clone())
            .await
            .is_err()
    );
    assert!(!StateDatabase::new(destination).path().exists());
}

#[tokio::test]
async fn sqlite_full_rolls_back_outcome_and_preserves_pending_intent() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("full.json"));
    let id = journal
        .begin("A", "place_order", json!({"tag":"full-test"}))
        .await
        .unwrap();
    let attempt = id.clone();
    journal
        .database()
        .run(move |connection| {
            let pages: i64 = connection
                .query_row("PRAGMA page_count", [], |r| r.get(0))
                .unwrap();
            connection
                .pragma_update(None, "max_page_count", pages)
                .unwrap();
            let tx = connection.transaction().unwrap();
            tx.execute(
                "UPDATE submissions SET outcome='\"Rejected\"' WHERE id=?1",
                [attempt],
            )
            .unwrap();
            let error = tx
                .execute("INSERT INTO meta VALUES('fill',zeroblob(1048576))", [])
                .unwrap_err();
            let disk_full = matches!(
                error,
                rusqlite::Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::DiskFull
            );
            assert!(disk_full, "expected SQLITE_FULL, got {error}");
            // SQLITE_FULL may roll back the transaction itself; Drop is also safe.
            drop(tx);
            Ok(())
        })
        .await
        .unwrap();
    let reopened = OrderJournal::new(journal.path().to_owned());
    assert!(
        reopened
            .records()
            .await
            .unwrap()
            .iter()
            .any(|r| r.attempt_id == id && r.outcome.is_none())
    );
    assert!(reopened.begin("A", "place_order", json!({})).await.is_err());
}

#[tokio::test]
async fn backup_destination_is_exclusive_and_restore_preserves_source() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("source.json"));
    journal
        .begin("A", "place_order", json!({"tag":"backup"}))
        .await
        .unwrap();
    let destination = dir.path().join("backup/state.sqlite3");
    let database = journal.database();
    let (first, second) = tokio::join!(
        database.backup(destination.clone()),
        database.backup(destination.clone())
    );
    assert_ne!(first.is_ok(), second.is_ok());
    let before = std::fs::read(&destination).unwrap();
    let restored = StateDatabase::restore(destination.clone(), dir.path().join("restore.json"))
        .await
        .unwrap();
    assert_eq!(before, std::fs::read(&destination).unwrap());
    assert!(
        StateDatabase::restore(destination, restored.path().to_owned())
            .await
            .is_err()
    );
    assert_eq!(
        OrderJournal::new(restored.path().to_owned())
            .records()
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn external_partial_orders_count_toward_risk_and_dispatch_rechecks_freshness() {
    let dir = tempfile::tempdir().unwrap();
    let db = StateDatabase::new(dir.path().join("risk.json"));
    let baseline =
        json!({"securitiesAccount":{"currentBalances":{"availableFunds":1000},"positions":[]}});
    let external = json!({"orderId":55,"status":"WORKING","orderStrategyType":"SINGLE","orderType":"LIMIT","quantity":2,"filledQuantity":1,"remainingQuantity":1,"price":10,"orderLegCollection":[{"instruction":"BUY","quantity":2,"instrument":{"assetType":"EQUITY","symbol":"AAPL"}}]});
    db.reconcile_snapshot(
        "A",
        1,
        chrono::Utc::now().timestamp_millis(),
        baseline.clone(),
        vec![external.clone()],
    )
    .await
    .unwrap();
    let risk = PortfolioRisk {
        database: db.clone(),
        accounts: HashMap::from([(
            "A".into(),
            PortfolioLimits {
                max_open_quantity: 2.into(),
                max_open_notional: 100.into(),
                max_position_quantity: 10.into(),
                max_snapshot_age_ms: 5000,
                max_daily_loss: None,
            },
        )]),
    };
    let mut order:schwab_rs::types::trading::OrderRequest=serde_json::from_value(json!({"orderStrategyType":"SINGLE","orderType":"LIMIT","price":"10","session":"NORMAL","duration":"DAY","orderLegCollection":[{"quantity":"2","instruction":"BUY","instrument":{"symbol":"AAPL","assetType":"EQUITY"}}]})).unwrap();
    assert!(risk.reserve("A", "too-large", &order).await.is_err());
    order.order_leg_collection[0].quantity = 1.into();
    risk.reserve("A", "held", &order).await.unwrap();
    risk.revalidate("A", "held", &order).await.unwrap();
    db.invalidate_account("A", 2).await.unwrap();
    assert!(risk.revalidate("A", "held", &order).await.is_err());
    let mut terminal = external;
    terminal["status"] = json!("FILLED");
    db.reconcile_snapshot(
        "A",
        2,
        chrono::Utc::now().timestamp_millis(),
        baseline,
        vec![terminal],
    )
    .await
    .unwrap();
    risk.revalidate("A", "held", &order).await.unwrap();
    risk.reserve("A", "another", &order).await.unwrap();
}

#[tokio::test]
async fn fenced_risk_revalidation_rejects_released_ownership_and_operator_halt() {
    use schwab_rs::execution::ExecutionStore;
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("fenced.json"));
    let db = journal.database().clone();
    db.reconcile_snapshot(
        "A",
        0,
        chrono::Utc::now().timestamp_millis(),
        json!({"securitiesAccount":{"positions":[],"currentBalances":{"availableFunds":100}}}),
        vec![],
    )
    .await
    .unwrap();
    let risk = PortfolioRisk {
        database: db.clone(),
        accounts: HashMap::from([(
            "A".into(),
            PortfolioLimits {
                max_open_quantity: 2.into(),
                max_open_notional: 100.into(),
                max_position_quantity: 10.into(),
                max_snapshot_age_ms: 5000,
                max_daily_loss: None,
            },
        )]),
    };
    let order=serde_json::from_value(json!({"session":"NORMAL","duration":"DAY","orderStrategyType":"SINGLE","orderType":"LIMIT","price":"10","orderLegCollection":[{"quantity":"1","instruction":"BUY","instrument":{"assetType":"EQUITY","symbol":"AAPL"}}]})).unwrap();
    risk.reserve("A", "held", &order).await.unwrap();
    let old = db
        .acquire("gateway", "trader", Duration::from_secs(30))
        .await
        .unwrap();
    risk.revalidate_fenced("A", "held", &order, &old)
        .await
        .unwrap();
    db.release(&old).await.unwrap();
    let current = db
        .acquire("gateway", "trader", Duration::from_secs(30))
        .await
        .unwrap();
    assert!(matches!(
        risk.revalidate_fenced("A", "held", &order, &old).await,
        Err(schwab_rs::Error::OwnershipLost)
    ));
    journal
        .set_operator_halt(Some("review".into()), "supervisor")
        .await
        .unwrap();
    assert!(
        risk.revalidate_fenced("A", "held", &order, &current)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn direct_child_lookup_wins_over_stale_embedded_parent_copy() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("children.json"));
    let id = journal
        .begin("A", "place_order", json!({"tag":"child"}))
        .await
        .unwrap();
    journal
        .finish(
            &id,
            SubmissionOutcome::Accepted {
                order_id: Some("100".into()),
            },
        )
        .await
        .unwrap();
    journal.database().reconcile_snapshot("A",1,chrono::Utc::now().timestamp_millis(),json!({}),vec![json!({"orderId":100,"status":"FILLED"}),json!({"orderId":99,"status":"WORKING","childOrderStrategies":[{"orderId":100,"status":"WORKING"}]})]).await.unwrap();
    // The direct FILLED copy won: 100 is no longer a target, 99 still is.
    use schwab_rs::execution::ExecutionStore;
    assert_eq!(
        journal.reconciliation_targets("A").await.unwrap(),
        vec!["99"]
    );
}

#[tokio::test]
async fn retention_keeps_live_order_links_until_terminal_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("retention.json"));
    let id = journal
        .begin("A", "place_order", json!({"tag":"live"}))
        .await
        .unwrap();
    journal
        .finish(
            &id,
            SubmissionOutcome::Accepted {
                order_id: Some("123".into()),
            },
        )
        .await
        .unwrap();
    let ack = journal
        .audit_page(0, 100)
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence;
    assert_eq!(journal.prune_resolved(i64::MAX, ack).await.unwrap(), 0);
    journal
        .database()
        .reconcile_snapshot(
            "A",
            1,
            chrono::Utc::now().timestamp_millis(),
            json!({}),
            vec![json!({"orderId":123,"status":"FILLED"})],
        )
        .await
        .unwrap();
    let ack = journal
        .audit_page(0, 100)
        .await
        .unwrap()
        .last()
        .unwrap()
        .sequence;
    assert_eq!(journal.prune_resolved(i64::MAX, ack).await.unwrap(), 1);
}

#[tokio::test]
async fn state_reopen_rejects_missing_metadata_and_schema_version() {
    for operation in ["DROP TABLE meta", "DELETE FROM meta WHERE key='schema'"] {
        let dir = tempfile::tempdir().unwrap();
        let database =
            schwab_rs::storage::StateDatabase::new(dir.path().join("state/state.sqlite3"));
        database
            .run(move |c| {
                c.execute_batch(operation)
                    .map_err(|e| schwab_rs::Error::Storage(e.to_string()))?;
                Ok(())
            })
            .await
            .unwrap();
        let reopened = StateDatabase::new(database.path().to_owned());
        assert!(reopened.run(|_| Ok(())).await.is_err());
        let connection = rusqlite::Connection::open(database.path()).unwrap();
        let has_meta: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='meta')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if operation == "DROP TABLE meta" {
            assert!(!has_meta);
        } else {
            let count: i64 = connection
                .query_row("SELECT COUNT(*) FROM meta WHERE key='schema'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(count, 0);
        }
    }
}

fn meta(connection: &rusqlite::Connection, key: &str) -> Option<String> {
    use rusqlite::OptionalExtension;
    connection
        .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
        .optional()
        .unwrap()
}

#[tokio::test]
async fn fresh_state_records_every_migration_and_refuses_newer_files() {
    let dir = tempfile::tempdir().unwrap();
    let database = StateDatabase::new(dir.path().join("fresh.json"));
    database.run(|_| Ok(())).await.unwrap();
    let connection = rusqlite::Connection::open(database.path()).unwrap();
    let applied: Vec<(u32, String)> = connection
        .prepare("SELECT version,name FROM schema_migrations ORDER BY version")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        applied,
        vec![
            (1, "baseline".to_string()),
            (2, "import_order_tags".into()),
            (3, "armed_intents_reservation_scope".into()),
            (4, "pending_intents".into())
        ]
    );
    assert_eq!(meta(&connection, "schema").as_deref(), Some("4"));
    assert_eq!(meta(&connection, "tags_imported"), None);
    connection
        .execute("UPDATE meta SET value='99' WHERE key='schema'", [])
        .unwrap();
    let newer = StateDatabase::new(database.path().to_owned());
    let error = newer.run(|_| Ok(())).await.unwrap_err();
    assert!(matches!(error, schwab_rs::Error::Storage(ref m) if m.contains("schema version")));
    assert_eq!(meta(&connection, "schema").as_deref(), Some("99"));
}

#[tokio::test]
async fn version_one_state_upgrades_in_place_and_keeps_its_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v1/state.sqlite3");
    // Build a complete version 1 file the way the previous release left it,
    // including its ad hoc tag-import flag and a submission with a tag that
    // was never copied into used_order_tags.
    let database = StateDatabase::new(path.clone());
    database.run(|_| Ok(())).await.unwrap();
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "DROP TABLE schema_migrations;
                 DROP INDEX reservations_held;
                 DROP INDEX submissions_order;
                 DROP INDEX orders_status;
                 DROP INDEX orders_tag;
                 DROP TABLE intents;
                 ALTER TABLE submissions DROP COLUMN armed;
                 ALTER TABLE reservations DROP COLUMN symbol;
                 ALTER TABLE reservations DROP COLUMN side;
                 ALTER TABLE reservations DROP COLUMN replaces;
                 UPDATE meta SET value='1' WHERE key='schema';
                 INSERT INTO meta VALUES('tags_imported','1');
                 INSERT INTO submissions VALUES('a1','A','place_order',1,\
                    '{\"tag\":\"legacy-tag\"}','{\"status\":\"rejected\"}',NULL);",
            )
            .unwrap();
    }
    let upgraded = OrderJournal::new(path.clone());
    assert_eq!(upgraded.records().await.unwrap().len(), 1);
    assert!(matches!(
        upgraded
            .begin("A", "place_order", json!({"tag":"legacy-tag"}))
            .await,
        Err(schwab_rs::Error::Duplicate(_))
    ));
    // Rows written before the marker existed read as armed.
    assert!(upgraded.records().await.unwrap()[0].armed);
    let connection = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(meta(&connection, "schema").as_deref(), Some("4"));
    assert_eq!(meta(&connection, "tags_imported"), None);
    let history: Vec<u32> = connection
        .prepare("SELECT version FROM schema_migrations ORDER BY version")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(history, vec![2, 3, 4]);
}

#[tokio::test]
async fn pooled_connections_never_inherit_an_open_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let database = StateDatabase::new(dir.path().join("pool.json"));
    database
        .run(|c| {
            c.execute_batch("BEGIN IMMEDIATE; INSERT INTO meta VALUES('leaked','1');")
                .map_err(|e| schwab_rs::Error::Storage(e.to_string()))
        })
        .await
        .unwrap();
    let leaked = database
        .run(|c| {
            assert!(c.is_autocommit());
            c.query_row("SELECT COUNT(*) FROM meta WHERE key='leaked'", [], |r| {
                r.get::<_, i64>(0)
            })
            .map_err(|e| schwab_rs::Error::Storage(e.to_string()))
        })
        .await
        .unwrap();
    assert_eq!(leaked, 0, "an abandoned transaction must roll back");
}

#[tokio::test]
async fn concurrent_first_use_initializes_once() {
    let dir = tempfile::tempdir().unwrap();
    let database = StateDatabase::new(dir.path().join("race.json"));
    let tasks: Vec<_> = (0..16)
        .map(|i| {
            let database = database.clone();
            tokio::spawn(async move { database.quota(&format!("k{i}"), 10, 10).await })
        })
        .collect();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    let connection = rusqlite::Connection::open(database.path()).unwrap();
    let migrations: i64 = connection
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(migrations, 4);
}

/// A partial fill shrinks the order's held reservation to what can still
/// fill; the filled part is already out of the broker's available funds.
#[tokio::test]
async fn partial_fills_shrink_held_reservations() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("partial.json"));
    let db = journal.database().clone();
    let baseline =
        json!({"securitiesAccount":{"currentBalances":{"availableFunds":1000},"positions":[]}});
    let now = || chrono::Utc::now().timestamp_millis();
    db.reconcile_snapshot("A", 1, now(), baseline.clone(), vec![])
        .await
        .unwrap();
    let risk = PortfolioRisk {
        database: db.clone(),
        accounts: HashMap::from([(
            "A".into(),
            PortfolioLimits {
                max_open_quantity: 10.into(),
                max_open_notional: 100.into(),
                max_position_quantity: 100.into(),
                max_snapshot_age_ms: 5000,
                max_daily_loss: None,
            },
        )]),
    };
    let order = |quantity: u32| -> schwab_rs::types::trading::OrderRequest {
        serde_json::from_value(json!({"orderStrategyType":"SINGLE","orderType":"LIMIT",
            "price":"10","session":"NORMAL","duration":"DAY","orderLegCollection":[
            {"quantity":quantity.to_string(),"instruction":"BUY",
             "instrument":{"symbol":"AAPL","assetType":"EQUITY"}}]}))
        .unwrap()
    };
    let id = journal
        .begin_once("A", "place_order", json!({}), None)
        .await
        .unwrap();
    risk.reserve("A", &id, &order(10)).await.unwrap();
    journal
        .finish(
            &id,
            SubmissionOutcome::Accepted {
                order_id: Some("9".into()),
            },
        )
        .await
        .unwrap();
    // The full reservation leaves no room.
    assert!(risk.reserve("A", "next", &order(6)).await.is_err());
    let working = json!({"orderId":9,"status":"WORKING","orderStrategyType":"SINGLE",
        "orderType":"LIMIT","quantity":10,"filledQuantity":6,"remainingQuantity":4,"price":10,
        "orderLegCollection":[{"instruction":"BUY","quantity":10,
            "instrument":{"assetType":"EQUITY","symbol":"AAPL"}}]});
    db.reconcile_snapshot("A", 1, now(), baseline, vec![working])
        .await
        .unwrap();
    // Four remain held, so six more fit the ten-share cap.
    risk.reserve("A", "next", &order(6)).await.unwrap();
}
