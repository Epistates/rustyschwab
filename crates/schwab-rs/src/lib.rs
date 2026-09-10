#![doc = include_str!("../../../README.md")]

/// Library version string in the format `schwab-rs X.Y.Z`.
///
/// # Example
/// ```
/// use schwab_rs::VERSION;
/// println!("Using {}", VERSION);
/// ```
pub const VERSION: &str = concat!("schwab-rs ", env!("CARGO_PKG_VERSION"));

pub mod audit_delivery;
pub mod auth;
pub mod broker;
pub mod circuit_breaker;
pub mod client;
pub mod config;
pub mod coordination;
pub mod error;
pub mod execution;
pub mod metrics;
pub mod paper;
pub mod readiness;
pub mod reconciliation;
pub mod recovery;
pub mod remote;
mod request_budget;
pub mod retry;
pub mod risk;
pub mod security;
#[cfg(windows)]
mod security_windows;
pub mod storage;
pub mod streaming;
pub mod telemetry;
pub mod transport;
pub mod utils;

pub use auth::{AuthManager, AuthResult, OAuthConfig, TokenSet};
pub use circuit_breaker::{CircuitBreaker, CircuitState};
pub use client::{SchwabClient, SchwabClientBuilder};
pub use config::{ChannelKind, CircuitBreakerConfig, ClientConfig, SchwabConfig, StreamConfig};
pub use error::{Error, Result};
pub use streaming::{MessageReceiver, StreamClient, StreamMessage, Subscription};
pub use utils::{TimeFormat, format_list, format_list_str, format_time};

pub use schwab_types as types;

/// Convenient re-exports for common Schwab SDK usage.
///
/// Use `use schwab_rs::prelude::*` to import the most commonly needed types.
pub mod prelude {
    pub use crate::auth::{AuthManager, OAuthConfig};
    pub use crate::client::{SchwabClient, SchwabClientBuilder};
    pub use crate::config::SchwabConfig;
    pub use crate::error::{Error, Result};
    pub use crate::streaming::{StreamClient, StreamMessage};
    pub use crate::types::*;
}
