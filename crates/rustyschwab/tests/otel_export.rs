//! `init_otel` against a collector that speaks HTTP: the standard OTEL_*
//! environment configures the exporter, a span reaches the endpoint with
//! the service name, resource attributes and headers, and dropping the
//! guard is what flushes it.
//!
//! The test sets process environment and installs the global subscriber, so
//! it is the only test in this binary. The no-endpoint case lives in
//! `otel_disabled.rs` for the same reason.
#![cfg(feature = "otel")]
#![allow(unsafe_code)] // `set_var` is unsafe in edition 2024; one thread sets it before any other starts.

use rustyschwab::telemetry::init_otel;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spans_reach_the_collector_with_the_configured_resource() {
    let collector = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/traces"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&collector)
        .await;

    // SAFETY: no other thread reads the environment yet; the test runtime's
    // workers are idle and the collector only serves requests we send later.
    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", collector.uri());
        std::env::set_var("OTEL_EXPORTER_OTLP_HEADERS", "x-collector-key=sekrit");
        std::env::set_var("OTEL_SERVICE_NAME", "smoke-service");
        std::env::set_var("OTEL_RESOURCE_ATTRIBUTES", "deployment.environment=ci");
        std::env::set_var("OTEL_METRICS_EXPORTER", "none");
        std::env::remove_var("OTEL_SDK_DISABLED");
        std::env::remove_var("OTEL_EXPORTER_OTLP_PROTOCOL");
    }

    let guard = init_otel("fallback-name", "info", std::io::sink).expect("OTLP exporter builds");
    assert!(guard.exports_traces());

    {
        let span = tracing::info_span!("schwab.request", op = "get_quotes");
        let _entered = span.enter();
        tracing::info!(secret = "never-exported", "a log line inside the span");
    }

    // Dropping the guard flushes the batch; it blocks, so keep it off the
    // runtime thread the collector needs.
    tokio::task::spawn_blocking(move || drop(guard))
        .await
        .expect("guard drop");

    let requests = collector.received_requests().await.expect("recording on");
    let request = requests
        .iter()
        .find(|request| request.url.path() == "/v1/traces")
        .expect("a trace export reached the collector");
    assert_eq!(
        request
            .headers
            .get("x-collector-key")
            .and_then(|v| v.to_str().ok()),
        Some("sekrit")
    );
    let body = &request.body;
    for expected in [
        "smoke-service",
        "deployment.environment",
        "ci",
        "schwab.request",
        "get_quotes",
    ] {
        assert!(contains(body, expected), "export lacks {expected:?}");
    }
    assert!(!contains(body, "fallback-name"), "OTEL_SERVICE_NAME wins");
    assert!(
        !contains(body, "never-exported"),
        "a log event field reached the collector"
    );
}
