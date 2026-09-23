//! Response handling bounds: what the transport reads, keeps and prints.

mod common;

use common::{mock_client, mock_client_with};
use schwab_rs::Error;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An unrecognised error body is kept (bounded) for trusted callers but
/// never printed: a proxy page or new error shape can echo request data.
#[tokio::test]
async fn unrecognised_error_bodies_stay_out_of_display_and_are_bounded() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    let body = format!("SECRET-MARKER {}", "x".repeat(64 * 1024));
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(418).set_body_string(body))
        .mount(&server)
        .await;
    let error = client.get_quotes(&["AAPL"]).await.unwrap_err();
    assert!(!error.to_string().contains("SECRET-MARKER"), "{error}");
    match error {
        Error::Http { message, .. } => {
            assert!(message.len() <= 16 * 1024, "kept {} bytes", message.len());
        }
        other => panic!("expected Error::Http, got {other:?}"),
    }
}

#[tokio::test]
async fn small_error_bodies_are_kept_for_callers() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(418).set_body_string("teapot"))
        .mount(&server)
        .await;
    match client.get_quotes(&["AAPL"]).await.unwrap_err() {
        Error::Http { message, .. } => assert_eq!(message, "teapot"),
        other => panic!("expected Error::Http, got {other:?}"),
    }
}

/// A server-directed wait is honoured only up to five minutes.
#[tokio::test]
async fn retry_after_is_clamped() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client_with(&server, false, |config| {
        config.client.retry.max_retries = 0;
    });
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "86400"))
        .mount(&server)
        .await;
    match client.get_quotes(&["AAPL"]).await.unwrap_err() {
        Error::RateLimit { retry_after } => assert_eq!(retry_after, 300),
        other => panic!("expected Error::RateLimit, got {other:?}"),
    }
}
