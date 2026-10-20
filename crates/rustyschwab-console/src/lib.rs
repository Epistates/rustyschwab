//! The operator's console for agents trading through `rustyschwab-mcp`,
//! independent of any user interface.
//!
//! A [`Console`] reads the state the MCP server keeps (each principal's
//! journal and activity log, the venue's account baselines and orders, the
//! operator halt) and builds a [`Snapshot`]: plain, serializable data that
//! a terminal UI renders (`rustyschwab-dash`) and a web frontend could send
//! as JSON. It also carries out the operator's [`Command`]s: approve or
//! refuse an order, delegate or withdraw authority, halt, resume, kill.
//!
//! It runs as its own process beside the servers and needs no network
//! surface: it opens the same files, with the same trust as
//! `rustyschwab-admin`. Watching sends nothing to Schwab, except quotes in
//! live mode (one request every few seconds, from the shared budget).
//!
//! ```no_run
//! # async fn run() -> anyhow::Result<()> {
//! use rustyschwab_console::{Console, ConsoleConfig};
//! let handle = Console::open(ConsoleConfig::from_env()?).await?.spawn();
//! let mut snapshots = handle.subscribe();
//! while snapshots.changed().await.is_ok() {
//!     let snapshot = snapshots.borrow().clone();
//!     println!("{}", serde_json::to_string(&*snapshot)?);
//! }
//! # Ok(()) }
//! ```

mod config;
mod console;
mod read;
pub mod snapshot;

pub use config::{ConsoleConfig, PaperFiles};
pub use console::{
    ACTIVITY_SHOWN, APPROVAL_MINUTES, Command, Console, Handle, describe_mode, unapproved_reach,
};
pub use snapshot::Snapshot;
