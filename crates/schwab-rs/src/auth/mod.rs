//! OAuth 2.0 authentication with PKCE and secure token storage.
//!
//! Provides complete OAuth flow implementation with automatic token refresh.

#![allow(missing_docs)] // Internal auth implementation

// Token storage module with secure backends
mod token_store;

// HTTP authorization header utilities
mod headers;

// PKCE (Proof Key for Public Clients) implementation
mod pkce;

// Re-export token storage types for public API
pub use token_store::{
    TokenSet, TokenStore,
    FileTokenStore, EncryptedFileTokenStore, KeychainTokenStore,
};

// Re-export header utilities (used internally, but might be useful publicly)
pub use headers::{create_bearer_header, create_basic_header};

// Re-export PKCE types for advanced usage
pub use pkce::PkceSession;

use crate::error::AuthError;
use chrono::{DateTime, Duration, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use base64::{engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD as BASE64_URL}, Engine};
use ring::{digest, rand as ring_rand};
use ring::rand::SecureRandom;

#[cfg(feature = "callback-server")]
use axum::{extract::Query, response::Html, routing::get, Router};

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

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
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
    /// Enable PKCE (S256). Defaults to true for OAuth 2.1 compliance.
    #[serde(default = "default_true")]
    pub pkce_enabled: bool,
    /// Token storage backend
    #[serde(default)]
    pub token_store_kind: TokenStoreKind,
    /// Allow callback server to bind to 0.0.0.0 (all interfaces).
    /// **Security Warning:** Only enable for cloudflared tunneling.
    /// Default: false (binds to localhost only)
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
        log::debug!("Defaulting to Keychain storage on macOS");
        TokenStoreKind::Keychain
    }
}

#[cfg(not(target_os = "macos"))]
impl Default for TokenStoreKind {
    fn default() -> Self {
        log::debug!("Defaulting to EncryptedFile storage");
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
            .field("on_token_notification", &self.on_token_notification.is_some())
            .finish()
    }
}

fn default_true() -> bool {
    true
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
            pkce_enabled: true, // OAuth 2.1 compliant default
            token_store_kind: TokenStoreKind::File,
            allow_external_callback: false, // Secure default: localhost only
            on_token_notification: None,
        }
    }
}

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
    /// Track if we need to recreate the HTTP client after token refresh
    needs_client_recreation: Arc<RwLock<bool>>,
    /// Optional PKCE verifier for the current auth session
    pkce_verifier: Arc<RwLock<Option<String>>>,
    /// OAuth state parameter for CSRF protection (RFC 6749 Section 10.12)
    oauth_state: Arc<RwLock<Option<String>>>,
    /// Pluggable token store
    token_store: Arc<dyn TokenStore + Send + Sync>,
}

impl std::fmt::Debug for AuthManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthManager")
            .field("config", &self.config)
            .field("tokens", &"<tokens>")
            .field("client", &"<client>")
            .field("refresh_lock", &"<mutex>")
            .field("refresh_task", &self.refresh_task.read().is_some())
            .field("needs_client_recreation", &*self.needs_client_recreation.read())
            .field("pkce", &self.config.pkce_enabled)
            .finish()
    }
}

// TokenStore trait and implementations are now provided by token_store module

impl AuthManager {
    pub fn new(config: OAuthConfig) -> AuthResult<Self> {
        // Validate app key and secret lengths per Schwab requirements (matching Python reference)
        if config.app_key.is_empty() {
            return Err(AuthError::MissingConfig(
                "App key cannot be empty".to_string()
            ));
        }
        if config.app_secret.is_empty() {
            return Err(AuthError::MissingConfig(
                "App secret cannot be empty".to_string()
            ));
        }
        
        let key_len_ok = matches!(config.app_key.len(), 32 | 48);
        let secret_len_ok = matches!(config.app_secret.len(), 16 | 64);
        if !key_len_ok || !secret_len_ok {
            return Err(AuthError::MissingConfig(
                format!(
                    "App key length must be 32 or 48 (got {}), and app secret length must be 16 or 64 (got {})",
                    config.app_key.len(),
                    config.app_secret.len()
                )
            ));
        }
        
        // Validate callback URL (must be HTTPS and no trailing slash) - matching Python
        if !config.callback_url.starts_with("https") {
            return Err(AuthError::MissingConfig(
                "Callback URL must be HTTPS".to_string()
            ));
        }
        if config.callback_url.ends_with('/') {
            return Err(AuthError::MissingConfig(
                "Callback URL cannot be path (ends with '/')".to_string()
            ));
        }

        let manager = Self {
            config: config.clone(),
            tokens: Arc::new(RwLock::new(None)),
            client: Arc::new(RwLock::new(
                reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10)) // 10 second timeout
                    .build()
                    .map_err(|e| AuthError::OAuthFlow(e.to_string()))?
            )),
            refresh_lock: Arc::new(Mutex::new(())),
            refresh_task: Arc::new(RwLock::new(None)),
            needs_client_recreation: Arc::new(RwLock::new(false)),
            pkce_verifier: Arc::new(RwLock::new(None)),
            oauth_state: Arc::new(RwLock::new(None)),
            token_store: match config.token_store_kind {
                TokenStoreKind::File => {
                    log::info!("Using FileTokenStore (basic security: 0600 permissions only)");
                    Arc::new(FileTokenStore::new(config.tokens_file.clone()))
                }
                TokenStoreKind::EncryptedFile => {
                    log::info!("Using EncryptedFileTokenStore (ChaCha20Poly1305 encryption + 0600 permissions)");
                    Arc::new(EncryptedFileTokenStore::new(config.tokens_file.clone()))
                }
                TokenStoreKind::Keychain => {
                    log::info!("Using KeychainTokenStore (OS-native credential storage)");
                    Arc::new(KeychainTokenStore::new("schwab-rs".to_string(), config.app_key.clone()))
                }
            },
        };

        // Try to load existing tokens
        match manager.load_tokens() {
            Ok(tokens) => {
                *manager.tokens.write() = Some(tokens);
            }
            Err(e) => {
                warn!("Could not load tokens: {}. Will need to authenticate.", e);
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

        // Start auto-refresh task if enabled
        if self.config.auto_refresh {
            self.start_refresh_task().await;
        }

        Ok(())
    }

    pub async fn get_access_token(&self) -> AuthResult<String> {
        // Check if we have valid tokens
        {
            let token_guard = self.tokens.read();
            if let Some(tokens) = token_guard.as_ref() {
                if tokens.is_valid() && !tokens.needs_refresh(self.config.refresh_buffer_seconds) {
                    return tokens.access_token().map_err(|e| e.into());
                }
            }
        } // Drop lock before await

        // Need to refresh or get new tokens
        self.ensure_valid_tokens().await?;

        self.tokens
            .read()
            .as_ref()
            .ok_or(AuthError::TokenExpired)?
            .access_token()
            .map_err(|e| e.into())
    }

    pub async fn ensure_valid_tokens(&self) -> AuthResult<()> {
        let _lock = self.refresh_lock.lock().await;

        // Check if we need to recreate the HTTP client
        if *self.needs_client_recreation.read() {
            self.recreate_http_client().await?;
        }

        // Double-check after acquiring lock and extract needed data
        let (should_refresh, refresh_token, _is_expired) = {
            let token_guard = self.tokens.read();
            if let Some(tokens) = token_guard.as_ref() {
                if tokens.is_valid() && !tokens.needs_refresh(self.config.refresh_buffer_seconds) {
                    return Ok(());
                }
                
                // Check refresh token expiry and notify user
                let refresh_remaining = if let Some(expires) = tokens.refresh_token_expires_at {
                    (expires - Utc::now()).num_seconds()
                } else {
                    (tokens.issued_at + Duration::days(7) - Utc::now()).num_seconds()
                };
                
                // Notify if refresh token expires soon
                if let Some(ref notify) = self.config.on_token_notification {
                    if refresh_remaining < 3600 * 12 { // Less than 12 hours
                        notify(TokenNotification::RefreshTokenExpiring {
                            hours_remaining: (refresh_remaining / 3600).max(0),
                        });
                    }
                }
                
                // Check if refresh token is expired (7 days)
                if tokens.refresh_token_expired() {
                    warn!("Refresh token expired after 7 days, need full OAuth flow");
                    if let Some(ref notify) = self.config.on_token_notification {
                        notify(TokenNotification::RefreshTokenExpired);
                    }
                    return Err(AuthError::TokenExpired);
                }
                
                let refresh_token_str = tokens.refresh_token().unwrap_or_default();
                let needs_refresh = !refresh_token_str.is_empty();
                (needs_refresh, refresh_token_str, false)
            } else {
                (false, String::new(), false)
            }
        }; // Drop read lock here

        // Try to refresh if needed
        if should_refresh && !refresh_token.is_empty() {
            match self.refresh_tokens(&refresh_token).await {
                Ok(new_tokens) => {
                    *self.tokens.write() = Some(new_tokens);
                    self.save_tokens()?;
                    // Mark that we need to recreate HTTP client
                    *self.needs_client_recreation.write() = true;
                    return Ok(());
                }
                Err(e) => {
                    warn!("Token refresh failed: {}", e);
                    // Per docs, certain errors require OAuth restart
                    return Err(AuthError::TokenExpired);
                }
            }
        }

        // Need to do full OAuth flow
        Err(AuthError::TokenExpired)
    }

    pub async fn authorize(&self) -> AuthResult<(String, String)> {
        // Generate OAuth state parameter for CSRF protection (RFC 6749 Section 10.12)
        // Using UUID v4 provides sufficient entropy for secure state tokens
        let state = uuid::Uuid::new_v4().to_string();
        *self.oauth_state.write() = Some(state.clone());
        debug!("Generated OAuth state parameter for CSRF protection");

        // Build auth URL with state parameter
        let auth_url = if self.config.pkce_enabled {
            // Generate PKCE code_verifier and code_challenge (S256)
            let verifier = Self::generate_code_verifier()?;
            let challenge = Self::code_challenge_s256(&verifier);
            *self.pkce_verifier.write() = Some(verifier);
            format!(
                "{}?client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
                self.config.auth_url,
                self.config.app_key,
                self.config.callback_url,
                challenge,
                state
            )
        } else {
            format!(
                "{}?client_id={}&redirect_uri={}&state={}",
                self.config.auth_url,
                self.config.app_key,
                self.config.callback_url,
                state
            )
        };

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
                "Authorization code is empty".to_string()
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
        if !code_without_at.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
            return Err(AuthError::OAuthFlow(
                format!("Invalid authorization code format: contains invalid characters")
            ));
        }

        if code_without_at.len() < 10 {
            return Err(AuthError::OAuthFlow(
                "Authorization code too short - may be truncated or invalid".to_string()
            ));
        }

        self.exchange_code_for_tokens(&extracted_code).await
    }

    fn extract_code_from_callback_url(&self, url: &str) -> AuthResult<String> {
        // Parse URL to extract code and state parameters
        let parsed = url::Url::parse(url)
            .map_err(|e| AuthError::InvalidCallbackUrl(
                format!("Invalid callback URL: {}", e)
            ))?;

        // Extract state parameter and validate CSRF token (RFC 6749 Section 10.12)
        let callback_state = parsed
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned());

        if let Some(callback_state) = callback_state {
            let expected_state = self.oauth_state.read().clone();
            match expected_state {
                Some(expected) => {
                    if callback_state != expected {
                        warn!("OAuth state mismatch - possible CSRF attack");
                        return Err(AuthError::OAuthFlow(
                            "OAuth state parameter mismatch - possible CSRF attack".to_string()
                        ));
                    }
                    debug!("OAuth state parameter validated successfully");
                }
                None => {
                    warn!("Callback received state parameter but no state was generated");
                    return Err(AuthError::OAuthFlow(
                        "Unexpected state parameter in callback".to_string()
                    ));
                }
            }
        } else {
            // State parameter should be present if we generated one
            if self.oauth_state.read().is_some() {
                warn!("Callback missing state parameter - possible CSRF attack");
                return Err(AuthError::OAuthFlow(
                    "OAuth callback missing required state parameter".to_string()
                ));
            }
        }

        // Extract code from query parameters
        let code = parsed
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.into_owned())
            .ok_or_else(|| AuthError::OAuthFlow(
                "Callback URL missing 'code' parameter".to_string()
            ))?;

        // URL-decode and add @ suffix if needed
        // Schwab returns codes ending with %40 (URL-encoded @)
        let decoded = if code.ends_with("%40") {
            format!("{}@", &code[..code.len()-3])
        } else if !code.ends_with('@') {
            format!("{}@", code)
        } else {
            code
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

        // Create basic auth header using utility function
        let auth_header = create_basic_header(&self.config.app_key, &self.config.app_secret);
        
        debug!("Exchanging code for tokens");
        debug!("Token URL: {}", self.config.token_url);
        debug!("Redirect URI: {}", self.config.callback_url);
        debug!("Code length: {}, ends with @: {}",
               code_with_at.len(),
               code_with_at.ends_with('@'));
        debug!("App Key length: {}, Secret length: {}",
               self.config.app_key.len(),
               self.config.app_secret.len());
        
        let mut form_params: Vec<(&str, String)> = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code_with_at.clone()),
            ("redirect_uri", self.config.callback_url.clone()),
        ];
        if self.config.pkce_enabled {
            // CRITICAL: Take (consume) the verifier to enforce single-use per RFC 7636
            // This prevents accidental reuse and ensures spec compliance
            let verifier = self.pkce_verifier.write().take()
                .ok_or_else(|| AuthError::OAuthFlow(
                    "PKCE enabled but no code verifier found. Call authorize() first.".to_string()
                ))?;
            form_params.push(("code_verifier", verifier));
            // Verifier is now consumed and cannot be reused
        }
        let response = self
            .client
            .read()
            .post(&self.config.token_url)
            .header("Authorization", auth_header)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(&form_params)
            .send()
            .await
            .map_err(|e| AuthError::OAuthFlow(e.to_string()))?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(AuthError::OAuthFlow(format!(
                "Token exchange failed: {}",
                error_text
            )));
        }

        let token_response: TokenResponse = response
            .json()
            .await
            .map_err(|e| AuthError::OAuthFlow(e.to_string()))?;

        let tokens = token_response.into_token_set(Utc::now());
        
        *self.tokens.write() = Some(tokens.clone());
        self.save_tokens()?;

        Ok(tokens)
    }


    async fn refresh_tokens(&self, refresh_token: &str) -> AuthResult<TokenSet> {
        debug!("Refreshing access token");

        // Create basic auth header using utility function
        let auth_header = create_basic_header(&self.config.app_key, &self.config.app_secret);

        let client = self.client.read().clone();
        let response = client
            .post(&self.config.token_url)
            .header("Authorization", auth_header)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await
            .map_err(|e| AuthError::RefreshFailed(e.to_string()))?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(AuthError::RefreshFailed(error_text));
        }

        let mut token_response: TokenResponse = response
            .json()
            .await
            .map_err(|e| AuthError::RefreshFailed(e.to_string()))?;

        // Keep the same refresh token if not provided
        if token_response.refresh_token.is_none() {
            token_response.refresh_token = Some(refresh_token.to_string());
        }

        let tokens = token_response.into_token_set(Utc::now());

        info!("Token refreshed successfully, expires at: {}", tokens.expires_at);

        Ok(tokens)
    }

    async fn start_refresh_task(&self) {
        let tokens = self.tokens.clone();
        let config = self.config.clone();
        let client = self.client.clone();
        let refresh_lock = self.refresh_lock.clone();
        let needs_recreation = self.needs_client_recreation.clone();

        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30)); // Check every 30 seconds
            
            loop {
                interval.tick().await;
                
                // Check tokens and determine if refresh is needed
                let (needs_refresh, refresh_token) = {
                    let token_guard = tokens.read();
                    if let Some(token_set) = token_guard.as_ref() {
                        // Check for refresh token expiry warnings
                        if let Some(expires) = token_set.refresh_token_expires_at {
                            let remaining = (expires - Utc::now()).num_seconds();
                            
                            // Notify at specific intervals
                            if remaining > 30 && remaining <= 43200 { // 12 hours
                                if remaining % 3600 <= 30 { // Every hour
                                    if let Some(ref notify) = config.on_token_notification {
                                        notify(TokenNotification::RefreshTokenExpiring {
                                            hours_remaining: (remaining / 3600).max(0),
                                        });
                                    }
                                }
                            }
                        }
                        
                        // Check if access token needs refresh
                        let needs_refresh = token_set.needs_refresh(config.refresh_buffer_seconds);
                        (needs_refresh, token_set.refresh_token().unwrap_or_default())
                    } else {
                        (false, String::new())
                    }
                }; // Drop read lock here
                
                if needs_refresh && !refresh_token.is_empty() {
                    let _lock = refresh_lock.lock().await;
                    
                    // Double-check after acquiring lock
                    let still_needs_refresh = tokens.read().as_ref()
                        .map(|t| t.needs_refresh(config.refresh_buffer_seconds))
                        .unwrap_or(false);
                    
                    if !still_needs_refresh {
                        continue;
                    }
                    
                    // Notify about access token expiring
                    if let Some(ref notify) = config.on_token_notification {
                        let remaining = tokens.read().as_ref()
                            .map(|t| (t.expires_at - Utc::now()).num_seconds())
                            .unwrap_or(0);
                        notify(TokenNotification::AccessTokenExpiring {
                            seconds_remaining: remaining.max(0),
                        });
                    }
                    
                    debug!("Auto-refreshing token");
                    
                    // Perform refresh
                    let refresh_result = Self::refresh_tokens_static(
                        &client,
                        &config,
                        &refresh_token,
                    )
                    .await;
                    
                    match refresh_result {
                        Ok(new_tokens) => {
                            *tokens.write() = Some(new_tokens);
                            *needs_recreation.write() = true; // Mark for client recreation
                            info!("Token auto-refreshed successfully");
                        }
                        Err(e) => {
                            warn!("Auto-refresh failed: {}", e);
                        }
                    }
                }
            }
        });

        *self.refresh_task.write() = Some(task);
    }

    async fn refresh_tokens_static(
        client: &Arc<RwLock<reqwest::Client>>,
        config: &OAuthConfig,
        refresh_token: &str,
    ) -> AuthResult<TokenSet> {
        // Create basic auth header - matching Python implementation
        let auth_string = format!("{}:{}", config.app_key, config.app_secret);
        let auth_bytes = auth_string.as_bytes();
        let auth_header = format!("Basic {}", BASE64.encode(auth_bytes));
        
        let http_client = client.read().clone();
        let response = http_client
            .post(&config.token_url)
            .header("Authorization", auth_header)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await
            .map_err(|e| AuthError::RefreshFailed(e.to_string()))?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(AuthError::RefreshFailed(error_text));
        }

        let mut token_response: TokenResponse = response
            .json()
            .await
            .map_err(|e| AuthError::RefreshFailed(e.to_string()))?;

        if token_response.refresh_token.is_none() {
            token_response.refresh_token = Some(refresh_token.to_string());
        }

        Ok(token_response.into_token_set(Utc::now()))
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
            move |Query(params): Query<CallbackParams>| async move {
                // Extract code and add @ like Python reference does
                let code_with_at = if !params.code.ends_with('@') {
                    format!("{}@", params.code)
                } else {
                    params.code.clone()
                };
                
                if let Some(tx) = tx.lock().await.take() {
                    let _ = tx.send(code_with_at);
                }
                
                Html(format!(
                    r#"<!DOCTYPE html>
                    <html>
                    <head>
                        <title>Authorization Successful</title>
                        <style>
                            body {{
                                font-family: system-ui, -apple-system, sans-serif;
                                display: flex;
                                justify-content: center;
                                align-items: center;
                                height: 100vh;
                                margin: 0;
                                background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
                            }}
                            .container {{
                                background: white;
                                padding: 2rem;
                                border-radius: 10px;
                                box-shadow: 0 10px 40px rgba(0,0,0,0.2);
                                text-align: center;
                            }}
                            .success {{ color: #48bb78; font-size: 3rem; }}
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
                ))
            }
        };
        
        // Build router
        let app = Router::new().route("/", get(callback_handler));
        
        // Parse callback URL to get port
        let callback_url = url::Url::parse(&self.config.callback_url)
            .map_err(|e| AuthError::InvalidCallbackUrl(e.to_string()))?;
        
        let port = callback_url.port().unwrap_or(8080);

        // Determine bind address based on security configuration
        let addr = if self.config.allow_external_callback {
            warn!(
                "⚠️  SECURITY: OAuth callback server binding to 0.0.0.0:{} (all network interfaces). \
                 This exposes the callback to your LAN/WAN. Only enable for cloudflared tunneling!",
                port
            );
            std::net::SocketAddr::from(([0, 0, 0, 0], port))
        } else {
            // Secure default: bind to localhost only
            info!("OAuth callback server binding to 127.0.0.1:{} (localhost only)", port);
            std::net::SocketAddr::from(([127, 0, 0, 1], port))
        };

        // Start server
        let listener = tokio::net::TcpListener::bind(addr).await
            .map_err(|e| AuthError::OAuthFlow(format!("Failed to bind to port {}: {}", port, e)))?;
        
        // Run server and wait for callback
        let server = axum::serve(listener, app);
        
        tokio::select! {
            code = rx => {
                code.map_err(|_| AuthError::OAuthFlow("Failed to receive authorization code".to_string()))
            }
            _ = server => {
                Err(AuthError::OAuthFlow("Server stopped unexpectedly".to_string()))
            }
        }
    }
    
    #[cfg(not(feature = "callback-server"))]
    async fn capture_callback_code(&self) -> AuthResult<String> {
        Err(AuthError::OAuthFlow(
            "Callback server not enabled. Enable the 'callback-server' feature or manually copy the authorization code.".to_string()
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
    
    /// Recreate the HTTP client after token refresh
    async fn recreate_http_client(&self) -> AuthResult<()> {
        debug!("Recreating HTTP client after token refresh");
        
        let new_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| AuthError::OAuthFlow(e.to_string()))?;
        
        *self.client.write() = new_client;
        *self.needs_client_recreation.write() = false;
        
        if let Some(ref notify) = self.config.on_token_notification {
            notify(TokenNotification::SessionRecreated);
        }
        
        Ok(())
    }
    
    /// Get the underlying HTTP client (for use by Client)
    pub fn get_http_client(&self) -> Arc<RwLock<reqwest::Client>> {
        self.client.clone()
    }

    fn generate_code_verifier() -> AuthResult<String> {
        // 43-128 chars, use 32 bytes random then base64url encode without padding
        let rng = ring_rand::SystemRandom::new();
        let mut bytes = [0u8; 32];
        rng.fill(&mut bytes)
            .map_err(|_| AuthError::OAuthFlow(
                "Failed to generate PKCE verifier - cryptographic RNG unavailable".to_string()
            ))?;
        Ok(BASE64_URL.encode(bytes))
    }

    fn code_challenge_s256(verifier: &str) -> String {
        let digest = digest::digest(&digest::SHA256, verifier.as_bytes());
        BASE64_URL.encode(digest.as_ref())
    }
}

impl Drop for AuthManager {
    fn drop(&mut self) {
        if let Some(task) = self.refresh_task.write().take() {
            task.abort();
        }
    }
}