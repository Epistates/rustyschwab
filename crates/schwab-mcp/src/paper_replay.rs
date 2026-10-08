//! Shared local paper command execution and bounded JSONL decoding.
use schwab_rs::{
    broker::TradingBackend,
    paper::{PAPER_REJECTION_TITLE, PaperBroker, PaperCommand},
};
use serde_json::Value;
use std::io::{BufRead, Read};

pub const MAX_RECORD_BYTES: u64 = 1024 * 1024;

/// A physical line is one record; blank or oversized lines are errors.
pub fn read_record<T: serde::de::DeserializeOwned>(
    reader: &mut impl BufRead,
) -> anyhow::Result<Option<T>> {
    let mut line = Vec::new();
    let bytes = reader
        .by_ref()
        .take(MAX_RECORD_BYTES + 1)
        .read_until(b'\n', &mut line)?;
    if bytes == 0 {
        return Ok(None);
    }
    anyhow::ensure!(
        bytes as u64 <= MAX_RECORD_BYTES,
        "Paper record exceeds 1 MiB"
    );
    Ok(Some(serde_json::from_slice(&line)?))
}

pub async fn execute(broker: &PaperBroker, command: PaperCommand) -> anyhow::Result<Value> {
    let result = match command {
        PaperCommand::Archive {
            destination,
            through_sequence,
        } => serde_json::to_value(broker.archive_events(destination, through_sequence).await?),
        PaperCommand::Verify => serde_json::to_value(broker.verify_integrity().await?),
        PaperCommand::Event { event } => serde_json::to_value(broker.apply(event).await?),
        PaperCommand::Place { account, order } => {
            serde_json::to_value(broker.place_order(&account, &order).await?)
        }
        PaperCommand::Replace {
            account,
            order_id,
            order,
        } => serde_json::to_value(broker.replace_order(&account, &order_id, &order).await?),
        PaperCommand::Cancel { account, order_id } => {
            broker.cancel_order(&account, &order_id).await?;
            Ok(serde_json::json!({"cancel_requested":true}))
        }
        PaperCommand::Snapshot { account } => {
            serde_json::to_value(broker.snapshot(&account).await?)
        }
        PaperCommand::Halt { reason } => {
            broker
                .halt_submissions(&reason, "local-paper-operator")
                .await?;
            Ok(serde_json::json!({"halted":true}))
        }
        PaperCommand::Resume => {
            broker.clear_operator_halt("local-paper-operator").await?;
            Ok(serde_json::json!({"halted":false}))
        }
    }?;
    Ok(serde_json::json!({"mode":"paper","result":result}))
}

/// Restore a private backup into scratch state, replay its committed execution
/// history, and compare all account snapshots and historical orders. Admission
/// policy, leases, halts and rejected requests without state changes are outside
/// this execution-state comparison.
pub async fn verify_backup(
    config: schwab_rs::paper::PaperConfig,
    backup: std::path::PathBuf,
) -> anyhow::Result<Value> {
    use anyhow::Context;
    use schwab_rs::paper::{PaperCapturedCommand, PaperCapturedOutcome, PaperClockMode};
    anyhow::ensure!(
        config.clock == PaperClockMode::Replay,
        "Backup verification requires replay clock mode"
    );
    let scratch = tempfile::tempdir()?;
    let restored = schwab_rs::storage::StateDatabase::restore(
        backup,
        scratch.path().join("source/state.sqlite3"),
    )
    .await?;
    let source = PaperBroker::open(restored.path().to_owned(), config.clone()).await?;
    let count = source.captured_command_count().await?;
    source.verify_integrity().await?;
    let replay =
        PaperBroker::open(scratch.path().join("replay/state.sqlite3"), config.clone()).await?;
    let mut cursor = 0;
    let mut applied = 0_u64;
    loop {
        let page = source.order_journal().audit_page(cursor, 100).await?;
        if page.is_empty() {
            break;
        }
        for event in page {
            cursor = event.sequence;
            // Operator halts and their clearing are not captured commands,
            // but they decide what the next command meets: mirror them in
            // order. A halt the daily-loss control trips is re-created by
            // the replayed events themselves.
            if event.kind == "operator_halt_changed" {
                use schwab_rs::broker::TradingBackend;
                match event.payload.get("reason").and_then(|r| r.as_str()) {
                    None => replay.clear_operator_halt(&event.subject).await?,
                    Some(_) if event.subject == schwab_rs::risk::DAILY_LOSS_CONTROL => {}
                    Some(reason) => replay.halt_submissions(reason, &event.subject).await?,
                }
                continue;
            }
            if event.kind != "paper_command" {
                continue;
            }
            let record: PaperCapturedCommand = serde_json::from_value(event.payload)?;
            anyhow::ensure!(
                record.index == applied + 1,
                "Captured command sequence has a gap or conflict"
            );
            anyhow::ensure!(
                matches!(
                    &record.command,
                    PaperCommand::Event { .. }
                        | PaperCommand::Place { .. }
                        | PaperCommand::Replace { .. }
                        | PaperCommand::Cancel { .. }
                ),
                "Non-execution command in captured history"
            );
            let result = execute(&replay, record.command).await;
            match record.outcome {
                PaperCapturedOutcome::Success { result: expected } => {
                    let actual = result
                        .with_context(|| format!("Captured command {} failed", record.index))?;
                    anyhow::ensure!(
                        actual["result"] == expected,
                        "Captured command {} result mismatch",
                        record.index
                    );
                }
                PaperCapturedOutcome::InjectedRejection => {
                    anyhow::ensure!(
                        matches!(result.as_ref().err().and_then(|e| e.downcast_ref::<schwab_rs::Error>()),
                        Some(schwab_rs::Error::Broker { details, .. })
                            if details.first().is_some_and(|d| d.title.as_deref() == Some(PAPER_REJECTION_TITLE)
                                && d.detail.as_deref() == Some("Injected paper rejection"))),
                        "Captured rejection mismatch at {}",
                        record.index
                    );
                }
                PaperCapturedOutcome::LostAcknowledgement { order_id } => {
                    anyhow::ensure!(
                        matches!(result.as_ref().err().and_then(|e| e.downcast_ref::<schwab_rs::Error>()),
                        Some(schwab_rs::Error::SubmissionUnknown { attempt_id, .. }) if attempt_id == &format!("paper-{order_id}")),
                        "Captured lost acknowledgement mismatch at {}",
                        record.index
                    );
                }
            }
            applied = record.index;
        }
    }
    anyhow::ensure!(applied == count, "Captured command count mismatch");
    for account in config.accounts.keys() {
        anyhow::ensure!(
            serde_json::to_value(source.snapshot(account).await?)?
                == serde_json::to_value(replay.snapshot(account).await?)?,
            "Account replay mismatch for {account}"
        );
        let mut after = 0;
        loop {
            let expected = source.orders(account, after, 100).await?;
            let actual = replay.orders(account, after, 100).await?;
            anyhow::ensure!(
                serde_json::to_value(&expected)? == serde_json::to_value(&actual)?,
                "Order history replay mismatch for {account}"
            );
            let Some(last) = expected.last() else {
                break;
            };
            after = last.order_id.parse()?;
        }
    }
    let integrity = replay.verify_integrity().await?;
    Ok(
        serde_json::json!({"mode":"paper","verified_captured_commands":applied,"integrity":integrity}),
    )
}
