//! Deterministic replay, an explicitly selected read-only Schwab market feed,
//! or a synthetic quote stream generated from price history.
use clap::Parser;
use rustyschwab::paper::{PaperBroker, PaperCommand, PaperConfig, synthetic::SyntheticBbo};
use std::{io::Write, path::PathBuf};
#[derive(Parser)]
#[command(
    about = "Isolated paper trading: offline JSONL replay, an opt-in read-only Schwab feed, or synthetic quotes from candles"
)]
struct Args {
    #[arg(long, required_unless_present = "synthesize")]
    config: Option<PathBuf>,
    #[arg(long, required_unless_present = "synthesize")]
    state: Option<PathBuf>,
    #[arg(
        long,
        required_unless_present_any = ["live_feed", "synthesize"],
        conflicts_with_all = ["live_feed", "synthesize"]
    )]
    commands: Option<PathBuf>,
    /// Milliseconds to wait between replayed commands, so a venue served by
    /// the MCP server or the authority advances in paced virtual time.
    #[arg(long, default_value_t = 0, requires = "commands")]
    pace_ms: u64,
    #[arg(long, requires_all=["symbols","quote_size_multiplier"])]
    live_feed: bool,
    #[arg(long, value_delimiter = ',', requires = "live_feed")]
    symbols: Vec<String>,
    #[arg(long, requires = "live_feed")]
    quote_size_multiplier: Option<rust_decimal::Decimal>,
    #[arg(long, default_value_t = 2)]
    poll_seconds: u64,
    /// Price history files, one `PriceHistoryResponse` JSON per symbol, to
    /// turn into `event` commands on stdout under the `candle-bbo-v1` model.
    /// Nothing is opened or written: replay the output with `--commands`.
    #[arg(
        long,
        num_args = 1..,
        requires_all = ["candle_ms", "spread_bps", "participation_bps"],
        conflicts_with = "live_feed"
    )]
    synthesize: Vec<PathBuf>,
    #[arg(long, requires = "synthesize")]
    candle_ms: Option<i64>,
    #[arg(long, requires = "synthesize")]
    spread_bps: Option<rust_decimal::Decimal>,
    #[arg(long, requires = "synthesize")]
    participation_bps: Option<rust_decimal::Decimal>,
    #[arg(long, default_value_t = 1, requires = "synthesize")]
    first_sequence: u64,
    /// Keep pre- and post-market candles too; by default only candles in
    /// New York's regular session become quotes.
    #[arg(long, requires = "synthesize")]
    extended_hours: bool,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut output = std::io::BufWriter::new(std::io::stdout());
    if !args.synthesize.is_empty() {
        let required = |what: &str| anyhow::anyhow!("{what} required to synthesize quotes");
        let model = SyntheticBbo {
            candle_ms: args.candle_ms.ok_or_else(|| required("Candle width"))?,
            spread_bps: args.spread_bps.ok_or_else(|| required("Spread"))?,
            participation_bps: args
                .participation_bps
                .ok_or_else(|| required("Participation"))?,
            regular_hours_only: !args.extended_hours,
        };
        let histories = args
            .synthesize
            .iter()
            .map(|path| Ok(serde_json::from_slice(&std::fs::read(path)?)?))
            .collect::<anyhow::Result<Vec<_>>>()?;
        for event in model.events(&histories, args.first_sequence)? {
            serde_json::to_writer(&mut output, &PaperCommand::Event { event })?;
            output.write_all(b"\n")?;
        }
        output.flush()?;
        return Ok(());
    }
    let config: PaperConfig = serde_json::from_slice(&std::fs::read(
        args.config
            .ok_or_else(|| anyhow::anyhow!("Configuration file required"))?,
    )?)?;
    let broker = PaperBroker::open(
        args.state
            .ok_or_else(|| anyhow::anyhow!("State path required"))?,
        config,
    )
    .await?;
    if args.live_feed {
        anyhow::ensure!(
            (1..=60).contains(&args.poll_seconds),
            "Poll interval must be 1..=60 seconds"
        );
        let client = std::sync::Arc::new(rustyschwab::SchwabClient::new(
            rustyschwab::SchwabConfig::from_env()?,
        )?);
        let feed = rustyschwab::paper::feed::SchwabPaperFeed::new(
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
                _ = tokio::signal::ctrl_c() => {
                    broker.begin_drain();
                    break;
                }
                _ = interval.tick() => {
                    if let Err(error) = feed.tick().await {
                        eprintln!("paper feed suspended: {error}");
                    }
                }
            }
        }
        return Ok(());
    }
    let mut reader = std::io::BufReader::new(std::fs::File::open(
        args.commands
            .ok_or_else(|| anyhow::anyhow!("Commands file required"))?,
    )?);
    let pace = std::time::Duration::from_millis(args.pace_ms);
    let mut first = true;
    while let Some(command) =
        rustyschwab_mcp::paper_replay::read_record::<PaperCommand>(&mut reader)?
    {
        if !std::mem::take(&mut first) && !pace.is_zero() {
            tokio::time::sleep(pace).await;
        }
        let result = rustyschwab_mcp::paper_replay::execute(&broker, command).await?;
        serde_json::to_writer(&mut output, &result)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}
