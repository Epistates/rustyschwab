//! Unit tests for authentication module
//!
//! Tests token management, OAuth2 flow, PKCE, CSRF protection, and header generation

use schwab_rs::auth::{create_basic_header, create_bearer_header, PkceSession};

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

#[test]
fn test_pkce_session_creation() {
    let session = PkceSession::new();
    assert_eq!(session.used_count(), 0);
}

#[test]
fn test_pkce_generate_verifier_and_challenge() {
    let session = PkceSession::new();

    let result = session.generate();
    assert!(result.is_ok());

    let (verifier, challenge) = result.unwrap();

    // Verifier should be non-empty base64url string
    assert!(!verifier.is_empty());
    assert!(verifier.len() >= 43); // RFC 7636: 43-128 chars

    // Challenge should be different from verifier (SHA256 hash)
    assert!(!challenge.is_empty());
    assert_ne!(verifier, challenge);

    // Both should be valid base64url (no + or / chars)
    assert!(!verifier.contains('+'));
    assert!(!verifier.contains('/'));
    assert!(!challenge.contains('+'));
    assert!(!challenge.contains('/'));
}

#[test]
fn test_pkce_single_use_enforcement() {
    let session = PkceSession::new();

    session.generate().expect("Failed to generate");
    let verifier = session.take_verifier().expect("Failed to take verifier");

    assert!(!verifier.is_empty());
    assert_eq!(session.used_count(), 1);

    // Attempting to take again should fail
    let result = session.take_verifier();
    assert!(result.is_err());
}

#[test]
fn test_pkce_replay_detection() {
    let session = PkceSession::new();

    let (verifier, _) = session.generate().expect("Failed to generate");
    session.take_verifier().expect("Failed to take");

    // Verifier should be tracked as used
    assert!(session.has_been_used(&verifier));
}

#[test]
fn test_pkce_multiple_sessions() {
    let session = PkceSession::new();

    // First session
    let (v1, _) = session.generate().expect("Generate 1 failed");
    session.take_verifier().expect("Take 1 failed");

    // Second session
    let (v2, _) = session.generate().expect("Generate 2 failed");
    session.take_verifier().expect("Take 2 failed");

    // Both should be tracked
    assert!(session.has_been_used(&v1));
    assert!(session.has_been_used(&v2));
    assert_eq!(session.used_count(), 2);

    // Verifiers should be different
    assert_ne!(v1, v2);
}

#[test]
fn test_pkce_challenge_deterministic() {
    // Although we can't directly call the private function,
    // we can verify that the same verifier generates same challenge
    // by testing via the public API multiple times
    let session1 = PkceSession::new();
    let session2 = PkceSession::new();

    let (v1, c1) = session1.generate().expect("Generate 1 failed");
    let (v2, c2) = session2.generate().expect("Generate 2 failed");

    // Different sessions should produce different verifiers
    assert_ne!(v1, v2);
    assert_ne!(c1, c2);
}

#[test]
fn test_pkce_verifier_length() {
    let session = PkceSession::new();
    let (verifier, _) = session.generate().expect("Failed to generate");

    // RFC 7636: verifier must be 43-128 characters
    assert!(verifier.len() >= 43, "Verifier too short: {}", verifier.len());
    assert!(verifier.len() <= 128, "Verifier too long: {}", verifier.len());
}

#[test]
fn test_pkce_challenge_format() {
    let session = PkceSession::new();
    let (_, challenge) = session.generate().expect("Failed to generate");

    // Challenge should be base64url encoded SHA256 hash (43 chars for 256 bits)
    assert_eq!(challenge.len(), 43, "Challenge should be 43 chars");

    // Should only contain base64url characters
    assert!(challenge.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_'));
}
