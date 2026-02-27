//! Configuration types for the Schwab SDK.
//!
//! Provides strongly-typed configuration for OAuth, HTTP client, streaming, and retry policies.

#![allow(missing_docs)] // Config fields are self-documenting via Schwab API docs

use crate::auth::{OAuthConfig, TokenStoreKind};
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

/// Allowed API domains for security - prevents configuration injection attacks (SSRF)
const ALLOWED_API_DOMAINS: &[&str] = &[
    "api.schwabapi.com",
    "streamer.schwabapi.com",
    // Allow localhost for testing and development
    "127.0.0.1",
    "localhost",
];

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
    /// Unbounded channel (default, backward compatible)
    /// Use when: Guaranteed fast consumer, memory not a concern
    /// Behavior: No backpressure, can grow unbounded
    Unbounded,

    /// Bounded channel with specified buffer size
    /// Use when: Need backpressure, want to limit memory usage
    /// Behavior: Sender blocks when buffer full (backpressure)
    Bounded(usize),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamConfig {
    pub websocket_url: String,
    pub reconnect: ReconnectConfig,
    #[serde(with = "humantime_serde")]
    pub heartbeat_interval: Duration,
    #[serde(with = "humantime_serde")]
    pub ping_timeout: Duration,
    pub max_subscriptions: usize,
    #[deprecated(since = "0.2.0", note = "Use channel_kind instead")]
    pub buffer_size: usize,
    /// Channel type for message passing (default: Unbounded)
    /// Set to Bounded(size) to enable backpressure control
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
            user_agent: format!("schwab-rs/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            websocket_url: "wss://streamer.schwabapi.com/ws".to_string(),
            reconnect: ReconnectConfig::default(),
            heartbeat_interval: Duration::from_secs(20), // WebSocket heartbeat interval
            ping_timeout: Duration::from_secs(30), // WebSocket ping timeout
            max_subscriptions: 500,
            #[allow(deprecated)]
            buffer_size: 10000, // Deprecated, kept for backward compatibility
            channel_kind: ChannelKind::Unbounded, // Default to unbounded for backward compatibility
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
            max_retries: None, // No retry limit by default
            initial_backoff: Duration::from_secs(2), // Initial reconnection delay
            max_backoff: Duration::from_secs(128), // Maximum reconnection delay
            backoff_multiplier: 2.0,
        }
    }
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            requests_per_second: 120,
            burst_size: 20,
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
    fn validate_url_domain(url_str: &str, field_name: &str) -> Result<()> {
        let url = Url::parse(url_str)
            .map_err(|e| Error::Config(format!("Invalid URL for {}: {}", field_name, e)))?;

        let host = url.host_str().ok_or_else(|| {
            Error::Config(format!("No host found in URL for {}: {}", field_name, url_str))
        })?;

        let scheme = url.scheme();

        // Check if host matches any allowed domain
        let is_allowed = ALLOWED_API_DOMAINS.iter().any(|allowed| {
            // Exact match or subdomain match (for *.schwabapi.com)
            host == *allowed || host.ends_with(&format!(".{}", allowed))
        });

        if !is_allowed {
            return Err(Error::Config(format!(
                "Domain '{}' not in allowed list for {} (allowed: {:?})",
                host, field_name, ALLOWED_API_DOMAINS
            )));
        }

        // Strict scheme validation: Only localhost can use http/ws. External must use https/wss.
        let is_localhost = host == "localhost" || host == "127.0.0.1";
        if !is_localhost {
            if scheme != "https" && scheme != "wss" {
                return Err(Error::Config(format!(
                    "Insecure scheme '{}' used for external domain '{}' in {}. Must use 'https' or 'wss'.",
                    scheme, host, field_name
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
            capture_callback: env::var("SCHWAB_CAPTURE_CALLBACK")
                .map(|v| v.parse().unwrap_or(false))
                .unwrap_or(false),
            auto_refresh: env::var("SCHWAB_AUTO_REFRESH")
                .map(|v| v.parse().unwrap_or(true))
                .unwrap_or(true),
            refresh_buffer_seconds: env::var("SCHWAB_REFRESH_BUFFER")
                .map(|v| v.parse().unwrap_or(61))
                .unwrap_or(61),
            pkce_enabled: env::var("SCHWAB_PKCE_ENABLED")
                .map(|v| v.parse().unwrap_or(true))
                .unwrap_or(true),
            token_store_kind: match env::var("SCHWAB_TOKEN_STORE").unwrap_or_else(|_| "file".to_string()).to_lowercase().as_str() {
                "keychain" => TokenStoreKind::Keychain,
                _ => TokenStoreKind::File,
            },
            allow_external_callback: env::var("SCHWAB_ALLOW_EXTERNAL_CALLBACK")
                .map(|v| v.parse().unwrap_or(false))
                .unwrap_or(false),
            on_token_notification: None,
        };

        let client = ClientConfig {
            base_url: env::var("SCHWAB_CLIENT_BASE_URL")
                .unwrap_or_else(|_| "https://api.schwabapi.com".to_string()),
            ..ClientConfig::default()
        };

        let streaming = if env::var("SCHWAB_STREAMING_ENABLED")
            .map(|v| v.parse().unwrap_or(true))
            .unwrap_or(true)
        {
            Some(StreamConfig {
                websocket_url: env::var("SCHWAB_WEBSOCKET_URL")
                    .unwrap_or_else(|_| "wss://streamer.schwabapi.com/ws".to_string()),
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

    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("Failed to read config file: {}", e)))?;

        let config: Self = serde_json::from_str(&contents)
            .map_err(|e| Error::Config(format!("Failed to parse config file: {}", e)))?;

        // Validate all URLs are from allowed domains (SSRF prevention)
        config.validate_urls()?;

        Ok(config)
    }

    pub fn save(&self, path: impl AsRef<std::path::Path>) -> Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("Failed to serialize config: {}", e)))?;

        std::fs::write(path, json)
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

        Ok(())
    }
}

#[derive(Debug, Clone)]
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