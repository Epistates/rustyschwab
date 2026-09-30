//! The tool result contract every tool and the policy layer share.
//!
//! A success is JSON text; responses that came from Schwab or the stream
//! cache are wrapped as `{"asOf", "source", "data"}` so the model always
//! knows how old the data is and where it came from, while `data` keeps
//! Schwab's raw shape. Local computations (`build_order`, stream control
//! acknowledgements) are returned bare.
//!
//! A failure is a tool result with `isError: true` whose text and
//! `structuredContent` are `{"error": {code, retryable, disposition,
//! message, attemptId?, intentDigest?}}`. `code` is a closed vocabulary,
//! `retryable` says whether the same call may be repeated after a delay
//! (only when nothing reached the broker), and `disposition` names the
//! model's next move. Protocol-level JSON-RPC errors are reserved for
//! protocol faults (unknown tool, malformed arguments); every business
//! outcome is a tool result so the model can read it and self-correct.
//!
//! Upstream detail crosses this boundary only when it is addressed to the
//! caller: SDK guardrail messages and Schwab's own 4xx rejection text
//! (which describes the submitted request, never credentials). Anything
//! else is logged under a reference id and replaced with an opaque message.

use std::future::Future;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use turbomcp::prelude::*;

use crate::redact;

/// Where a successful response's `data` came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    SchwabRest,
    StreamCache,
    /// The paper venue: simulated accounts, orders, fills and its own quotes.
    Paper,
    Local,
}

/// Closed failure vocabulary. Add a variant rather than overloading one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    /// The arguments are malformed or violate a static rule; change them.
    InvalidArguments,
    /// The referenced order, account or instrument does not exist.
    NotFound,
    /// Policy forbids the call for this principal or server; do not retry.
    NotPermitted,
    /// The intent is valid but needs an operator's approval of its digest.
    ApprovalRequired,
    /// The identical intent was already submitted; inspect it before deciding.
    DuplicateIntent,
    /// A risk limit rejected the order; a smaller or bounded order may pass.
    RiskDenied,
    /// An operator halt is active; only the operator can clear it.
    Halted,
    /// A dependency is temporarily unavailable; nothing reached the broker.
    Unavailable,
    /// The request budget is exhausted; wait and retry.
    RateLimited,
    /// The call timed out before anything was sent.
    Timeout,
    /// A submission may have reached the broker; reconcile, never resubmit.
    ReconciliationRequired,
    /// The response exceeds the message budget; narrow the request.
    ResponseTooLarge,
    /// An unexpected server-side failure, logged under `message`'s reference.
    Internal,
}

/// The model's next move after a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// Change the arguments and call again.
    FixArguments,
    /// Wait, then repeat the same call; nothing was sent.
    RetryLater,
    /// Read current state (`get_order_status`, `get_account_orders`) before
    /// deciding anything; do not resubmit blindly.
    Reconcile,
    /// Blocked by operator control; the model cannot clear it.
    AskOperator,
    /// Forbidden by policy; do not retry.
    NotPermitted,
}

/// The wire spelling (`risk_denied`), so logs and audit evidence read as
/// the contract does.
impl std::fmt::Display for FailureCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match serde_json::to_value(self) {
            Ok(Value::String(name)) => f.write_str(&name),
            _ => write!(f, "{self:?}"),
        }
    }
}

impl std::str::FromStr for FailureCode {
    type Err = ();
    fn from_str(name: &str) -> Result<Self, ()> {
        serde_json::from_value(Value::String(name.to_owned())).map_err(|_| ())
    }
}

impl FailureCode {
    /// Default disposition; `Auth` failures override it to `AskOperator`.
    fn disposition(self) -> Disposition {
        match self {
            Self::InvalidArguments | Self::NotFound | Self::RiskDenied | Self::ResponseTooLarge => {
                Disposition::FixArguments
            }
            Self::NotPermitted => Disposition::NotPermitted,
            Self::ApprovalRequired | Self::Halted | Self::Internal => Disposition::AskOperator,
            Self::DuplicateIntent | Self::ReconciliationRequired => Disposition::Reconcile,
            Self::Unavailable | Self::RateLimited | Self::Timeout => Disposition::RetryLater,
        }
    }
    fn retryable(self) -> bool {
        matches!(self, Self::Unavailable | Self::RateLimited | Self::Timeout)
    }
    /// A submission refused with this code never reached the broker, and the
    /// same intent may succeed later, so it gives back the one-use approval
    /// it consumed. (A submission that may have been sent is always
    /// `reconciliation_required`.)
    pub fn keeps_approval(self) -> bool {
        self.retryable() || matches!(self, Self::Halted | Self::RiskDenied)
    }
}

/// [`FailureCode::keeps_approval`] for an SDK error, as the authority sees it.
pub fn refusal_keeps_approval(kind: schwab_rs::ErrorKind) -> bool {
    use schwab_rs::ErrorKind;
    matches!(
        kind,
        ErrorKind::Halted
            | ErrorKind::Suspended
            | ErrorKind::RiskDenied
            | ErrorKind::BaselineStale
            | ErrorKind::CircuitOpen
            | ErrorKind::OwnershipLost
            | ErrorKind::LeaseBusy
            | ErrorKind::Auth
            | ErrorKind::RateLimit
            | ErrorKind::Timeout
    )
}

/// A failure the model can act on. Build with the code constructors so
/// `retryable` and `disposition` stay consistent with `code`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolFailure {
    pub code: FailureCode,
    pub retryable: bool,
    pub disposition: Disposition,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent_digest: Option<String>,
}

impl ToolFailure {
    pub fn new(code: FailureCode, message: impl Into<String>) -> Self {
        Self {
            code,
            retryable: code.retryable(),
            disposition: code.disposition(),
            message: message.into(),
            attempt_id: None,
            intent_digest: None,
        }
    }
    pub fn invalid_arguments(message: impl Into<String>) -> Self {
        Self::new(FailureCode::InvalidArguments, message)
    }
    pub fn not_permitted(message: impl Into<String>) -> Self {
        Self::new(FailureCode::NotPermitted, message)
    }
    /// Log the cause under a fresh reference id and return an opaque failure.
    pub fn internal(context: &'static str, cause: impl std::fmt::Display) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        tracing::warn!(error_id = %id, %cause, "{context}");
        Self::new(FailureCode::Internal, format!("{context}; reference {id}"))
    }
    /// No Schwab session: `unavailable`, but only the operator can re-run
    /// OAuth, so it is not retryable and the disposition is `ask_operator`.
    pub fn unauthenticated(message: impl Into<String>) -> Self {
        let mut failure = Self::new(FailureCode::Unavailable, message);
        failure.retryable = false;
        failure.disposition = Disposition::AskOperator;
        failure
    }
    pub fn with_attempt(mut self, attempt_id: impl Into<String>) -> Self {
        self.attempt_id = Some(attempt_id.into());
        self
    }
    pub fn with_intent(mut self, digest: impl Into<String>) -> Self {
        self.intent_digest = Some(digest.into());
        self
    }
    /// The `isError` tool result: text for the model, `structuredContent`
    /// for client middleware that must honour `retryable` without parsing.
    pub fn into_result(self) -> ToolResult {
        let body = json!({ "error": self });
        let mut result = ToolResult::error(body.to_string());
        result.structured_content = Some(body);
        result
    }
}

impl From<schwab_rs::Error> for ToolFailure {
    fn from(e: schwab_rs::Error) -> Self {
        use schwab_rs::{Error, ErrorKind};
        match &e {
            Error::SubmissionUnknown { attempt_id, .. }
            | Error::ReconciliationRequired { attempt_id } => {
                return Self::new(
                    FailureCode::ReconciliationRequired,
                    "Submission outcome is unknown; do not resubmit. Reconcile the order \
                     state, or ask the operator to.",
                )
                .with_attempt(attempt_id);
            }
            Error::Broker {
                status, details, ..
            } if status.is_client_error() => {
                let code = match status.as_u16() {
                    404 => FailureCode::NotFound,
                    401 | 403 => FailureCode::NotPermitted,
                    429 => FailureCode::RateLimited,
                    _ => FailureCode::InvalidArguments,
                };
                let reasons: Vec<String> = details
                    .iter()
                    .take(8)
                    .map(ToString::to_string)
                    .filter(|text| !text.is_empty())
                    .collect();
                let mut message = format!("Schwab rejected the request (HTTP {status})");
                if !reasons.is_empty() {
                    message.push_str(": ");
                    // Schwab's prose is not redacted like its structured
                    // payloads; mask anything shaped like an account number.
                    let reasons = crate::redact::mask_accounts_in_text(&reasons.join("; "));
                    message.push_str(&truncate(&reasons, 1024));
                }
                return Self::new(code, message);
            }
            Error::Auth(_) => {
                let logged = Self::internal("Schwab session is not authenticated", &e);
                return Self::unauthenticated(logged.message);
            }
            Error::Validation(invalid) => return Self::from(invalid.clone()),
            _ => {}
        }
        match e.kind() {
            ErrorKind::InvalidParameter => Self::invalid_arguments(e.to_string()),
            ErrorKind::Duplicate => Self::new(
                FailureCode::DuplicateIntent,
                format!(
                    "{e}. Inspect the existing order before deciding; to submit it again \
                         deliberately, change the order or set a distinct `tag`."
                ),
            ),
            ErrorKind::RiskDenied => Self::new(FailureCode::RiskDenied, e.to_string()),
            ErrorKind::ApprovalInvalid => Self::new(FailureCode::ApprovalRequired, e.to_string()),
            ErrorKind::Halted => Self::new(FailureCode::Halted, e.to_string()),
            ErrorKind::BaselineStale
            | ErrorKind::Suspended
            | ErrorKind::CircuitOpen
            | ErrorKind::OwnershipLost
            | ErrorKind::LeaseBusy => Self::new(FailureCode::Unavailable, e.to_string()),
            ErrorKind::RateLimit => Self::new(FailureCode::RateLimited, e.to_string()),
            ErrorKind::Timeout => Self::new(FailureCode::Timeout, e.to_string()),
            _ if e.is_retryable() => {
                let mut failure = Self::internal("Schwab is temporarily unavailable", &e);
                failure.code = FailureCode::Unavailable;
                failure.retryable = true;
                failure.disposition = Disposition::RetryLater;
                failure
            }
            _ => Self::internal("Schwab API operation failed", &e),
        }
    }
}

/// Residual `McpError`s (argument parsing, local helpers) map by kind.
/// A request that failed its own validation: change the named field.
impl From<schwab_types::ValidationError> for ToolFailure {
    fn from(e: schwab_types::ValidationError) -> Self {
        match e.field() {
            Some(field) => Self::invalid_arguments(format!("{e} (field `{field}`)")),
            None => Self::invalid_arguments(e.to_string()),
        }
    }
}

impl From<McpError> for ToolFailure {
    fn from(e: McpError) -> Self {
        use turbomcp_core::ErrorKind;
        match e.kind {
            ErrorKind::InvalidParams | ErrorKind::ParseError => Self::invalid_arguments(e.message),
            ErrorKind::PermissionDenied | ErrorKind::Authentication => {
                Self::not_permitted(e.message)
            }
            ErrorKind::RateLimited => Self::new(FailureCode::RateLimited, e.message),
            ErrorKind::Timeout => Self::new(FailureCode::Timeout, e.message),
            ErrorKind::Unavailable => Self::new(FailureCode::Unavailable, e.message),
            _ => Self::internal("MCP operation failed", e),
        }
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Wrap a Schwab or stream-cache response with its provenance, redacted.
pub fn envelope<T: Serialize>(source: Source, value: T) -> Value {
    let mut data = serde_json::to_value(value).unwrap_or(Value::Null);
    redact::apply(&mut data);
    json!({
        "asOf": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "source": source,
        "data": data,
    })
}

/// A Schwab REST response with its provenance, redacted.
pub fn rest<T: Serialize>(value: T) -> Value {
    envelope(Source::SchwabRest, value)
}

/// A locally computed payload, redacted, with no provenance envelope.
pub fn local(mut value: Value) -> Value {
    redact::apply(&mut value);
    value
}

/// Run one tool body: record `schwab_mcp_tool_calls_total{tool,outcome}`
/// and the duration histogram, encode the success compactly, and turn a
/// [`ToolFailure`] into its `isError` result. Never returns `Err`, so the
/// transport carries every outcome as a tool result.
///
/// The body is boxed here, once, so every tool's future is a pointer
/// wide. The `#[server]` macro dispatches all tools from one `async fn`
/// whose state and, in unoptimized builds, stack frame hold every tool's
/// future side by side; left inline, the bodies' client calls made that
/// frame close to a megabyte and the deeper tools overflowed a 2 MiB
/// thread.
pub(crate) fn run(
    tool: &'static str,
    body: impl Future<Output = Result<Value, ToolFailure>> + Send,
) -> impl Future<Output = McpResult<ToolResult>> + Send {
    let body = Box::pin(body);
    async move {
        let timer = crate::metrics::ToolTimer::start(tool);
        let failure = match body.await {
            Ok(value) => match encode(&value) {
                Ok(text) => {
                    timer.success();
                    return Ok(ToolResult::text(text));
                }
                Err(failure) => failure,
            },
            Err(failure) => failure,
        };
        tracing::debug!(tool, code = ?failure.code, "tool call failed");
        Ok(failure.into_result())
    }
}

/// Bring an argument-extraction failure into the contract.
///
/// The `#[tool]` macro answers a malformed `arguments` object with a bare
/// text `isError` result before the tool body runs. Every body in this
/// crate goes through `run`, which always attaches `structuredContent`
/// to a failure, so an error result without it can only be that case and
/// is re-issued as `invalid_arguments`.
pub fn normalize(result: ToolResult) -> ToolResult {
    if result.is_error() && result.structured_content.is_none() {
        let message = result
            .first_text()
            .unwrap_or("Arguments did not match the tool's input schema");
        return ToolFailure::invalid_arguments(message).into_result();
    }
    result
}

/// What a tool result reports, read back through the contract: the failure
/// an `isError` result carries, or the parsed JSON text of a success. The
/// policy layer attributes a dispatch with it and the evaluation harness
/// scores with it, so neither parses result text on its own.
#[derive(Debug, Default)]
pub struct Reported {
    /// The failure and its message, when the result is an error with a
    /// contract code.
    pub failure: Option<(FailureCode, String)>,
    /// The success text as JSON; `None` for an error or non-JSON text.
    pub body: Option<Value>,
}

impl Reported {
    pub fn of(result: &ToolResult) -> Self {
        if result.is_error() {
            let failure = result.structured_content.as_ref().and_then(|content| {
                let error = &content["error"];
                let code = serde_json::from_value(error["code"].clone()).ok()?;
                let message = error["message"].as_str().unwrap_or_default().to_owned();
                Some((code, message))
            });
            return Self {
                failure,
                body: None,
            };
        }
        Self {
            failure: None,
            body: result
                .first_text()
                .and_then(|text| serde_json::from_str(text).ok()),
        }
    }

    /// The venue order id a submission (`data.orderId`) or a cancellation
    /// (`data.order_id`) reported.
    pub fn order_id(&self) -> Option<&str> {
        let data = &self.body.as_ref()?["data"];
        data["orderId"]
            .as_str()
            .or_else(|| data["order_id"].as_str())
    }
}

/// Compact JSON text, refused rather than clipped when it would exceed the
/// transport's message limit.
fn encode(value: &Value) -> Result<String, ToolFailure> {
    let text = value.to_string();
    let limit = turbomcp_core::MAX_MESSAGE_SIZE;
    if text.len() > limit {
        return Err(ToolFailure::new(
            FailureCode::ResponseTooLarge,
            format!(
                "Response is {} bytes, over the {limit} byte limit; narrow the request \
                 (fewer symbols or strikes, a shorter window, or a projection tool)",
                text.len()
            ),
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispositions_and_retryability_follow_the_code() {
        let f = ToolFailure::new(FailureCode::RateLimited, "slow down");
        assert!(f.retryable);
        assert_eq!(f.disposition, Disposition::RetryLater);
        let f = ToolFailure::from(schwab_rs::Error::SubmissionUnknown {
            attempt_id: "a1".into(),
            source: Box::new(schwab_rs::Error::ConnectionClosed),
        });
        assert_eq!(f.code, FailureCode::ReconciliationRequired);
        assert!(!f.retryable);
        assert_eq!(f.disposition, Disposition::Reconcile);
        assert_eq!(f.attempt_id.as_deref(), Some("a1"));
        let f = ToolFailure::from(schwab_rs::Error::Duplicate("Order tag reused".into()));
        assert_eq!(f.code, FailureCode::DuplicateIntent);
        assert_eq!(f.disposition, Disposition::Reconcile);
    }

    #[test]
    fn failure_result_is_structured_and_flagged() {
        let result = ToolFailure::not_permitted("Account not authorized")
            .with_intent("abc")
            .into_result();
        assert!(result.is_error());
        let structured = result.structured_content.clone().unwrap();
        assert_eq!(structured["error"]["code"], "not_permitted");
        assert_eq!(structured["error"]["disposition"], "not_permitted");
        assert_eq!(structured["error"]["retryable"], false);
        assert_eq!(structured["error"]["intentDigest"], "abc");
        assert!(structured["error"].get("attemptId").is_none());
        let text: Value = serde_json::from_str(result.first_text().unwrap()).unwrap();
        assert_eq!(text, structured);
    }

    #[test]
    fn upstream_messages_are_not_exposed_to_tools() {
        let error = ToolFailure::from(schwab_rs::Error::Unknown(
            "secret-token-abc account-12345 /private/username".into(),
        ));
        assert_eq!(error.code, FailureCode::Internal);
        for marker in ["secret-token-abc", "account-12345", "/private/username"] {
            assert!(!error.message.contains(marker));
        }
        let auth = ToolFailure::from(schwab_rs::Error::Auth(
            schwab_rs::error::AuthError::TokenExpired,
        ));
        assert_eq!(auth.code, FailureCode::Unavailable);
        assert_eq!(auth.disposition, Disposition::AskOperator);
        assert!(!auth.retryable);
    }

    #[test]
    fn schwab_rejections_relay_their_reasons() {
        let rejected = schwab_rs::Error::Broker {
            status: reqwest::StatusCode::BAD_REQUEST,
            details: vec![schwab_rs::error::ApiError {
                id: None,
                status: Some("400".into()),
                title: Some("Bad Request".into()),
                detail: Some("Order price is not in a valid increment".into()),
                source: None,
            }],
            request_id: None,
        };
        let failure = ToolFailure::from(rejected);
        assert_eq!(failure.code, FailureCode::InvalidArguments);
        assert!(
            failure.message.contains("valid increment"),
            "{}",
            failure.message
        );
        let missing = ToolFailure::from(schwab_rs::Error::Broker {
            status: reqwest::StatusCode::NOT_FOUND,
            details: vec![],
            request_id: None,
        });
        assert_eq!(missing.code, FailureCode::NotFound);
        let outage = ToolFailure::from(schwab_rs::Error::Broker {
            status: reqwest::StatusCode::BAD_GATEWAY,
            details: vec![],
            request_id: None,
        });
        assert_eq!(outage.code, FailureCode::Unavailable);
        assert!(outage.retryable);
    }

    #[test]
    fn envelope_carries_provenance_and_redacts() {
        let value = envelope(
            Source::SchwabRest,
            json!({"accountNumber": "123456789", "streamerSocketUrl": "wss://x"}),
        );
        assert_eq!(value["source"], "schwab_rest");
        assert!(value["asOf"].as_str().unwrap().ends_with('Z'));
        assert_eq!(value["data"]["accountNumber"], "***6789");
        assert!(value["data"].get("streamerSocketUrl").is_none());
    }

    #[tokio::test]
    async fn oversized_successes_are_refused_not_clipped() {
        let huge = json!({"blob": "x".repeat(turbomcp_core::MAX_MESSAGE_SIZE)});
        let result = run("test", async { Ok(huge) }).await.unwrap();
        assert!(result.is_error());
        assert_eq!(
            result.structured_content.unwrap()["error"]["code"],
            "response_too_large"
        );
        let result = run("test", async { Ok(json!({"ok": true})) })
            .await
            .unwrap();
        assert!(!result.is_error());
        assert_eq!(result.first_text(), Some(r#"{"ok":true}"#));
    }
}
