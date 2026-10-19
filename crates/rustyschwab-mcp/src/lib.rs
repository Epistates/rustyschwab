//! Library surface for `rustyschwab-mcp` — consumed by the binary (`main.rs`)
//! and integration tests.

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod approvals;
pub mod authority;
pub mod backend;
pub mod contract;
pub mod dead_man;
pub mod eval;
pub mod guardrails;
pub mod guidance;
pub mod identity;
pub mod journal;
pub mod metrics;
pub mod network;
pub mod order_status;
pub mod paper_replay;
pub mod performance;
pub mod policy;
pub mod portfolio;
pub mod preflight;
pub mod projection;
pub mod query;
pub mod redact;
pub mod server;
pub mod status;
pub mod stream_cache;
#[doc(hidden)]
pub mod testing;

pub use server::SchwabMcp;

/// The largest authorization file (policy, grants, identity, risk or paper
/// configuration) the server reads.
pub const CONFIG_FILE_LIMIT: u64 = 1024 * 1024;

/// Read an authorization file through [`rustyschwab::security::read_trusted_file`]:
/// on Unix it must be owned by this account or root and writable by no one
/// else, since whoever can edit it decides who may trade.
pub fn read_config(path: impl AsRef<std::path::Path>) -> anyhow::Result<Vec<u8>> {
    let path = path.as_ref();
    rustyschwab::security::read_trusted_file(path, CONFIG_FILE_LIMIT)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}
