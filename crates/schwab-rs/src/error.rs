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
//! - HTTP 5xx errors ([`Error::Http`], [`Error::Broker`], [`Error::Remote`]) -
//!   Retry with backoff (automatically handled by SDK)
//!
//! ## Permanent Errors (Don't Retry)
//! - [`Error::Auth`] - Re-authenticate with fresh OAuth flow
//! - [`Error::InvalidParameter`] - Fix parameter values
//! - [`Error::Config`] - Validate configuration settings
//! - HTTP 4xx errors (except 429) - Fix request parameters
//!
//! ## Trading Control Errors (Resolve the condition, then submit a new intent)
//! - [`Error::Halted`] - An operator halt is active; wait for the operator
//!   to lift it
//! - [`Error::Suspended`] - Data recovery or a process drain paused
//!   submissions; nothing was sent, so retry shortly
//! - [`Error::RiskDenied`] - A risk policy rejected the order; adjust size or
//!   wait for exposure to fall
//! - [`Error::BaselineStale`] - Reconcile the account, then resubmit
//! - [`Error::Duplicate`] - The intent, tag or approval was already used;
//!   never resubmit blindly
//! - [`Error::ApprovalInvalid`] - Obtain a fresh operator approval
//! - [`Error::ReconciliationRequired`] / [`Error::SubmissionUnknown`] -
//!   Reconcile the pending attempt before submitting again
//! - [`Error::OwnershipLost`] - Another writer holds the execution lease
//! - [`Error::LeaseBusy`] - Another operation held the lease for the whole
//!   wait; nothing was sent, so the same intent can be submitted again
//!
//! # Portability
//!
//! [`Error::kind`] gives a stable, serializable [`ErrorKind`] for logging,
//! metrics and policy decisions. [`ErrorEnvelope`] carries an error across a
//! process boundary (the remote authority uses it) so the typed variants above
//! survive the hop instead of collapsing into an opaque status code.
//!
//! # Example
//!
//! ```no_run
//! use schwab_rs::{Error, Result};
//! use std::{future::Future, time::Duration};
//!
//! /// Honour one server-directed wait, then give up.
//! async fn with_rate_limit_retry<T, F, Fut>(mut call: F) -> Result<T>
//! where
//!     F: FnMut() -> Fut,
//!     Fut: Future<Output = Result<T>>,
//! {
//!     match call().await {
//!         // Rate limit: wait as told and try once more
//!         Err(Error::RateLimit { retry_after }) => {
//!             tokio::time::sleep(Duration::from_secs(retry_after)).await;
//!             call().await
//!         }
//!         // Configuration errors are the caller's to fix
//!         Err(Error::Config(msg)) => {
//!             eprintln!("Config error: {msg}");
//!             Err(Error::Config(msg))
//!         }
//!         // Everything else propagates
//!         other => other,
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
/// The enum is `#[non_exhaustive]`: match on the variants you handle and
/// fall through on the rest, or classify with [`Error::kind`].
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// Durable state (SQLite journal, ledger, reservation) failed or is
    /// inconsistent. Not a caller error; check the database and its backups.
    #[error("Durable state failure: {0}")]
    Storage(String),
    #[error("Ownership lease lost; reconcile in-flight operations")]
    OwnershipLost,
    /// Another operation holds the lease right now; nothing was attempted.
    /// The resource is not named: lease names embed the app key.
    #[error("Another operation holds the lease; nothing was sent")]
    LeaseBusy,
    #[error("Circuit breaker is open")]
    CircuitOpen,
    #[error("Reconciliation is required for pending submission {attempt_id}")]
    ReconciliationRequired { attempt_id: String },
    /// Trading is administratively paused: an operator halt, or one a
    /// control (the daily-loss limit) recorded. Only an operator lifts it.
    #[error("Trading halted: {0}")]
    Halted(String),
    /// Submissions are paused for a moment, not by an operator: required data
    /// is recovering after a stream gap, or the process is draining. Nothing
    /// was sent; retry shortly (on another process, when draining).
    #[error("Submissions suspended: {0}")]
    Suspended(String),
    /// A risk policy (built-in portfolio limits, an external provider, or the
    /// paper venue's capacity limits) refused the order.
    #[error("Risk policy denied the order: {0}")]
    RiskDenied(String),
    /// The reconciled account baseline is missing, expired or unusable, so the
    /// order cannot be sized safely. Reconcile the account and resubmit.
    #[error("Account baseline for {account} is unusable: {reason}")]
    BaselineStale { account: String, reason: String },
    /// An idempotency identity (intent key, order tag, corporate action id,
    /// submission outcome) was already used. Never resubmit blindly.
    #[error("Duplicate request: {0}")]
    Duplicate(String),
    /// The operator approval is expired, revoked or issued to another principal.
    #[error("Approval invalid: {0}")]
    ApprovalInvalid(String),
    /// The remote authority answered with a non-2xx status and no typed
    /// [`ErrorEnvelope`]. `status` is the authority's HTTP status, not Schwab's.
    ///
    /// As with [`Error::Http`], `message` is left out of `Display`: it can be
    /// a relayed Schwab error body or an unrecognised reply, either of which
    /// can echo request data into logs.
    #[error("Remote authority rejected request (HTTP {status})")]
    Remote {
        status: reqwest::StatusCode,
        message: String,
    },
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
    /// **Recovery**: Initiate a new OAuth2 authentication flow by calling
    /// `AuthManager::authorize()`
    #[error("Authentication error: {0}")]
    Auth(#[from] AuthError),

    /// HTTP request failed with non-2xx status code
    ///
    /// **Recovery**:
    /// - 4xx: Fix the request (invalid parameters, unauthorized, etc.)
    /// - 5xx: Retry with exponential backoff (SDK handles automatically)
    /// - 429: Rate limited - wait and retry (SDK handles automatically)
    ///
    /// Like [`Error::Broker`]'s details, the body is kept in `message` for
    /// trusted callers and left out of `Display`: an unrecognised error body
    /// (a proxy page, a new error shape) can echo request data into logs.
    #[error("HTTP request failed with status {status}")]
    Http {
        /// HTTP status code (e.g., 401, 404, 500)
        status: reqwest::StatusCode,
        /// Response body (at most 16 KiB) or error message
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
    /// **Recovery**: Check the error code and message for specific guidance from the
    /// Schwab API documentation
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

    /// A request failed its own validation before anything was sent; the
    /// [`ValidationError`](crate::types::ValidationError) says which field
    /// and why. Its kind is [`ErrorKind::InvalidParameter`].
    #[error("Invalid parameter: {0}")]
    Validation(#[from] crate::types::ValidationError),

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
#[non_exhaustive]
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

    /// Only a new authorization (the browser flow) can restore access: the
    /// refresh token expired, Schwab refused it (revoked, or replaced by a
    /// newer authorization), or no tokens are stored. Retrying will not
    /// help, and the manager does not ask Schwab again until the stored
    /// tokens change.
    #[error("Re-authorization required: {0}")]
    ReauthorizationRequired(String),
}

impl AuthError {
    /// Whether only a new authorization can restore access, as opposed to a
    /// failure that may pass (a network error, a 5xx from the token
    /// endpoint) or one about to need it ([`AuthError::RefreshTokenExpiring`]).
    pub fn requires_reauthorization(&self) -> bool {
        matches!(
            self,
            Self::ReauthorizationRequired(_) | Self::MissingRefreshToken
        )
    }
}

#[derive(Error, Debug)]
#[non_exhaustive]
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

/// One error from a Schwab error body; see
/// [`crate::types::market_data::ApiError`].
pub use crate::types::market_data::{ApiError, ErrorSource};

impl Error {
    /// Stable classification of this error, independent of its payload.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Storage(_) => ErrorKind::Storage,
            Self::OwnershipLost => ErrorKind::OwnershipLost,
            Self::LeaseBusy => ErrorKind::LeaseBusy,
            Self::CircuitOpen => ErrorKind::CircuitOpen,
            Self::ReconciliationRequired { .. } => ErrorKind::ReconciliationRequired,
            Self::Halted(_) => ErrorKind::Halted,
            Self::Suspended(_) => ErrorKind::Suspended,
            Self::RiskDenied(_) => ErrorKind::RiskDenied,
            Self::BaselineStale { .. } => ErrorKind::BaselineStale,
            Self::Duplicate(_) => ErrorKind::Duplicate,
            Self::ApprovalInvalid(_) => ErrorKind::ApprovalInvalid,
            Self::Remote { .. } => ErrorKind::Remote,
            Self::SubmissionUnknown { .. } => ErrorKind::SubmissionUnknown,
            Self::Broker { .. } => ErrorKind::Broker,
            Self::Auth(_) => ErrorKind::Auth,
            Self::Http { .. } => ErrorKind::Http,
            Self::WebSocket(_) => ErrorKind::WebSocket,
            Self::Serialization(_) => ErrorKind::Serialization,
            Self::UrlParse(_) => ErrorKind::UrlParse,
            Self::RateLimit { .. } => ErrorKind::RateLimit,
            Self::Api { .. } => ErrorKind::Api,
            Self::Config(_) => ErrorKind::Config,
            Self::Network(_) => ErrorKind::Network,
            Self::InvalidParameter(_) | Self::Validation(_) => ErrorKind::InvalidParameter,
            Self::Timeout { .. } => ErrorKind::Timeout,
            Self::ConnectionClosed => ErrorKind::ConnectionClosed,
            Self::Subscription(_) => ErrorKind::Subscription,
            Self::Stream(_) => ErrorKind::Stream,
            Self::Io(_) => ErrorKind::Io,
            Self::Unknown(_) => ErrorKind::Unknown,
        }
    }

    /// Upstream (Schwab) HTTP status. [`Error::Remote`] is deliberately
    /// excluded: an authority's 401 must not trigger a broker token refresh.
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
            Error::Http { status, .. }
            | Error::Broker { status, .. }
            | Error::Remote { status, .. }
                if status.as_u16() >= 500 =>
            {
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

/// Parse a Schwab error body. Two shapes are documented: the market data
/// API's `{"errors":[{"status","title","detail",...}]}` and the trader
/// API's `{"message":"...","errors":["..."]}`; either becomes
/// [`Error::Broker`], so the reason reaches the caller. Anything else is
/// [`Error::Http`] carrying the body.
pub fn parse_api_error(status: reqwest::StatusCode, body: &str) -> Error {
    if let Ok(api_errors) = serde_json::from_str::<ApiErrorResponse>(body)
        && !api_errors.errors.is_empty()
    {
        return Error::Broker {
            status,
            details: api_errors.errors,
            request_id: None,
        };
    }
    if let Ok(trader) = serde_json::from_str::<TraderErrorResponse>(body)
        && (trader.message.is_some() || !trader.errors.is_empty())
    {
        let title = trader
            .message
            .unwrap_or_else(|| status.canonical_reason().unwrap_or("Error").to_owned());
        let detail = (!trader.errors.is_empty()).then(|| trader.errors.join("; "));
        return Error::Broker {
            status,
            details: vec![ApiError::new(
                Some(status.as_u16().to_string()),
                title,
                detail,
            )],
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

/// The trader API's error body.
#[derive(Debug, serde::Deserialize)]
struct TraderErrorResponse {
    message: Option<String>,
    #[serde(default)]
    errors: Vec<String>,
}

/// Stable classification of an [`enum@Error`], independent of its payload.
///
/// Serializes as `snake_case`. New kinds may appear in minor releases, so
/// match with a fallback arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorKind {
    Storage,
    OwnershipLost,
    LeaseBusy,
    CircuitOpen,
    ReconciliationRequired,
    Halted,
    Suspended,
    RiskDenied,
    BaselineStale,
    Duplicate,
    ApprovalInvalid,
    Remote,
    SubmissionUnknown,
    Broker,
    Auth,
    Http,
    WebSocket,
    Serialization,
    UrlParse,
    RateLimit,
    Api,
    Config,
    Network,
    InvalidParameter,
    Timeout,
    ConnectionClosed,
    Subscription,
    Stream,
    Io,
    /// Also any kind a newer peer sends that this release does not know, so
    /// an envelope from a newer authority still decodes.
    #[serde(other)]
    Unknown,
}

impl ErrorKind {
    /// The `snake_case` wire name, also used as a metrics label value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Storage => "storage",
            Self::OwnershipLost => "ownership_lost",
            Self::LeaseBusy => "lease_busy",
            Self::CircuitOpen => "circuit_open",
            Self::ReconciliationRequired => "reconciliation_required",
            Self::Halted => "halted",
            Self::Suspended => "suspended",
            Self::RiskDenied => "risk_denied",
            Self::BaselineStale => "baseline_stale",
            Self::Duplicate => "duplicate",
            Self::ApprovalInvalid => "approval_invalid",
            Self::Remote => "remote",
            Self::SubmissionUnknown => "submission_unknown",
            Self::Broker => "broker",
            Self::Auth => "auth",
            Self::Http => "http",
            Self::WebSocket => "web_socket",
            Self::Serialization => "serialization",
            Self::UrlParse => "url_parse",
            Self::RateLimit => "rate_limit",
            Self::Api => "api",
            Self::Config => "config",
            Self::Network => "network",
            Self::InvalidParameter => "invalid_parameter",
            Self::Timeout => "timeout",
            Self::ConnectionClosed => "connection_closed",
            Self::Subscription => "subscription",
            Self::Stream => "stream",
            Self::Io => "io",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Wire form of an [`enum@Error`] with enough structure to rebuild the typed
/// variant on the other side of a process boundary.
///
/// `message` is the variant's own detail (or the fixed `Display` text for
/// payload-free variants), never the full `Display` string, so decoding does
/// not double-prefix. [`Error::Broker`] details are omitted on purpose; only
/// the upstream status crosses the boundary. Errors wrapping foreign types
/// (`auth`, `network`, `web_socket`, `url_parse`, `stream`) cannot be rebuilt
/// and decode to [`Error::Unknown`] carrying their `Display` text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct ErrorEnvelope {
    pub kind: ErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Upstream HTTP status for `broker`, `http` and `remote`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Seconds to wait for `rate_limit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<u64>,
    /// Seconds elapsed for `timeout`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<u64>,
    /// Schwab error code for `api`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl From<&Error> for ErrorEnvelope {
    fn from(error: &Error) -> Self {
        let mut envelope = Self {
            kind: error.kind(),
            message: String::new(),
            attempt_id: None,
            account: None,
            status: None,
            retry_after: None,
            duration: None,
            code: None,
        };
        match error {
            Error::Storage(message)
            | Error::Halted(message)
            | Error::Suspended(message)
            | Error::RiskDenied(message)
            | Error::Duplicate(message)
            | Error::ApprovalInvalid(message)
            | Error::Config(message)
            | Error::InvalidParameter(message)
            | Error::Subscription(message)
            | Error::Unknown(message) => envelope.message = message.clone(),
            Error::ReconciliationRequired { attempt_id } => {
                envelope.attempt_id = Some(attempt_id.clone());
            }
            Error::SubmissionUnknown { attempt_id, source } => {
                envelope.attempt_id = Some(attempt_id.clone());
                envelope.message = source.to_string();
            }
            Error::BaselineStale { account, reason } => {
                envelope.account = Some(account.clone());
                envelope.message = reason.clone();
            }
            Error::Remote { status, message } | Error::Http { status, message } => {
                envelope.status = Some(status.as_u16());
                envelope.message = message.clone();
            }
            Error::Broker { status, .. } => {
                envelope.status = Some(status.as_u16());
                envelope.message = error.to_string();
            }
            Error::RateLimit { retry_after } => envelope.retry_after = Some(*retry_after),
            Error::Timeout { duration } => envelope.duration = Some(*duration),
            Error::Api { code, message } => {
                envelope.code = Some(code.clone());
                envelope.message = message.clone();
            }
            // Rebuilt on decode, so carry the inner text without the prefix
            Error::Validation(inner) => envelope.message = inner.to_string(),
            Error::Serialization(inner) => envelope.message = inner.to_string(),
            Error::Io(inner) => envelope.message = inner.to_string(),
            Error::OwnershipLost
            | Error::LeaseBusy
            | Error::CircuitOpen
            | Error::ConnectionClosed
            | Error::Auth(_)
            | Error::WebSocket(_)
            | Error::UrlParse(_)
            | Error::Network(_)
            | Error::Stream(_) => envelope.message = error.to_string(),
        }
        envelope
    }
}

impl From<ErrorEnvelope> for Error {
    fn from(envelope: ErrorEnvelope) -> Self {
        let malformed = |envelope: &ErrorEnvelope| {
            Error::Unknown(format!(
                "Malformed {} error envelope: {}",
                envelope.kind, envelope.message
            ))
        };
        let status = envelope
            .status
            .and_then(|status| reqwest::StatusCode::from_u16(status).ok());
        match envelope.kind {
            ErrorKind::Storage => Error::Storage(envelope.message),
            ErrorKind::OwnershipLost => Error::OwnershipLost,
            ErrorKind::LeaseBusy => Error::LeaseBusy,
            ErrorKind::CircuitOpen => Error::CircuitOpen,
            ErrorKind::ReconciliationRequired => match envelope.attempt_id.clone() {
                Some(attempt_id) => Error::ReconciliationRequired { attempt_id },
                None => malformed(&envelope),
            },
            ErrorKind::SubmissionUnknown => match envelope.attempt_id.clone() {
                Some(attempt_id) => Error::SubmissionUnknown {
                    attempt_id,
                    source: Box::new(Error::Unknown(envelope.message)),
                },
                None => malformed(&envelope),
            },
            ErrorKind::Halted => Error::Halted(envelope.message),
            ErrorKind::Suspended => Error::Suspended(envelope.message),
            ErrorKind::RiskDenied => Error::RiskDenied(envelope.message),
            ErrorKind::BaselineStale => Error::BaselineStale {
                account: envelope.account.unwrap_or_default(),
                reason: envelope.message,
            },
            ErrorKind::Duplicate => Error::Duplicate(envelope.message),
            ErrorKind::ApprovalInvalid => Error::ApprovalInvalid(envelope.message),
            ErrorKind::Remote => match status {
                Some(status) => Error::Remote {
                    status,
                    message: envelope.message,
                },
                None => malformed(&envelope),
            },
            ErrorKind::Broker => match status {
                Some(status) => Error::Broker {
                    status,
                    details: Vec::new(),
                    request_id: None,
                },
                None => malformed(&envelope),
            },
            ErrorKind::Http => match status {
                Some(status) => Error::Http {
                    status,
                    message: envelope.message,
                },
                None => malformed(&envelope),
            },
            ErrorKind::RateLimit => match envelope.retry_after {
                Some(retry_after) => Error::RateLimit { retry_after },
                None => malformed(&envelope),
            },
            ErrorKind::Api => match envelope.code.clone() {
                Some(code) => Error::Api {
                    code,
                    message: envelope.message,
                },
                None => malformed(&envelope),
            },
            ErrorKind::Config => Error::Config(envelope.message),
            ErrorKind::InvalidParameter => Error::InvalidParameter(envelope.message),
            ErrorKind::Timeout => match envelope.duration {
                Some(duration) => Error::Timeout { duration },
                None => malformed(&envelope),
            },
            ErrorKind::ConnectionClosed => Error::ConnectionClosed,
            ErrorKind::Subscription => Error::Subscription(envelope.message),
            ErrorKind::Serialization => Error::Serialization(
                <serde_json::Error as serde::de::Error>::custom(envelope.message),
            ),
            ErrorKind::Io => Error::Io(std::io::Error::other(envelope.message)),
            ErrorKind::Auth
            | ErrorKind::WebSocket
            | ErrorKind::UrlParse
            | ErrorKind::Network
            | ErrorKind::Stream
            | ErrorKind::Unknown => Error::Unknown(envelope.message),
        }
    }
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

        // Trading-control outcomes need a new intent, never a blind retry
        assert!(!Error::Halted("operator".into()).is_retryable());
        assert!(!Error::RiskDenied("limit".into()).is_retryable());
        assert!(
            !Error::BaselineStale {
                account: "A".into(),
                reason: "expired".into()
            }
            .is_retryable()
        );
        assert!(!Error::Duplicate("tag".into()).is_retryable());
        assert!(!Error::ApprovalInvalid("expired".into()).is_retryable());

        // Authority failures follow the HTTP 5xx rule but never expose an
        // upstream status
        let unavailable = Error::Remote {
            status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
            message: "restarting".into(),
        };
        assert!(unavailable.is_retryable());
        assert_eq!(unavailable.http_status(), None);
        assert!(
            !Error::Remote {
                status: reqwest::StatusCode::UNAUTHORIZED,
                message: "bad bearer".into(),
            }
            .is_auth_error()
        );
    }

    /// Both documented error shapes, and a numeric `status`, become broker
    /// errors that carry Schwab's reason.
    #[test]
    fn schwab_error_bodies_keep_their_reasons() {
        let numeric = parse_api_error(
            reqwest::StatusCode::NOT_FOUND,
            r#"{"errors":[{"id":"x","status":404,"title":"Not Found","detail":"No such order"}]}"#,
        );
        let Error::Broker { details, .. } = &numeric else {
            panic!("{numeric:?}");
        };
        assert_eq!(details[0].status.as_deref(), Some("404"));
        assert_eq!(details[0].detail.as_deref(), Some("No such order"));
        let trader = parse_api_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"message":"Order rejected","errors":["Insufficient buying power"]}"#,
        );
        let Error::Broker { details, .. } = &trader else {
            panic!("{trader:?}");
        };
        assert_eq!(details[0].title.as_deref(), Some("Order rejected"));
        assert_eq!(
            details[0].detail.as_deref(),
            Some("Insufficient buying power")
        );
        assert!(matches!(
            parse_api_error(reqwest::StatusCode::BAD_GATEWAY, "<html>"),
            Error::Http { .. }
        ));
    }

    fn round_trip(error: Error) -> Error {
        let json = serde_json::to_string(&ErrorEnvelope::from(&error)).unwrap();
        Error::from(serde_json::from_str::<ErrorEnvelope>(&json).unwrap())
    }

    #[test]
    fn envelope_round_trips_typed_variants() {
        assert!(matches!(
            round_trip(Error::Halted("review".into())),
            Error::Halted(m) if m == "review"
        ));
        assert!(matches!(
            round_trip(Error::Suspended("drain".into())),
            Error::Suspended(m) if m == "drain"
        ));
        assert!(matches!(
            round_trip(Error::RiskDenied("limit".into())),
            Error::RiskDenied(m) if m == "limit"
        ));
        assert!(matches!(
            round_trip(Error::BaselineStale { account: "A".into(), reason: "old".into() }),
            Error::BaselineStale { account, reason } if account == "A" && reason == "old"
        ));
        assert!(matches!(
            round_trip(Error::Duplicate("tag".into())),
            Error::Duplicate(m) if m == "tag"
        ));
        assert!(matches!(
            round_trip(Error::ApprovalInvalid("expired".into())),
            Error::ApprovalInvalid(m) if m == "expired"
        ));
        assert!(matches!(
            round_trip(Error::ReconciliationRequired { attempt_id: "a1".into() }),
            Error::ReconciliationRequired { attempt_id } if attempt_id == "a1"
        ));
        let Error::SubmissionUnknown { attempt_id, source } =
            round_trip(Error::SubmissionUnknown {
                attempt_id: "a2".into(),
                source: Box::new(Error::ConnectionClosed),
            })
        else {
            panic!("submission_unknown must survive the round trip");
        };
        assert_eq!(attempt_id, "a2");
        assert!(matches!(*source, Error::Unknown(ref m) if m == "Connection closed unexpectedly"));
        assert!(matches!(
            round_trip(Error::OwnershipLost),
            Error::OwnershipLost
        ));
        assert!(matches!(round_trip(Error::CircuitOpen), Error::CircuitOpen));
        assert!(matches!(round_trip(Error::LeaseBusy), Error::LeaseBusy));
        assert!(matches!(
            round_trip(Error::Broker {
                status: reqwest::StatusCode::BAD_REQUEST,
                details: vec![ApiError::new(None, "secret", None)],
                request_id: Some("r".into()),
            }),
            Error::Broker { status, details, request_id: None }
                if status == reqwest::StatusCode::BAD_REQUEST && details.is_empty()
        ));
        assert!(matches!(
            round_trip(Error::RateLimit { retry_after: 7 }),
            Error::RateLimit { retry_after: 7 }
        ));
        assert!(matches!(
            round_trip(Error::Timeout { duration: 30 }),
            Error::Timeout { duration: 30 }
        ));
        assert!(matches!(
            round_trip(Error::Remote {
                status: reqwest::StatusCode::BAD_GATEWAY,
                message: "upstream".into(),
            }),
            Error::Remote { status, message }
                if status == reqwest::StatusCode::BAD_GATEWAY && message == "upstream"
        ));
        assert!(matches!(
            round_trip(Error::Api { code: "E1".into(), message: "m".into() }),
            Error::Api { code, message } if code == "E1" && message == "m"
        ));
        assert!(matches!(
            round_trip(Error::Io(std::io::Error::other("disk"))),
            Error::Io(e) if e.to_string() == "disk"
        ));
    }

    #[test]
    fn envelope_decodes_foreign_and_malformed_payloads_conservatively() {
        // Foreign payloads keep their Display text but cannot be rebuilt
        let auth = round_trip(Error::Auth(AuthError::TokenExpired));
        assert!(matches!(&auth, Error::Unknown(m) if m == "Authentication error: Token expired"));
        assert!(!auth.is_auth_error());

        // Required structured fields missing -> Unknown, never a panic or a
        // fabricated attempt id
        let mut envelope = ErrorEnvelope::from(&Error::ReconciliationRequired {
            attempt_id: "a".into(),
        });
        envelope.attempt_id = None;
        assert!(
            matches!(Error::from(envelope), Error::Unknown(m) if m.contains("reconciliation_required"))
        );

        // A kind from a newer peer decodes as Unknown: never misread as a
        // known kind, never retryable, and the envelope itself still decodes.
        let later: ErrorEnvelope =
            serde_json::from_str(r#"{"kind":"later","message":"new"}"#).unwrap();
        assert_eq!(later.kind, ErrorKind::Unknown);
        let later = Error::from(later);
        assert!(matches!(&later, Error::Unknown(m) if m == "new"));
        assert!(!later.is_retryable());

        // Wire names are stable
        assert_eq!(
            serde_json::to_value(ErrorKind::BaselineStale).unwrap(),
            serde_json::Value::String("baseline_stale".into())
        );
        assert_eq!(ErrorKind::RiskDenied.to_string(), "risk_denied");
        assert_eq!(Error::Halted(String::new()).kind(), ErrorKind::Halted);
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
