//! A principal's tool calls as they happen, for a person following the
//! agent live (`rustyschwab-dash`, or anything that tails the journal).
//!
//! Each call the principal makes, answered or refused, becomes one
//! `tool_call` entry in its journal's activity log
//! ([`OrderJournal::record_activity`]): the tool, its arguments (an order's
//! `rationale` included), how it ended, how long it took, and the venue
//! order id a write produced. Results are not kept: they can be large, and
//! the journal and the venue already hold what matters.
//!
//! The log is for watching, not evidence: writes are journaled and audited
//! on their own, and a note that cannot be written is logged and dropped
//! rather than holding up the call.

use std::time::Duration;

use crate::contract::ToolResult;
use rustyschwab::execution::OrderJournal;
use serde_json::{Value, json};
use turbomcp::McpResult;

/// The kind of activity entry a tool call writes.
pub const TOOL_CALL: &str = "tool_call";

/// Arguments longer than this, serialized, are noted as their size only.
const MAX_ARGUMENT_BYTES: usize = 4096;

/// Answers longer than this are not read for their source and order id.
const MAX_PARSED_BYTES: usize = 64 * 1024;

/// Characters of a refusal's message kept in the note.
const MAX_MESSAGE_CHARS: usize = 300;

/// Note one call of `tool` by `principal` in `journal`, on its own task.
pub(crate) fn note(
    journal: &OrderJournal,
    principal: &str,
    tool: &str,
    arguments: Value,
    result: &McpResult<ToolResult>,
    took: Duration,
) {
    let payload = describe(tool, arguments, result, took);
    let journal = journal.clone();
    let principal = principal.to_owned();
    tokio::spawn(async move {
        if let Err(e) = journal
            .record_activity(&principal, TOOL_CALL, payload)
            .await
        {
            tracing::debug!(error = %e, "a tool call was not noted in the activity log");
        }
    });
}

/// The `tool_call` payload: `tool`, `arguments`, `outcome` (`ok`, a failure
/// code, or `protocol_error`), `durationMs`, and when present `message`,
/// `source` and `orderId`.
pub fn describe(
    tool: &str,
    arguments: Value,
    result: &McpResult<ToolResult>,
    took: Duration,
) -> Value {
    let arguments = match serde_json::to_string(&arguments) {
        Ok(text) if text.len() <= MAX_ARGUMENT_BYTES => arguments,
        Ok(text) => json!({"omittedBytes": text.len()}),
        Err(_) => Value::Null,
    };
    let mut payload = json!({
        "tool": tool,
        "arguments": arguments,
        "durationMs": u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
    });
    let clip = |text: &str| text.chars().take(MAX_MESSAGE_CHARS).collect::<String>();
    match result {
        // An answer is the envelope as text; a large one (a chain, a
        // history) is not worth parsing for a note.
        Ok(result)
            if !result.is_error
                && crate::contract::first_text(result)
                    .is_some_and(|text| text.len() > MAX_PARSED_BYTES) =>
        {
            payload["outcome"] = "ok".into();
        }
        Ok(result) => {
            let reported = crate::contract::Reported::of(result);
            match &reported.failure {
                Some((code, message)) => {
                    payload["outcome"] = serde_json::to_value(code).unwrap_or(Value::Null);
                    payload["message"] = clip(message).into();
                }
                None if result.is_error => payload["outcome"] = "error".into(),
                None => payload["outcome"] = "ok".into(),
            }
            if let Some(source) = reported.body.as_ref().and_then(|b| b.get("source")) {
                payload["source"] = source.clone();
            }
            if let Some(order_id) = reported.order_id() {
                payload["orderId"] = order_id.into();
            }
        }
        Err(error) => {
            payload["outcome"] = "protocol_error".into();
            payload["message"] = clip(&error.to_string()).into();
        }
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A success as tools answer one: the envelope as JSON text.
    fn ok(content: Value) -> McpResult<ToolResult> {
        Ok(ToolResult::text(content.to_string()))
    }

    #[test]
    fn a_note_says_how_the_call_ended() {
        let placed = describe(
            "place_order",
            json!({"rationale": "breakout"}),
            &ok(json!({"source": "paper", "data": {"orderId": "41"}})),
            Duration::from_millis(12),
        );
        assert_eq!(placed["outcome"], "ok");
        assert_eq!(placed["orderId"], "41");
        assert_eq!(placed["source"], "paper");
        assert_eq!(placed["durationMs"], 12);
        assert_eq!(placed["arguments"]["rationale"], "breakout");

        let refused = describe(
            "place_order",
            json!({}),
            &Ok(crate::contract::ToolFailure::new(
                crate::contract::FailureCode::ApprovalRequired,
                "needs approval",
            )
            .into_result()),
            Duration::ZERO,
        );
        assert_eq!(refused["outcome"], "approval_required");
        assert_eq!(refused["message"], "needs approval");

        let big = describe(
            "get_quotes",
            json!({"symbols": "X".repeat(MAX_ARGUMENT_BYTES)}),
            &ok(json!({})),
            Duration::ZERO,
        );
        assert!(big["arguments"]["omittedBytes"].as_u64().unwrap() > MAX_ARGUMENT_BYTES as u64);
    }
}
