//! Library surface for `schwab-mcp` — consumed by the binary (`main.rs`)
//! and integration tests.

pub mod authority;
pub mod backend;
pub mod contract;
pub mod eval;
pub mod guidance;
pub mod identity;
pub mod journal;
pub mod metrics;
pub mod network;
pub mod order_status;
pub mod paper_replay;
pub mod policy;
pub mod portfolio;
pub mod preflight;
pub mod projection;
pub mod query;
pub mod redact;
pub mod server;
pub mod status;
pub mod stream_cache;

pub use server::SchwabMcp;
