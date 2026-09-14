//! Isolated, durable long-equity paper execution and deterministic replay.
//!
//! `PaperBroker` owns no network transport or OAuth credentials. Market input
//! is explicit. Quotes are observations with finite per-event liquidity, not
//! exchange queue priority or a promise of actual broker fills.
mod engine;
mod model;
pub use engine::{PAPER_NOT_FOUND_TITLE, PAPER_REJECTION_TITLE, PaperBroker};
pub use model::*;

pub mod feed;
