//! Library surface for `schwab-mcp` — consumed by the binary (`main.rs`)
//! and integration tests.

pub mod metrics;
pub mod redact;
pub mod server;
pub mod stream_cache;

pub use server::SchwabMcp;
