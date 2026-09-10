# MCP and LLM integration review

Reviewed 2026-09-10. The MCP server now uses stable TurboMCP 3.3.0, with
explicit behavior annotations for all 30 tools. Reads and previews are marked
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

## Remaining implementation work

| Priority | Gap | Completion criteria |
| --- | --- | --- |
| High | Paper mode in MCP | Inject an explicit trading backend into MCP; publish mode in discovery and results; prove paper startup requires no live credentials and cannot reach order endpoints. The existing paper authority/CLI does not make the live MCP wrapper paper-capable. |
| High | Structured failure contract | Return bounded typed error codes, retry/reconciliation disposition and attempt IDs; verify uncertain mutations cannot be retried by client middleware. Current MCP failures primarily contain sanitized text. |
| High | Bounded model context | Enforce response budgets and pagination for quotes, option chains, history and account lists; represent truncation explicitly and retain freshness/source information. Never silently truncate financial records. |
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
