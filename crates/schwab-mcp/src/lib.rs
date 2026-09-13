//! Library surface for `schwab-mcp` — consumed by the binary (`main.rs`)
//! and integration tests.

pub mod authority;
pub mod contract;
pub mod identity;
pub mod metrics;
pub mod network;
pub mod paper_replay;
pub mod policy;
pub mod preflight;
pub mod query;
pub mod redact;
pub mod server;
pub mod status;
pub mod stream_cache;

pub use server::SchwabMcp;
