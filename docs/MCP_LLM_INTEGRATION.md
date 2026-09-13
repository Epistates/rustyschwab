# MCP and LLM integration review

Reviewed 2026-09-10. The MCP server now uses stable TurboMCP 3.3.0, with
explicit behavior annotations for all 32 tools (`build_order`, which expands
an `OrderSpec` locally, and `check_order`, the local pre-flight described
below, were added after the review). Reads and previews are marked
read-only; order and stream mutations are conservatively marked destructive
and non-idempotent. Cached stream reads are marked closed-world. These are
client hints, not authorization controls. Read-only does not imply public data.
Server discovery describes live trading, untrusted returned text, stale caches,
and the requirement to reconcile uncertain submissions before further action.

## Recommended integration boundary

Use MCP as the provider-neutral model interface. Keep model inference in the
host application; this server does not need model-provider credentials or an
embedded agent framework. Give each application a distinct principal and a
small tool allowlist. Research assistants normally need market-data reads;
account reads require separate account-scoped permission. Stream control is
stateful and should have one owner per Schwab login.

For network clients use authenticated Streamable HTTP, TLS termination, and
short-lived issuer tokens with the existing audience/issuer checks. Stdio is
appropriate for a trusted local host. WebSocket is a convenience transport,
not a substitute for standard MCP interoperability testing. See
[authorization and operations](OPERATIONS.md) and [shared authority](SHARED_AUTHORITY.md).

A model may propose an order, but operator-controlled policy and durable approval
must authorize the exact intent. A preview is neither approval nor a fill
prediction. Tool annotations, prompts, elicitation answers, and text in tool
results must never grant permission. Preserve decimal strings through the host;
do not round-trip prices through binary floating point. If execution is
uncertain, retain the attempt ID and reconcile; do not regenerate the request
or blindly retry a tool call. Treat descriptions and external market text as
untrusted data even when authenticated transport supplied them.

## Result contract

Every tool answers in one of two shapes (`crates/schwab-mcp/src/contract.rs`).

A success is compact JSON text. Data that came from Schwab or the stream
cache is wrapped as `{"asOf": <RFC 3339>, "source": "schwab_rest" |
"stream_cache", "data": <Schwab's raw shape, redacted>}`; local results
(`build_order`, stream control acknowledgements, `stream_status`) are bare.
A response that would exceed the transport's 1 MiB message limit is refused
with `response_too_large` rather than clipped.

A failure is a tool result with `isError: true` whose text and
`structuredContent` are both
`{"error": {"code", "retryable", "disposition", "message", "attemptId"?, "intentDigest"?}}`:

| `code` | `disposition` | `retryable` | Meaning |
| --- | --- | --- | --- |
| `invalid_arguments`, `not_found`, `risk_denied`, `response_too_large` | `fix_arguments` | false | Change the request; a smaller or bounded order may pass a risk denial |
| `not_permitted` | `not_permitted` | false | Policy or the server's trading switch forbids the call; do not retry |
| `approval_required` | `ask_operator` | false | Valid intent awaiting operator approval of `intentDigest` |
| `halted`, `internal` | `ask_operator` | false | Operator halt, or a fault logged under the reference id in `message` |
| `duplicate_intent`, `reconciliation_required` | `reconcile` | false | Read the order state first; never resubmit. `attemptId` names the uncertain submission |
| `unavailable`, `rate_limited`, `timeout` | `retry_later` | true | Nothing reached the broker; repeat the same call after a delay |

An unauthenticated Schwab session is `unavailable` with `ask_operator`, since
only the operator can re-run OAuth. Policy denials from the principal wrapper
use the same shape, so the model reads them instead of receiving a JSON-RPC
error; JSON-RPC errors are reserved for protocol faults. Schwab's own 4xx
rejection text (title and detail, describing the submitted request) is relayed
in `message`; every other upstream detail is logged under a reference id.
Client middleware that retries must key on `retryable`, never on `isError`.

## Query windows

The time-windowed reads (`get_account_orders`, `get_all_orders`,
`get_transactions`, `get_price_history`) take model-facing query types
(`crates/schwab-mcp/src/query.rs`) rather than the SDK's strict parameter
structs. A bound may be a calendar date (`YYYY-MM-DD`, covering that whole
UTC day), an RFC 3339 timestamp, a date-time without an offset (taken as
UTC), or epoch milliseconds. Orders and transactions default to the last
seven days ending now, and transactions to `TRADE`, so `params` may be
omitted. Unknown fields are refused rather than ignored, so a misspelled
bound cannot silently widen a window. The resolved request is still built
and validated by the SDK, and Schwab's one-year cap still applies.

## Pre-flight

`check_order(account_hash, order, order_id?)` evaluates a `place_order` (or,
with `order_id`, a `replace_order`) intent against everything this server
enforces locally and answers without contacting Schwab:

```json
{"allowed": false,
 "reasons": [{"code": "approval_required", "disposition": "ask_operator", "intentDigest": "…"}],
 "exposure": {"quantity": "2", "notional": "100"},
 "limits": {"maxQuantity": "2", "maxNotional": "100"},
 "requiresApproval": true, "approved": false,
 "intentDigest": "…", "tag": "mcp-…", "tradingEnabled": false}
```

`reasons` are the failures the submission would return, all of them, in the
order a dispatch finds them: argument validity, the principal's tool and
account permissions, risk limits, approval (policy or durable), the trading
switch, and operator halts or a recovery suspension. `intentDigest` is what
an operator approves with `schwab-admin approve`; `tag` is the order tag the
submission would carry. The verdict is advisory: every check is repeated on
submission, and a `duplicate_intent` is only detected there. Nothing is
audited or approved by checking. Grant `check_order` to any principal that
holds `place_order` or `replace_order`; without a principal (the local stdio
transport with trading off) `limits` is null and approval does not apply.
`preview_order` remains the way to obtain Schwab's own validation, fees and
buying-power impact.

## Remaining implementation work

| Priority | Gap | Completion criteria |
| --- | --- | --- |
| High | Paper mode in MCP | Inject an explicit trading backend into MCP; publish mode in discovery and results; prove paper startup requires no live credentials and cannot reach order endpoints. The existing paper authority/CLI does not make the live MCP wrapper paper-capable. |
| High | Bounded model context | Enforce response budgets and pagination for quotes, option chains, history and account lists; represent truncation explicitly and retain freshness/source information. Never silently truncate financial records. Oversized responses are now refused with `response_too_large`; projections and pagination remain. |
| High | Client interoperability | Exercise initialize, discovery, schemas, structured results, errors, cancellation, token rotation and cross-principal session denial with independent stdio and HTTP clients. Current regression tests are not a complete external conformance certification. |
| Medium | Model evaluation | Version scenarios for stale data, prompt injection, account confusion, denied approvals, repeated submissions and uncertain fills. Require zero unauthorized executions; record model/version/config, not just aggregate success. |
| Medium | Optional resources/prompts | Add account-scoped resources and workflow prompts only with matching lookup/call authorization and output budgets. Do not expose approval-granting tools to the proposing model. |

## TurboMCP v4 decision

The registry reports `4.0.0-alpha.4`, published alongside stable `3.3.0`.
V4 is a prerelease with new provider/lookup, dispatcher and session ownership
contracts. Its [migration guide](https://github.com/Epistates/turbomcp/blob/main/docs/DEPLOYMENT.md)
requires authoritative tool lookup, identity-bound sessions, and coordinated
HTTP session termination. It also validates input and structured output schemas.
The current `McpHandler` policy wrapper, direct core imports, HTTP router and
custom WebSocket dispatch must be ported together. A version-number-only upgrade
would not establish compatibility or preserve our authorization boundary.

Adopt v4 in a dedicated migration with the existing authorization regression
suite plus independent transport conformance, schema, overload, cancellation
and session-revocation tests. Assess whether its OAuth/session facilities can
replace custom plumbing while preserving account risk and approval rules.
No v4 runtime or model-provider integration was added in this pass.
