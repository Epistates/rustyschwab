//! Metric names, label conventions, and small helpers for SDK-side
//! instrumentation.
//!
//! The SDK depends only on the `metrics` *facade* — every
//! `counter!`/`histogram!`/`gauge!` call is a no-op unless something
//! (typically the `schwab-mcp` binary) installs a recorder via
//! `metrics::set_global_recorder(...)`. Library consumers can install
//! their own recorder — Prometheus, StatsD, OTLP — without any change
//! to the SDK surface.
//!
//! # Metric catalog
//!
//! | Name | Kind | Labels | Description |
//! |------|------|--------|-------------|
//! | `schwab_http_requests_total` | counter | `op`, `method`, `status` | Outermost logical request completions. One increment per call to `SchwabClient::request_with_options`, regardless of retries. |
//! | `schwab_http_request_duration_seconds` | histogram | `op`, `method` | Wall-clock time including retries, token refresh, and circuit-breaker short-circuits. |
//! | `schwab_retry_attempts_total` | counter | `outcome` | One per actual retry fired by `RetryPolicy::execute`. Outcomes: `succeeded`, `exhausted`. |
//! | `schwab_circuit_state` | gauge | — | Current state: `0=Closed`, `1=HalfOpen`, `2=Open`. |
//! | `schwab_circuit_transitions_total` | counter | `from`, `to` | Every state change. |
//! | `schwab_token_refresh_total` | counter | `outcome` | `success` / `failure`, for both OAuth code exchange and refresh-grant paths. |
//! | `schwab_stream_connects_total` | counter | `outcome` | `success` / `failure` per connection attempt. |
//! | `schwab_stream_reconnects_total` | counter | — | One per reconnect the loop triggers. |
//! | `schwab_stream_connected` | gauge | — | `1` while authenticated, `0` otherwise. |
//! | `schwab_stream_messages_total` | counter | `service`, `kind` | Data/Response/Notify frames delivered to the receiver. |
//!
//! MCP-side tool metrics (`schwab_mcp_tool_calls_total` etc.) live in
//! `schwab-mcp` — they're separate because the SDK is transport-agnostic.

/// HTTP request lifecycle.
pub const HTTP_REQUESTS_TOTAL: &str = "schwab_http_requests_total";
pub const HTTP_REQUEST_DURATION: &str = "schwab_http_request_duration_seconds";

/// Retry policy outcomes.
pub const RETRY_ATTEMPTS_TOTAL: &str = "schwab_retry_attempts_total";

/// Circuit breaker.
pub const CIRCUIT_STATE: &str = "schwab_circuit_state";
pub const CIRCUIT_TRANSITIONS_TOTAL: &str = "schwab_circuit_transitions_total";

/// OAuth token management.
pub const TOKEN_REFRESH_TOTAL: &str = "schwab_token_refresh_total";

/// Streaming WebSocket lifecycle and throughput.
pub const STREAM_CONNECTS_TOTAL: &str = "schwab_stream_connects_total";
pub const STREAM_RECONNECTS_TOTAL: &str = "schwab_stream_reconnects_total";
pub const STREAM_CONNECTED: &str = "schwab_stream_connected";
pub const STREAM_MESSAGES_TOTAL: &str = "schwab_stream_messages_total";

/// Classify a status code into a `2xx`/`4xx`/`5xx` bucket.
///
/// Used instead of the raw status to keep Prometheus cardinality
/// bounded — an `op × method × status_class` label set is ~150 series,
/// while `op × method × raw_status` could explode if a remote service
/// starts returning arbitrary codes.
pub fn status_class(status: u16) -> &'static str {
    match status {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "unknown",
    }
}

/// Convert a `CircuitState` to the numeric value stored in the gauge.
///
/// Centralized here so dashboards can rely on a stable mapping even if
/// we add intermediate states later.
pub fn circuit_state_value(state: crate::circuit_breaker::CircuitState) -> f64 {
    use crate::circuit_breaker::CircuitState;
    match state {
        CircuitState::Closed => 0.0,
        CircuitState::HalfOpen => 1.0,
        CircuitState::Open => 2.0,
    }
}

pub fn circuit_state_name(state: crate::circuit_breaker::CircuitState) -> &'static str {
    use crate::circuit_breaker::CircuitState;
    match state {
        CircuitState::Closed => "closed",
        CircuitState::HalfOpen => "half_open",
        CircuitState::Open => "open",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_class_buckets() {
        assert_eq!(status_class(200), "2xx");
        assert_eq!(status_class(204), "2xx");
        assert_eq!(status_class(401), "4xx");
        assert_eq!(status_class(429), "4xx");
        assert_eq!(status_class(503), "5xx");
        assert_eq!(status_class(999), "unknown");
    }
}
