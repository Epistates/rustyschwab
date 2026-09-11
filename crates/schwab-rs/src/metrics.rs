//! Metric names, label conventions, and small helpers for SDK-side
//! instrumentation.
//!
//! The SDK depends only on the `metrics` *facade* — every
//! `counter!`/`histogram!`/`gauge!` call is a no-op unless something
//! (typically the `schwab-mcp` binary) installs a recorder via
//! `metrics::set_global_recorder(...)`. Library consumers can install
//! their own recorder — Prometheus, StatsD, OTLP — without any change
//! to the SDK surface. Call [`describe`] after installing one to register
//! the help text below with it.
//!
//! # Metric catalog
//!
//! Every metric the SDK emits is declared here, both as a named constant
//! (the only form call sites may use) and as a [`CATALOG`] entry. A
//! workspace test keeps the two, the call sites and this table in sync.
//!
//! | Name | Kind | Labels | Description |
//! |------|------|--------|-------------|
//! | `schwab_http_requests_total` | counter | `op`, `method`, `status` | Outermost logical request completions. One increment per call to `SchwabClient::request_with_options`, regardless of retries. `status` is a class bucket (`2xx`…`5xx`, `unknown`) or `error` when no HTTP response was obtained. |
//! | `schwab_http_request_duration_seconds` | histogram | `op`, `method` | Wall-clock time including retries, token refresh, and circuit-breaker short-circuits. |
//! | `schwab_submission_unknown_total` | counter | — | Order submissions that were sent but whose outcome could not be determined; each one needs reconciliation before trading resumes. |
//! | `schwab_retry_attempts_total` | counter | `outcome` | One per actual retry fired by `RetryPolicy::execute`. Outcomes: `succeeded`, `exhausted`. |
//! | `schwab_circuit_state` | gauge | — | Current state: `0=Closed`, `1=HalfOpen`, `2=Open`. |
//! | `schwab_circuit_transitions_total` | counter | `from`, `to` | Every state change. |
//! | `schwab_token_refresh_total` | counter | `outcome`, `grant` | One per `/oauth/token` request after internal retries: `success` / `failure` × `authorization_code` / `refresh_token`. |
//! | `schwab_reconciliation_total` | counter | `outcome` | Account reconciliation runs: `ready` when the baseline is usable, `unresolved` otherwise. |
//! | `schwab_pending_submissions` | gauge | — | Submissions awaiting reconciliation, as of the last readiness probe. |
//! | `schwab_unreconciled_accounts` | gauge | — | Required accounts without a fresh baseline, as of the last readiness probe. |
//! | `schwab_baseline_age_seconds` | gauge | — | Age of the oldest required-account baseline, as of the last readiness probe. |
//! | `schwab_trading_ready` | gauge | — | `1` when the last readiness probe found trading admissible, `0` otherwise. |
//! | `schwab_recovery_ready` | gauge | — | `1` while the recovery worker has no accounts left to reconcile. |
//! | `schwab_stream_connects_total` | counter | `outcome` | `success` / `failure` per connection attempt. |
//! | `schwab_stream_reconnects_total` | counter | — | One per reconnect the loop triggers. |
//! | `schwab_stream_connected` | gauge | — | `1` while authenticated, `0` otherwise. |
//! | `schwab_stream_messages_total` | counter | `service`, `kind` | Data/Response/Notify frames delivered to the receiver (`kind` ∈ `data`, `response`, `notify`; control frames use `service="_control"`) plus `dropped` frames that failed to parse (`service="_unparsed"`). |
//! | `schwab_stream_dropped_messages_total` | counter | — | Frames dropped because the consumer queue was full or closed; the stream is unready until reconciled. |
//!
//! MCP-side tool metrics (`schwab_mcp_tool_calls_total` etc.) live in
//! `schwab-mcp` — they're separate because the SDK is transport-agnostic.

use metrics::Unit;

/// HTTP request lifecycle.
pub const HTTP_REQUESTS_TOTAL: &str = "schwab_http_requests_total";
pub const HTTP_REQUEST_DURATION: &str = "schwab_http_request_duration_seconds";
pub const SUBMISSION_UNKNOWN_TOTAL: &str = "schwab_submission_unknown_total";

/// Retry policy outcomes.
pub const RETRY_ATTEMPTS_TOTAL: &str = "schwab_retry_attempts_total";

/// Circuit breaker.
pub const CIRCUIT_STATE: &str = "schwab_circuit_state";
pub const CIRCUIT_TRANSITIONS_TOTAL: &str = "schwab_circuit_transitions_total";

/// OAuth token management.
pub const TOKEN_REFRESH_TOTAL: &str = "schwab_token_refresh_total";

/// Reconciliation and readiness.
pub const RECONCILIATION_TOTAL: &str = "schwab_reconciliation_total";
pub const PENDING_SUBMISSIONS: &str = "schwab_pending_submissions";
pub const UNRECONCILED_ACCOUNTS: &str = "schwab_unreconciled_accounts";
pub const BASELINE_AGE_SECONDS: &str = "schwab_baseline_age_seconds";
pub const TRADING_READY: &str = "schwab_trading_ready";
pub const RECOVERY_READY: &str = "schwab_recovery_ready";

/// Streaming WebSocket lifecycle and throughput.
pub const STREAM_CONNECTS_TOTAL: &str = "schwab_stream_connects_total";
pub const STREAM_RECONNECTS_TOTAL: &str = "schwab_stream_reconnects_total";
pub const STREAM_CONNECTED: &str = "schwab_stream_connected";
pub const STREAM_MESSAGES_TOTAL: &str = "schwab_stream_messages_total";
pub const STREAM_DROPPED_MESSAGES_TOTAL: &str = "schwab_stream_dropped_messages_total";

/// The instrument a catalog entry is recorded with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

/// One entry of a metric catalog: enough for [`describe`] to register it
/// with any recorder and for operators to know what a series means.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Metric {
    pub name: &'static str,
    pub kind: MetricKind,
    pub unit: Option<Unit>,
    pub help: &'static str,
}

impl Metric {
    /// A monotonic count of events.
    pub const fn counter(name: &'static str, help: &'static str) -> Self {
        Self {
            name,
            kind: MetricKind::Counter,
            unit: Some(Unit::Count),
            help,
        }
    }

    /// A point-in-time value; `unit` is `None` for booleans and enums.
    pub const fn gauge(name: &'static str, unit: Option<Unit>, help: &'static str) -> Self {
        Self {
            name,
            kind: MetricKind::Gauge,
            unit,
            help,
        }
    }

    /// A distribution of measurements.
    pub const fn histogram(name: &'static str, unit: Unit, help: &'static str) -> Self {
        Self {
            name,
            kind: MetricKind::Histogram,
            unit: Some(unit),
            help,
        }
    }
}

/// Every metric the SDK emits. See the module docs for labels.
pub const CATALOG: &[Metric] = &[
    Metric::counter(
        HTTP_REQUESTS_TOTAL,
        "Logical Schwab API request completions, regardless of retries",
    ),
    Metric::histogram(
        HTTP_REQUEST_DURATION,
        Unit::Seconds,
        "Schwab API request wall-clock time including retries and token refresh",
    ),
    Metric::counter(
        SUBMISSION_UNKNOWN_TOTAL,
        "Order submissions sent with an undetermined outcome; each needs reconciliation",
    ),
    Metric::counter(
        RETRY_ATTEMPTS_TOTAL,
        "Retries fired by the retry policy, by final outcome",
    ),
    Metric::gauge(
        CIRCUIT_STATE,
        None,
        "Circuit breaker state: 0 closed, 1 half-open, 2 open",
    ),
    Metric::counter(CIRCUIT_TRANSITIONS_TOTAL, "Circuit breaker state changes"),
    Metric::counter(
        TOKEN_REFRESH_TOTAL,
        "OAuth token endpoint requests, by grant and outcome",
    ),
    Metric::counter(
        RECONCILIATION_TOTAL,
        "Account reconciliation runs, by outcome",
    ),
    Metric::gauge(
        PENDING_SUBMISSIONS,
        Some(Unit::Count),
        "Order submissions awaiting reconciliation at the last readiness probe",
    ),
    Metric::gauge(
        UNRECONCILED_ACCOUNTS,
        Some(Unit::Count),
        "Required accounts without a fresh baseline at the last readiness probe",
    ),
    Metric::gauge(
        BASELINE_AGE_SECONDS,
        Some(Unit::Seconds),
        "Age of the oldest required-account baseline at the last readiness probe",
    ),
    Metric::gauge(
        TRADING_READY,
        None,
        "1 when the last readiness probe found trading admissible",
    ),
    Metric::gauge(
        RECOVERY_READY,
        None,
        "1 while the recovery worker has no accounts left to reconcile",
    ),
    Metric::counter(
        STREAM_CONNECTS_TOTAL,
        "Streaming connection attempts, by outcome",
    ),
    Metric::counter(STREAM_RECONNECTS_TOTAL, "Streaming reconnects triggered"),
    Metric::gauge(
        STREAM_CONNECTED,
        None,
        "1 while the streaming session is authenticated",
    ),
    Metric::counter(
        STREAM_MESSAGES_TOTAL,
        "Streaming frames delivered to the receiver, by service and kind",
    ),
    Metric::counter(
        STREAM_DROPPED_MESSAGES_TOTAL,
        "Streaming frames dropped because the consumer queue was full or closed",
    ),
];

/// Register the help text and units of every [`CATALOG`] entry with the
/// installed recorder. Call it once, after `metrics::set_global_recorder`;
/// recorders that ignore descriptions make this a no-op.
pub fn describe() {
    describe_all(CATALOG);
}

/// [`describe`] for any catalog, so downstream crates with their own
/// metrics can register them the same way.
pub fn describe_all(catalog: &[Metric]) {
    metrics::with_recorder(|recorder| {
        for metric in catalog {
            let (name, help) = (metric.name.into(), metric.help.into());
            match metric.kind {
                MetricKind::Counter => recorder.describe_counter(name, metric.unit, help),
                MetricKind::Gauge => recorder.describe_gauge(name, metric.unit, help),
                MetricKind::Histogram => recorder.describe_histogram(name, metric.unit, help),
            }
        }
    });
}

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
