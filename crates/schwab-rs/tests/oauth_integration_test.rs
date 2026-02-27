//! Integration tests for OAuth2 flow using wiremock
//!
//! Tests the complete OAuth2 authentication flow including:
//! - Authorization URL generation
//! - Token exchange (code -> access token)
//! - Token refresh
//! - Error handling

use schwab_rs::auth::{AuthManager, PkceSession};
use schwab_rs::config::{SchwabConfig, ClientConfig, RetryConfig, RateLimitConfig, CircuitBreakerConfig};
use schwab_rs::auth::OAuthConfig;
use schwab_rs::auth::TokenStoreKind;
use std::time::Duration;
use std::path::PathBuf;
use wiremock::{MockServer, Mock, ResponseTemplate};
use wiremock::matchers::{method, path, header, body_string_contains};

/// Helper to create test OAuth config pointing to mock server
fn create_test_config(mock_server_uri: &str) -> SchwabConfig {
    SchwabConfig {
        oauth: OAuthConfig {
            app_key: "12345678901234567890123456789012".to_string(), // 32 chars (valid length)
            app_secret: "1234567890123456".to_string(), // 16 chars (valid length)
            callback_url: "https://127.0.0.1:8080/callback".to_string(),
            auth_url: format!("{}/oauth/authorize", mock_server_uri),
            token_url: format!("{}/oauth/token", mock_server_uri),
            tokens_file: PathBuf::from("/tmp/test_tokens.json"),
            capture_callback: false,
            auto_refresh: false,
            refresh_buffer_seconds: 60,
            pkce_enabled: true,
            token_store_kind: TokenStoreKind::File,
            allow_external_callback: false,
            on_token_notification: None,
        },
        client: ClientConfig {
            base_url: mock_server_uri.to_string(),
            timeout: Duration::from_secs(10),
            retry: RetryConfig::default(),
            rate_limit: RateLimitConfig::default(),
            circuit_breaker: CircuitBreakerConfig::default(),
            user_agent: "schwab-rs-test/0.1.0".to_string(),
        },
        streaming: None,
    }
}

#[tokio::test]
async fn test_oauth_authorize_url_generation() {
    let mock_server = MockServer::start().await;
    let config = create_test_config(&mock_server.uri());

    let auth_manager = AuthManager::new(config.oauth).expect("Failed to create AuthManager");
    let (auth_url, _state) = auth_manager.authorize().await.expect("Failed to generate authorize URL");

    // Verify URL structure
    assert!(auth_url.starts_with(&format!("{}/oauth/authorize", mock_server.uri())));
    assert!(auth_url.contains("client_id=12345678901234567890123456789012"));
    assert!(auth_url.contains("redirect_uri="));
    assert!(auth_url.contains("state="), "URL should contain CSRF state parameter");
}

#[tokio::test]
async fn test_oauth_authorize_url_with_pkce() {
    let mock_server = MockServer::start().await;
    let config = create_test_config(&mock_server.uri());

    let auth_manager = AuthManager::new(config.oauth).expect("Failed to create AuthManager");
    let (auth_url, _state) = auth_manager.authorize().await.expect("Failed to generate authorize URL");

    // Verify PKCE parameters are present
    assert!(auth_url.contains("code_challenge="));
    assert!(auth_url.contains("code_challenge_method=S256"));
}

#[tokio::test]
async fn test_token_exchange_success() {
    let mock_server = MockServer::start().await;

    // Mock successful token response
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(header("content-type", "application/x-www-form-urlencoded"))
        .and(body_string_contains("grant_type=authorization_code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "test_access_token_12345",
            "refresh_token": "test_refresh_token_67890",
            "token_type": "Bearer",
            "expires_in": 1800,
            "scope": "api"
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let config = create_test_config(&mock_server.uri());
    let auth_manager = AuthManager::new(config.oauth).expect("Failed to create AuthManager");

    // Generate authorization to set up PKCE
    let (_auth_url, _state) = auth_manager.authorize().await.expect("Failed to authorize");

    // Exchange authorization code for tokens
    let result = auth_manager.exchange_code("test_auth_code_abc".to_string()).await;

    // Should succeed with mocked response
    assert!(result.is_ok(), "Token exchange should succeed with valid mock");
}

#[tokio::test]
async fn test_token_exchange_invalid_code() {
    let mock_server = MockServer::start().await;

    // Mock error response for invalid authorization code
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_grant",
            "error_description": "Invalid authorization code"
        })))
        .expect(1..)
        .mount(&mock_server)
        .await;

    let config = create_test_config(&mock_server.uri());
    let auth_manager = AuthManager::new(config.oauth).expect("Failed to create AuthManager");

    let (_auth_url, _state) = auth_manager.authorize().await.expect("Failed to authorize");

    let result = auth_manager.exchange_code("invalid_code".to_string()).await;

    // Should fail due to invalid code
    assert!(result.is_err(), "Token exchange should fail with invalid code");
}

// NOTE: Token refresh tests would require setting up existing tokens first
// These are placeholder tests demonstrating the mock setup pattern

#[tokio::test]
#[ignore] // Ignore until token persistence is fully tested
async fn test_token_refresh_success() {
    let _mock_server = MockServer::start().await;
    // Mock successful token refresh response would go here
    // Full integration test would require existing tokens
}

#[tokio::test]
#[ignore] // Ignore until token persistence is fully tested
async fn test_token_refresh_expired_refresh_token() {
    let _mock_server = MockServer::start().await;
    // Mock error response for expired refresh token would go here
}

#[tokio::test]
async fn test_pkce_verifier_generation() {
    let session = PkceSession::new();

    let result = session.generate();
    assert!(result.is_ok());

    let (verifier, challenge) = result.unwrap();

    // Verify RFC 7636 compliance
    assert!(verifier.len() >= 43, "Verifier must be at least 43 characters");
    assert!(verifier.len() <= 128, "Verifier must not exceed 128 characters");
    assert_eq!(challenge.len(), 43, "S256 challenge should be 43 characters");

    // Verify base64url encoding (no +, /, or =)
    assert!(!verifier.contains('+'));
    assert!(!verifier.contains('/'));
    assert!(!challenge.contains('+'));
    assert!(!challenge.contains('/'));
}

#[tokio::test]
async fn test_oauth_state_parameter_generated() {
    let mock_server = MockServer::start().await;
    let config = create_test_config(&mock_server.uri());

    let auth_manager = AuthManager::new(config.oauth).expect("Failed to create AuthManager");
    let (auth_url, _state) = auth_manager.authorize().await.expect("Failed to authorize");

    // Verify state parameter is present for CSRF protection
    assert!(auth_url.contains("state="), "URL should contain state parameter");

    // Extract state parameter from URL
    let state_start = auth_url.find("state=").unwrap() + 6;
    let state_end = auth_url[state_start..].find('&').unwrap_or(auth_url[state_start..].len());
    let extracted_state = &auth_url[state_start..state_start + state_end];

    // State should be a UUID (36 chars including hyphens)
    assert_eq!(extracted_state.len(), 36, "State should be UUID format (got {})", extracted_state);
}

#[tokio::test]
#[ignore] // This test demonstrates mock setup but doesn't call actual code
async fn test_authorization_header_format() {
    let _mock_server = MockServer::start().await;
    // Test would validate that Basic auth header is properly formatted
    // Requires actual token exchange call to verify header
}
