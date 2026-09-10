//! Error types and handling for the Schwab SDK.
//!
//! All SDK operations return `Result<T>` which is an alias for `std::result::Result<T, Error>`.
//! Each error variant includes recovery guidance for handling different failure scenarios.
//!
//! # Error Recovery Strategies
//!
//! ## Transient Errors (Retryable)
//! - [`Error::RateLimit`] - Wait and retry with exponential backoff
//! - [`Error::Timeout`] - Retry with longer timeout
//! - [`Error::ConnectionClosed`] - Reconnect using built-in reconnect logic
//! - HTTP 5xx errors - Retry with backoff (automatically handled by SDK)
//!
//! ## Permanent Errors (Don't Retry)
//! - [`Error::Auth`] - Re-authenticate with fresh OAuth flow
//! - [`Error::InvalidParameter`] - Fix parameter values
//! - [`Error::Config`] - Validate configuration settings
//! - HTTP 4xx errors (except 429) - Fix request parameters
//!
//! # Example
//!
//! ```ignore
//! use schwab_rs::Result;
//!
//! async fn call_api() -> Result<String> {
//!     match make_request().await {
//!         Ok(result) => Ok(result),
//!         Err(e) => match e {
//!             // Rate limit: wait and retry
//!             Error::RateLimit { retry_after } => {
//!                 tokio::time::sleep(Duration::from_secs(retry_after)).await;
//!                 make_request().await
//!             }
//!             // Configuration error: fix config and retry
//!             Error::Config(msg) => {
//!                 eprintln!("Config error: {}", msg);
//!                 Err(e)
//!             }
//!             // Other errors: propagate
//!             _ => Err(e),
//!         }
//!     }
//! }
//! ```

use std::fmt;
use thiserror::Error;

/// Standard result type for SDK operations.
///
/// All SDK functions return `Result<T>` which is either:
/// - `Ok(T)` on success
/// - `Err(Error)` on failure
pub type Result<T> = std::result::Result<T, Error>;

/// Comprehensive error type for all SDK operations.
///
/// Each variant includes context about what failed and recovery hints.
#[derive(Error, Debug)]
pub enum Error {
    #[error("Durable state failure: {0}")]
    Storage(String),
    #[error("Ownership lease lost; reconcile in-flight operations")]
    OwnershipLost,
    #[error("Circuit breaker is open")]
    CircuitOpen,
    #[error("Reconciliation is required for pending submission {attempt_id}")]
    ReconciliationRequired { attempt_id: String },
    /// A submission may have reached the broker. Reconcile before resubmitting.
    #[error(
        "Order submission outcome is unknown; reconcile before resubmitting (attempt {attempt_id})"
    )]
    SubmissionUnknown {
        attempt_id: String,
        #[source]
        source: Box<Error>,
    },
    /// Structured upstream error with status preserved. Details are available to
    /// trusted SDK callers but deliberately omitted from Display/log output.
    #[error("Broker HTTP error {status}")]
    Broker {
        status: reqwest::StatusCode,
        details: Vec<ApiError>,
        request_id: Option<String>,
    },

    /// Authentication-related error (OAuth flow, token refresh, credentials)
    ///
    /// **Recovery**: Initiate a new OAuth2 authentication flow by calling `AuthManager::authorize()`
    #[error("Authentication error: {0}")]
    Auth(#[from] AuthError),

    /// HTTP request failed with non-2xx status code
    ///
    /// **Recovery**:
    /// - 4xx: Fix the request (invalid parameters, unauthorized, etc.)
    /// - 5xx: Retry with exponential backoff (SDK handles automatically)
    /// - 429: Rate limited - wait and retry (SDK handles automatically)
    #[error("HTTP request failed with status {status}: {message}")]
    Http {
        /// HTTP status code (e.g., 401, 404, 500)
        status: reqwest::StatusCode,
        /// Response body or error message
        message: String,
    },

    /// WebSocket connection or streaming error
    ///
    /// **Recovery**: Reconnect using StreamClient's automatic reconnection (if enabled)
    #[error("WebSocket error: {0}")]
    WebSocket(#[from] Box<tokio_tungstenite::tungstenite::Error>),

    /// JSON serialization/deserialization error
    ///
    /// **Recovery**: Check API response format - may indicate API version mismatch
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// URL parsing error (invalid configuration URL)
    ///
    /// **Recovery**: Validate URL format in configuration
    #[error("URL parse error: {0}")]
    UrlParse(#[from] url::ParseError),

    /// Rate limit exceeded - too many requests
    ///
    /// **Recovery**: Wait `retry_after` seconds, then retry. SDK handles this automatically.
    #[error("Rate limit exceeded: retry after {retry_after} seconds")]
    RateLimit { retry_after: u64 },

    /// API-specific error returned by Schwab servers
    ///
    /// **Recovery**: Check the error code and message for specific guidance from Schwab API documentation
    #[error("API error [{code}]: {message}")]
    Api {
        /// Schwab API error code
        code: String,
        /// Error description from API
        message: String,
    },

    /// Configuration validation failed (invalid URLs, missing credentials, etc.)
    ///
    /// **Recovery**: Check SchwabConfig for invalid values. Common issues:
    /// - Invalid API domain (must be *.schwabapi.com or localhost)
    /// - Missing required credentials
    /// - Invalid URL format
    #[error("Configuration error: {0}")]
    Config(String),

    /// Network error (connection refused, timeout, DNS failure, etc.)
    ///
    /// **Recovery**: Check network connectivity and retry
    #[error("Network error: {0}")]
    Network(#[from] Box<reqwest::Error>),

    /// Invalid parameter passed to SDK function
    ///
    /// **Recovery**: Check function signature and parameter values
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),

    /// Request timed out after specified duration
    ///
    /// **Recovery**: Retry with longer timeout or check server status
    #[error("Timeout occurred after {duration} seconds")]
    Timeout { duration: u64 },

    /// WebSocket connection unexpectedly closed
    ///
    /// **Recovery**: Use StreamClient's automatic reconnection feature
    #[error("Connection closed unexpectedly")]
    ConnectionClosed,

    /// Error managing subscriptions on streaming connection
    ///
    /// **Recovery**: Check subscription parameters and reconnect
    #[error("Subscription error: {0}")]
    Subscription(String),

    /// General streaming error
    ///
    /// **Recovery**: Check StreamClient status and reconnect if needed
    #[error("Stream error: {0}")]
    Stream(#[from] StreamError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Unknown error: {0}")]
    Unknown(String),
}

// Manual From impls so callers using `?` don't need to box themselves.
// The variants are boxed to keep `Error` small (Result<T, Error> stays cheap).
impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Error::Network(Box::new(err))
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for Error {
    fn from(err: tokio_tungstenite::tungstenite::Error) -> Self {
        Error::WebSocket(Box::new(err))
    }
}

#[derive(Error, Debug)]
pub enum AuthError {
    #[error("Token expired")]
    TokenExpired,

    /// Refresh token has less runway than our safety threshold (Schwabdev's
    /// 60.5 min). Callers must run a fresh OAuth flow rather than attempting
    /// to refresh — Schwab does not renew the refresh token itself; it is
    /// only issued during the initial auth-code exchange.
    #[error(
        "Refresh token expiring soon ({seconds_remaining}s remaining) — full OAuth flow required"
    )]
    RefreshTokenExpiring { seconds_remaining: i64 },

    #[error("OAuth flow failed: {0}")]
    OAuthFlow(String),

    #[error("Invalid credentials")]
    InvalidCredentials,

    #[error("Token storage error: {0}")]
    TokenStorage(#[from] std::io::Error),

    #[error("Missing refresh token")]
    MissingRefreshToken,

    #[error("Authorization denied by user")]
    AuthorizationDenied,

    #[error("Invalid callback URL: {0}")]
    InvalidCallbackUrl(String),

    #[error("Token refresh failed: {0}")]
    RefreshFailed(String),

    #[error("Missing required configuration: {0}")]
    MissingConfig(String),

    #[error("Encryption failed: {0}")]
    EncryptionFailed(String),

    #[error("Decryption failed: {0}")]
    DecryptionFailed(String),

    #[error("Token file error: {0}")]
    TokenFileError(String),

    #[error("Token file has insecure permissions: {0}")]
    TokenFileInsecure(String),

    #[error("Keyring error: {0}")]
    KeyringError(String),
}

#[derive(Error, Debug)]
pub enum StreamError {
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    #[error("Authentication failed: {0}")]
    AuthenticationFailed(String),

    #[error("Subscription failed: {service} - {code}: {message}")]
    SubscriptionFailed {
        service: String,
        code: i32,
        message: String,
    },

    #[error("Invalid message format: {0}")]
    InvalidMessage(String),

    #[error("Service not available: {0}")]
    ServiceNotAvailable(String),

    #[error("Symbol limit reached: {limit}")]
    SymbolLimitReached { limit: usize },

    #[error("Connection limit reached")]
    ConnectionLimitReached,

    #[error("Heartbeat timeout")]
    HeartbeatTimeout,

    #[error("Protocol error: {0}")]
    Protocol(String),
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ApiError {
    pub id: Option<String>,
    pub status: Option<String>,
    pub title: Option<String>,
    pub detail: Option<String>,
    pub source: Option<ErrorSource>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ErrorSource {
    pub pointer: Option<Vec<String>>,
    pub parameter: Option<String>,
    pub header: Option<String>,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(title) = &self.title {
            write!(f, "{}", title)?;
        }
        if let Some(detail) = &self.detail {
            write!(f, ": {}", detail)?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

impl Error {
    pub fn http_status(&self) -> Option<reqwest::StatusCode> {
        match self {
            Self::Http { status, .. } | Self::Broker { status, .. } => Some(*status),
            Self::RateLimit { .. } => Some(reqwest::StatusCode::TOO_MANY_REQUESTS),
            _ => None,
        }
    }
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Network(_)
            | Error::Timeout { .. }
            | Error::RateLimit { .. }
            | Error::ConnectionClosed => true,
            Error::Http { status, .. } | Error::Broker { status, .. } if status.as_u16() >= 500 => {
                true
            }
            _ => false,
        }
    }

    pub fn is_auth_error(&self) -> bool {
        match self {
            Error::Auth(_) => true,
            Error::Http { status, .. } | Error::Broker { status, .. }
                if *status == reqwest::StatusCode::UNAUTHORIZED =>
            {
                true
            }
            _ => false,
        }
    }

    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            Error::RateLimit { retry_after } => Some(std::time::Duration::from_secs(*retry_after)),
            _ => None,
        }
    }
}

pub fn parse_api_error(status: reqwest::StatusCode, body: &str) -> Error {
    if let Ok(api_errors) = serde_json::from_str::<ApiErrorResponse>(body) {
        return Error::Broker {
            status,
            details: api_errors.errors,
            request_id: None,
        };
    }

    Error::Http {
        status,
        message: body.to_string(),
    }
}

#[derive(Debug, serde::Deserialize)]
struct ApiErrorResponse {
    errors: Vec<ApiError>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_is_retryable() {
        // Timeout errors should be retryable
        assert!(Error::Timeout { duration: 30 }.is_retryable());

        // Rate limit errors should be retryable
        assert!(Error::RateLimit { retry_after: 60 }.is_retryable());

        // ConnectionClosed should be retryable
        assert!(Error::ConnectionClosed.is_retryable());

        // InvalidParameter should NOT be retryable
        assert!(!Error::InvalidParameter("test".to_string()).is_retryable());
    }

    #[test]
    fn test_error_is_auth_error() {
        // Auth errors should be identified
        assert!(Error::Auth(AuthError::TokenExpired).is_auth_error());

        // 401 Unauthorized HTTP status should be auth error
        assert!(
            Error::Http {
                status: reqwest::StatusCode::UNAUTHORIZED,
                message: "Unauthorized".to_string()
            }
            .is_auth_error()
        );

        // Timeout is NOT an auth error
        assert!(!Error::Timeout { duration: 30 }.is_auth_error());

        // ConnectionClosed is NOT an auth error
        assert!(!Error::ConnectionClosed.is_auth_error());
    }
}
