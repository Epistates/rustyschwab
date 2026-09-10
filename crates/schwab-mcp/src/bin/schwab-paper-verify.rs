//! Re-execute an expected paper transcript in disposable isolated state.
use anyhow::Context;
use clap::Parser;
use schwab_mcp::paper_replay::{execute, read_record};
use schwab_rs::paper::{PaperBroker, PaperClockMode, PaperCommand, PaperConfig};
use serde_json::Value;
use std::{collections::BTreeSet, fs::File, io::BufReader, path::PathBuf};

#[derive(Parser)]
#[command(about = "Verify an offline paper transcript in fresh temporary state")]
struct Args {
    #[arg(long)]
    config: PathBuf,
    #[arg(
        long,
        required_unless_present = "backup",
        conflicts_with = "backup",
        requires = "expected"
    )]
    commands: Option<PathBuf>,
    #[arg(
        long,
        required_unless_present = "backup",
        conflicts_with = "backup",
        requires = "commands"
    )]
    expected: Option<PathBuf>,
    #[arg(long, conflicts_with_all = ["commands", "expected"])]
    backup: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config: PaperConfig = serde_json::from_slice(&std::fs::read(args.config)?)?;
    anyhow::ensure!(
        config.clock == PaperClockMode::Replay,
        "Verification requires replay clock mode"
    );
    if let Some(backup) = args.backup {
        let report = schwab_mcp::paper_replay::verify_backup(config, backup).await?;
        println!("{report}");
        return Ok(());
    }
    let accounts: BTreeSet<_> = config.accounts.keys().cloned().collect();
    let scratch = tempfile::tempdir()?;
    let broker = PaperBroker::open(scratch.path().join("state/paper.sqlite3"), config).await?;
    let mut commands = BufReader::new(File::open(
        args.commands.context("Commands file required")?,
    )?);
    let mut expected = BufReader::new(File::open(
        args.expected.context("Expected file required")?,
    )?);
    let mut count = 0_u64;
    let mut snapshots = BTreeSet::new();
    loop {
        let line = count + 1;
        let command: Option<PaperCommand> = read_record(&mut commands)
            .with_context(|| format!("Invalid command at line {line}"))?;
        let Some(command) = command else {
            break;
        };
        let expected: Value = read_record(&mut expected)
            .with_context(|| format!("Invalid expected output at line {line}"))?
            .with_context(|| format!("Expected output missing at line {line}"))?;
        match &command {
            PaperCommand::Archive { .. } => {
                anyhow::bail!("Archive commands are forbidden during verification")
            }
            PaperCommand::Snapshot { account } => {
                snapshots.insert(account.clone());
            }
            PaperCommand::Verify => {}
            PaperCommand::Event { .. }
            | PaperCommand::Place { .. }
            | PaperCommand::Replace { .. }
            | PaperCommand::Cancel { .. }
            | PaperCommand::Halt { .. }
            | PaperCommand::Resume => snapshots.clear(),
        }
        let actual = execute(&broker, command)
            .await
            .with_context(|| format!("Command failed at line {line}"))?;
        anyhow::ensure!(actual == expected, "Transcript mismatch at line {line}");
        count = line;
    }
    anyhow::ensure!(count > 0, "Empty transcript");
    anyhow::ensure!(
        read_record::<Value>(&mut expected)?.is_none(),
        "Expected transcript has extra records"
    );
    anyhow::ensure!(
        accounts.is_subset(&snapshots),
        "Final snapshots required for every account after the last mutation"
    );
    let integrity = broker.verify_integrity().await?;
    println!(
        "{}",
        serde_json::json!({"mode":"paper","verified_commands":count,"integrity":integrity})
    );
    Ok(())
}
