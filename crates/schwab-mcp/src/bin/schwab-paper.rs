//! Deterministic replay or an explicitly selected read-only Schwab market feed.
use clap::Parser;
use schwab_rs::paper::{PaperBroker, PaperCommand, PaperConfig};
use std::{io::Write, path::PathBuf};
#[derive(Parser)]
#[command(
    about = "Isolated paper trading: offline JSONL replay or an opt-in read-only Schwab feed"
)]
struct Args {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    state: PathBuf,
    #[arg(
        long,
        required_unless_present = "live_feed",
        conflicts_with = "live_feed"
    )]
    commands: Option<PathBuf>,
    #[arg(long, requires_all=["symbols","quote_size_multiplier"])]
    live_feed: bool,
    #[arg(long, value_delimiter = ',', requires = "live_feed")]
    symbols: Vec<String>,
    #[arg(long, requires = "live_feed")]
    quote_size_multiplier: Option<rust_decimal::Decimal>,
    #[arg(long, default_value_t = 2)]
    poll_seconds: u64,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config: PaperConfig = serde_json::from_slice(&std::fs::read(args.config)?)?;
    let broker = PaperBroker::open(args.state, config).await?;
    if args.live_feed {
        anyhow::ensure!(
            (1..=60).contains(&args.poll_seconds),
            "Poll interval must be 1..=60 seconds"
        );
        let client = std::sync::Arc::new(schwab_rs::SchwabClient::new(
            schwab_rs::SchwabConfig::from_env()?,
        )?);
        let feed = schwab_rs::paper::feed::SchwabPaperFeed::new(
            client.clone(),
            broker.clone(),
            args.symbols,
            args.quote_size_multiplier
                .ok_or_else(|| anyhow::anyhow!("Quote size multiplier required"))?,
        )?;
        client.init().await?;
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(args.poll_seconds));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _=tokio::signal::ctrl_c()=>{broker.begin_drain();break;}
                _=interval.tick()=>{
                    if let Err(error)=feed.tick().await {eprintln!("paper feed suspended: {error}");}
                }
            }
        }
        return Ok(());
    }
    let mut reader = std::io::BufReader::new(std::fs::File::open(
        args.commands
            .ok_or_else(|| anyhow::anyhow!("Commands file required"))?,
    )?);
    let mut output = std::io::BufWriter::new(std::io::stdout());
    while let Some(command) = schwab_mcp::paper_replay::read_record::<PaperCommand>(&mut reader)? {
        let result = schwab_mcp::paper_replay::execute(&broker, command).await?;
        serde_json::to_writer(&mut output, &result)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}
