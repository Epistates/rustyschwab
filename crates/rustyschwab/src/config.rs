//! Configuration types for the Schwab SDK.
//!
//! Provides strongly-typed configuration for OAuth, HTTP client, streaming, and retry policies.

use crate::auth::{OAuthConfig, TokenStoreKind};
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

/// Allowed API domains for security - prevents configuration injection attacks (SSRF)
/// Schwab hosts, matched exactly or as a parent domain.
const ALLOWED_API_DOMAINS: &[&str] = &[
    "api.schwabapi.com",
    "streamer.schwabapi.com",
    // The `streamerSocketUrl` Schwab's user preferences name.
    "streamer-api.schwab.com",
];
/// Loopback hosts for tests and development, matched exactly: no
/// `*.localhost` name inherits their trust (or their plain `http`/`ws`).
const LOOPBACK_HOSTS: &[&str] = &["127.0.0.1", "localhost"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchwabConfig {
    pub oauth: OAuthConfig,
    pub client: ClientConfig,
    pub streaming: Option<StreamConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub base_url: String,
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
    pub retry: RetryConfig,
    pub rate_limit: RateLimitConfig,
    pub circuit_breaker: CircuitBreakerConfig,
    pub user_agent: String,
}

/// Channel type for streaming message passing
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    /// Legacy configuration, capped at 65,536 messages by StreamClient.
    /// Overflow behaves as for `Bounded`.
    Unbounded,

    /// Bounded channel with specified buffer size.
    /// Overflow drops frames, emits a loss notification (`ready: false`) and,
    /// once the consumer drains half the queue, resumes delivery under a new
    /// generation on the same connection.
    Bounded(usize),
}

/// The streamer Schwab's user preferences name today. Prefer the
/// `streamerSocketUrl` they carry: see [`StreamConfig::set_socket_url`].
pub const DEFAULT_STREAMER_URL: &str = "wss://streamer-api.schwab.com/ws";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamConfig {
    pub websocket_url: String,
    pub reconnect: ReconnectConfig,
    #[serde(with = "humantime_serde")]
    pub heartbeat_interval: Duration,
    #[serde(with = "humantime_serde")]
    pub ping_timeout: Duration,
    pub max_subscriptions: usize,
    /// Channel capacity (default: Bounded(4096)).
    /// Consumers must reconcile on an explicit stream loss notification.
    pub channel_kind: ChannelKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    pub max_retries: u32,
    #[serde(with = "humantime_serde")]
    pub initial_backoff: Duration,
    #[serde(with = "humantime_serde")]
    pub max_backoff: Duration,
    pub backoff_multiplier: f64,
    pub retry_on_status: Vec<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconnectConfig {
    pub enabled: bool,
    pub max_retries: Option<usize>,
    #[serde(with = "humantime_serde")]
    pub initial_backoff: Duration,
    #[serde(with = "humantime_serde")]
    pub max_backoff: Duration,
    pub backoff_multiplier: f64,
    /// Each reconnect delay is drawn uniformly from ±this fraction of the
    /// backoff (0 to 1), so clients dropped together by one streamer
    /// restart do not all log in again at the same instant.
    #[serde(default = "default_reconnect_jitter")]
    pub jitter: f64,
}

fn default_reconnect_jitter() -> f64 {
    0.25
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    pub enabled: bool,
    pub requests_per_second: u32,
    pub burst_size: u32,
}

/// Configuration for the circuit breaker pattern.
///
/// The circuit breaker protects downstream services from cascading failures
/// by failing fast when a service is unhealthy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    /// Whether the circuit breaker is enabled. Default: true
    pub enabled: bool,
    /// Number of failures before opening the circuit. Default: 5
    pub failure_threshold: u32,
    /// Number of successes in half-open state to close the circuit. Default: 3
    pub success_threshold: u32,
    /// Duration to wait in open state before transitioning to half-open
    #[serde(with = "humantime_serde")]
    pub open_duration: Duration,
    /// Maximum requests to allow through in half-open state. Default: 1
    pub half_open_max_requests: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            failure_threshold: 5,
            success_threshold: 3,
            open_duration: Duration::from_secs(30),
            half_open_max_requests: 1,
        }
    }
}

impl Default for SchwabConfig {
    fn default() -> Self {
        Self {
            oauth: OAuthConfig::default(),
            client: ClientConfig::default(),
            streaming: Some(StreamConfig::default()),
        }
    }
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.schwabapi.com".to_string(),
            timeout: Duration::from_secs(10), // Default request timeout
            retry: RetryConfig::default(),
            rate_limit: RateLimitConfig::default(),
            circuit_breaker: CircuitBreakerConfig::default(),
            user_agent: format!("rustyschwab/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

impl StreamConfig {
    /// Stream from the `streamerSocketUrl` of Schwab's user preferences,
    /// where the LOGIN it issues is valid. Refused unless it passes the same
    /// host and scheme checks as configured URLs.
    pub fn set_socket_url(&mut self, url: &str) -> Result<()> {
        SchwabConfig::validate_url_domain(url, "streamerSocketUrl")?;
        self.websocket_url = url.to_owned();
        Ok(())
    }
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            websocket_url: DEFAULT_STREAMER_URL.to_string(),
            reconnect: ReconnectConfig::default(),
            heartbeat_interval: Duration::from_secs(20), // WebSocket heartbeat interval
            ping_timeout: Duration::from_secs(30),       // WebSocket ping timeout
            max_subscriptions: 500,
            // Bounded with explicit loss/lifecycle notifications
            channel_kind: ChannelKind::Bounded(4096),
        }
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(10),
            backoff_multiplier: 2.0,
            retry_on_status: vec![429, 500, 502, 503, 504],
        }
    }
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: None,                       // No retry limit by default
            initial_backoff: Duration::from_secs(2), // Initial reconnection delay
            max_backoff: Duration::from_secs(128),   // Maximum reconnection delay
            backoff_multiplier: 2.0,
            jitter: default_reconnect_jitter(),
        }
    }
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Schwab's documented limit is 120 requests **per minute**;
            // 2 req/s sustains exactly that. A small burst absorbs spiky
            // call patterns without meaningfully exceeding the budget.
            requests_per_second: 2,
            burst_size: 10,
        }
    }
}

impl SchwabConfig {
    /// Validates that all configured URLs are from allowed domains (SSRF prevention).
    ///
    /// This security check prevents configuration injection attacks where an attacker
    /// could redirect SDK traffic to a malicious server.
    pub fn validate_urls(&self) -> Result<()> {
        Self::validate_url_domain(&self.oauth.auth_url, "auth_url")?;
        Self::validate_url_domain(&self.oauth.token_url, "token_url")?;
        Self::validate_url_domain(&self.client.base_url, "base_url")?;
        if let Some(ref streaming) = self.streaming {
            Self::validate_url_domain(&streaming.websocket_url, "websocket_url")?;
        }
        Ok(())
    }

    /// Checks if a URL's domain is in the allowed list and enforces secure schemes.
    pub(crate) fn validate_url_domain(url_str: &str, field_name: &str) -> Result<()> {
        let url = Url::parse(url_str)
            .map_err(|e| Error::Config(format!("Invalid URL for {}: {}", field_name, e)))?;

        let host = url.host_str().ok_or_else(|| {
            Error::Config(format!(
                "No host found in URL for {}: {}",
                field_name, url_str
            ))
        })?;

        let scheme = url.scheme();

        let is_localhost = LOOPBACK_HOSTS.contains(&host);
        let is_allowed = is_localhost
            || ALLOWED_API_DOMAINS
                .iter()
                .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")));

        if !is_allowed {
            return Err(Error::Config(format!(
                "Domain '{}' not in allowed list for {} (allowed: {:?} and loopback)",
                host, field_name, ALLOWED_API_DOMAINS
            )));
        }

        // Strict scheme validation: Only localhost can use http/ws. External must use https/wss.
        if !is_localhost {
            if scheme != "https" && scheme != "wss" {
                return Err(Error::Config(format!(
                    "Insecure scheme '{scheme}' used for external domain '{host}' in \
                     {field_name}. Must use 'https' or 'wss'."
                )));
            }
        } else {
            if scheme != "http" && scheme != "https" && scheme != "ws" && scheme != "wss" {
                return Err(Error::Config(format!(
                    "Invalid scheme '{}' for localhost in {}.",
                    scheme, field_name
                )));
            }
        }

        Ok(())
    }

    pub fn from_env() -> Result<Self> {
        let oauth = OAuthConfig {
            app_key: env::var("SCHWAB_APP_KEY")
                .map_err(|_| Error::Config("SCHWAB_APP_KEY not set".to_string()))?,
            app_secret: env::var("SCHWAB_APP_SECRET")
                .map_err(|_| Error::Config("SCHWAB_APP_SECRET not set".to_string()))?,
            callback_url: env::var("SCHWAB_OAUTH_CALLBACK_URL")
                .unwrap_or_else(|_| "https://127.0.0.1:8080".to_string()),
            auth_url: env::var("SCHWAB_OAUTH_AUTH_URL")
                .unwrap_or_else(|_| "https://api.schwabapi.com/v1/oauth/authorize".to_string()),
            token_url: env::var("SCHWAB_OAUTH_TOKEN_URL")
                .unwrap_or_else(|_| "https://api.schwabapi.com/v1/oauth/token".to_string()),
            tokens_file: env::var("SCHWAB_TOKENS_FILE")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("schwab_tokens.json")),
            capture_callback: env_flag("SCHWAB_CAPTURE_CALLBACK", false)?,
            auto_refresh: env_flag("SCHWAB_AUTO_REFRESH", true)?,
            refresh_buffer_seconds: env_parse("SCHWAB_REFRESH_BUFFER", 61)?,
            // Schwab's public OAuth docs + the Schwabdev reference client
            // do NOT use PKCE. Match `OAuthConfig::default()` and default
            // to `false` — only opt in via env if the user has verified
            // Schwab accepts their `code_challenge`.
            pkce_enabled: env_flag("SCHWAB_PKCE_ENABLED", false)?,
            token_store_kind: match env::var("SCHWAB_TOKEN_STORE").ok() {
                // Unset → platform-appropriate default (Keychain on macOS,
                // EncryptedFile elsewhere). Setting this env var to an
                // unrecognized value now fails loudly rather than silently
                // downgrading to plaintext `File`.
                None => TokenStoreKind::default(),
                Some(raw) => match raw.to_lowercase().as_str() {
                    "keychain" => TokenStoreKind::Keychain,
                    "encrypted" | "encryptedfile" | "encrypted_file" => {
                        TokenStoreKind::EncryptedFile
                    }
                    "file" => TokenStoreKind::File,
                    other => {
                        return Err(Error::Config(format!(
                            "SCHWAB_TOKEN_STORE={other:?} not recognized \
                             (valid: keychain | encrypted | file)"
                        )));
                    }
                },
            },
            allow_external_callback: env_flag("SCHWAB_ALLOW_EXTERNAL_CALLBACK", false)?,
            callback_cert_file: env::var_os("SCHWAB_CALLBACK_CERT_FILE").map(PathBuf::from),
            callback_key_file: env::var_os("SCHWAB_CALLBACK_KEY_FILE").map(PathBuf::from),
            on_token_notification: None,
        };

        let client = ClientConfig {
            base_url: env::var("SCHWAB_CLIENT_BASE_URL")
                .unwrap_or_else(|_| "https://api.schwabapi.com".to_string()),
            ..ClientConfig::default()
        };

        let streaming = if env_flag("SCHWAB_STREAMING_ENABLED", true)? {
            Some(StreamConfig {
                websocket_url: env::var("SCHWAB_WEBSOCKET_URL")
                    .unwrap_or_else(|_| DEFAULT_STREAMER_URL.to_string()),
                ..StreamConfig::default()
            })
        } else {
            None
        };

        let config = Self {
            oauth,
            client,
            streaming,
        };

        // Validate all URLs are from allowed domains (SSRF prevention)
        config.validate_urls()?;

        Ok(config)
    }

    /// Load a configuration written by [`SchwabConfig::save`]. It holds the
    /// app secret, so it gets the same check as a token file: a regular
    /// file (not a symlink) readable by its owner only.
    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = crate::security::read_private_file(path)?;

        let config: Self = serde_json::from_slice(&contents)
            .map_err(|e| Error::Config(format!("Failed to parse config file: {}", e)))?;

        // Validate all URLs are from allowed domains (SSRF prevention)
        config.validate_urls()?;

        Ok(config)
    }

    /// Persist the config to disk.
    ///
    /// Uses [`crate::security::secure_file_write`] (0600 permissions,
    /// atomic temp-file rename) because the serialized config contains
    /// `app_secret` in plaintext; the serialized copy is wiped after writing.
    pub fn save(&self, path: impl AsRef<std::path::Path>) -> Result<()> {
        // Sized so it never reallocates, which would leave an unwiped copy.
        let mut json = zeroize::Zeroizing::new(Vec::with_capacity(64 * 1024));
        serde_json::to_writer_pretty(&mut *json, self)
            .map_err(|e| Error::Config(format!("Failed to serialize config: {}", e)))?;

        crate::security::secure_file_write(path.as_ref(), &json)
            .map_err(|e| Error::Config(format!("Failed to write config file: {}", e)))?;

        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.oauth.app_key.is_empty() {
            return Err(Error::Config("OAuth app_key is required".to_string()));
        }

        if self.oauth.app_secret.is_empty() {
            return Err(Error::Config("OAuth app_secret is required".to_string()));
        }

        if self.client.retry.max_retries > 10 {
            return Err(Error::Config(
                "Maximum retries should not exceed 10".to_string(),
            ));
        }

        if self.client.rate_limit.requests_per_second > 500 {
            return Err(Error::Config(
                "Rate limit should not exceed 500 requests per second".to_string(),
            ));
        }
        let rate = &self.client.rate_limit;
        if rate.enabled && (rate.requests_per_second == 0 || rate.burst_size == 0) {
            return Err(Error::Config(
                "Rate limit and burst must be positive when rate limiting is enabled".into(),
            ));
        }
        let retry = &self.client.retry;
        if !retry.backoff_multiplier.is_finite()
            || retry.backoff_multiplier < 1.0
            || retry.initial_backoff.is_zero()
            || retry.max_backoff < retry.initial_backoff
        {
            return Err(Error::Config(
                "Retry backoff needs a positive initial delay, a maximum at least as long, \
                 and a finite multiplier of at least 1"
                    .into(),
            ));
        }
        if self.client.timeout.is_zero() || self.client.timeout > Duration::from_secs(300) {
            return Err(Error::Config(
                "Request timeout must be positive and at most 300 seconds".into(),
            ));
        }
        if !(30..=1800).contains(&self.oauth.refresh_buffer_seconds) {
            return Err(Error::Config(
                "Token refresh buffer must be between 30 and 1800 seconds".into(),
            ));
        }
        if let Some(stream) = &self.streaming
            && (stream.heartbeat_interval.is_zero()
                || stream.ping_timeout <= stream.heartbeat_interval)
        {
            return Err(Error::Config(
                "Stream heartbeat interval must be positive and shorter than the ping timeout"
                    .into(),
            ));
        }
        if self.oauth.capture_callback || !self.oauth.callback_url.is_empty() {
            let callback = Url::parse(&self.oauth.callback_url)
                .map_err(|e| Error::Config(format!("Invalid callback_url: {e}")))?;
            let loopback = callback
                .host_str()
                .is_some_and(|host| LOOPBACK_HOSTS.contains(&host));
            if callback.scheme() != "https" && !(loopback && callback.scheme() == "http") {
                return Err(Error::Config(
                    "callback_url must use https (http only on loopback)".into(),
                ));
            }
        }

        // SSRF prevention applies regardless of how the config was built
        // — previously only `from_env`/`from_file` enforced the domain
        // allowlist, leaving builder-constructed configs unchecked.
        self.validate_urls()?;

        Ok(())
    }
}

/// An environment variable parsed as `T`, `default` when unset. A value
/// that does not parse is an error, not a silent default: a typo in
/// `SCHWAB_AUTO_REFRESH` should not quietly change behaviour.
fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> Result<T> {
    match env::var(name) {
        Err(env::VarError::NotPresent) => Ok(default),
        Err(env::VarError::NotUnicode(_)) => {
            Err(Error::Config(format!("{name} is not valid UTF-8")))
        }
        Ok(raw) => raw
            .trim()
            .parse()
            .map_err(|_| Error::Config(format!("{name}={raw:?} is not a valid value"))),
    }
}

/// A boolean environment variable: `true`/`false`, `1`/`0`, `yes`/`no` or
/// `on`/`off` in any case; `default` when unset; an error otherwise.
fn env_flag(name: &str, default: bool) -> Result<bool> {
    match env::var(name) {
        Err(env::VarError::NotPresent) => Ok(default),
        Err(env::VarError::NotUnicode(_)) => {
            Err(Error::Config(format!("{name} is not valid UTF-8")))
        }
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            _ => Err(Error::Config(format!(
                "{name}={raw:?} is not a boolean (true/false, 1/0, yes/no, on/off)"
            ))),
        },
    }
}

#[derive(Debug, Clone)]
#[must_use = "a builder does nothing until `build` is called"]
pub struct ConfigBuilder {
    config: SchwabConfig,
}

impl ConfigBuilder {
    pub fn new() -> Self {
        Self {
            config: SchwabConfig::default(),
        }
    }

    pub fn app_key(mut self, key: impl Into<String>) -> Self {
        self.config.oauth.app_key = key.into();
        self
    }

    pub fn app_secret(mut self, secret: impl Into<String>) -> Self {
        self.config.oauth.app_secret = secret.into();
        self
    }

    pub fn callback_url(mut self, url: impl Into<String>) -> Self {
        self.config.oauth.callback_url = url.into();
        self
    }

    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.config.client.base_url = url.into();
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.config.client.timeout = timeout;
        self
    }

    pub fn max_retries(mut self, retries: u32) -> Self {
        self.config.client.retry.max_retries = retries;
        self
    }

    pub fn enable_streaming(mut self, enabled: bool) -> Self {
        if enabled && self.config.streaming.is_none() {
            self.config.streaming = Some(StreamConfig::default());
        } else if !enabled {
            self.config.streaming = None;
        }
        self
    }

    pub fn websocket_url(mut self, url: impl Into<String>) -> Self {
        if let Some(ref mut streaming) = self.config.streaming {
            streaming.websocket_url = url.into();
        }
        self
    }

    pub fn build(self) -> Result<SchwabConfig> {
        self.config.validate()?;
        Ok(self.config)
    }
}

impl Default for ConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_builder() {
        let config = ConfigBuilder::new()
            .app_key("test_key")
            .app_secret("test_secret")
            .callback_url("http://localhost:8080")
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap();

        assert_eq!(config.oauth.app_key, "test_key");
        assert_eq!(config.oauth.app_secret, "test_secret");
        assert_eq!(config.client.timeout, Duration::from_secs(60));
    }

    #[test]
    fn test_config_validation() {
        let config = SchwabConfig::default();
        assert!(config.validate().is_err());

        let config = ConfigBuilder::new()
            .app_key("key")
            .app_secret("secret")
            .build()
            .unwrap();
        assert!(config.validate().is_ok());
    }
}
