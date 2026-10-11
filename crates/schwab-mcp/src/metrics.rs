//! Tool-level metrics for the MCP server.
//!
//! The SDK owns HTTP / retry / circuit-breaker / stream metrics
//! (see `schwab_rs::metrics`); this module only adds the *MCP tool
//! call* level — who invoked which tool and how long it took — and the
//! stream-cache recovery that the server drives after a reconnect.
//!
//! # Metrics
//!
//! | Name | Kind | Labels | Description |
//! |------|------|--------|-------------|
//! | `schwab_mcp_tool_calls_total` | counter | `tool`, `outcome` | Outcome is `ok` or `error`. One increment per invocation. |
//! | `schwab_mcp_tool_call_duration_seconds` | histogram | `tool` | End-to-end time including upstream HTTP, retries, and redaction. |
//! | `schwab_stream_recovery_seconds` | histogram | — | Time spent re-subscribing and reconciling the stream cache after a reconnect, capped at the recovery deadline. |
//! | `schwab_stream_recovery_ready` | gauge | — | `1` when the last stream recovery completed within its deadline, `0` otherwise. |
//! | `schwab_mcp_dead_man_firings_total` | counter | `outcome` | Dead-man switch firings that found working orders (or could not look): `canceled` when every cancel succeeded, `failed` otherwise. |
//!
//! Recording is via an RAII guard — see [`ToolTimer`].

use metrics::Unit;
use schwab_rs::metrics::Metric;
use std::time::Instant;

pub const MCP_TOOL_CALLS_TOTAL: &str = "schwab_mcp_tool_calls_total";
pub const MCP_TOOL_CALL_DURATION: &str = "schwab_mcp_tool_call_duration_seconds";
pub const STREAM_RECOVERY_SECONDS: &str = "schwab_stream_recovery_seconds";
pub const STREAM_RECOVERY_READY: &str = "schwab_stream_recovery_ready";
pub const DEAD_MAN_FIRINGS_TOTAL: &str = "schwab_mcp_dead_man_firings_total";

/// Every metric the server emits on top of the SDK's. See the module
/// docs for labels.
pub const CATALOG: &[Metric] = &[
    Metric::counter(
        MCP_TOOL_CALLS_TOTAL,
        "MCP tool invocations, by tool and outcome",
    ),
    Metric::histogram(
        MCP_TOOL_CALL_DURATION,
        Unit::Seconds,
        "MCP tool call wall-clock time including upstream requests and redaction",
    ),
    Metric::histogram(
        STREAM_RECOVERY_SECONDS,
        Unit::Seconds,
        "Stream cache recovery time after a reconnect",
    ),
    Metric::gauge(
        STREAM_RECOVERY_READY,
        None,
        "1 when the last stream recovery completed within its deadline",
    ),
    Metric::counter(
        DEAD_MAN_FIRINGS_TOTAL,
        "Dead-man switch firings that found working orders to cancel, by outcome",
    ),
];

/// Register the SDK and server catalogs with the installed recorder.
pub fn describe() {
    schwab_rs::metrics::describe();
    schwab_rs::metrics::describe_all(CATALOG);
}

/// Scope guard that records a tool-call metric on drop.
///
/// The default outcome is `error`; call [`ToolTimer::success`] before
/// returning the encoded success to flip it, so every early return
/// (including a `ToolFailure` turned into an `isError` result) is
/// classified without branching at each site. `contract::run` is the one
/// caller; tools never start a timer themselves.
#[derive(Debug)]
pub struct ToolTimer {
    tool: &'static str,
    start: Instant,
    succeeded: bool,
}

impl ToolTimer {
    pub fn start(tool: &'static str) -> Self {
        Self {
            tool,
            start: Instant::now(),
            succeeded: false,
        }
    }

    /// Mark the call as successful. Consumes the timer so Drop still
    /// fires exactly once (and exactly once).
    pub fn success(mut self) {
        self.succeeded = true;
        // Drop runs here.
    }
}

impl Drop for ToolTimer {
    fn drop(&mut self) {
        let outcome = if self.succeeded { "ok" } else { "error" };
        metrics::counter!(
            MCP_TOOL_CALLS_TOTAL,
            "tool" => self.tool,
            "outcome" => outcome,
        )
        .increment(1);
        metrics::histogram!(
            MCP_TOOL_CALL_DURATION,
            "tool" => self.tool,
        )
        .record(self.start.elapsed().as_secs_f64());
    }
}
