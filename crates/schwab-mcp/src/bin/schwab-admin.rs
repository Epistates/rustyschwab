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
    /// Delete terminal broker orders, their observed activity and released
    /// risk reservations last seen before `--before-ms`. Working orders,
    /// held reservations, spent tags and audit events are kept.
    PruneHistory {
        #[arg(long)]
        before_ms: i64,
    },
    /// List intents principals asked for that need approval (in a
    /// principal's audit journal), newest first, as the orders they are.
    Pending {
        /// Look back this many hours.
        #[arg(long, default_value_t = 24)]
        hours: i64,
        /// Include intents already approved, spent, revoked or lapsed.
        #[arg(long)]
        all: bool,
    },
    /// Show one intent: the order its digest binds and the model's
    /// rationale.
    Show {
        digest: String,
    },
    /// Approve an intent. Prints the order it binds first and asks for
    /// confirmation unless `--yes`; an intent this journal has never seen
    /// (so cannot show) needs `--yes`.
    Approve {
        principal: String,
        digest: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        expires_at: i64,
        #[arg(long)]
        yes: bool,
    },
    /// Kill switch: halt submissions, then cancel every working order on
    /// the given accounts (all linked accounts if none), with the broker
    /// credentials configured as for `reconcile`.
    Kill {
        #[arg(long)]
        reason: String,
        #[arg(long)]
        operator: String,
        #[arg(long = "account")]
        accounts: Vec<String>,
    },
    /// Release the risk reserved for a resolved submission, on evidence.
    ReleaseReservation {
        attempt: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        evidence: String,
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
            let token = schwab_rs::security::read_secret_file(&token_file)?;
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
        DurableCommand::PruneHistory { before_ms } => {
            println!("{}", journal.prune_history(before_ms).await?)
        }
        DurableCommand::Pending { hours, all } => {
            let since = chrono::Utc::now().timestamp_millis() - hours.max(0) * 3_600_000;
            for intent in journal.intents(None, since, 500).await? {
                if all || intent.state == schwab_rs::execution::IntentState::Awaiting {
                    println!("{}\n", schwab_mcp::approvals::render(&intent));
                }
            }
        }
        DurableCommand::Show { digest } => {
            let intent = journal
                .intents(Some(&digest), 0, 1)
                .await?
                .pop()
                .context("No intent with that digest was asked for in this journal")?;
            println!("{}", schwab_mcp::approvals::render(&intent));
        }
        DurableCommand::Approve {
            principal,
            digest,
            operator,
            expires_at,
            yes,
        } => {
            match journal.intents(Some(&digest), 0, 1).await?.pop() {
                Some(intent) => {
                    anyhow::ensure!(
                        intent.principal == principal,
                        "Intent {digest} was asked for by {}, not {principal}",
                        intent.principal
                    );
                    eprintln!("{}", schwab_mcp::approvals::render(&intent));
                }
                None => {
                    anyhow::ensure!(
                        yes,
                        "This journal has not seen intent {digest}, so it cannot show what \
                         it is; verify it with `digest` and pass --yes"
                    );
                    eprintln!("Approving an intent this journal has not seen: {digest}");
                }
            }
            if !yes {
                use std::io::{BufRead, IsTerminal, Write};
                anyhow::ensure!(
                    std::io::stdin().is_terminal(),
                    "Confirm with --yes when not at a terminal"
                );
                eprint!("Approve this order? [y/N] ");
                std::io::stderr().flush()?;
                let mut answer = String::new();
                std::io::stdin().lock().read_line(&mut answer)?;
                anyhow::ensure!(matches!(answer.trim(), "y" | "Y" | "yes"), "Not approved");
            }
            journal
                .approve(&principal, &digest, &operator, expires_at)
                .await?
        }
        DurableCommand::Kill {
            reason,
            operator,
            accounts,
        } => {
            use schwab_rs::broker::TradingBackend;
            let client = schwab_rs::SchwabClient::new(schwab_rs::SchwabConfig::from_env()?)?;
            anyhow::ensure!(
                client.order_journal().path() == journal.path(),
                "--state must match the configured client's order journal"
            );
            // Halt first, so nothing new is placed while orders are cancelled.
            client.halt_submissions(&reason, &operator).await?;
            client
                .init()
                .await
                .context("Broker authentication failed")?;
            let accounts = if accounts.is_empty() {
                client
                    .get_account_numbers()
                    .await?
                    .into_iter()
                    .map(|a| a.hash_value)
                    .collect()
            } else {
                accounts
            };
            let mut report = Vec::new();
            for account in accounts {
                let outcomes = match TradingBackend::cancel_working_orders(&client, &account).await
                {
                    Ok(outcomes) => outcomes
                        .into_iter()
                        .map(|(id, outcome)| {
                            serde_json::json!({
                                "orderId": id,
                                "cancelled": outcome.is_ok(),
                                "error": outcome.err().map(|e| e.to_string()),
                            })
                        })
                        .collect::<Vec<_>>(),
                    Err(e) => vec![serde_json::json!({"error": e.to_string()})],
                };
                report.push(serde_json::json!({"account": account, "orders": outcomes}));
            }
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        DurableCommand::ReleaseReservation {
            attempt,
            operator,
            evidence,
        } => {
            journal
                .release_reservation(&attempt, &operator, &evidence)
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
