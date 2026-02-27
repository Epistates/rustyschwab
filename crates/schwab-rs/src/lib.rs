#![warn(missing_docs)]
#![doc = include_str!("../../../README.md")]

/// Library version string
///
/// This constant contains the version of the schwab-rs library in the format
/// "schwab-rs X.Y.Z" matching the Cargo.toml version.
///
/// # Example
/// ```
/// use schwab_rs::VERSION;
/// println!("Using {}", VERSION);
/// ```
pub const VERSION: &str = concat!("schwab-rs ", env!("CARGO_PKG_VERSION"));

/// OAuth2 authentication and token management with PKCE and automatic refresh
pub mod auth;

/// Main HTTP client for accessing the Schwab API
pub mod client;

/// Configuration management for the SDK with security validation
pub mod config;

/// Error types and result handling
pub mod error;

/// Retry policy with exponential backoff for transient failures
pub mod retry;

/// Circuit breaker pattern for cascading failure prevention
pub mod circuit_breaker;

/// Telemetry and observability initialization
pub mod telemetry;

/// Cryptographic utilities: encryption, key derivation, secure storage
pub mod security;

/// WebSocket streaming client for real-time market data
pub mod streaming;

/// Low-level transport layer (HTTP and WebSocket)
pub mod transport;

/// Utility functions for formatting and conversions
pub mod utils;

/// API endpoint implementations
pub mod endpoints {
    /// Account information and account management endpoints
    pub mod accounts;

    /// Instrument lookup endpoints
    pub mod instruments;

    /// Market data and quote endpoints
    pub mod market_data;

    /// Market movers endpoints
    pub mod movers;

    /// Options chain endpoints
    pub mod options;

    /// Historical price data endpoints
    pub mod price_history;

    /// Quote endpoints
    pub mod quotes;

    /// Order placement and management endpoints
    pub mod trading;
}

pub use auth::{AuthManager, AuthResult, OAuthConfig, TokenSet};
pub use client::{SchwabClient, SchwabClientBuilder};
pub use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitState};
pub use config::{ChannelKind, ClientConfig, SchwabConfig, StreamConfig};
pub use error::{Error, Result};
pub use streaming::{MessageReceiver, StreamClient, StreamMessage, Subscription};
pub use utils::{format_list, format_list_str, format_time, TimeFormat};

pub use schwab_types as types;

/// Convenient re-exports for common Schwab SDK usage.
///
/// Use `use schwab_rs::prelude::*` to import all commonly needed types.
pub mod prelude {
    pub use crate::auth::{AuthManager, OAuthConfig};
    pub use crate::client::{SchwabClient, SchwabClientBuilder};
    pub use crate::config::SchwabConfig;
    pub use crate::error::{Error, Result};
    pub use crate::streaming::{StreamClient, StreamMessage};
    pub use crate::types::*;
}