//! Trusted local operator interface; intentionally not exposed as MCP tools.
use anyhow::Context;
use clap::{Parser, Subcommand};
use schwab_rs::{
    execution::{ExecutionStore, OrderJournal},
    storage::StateDatabase,
};
use std::path::PathBuf;
#[derive(Parser)]
#[command(about = "Schwab durable state and operator approvals")]
struct Args {
    /// Journal path, legacy JSON path, or state.sqlite3 path.
    #[arg(long)]
    state: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Print the approval digest for a tool invocation without touching state.
    Digest { tool: String, arguments: PathBuf },
    #[command(flatten)]
    Durable(DurableCommand),
}
/// Commands that operate on the `--state` journal.
#[derive(Subcommand)]
enum DurableCommand {
    Status,
    Halt {
        reason: String,
        #[arg(long)]
        operator: String,
    },
    Resume {
        #[arg(long)]
        operator: String,
    },
    DeliverAudit {
        sink_id: String,
        endpoint: String,
        #[arg(long)]
        token_file: PathBuf,

        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
    Export {
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 1000)]
        limit: u32,
    },
    Backup {
        destination: PathBuf,
    },
    Restore {
        destination: PathBuf,
    },
    Prune {
        #[arg(long)]
        before_ms: i64,
        #[arg(long)]
        exported_through: i64,
    },
    Approve {
        principal: String,
        digest: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        expires_at: i64,
    },
    Revoke {
        digest: String,
        #[arg(long)]
        operator: String,
    },
    Reconcile {
        account: String,
    },
    Resolve {
        attempt: String,
        #[arg(long)]
        evidence: String,
    },
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let command = match args.command {
        Command::Digest { tool, arguments } => {
            let value = serde_json::from_slice(&std::fs::read(arguments)?)?;
            println!("{}", schwab_mcp::policy::intent_digest(&tool, &value));
            return Ok(());
        }
        Command::Durable(command) => command,
    };
    let state = args
        .state
        .context("--state is required for durable operations")?;
    let journal = OrderJournal::new(state.clone());
    match command {
        DurableCommand::Halt { reason, operator } => {
            journal.set_operator_halt(Some(reason), &operator).await?
        }
        DurableCommand::Resume { operator } => journal.set_operator_halt(None, &operator).await?,
        DurableCommand::DeliverAudit {
            sink_id,
            endpoint,
            token_file,
            limit,
        } => {
            schwab_rs::security::verify_file_permissions(&token_file)?;
            let token =
                secrecy::SecretString::from(std::fs::read_to_string(token_file)?.trim().to_owned());
            println!(
                "{}",
                serde_json::to_string(
                    &journal
                        .deliver_audit(&sink_id, &endpoint, token, limit)
                        .await?
                )?
            );
        }
        DurableCommand::Status => println!(
            "{}",
            serde_json::to_string_pretty(&journal.records().await?)?
        ),
        DurableCommand::Export { after, limit } => {
            for event in journal.audit_page(after, limit).await? {
                println!("{}", serde_json::to_string(&event)?);
            }
        }
        DurableCommand::Backup { destination } => journal.database().backup(destination).await?,
        DurableCommand::Restore { destination } => {
            let state = StateDatabase::restore(state, destination).await?;
            println!("{}", state.path().display());
        }
        DurableCommand::Prune {
            before_ms,
            exported_through,
        } => println!(
            "{}",
            journal.prune_resolved(before_ms, exported_through).await?
        ),
        DurableCommand::Approve {
            principal,
            digest,
            operator,
            expires_at,
        } => {
            journal
                .approve(&principal, &digest, &operator, expires_at)
                .await?
        }
        DurableCommand::Revoke { digest, operator } => {
            journal.revoke_approval(&digest, &operator).await?
        }
        DurableCommand::Resolve { attempt, evidence } => {
            journal
                .finish(
                    &attempt,
                    schwab_rs::execution::SubmissionOutcome::Reconciled { evidence },
                )
                .await?
        }
        DurableCommand::Reconcile { account } => {
            let client = schwab_rs::SchwabClient::new(schwab_rs::SchwabConfig::from_env()?)?;
            anyhow::ensure!(
                client.order_journal().path() == journal.path(),
                "--state must match the configured client's order journal"
            );
            client
                .init()
                .await
                .context("Broker authentication failed")?;
            let generation = journal
                .database()
                .account_state(&account)
                .await?
                .map_or(0, |s| s.generation);
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &client.reconcile_account(&account, generation).await?
                )?
            );
        }
    }
    Ok(())
}
