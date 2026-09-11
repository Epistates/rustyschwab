//! OAuth 2.0 authentication with PKCE and secure token storage.
//!
//! Provides complete OAuth flow implementation with automatic token refresh.

// Token storage module with secure backends
pub(crate) mod token_store;

// HTTP authorization header utilities
mod headers;

// Re-export token storage types for public API
pub use token_store::{
    EncryptedFileTokenStore, FileTokenStore, KeychainTokenStore, TokenSet, TokenStore,
};

// Re-export header utilities (used internally, but might be useful publicly)
pub use headers::{create_basic_header, create_bearer_header};

use crate::error::AuthError;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL};
use chrono::{DateTime, Duration, Utc};
use parking_lot::RwLock;
use ring::rand::SecureRandom;
use ring::{digest, rand as ring_rand};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

#[cfg(feature = "callback-server")]
use axum::{Router, extract::Query, response::Html, routing::get};

pub type AuthResult<T> = std::result::Result<T, AuthError>;

/// Notification types for token events
#[derive(Debug, Clone)]
pub enum TokenNotification {
    /// Access token will expire soon
    AccessTokenExpiring { seconds_remaining: i64 },
    /// Refresh token will expire soon
    RefreshTokenExpiring { hours_remaining: i64 },
    /// Refresh token has expired, OAuth flow needed
    RefreshTokenExpired,
    /// Token file was corrupted and recreated
    TokenFileCorrupted,
    /// New session created after token refresh
    SessionRecreated,
}

#[cfg(feature = "callback-server")]
#[derive(Debug, Deserialize)]
struct CallbackParams {
    code: String,
    state: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthConfig {
    pub app_key: String,
    pub app_secret: String,
    pub callback_url: String,
    pub auth_url: String,
    pub token_url: String,
    pub tokens_file: PathBuf,
    pub capture_callback: bool,
    pub auto_refresh: bool,
    pub refresh_buffer_seconds: i64,
    /// Enable PKCE (S256). Schwab's public OAuth docs and the battle-tested
    /// Python reference client (Schwabdev) do **not** use PKCE, so we default
    /// to `false`. Opt in only if you've verified Schwab accepts
    /// `code_challenge`/`code_verifier` for your app.
    #[serde(default)]
    pub pkce_enabled: bool,
    /// Token storage backend
    #[serde(default)]
    pub token_store_kind: TokenStoreKind,
    /// Allow callback server to bind to `0.0.0.0` (all interfaces).
    ///
    /// Default: `false` — binds to `127.0.0.1` only, which is what you
    /// want for the standard dev-machine flow.
    ///
    /// Only enable this if you're terminating TLS in front of this
    /// process with an HTTPS tunnel or reverse proxy (ngrok,
    /// cloudflared, an L7 LB, etc.). Exposes the callback port to
    /// your LAN/WAN while enabled.
    #[serde(default)]
    pub allow_external_callback: bool,
    /// Callback function when tokens need user intervention (e.g., refresh token expiring)
    #[serde(skip)]
    pub on_token_notification: Option<Arc<dyn Fn(TokenNotification) + Send + Sync>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenStoreKind {
    /// Plain JSON file with 0600 permissions (basic security)
    File,
    /// Encrypted JSON file with ChaCha20Poly1305 AEAD (recommended)
    EncryptedFile,
    /// OS-native keychain/credential storage (best security)
    Keychain,
}

#[cfg(target_os = "macos")]
impl Default for TokenStoreKind {
    fn default() -> Self {
        tracing::debug!("Defaulting to Keychain storage on macOS");
        TokenStoreKind::Keychain
    }
}

#[cfg(not(target_os = "macos"))]
impl Default for TokenStoreKind {
    fn default() -> Self {
        tracing::debug!("Defaulting to EncryptedFile storage");
        TokenStoreKind::EncryptedFile
    }
}

impl std::fmt::Debug for OAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthConfig")
            .field("app_key", &"***")
            .field("app_secret", &"***")
            .field("callback_url", &self.callback_url)
            .field("auth_url", &self.auth_url)
            .field("token_url", &self.token_url)
            .field("tokens_file", &self.tokens_file)
            .field("capture_callback", &self.capture_callback)
            .field("auto_refresh", &self.auto_refresh)
            .field("refresh_buffer_seconds", &self.refresh_buffer_seconds)
            .field(
                "on_token_notification",
                &self.on_token_notification.is_some(),
            )
            .finish()
    }
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            app_key: String::new(),
            app_secret: String::new(),
            callback_url: "https://127.0.0.1:8080".to_string(),
            auth_url: "https://api.schwabapi.com/v1/oauth/authorize".to_string(),
            token_url: "https://api.schwabapi.com/v1/oauth/token".to_string(),
            tokens_file: PathBuf::from("schwab_tokens.json"),
            capture_callback: false,
            auto_refresh: true,
            refresh_buffer_seconds: 61, // Refresh 61 seconds before expiry
            pkce_enabled: false,        // Schwab + reference client don't require PKCE
            token_store_kind: TokenStoreKind::default(),
            allow_external_callback: false, // Secure default: localhost only
            on_token_notification: None,
        }
    }
}

/// Threshold at which we refuse to refresh the access token and instead
/// surface an error demanding a fresh OAuth flow — matches Schwabdev's
/// 60.5 min (3630 s) safety buffer (`tokens.py:293`).
const REFRESH_TOKEN_MIN_REMAINING_SECS: i64 = 3630;

/// Timeout for POSTs to `/oauth/token`. Matches Schwabdev's `requests`
/// default of 30 s (`tokens.py:275`); the Schwab token endpoint is
/// occasionally slow and a 10 s cap was spuriously timing out.
const OAUTH_TOKEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Retry config for transient 5xx at `/oauth/token`.
const OAUTH_TOKEN_RETRY_ATTEMPTS: u32 = 3;
const OAUTH_TOKEN_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(250);

// TokenSet is now provided by token_store module and re-exported at the top

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: String,
    expires_in: u64,
    scope: Option<String>,
}

impl TokenResponse {
    fn into_token_set(self, issued_at: DateTime<Utc>) -> TokenSet {
        TokenSet::new(
            self.access_token,
            self.refresh_token.unwrap_or_default(),
            self.id_token.unwrap_or_default(),
            self.token_type,
            self.expires_in as i64,
            self.scope.unwrap_or_default(),
            issued_at,
        )
    }
}

#[derive(Clone)]
pub struct AuthManager {
    config: OAuthConfig,
    tokens: Arc<RwLock<Option<TokenSet>>>,
    client: Arc<RwLock<reqwest::Client>>,
    refresh_lock: Arc<Mutex<()>>,
    refresh_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    /// Optional PKCE verifier for the current auth session
    pkce_verifier: Arc<RwLock<Option<String>>>,
    /// OAuth state parameter for CSRF protection (RFC 6749 Section 10.12)
    oauth_state: Arc<RwLock<Option<(String, std::time::Instant)>>>,
    /// Pluggable token store
    token_store: Arc<dyn TokenStore + Send + Sync>,
    /// Last "hours-remaining" bucket we fired a `RefreshTokenExpiring`
    /// notification for. Prevents spamming a 30 s-tick-based modulo check
    /// that misfired or double-fired depending on scheduling jitter.
    last_notified_rt_hour: Arc<RwLock<Option<i64>>>,
}

impl std::fmt::Debug for AuthManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthManager")
            .field("config", &self.config)
            .field("tokens", &"<tokens>")
            .field("client", &"<client>")
            .field("refresh_lock", &"<mutex>")
            .field("refresh_task", &self.refresh_task.read().is_some())
            .field("pkce", &self.config.pkce_enabled)
            .finish()
    }
}

// TokenStore trait and implementations are now provided by token_store module

/// Which OAuth grant is being exchanged — used to pick the right error
/// variant (`OAuthFlow` for initial auth, `RefreshFailed` for refresh)
/// and to tag log/span context.
#[derive(Debug, Clone, Copy)]
enum OAuthGrant {
    AuthCode,
    Refresh,
}

impl OAuthGrant {
    /// The RFC 6749 `grant_type` value, also the metric label.
    fn grant_type(self) -> &'static str {
        match self {
            OAuthGrant::AuthCode => "authorization_code",
            OAuthGrant::Refresh => "refresh_token",
        }
    }

    fn wrap(self, msg: impl Into<String>) -> AuthError {
        match self {
            OAuthGrant::AuthCode => AuthError::OAuthFlow(msg.into()),
            OAuthGrant::Refresh => AuthError::RefreshFailed(msg.into()),
        }
    }
}

/// Single code path for every `POST /oauth/token` call — handles Basic
/// auth header construction, form encoding, transient-5xx retry with
/// jittered backoff, JSON deserialization and the per-grant outcome metric.
///
/// Replaces two near-identical implementations that had drifted slightly
/// in error handling and header construction. 4xx responses are surfaced
/// immediately (no retry) since they indicate a protocol-level problem
/// that won't resolve on retry (bad code, invalid_grant, etc.).
async fn post_oauth_token(
    client: &reqwest::Client,
    config: &OAuthConfig,
    form: &[(&str, String)],
    grant: OAuthGrant,
) -> AuthResult<TokenResponse> {
    let result = post_oauth_token_with_retry(client, config, form, grant).await;
    metrics::counter!(
        crate::metrics::TOKEN_REFRESH_TOTAL,
        "outcome" => if result.is_ok() { "success" } else { "failure" },
        "grant" => grant.grant_type(),
    )
    .increment(1);
    result
}

async fn post_oauth_token_with_retry(
    client: &reqwest::Client,
    config: &OAuthConfig,
    form: &[(&str, String)],
    grant: OAuthGrant,
) -> AuthResult<TokenResponse> {
    let auth_header = create_basic_header(&config.app_key, &config.app_secret);

    let mut last_err: Option<AuthError> = None;
    for attempt in 1..=OAUTH_TOKEN_RETRY_ATTEMPTS {
        let result = client
            .post(&config.token_url)
            .header("Authorization", &auth_header)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(form)
            .send()
            .await;

        let response = match result {
            Ok(r) => r,
            Err(e) => {
                // Network / timeout — retry
                let err = grant.wrap(format!("oauth request error: {e}"));
                last_err = Some(err);
                if attempt < OAUTH_TOKEN_RETRY_ATTEMPTS {
                    backoff(attempt).await;
                    continue;
                }
                break;
            }
        };

        let status = response.status();
        if status.is_success() {
            return response
                .json::<TokenResponse>()
                .await
                .map_err(|e| grant.wrap(format!("failed to parse token response: {e}")));
        }

        // Do not propagate token endpoint bodies: a proxy can echo credentials.

        // 5xx: transient — retry. 4xx: fatal — return immediately.
        if status.is_server_error() {
            warn!(
                "oauth token endpoint returned {} on attempt {}/{}",
                status, attempt, OAUTH_TOKEN_RETRY_ATTEMPTS
            );
            last_err = Some(grant.wrap(format!("oauth token endpoint returned {status}")));
            if attempt < OAUTH_TOKEN_RETRY_ATTEMPTS {
                backoff(attempt).await;
                continue;
            }
            break;
        }

        return Err(grant.wrap(format!("oauth token endpoint returned {status}")));
    }

    Err(last_err.unwrap_or_else(|| grant.wrap("oauth token request failed")))
}

/// Exponential jittered backoff between `/oauth/token` retries.
async fn backoff(attempt: u32) {
    let base = OAUTH_TOKEN_RETRY_BASE * 2u32.saturating_pow(attempt - 1);
    let jitter_ms: u64 = (rand::random::<u64>() % 100) + 1;
    tokio::time::sleep(base + std::time::Duration::from_millis(jitter_ms)).await;
}

impl AuthManager {
    pub fn new(config: OAuthConfig) -> AuthResult<Self> {
        // Validate app key and secret lengths per Schwab requirements (matching Python reference)
        if config.app_key.is_empty() {
            return Err(AuthError::MissingConfig(
                "App key cannot be empty".to_string(),
            ));
        }
        if config.app_secret.is_empty() {
            return Err(AuthError::MissingConfig(
                "App secret cannot be empty".to_string(),
            ));
        }

        // Schwab issues several key/secret length variants, but they are
        // always even-length and at least 32 chars combined — the same
        // heuristic Schwabdev adopted in 3.0.4 after exact-length checks
        // started rejecting newly issued credentials.
        let key_len = config.app_key.len();
        let secret_len = config.app_secret.len();
        if !key_len.is_multiple_of(2) || !secret_len.is_multiple_of(2) || key_len + secret_len < 32
        {
            return Err(AuthError::MissingConfig(format!(
                "App key or app secret likely invalid: lengths must be even and total at \
                 least 32 chars (got key={key_len}, secret={secret_len})"
            )));
        }

        // Validate callback URL (must be HTTPS and no trailing slash) - matching Python
        if !config.callback_url.starts_with("https") {
            return Err(AuthError::MissingConfig(
                "Callback URL must be HTTPS".to_string(),
            ));
        }
        if config.callback_url.ends_with('/') {
            return Err(AuthError::MissingConfig(
                "Callback URL cannot be path (ends with '/')".to_string(),
            ));
        }

        let manager = Self {
            config: config.clone(),
            tokens: Arc::new(RwLock::new(None)),
            client: Arc::new(RwLock::new(
                reqwest::Client::builder()
                    .timeout(OAUTH_TOKEN_TIMEOUT)
                    .redirect(reqwest::redirect::Policy::none())
                    .retry(reqwest::retry::never())
                    .build()
                    .map_err(|e| AuthError::OAuthFlow(e.to_string()))?,
            )),
            refresh_lock: Arc::new(Mutex::new(())),
            refresh_task: Arc::new(RwLock::new(None)),
            pkce_verifier: Arc::new(RwLock::new(None)),
            oauth_state: Arc::new(RwLock::new(None)),
            last_notified_rt_hour: Arc::new(RwLock::new(None)),
            token_store: match config.token_store_kind {
                TokenStoreKind::File => {
                    tracing::info!("Using FileTokenStore (basic security: 0600 permissions only)");
                    Arc::new(FileTokenStore::new(config.tokens_file.clone()))
                }
                TokenStoreKind::EncryptedFile => {
                    tracing::info!(
                        "Using EncryptedFileTokenStore \
                         (ChaCha20Poly1305 encryption + 0600 permissions)"
                    );
                    Arc::new(EncryptedFileTokenStore::new(config.tokens_file.clone()))
                }
                TokenStoreKind::Keychain => {
                    tracing::info!("Using KeychainTokenStore (OS-native credential storage)");
                    Arc::new(KeychainTokenStore::new(
                        "schwab-rs".to_string(),
                        config.app_key.clone(),
                    ))
                }
            },
        };

        // Try to load existing tokens
        match manager.load_tokens() {
            Ok(tokens) => {
                *manager.tokens.write() = Some(tokens);
            }
            Err(e) => {
                warn!("Could not load tokens; authentication may be required");
                let _ = e;
                // Notify if token file was corrupted
                if let Some(ref notify) = config.on_token_notification {
                    notify(TokenNotification::TokenFileCorrupted);
                }
            }
        }

        Ok(manager)
    }

    /// Returns a reference to the OAuth configuration
    pub fn config(&self) -> &OAuthConfig {
        &self.config
    }

    pub async fn start(&self) -> AuthResult<()> {
        // Load tokens from file if they exist
        if let Ok(tokens) = self.load_tokens() {
            *self.tokens.write() = Some(tokens);
        }

        // Start auto-refresh task if enabled. Guard with a finished()
        // check so multiple `start()` calls (e.g. re-inits from an
        // embedding server) don't stack extra refresh tasks; prior
        // versions silently leaked the previous one.
        if self.config.auto_refresh {
            let already_running = self
                .refresh_task
                .read()
                .as_ref()
                .map(|h| !h.is_finished())
                .unwrap_or(false);
            if !already_running {
                self.start_refresh_task().await;
            }
        }

        Ok(())
    }

    pub async fn get_access_token(&self) -> AuthResult<String> {
        // Check if we have valid tokens
        {
            let token_guard = self.tokens.read();
            if let Some(tokens) = token_guard.as_ref()
                && tokens.is_valid()
                && !tokens.needs_refresh(self.config.refresh_buffer_seconds)
            {
                return tokens.access_token();
            }
        } // Drop lock before await

        // Need to refresh or get new tokens
        self.ensure_valid_tokens().await?;

        self.tokens
            .read()
            .as_ref()
            .ok_or(AuthError::TokenExpired)?
            .access_token()
    }

    pub async fn ensure_valid_tokens(&self) -> AuthResult<()> {
        self.refresh_if_needed(None).await
    }

    /// Refresh only if the broker-rejected access token is still current.
    /// Concurrent 401s reuse the winning refresh instead of rotating again.
    pub async fn refresh_rejected_token(&self, rejected: &str) -> AuthResult<()> {
        self.refresh_if_needed(Some(rejected)).await
    }

    async fn refresh_if_needed(&self, rejected: Option<&str>) -> AuthResult<()> {
        let _lock = self.refresh_lock.lock().await;
        let _process_lock = token_store::refresh_lock(&self.config.tokens_file).await?;
        // Another process may already have committed a newer token.
        if let Ok(stored) = self.token_store.load() {
            let mut current = self.tokens.write();
            if current
                .as_ref()
                .is_none_or(|t| stored.issued_at >= t.issued_at)
            {
                *current = Some(stored);
            }
        }
        let old = self.tokens.read().clone().ok_or(AuthError::TokenExpired)?;
        if let Some(rejected) = rejected {
            if old.access_token()? != rejected && old.is_valid() {
                return Ok(());
            }
        } else if old.is_valid() && !old.needs_refresh(self.config.refresh_buffer_seconds) {
            return Ok(());
        }
        let remaining = (old
            .refresh_token_expires_at
            .unwrap_or(old.issued_at + Duration::days(7))
            - Utc::now())
        .num_seconds();
        if remaining <= 0 {
            return Err(AuthError::TokenExpired);
        }
        if remaining <= REFRESH_TOKEN_MIN_REMAINING_SECS {
            return Err(AuthError::RefreshTokenExpiring {
                seconds_remaining: remaining,
            });
        }
        let refresh_token = old.refresh_token()?;
        if refresh_token.is_empty() {
            return Err(AuthError::MissingRefreshToken);
        }
        let mut new_tokens = self.refresh_tokens(&refresh_token).await?;
        // Schwab's refresh grant does not extend the original authorization's
        // seven-day lifetime, even if the returned credential string changes.
        new_tokens.refresh_token_expires_at = Some(
            old.refresh_token_expires_at
                .unwrap_or(old.issued_at + Duration::days(7)),
        );
        self.token_store.save(&new_tokens)?;
        *self.tokens.write() = Some(new_tokens);
        Ok(())
    }

    pub async fn authorize(&self) -> AuthResult<(String, String)> {
        // Generate OAuth state parameter for CSRF protection (RFC 6749 Section 10.12)
        // Using UUID v4 provides sufficient entropy for secure state tokens
        let state = uuid::Uuid::new_v4().to_string();
        *self.oauth_state.write() = Some((state.clone(), std::time::Instant::now()));
        debug!("Generated OAuth state parameter for CSRF protection");

        let mut url = url::Url::parse(&self.config.auth_url)
            .map_err(|_| AuthError::OAuthFlow("Invalid authorization URL".into()))?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.config.app_key)
            .append_pair("redirect_uri", &self.config.callback_url)
            .append_pair("state", &state);
        if self.config.pkce_enabled {
            let verifier = Self::generate_code_verifier()?;
            let challenge = Self::code_challenge_s256(&verifier);
            *self.pkce_verifier.write() = Some(verifier);
            url.query_pairs_mut()
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256");
        }
        let auth_url = url.to_string();

        info!("Open this URL in your browser:\n{}", auth_url);

        if self.config.capture_callback {
            // Start local server to capture callback
            let auth_code = self.capture_callback_code().await?;
            let tokens = self.exchange_code_for_tokens(&auth_code).await?;
            *self.tokens.write() = Some(tokens);
            self.save_tokens()?;
            Ok((auth_url, auth_code))
        } else {
            Ok((auth_url, String::new()))
        }
    }

    pub async fn exchange_code(&self, code: String) -> AuthResult<TokenSet> {
        // Validate input
        if code.trim().is_empty() {
            return Err(AuthError::OAuthFlow(
                "Authorization code is empty".to_string(),
            ));
        }

        // Extract code from full URL if provided
        let extracted_code = if code.starts_with("https://") || code.starts_with("http://") {
            self.extract_code_from_callback_url(&code)?
        } else {
            code
        };

        // Validate extracted code format
        // Authorization codes should be alphanumeric (and may have @ suffix)
        let code_without_at = extracted_code.trim_end_matches('@');
        if !code_without_at
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            return Err(AuthError::OAuthFlow(
                "Invalid authorization code format: contains invalid characters".to_string(),
            ));
        }

        if code_without_at.len() < 10 {
            return Err(AuthError::OAuthFlow(
                "Authorization code too short - may be truncated or invalid".to_string(),
            ));
        }

        self.exchange_code_for_tokens(&extracted_code).await
    }

    fn consume_callback_state(&self, supplied: Option<&str>) -> AuthResult<()> {
        let mut pending = self.oauth_state.write();
        match pending.as_ref() {
            Some((expected, created))
                if supplied == Some(expected.as_str())
                    && created.elapsed() < std::time::Duration::from_secs(300) =>
            {
                *pending = None;
                Ok(())
            }
            _ => Err(AuthError::OAuthFlow(
                "Missing, invalid, expired, or already consumed OAuth state".into(),
            )),
        }
    }

    fn extract_code_from_callback_url(&self, url: &str) -> AuthResult<String> {
        // Parse URL to extract code and state parameters
        let parsed = url::Url::parse(url)
            .map_err(|e| AuthError::InvalidCallbackUrl(format!("Invalid callback URL: {}", e)))?;

        // Extract state parameter and validate CSRF token (RFC 6749 Section 10.12)
        let callback_state = parsed
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned());

        self.consume_callback_state(callback_state.as_deref())?;

        // Extract code from query parameters.
        //
        // `url::Url::query_pairs` already URL-decodes values — matches
        // Python's `urllib.parse.unquote`. The previous implementation only
        // stripped a trailing `%40` hack which would have left other
        // percent-encoded characters raw; `query_pairs` handles the full
        // percent-decoding for us.
        let code = parsed
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.into_owned())
            .ok_or_else(|| {
                AuthError::OAuthFlow("Callback URL missing 'code' parameter".to_string())
            })?;

        // Schwab auth codes must be suffixed with `@` for the token
        // exchange — Schwabdev does not enforce this, but we've verified
        // via `OAUTH_IMPLEMENTATION.md` that the exchange fails without it.
        let decoded = if code.ends_with('@') {
            code
        } else {
            format!("{}@", code)
        };

        Ok(decoded)
    }

    async fn exchange_code_for_tokens(&self, code: &str) -> AuthResult<TokenSet> {
        // The Python reference adds @ at the end of the code if not present
        let code_with_at = if !code.ends_with('@') {
            format!("{}@", code)
        } else {
            code.to_string()
        };

        debug!("Exchanging code for tokens");
        debug!("Token URL: {}", self.config.token_url);
        debug!("Redirect URI: {}", self.config.callback_url);
        debug!(
            "Code length: {}, ends with @: {}",
            code_with_at.len(),
            code_with_at.ends_with('@')
        );
        debug!(
            "App Key length: {}, Secret length: {}",
            self.config.app_key.len(),
            self.config.app_secret.len()
        );

        let mut form_params: Vec<(&str, String)> = vec![
            ("grant_type", OAuthGrant::AuthCode.grant_type().to_string()),
            ("code", code_with_at.clone()),
            ("redirect_uri", self.config.callback_url.clone()),
        ];
        if self.config.pkce_enabled {
            // CRITICAL: Take (consume) the verifier to enforce single-use per RFC 7636
            // This prevents accidental reuse and ensures spec compliance
            let verifier = self.pkce_verifier.write().take().ok_or_else(|| {
                AuthError::OAuthFlow(
                    "PKCE enabled but no code verifier found. Call authorize() first.".to_string(),
                )
            })?;
            form_params.push(("code_verifier", verifier));
            // Verifier is now consumed and cannot be reused
        }

        let client = self.client.read().clone();
        let token_response =
            post_oauth_token(&client, &self.config, &form_params, OAuthGrant::AuthCode).await?;

        let tokens = token_response.into_token_set(Utc::now());

        let _file_lock = token_store::refresh_lock(&self.config.tokens_file).await?;
        self.token_store.save(&tokens)?;
        *self.tokens.write() = Some(tokens.clone());

        Ok(tokens)
    }

    async fn refresh_tokens(&self, refresh_token: &str) -> AuthResult<TokenSet> {
        debug!("Refreshing access token");

        let client = self.client.read().clone();
        let mut token_response = post_oauth_token(
            &client,
            &self.config,
            &[
                ("grant_type", OAuthGrant::Refresh.grant_type().to_string()),
                ("refresh_token", refresh_token.to_string()),
            ],
            OAuthGrant::Refresh,
        )
        .await?;

        // Keep the same refresh token if not provided
        if token_response.refresh_token.is_none() {
            token_response.refresh_token = Some(refresh_token.to_string());
        }

        let tokens = token_response.into_token_set(Utc::now());

        info!(
            "Token refreshed successfully, expires at: {}",
            tokens.expires_at
        );

        Ok(tokens)
    }

    async fn start_refresh_task(&self) {
        // The worker must not own the handle that controls its own lifetime.
        let mut manager = self.clone();
        manager.refresh_task = Arc::new(RwLock::new(None));
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                let remaining = manager
                    .tokens
                    .read()
                    .as_ref()
                    .and_then(|t| t.refresh_token_expires_at)
                    .map(|t| (t - Utc::now()).num_seconds());
                if let Some(remaining) = remaining {
                    let hour = (remaining / 3600).max(0);
                    let notify = {
                        let mut last = manager.last_notified_rt_hour.write();
                        let changed = remaining <= 43200 && *last != Some(hour);
                        if changed {
                            *last = Some(hour);
                        }
                        changed
                    };
                    if notify && let Some(ref callback) = manager.config.on_token_notification {
                        callback(if remaining <= 0 {
                            TokenNotification::RefreshTokenExpired
                        } else {
                            TokenNotification::RefreshTokenExpiring {
                                hours_remaining: hour,
                            }
                        });
                    }
                }
                if manager.ensure_valid_tokens().await.is_err() {
                    warn!(
                        "Automatic token refresh failed; authentication may require intervention"
                    );
                }
            }
        });
        *self.refresh_task.write() = Some(task);
    }

    #[cfg(feature = "callback-server")]
    async fn capture_callback_code(&self) -> AuthResult<String> {
        use std::sync::Arc;
        use tokio::sync::oneshot;

        let (tx, rx) = oneshot::channel();
        let tx = Arc::new(Mutex::new(Some(tx)));

        // OAuth callback handler
        let callback_handler = {
            let tx = tx.clone();
            let manager = self.clone();
            move |Query(params): Query<CallbackParams>| {
                let manager = manager.clone();
                async move {
                    if manager
                        .consume_callback_state(params.state.as_deref())
                        .is_err()
                    {
                        return Html("Invalid or expired authorization callback".to_string());
                    }
                    // Extract code and add @ like Python reference does
                    let code_with_at = if !params.code.ends_with('@') {
                        format!("{}@", params.code)
                    } else {
                        params.code.clone()
                    };

                    if let Some(tx) = tx.lock().await.take() {
                        let _ = tx.send(code_with_at);
                    }

                    Html(
                        r#"<!DOCTYPE html>
                    <html>
                    <head>
                        <title>Authorization Successful</title>
                        <style>
                            body {
                                font-family: system-ui, -apple-system, sans-serif;
                                display: flex;
                                justify-content: center;
                                align-items: center;
                                height: 100vh;
                                margin: 0;
                                background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
                            }
                            .container {
                                background: white;
                                padding: 2rem;
                                border-radius: 10px;
                                box-shadow: 0 10px 40px rgba(0,0,0,0.2);
                                text-align: center;
                            }
                            .success { color: #48bb78; font-size: 3rem; }
                        </style>
                    </head>
                    <body>
                        <div class="container">
                            <div class="success">✓</div>
                            <h1>Authorization Successful!</h1>
                            <p>You can close this window and return to your application.</p>
                        </div>
                    </body>
                    </html>"#
                            .to_string(),
                    )
                }
            }
        };

        // Build router
        let callback_path = url::Url::parse(&self.config.callback_url)
            .map_err(|_| AuthError::InvalidCallbackUrl("Invalid URL".into()))?;
        let app = Router::new().route(callback_path.path(), get(callback_handler));

        // Parse callback URL to get port
        let callback_url = url::Url::parse(&self.config.callback_url)
            .map_err(|e| AuthError::InvalidCallbackUrl(e.to_string()))?;

        let port = callback_url.port().unwrap_or(8080);

        // Determine bind address based on security configuration
        let addr = if self.config.allow_external_callback {
            warn!(
                "⚠️  SECURITY: OAuth callback server binding to 0.0.0.0:{} \
                 (all network interfaces). This exposes the callback to your LAN/WAN — \
                 only enable when terminating TLS in front of this process \
                 (ngrok / cloudflared / reverse proxy).",
                port
            );
            std::net::SocketAddr::from(([0, 0, 0, 0], port))
        } else {
            // Secure default: bind to localhost only
            info!(
                "OAuth callback server binding to 127.0.0.1:{} (localhost only)",
                port
            );
            std::net::SocketAddr::from(([127, 0, 0, 1], port))
        };

        if self.config.allow_external_callback {
            // A TLS-terminating tunnel / reverse proxy sits in front of
            // this process and forwards plain HTTP — serve HTTP here.
            let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
                AuthError::OAuthFlow(format!("Failed to bind to port {}: {}", port, e))
            })?;
            let server = axum::serve(listener, app);

            tokio::select! {
                code = rx => {
                    code.map_err(|_| {
                        AuthError::OAuthFlow("Failed to receive authorization code".to_string())
                    })
                }
                result = server => {
                    Err(AuthError::OAuthFlow(format!(
                        "Callback server stopped unexpectedly: {result:?}"
                    )))
                }
            }
        } else {
            // Schwab requires an https:// callback URL, so the browser
            // opens a TLS connection straight to this server — a plain
            // HTTP listener would fail the handshake and the code would
            // never arrive. Generate an in-memory self-signed certificate
            // for loopback; the browser shows a one-time warning the user
            // can bypass.
            let cert = rcgen::generate_simple_self_signed(vec![
                "127.0.0.1".to_string(),
                "localhost".to_string(),
            ])
            .map_err(|e| {
                AuthError::OAuthFlow(format!("Failed to generate TLS certificate: {e}"))
            })?;

            let cert_der = cert.cert.der().to_vec();
            let key_der = cert.key_pair.serialize_der();

            // Pick the ring provider explicitly: both ring and aws-lc-rs
            // are in the dependency tree, so rustls cannot auto-select one.
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let server_config = rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| AuthError::OAuthFlow(format!("TLS config error: {e}")))?
                .with_no_client_auth()
                .with_single_cert(
                    vec![rustls::pki_types::CertificateDer::from(cert_der)],
                    rustls::pki_types::PrivateKeyDer::Pkcs8(key_der.into()),
                )
                .map_err(|e| AuthError::OAuthFlow(format!("TLS config error: {e}")))?;

            let rustls_config =
                axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(server_config));

            info!(
                "Callback server uses a self-signed certificate — your browser will warn once; \
                 choose 'Advanced → Proceed' to complete the flow"
            );

            let server =
                axum_server::bind_rustls(addr, rustls_config).serve(app.into_make_service());

            tokio::select! {
                code = rx => {
                    code.map_err(|_| {
                        AuthError::OAuthFlow("Failed to receive authorization code".to_string())
                    })
                }
                result = server => {
                    Err(AuthError::OAuthFlow(format!(
                        "Callback server stopped unexpectedly: {result:?}"
                    )))
                }
            }
        }
    }

    #[cfg(not(feature = "callback-server"))]
    async fn capture_callback_code(&self) -> AuthResult<String> {
        Err(AuthError::OAuthFlow(
            "Callback server not enabled. Enable the 'callback-server' feature \
             or manually copy the authorization code."
                .to_string(),
        ))
    }

    fn save_tokens(&self) -> AuthResult<()> {
        if let Some(tokens) = self.tokens.read().as_ref() {
            self.token_store.save(tokens)?;
            debug!("Tokens saved");
        }
        Ok(())
    }

    fn load_tokens(&self) -> AuthResult<TokenSet> {
        let tokens = self.token_store.load()?;
        debug!("Tokens loaded");
        Ok(tokens)
    }

    pub fn has_valid_tokens(&self) -> bool {
        self.tokens
            .read()
            .as_ref()
            .map(|t| t.is_valid())
            .unwrap_or(false)
    }

    /// Get the underlying HTTP client (for use by Client)
    pub fn get_http_client(&self) -> Arc<RwLock<reqwest::Client>> {
        self.client.clone()
    }

    fn generate_code_verifier() -> AuthResult<String> {
        // 43-128 chars, use 32 bytes random then base64url encode without padding
        let rng = ring_rand::SystemRandom::new();
        let mut bytes = [0u8; 32];
        rng.fill(&mut bytes).map_err(|_| {
            AuthError::OAuthFlow(
                "Failed to generate PKCE verifier - cryptographic RNG unavailable".to_string(),
            )
        })?;
        Ok(BASE64_URL.encode(bytes))
    }

    fn code_challenge_s256(verifier: &str) -> String {
        let digest = digest::digest(&digest::SHA256, verifier.as_bytes());
        BASE64_URL.encode(digest.as_ref())
    }
}

impl Drop for AuthManager {
    fn drop(&mut self) {
        // `AuthManager` is `Clone` and every clone shares this Arc.
        // Only the *last* clone may abort the auto-refresh task —
        // previously the first dropped clone (e.g. one handed to a
        // short-lived `StreamClient`) silently killed token refresh
        // for the whole process.
        if Arc::strong_count(&self.refresh_task) == 1
            && let Some(task) = self.refresh_task.write().take()
        {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_creds(key_len: usize, secret_len: usize) -> OAuthConfig {
        OAuthConfig {
            app_key: "k".repeat(key_len),
            app_secret: "s".repeat(secret_len),
            callback_url: "https://127.0.0.1:8080".to_string(),
            // Avoid touching the real keychain/token files in unit tests
            token_store_kind: TokenStoreKind::File,
            tokens_file: std::env::temp_dir().join("schwab_auth_validation_test.json"),
            ..Default::default()
        }
    }

    /// Schwab issues several credential length variants; since Schwabdev
    /// 3.0.4 the accepted rule is "both even-length, combined ≥ 32".
    #[test]
    fn credential_validation_matches_upstream_rule() {
        // Classic 32/16 pair
        assert!(AuthManager::new(config_with_creds(32, 16)).is_ok());
        // Newer longer variants
        assert!(AuthManager::new(config_with_creds(48, 64)).is_ok());
        // Unusual but even + combined ≥ 32 — accepted under the relaxed rule
        assert!(AuthManager::new(config_with_creds(28, 8)).is_ok());

        // Odd lengths rejected
        assert!(AuthManager::new(config_with_creds(31, 16)).is_err());
        assert!(AuthManager::new(config_with_creds(32, 15)).is_err());
        // Combined too short
        assert!(AuthManager::new(config_with_creds(16, 14)).is_err());
    }
}

#[cfg(test)]
mod callback_regressions {
    use super::*;
    #[tokio::test]
    async fn callback_state_is_required_expires_and_is_single_use() {
        let dir = tempfile::tempdir().unwrap();
        let manager = AuthManager::new(OAuthConfig {
            app_key: "k".repeat(32),
            app_secret: "s".repeat(16),
            token_store_kind: TokenStoreKind::File,
            tokens_file: dir.path().join("tokens.json"),
            capture_callback: false,
            auto_refresh: false,
            ..Default::default()
        })
        .unwrap();
        let (url, _) = manager.authorize().await.unwrap();
        let state = url::Url::parse(&url)
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "state")
            .unwrap()
            .1
            .into_owned();
        assert!(manager.consume_callback_state(None).is_err());
        assert!(manager.consume_callback_state(Some("wrong")).is_err());
        manager.consume_callback_state(Some(&state)).unwrap();
        assert!(manager.consume_callback_state(Some(&state)).is_err());
        *manager.oauth_state.write() = Some((
            state.clone(),
            std::time::Instant::now() - std::time::Duration::from_secs(301),
        ));
        assert!(manager.consume_callback_state(Some(&state)).is_err());
    }
}
