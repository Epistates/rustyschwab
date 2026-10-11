# Enterprise audit remediation

**Historical first-phase report.** The 0.2 candidate supersedes the JSON journal and static-policy implementation below. See [current enterprise operations](OPERATIONS.md) and [implementation status](ENTERPRISE_ROADMAP.md).

This document tracks remediation of the 18 findings in [the original audit](ENTERPRISE_AUDIT.md). The original report and diagnostic probes remain historical evidence; their line numbers describe the audited tree, not the revised implementation. This is an engineering remediation, not a certification of broker behavior or trading performance.

| Finding | Implemented behavior | Regression evidence |
| --- | --- | --- |
| F01 | Place/replace have one wire attempt; ambiguous outcomes retain durable pending intent and block further account submissions until reconciliation. Cancellation and process restart preserve intent. | `enterprise_regressions`: ambiguous submission, cancellation/restart, journal approval concurrency |
| F02 | Separate strict `OrderRequest` and tolerant `Order`; decimal execution amounts, stop/trailing parameters, child strategies, GTC spelling, numeric/string IDs, execution activities, and explicit tick validation. | `response_shapes`, `enterprise_regressions` |
| F03 | Correct option dates/multiplier/flat Greeks, expiration list, dividend dates, deliverables, large volumes; unknown market-data fields retained. | Response fixtures and risk-data regressions |
| F04 | Shared callback-state validator checks presence, match, expiry and single use before consuming the browser callback. | `auth::callback_regressions` |
| F05 | Foreground/background/401 refresh share one transaction and OS lock; preserve refresh-token expiry and persist before publishing. | Concurrent manager refresh, persistence/restart and expiry regressions |
| F06 | Structured broker errors retain HTTP status and correlation ID; 401 recovery refreshes the rejected token once, conditional on its generation. | Revoked-but-unexpired token and structured error regressions |
| F07 | Generation-aware breaker with cancellation-safe concurrent half-open permits and responsive 4xx handling. | Default recovery and cancelled-probe regressions |
| F08 | Quote deltas merge; snapshots carry timestamps, field update times, generation and freshness. Disconnect/generation change invalidates snapshots; old updates cannot overwrite newer data. Activity is bounded event history. | `stream_cache` tests |
| F09 | Bounded channels by default; overflow invalidates health, increments consumer-visible loss count and reconnects. Control acknowledgements are processed independently of delivery. | Slow-consumer overflow regression |
| F10 | Commit subscriptions only after acknowledgement; replay acknowledgements gate readiness; process all response items; enforce login timeout and cancel background tasks. | `stream_recovery` tests |
| F11 | Correct mover/projection query serialization, wrapped CUSIP response and account `type`. | HTTP boundary and response regressions |
| F12 | Every HTTP attempt consumes a shared credential/origin budget, including retries; logical request deadline includes auth, queue waits and backoff. Conflicting quota configurations are rejected. | Shared-budget and retry-deadline regressions |
| F13 | Mandatory network bearer authentication; immutable principal tool/account policies; isolated HTTP sessions/caches; exact decimal risk caps; optional single-use intent approval; durable principal audit. | Policy, network authentication/session isolation and journal tests |
| F14 | Stream status reports actual acknowledged readiness, generation and loss; account activity uses its special key and permission boundary; unsubscribe evicts snapshots. | Streaming/cache/policy regressions |
| F15 | Atomic restrictive file writes, Unix directory sync, Windows protected owner/SYSTEM DACL and replacement; OS locks coordinate refresh and encryption-key creation. | Storage tests; Windows helper cross-compile; native Windows test in CI |
| F16 | Remove raw malformed frames, account-bearing HTTP paths and OAuth response bodies from logs; MCP errors expose safe classes and reconciliation IDs. | Synthetic secret/error-marker and redaction tests |
| F17 | Minimum Rust 1.89 declared and checked; CI pins 1.89.0 and tests Linux/macOS/Windows plus full features. | MSRV, format, Clippy and rustdoc checks |
| F18 | Updated dependency lockfile, TLS callback server and decimal dependency; fresh RustSec scan required. | Saved dependency scan and feature checks |

## Migration and operation

Use `schwab_types::trading::OrderRequest` for place, replace and preview. `Order` remains the broker response type. Unknown request fields are errors. Supply execution quantities and prices as decimal strings (for example `"0.01"`), especially through JSON/MCP; numeric JSON may already have lost precision before deserialization. The SDK never rounds a price. `validate_price_increment` accepts a caller-supplied asset tick; broker eligibility and buying power remain broker checks. Review generated schemas and update callers for the corrected response field types and `StreamMessage::State` variant.

The default SDK order journal is next to the token file, with extension `orders.json`. Its intent is written before submitting. `SubmissionUnknown` means the broker may have accepted the order; do not replay it. Query orders/executions and reconcile using `client.order_journal().finish(attempt_id, SubmissionOutcome::Reconciled { evidence })`. Record the broker IDs, query time, and evidence supporting the conclusion. Cancellation tools remain separately controllable while submissions are blocked. An accepted submission is not a fill; even an accepted response without a Location header must be followed through broker queries.

Keep token files, encryption keys, lock files, submission journals and principal audit journals in a private persistent directory. All processes sharing credentials must share the same token/journal paths. File locks require a filesystem that implements locking and atomic replacement correctly. The journal is durable local JSON, not a replicated ledger or tamper-proof compliance archive: it rewrites its history on each transition. Export records to the organization's audit system and benchmark storage latency at the intended order rate. Do not delete unresolved records or consumed approval keys to reclaim space. Filesystem operations can exceed the network deadline; monitor disk health.

Rate budgets are shared across clients **within a process**, with a rolling minute cap of `requests_per_second * 60` plus the configured short-term quota. Multiple replicas require a credential-level shared gateway/quota service. Disabling rate limiting is an explicit caller override. Broker limits must be set for the actual application entitlement.

Streaming defaults to 4,096 queued messages. The legacy `ChannelKind::Unbounded` configuration is capped at 65,536. A consumer must handle `State`, clear derived state on loss/generation change, and obtain an authoritative REST baseline before resuming trading decisions. Reconnection restores acknowledged subscriptions, not missing executions. MCP exposes activity history with loss/eviction and `reconciliationRequired`; it does not automatically certify a reconciled account. Snapshot `fresh` requires current session health and update ages no greater than two seconds, including retained numeric fields. Check required fields and broker timestamps as well: a sparse quote is not a complete book. Local receive freshness does not prove exchange freshness.

## MCP policy

Network transports require `SCHWAB_MCP_POLICY_FILE`, a JSON array of principals. For stdio with a policy, set `SCHWAB_MCP_PRINCIPAL`. Without a policy, only read-only stdio is allowed. Place/replace also require `SCHWAB_MCP_ALLOW_TRADING=1`; cancellation can be controlled independently with `SCHWAB_MCP_ALLOW_CANCEL=1`.

Example policy (replace the digest with the SHA-256 of a random bearer secret of at least 32 characters):

```json
[
  {
    "name": "desk",
    "token_sha256": "REPLACE_WITH_64_HEX_CHARACTERS",
    "tools": ["get_server_status", "get_account", "get_account_orders", "get_order", "get_quote", "check_order", "preview_order", "place_order", "cancel_order"],
    "accounts": ["BROKER_ACCOUNT_HASH"],
    "max_quantity": "10",
    "max_notional": "1000",
    "require_approval": true,
    "approved_intents": [],
    "max_calls_per_minute": 120
  }
]
```

`max_calls_per_minute` (optional) caps this principal's tool calls, bursting to a tenth of the cap; a call over it is `rate_limited`. Every principal shares one Schwab request budget, of which reads may use 75% and reconciliation 90%, so writes and baselines always keep headroom; the per-principal cap keeps one principal from crowding out the others' reads. A write tool's arguments must be ones its schema declares (an extra key would change the intent digest unseen), and a tool that does not exist is a JSON-RPC error, while one the principal is not granted is a `not_permitted` result.

`dead_man_secs` (optional, 30 to 86400, and only for a principal holding `place_order` or `replace_order`) arms a dead-man switch: after that many seconds with no tool call by the principal, the server cancels the working orders that principal placed, found from its audit journal and matched to the venue's working orders. Any tool call, `get_server_status` included, resets the timer; one watcher serves the principal across sessions, transports and policy reloads; it fires once per silence and re-arms on the next call. It is the operator's own control, so it cancels whether or not the principal holds `cancel_order` and regardless of `SCHWAB_MCP_ALLOW_CANCEL`. Each firing is a `dead_man_cancel` event in the principal's audit journal and counts in `schwab_mcp_dead_man_firings_total`; `get_server_status` reports `deadMan` (`windowSecs`, `secondsLeft`). See [OPERATIONS.md](OPERATIONS.md#dead-man-switch).

Policy, grants, identity, risk and paper configuration files are read through a trusted-file check: on Unix the file (followed through symlinks, so ConfigMap mounts work) and its directory must be owned by the server's account or root and writable by no one else. Reloads read the policy off the async workers with the same check.

Send `Authorization: Bearer <secret>` on every HTTP request or WebSocket handshake. Every request is served as the principal its own credential names, so a session ID or connection cannot cross identities; on the stateless `2026-07-28` revision there are no sessions at all. HTTP requests have a 120-second timeout. WebSocket connections allow at most 32 concurrent calls and 1 MiB messages; limit connections at the TLS proxy. Native WebSocket clients are supported; browser Origin headers are rejected. Bind to loopback behind a TLS proxy for remote use, with gateway connection/rate limits. Non-loopback binding requires `SCHWAB_MCP_ALLOW_REMOTE=1` as well as bearer policy. The HTTP transport retains its Origin validation.

Operator approval is the SHA-256 returned by `schwab_mcp::policy::intent_digest(tool_name, &arguments)`, binding the exact arguments including account and order. Add that digest to `approved_intents` and restart to load the immutable policy. The first dispatch atomically consumes it in the durable audit journal. Use a distinct order `tag` for a separately reviewed intent. A `rationale` argument is journaled beside the dispatch but excluded from the digest, so approval binds the order and not the words. Each finished audit intent records how the dispatch ended (the venue order id, or the failure code); an interrupted MCP dispatch may leave one pending. Inspect both that journal and the SDK journal before trusted reconciliation; `get_trade_journal` reads the principal's own journal for one account, and `schwab-admin` exports all of it. No MCP tool can approve itself or clear its audit history.

Restricted principals cannot call aggregate linked-account endpoints or access account activity. Only one configured principal may use streaming tools for a shared broker stream identity. Separate broker credentials/deployments are required for independently owned stream sessions.

The built-in risk policy is deliberately narrow: bounded-price, single-leg equity/option orders with positive quantity and gross notional caps, and defined-risk option strategies (one underlying, one expiration, net-priced) counted at the most they can lose at expiration; opening short positions on their own, calendars and unbounded strategies require an external portfolio risk engine. Options require an operator-verified multiplier per exact symbol. Child strategies are summed into the same gross figures, so a bracket counts entry + take-profit + stop-loss; the stop-loss is the one unbounded order the policy accepts, because a child that only flattens the long position its bounded parent opens cannot fill for more than the parent paid and is priced at the parent's limit. Quantity/notional caps do not implement portfolio margin, position limits or regulatory suitability. The SDK retains broader order expressiveness for applications implementing those checks.

## Remaining release validation

Local mock tests prove the specified client behavior. Before production release, run native Windows ACL tests and the full CI matrix, authenticated broker conformance tests for the deployed entitlements/asset classes, process-kill and storage-failure exercises, and a sustained stream/load soak at the intended traffic rate. These require external environments or measured operational evidence and are not represented as completed by local tests. Compare broker payloads with sanitized reference fixtures whenever the API changes. No live orders were submitted during remediation.

## Local verification evidence

On 2026-09-09: **161 tests passed, zero failed or ignored** with all workspace targets and features. Clippy with warnings denied, rustdoc with warnings denied, formatting, minimal SDK feature compilation, and Rust 1.89.0 all-feature compilation passed. The Windows file-security module cross-compiled for `x86_64-pc-windows-msvc`; native execution remains a CI gate. The fresh RustSec scan reported **zero advisories and zero warnings**. Logs were kept locally under `docs/audit/remediation/` (git-ignored).

The decimal dependency minimum is 1.43: upstream removed its legacy optional rkyv dependency in that release ([upstream release documentation](https://docs.rs/crate/rust_decimal/1.43.0)). Earlier cached versions reintroduced a lockfile advisory even though that optional feature was inactive.
