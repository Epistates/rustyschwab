use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};

fn verify(dir: &Path, commands: &str, expected: &str) -> Output {
    std::fs::write(
        dir.join("config.json"),
        include_str!("../../../ops/paper/config.json"),
    )
    .unwrap();
    std::fs::write(dir.join("commands.jsonl"), commands).unwrap();
    std::fs::write(dir.join("expected.jsonl"), expected).unwrap();
    Command::new(env!("CARGO_BIN_EXE_schwab-paper-verify"))
        .arg("--config")
        .arg(dir.join("config.json"))
        .arg("--commands")
        .arg(dir.join("commands.jsonl"))
        .arg("--expected")
        .arg(dir.join("expected.jsonl"))
        .output()
        .unwrap()
}
fn commands() -> String {
    format!(
        "{}\n{{\"command\":\"verify\"}}\n",
        include_str!("../../../ops/paper/replay.jsonl").trim_end()
    )
}
const EXPECTED: &str = include_str!("../../../ops/paper/replay-expected.jsonl");

#[test]
fn verifies_historical_transcript_in_disposable_state() {
    let dir = tempfile::tempdir().unwrap();
    let result = verify(dir.path(), &commands(), EXPECTED);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["verified_commands"], 9);
    assert_eq!(report["integrity"]["orders"], 1);
    assert!(!dir.path().join("state").exists());
}

#[test]
fn changed_expected_output_and_truncated_or_extra_transcripts_fail() {
    let dir = tempfile::tempdir().unwrap();
    let mut values: Vec<Value> = EXPECTED
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    values[7]["result"]["ledger"]["cash"] = serde_json::json!("1");
    let changed = values
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let result = verify(dir.path(), &commands(), &changed);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("mismatch at line 8"));
    assert!(
        !verify(dir.path(), &commands(), EXPECTED.lines().next().unwrap())
            .status
            .success()
    );
    assert!(
        !verify(dir.path(), &commands(), &format!("{EXPECTED}{{}}\n"))
            .status
            .success()
    );
}

#[test]
fn verification_requires_final_account_snapshots_and_forbids_archive_writes() {
    let dir = tempfile::tempdir().unwrap();
    let first = include_str!("../../../ops/paper/replay.jsonl")
        .lines()
        .next()
        .unwrap();
    let result = verify(dir.path(), first, EXPECTED.lines().next().unwrap());
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Final snapshots required"));
    let destination = dir.path().join("must-not-exist.sqlite3");
    let command =
        serde_json::json!({"command":"archive","destination":destination,"through_sequence":1});
    let result = verify(dir.path(), &command.to_string(), "{}");
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Archive commands are forbidden"));
    assert!(!destination.exists());
}

#[test]
fn bounded_decoder_rejects_oversized_and_blank_records() {
    use schwab_mcp::paper_replay::read_record;
    let oversized = vec![b' '; 1024 * 1024 + 1];
    assert!(read_record::<Value>(&mut oversized.as_slice()).is_err());
    assert!(read_record::<Value>(&mut b"\n".as_slice()).is_err());
    assert!(read_record::<Value>(&mut b"".as_slice()).unwrap().is_none());
}

async fn captured_run(dir: &Path) -> schwab_rs::paper::PaperBroker {
    use schwab_mcp::paper_replay::execute;
    use schwab_rs::paper::{PaperBroker, PaperCommand};
    let config = serde_json::from_str(include_str!("../../../ops/paper/config.json")).unwrap();
    let broker = PaperBroker::open(dir.join("state/run.sqlite3"), config)
        .await
        .unwrap();
    let commands: Vec<PaperCommand> = include_str!("../../../ops/paper/replay.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for command in commands {
        execute(&broker, command).await.unwrap();
    }
    broker
}

#[tokio::test]
async fn backup_replay_verifies_archived_history_without_modifying_backup() {
    let dir = tempfile::tempdir().unwrap();
    let broker = captured_run(dir.path()).await;
    broker
        .archive_events(dir.path().join("archive/first.sqlite3"), 4)
        .await
        .unwrap();
    let backup = dir.path().join("backup/run.sqlite3");
    broker
        .order_journal()
        .database()
        .backup(backup.clone())
        .await
        .unwrap();
    let original = std::fs::read(&backup).unwrap();
    let report = schwab_mcp::paper_replay::verify_backup(broker.config().clone(), backup.clone())
        .await
        .unwrap();
    assert_eq!(report["verified_captured_commands"], 6);
    assert_eq!(std::fs::read(backup).unwrap(), original);
}

#[tokio::test]
async fn backup_replay_rejects_capture_gaps_and_missing_capture_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let broker = captured_run(dir.path()).await;
    broker
        .order_journal()
        .database()
        .run(|c| {
            c.execute(
                "DELETE FROM audit WHERE kind='paper_command' AND subject='2'",
                [],
            )
            .map_err(|e| schwab_rs::Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
        .unwrap();
    let backup = dir.path().join("gap/run.sqlite3");
    broker
        .order_journal()
        .database()
        .backup(backup.clone())
        .await
        .unwrap();
    let error = schwab_mcp::paper_replay::verify_backup(broker.config().clone(), backup)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("capture history is incomplete"));
    broker
        .order_journal()
        .database()
        .run(|c| {
            c.execute("DELETE FROM meta WHERE key='paper_capture_count'", [])
                .map_err(|e| schwab_rs::Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(broker.captured_command_count().await.is_err());
}

#[tokio::test]
async fn backup_replay_detects_ledger_changes_that_preserve_conservation() {
    let dir = tempfile::tempdir().unwrap();
    let broker = captured_run(dir.path()).await;
    broker
        .order_journal()
        .database()
        .run(|c| {
            c.execute(
                "UPDATE paper_accounts SET payload=\
                 json_set(payload,'$.cash','99800.9700','$.realized_pnl','1')",
                [],
            )
            .map_err(|e| schwab_rs::Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
        .unwrap();
    broker.verify_integrity().await.unwrap();
    let backup = dir.path().join("backup/run.sqlite3");
    broker
        .order_journal()
        .database()
        .backup(backup.clone())
        .await
        .unwrap();
    let error = schwab_mcp::paper_replay::verify_backup(broker.config().clone(), backup)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Account replay mismatch"));
}

#[tokio::test]
async fn backup_replay_reproduces_committed_rejection_and_lost_acknowledgement() {
    use schwab_rs::paper::*;
    let dir = tempfile::tempdir().unwrap();
    let broker = captured_run(dir.path()).await;
    let mut order: schwab_rs::types::trading::OrderRequest =
        serde_json::from_value(serde_json::json!({
            "tag": "rejected",
            "orderStrategyType": "SINGLE",
            "orderType": "LIMIT",
            "session": "NORMAL",
            "duration": "DAY",
            "price": "101",
            "orderLegCollection": [{
                "quantity": "1",
                "instruction": "BUY",
                "instrument": {"symbol": "ABC", "assetType": "EQUITY"},
            }],
        }))
        .unwrap();
    broker
        .apply(PaperEvent {
            sequence: 5,
            at_ms: 270,
            kind: PaperEventKind::Fault {
                fault: PaperFault::RejectNext,
            },
        })
        .await
        .unwrap();
    assert!(broker.place_order("paper-demo", &order).await.is_err());
    broker
        .apply(PaperEvent {
            sequence: 6,
            at_ms: 280,
            kind: PaperEventKind::Fault {
                fault: PaperFault::LoseNextAcknowledgement,
            },
        })
        .await
        .unwrap();
    order.tag = Some("lost-ack".into());
    assert!(matches!(
        broker.place_order("paper-demo", &order).await,
        Err(schwab_rs::Error::SubmissionUnknown { .. })
    ));
    let backup = dir.path().join("backup/run.sqlite3");
    broker
        .order_journal()
        .database()
        .backup(backup.clone())
        .await
        .unwrap();
    let report = schwab_mcp::paper_replay::verify_backup(broker.config().clone(), backup)
        .await
        .unwrap();
    assert_eq!(report["verified_captured_commands"], 10);
}

/// A halt the daily-loss control tripped and an operator cleared is part of
/// the history a backup replays: the orders placed after the clear still
/// verify.
#[tokio::test]
async fn backup_replay_follows_a_cleared_daily_loss_halt() {
    use schwab_rs::broker::TradingBackend;
    use schwab_rs::paper::*;
    let dir = tempfile::tempdir().unwrap();
    let mut config: PaperConfig =
        serde_json::from_str(include_str!("../../../ops/paper/config.json")).unwrap();
    config
        .accounts
        .get_mut("paper-demo")
        .unwrap()
        .limits
        .max_daily_loss = Some("5".parse().unwrap());
    let broker = PaperBroker::open(dir.path().join("state/run.sqlite3"), config)
        .await
        .unwrap();
    let quote = |sequence, at_ms, bid: &str, ask: &str| PaperEvent {
        sequence,
        at_ms,
        kind: PaperEventKind::Quote {
            symbol: "ABC".into(),
            source_at_ms: at_ms,
            bid: bid.parse().unwrap(),
            ask: ask.parse().unwrap(),
            bid_size: "100".parse().unwrap(),
            ask_size: "100".parse().unwrap(),
        },
    };
    let order = |tag: &str| -> schwab_rs::types::trading::OrderRequest {
        serde_json::from_value(serde_json::json!({
            "tag": tag, "orderStrategyType": "SINGLE", "orderType": "LIMIT",
            "session": "NORMAL", "duration": "DAY", "price": "101",
            "orderLegCollection": [{"quantity": "10", "instruction": "BUY",
                "instrument": {"symbol": "ABC", "assetType": "EQUITY"}}]
        }))
        .unwrap()
    };
    broker
        .apply(PaperEvent {
            sequence: 1,
            at_ms: 100,
            kind: PaperEventKind::Session {
                session_id: "day-1".into(),
                open: true,
            },
        })
        .await
        .unwrap();
    broker.apply(quote(2, 110, "100", "100.01")).await.unwrap();
    broker
        .place_order("paper-demo", &order("first"))
        .await
        .unwrap();
    broker.apply(quote(3, 200, "100", "100.01")).await.unwrap();
    // Ten shares down a dollar each: past the five-dollar cap.
    broker.apply(quote(4, 300, "99", "99.01")).await.unwrap();
    assert!(broker.operator_halt().await.unwrap().is_some());
    broker.clear_operator_halt("desk").await.unwrap();
    broker
        .place_order("paper-demo", &order("second"))
        .await
        .unwrap();
    let backup = dir.path().join("backup/run.sqlite3");
    broker
        .order_journal()
        .database()
        .backup(backup.clone())
        .await
        .unwrap();
    schwab_mcp::paper_replay::verify_backup(broker.config().clone(), backup)
        .await
        .unwrap();
}
