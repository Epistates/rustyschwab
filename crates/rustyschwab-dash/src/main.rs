//! `rustyschwab-dash`: follow and supervise agents trading through
//! `rustyschwab-mcp`, from a terminal.
//!
//! It is a frontend over `rustyschwab-console`: it renders the console's
//! snapshots and sends it the operator's commands, nothing else.

mod app;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use crossterm::event::{Event, EventStream};
use futures_util::StreamExt;
use rustyschwab_console::{Console, ConsoleConfig};

/// Follow and supervise agents trading through rustyschwab-mcp. Reads the
/// same environment (and `.env`) as the server.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Who commands from this dashboard, as approvals and halts record it
    /// [default: SCHWAB_OPERATOR, else the login user].
    #[arg(long)]
    operator: Option<String>,
    /// Extra symbols to quote, comma-separated [env: SCHWAB_DASH_WATCHLIST].
    #[arg(long, value_delimiter = ',')]
    watch: Vec<String>,
    /// Append the dashboard's own log here; the terminal belongs to the UI.
    #[arg(long, default_value = "rustyschwab-dash.log")]
    log: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&args.log)
        .with_context(|| format!("cannot open the log {}", args.log.display()))?;
    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(log))
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mut config = ConsoleConfig::from_env()?;
    if let Some(operator) = args.operator {
        config.operator = operator;
    }
    config.watchlist.extend(
        args.watch
            .iter()
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty()),
    );
    let console = Console::open(config)
        .await
        .context("could not open the state the servers keep")?;
    let handle = console.spawn();

    let terminal = ratatui::init();
    let result = run(terminal, handle).await;
    ratatui::restore();
    result
}

async fn run(
    mut terminal: ratatui::DefaultTerminal,
    handle: rustyschwab_console::Handle,
) -> anyhow::Result<()> {
    let mut app = app::App::new(handle.snapshot());
    let mut snapshots = handle.subscribe();
    let mut events = EventStream::new();
    // Clocks and countdowns move between snapshots.
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let (done, mut finished) = tokio::sync::mpsc::channel(4);
    while !app.quit {
        terminal.draw(|frame| ui::render(frame, &app))?;
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) if key.is_press() => {
                    if let Some(command) = app.on_key(key) {
                        if app.busy {
                            app.finished(Err("Still running the last command".into()));
                            continue;
                        }
                        app.busy = true;
                        let handle = handle.clone();
                        let done = done.clone();
                        tokio::spawn(async move {
                            let _ = done.send(handle.execute(command).await).await;
                        });
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            changed = snapshots.changed() => {
                if changed.is_err() {
                    anyhow::bail!("the console stopped");
                }
                let snapshot: Arc<_> = snapshots.borrow_and_update().clone();
                app.update(snapshot);
            }
            Some(outcome) = finished.recv() => app.finished(outcome),
            _ = tick.tick() => {}
        }
    }
    Ok(())
}
