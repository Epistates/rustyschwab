//! Unit tests for error handling module
//!
//! Tests error classification, parsing, formatting, and helper methods

use schwab_rs::error::{Error, AuthError, StreamError, parse_api_error};
use reqwest::StatusCode;
use std::time::Duration;

#[test]
fn test_parse_api_error_with_valid_json() {
    // Simulate Schwab API error response format
    let json_body = r#"{
        "errors": [{
            "id": "ERR_123",
            "status": "400",
            "title": "Invalid Symbol",
            "detail": "The symbol 'INVALID' is not recognized"
        }]
    }"#;

    let error = parse_api_error(StatusCode::BAD_REQUEST, json_body);

    match error {
        Error::Api { code, message } => {
            assert_eq!(code, "ERR_123");
            assert!(message.contains("Invalid Symbol"));
        }
        _ => panic!("Expected Api error, got {:?}", error),
    }
}

#[test]
fn test_parse_api_error_with_malformed_json() {
    // Malformed JSON should fall back to Http error
    let bad_json = "Not valid JSON at all";

    let error = parse_api_error(StatusCode::INTERNAL_SERVER_ERROR, bad_json);

    match error {
        Error::Http { status, message } => {
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(message, "Not valid JSON at all");
        }
        _ => panic!("Expected Http error for malformed JSON, got {:?}", error),
    }
}

#[test]
fn test_retry_after_returns_duration_for_rate_limit() {
    let error = Error::RateLimit { retry_after: 120 };

    let retry_duration = error.retry_after();

    assert!(retry_duration.is_some(), "RateLimit error should have retry_after duration");
    assert_eq!(retry_duration.unwrap(), Duration::from_secs(120));
}

#[test]
fn test_retry_after_returns_none_for_non_retryable() {
    let error = Error::InvalidParameter("test param".to_string());

    let retry_duration = error.retry_after();

    assert!(retry_duration.is_none(), "InvalidParameter should not have retry_after duration");
}

#[test]
fn test_error_is_retryable_for_500_series() {
    let error = Error::Http {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "Server error".to_string(),
    };

    assert!(error.is_retryable(), "HTTP 500 errors should be retryable");

    let error_503 = Error::Http {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "Service unavailable".to_string(),
    };

    assert!(error_503.is_retryable(), "HTTP 503 errors should be retryable");
}

#[test]
fn test_error_is_not_retryable_for_400_series() {
    let error = Error::Http {
        status: StatusCode::BAD_REQUEST,
        message: "Bad request".to_string(),
    };

    assert!(!error.is_retryable(), "HTTP 400 errors should not be retryable");

    let error_404 = Error::Http {
        status: StatusCode::NOT_FOUND,
        message: "Not found".to_string(),
    };

    assert!(!error_404.is_retryable(), "HTTP 404 errors should not be retryable");
}

#[test]
fn test_auth_error_variants_display() {
    let token_expired = AuthError::TokenExpired;
    assert_eq!(token_expired.to_string(), "Token expired");

    let oauth_flow = AuthError::OAuthFlow("invalid_grant".to_string());
    assert_eq!(oauth_flow.to_string(), "OAuth flow failed: invalid_grant");

    let missing_refresh = AuthError::MissingRefreshToken;
    assert_eq!(missing_refresh.to_string(), "Missing refresh token");

    let encryption_failed = AuthError::EncryptionFailed("bad key".to_string());
    assert_eq!(encryption_failed.to_string(), "Encryption failed: bad key");
}

#[test]
fn test_stream_error_variants_display() {
    let connection_failed = StreamError::ConnectionFailed("timeout".to_string());
    assert_eq!(connection_failed.to_string(), "Connection failed: timeout");

    let subscription_failed = StreamError::SubscriptionFailed {
        service: "LEVELONE_EQUITIES".to_string(),
        code: 21,
        message: "Symbol limit exceeded".to_string(),
    };
    let display = subscription_failed.to_string();
    assert!(display.contains("LEVELONE_EQUITIES"));
    assert!(display.contains("21"));
    assert!(display.contains("Symbol limit exceeded"));

    let heartbeat_timeout = StreamError::HeartbeatTimeout;
    assert_eq!(heartbeat_timeout.to_string(), "Heartbeat timeout");
}

#[test]
fn test_error_conversions_from_auth_error() {
    let auth_error = AuthError::InvalidCredentials;
    let error: Error = Error::from(auth_error);

    assert!(error.is_auth_error(), "Should be recognized as auth error");
    assert_eq!(error.to_string(), "Authentication error: Invalid credentials");
}

#[test]
fn test_error_conversions_from_stream_error() {
    let stream_error = StreamError::ServiceNotAvailable("CHART_EQUITY".to_string());
    let error: Error = Error::from(stream_error);

    match error {
        Error::Stream(_) => {
            assert!(error.to_string().contains("Service not available"));
            assert!(error.to_string().contains("CHART_EQUITY"));
        }
        _ => panic!("Expected Stream error, got {:?}", error),
    }
}

#[test]
fn test_error_timeout_variant() {
    let error = Error::Timeout { duration: 30 };

    assert!(error.is_retryable(), "Timeout errors should be retryable");
    assert_eq!(error.to_string(), "Timeout occurred after 30 seconds");
}

#[test]
fn test_error_connection_closed_variant() {
    let error = Error::ConnectionClosed;

    assert!(error.is_retryable(), "ConnectionClosed should be retryable");
    assert_eq!(error.to_string(), "Connection closed unexpectedly");
}
