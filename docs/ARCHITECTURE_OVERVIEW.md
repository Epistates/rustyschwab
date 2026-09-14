# Architecture overview

How the workspace is put together and where each responsibility lives. This
describes the code as it is; for how to run it see the [README](../README.md),
[operations](OPERATIONS.md), [shared authority](SHARED_AUTHORITY.md) and
[paper trading](PAPER_TRADING.md).

## Workspace

Three published crates plus example packages, all on the workspace version
(`0.2.0` candidate, Rust 2024 edition, MSRV 1.89, `MIT OR Apache-2.0`):

| Crate | Role | Key dependencies |
|---|---|---|
| `schwab-types` | Wire types only. No I/O, no async. | `serde`, `serde_with`, `rust_decimal`, `chrono`; optional `schemars` |
| `schwab-rs` | The SDK: OAuth, REST client, streaming client, durable execution state, paper engine, remote backends. | `tokio`, `reqwest` (rustls), `tokio-tungstenite`, `rusqlite` (bundled), `governor`, `tracing`, `metrics`, `chacha20poly1305`, `keyring`, `secrecy` |
| `schwab-mcp` | MCP server over the SDK, plus the operator, authority, gateway and paper CLIs. | `turbomcp` 3.3, `axum`, `jsonwebtoken`, `metrics-exporter-prometheus` |

`examples/` holds workspace packages (`oauth-flow`, `auth_test`,
`comprehensive`, `streaming-examples`, `enterprise-bench`); run them with
`cargo run -p <package>`, not `cargo run --example`.

```
                 ┌──────────────────────────── schwab-mcp ────────────────────────────┐
                 │ server.rs (39 tools)  policy.rs  network.rs  identity.rs  redact.rs │
                 │ backend.rs (live | paper)  stream_cache.rs  status.rs               │
                 │ projection.rs  portfolio.rs  order_status.rs  journal.rs             │
                 │ guidance.rs (1 resource, 1 prompt)  authority.rs (gateway)           │
                 │ bins: admin/authority/remote/paper/paper-verify                      │
                 └───────────────┬─────────────────────────────────┬──────────────────┘
                                 │ SchwabClient / StreamClient      │ TradingBackend / PaperBroker
┌────────────────────────────────┴─────────────────────────────────┴──────────────────┐
│ schwab-rs                                                                            │
│  auth/        client.rs      streaming/     execution.rs  coordination.rs  risk.rs   │
│  transport/   retry.rs       reconciliation.rs  recovery.rs  readiness.rs           │
│  circuit_breaker.rs  request_budget.rs  storage.rs (SQLite)  audit_delivery.rs      │
│  broker.rs (TradingBackend)  paper/  remote.rs  security.rs  metrics.rs telemetry.rs │
└──────────────────────────────────────────┬──────────────────────────────────────────┘
                                           │
┌──────────────────────────────────────────┴──────────────────────────────────────────┐
│ schwab-types: market_data  trading (+ builder, spec)  accounts  streaming (+ payload) │
└─────────────────────────────────────────────────────────────────────────────────────┘
```

## schwab-types

One module per Schwab API area: `market_data`, `trading`, `accounts`,
`streaming`, `common`. Design rules:

- **Strict out, tolerant in.** Types the SDK sends (`trading::OrderRequest`,
  `OrderRequestLeg`, `OrderRequestInstrument`) use `deny_unknown_fields`
  and exact `Decimal` prices/quantities. Types Schwab returns (`Order`,
  `Account`, quotes, stream payloads, …) accept unknown fields and use `f64`
  where Schwab emits numbers. That is deliberate, not a shortcut: Schwab
  sends JSON numbers, `serde_json` parses them to `f64` before any `Decimal`
  deserializer runs, and `rust_decimal` then re-parses the shortest
  round-trip string, so a `Decimal` response field would carry exactly the
  same value. Turning on `serde_json`'s `arbitrary_precision` to change that
  would replace numbers inside `serde_json::Value` with private maps and
  break `canonical_json` intent digests. Prices become `Decimal` where they
  enter exact accounting (`paper::feed` converts via `to_string().parse()`).
- **`Decimal` is re-exported** (`schwab_types::Decimal`, also reachable as
  `schwab_rs::types::Decimal`) so callers do not depend on `rust_decimal`
  directly. Decimal fields deserialize from strings or numbers and serialize
  as strings.
- **Order construction** lives in `trading::builder` (constructors such as
  `OrderRequest::limit`, `one_cancels_other`, `bracket`; `OptionSymbol` for
  the OSI contract format) and `trading::spec` (`OrderSpec`, a JSON
  composition keyed by `kind` that expands to the same `OrderRequest`).
  `OrderRequest::validate` checks structure before anything leaves the
  process. See [ORDERS.md](ORDERS.md).
- **Streaming payloads** arrive with numbered fields; `streaming::payload`
  holds the per-service field tables and typed `StreamPayload` variants
  (`DataItem::decode`). Enum spellings for the Trader API were verified
  against Schwab's documentation and the Schwabdev 3.0.5 reference client
  (see the CHANGELOG "wire-shape audit" entries).
- The `schemars` feature derives `JsonSchema` on the input types so the MCP
  crate can publish tool schemas without hand-written JSON.

## schwab-rs

### Configuration and authentication

`config::SchwabConfig` bundles `OAuthConfig`, `ClientConfig` (base URL,
timeout, retry, rate limit, circuit breaker) and an optional `StreamConfig`;
`SchwabConfig::from_env` reads the `SCHWAB_*` variables listed in
[`.env.example`](../.env.example). `validate()` runs on construction.

`auth::AuthManager` owns the OAuth authorization-code flow, the token set
and refresh. Tokens are `SecretString`s held in a `TokenSet`; persistence
goes through the `TokenStore` trait with three backends
(`FileTokenStore` 0600 JSON, `EncryptedFileTokenStore` ChaCha20-Poly1305,
`KeychainTokenStore` via `keyring`). Refresh is serialized across processes
with a lock file next to the token file, and a rejected access token is
refreshed once and retried by the client. The optional `callback-server`
feature adds an axum/rustls HTTPS listener that captures the redirect
(`capture_callback` / `SCHWAB_CAPTURE_CALLBACK`); without it `authorize`
returns the URL and the caller passes the code to `exchange_code`. PKCE
(S256) is implemented but off by default because Schwab's public flow does
not use it.

`security.rs` is the encryption, key derivation and file-permission code
used by the stores; `security_windows.rs` applies an owner/SYSTEM DACL on
Windows.

### REST client

`client::SchwabClient` is a cheap-to-clone `Arc` handle. Every endpoint
method is a thin wrapper that names its operation (`op` label for metrics)
and calls `execute_resilient`, which runs one logical call as:

1. `ensure_live_state` — the durable state file must not belong to a paper
   run (`meta.execution_mode`).
2. Fetch the access token from `AuthManager`.
3. `CircuitBreaker::call` (default: opens after 5 failures, half-open after
   30 s). Reads run inside `RetryPolicy::execute` (default 3 retries,
   100 ms → 10 s backoff with jitter, `Retry-After` honoured); **writes are
   never retried** — an ambiguous outcome becomes `Error::SubmissionUnknown`
   carrying the attempt ID.
4. Each attempt: `RequestBudget::admit` (in-process `governor` limiter +
   rolling per-minute window + shared quota through the `Coordinator`,
   keyed by app key and API origin; default 2 req/s, burst 10), then for
   submissions the durable operator halt, `RiskProvider::revalidate`
   (fenced by a coordination lease when supplied) and the in-process
   submission gate, then the transport call.
5. A 401 on a read triggers one `refresh_rejected_token` and one replay.
6. The whole call is bounded by `ClientConfig.timeout` (default 10 s) and
   recorded in `schwab_http_requests_total` / `schwab_http_request_duration_seconds`.

`transport::HttpTransport` wraps `reqwest`; `request_for_location` handles
Schwab's `201` + `Location` order responses.

### Order submission and durable state

`submit_order_request` (behind `place_order`, `replace_order` and their
`_fenced` variants) validates the request, assigns a `tag` if absent, and
walks the execution journal:

```
OrderJournal::begin_once ─► RiskProvider::reserve ─► LeaseSession (execution:<quota key>)
        │                                                   │
        │                                          transport (never replayed)
        ▼                                                   ▼
  finish(Rejected) on definite failure      finish(Accepted{order_id}) / SubmissionUnknown
```

Pending intents survive crashes and block new submissions for that account
until reconciled. `execution::ExecutionStore` is the trait; `OrderJournal`
is the local implementation over `storage::StateDatabase`, a SQLite file in
WAL mode (`<tokens_file>.orders.store/state.sqlite3`; a pre-0.2 JSON journal
at `<tokens_file>.orders.json` is imported once). Tables: `submissions`,
`audit`, `consumed_approvals`, `used_order_tags`, `accounts`, `orders`,
`executions`, `leases`, `quota`, `quota_config`, `approvals`,
`reservations`, `meta`, `schema_migrations`. Migrations, online backup and
restore live in `storage.rs`.

Around the journal:

- `coordination::Coordinator` — renewable ownership leases with epochs and
  the shared request quota. The local implementation is the same SQLite
  file, so several processes on one host share limits and fences.
- `risk::RiskProvider` — pre-trade checks with durable reservations;
  `PortfolioRisk` enforces per-account open-quantity/notional/position caps
  from a JSON policy and counts external open orders. Its optional
  `max_daily_loss` trips the durable operator halt (`execution::trip_halt`)
  once the reconciled baseline's day loss exceeds it, at the next
  reservation and, through `observe_baseline`, at each reconciliation; the
  paper engine applies the same cap to its bid-marked equity at every event.
- `reconciliation` — matches broker orders and activity to journal intents
  by unique tag, records generation-stamped account snapshots, and treats
  absence as inconclusive (never as rejection).
- `recovery::RecoveryWorker` — restartable per-account reconciliation loop
  that publishes `RecoveryStatus`; `readiness::Readiness` reports what local
  evidence says about reads and trading being safe.
- `audit_delivery` — batches the `audit` table to an external sink with
  acknowledged, replay-safe delivery.

### Backends and the paper engine

`broker::TradingBackend` is the trait the MCP gateway and authority dispatch
through: `place_order_fenced`, `replace_order_fenced`, `cancel_order`,
readiness, operator halt. `SchwabClient` implements it as `"live"`.
`paper::PaperBroker` implements it as an isolated long-equity simulator
(`paper/engine.rs` execution and accounting, `paper/model.rs` versioned
inputs and reports, `paper/feed.rs` optional read-only REST quote/calendar
polling, `paper/synthetic.rs` quotes derived from OHLCV candles under the
declared `candle-bbo-v1` model for backtesting). Paper state is its own
SQLite file; a live client refuses to open it. `remote::RemoteBackend` implements `ExecutionStore`, `Coordinator` and
`RiskProvider` over authenticated HTTPS to a `schwab-authority` process so
workers on other hosts share one journal without holding broker credentials.

### Streaming

`streaming::StreamClient` speaks Schwab's streamer protocol over
`transport::WebSocketTransport`. It logs in with a `StreamerIdentity` taken
from `GET /userPreference`, exposes typed `subscribe_*`/`unsubscribe_*`
helpers for all 13 services, and delivers `StreamMessage`s through a
tracked bounded queue (`ChannelKind::Bounded(4096)` by default):

- Every queued message carries the connection **generation**; the
  `MessageReceiver` only yields messages from the current, ready generation
  and interleaves `StreamMessage::State(StreamStatus { ready, generation,
  lost_messages })` whenever that changes, so consumers re-baseline instead
  of consuming stale data.
- Overflow is never silent: it bumps `lost_messages`, flips `ready` to
  false and increments `schwab_stream_dropped_messages_total`.
- Heartbeat: ping every 20 s, reconnect if no pong within 30 s. Reconnect
  backoff 2 s → 128 s, unlimited by default. Subscriptions are restored on
  reconnect and only confirmed once Schwab acknowledges them.
- Field selection per service via `set_service_fields`; incoming frames
  decode to `StreamPayload` through the field tables in `schwab-types`.

### Observability

The SDK emits `tracing` spans/events and `metrics` facade calls only; both
are no-ops until the host installs a subscriber and a recorder.
`metrics.rs` is the closed catalog of every metric name, label set and
help text (`describe()` registers them), and `telemetry.rs` has `tracing`
initialisation helpers. Errors (`error.rs`) are a single `Error` enum with
`ErrorKind`, `http_status()`, `is_retryable()` and a serializable
`ErrorEnvelope` for transports.

## schwab-mcp

### Server and tools

`server::SchwabMcp` is one `#[turbomcp::server]` impl with 39 `#[tool]`
methods in category order: market data (12, including the `get_candles`
and `get_option_contracts` projections), accounts (6, including the
`get_positions` projection and `get_portfolio_summary`), orders read (5,
including `get_order_status` and `get_trade_journal`), orders write (`build_order`,
`check_order`, `preview_order`, `place_order`, `cancel_order`,
`replace_order`), transactions (2), streaming control and cached reads (7),
server (`get_server_status`).
Each tool carries explicit read-only/destructive/idempotent/open-world hints;
`tests/tool_inventory.rs` pins the list. Tools hold a `backend::Backend`
rather than a `SchwabClient`: `Live(Arc<SchwabClient>)` or
`Paper { broker: Arc<PaperBroker>, market_data: Option<Arc<SchwabClient>> }`.
The backend answers account, order and quote reads and routes mutations to
its venue; in paper mode the venue's `PaperOrder`, `PaperFill`, snapshot
and quote are rendered in Schwab's shapes and parsed back through the
Schwab types, so a paper read can never carry a shape a live one could
not, and everything the venue does not model (`preview_order`,
transactions, user preferences, `ACCT_ACTIVITY`) is refused as
`not_permitted`. `Backend::market_data(what)` is the Schwab client for
market data and the stream: the live client, or the optional read-only
one, which the venue never routes an order through. `Backend::source()`
labels every read and mutation the backend served (`schwab_rest` or
`paper`). `server::Switches` carries the two write switches, read once at
startup, so a server built without the environment cannot trade.
`build_order` expands an
`OrderSpec` locally so the operator-approved intent and the JSON sent to
Schwab are the same shape. Responses pass through `redact.rs`, which drops
streamer credentials and identity fields and masks account numbers unless
`SCHWAB_MCP_REDACT_PII=0`. `stream_cache.rs` keeps per-service snapshots
keyed by field name with explicit freshness and a bounded activity log, and
reconciles them from REST after a reconnect (`schwab_stream_recovery_seconds`).
`status.rs` assembles `get_server_status` from the backend's local evidence
(mode, trading switches, operator halt, `readiness`, request budget,
circuit, stream, the paper venue's clock and session) plus one cached read
of Schwab's equity calendar per US-Eastern day; `AuthorizedServer` calls
`ServerStatus::scope_to` on the policy-gated transports, which adds the
principal section and withholds the halt's reason (operator text, and from
the daily-loss control an account's equity, loss and cap) from a principal
that is not granted every account, and puts the mode in
`serverInfo.title`. `projection.rs` turns a price history, an option chain or an
account into compact column/row pages: rows are sorted by a typed key
(candle time, `expiration|strike|C/P`, `symbol#ordinal`), a page stops at
the caller's `limit` or the `SCHWAB_MCP_RESPONSE_BUDGET_BYTES` budget, and
`nextCursor` is the last key so re-reading Schwab between pages never
skips or repeats a row. The raw tools keep Schwab's shape. `portfolio.rs`
composes `get_portfolio_summary` from two backend reads (orders from the
last 59 days, inside Schwab's 60-day list horizon, then the account with
positions) and the
execution journal: typed balances, P&L summed over positions, the first
`get_positions` page, the non-terminal orders (`OrderStatus::is_terminal`,
pinned to reconciliation's SQL) with children flattened, the count of
unanswered submissions and, in paper mode, the venue's own ledger; the
account read is essential, the others degrade to a null section plus a
warning. `order_status.rs` reduces one `Order` to
`get_order_status`'s report (terminal flag, filled and remaining
quantities, quantity-weighted average fill price overall and per leg from
the `FILL` executions, latest update time, replacing and child orders) and
implements the bounded wait: one `get_order` read per second until the
order is terminal or `wait_ms` (capped at 30 s) elapses. Its leg projection
is shared with the portfolio summary's working-order rows. `journal.rs`
validates the write tools' optional `rationale` (journal-only text, excluded
from the intent digest), records how each audited dispatch ended as
structured evidence (`Dispatch`: the venue order id, or the failure code)
and composes `get_trade_journal`: the principal's audit records for one
account joined, by order id or by tag, to one backend read of the orders
entered within the window, paged newest first. Every tool body runs through
`contract::run`, which boxes it once so the macro-generated dispatcher's
future and stack frame stay small however many tools there are.

### Startup and transports (`main.rs`)

1. `.env`, `tracing` to stderr (stdout is the MCP channel), optional
   Prometheus listener at `SCHWAB_METRICS_BIND`.
2. `server::Switches::from_env`, then the backend. Live:
   `SchwabConfig::from_env` → `SchwabClient`; if `SCHWAB_MCP_ALLOW_TRADING=1`
   the process also requires `SCHWAB_RISK_POLICY_FILE` and
   `SCHWAB_RECOVERY_ACCOUNTS`, and a `RecoveryWorker` runs for those
   accounts. Paper (`SCHWAB_MCP_PAPER_CONFIG` + `SCHWAB_MCP_PAPER_STATE`):
   `PaperBroker::open` over the venue's config and state, reading no Schwab
   environment unless `SCHWAB_APP_KEY` is set, which adds the read-only
   market-data client; the venue's per-account limits stand in for the
   risk file and no recovery worker runs. `SCHWAB_HEALTH_BIND` (loopback
   only) serves `/livez` and `/readyz`, whose body carries `mode`.
3. Transport from `SCHWAB_MCP_TRANSPORT`:
   - `stdio` without a policy file and without trading/cancel enabled runs
     the plain `SchwabMcp` (local trusted host, read-only tools).
   - Otherwise `SCHWAB_MCP_POLICY_FILE` is mandatory. It is a list of
     `policy::PrincipalPolicy` entries; each becomes an `AuthorizedServer`.
     Stdio picks one by `SCHWAB_MCP_PRINCIPAL`; `http`/`websocket` bind
     `SCHWAB_MCP_BIND` (loopback unless `SCHWAB_MCP_ALLOW_REMOTE=1` behind a
     TLS proxy).
4. SIGTERM/Ctrl-C calls `begin_drain` (no new submissions) and gives open
   transports 30 s to finish.

### Authorization boundary

- `policy.rs` — `PrincipalPolicy` names the tools, accounts, quantity and
  notional caps, option multipliers and approval requirement for one
  principal. `authorize(name, args)` runs before dispatch: it parses
  `args.order` as `OrderRequest`, computes exposure (rejecting anything it
  cannot bound, such as market or stop children and multi-leg orders) and,
  when `require_approval` is set, demands a consumed single-use approval of
  `intent_digest(name, args)` (SHA-256 over canonically sorted JSON, with a
  valid `order` first normalized to its `OrderRequest` wire form so the
  digest binds the order Schwab would receive, not one JSON spelling of
  it). `authorize` is the first denial of `review`, which keeps them all.
  `AuthorizedServer` wraps `SchwabMcp` as an `McpHandler`, writes the
  principal's audit journal and hands write tools to a `TradingBackend`.
- `preflight.rs` — `check_order`: the same `review`, plus the trading switch
  and `Backend::submission_admission` (operator halt, recovery suspension
  or the paper venue draining), reported together as a `Verdict` without
  dispatching or auditing. `AuthorizedServer` evaluates it under the principal with durable
  approvals merged in; the unauthorized stdio path evaluates it without a
  policy.
- `guidance.rs` — the `schwab://order-spec` resource (the `OrderSpec`
  grammar, `guidance/order_spec.md`) and the `propose_trade` prompt,
  rendered for an `Audience`: the tools the session holds, less the write
  tools the switches keep off, and the mode. `AuthorizedServer` lists and
  serves them only to principals holding the tools they are about, and the
  prompt only for the principal's accounts.
- `network.rs` — the axum router authenticates every request first
  (constant-time comparison of a bearer token's SHA-256 against the policy,
  or `identity::JwtVerifier` for short-lived RS256 tokens from
  `SCHWAB_MCP_IDENTITY_FILE`), then routes into that principal's isolated
  MCP router and session store. The policy file is re-read every 2 s; any
  change, or an invalid file, revokes existing sessions.
- `authority.rs` and `bin/schwab-authority.rs` — the shared transactional
  authority: `/v1/backend` for privileged SDK workers (`RemoteBackend`),
  `/v1/orders` + `/v1/lease` for trading identities that must not hold
  broker credentials, `/v1/operator` for verified approvals, halts and
  revocations. Single writer over its own SQLite file; default bind
  `127.0.0.1:8766`.
- `bin/schwab-admin.rs` — trusted local operator commands (digest, approve,
  halt, resume, audit export) that act on the state file directly and are
  deliberately not MCP tools. `bin/schwab-remote.rs` submits one typed
  request to the authority with a protected token file.
- `bin/schwab-paper.rs` and `bin/schwab-paper-verify.rs` — deterministic
  replay (paced with `--pace-ms` when a served venue should advance in
  virtual time), synthetic quote generation from price history, and
  transcript verification for the paper engine (`paper_replay.rs` is the
  shared command decoder).
- `eval.rs` and `bin/schwab-eval.rs` — model evaluation over the paper
  venue. `Scenario` fixes a seeded venue, a principal policy and a task;
  `Fixture::open` builds them under a private directory behind an
  `AuthorizedServer`; `Recording` is the `McpHandler` that appends each
  call to a JSONL transcript before dispatching it; `score` replays a
  `Transcript` through a fresh fixture and reduces the contract codes and
  venue orders to named checks plus the harness invariant
  (`unauthorized_executions`). No model is embedded.

## Request flows

**REST read**: tool or SDK call → `execute_resilient` → token → circuit
breaker → retry loop → budget admit → `HttpTransport` → typed response →
(MCP only) redaction.

**Order through MCP over the network**: bearer/JWT auth → principal router →
`PrincipalPolicy::authorize` (caps, approval digest) → `TradingBackend::
place_order_fenced` with a coordination lease → journal `begin_once` → risk
reserve → single transport attempt → journal `finish` → audit event →
`OrderSubmitResponse { order_id, location }` or `SubmissionUnknown` (never
auto-resubmitted; reconcile first).

**Stream recovery**: transport drop → generation bump and `ready=false`
(`State` message to consumers) → reconnect with backoff → re-login →
re-subscribe → acknowledgements confirm → `ready=true`; the MCP cache
additionally re-baselines each service from REST and reports the recovery
time.

## Verification gates

CI (`.github/workflows/ci.yml`) runs `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets --locked -- -D warnings`,
`cargo doc --workspace --no-deps --locked`,
`cargo build --workspace --all-targets --locked`,
`cargo test --workspace --all-targets --all-features --locked`,
`cargo test --workspace --doc --all-features --locked`,
`cargo check -p schwab-rs --no-default-features --locked`, `cargo audit`, an
MSRV (1.89) build, `cargo-semver-checks`, and the stream soak harness
(`examples/enterprise_bench` + `tools/soak.py`). `qualification.yml` runs the
full test suite on Linux, macOS and Windows plus longer soaks; `release.yml`
builds the binaries with a CycloneDX SBOM and build provenance attestation.
Property tests use `proptest`;
HTTP mocking uses `wiremock`. Evidence from local qualification runs is
written to `docs/audit/`, which is git-ignored; the hosted workflows upload
the same files as artifacts.

Everything above runs against mocks. The one gate that talks to Schwab is
the **live conformance harness**, `crates/schwab-rs/tests/live_conformance.rs`:
with `SCHWAB_LIVE_CONFORMANCE=1` and real credentials it `GET`s every
read-only endpoint through `SchwabClient::get_json`, proves the typed
binding still decodes the body, and diffs the body's *shape* (key sets and
scalar types, never values) against `tests/fixtures/conformance/*.shape.json`.
A decode error or a scalar changing type fails; keys appearing or vanishing
warn, because Schwab omits absent fields and a sample is data-dependent.
`SCHWAB_LIVE_CONFORMANCE=record` merges the live shapes into the fixtures.
`qualification.yml` runs it weekly when the repository has
`SCHWAB_APP_KEY`, `SCHWAB_APP_SECRET` and `SCHWAB_TOKENS_JSON` secrets and
uploads `live-conformance.json`; without them the job is skipped, and the
harness's own request → decode → diff → record cycle is still covered by a
`wiremock` test in the same file.

## Where to look

| Task | Start at |
|---|---|
| Add or fix a REST endpoint | `crates/schwab-rs/src/client.rs` (one method per endpoint) and the response type in `crates/schwab-types/src/` |
| Change an order shape or add a constructor | `crates/schwab-types/src/trading.rs`, `trading/builder.rs`, `trading/spec.rs`; `docs/ORDERS.md` |
| Add a streaming service or field | `crates/schwab-types/src/streaming/payload.rs` field tables, then `StreamClient::subscribe_*` |
| Add an MCP tool | `crates/schwab-mcp/src/server.rs`, then `tests/tool_inventory.rs`; policy-gated tools also need `policy.rs` |
| Change what a principal may do | `crates/schwab-mcp/src/policy.rs`; runtime policy JSON documented in `docs/OPERATIONS.md` |
| Durable state, migrations, backup | `crates/schwab-rs/src/storage.rs`, `execution.rs` |
| Metrics or alerts | `crates/schwab-rs/src/metrics.rs`, `crates/schwab-mcp/src/metrics.rs`, `ops/` dashboards and rules |
| Paper engine behaviour | `crates/schwab-rs/src/paper/`, `docs/PAPER_TRADING.md`, `docs/PAPER_LIMITATIONS.md` |
| Add a model-evaluation scenario or check | `crates/schwab-mcp/src/eval.rs` (`Scenario`, `Fixture::open`, `checks`), then `tests/eval_harness.rs`; `docs/MCP_LLM_INTEGRATION.md#model-evaluation` |
