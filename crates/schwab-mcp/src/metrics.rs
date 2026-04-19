//! Tool-level metrics for the MCP server.
//!
//! The SDK owns HTTP / retry / circuit-breaker / stream metrics
//! (see `schwab_rs::metrics`); this module only adds the *MCP tool
//! call* level — who invoked which tool and how long it took.
//!
//! # Metrics
//!
//! | Name | Kind | Labels | Description |
//! |------|------|--------|-------------|
//! | `schwab_mcp_tool_calls_total` | counter | `tool`, `outcome` | Outcome is `ok` or `error`. One increment per invocation. |
//! | `schwab_mcp_tool_call_duration_seconds` | histogram | `tool` | End-to-end time including upstream HTTP, retries, and redaction. |
//!
//! Recording is via an RAII guard — see [`ToolTimer`].

use std::time::Instant;

pub const MCP_TOOL_CALLS_TOTAL: &str = "schwab_mcp_tool_calls_total";
pub const MCP_TOOL_CALL_DURATION: &str = "schwab_mcp_tool_call_duration_seconds";

/// Scope guard that records a tool-call metric on drop.
///
/// The default outcome is `error`; call [`ToolTimer::success`] before
/// returning `Ok(..)` to flip it. This way, `?`-propagated errors
/// automatically get classified correctly without needing to branch
/// at every return site.
///
/// ```ignore
/// let timer = ToolTimer::start("get_quote");
/// let resp = self.client.get_quote(&symbol).await.map_err(map_err)?;
/// // ^ on `?` short-circuit, timer drops with outcome="error"
/// let out = redact::json_response(resp);
/// timer.success();
/// Ok(out)
/// ```
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
