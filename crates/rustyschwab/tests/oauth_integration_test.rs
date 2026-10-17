//! Integration tests for OAuth2 flow using wiremock
//!
//! Tests the complete OAuth2 authentication flow including:
//! - Authorization URL generation
//! - Token exchange (code -> access token)
//! - Token refresh
//! - Error handling

use rustyschwab::auth::AuthManager;
use rustyschwab::auth::OAuthConfig;
use rustyschwab::auth::TokenStoreKind;
use rustyschwab::config::{
    CircuitBreakerConfig, ClientConfig, RateLimitConfig, RetryConfig, SchwabConfig,
};
use std::time::Duration;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Helper to create test OAuth config pointing to mock server
fn create_test_config(mock_server_uri: &str) -> SchwabConfig {
    SchwabConfig {
        oauth: OAuthConfig {
            app_key: "12345678901234567890123456789012".to_string(), // 32 chars (valid length)
            app_secret: "1234567890123456".to_string(),              // 16 chars (valid length)
            callback_url: "https://127.0.0.1:8080/callback".to_string(),
            auth_url: format!("{}/oauth/authorize", mock_server_uri),
            token_url: format!("{}/oauth/token", mock_server_uri),
            // Under target/, not the system temp directory, which no one cleans.
            tokens_file: std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
                .join(format!("oauth-test-{}.json", uuid::Uuid::new_v4())),
            capture_callback: false,
            auto_refresh: false,
            refresh_buffer_seconds: 60,
            pkce_enabled: true,
            token_store_kind: TokenStoreKind::File,
            allow_external_callback: false,
            callback_cert_file: None,
            callback_key_file: None,
            on_token_notification: None,
        },
        client: ClientConfig {
            base_url: mock_server_uri.to_string(),
            timeout: Duration::from_secs(10),
            retry: RetryConfig::default(),
            rate_limit: RateLimitConfig::default(),
            circuit_breaker: CircuitBreakerConfig::default(),
            user_agent: "rustyschwab-test/0.1.0".to_string(),
        },
        streaming: None,
    }
}

#[tokio::test]
async fn test_oauth_authorize_url_generation() {
    let mock_server = MockServer::start().await;
    let config = create_test_config(&mock_server.uri());

    let auth_manager = AuthManager::new(config.oauth).expect("Failed to create AuthManager");
    let (auth_url, _state) = auth_manager
        .authorize()
        .await
        .expect("Failed to generate authorize URL");

    // Verify URL structure
    assert!(auth_url.starts_with(&format!("{}/oauth/authorize", mock_server.uri())));
    assert!(auth_url.contains("client_id=12345678901234567890123456789012"));
    assert!(auth_url.contains("redirect_uri="));
    assert!(
        auth_url.contains("state="),
        "URL should contain CSRF state parameter"
    );
}

#[tokio::test]
async fn test_oauth_authorize_url_with_pkce() {
    let mock_server = MockServer::start().await;
    let config = create_test_config(&mock_server.uri());

    let auth_manager = AuthManager::new(config.oauth).expect("Failed to create AuthManager");
    let (auth_url, _state) = auth_manager
        .authorize()
        .await
        .expect("Failed to generate authorize URL");

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
    let result = auth_manager
        .exchange_code("test_auth_code_abc".to_string())
        .await;

    // Should succeed with mocked response
    assert!(
        result.is_ok(),
        "Token exchange should succeed with valid mock"
    );
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
    assert!(
        result.is_err(),
        "Token exchange should fail with invalid code"
    );
}

#[tokio::test]
async fn test_oauth_state_parameter_generated() {
    let mock_server = MockServer::start().await;
    let config = create_test_config(&mock_server.uri());

    let auth_manager = AuthManager::new(config.oauth).expect("Failed to create AuthManager");
    let (auth_url, _state) = auth_manager.authorize().await.expect("Failed to authorize");

    // Verify state parameter is present for CSRF protection
    assert!(
        auth_url.contains("state="),
        "URL should contain state parameter"
    );

    // Extract state parameter from URL
    let state_start = auth_url.find("state=").unwrap() + 6;
    let state_end = auth_url[state_start..]
        .find('&')
        .unwrap_or(auth_url[state_start..].len());
    let extracted_state = &auth_url[state_start..state_start + state_end];

    // State should be a UUID (36 chars including hyphens)
    assert_eq!(
        extracted_state.len(),
        36,
        "State should be UUID format (got {})",
        extracted_state
    );
}

#[tokio::test]
async fn test_authorization_header_format() {
    use base64::Engine;
    let mock = MockServer::start().await;
    let config = create_test_config(&mock.uri());
    let basic = base64::engine::general_purpose::STANDARD.encode(format!(
        "{}:{}",
        config.oauth.app_key, config.oauth.app_secret
    ));
    Mock::given(method("POST"))
        .and(header("authorization", format!("Basic {basic}")))
        .respond_with(ResponseTemplate::new(400).set_body_string("secret-marker-do-not-echo"))
        .expect(1)
        .mount(&mock)
        .await;
    let auth = AuthManager::new(config.oauth).unwrap();
    auth.authorize().await.unwrap();
    let error = auth
        .exchange_code("synthetic_code".into())
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("secret-marker"));
}

/// `authorize` in one process and the pasted redirect exchanged in another
/// (the CLI flow) is still checked against the state, once.
#[tokio::test]
async fn a_redirect_exchanged_in_another_process_is_checked_against_its_state() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "access", "refresh_token": "refresh",
            "token_type": "Bearer", "expires_in": 1800
        })))
        .mount(&mock_server)
        .await;
    let mut config = create_test_config(&mock_server.uri());
    config.oauth.pkce_enabled = false;
    let starter = AuthManager::new(config.oauth.clone()).unwrap();
    let (url, _) = starter.authorize().await.unwrap();
    let state = url::Url::parse(&url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    let other = AuthManager::new(config.oauth.clone()).unwrap();
    let redirect = format!("https://127.0.0.1:8080/callback?code=ABCDEFGHIJKL&state={state}");
    other.exchange_code(redirect.clone()).await.unwrap();
    // Consumed: the same redirect, or a forged state, is refused.
    assert!(other.exchange_code(redirect).await.is_err());
    let _ = std::fs::remove_file(&config.oauth.tokens_file);
}

/// Schwab's codes look like `C0.<base64>.<base64>@`: dotted, and
/// percent-encoded (`%40`) in the redirect, which also carries a `session`.
/// Pasting the whole redirect or just the code both reach the token endpoint
/// with the decoded code.
#[tokio::test]
async fn real_shaped_codes_are_exchanged_from_a_redirect_or_pasted_alone() {
    let code = "C0.b2F1dGgyLmJkYy5zY2h3YWIuY29t.Zx9-Ab_cD3ef4GhIjK5lMnOp6QrStUvWx";
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        // The form encoding of the decoded code: `@` is sent as `%40`.
        .and(wiremock::matchers::body_string_contains(format!(
            "code={code}%40"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "access", "refresh_token": "refresh",
            "token_type": "Bearer", "expires_in": 1800
        })))
        .expect(3)
        .mount(&mock_server)
        .await;
    let mut config = create_test_config(&mock_server.uri());
    config.oauth.pkce_enabled = false;
    let auth = AuthManager::new(config.oauth.clone()).unwrap();

    let (url, _) = auth.authorize().await.unwrap();
    let state = url::Url::parse(&url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    let redirect = format!("https://127.0.0.1:8080/?code={code}%40&session=0f1e2d3c&state={state}");
    auth.exchange_code(redirect).await.unwrap();

    // The `code` value copied out of the address bar, still encoded, and
    // the same value decoded.
    auth.exchange_code(format!("{code}%40")).await.unwrap();
    auth.exchange_code(format!("{code}@")).await.unwrap();
    let _ = std::fs::remove_file(&config.oauth.tokens_file);
}

/// The callback server serves a configured certificate, as `mkcert` issues
/// one: a browser that trusts only the issuing CA completes the redirect
/// without a warning, and the server exchanges the code and stores tokens.
#[cfg(feature = "callback-server")]
#[tokio::test]
async fn the_callback_server_serves_a_configured_trusted_certificate() {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
    use std::sync::Arc;

    // A local CA and a 127.0.0.1 certificate it signed, like mkcert's.
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["127.0.0.1".to_string(), "localhost".to_string()])
        .unwrap()
        .signed_by(&leaf_key, &ca, &ca_key)
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (cert_file, key_file) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert_file, leaf.pem()).unwrap();
    std::fs::write(&key_file, leaf_key.serialize_pem()).unwrap();

    let code = "C0.b2F1dGgyLmJkYy5zY2h3YWIuY29t.trusted-cert-test";
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(wiremock::matchers::body_string_contains(format!(
            "code={code}%40"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "access", "refresh_token": "refresh",
            "token_type": "Bearer", "expires_in": 1800
        })))
        .expect(1)
        .mount(&mock_server)
        .await;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut config = create_test_config(&mock_server.uri()).oauth;
    config.pkce_enabled = false;
    config.capture_callback = true;
    config.callback_url = format!("https://127.0.0.1:{port}");
    config.callback_cert_file = Some(cert_file);
    config.callback_key_file = Some(key_file);
    let auth = Arc::new(AuthManager::new(config.clone()).unwrap());

    let (url_tx, url_rx) = tokio::sync::oneshot::channel();
    let flow = tokio::spawn({
        let auth = auth.clone();
        async move {
            auth.authorize_with(|url| {
                let _ = url_tx.send(url.to_string());
            })
            .await
        }
    });
    let consent = url::Url::parse(&url_rx.await.unwrap()).unwrap();
    let state = consent
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();

    // A client that trusts the local CA and nothing else (rustls on ring,
    // as the workspace pins it).
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.der().clone()).unwrap();
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let browser = reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .build()
        .unwrap();
    let redirect = format!("https://127.0.0.1:{port}/?code={code}%40&session=s1&state={state}");
    let mut answer = Err(String::new());
    for _ in 0..50 {
        match browser.get(&redirect).send().await {
            Ok(response) => {
                answer = Ok(response);
                break;
            }
            // The server binds after handing out the URL.
            Err(error) => {
                answer = Err(format!("{error:?}"));
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    assert!(answer.unwrap().status().is_success());
    let (_, captured) = flow.await.unwrap().unwrap();
    assert_eq!(captured, format!("{code}@"));
    assert!(auth.has_valid_tokens());
    let _ = std::fs::remove_file(&config.tokens_file);
}

/// A refresh token Schwab refuses means only a new authorization helps:
/// the error says so, and the endpoint is not asked again for the same
/// tokens.
#[tokio::test]
async fn a_refused_refresh_token_requires_reauthorization_and_is_not_retried() {
    use rustyschwab::auth::{FileTokenStore, TokenSet, TokenStore};
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_grant"
        })))
        .expect(1)
        .mount(&mock_server)
        .await;
    let config = create_test_config(&mock_server.uri());
    // An access token already past its life, with a valid refresh token.
    FileTokenStore::new(config.oauth.tokens_file.clone())
        .save(&TokenSet::new(
            "stale".into(),
            "refresh".into(),
            "".into(),
            "Bearer".into(),
            0,
            "".into(),
            chrono::Utc::now() - chrono::Duration::hours(1),
        ))
        .unwrap();
    let manager = AuthManager::new(config.oauth.clone()).unwrap();
    for _ in 0..2 {
        let error = manager.ensure_valid_tokens().await.unwrap_err();
        assert!(error.requires_reauthorization(), "{error}");
    }
    mock_server.verify().await;
    let _ = std::fs::remove_file(&config.oauth.tokens_file);
}
