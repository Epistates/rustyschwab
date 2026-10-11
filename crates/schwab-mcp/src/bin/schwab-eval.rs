//! Evaluate a model's tool use against the paper venue: serve a scenario over
//! stdio while recording the calls, then score the transcript offline.
use anyhow::Context;
use clap::{Parser, Subcommand};
use schwab_mcp::eval::{Fixture, Recording, Scenario, Transcript, score};
use std::{fs::File, io::BufReader, path::PathBuf};

#[derive(Parser)]
#[command(about = "Score a model's tool-call transcript against a paper trading scenario")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print every scenario with its task and what passing means.
    List,
    /// Serve a scenario to an MCP client over stdio, recording each tool
    /// call to the transcript before it is dispatched. Give the model the
    /// scenario's task (printed on stderr) as its user turn.
    Serve {
        #[arg(long)]
        scenario: Scenario,
        /// JSONL transcript to write; scored later with `score`.
        #[arg(long)]
        record: PathBuf,
        /// The model under test, recorded in the transcript.
        #[arg(long)]
        model: String,
        /// File holding the system prompt the model runs under, recorded verbatim.
        #[arg(long)]
        prompt_file: PathBuf,
        /// Optional JSON file describing anything else that shaped the run.
        #[arg(long)]
        config_file: Option<PathBuf>,
    },
    /// Replay a transcript through a fresh scenario fixture and print the
    /// report as JSON. Exits non-zero when the transcript fails.
    Score {
        #[arg(long)]
        scenario: Scenario,
        #[arg(long)]
        transcript: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Args::parse().command {
        Command::List => {
            for scenario in Scenario::ALL {
                println!(
                    "{}\n  task: {}\n  pass: {}\n",
                    scenario.name(),
                    scenario.task(),
                    scenario.expectation()
                );
            }
            Ok(())
        }
        Command::Serve {
            scenario,
            record,
            model,
            prompt_file,
            config_file,
        } => {
            let prompt = std::fs::read_to_string(&prompt_file)
                .with_context(|| format!("failed to read {}", prompt_file.display()))?;
            let config = match config_file {
                Some(path) => serde_json::from_slice(
                    &std::fs::read(&path)
                        .with_context(|| format!("failed to read {}", path.display()))?,
                )?,
                None => serde_json::Value::Null,
            };
            let sink = File::create_new(&record)
                .with_context(|| format!("failed to create {}", record.display()))?;
            let scratch = tempfile::tempdir()?;
            let fixture = Fixture::open(scenario, scratch.path()).await?;
            let server = Recording::new(fixture.server, Box::new(sink), model, prompt, config)?;
            eprintln!("scenario {}: {}", scenario.name(), scenario.task());
            server.run_stdio().await?;
            Ok(())
        }
        Command::Score {
            scenario,
            transcript,
        } => {
            let mut reader = BufReader::new(
                File::open(&transcript)
                    .with_context(|| format!("failed to open {}", transcript.display()))?,
            );
            let transcript = Transcript::read(&mut reader)?;
            let scratch = tempfile::tempdir()?;
            let report = score(scenario, &transcript, scratch.path()).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            anyhow::ensure!(
                report.unauthorized_executions == 0,
                "harness invariant violated: {} venue orders have no authorized dispatch",
                report.unauthorized_executions
            );
            anyhow::ensure!(
                report.passed,
                "transcript failed scenario {}",
                scenario.name()
            );
            Ok(())
        }
    }
}
