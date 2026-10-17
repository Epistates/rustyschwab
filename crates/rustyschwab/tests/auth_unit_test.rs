//! Unit tests for authentication module
//!
//! Tests token management, OAuth2 flow, PKCE, CSRF protection, and header generation

use rustyschwab::auth::{OAuthConfig, TokenStoreKind, create_basic_header, create_bearer_header};

#[test]
fn oauth_config_default_disables_pkce() {
    // Schwab + Schwabdev don't use PKCE; defaulting it on caused spurious
    // `invalid_request` responses in some app configs. Lock the default off.
    let cfg = OAuthConfig::default();
    assert!(!cfg.pkce_enabled, "PKCE must be opt-in, not on by default");
}

#[test]
fn oauth_config_default_uses_platform_token_store() {
    // Previously hardcoded to `File`, defeating the per-OS `Default for
    // TokenStoreKind` (Keychain on macOS, EncryptedFile elsewhere).
    let cfg = OAuthConfig::default();
    let platform_default = TokenStoreKind::default();
    // Enum doesn't derive PartialEq; compare via Debug formatting.
    assert_eq!(
        format!("{:?}", cfg.token_store_kind),
        format!("{:?}", platform_default),
        "OAuthConfig::default() should inherit TokenStoreKind::default()"
    );
}

#[test]
fn test_bearer_header_format() {
    let token = "test_access_token_12345";
    let header = create_bearer_header(token);

    assert_eq!(header, "Bearer test_access_token_12345");
    assert!(header.starts_with("Bearer "));
}

#[test]
fn test_bearer_header_with_special_chars() {
    let token = "token.with-special_chars~123";
    let header = create_bearer_header(token);

    assert_eq!(header, "Bearer token.with-special_chars~123");
}

#[test]
fn test_bearer_header_empty() {
    let header = create_bearer_header("");
    assert_eq!(header, "Bearer ");
}

#[test]
fn test_basic_header_encoding() {
    let app_key = "user";
    let app_secret = "pass";
    let header = create_basic_header(app_key, app_secret);

    // "user:pass" base64 encoded is "dXNlcjpwYXNz"
    assert_eq!(header, "Basic dXNlcjpwYXNz");
    assert!(header.starts_with("Basic "));
}

#[test]
fn test_basic_header_with_special_chars() {
    let app_key = "user@example.com";
    let app_secret = "pass:word!123";
    let header = create_basic_header(app_key, app_secret);

    assert!(header.starts_with("Basic "));
    // Verify it's properly base64 encoded
    let encoded_part = &header[6..];
    assert!(!encoded_part.is_empty());
    assert!(!encoded_part.contains(':'));
}

#[test]
fn test_basic_header_deterministic() {
    let app_key = "key";
    let app_secret = "secret";

    let header1 = create_basic_header(app_key, app_secret);
    let header2 = create_basic_header(app_key, app_secret);

    // Same inputs should produce same output
    assert_eq!(header1, header2);
}

// NOTE: PKCE-specific unit tests used to live here, covering the old
// `PkceSession` helper. That module has been removed — the active OAuth
// path in `AuthManager` owns the verifier lifecycle directly, and the
// end-to-end behavior is exercised by `oauth_integration_test::
// test_oauth_authorize_url_with_pkce`.
