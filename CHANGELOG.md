# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased — 0.2.0 candidate]

### Added — projection tools

- `get_candles(params, limit?, cursor?)`, `get_option_contracts(params,
  limit?, cursor?)` and `get_positions(account_hash, limit?, cursor?)`
  return the same Schwab reads as `get_price_history`, `get_option_chain`
  and `get_account` as compact pages: a `columns` header, `rows` of
  arrays, `count`, `total` (rows before paging), `truncated` and, when
  truncated, `nextCursor`. Rows are sorted by a typed key (candle time,
  `expiration|strike|C/P`, `symbol#ordinal`) and the cursor is the last
  key, so re-reading Schwab between pages never skips or repeats a row.
  A page stops at `limit` or at `SCHWAB_MCP_RESPONSE_BUDGET_BYTES` of
  encoded rows (default 65536, clamped to 4096..1 MiB) and always holds
  at least one row. Candles carry `previousClose`; option contracts carry
  the `underlying` quote and refuse spread strategies (`invalid_arguments`
  without a request) because Schwab answers those with strategy legs, not
  contracts; positions always request `fields=positions` and mask
  `accountNumber` like every other response. The raw tools are unchanged.

### Added — `get_server_status`

- `get_server_status()` reports, from local evidence, the mode (`live`),
  version and server time, the trading switches, operator halt, recovery
  suspension and circuit state summarized as `trading.admissible`, the
  `/readyz` readiness body, this process's request budget, stream health
  with `reconciliationRequired`, and the equity session phase
  (`pre_market`, `regular`, `post_market`, `closed`) with the next
  transition, read from Schwab's calendar at most once per US-Eastern day
  and looked ahead up to four days. On the policy-gated transports it also
  carries the principal's granted tools, accounts, limits and approval
  requirement. Sections whose probe fails are null and explained in
  `warnings`; an unauthenticated session performs no network call and
  yields one `unavailable`/`ask_operator` warning. The tool is read-only,
  not audited, and should be granted to every principal.
- `ToolFailure::unauthenticated`: `unavailable` with `ask_operator` and
  `retryable: false`, now also what an SDK `Error::Auth` maps to.
- SDK: `MarketHours::phase_at` classifies an instant against a day's
  sessions and names the next boundary; `market_data::equity_hours` finds
  the equity entry whether Schwab keyed it `EQ` (open day) or `equity`
  (closed day). `RequestBudgetUsage` and `SchwabClient::request_budget`
  expose the trailing-minute quota. `CircuitState` serializes in
  snake_case.
- Breaking: `Readiness`, the `/readyz` body of the MCP server and the
  authority, now serializes in camelCase (`readsReady`, `tradingReady`,
  `tokensValid`, `storageHealthy`, `pendingSubmissions`,
  `unreconciledAccounts`, `oldestBaselineAgeMs`, `circuitOpen`); the MCP
  probe's failure body uses the same names.

### Changed — MCP query windows default and accept dates

- `get_account_orders`, `get_all_orders`, `get_transactions` and
  `get_price_history` take model-facing query types (`schwab_mcp::query`)
  instead of the SDK parameter structs. Window bounds accept a calendar
  date (`YYYY-MM-DD`, covering the whole UTC day), RFC 3339, a date-time
  without an offset (UTC), or epoch milliseconds. Orders and transactions
  default to the last seven days ending now, transactions to `TRADE`, and
  `params` may be omitted. Unknown fields are refused. The SDK types and
  their validation are unchanged.

### Added — `check_order` pre-flight

- `check_order(account_hash, order, order_id?)` evaluates a `place_order`
  or `replace_order` intent locally and reports every reason the submission
  would fail at once, in dispatch order: argument validity, the principal's
  tool and account permissions, risk limits, approval state (policy or
  durable), the trading switch, and operator halts or a recovery
  suspension. The verdict carries the intent digest an operator approves,
  the tag the submission would carry, the bounded exposure and the
  principal's limits. Nothing is submitted, audited or approved; a
  duplicate intent is still only detected on submission. Grant it to
  trading principals alongside `place_order`.
- `PrincipalPolicy::review` collects every denial; `authorize` is now its
  first one.
- `SchwabClient::submission_admission` exposes the halt and recovery
  suspension check every submission attempt applies.
- `intent_digest` normalizes a valid `order` argument to its `OrderRequest`
  wire form before hashing, so `"10"` and `10` produce one digest and an
  approval binds the order Schwab would receive. Digests of orders that do
  not parse strictly are unchanged. Approvals computed before this change
  with non-canonical spellings (numeric quantities or prices) no longer
  match; recompute them with `schwab-admin digest`.

### Changed — MCP result contract (breaking for MCP clients)

- Every tool now answers in one shape (`schwab_mcp::contract`). Schwab and
  stream-cache data is wrapped as `{asOf, source, data}` with `data` in
  Schwab's raw, redacted shape; local results stay bare. Success text is
  compact JSON instead of pretty-printed, and a response that would exceed
  the 1 MiB transport limit is refused with `response_too_large` rather
  than clipped.
- Failures are `isError` tool results whose text and `structuredContent`
  are `{"error": {code, retryable, disposition, message, attemptId?,
  intentDigest?}}` with a closed `code` vocabulary and a `disposition` of
  `fix_arguments`, `retry_later`, `reconcile`, `ask_operator` or
  `not_permitted`. `SubmissionUnknown`/`ReconciliationRequired` carry the
  attempt id; approval denials carry the intent digest. Principal policy
  denials and audit faults use the same shape instead of JSON-RPC errors,
  which are now reserved for protocol faults. Argument-extraction
  failures on the authorized path are normalized to `invalid_arguments`.
- The trading switch (`SCHWAB_MCP_ALLOW_TRADING`) now answers
  `not_permitted` instead of `invalid_params`.
- Schwab's own 4xx rejection text (`ApiError` title and detail) is relayed
  in `message` so the model can correct the request; 401/403 map to
  `not_permitted`, 404 to `not_found`, 429 to `rate_limited`. Everything
  else upstream stays behind a logged reference id.
- `redact::json_response`/`json_value` are replaced by
  `contract::rest`/`local`/`envelope`.

### Changed — order tags carry intent identity

- The MCP server defaults an omitted order `tag` to `mcp-` plus the first
  32 hex characters of the intent digest, so an identical `place_order` or
  `replace_order` intent is refused as `duplicate_intent` whether or not
  approvals are required. Approval still binds the arguments as the model
  sent them; the audit record keeps the dispatched, tagged copy.
- `OrderJournal::finish(.., Rejected)` releases the submission's tag. A
  rejection (Schwab 4xx, risk refusal, or a failure before transmission)
  means the broker holds no order, so a caller may retry the same tagged
  request; accepted and unresolved submissions still consume their tag
  permanently, including after retention. The paper broker's injected
  rejection no longer consumes the tag either. Previously any use burned
  the tag, so a `retryable` failure could not be retried under an
  idempotency tag.

### Added — paper trading

- Capture execution mutations atomically for new paper runs, including committed injected rejection and lost acknowledgement.
- Verify captured execution history from a backup with `schwab-paper-verify --backup`; compare final accounts and complete order history and reject incomplete capture.
- Add `schwab-paper-verify` for isolated bounded transcript verification with final account coverage and integrity checks.
- Require dedicated paper ingestion roles; operator grants no longer imply quote, calendar, corporate-action or fault injection. Record accepted-event provenance atomically.
- Version the paper model as `equity-bbo-v2`; older runs require their original binary/configuration.
- Add explicit settlement calendars, durable pending cash, dividend entitlements, split cash-in-lieu and persistent stop triggers.
- Add opt-in read-only REST quotes/calendar polling, logical integrity checks and backup-backed event payload compaction.
- Add isolated durable long-equity paper execution, replay and wall-clock modes, finite-liquidity partial fills, exact-decimal accounting and injected faults.
- Share verified gateway approval/lease controls through `TradingBackend`; add paper market ingestion and scoped account reports.
- Add `schwab-paper` offline replay CLI, example commands, mode-isolation checks and release artifact inclusion.

### Changed — enterprise follow-up

- Revalidate risk after admission waits; persist operator halts independently of recovery.
- Split full order-history windows, follow known old orders by ID, and include external open-order exposure.
- Add authenticated remote backends and a broker authority/gateway with verified operator roles and fenced risk validation.
- Deliver audit batches with exact durable acknowledgements and canonical object-key hashing.
- Bound REST recovery and synthetic delivery gaps; cover shared state, approval replay/revocation and recovery end to end.
- Existing risk plugins must implement dispatch revalidation; use durable halt APIs for operator control. Review outstanding approvals after canonical digest migration.

### Changed — MCP dependency and integration review

- Upgrade stable TurboMCP to 3.3 and refresh compatible locked dependencies.
- Add behavior hints to all MCP tools and live-execution guidance to discovery.
- Remove unused dependencies, configure weekly dependency checks, and document LLM integration gaps and v4 migration criteria.

### Enterprise utility operations

- Replace local JSON execution state with transactional SQLite, preserving legacy intent/deduplication state through migration; add backup/restore and ordered audit export.
- Add tagged broker reconciliation, generation-aware stream baseline recovery, durable activity observations and retained risk reservations.
- Add shared local quotas, renewable ownership leases and pluggable execution/coordination/risk backends.
- Add issuer JWT verification, expiring static credentials, policy reload/revocation and trusted operator approval commands.
- Add portfolio/readiness checks, Prometheus alert rules, property/crash tests, benchmark/soak harnesses, compatibility checks and release provenance/SBOM generation.
- This is a breaking pre-1.0 release candidate; see `docs/OPERATIONS.md` before migration. It has not been published.

### Enterprise audit remediation

- Prevent ambiguous order replay with durable intent/reconciliation records.
- Separate strict decimal order requests from tolerant broker responses; correct protective orders and market-data wire contracts.
- Coordinate token refresh across processes, validate callback state, and recover rejected access tokens and circuit breakers.
- Expose stream loss/freshness and gate subscription restoration on acknowledgements.
- Require network principal authorization, account/tool policy, risk caps and durable execution audit; keep cancellation separately controllable.
- Add Windows atomic protected file storage, update dependencies and raise the minimum compiler to Rust 1.89.
- See [migration and validation notes](docs/ENTERPRISE_REMEDIATION.md) for breaking API changes and deployment requirements.

### Changed — error taxonomy (breaking)
- `schwab_rs::Error`, `AuthError` and `StreamError` are now
  `#[non_exhaustive]`; exhaustive matches need a fallback arm.
- Trading-control outcomes have their own variants instead of sharing
  `InvalidParameter`/`Storage`: `Halted(reason)` (operator halt, drain,
  suspended recovery), `RiskDenied(reason)`, `BaselineStale { account,
  reason }`, `Duplicate(reason)` (consumed intents, reused order tags,
  already-resolved submissions, replayed paper events) and
  `ApprovalInvalid(reason)`. `InvalidParameter` is now reserved for
  caller mistakes and `Storage` for durable-state failures.
- `Error::kind()` returns the new `ErrorKind` (stable `snake_case` wire
  names). `ErrorEnvelope` is the JSON body an error takes across a process
  boundary; `From<&Error>` / `From<ErrorEnvelope>` round-trip every typed
  variant and decode foreign or malformed payloads to `Error::Unknown`
  rather than fabricating attempt ids.
- `Error::Remote { status, message }` replaces the `Storage("Remote
  authority rejected request (HTTP n)")` string. It is retryable on 5xx
  and deliberately excluded from `http_status()`, so a relayed 401 can
  never trigger a local token refresh. `RemoteBackend` decodes authority
  envelopes back into the typed error; authority-side transport kinds
  (`http`, `api`, `broker`, `auth`, …) are re-homed under `Remote`.
- The authority (`schwab-mcp`) answers SDK failures with an
  `ErrorEnvelope` body and a kind-derived status (400 invalid parameter,
  403 approval invalid, 409 duplicate/reconciliation/ownership, 422 risk
  or baseline, 423 halted, 429 rate limit, 503 storage/remote/circuit,
  504 timeout) instead of a fixed 409; gateway `rejected` results include
  the envelope under `error`. Messages describing authority internals are
  logged there and replaced before leaving.
- Paper venue rejections (no fresh quote, replace rules, fee protection,
  injected rejections, terminal cancels) are `Error::Broker` 400 with an
  `ApiError { title: PAPER_REJECTION_TITLE, detail }` so live-targeted
  handling applies unchanged; paper limits are `RiskDenied`, engine
  invariants are `Storage`, and configuration-shaped refusals are
  `Config`. `SchwabPaperFeed` reports `Error::Timeout` when a fetch
  exceeds its deadline.
- MCP tools surface `Halted`/`BaselineStale`/`CircuitOpen` as
  `unavailable`, `RiskDenied`/`ApprovalInvalid` as `permission_denied`,
  `InvalidParameter`/`Duplicate` as `invalid_params`, and rate limits and
  timeouts under their own kinds; everything else keeps the opaque
  reference id. `PrincipalPolicy` rejects a price-less order outright
  instead of pricing it at `Decimal::MAX`.

### Changed — durable state hot path
- `StateDatabase` validates and migrates its file once per handle (on
  first use) and then serves operations from a pool of open connections
  instead of re-opening, re-locking and re-checking the schema on every
  call. Clones share the pool; a connection left mid-transaction is closed
  rather than reused. Measured per operation in release builds: no-op
  `run` 360µs → 7µs, `quota` 719µs → 102µs, `begin`+`finish` 1.6ms →
  134µs, with the remaining cost being `synchronous=FULL` fsyncs.
- Schema changes are ordered migrations recorded in a new
  `schema_migrations(version, name, applied_at)` table; `meta.schema` is
  the highest applied version (now `2`). Each migration commits together
  with its history row and the version bump. Version 1 files upgrade in
  place on first open (the ad hoc `tags_imported` flag becomes migration
  2); files from a newer build are refused; `restore` accepts any
  supported version and migrates the copy forward.
- A tampered file is now detected on the next open, not the next call:
  reopen with a new `StateDatabase` (or process) to re-validate.

### Changed — one owner per durable-state concept
- The `meta` key/value table has a single crate-private accessor
  (`storage::meta::{get, exists, put, insert_if_new, delete, get_json,
  put_json}`); the audit-delivery cursor, execution mode, operator halt,
  paper run state and schema version all go through it instead of each
  module carrying its own SQL.
- One `execution::ensure_not_halted` admission check and one
  `coordination::lease_held` fence check replace the copies in the risk
  provider, the paper engine and the coordinator. A paper submission under
  an operator halt now reports the same `Halted("Operator trading halt is
  active")` as live dispatch, and a fence whose epoch cannot exist in the
  store is treated as lost ownership rather than an invalid parameter.
- In-process and durable rate-limit admission collapse into one
  `RequestBudget::admit` call; `SchwabClient` no longer parses the base URL
  twice.
- `schwab-mcp`: `policy::validate_policies` is the one place the principal
  set is checked (per-policy validity, unique names/digests/subjects, a
  single streaming owner, short-lived static credentials without an
  issuer), shared by startup and hot reload, and
  `AuthorizedServer::new(client, policy)` derives the principal's journal
  path from `TradingBackend::principal_journal_path` instead of two hand
  copies of the extension dance. A reload that contains no principals now
  revokes every session the same way an invalid file does.

### Changed — metric catalog closure
- `schwab_rs::metrics` gains `Metric`, `MetricKind`, `CATALOG` and
  `describe()` / `describe_all()`: every metric the SDK emits is declared
  once with its kind, unit and help text, and a recorder installed by the
  consumer can register the descriptions in one call. `schwab-mcp` does so
  after installing the Prometheus recorder, so `/metrics` now carries
  `# HELP` lines. Its own `schwab_mcp::metrics::CATALOG` covers the tool
  and stream-recovery series.
- The eight metrics that were emitted from string literals
  (`schwab_submission_unknown_total`, `schwab_reconciliation_total`,
  `schwab_pending_submissions`, `schwab_unreconciled_accounts`,
  `schwab_baseline_age_seconds`, `schwab_trading_ready`,
  `schwab_recovery_ready`, `schwab_stream_dropped_messages_total`) and the
  two in `schwab-mcp` (`schwab_stream_recovery_seconds`,
  `schwab_stream_recovery_ready`) now have catalog constants and doc rows.
  A workspace test (`schwab-mcp/tests/metrics_catalog.rs`) fails when a
  call site uses a literal, records a catalogued name with the wrong
  instrument, or a catalog entry is missing from the module doc table.
- `schwab_token_refresh_total` is recorded once per `/oauth/token` request
  in the single code path that makes them, so on-demand refresh failures
  (previously only the background task counted failures) and authorization
  code exchanges (`grant="authorization_code"`) are visible. The README
  table now matches the emitted labels (`status` buckets are `2xx`…`5xx`,
  `unknown` or `error`; the `grant` values are the RFC 6749 grant types).
- `schwab-mcp` asserts that the version advertised in `server_info` equals
  `CARGO_PKG_VERSION`, since the server macro only takes a literal.

### Added — typed streaming payloads (breaking for the `*Fields` helpers)
- Every streamer data service has a field table and a typed payload in
  `schwab_types::streaming`: `LevelOneEquity`, `LevelOneOption`,
  `LevelOneFutures`, `LevelOneFuturesOption`, `LevelOneForex`, `Book`
  (with nested `BookLevel` / `BookMarketMaker`), `ChartEquity`,
  `ChartFutures`, `Screener` (with `ScreenerItem`) and `AccountActivity`.
  `DataItem::decode()` / `StreamService::decode()` turn a numbered frame
  into a `StreamPayload`; `Payload::from_content()` decodes one item.
  Field names follow the REST quote types (`bidPrice`, `52WeekHigh`,
  `askMICId`…) so stream deltas and REST quotes merge by name; unknown
  field numbers land in `extra`. `DataContent::named_fields()` /
  `to_named_value()` do the renaming without the typing.
- The `*Fields` tables (`LevelOneEquityFields`, …, plus new
  `LevelOneFuturesFields`, `LevelOneFuturesOptionFields`,
  `LevelOneForexFields`, `BookFields`, `ChartFuturesFields`,
  `ScreenerFields`) expose `FIELDS: &[Field]`, `name(number)`,
  `all_fields()` and their numeric constants; `StreamService::fields()`
  returns the table for a service and `default_fields()` is derived from
  it. Only `LevelOneEquityFields` keeps a narrower `default_fields()`;
  `LevelOneOptionFields::default_fields()` and
  `ChartEquityFields::default_fields()` are replaced by `all_fields()`, and
  the `AcctActivityFields::SEQUENCE` / `KEY` string constants are gone
  (`seq` is the payload's `seq` field). `StreamService` is now `Copy`,
  `Eq` and `Hash`.
- `schwab-mcp` decodes every frame before caching: `stream_snapshot`
  content and the `ACCT_ACTIVITY` event log carry field names
  (`bidPrice`, `messageType`, `messageData`) instead of `"1"`, `"2"`,
  `"3"`. The REST baseline copies every `quote` / `reference` /
  `fundamental` / `regular` field the stream table also names, replacing
  the seven-field hand tables; PII redaction of account activity now goes
  through the same name-keyed walk as REST responses.
- The `streaming_quotes` example decoded fields 28 and 29 as open price and
  net change; they are the regular-market trade flag and last price. It now
  subscribes by `LevelOneEquityFields` constants and prints a
  `LevelOneEquity`.
- `AccountActivity::message()` returns `messageData` parsed as the JSON
  object an order event carries (`None` for plain-text errors). The MCP
  `ACCT_ACTIVITY` event log stores that parsed object under `messageData`
  instead of the JSON-in-a-string the wire sends, so consumers no longer
  parse twice; plain-text messages stay strings.

### Fixed — `stream_snapshot` freshness
- `fresh` was effectively never true after the first delta: it required
  *every* field to have been updated within 2s, but level-one deltas carry
  only the fields that changed, so baseline-filled statics such as
  `description` aged out immediately. It also compared the broker
  timestamp against the wall clock, so a quiet symbol on a healthy stream
  reported late delivery. Freshness is now judged per key: the stream is
  ready on the entry's generation, the REST baseline has landed when one
  is required, and the last delta both arrived within 2s of the broker
  stamping it (`lagMs`) and within 2s of now (`ageMs`). `staleReason`
  names the first failed condition (`"stream not ready"`, `"REST baseline
  not applied yet"`, `"no update within the freshness window"`, …) so a
  consumer can tell reconnecting from quiet. `fieldUpdatedAt` is receipt
  time for baseline-filled fields too, matching delta-filled ones.

### Added — order constructors
- `OrderRequest` builds the way Schwab's order examples are written:
  `market`, `limit`, `stop`, `stop_limit`, `trailing_stop`, `net_debit`,
  `net_credit` and `net_zero` take legs from `OrderRequestLeg::equity` /
  `OrderRequestLeg::option` (plus `with_position_effect`) and default to
  `NORMAL` / `DAY`; `with_session`, `with_duration`, `with_tag`,
  `with_complex_strategy`, `with_special_instruction`,
  `with_destination`, `with_tax_lot_method` and `with_stop_type` set the
  rest. `one_cancels_other`, `triggers` and `bracket` nest
  `childOrderStrategies`; `bracket` derives the GTC take-profit / stop-loss
  OCO pair from the entry leg via `OrderInstruction::closing()` and
  rejects inverted exit prices. Unit tests pin the JSON against Schwab's
  documented examples.
- `trading::OptionSymbol` formats and parses the 21-character OSI form
  (`AAPL  240517P00190000`), validating the root, right and strike.
  `schwab_types::Decimal` re-exports `rust_decimal::Decimal` so callers
  can write prices without their own dependency.
- `trading::OrderSpec` is the same composition as JSON, keyed by `kind`
  (`market` … `net_zero`, `oco`, `trigger`, `bracket`) with `LegSpec` /
  `InstrumentSpec` (OSI symbol or contract parts) and `OrderSettings`;
  `OrderSpec::build()` / `OrderRequest::try_from` expand and validate it.
  Unknown keys are rejected at every level.
- `schwab-mcp` gains the read-only, local `build_order(spec)` tool that
  returns the expanded `OrderRequest` JSON for `preview_order` /
  `place_order` / `replace_order`, so policy digests and operator
  approvals keep seeing the single wire shape. Principals must list it in
  `tools` like any other tool.
- `docs/ORDERS.md` is rewritten around the constructors; it previously
  showed the response type `Order` with fields that do not exist.
- `PrincipalPolicy` accepts the bracket it builds: a triggered child that
  only flattens the long position its bounded parent opens (the paired
  closing instruction, the same instrument, no more than the entry's
  quantity) is priced at the parent's limit, so `bracket(...)` / the
  `bracket` spec clears the risk check with entry + take-profit + stop-loss
  summed as gross exposure. Previously the stop-loss child was refused as
  "requires an order with a bounded limit price", which rejected every
  bracket over the network. Every other unbounded order (market, trailing,
  naked stop, a stop that sells more than the entry or another symbol, a
  stop under a market entry) is still refused. `OrderInstruction::opens_long`
  is now public.
- `TRAILING_STOP_LIMIT` deliberately keeps no constructor; `docs/ORDERS.md`
  and the `trailing_stop` docs now say why (Schwab's schema has no
  `priceOffset` and no published example, so the limit linkage is
  unspecified) and how to send one with a fixed `price`.

### Changed — documentation truth pass
- `docs/ARCHITECTURE_OVERVIEW.md` is rewritten to describe the current
  workspace: the three crates and five MCP binaries, the REST resilience
  pipeline in the order it actually runs, the submission journal and SQLite
  state layout, streaming generation/loss semantics, the MCP authorization
  boundary and the authority gateway, plus a "where to look" table. It no
  longer quotes line counts, test counts, dependency versions or grades.
- `CONTRIBUTING.md` lists the exact CI commands and the `-p` form for the
  example packages; `docs/INDEX.md` indexes every document that exists.
- `docs/DEVELOPER_DOCUMENTATION.md`, `docs/QUICK_START.md` and
  `docs/QUICK_REFERENCE.md` are removed. They were generated snapshots of an
  older tree (wrong module layout, `cargo run --example`, "PKCE on by
  default", test-count tables), and the last one documented the Python
  Schwabdev API rather than this SDK.
- README and the security section state what the code does instead of
  grading it ("OWASP compliant", "world-class", "best-in-class",
  "100% improvement" are gone); the nonexistent "90-second crash detection"
  bullet and the `.rs` example file names are corrected.
- Links to `docs/audit/` are converted to plain paths with a note that the
  directory is git-ignored local evidence (it is uploaded as workflow
  artifacts, never committed), so the docs no longer link to files that a
  clone does not contain. `.gitignore` also covers the derived state files
  next to the token file (`*.orders.json`, `*.orders.store/`, `*.sqlite3*`,
  lock files).
- `.env.example` documents `SCHWAB_CAPTURE_CALLBACK`; `docs/SETUP_GUIDE.md`
  names the real key-file path (`schwab_tokens.key`) and the `-p oauth-flow`
  command; `docs/MCP_LLM_INTEGRATION.md` counts 31 tools.
- CHANGELOG: the paper-trading and enterprise follow-up notes that sat above
  the preamble, and the MCP dependency review that sat under 0.1.0, now
  live under Unreleased where the work is.
- README links to `docs/*.md`, `CONTRIBUTING.md` and the issue tracker are
  absolute GitHub URLs. The README is the `schwab-rs` crate front page on
  docs.rs, where repository-relative links (`docs/ORDERS.md`,
  `../../issues`) resolved to nothing.
- The example packages (`auth_test`, `comprehensive`, `oauth-flow`,
  `streaming-examples`, `enterprise-bench`) inherit `version`, `edition`
  and `rust-version` from the workspace and are marked `publish = false`;
  they were pinned at `0.1.0` / a literal edition and, apart from
  `enterprise-bench`, publishable.

### Added — live conformance harness
- `SchwabClient::get_json(path, query)` issues an authenticated `GET` to any
  REST path and returns the raw `serde_json::Value`, through the same
  budget, circuit-breaker, retry and refresh pipeline as the typed methods.
  It is deliberately `GET`-only so writes stay on the journaled order
  methods.
- `crates/schwab-rs/tests/live_conformance.rs` is a read-only sweep of the
  real API, skipped unless `SCHWAB_LIVE_CONFORMANCE` is set. It fetches the
  market-data, account, order, transaction and preference endpoints, proves
  each typed binding still decodes the live body, and diffs the body's
  shape (key sets and scalar types, never values) against
  `tests/fixtures/conformance/*.shape.json`. A decode error or a scalar
  changing type fails the run; keys appearing or vanishing are warnings,
  because Schwab omits absent fields. `SCHWAB_LIVE_CONFORMANCE=record`
  merges the live shapes into the fixtures; the JSON report goes to
  `SCHWAB_LIVE_CONFORMANCE_REPORT` (default `target/live-conformance.json`).
  The request → decode → diff → record cycle is also exercised offline
  against `wiremock`, so the harness itself is tested in every CI run.
- `qualification.yml` runs the sweep weekly when `SCHWAB_APP_KEY`,
  `SCHWAB_APP_SECRET` and `SCHWAB_TOKENS_JSON` secrets exist (a preflight
  job turns their presence into an output, so forks skip rather than fail)
  and uploads `live-conformance.json`. The token file is written with
  `umask 077` outside the workspace and removed afterwards.

### Fixed — order enums aligned with the Trader API (breaking)
- Spellings Schwab does not use are gone: `OrderType::CabinetOrder` is
  `Cabinet` (`CABINET`), `ComplexOrderStrategyType::DoubleCalendar` /
  `UnbalancedButterflySpread` / `UnbalancedCondorSpread` are
  `DoubleDiagonal` / `UnbalancedButterfly` / `UnbalancedCondor`, and the
  phantom `OrderStatus::Awaiting` is replaced by the real
  `AwaitingParentOrder` and `AwaitingStopCondition`.
- `OrderStrategyType` carries every value Schwab reports (`CANCEL`,
  `RECALL`, `PAIR`, `FLATTEN`, `TWO_DAY_SWAP`, `BLAST_ALL`) so order
  history no longer fails to deserialize; `OrderRequest::validate()`
  rejects them, since only `SINGLE`, `OCO` and `TRIGGER` can be submitted.
  `OrderLegType` gained `COLLECTIVE_INVESTMENT`.
- The string-typed request fields are enums: `PriceLinkBasis`
  (`stopPriceLinkBasis` / `priceLinkBasis`), `PriceLinkType`, `StopType`,
  `TaxLotMethod`, `SpecialInstruction` and `RequestedDestination`, all
  with Schwab's value sets, so a typo is a compile error rather than a
  400. `schwab_types::common::PutCall` names an option right.
- The order enums (`OrderSession`, `OrderDuration`, `OrderType`,
  `OrderInstruction`, `ComplexOrderStrategyType`, `OrderStatus`,
  `OrderLegType`, `PositionEffect`) are `Copy`, `Eq` and `Hash`.
- Asset types are an enum on both sides of the wire.
  `schwab_types::common::AssetType` carries the Trader API's full
  `assetType` set (`EQUITY`, `OPTION`, `INDEX`, `MUTUAL_FUND`,
  `CASH_EQUIVALENT`, `FIXED_INCOME`, `CURRENCY`, `COLLECTIVE_INVESTMENT`,
  `FUTURE`, `FOREX`, `PRODUCT`) and replaces the `String` on
  `OrderRequestInstrument`, order legs and positions; the previous
  `common::AssetType` (Market Data's `assetMainType` values) is renamed
  `AssetMainType` and now types `QuoteItem::asset_main_type`,
  `DataContent::asset_main_type`, the stream payload envelopes and
  `Instrument::asset_type` (all previously `Option<String>`), with
  `common::AssetSubType` (`Copy`/`Eq`/`Hash`, plus `MMF`) on the matching
  `assetSubType` fields; both read values outside the enum, including
  Schwab's own `UNKNOWN`, as `Unknown` rather than failing the payload.
  Positions and order legs share one instrument type,
  `common::AccountsInstrument` (Schwab's `AccountsInstrument`), which
  replaces `accounts::PositionInstrument` and `trading::OrderInstrument`
  and adds the option (`putCall`, `underlyingSymbol`, `optionMultiplier`)
  and fixed-income (`maturityDate`, `factor`, `variableRate`) extensions
  plus a tolerant `extra` map. `putCall` decodes through
  `common::lenient`, which reads Schwab's documented `UNKNOWN` as `None`
  instead of failing the account.
- `Order` is fully typed against the spec: `requestedDestination`,
  `stopPriceLinkBasis` and `stopPriceLinkType` are the same enums the
  request side uses, and the documented fields that previously landed in
  `extra` (`statusDescription`, `cancelTime`, `releaseTime`, `stopType`,
  `priceLinkBasis`, `priceLinkType`, `activationPrice`, `taxLotMethod`,
  `specialInstruction`, `tag`, `replacingOrderCollection`) have fields.
  `orderActivityCollection` is `Vec<OrderActivity>` (activity type,
  quantities, `ExecutionLeg`s with price/time/`legId`) instead of raw
  JSON; `executionType` stays a string because Schwab sends `CANCELED`,
  which the spec omits. `OrderLeg` gained `quantityType` and skips
  absent fields when serialized, as `Order` already did.

### Fixed — API wire-shape audit (response shapes verified against Schwab docs + Schwabdev 3.0.5)
- **Quotes**: `GET /quotes` and `GET /{symbol}/quotes` return a
  **symbol-keyed map**, not a `quotes` array. `QuotesResponse` is now
  `{ errors, quotes: HashMap<String, QuoteItem> }` (flattened on the
  wire); `get_quote` extracts the requested symbol from the map.
  `QuoteItem` gained `assetMainType`/`assetSubType`/`quoteType`/
  `realtime`/`ssid` and tolerates missing fields.
- **Order submission**: Schwab answers `POST .../orders` and
  `PUT .../orders/{id}` with **201 + empty body** and the order URL in
  the `Location` header. `place_order`/`replace_order` now return
  `OrderSubmitResponse { order_id, location }` parsed from that header
  (previously they tried to deserialize a JSON body and reported
  *failure on successful submissions*). `preview_order` returns the raw
  JSON validation payload (`serde_json::Value`) since its shape varies.
- **Instruments**: `InstrumentsResponse` now matches the
  `{"instruments": [...]}` wrapper. `GET /instruments/{cusip}` uses the
  same envelope; `get_instrument` unwraps its first entry and answers an
  empty list with `Error::Api { code: "NOT_FOUND" }`, pinned by a
  `wiremock` test and decoded that way by the live conformance harness.
- **Fundamentals** (breaking): `QuoteFundamental` and
  `InstrumentFundamental` are retyped to the payloads Schwab sends.
  Share-count volumes arrive as JSON floats (`"avg10DaysVolume":
  37498265.0`) and `dividendDate` as a string, so the previous `i64`
  fields failed to decode any quote requested with `fields=fundamental`
  and any `projection=fundamental` search. `QuoteFundamental` now carries
  exactly the documented quote section (`avg1YearVolume`, `divExDate`,
  `divFreq`, `lastEarningsDate`, `nextDivExDate`, …; the ratio fields it
  never received are gone) and `InstrumentFundamental` the full
  fundamental projection, with explicit renames for the `TTM`/`MRQ`
  suffixes that a `camelCase` rename would emit as `Ttm`/`Mrq`.
- **Market hours**: `get_markets`/`get_market` return the doubly nested
  `{market_type: {product: hours}}` map Schwab actually sends.
- **Transactions**: parameter renamed `type` → `types` (the name Schwab
  accepts) and `start_date`/`end_date`/`types` are now required
  arguments, as the API mandates. `get_account_orders`/`get_all_orders`
  similarly require `fromEnteredTime`/`toEnteredTime`.
- **Transaction bodies** (breaking): `Transaction` was still the TD
  Ameritrade shape (`principal`, `accruedInterest`, a `fees` map, one
  top-level `instrument`), none of which Schwab sends, so every
  transaction decoded with its contents silently dropped and
  `get_transactions` (SDK and MCP) reported net amounts with no
  instruments, quantities or fees. It now matches the Trader API:
  `transferItems: Vec<TransferItem>` (one per instrument moved and per
  fee, with `amount`/`cost`/`price`, a typed `FeeType`, and
  `positionEffect`), `tradeDate`/`settlementDate`, and typed
  `TransactionType`, `TransactionStatus`, `SubAccount` and
  `TransactionActivityType`. `TransactionInstrument` is retyped to the
  spec's base fields plus the option, fixed-income and future extensions
  with a tolerant `extra` map; `orderId` is a string like every other
  order ID in the crate. The response-only enums carry Schwab's own
  `UNKNOWN` as a catch-all, and `type`/`putCall`/`positionEffect` decode
  through `common::lenient` so an undocumented value degrades to `None`
  rather than failing the list.
- **User preferences**: `AccountPreferences` field names corrected to
  the wire format (`primaryAccount`, `nickName`, `displayAcctId`);
  `TransactionUser`/`Offer` fields made optional for tolerance.
- Added `crates/schwab-types/tests/response_shapes.rs` — fixture-based
  deserialization tests so wire-shape regressions fail in CI.

### Fixed — request-side wire audit (query names and streamer field tables verified against Schwab docs + Schwabdev 3.0.5)
- **Price history**: `periodType`/`frequencyType` were sent upper-case
  (`DAY`, `MINUTE`); Schwab only accepts lower-case. `PriceHistoryParams`
  and `OptionChainParams` moved to `schwab_types::market_data` and grew
  `new(symbol)` plus `validate()`, which enforces Schwab's period /
  frequency-type / frequency tables (and the defaulted frequency type)
  before the request is sent. `Candle.volume` is now `i64`.
- **Typed request enums** replace free-form strings: `QuoteField`
  (`fields=quote,fundamental`), `MoverIndex` (`$DJI`, `NYSE`, …),
  `MoverFrequency` (bare integer `0|1|5|10|30|60`), `Market`
  (`equity|option|bond|future|forex`), `Entitlement` (`PN|NP|PP`) and
  `OptionType` (`S|NS|ALL`). `get_quotes_with_options`, `get_movers`,
  `get_markets`/`get_market` and the matching MCP tools take them; each
  enum exposes `as_str()`/`Display` that is tested equal to its serde
  wire value. The unused `common::MarketType` was removed.
- **Order and transaction listings** (breaking): `get_account_orders`,
  `get_all_orders` and `get_transactions` take `OrdersParams` /
  `TransactionsParams` (`schwab_types::trading`) instead of five
  positional strings. Windows are `DateTime<Utc>` and leave the process
  as `common::schwab_timestamp`, the `yyyy-MM-dd'T'HH:mm:ss.SSSZ`
  spelling Schwab documents (three fractional digits, always). `status`
  is `OrderStatus`, `types` is a non-empty `Vec<TransactionType>` joined
  with commas on the wire (and read back from either a list or that
  string), and `validate()` rejects an inverted window, a zero
  `maxResults`, an empty type list or a blank symbol before any bytes
  are sent. Reconciliation's order scan and the live-conformance harness
  use the same formatter; previously reconciliation sent
  `to_rfc3339()` (nanosecond fractions, `+00:00`), which is not the
  documented form. The MCP `get_account_orders`, `get_all_orders` and
  `get_transactions` tools take `params` with the same shape.
- **Account paths**: every `/trader/v1/accounts/{hash}` builder now
  rejects empty or non-URL-safe segments and all-digit values (an
  account *number* where the `hashValue` is required) before any
  request is made.
- **Streamer field tables**: `LEVELONE_OPTIONS` now covers fields
  36 and 40–55 (exchange, 52-week range, indicative quotes, exercise
  type); `LEVELONE_FUTURES_OPTIONS` stops at field 31, which is where
  Schwab's guide ends and past which the streamer rejects the command.
- Added `crates/schwab-types/tests/request_params.rs` and
  `crates/schwab-rs/tests/market_data_wire.rs` (wiremock) so the query
  strings and local guards are asserted in CI; the wiremock client
  fixture now lives in `crates/schwab-rs/tests/common/`.

### Fixed — streaming protocol & reliability
- Subscriptions now use the **`ADD`** command (additive) like the
  reference client. `SUBS` *replaces* a service's entire key list
  server-side, so a second `subscribe()` call silently dropped earlier
  symbols while the local tracker still counted them.
- Streamer LOGIN sends `SchwabClientChannel`/`SchwabClientFunctionId`
  from the user-preferences `streamerInfo` (via the new
  `StreamerIdentity`) instead of hardcoded `N9`/`APIAPP`.
- Heartbeat timeout now actually tears the connection down: the read
  loop runs under `tokio::select!` against the heartbeat watchdog, so a
  silently dead TCP connection reconnects instead of blocking forever.
- The 90-second crash heuristic only applies to the *first* session
  after `connect()`; once a session has authenticated, drops always go
  through backoff-reconnect. Previously any error within 90 s of a
  (re)connect attempt — e.g. a transient DNS failure during reconnect —
  permanently killed a stream that had been healthy for hours.
- Matching Schwabdev 3.0.5: a server close with WebSocket code 1000 is
  treated as a graceful stop (no reconnect); abnormal closes reconnect.
- Backoff resets after each successful login (was: only on clean close).
- Per-service default field lists (books, screeners, futures, forex,
  chart futures) via `StreamService::default_fields()` — previously all
  non-equity services fell back to the *equity* field list.
- `disconnect()` sends a best-effort `ADMIN/LOGOUT` before tearing down.
- Unparseable stream frames are logged + counted
  (`schwab_stream_messages_total{service="_unparsed"}`) instead of
  silently dropped; `DataContent.delayed` is optional for tolerance.
- `max_subscriptions` is now enforced (`SymbolLimitReached`).
- Fixed a `pending_acks` entry leak when queuing a request fails.

### Fixed — auth lifecycle
- Credential validation relaxed to Schwabdev 3.0.4's rule (even-length
  key + secret, ≥ 32 chars combined) — the previous exact 32/48 + 16/64
  check rejects newer Schwab-issued credential formats.
- `AuthManager` is `Clone`, but `Drop` aborted the **shared**
  auto-refresh task as soon as *any* clone dropped (e.g. via
  `stream_disconnect`), silently stopping token refresh process-wide.
  The task is now only aborted when the last clone drops.
- The OAuth callback server now serves **TLS** with an in-memory
  self-signed certificate (rcgen + axum-server). Schwab requires an
  `https://` callback URL, so the previous plain-HTTP listener could
  never complete the browser handshake and the code was never captured.

### Fixed — client defaults & transport
- **Rate limiter default corrected to Schwab's actual limit**: 120
  requests/**minute** (2 req/s, burst 10). The previous default of 120
  req/s was 60× too permissive and effectively decorative.
- 429 responses now produce `Error::RateLimit` carrying the server's
  `Retry-After` value, which the retry policy honors (this path was
  previously dead code — the header was never parsed).
- `ClientConfig::user_agent` is actually applied to the HTTP transport.
- `SchwabConfig::validate()` runs the SSRF URL-domain allowlist for all
  construction paths (previously only `from_env`/`from_file`).
- `SchwabConfig::save()` writes via `secure_file_write` (0600, atomic) —
  the config contains `app_secret` in plaintext.
- `get_instrument` URL-encodes the CUSIP path segment.

### Changed
- `StreamClient::new` takes a `StreamerIdentity` (customer/correl IDs +
  channel/function ID); the builder gains `.channel()`, `.function_id()`
  and `.streamer_info(&StreamerInfo)`.
- `SchwabClientBuilder` dropped its `auth_manager()`/`http_client()`
  setters — `build()` always ignored them.
- Removed the deprecated `StreamConfig::buffer_size` (use
  `channel_kind`).
- `turbomcp` is now a crates.io dependency (was a local path dep that
  made the repo unbuildable without a sibling checkout); use
  `[patch.crates-io]` for local development.
- `schwab-mcp`: `get_transactions`/`get_account_orders`/`get_all_orders`
  tool params updated to match the required-argument API; tool timing
  boilerplate collapsed into a `timed()` helper; `stream_connect` and
  the implicit connect in `stream_subscribe` share one code path.
- Repo is dual-licensed properly: `LICENSE-MIT` + `LICENSE-APACHE` now
  both ship, matching the `MIT OR Apache-2.0` manifest declaration.

### Security
- `cargo update -p rustls-webpki` → 0.103.13 (RUSTSEC-2026-0104,
  reachable panic in CRL parsing). Remaining `cargo audit` findings are
  *warnings* in turbomcp's transitive deps (`rustls-pemfile`
  unmaintained, `rkyv` unsound advisory) — tracked upstream.
- `schwab-mcp` redaction extended to `ACCT_ACTIVITY` streamer frames
  cached for `stream_snapshot`: the raw account number in positional
  field `1` is masked and the JSON activity payload in field `3`
  (including PascalCase `AccountNumber`) is recursively redacted
  *before caching*, so no tool can expose the raw frame.
- `stream_connect` no longer echoes the streamer websocket URL
  (consistent with `streamerSocketUrl` being stripped from every
  response), and the `get_user_preferences` tool description no longer
  advertises the streaming credentials that redaction removes.
- README now documents that the MCP HTTP/WebSocket transports carry no
  request authentication (Origin validation only) and must stay on
  localhost or behind an authenticating reverse proxy.
- `schwab-mcp` now **refuses to bind** the HTTP/WebSocket transports to a
  non-loopback address unless `SCHWAB_MCP_ALLOW_REMOTE=1` is set, closing
  the most dangerous misconfiguration (an unauthenticated tool surface,
  incl. trading, exposed on `0.0.0.0`). Opting in logs a loud warning,
  with an extra warning when trading is also enabled. (Per-request bearer
  auth is deferred to the turbomcp v4 migration rather than wiring the
  soon-to-be-replaced v3 server auth path.)
- Redaction hardened so the `SCHWAB_MCP_REDACT_PII` kill switch can
  **only un-mask account numbers** for local debugging — streamer
  credentials and user-identity fields are now dropped unconditionally
  and cannot be re-enabled by any env var.
- `schwab-rs` no longer embeds the absolute token-file path in the
  `TokenFileInsecure` error (it could reach an LLM via an MCP tool
  response); the path is logged for the operator instead.

### Added
- **Observability** — SDK-side metrics facade via the `metrics` crate
  (no-op when no recorder is installed) plus opt-in Prometheus scrape
  listener in the `schwab-mcp` binary (`SCHWAB_METRICS_BIND=host:port`).
  Full catalog: HTTP request counters/histograms, retry outcomes,
  circuit-breaker transitions + state gauge, token-refresh outcomes,
  streaming connect/reconnect/message counters, and MCP tool-level
  counter + duration histogram.
- **PII redaction** in `schwab-mcp` tool responses — account numbers
  masked to last four, streamer credentials + user identity fields
  stripped. Controlled by `SCHWAB_MCP_REDACT_PII` (default on).
- Explicit `StreamClient::disconnect()` that stops the reconnect loop
  and aborts inner tasks, replacing the previous "drop the Arc"
  idiom that leaked the loop for the life of the process.

### Changed
- `CircuitBreakerConfig` moved from `circuit_breaker` into `config` and
  re-exported; it's now wired through `SchwabClient` so the breaker
  wraps the full retry loop.
- `SCHWAB_TOKEN_STORE` now errors loudly on unrecognized values instead
  of silently downgrading to the plaintext `File` backend. Accepted
  values: `keychain`, `encrypted` (aliases `encryptedfile` /
  `encrypted_file`), `file`. Unset uses the platform-secure default.
- `SCHWAB_PKCE_ENABLED` defaults to `false` to match `OAuthConfig` and
  Schwab's reference client (Schwabdev). Enable only after verifying
  your Schwab app accepts `code_challenge`.
- OAuth callback guidance: `https://127.0.0.1:8080` + the built-in
  callback server is now the recommended path; tunnels (cloudflared,
  ngrok, etc.) are demoted to an advanced use case.

### Fixed
- Circuit breaker stored `Instant::now().elapsed()` (~0) in an
  `AtomicU64`, which left the breaker stuck `Open` forever.
  Switched to `RwLock<Option<Instant>>`; added regression test for
  the `Open → HalfOpen` transition after `open_duration` elapses.
- `HttpTransport` now handles HTTP `204 No Content` and empty bodies
  by deserializing `null`, unblocking `DELETE` endpoints like
  `cancel_order` that previously failed JSON parse.
- `SchwabClient::get_market` return type corrected to the nested map
  shape Schwab actually returns (was `Vec<MarketHours>`; deserialize
  used to fail in practice).
- Streaming: reject concurrent `connect()` calls, switch the auth-wait
  from polling to a one-shot so `HalfOpen → Disconnected` flips
  inside a tick can't race, continue the reconnect loop on clean
  peer close instead of exiting silently, fail `connect()` fast
  when the loop terminates before auth, and return `ConnectionFailed`
  on ack timeout instead of silently `Ok(())`.
- `AuthManager::start()` guards against stacking duplicate
  auto-refresh tasks when `start()` is called more than once.

### Removed
- Standalone `auth::PkceSession` helper — no remaining callers; the
  PKCE verifier lifecycle lives directly in `AuthManager`.
- `schwab_rs::utils::{format_time, TimeFormat, format_list,
  format_list_str}` and their crate-root re-exports — ports of the Python
  client's helpers with no callers. The typed query parameters serialize
  their own dates (`schwab_types::common::schwab_timestamp` is the one
  timestamp spelling the Trader API accepts) and symbol lists
  (`std::slice::join(",")`). `utils::canonical_json` stays.

## [0.1.0] - 2026-02-26

### Added

#### Core Features (Q1 2026 SOTA)
- **OAuth 2.0 & OIDC**: Full authorization code flow with PKCE (RFC 7636) and OpenID Connect `id_token` support.
- **Auto-Refresh**: Background token management with configurable buffers and proactive refresh.
- **Secure Token Storage**: Three pluggable backends:
    - `File`: Basic 0600 permission-enforced JSON.
    - `EncryptedFile`: ChaCha20Poly1305 AEAD authenticated encryption (Production Default).
    - `Keychain`: OS-native secure storage (macOS Default).
- **Callback Server**: Integrated Axum-based server for seamless OAuth callback capture (behind `callback-server` feature).

#### Market Data APIs
- **Enhanced Quotes**: Support for `indicative` quotes and field-specific filtering.
- **Price History**: High-resolution historical data with customizable period/frequency types.
- **Options Chains**: Deep integration with Greeks, underlying quotes, and expiration maps.
- **Instruments & Movers**: Full support for security search, CUSIP lookups, and market movers.

#### Trading APIs
- **Order Management**: Place, replace, and cancel orders.
- **Order Preview**: New `preview_order` endpoint for pre-flight validation.
- **Account Services**: Linked account discovery, balance tracking, and position monitoring.
- **Transactions**: Historical transaction auditing across all linked accounts.

#### Advanced Streaming (v0.2.0 Engine)
- **Subscription Persistence**: Automatic re-subscription to all active services on reconnection.
- **Optimized Re-subscription**: Intelligent grouping of symbols by field-set for efficient recovery.
- **Backpressure Control**: Configurable bounded/unbounded channels for stream data routing.
- **Heartbeat Monitoring**: Robust Ping/Pong watchdog with 90-second crash detection.
- **Service Coverage**: All 13 Schwab streaming services implemented with type-safe field constants.

#### Resilience & Observability
- **Rate Limiting**: Built-in `governor`-based throttling (120 req/s with burst of 20).
- **Circuit Breaker**: Automatic fast-failure for degraded upstream services.
- **Retry Logic**: Exponential backoff with jitter for transient error recovery.
- **Telemetry**: Full `tracing` integration with structured JSON logging support.

### Changed
- **Rust Edition**: Upgraded entire workspace to **Rust 2024**.
- **Modernized Dependencies**: Synced to latest Q1 2026 stack (`tokio 1.49`, `reqwest 0.13`, `rand 0.10`).
- **Thin Client Architecture**: Refactored endpoint modules to delegate to a centralized, authenticated `SchwabClient`.

### Fixed
- Deterministic unit tests for environment-dependent configuration.
- `reqwest 0.13` feature naming conflicts (`rustls-tls` -> `rustls`).
- `rand 0.10` API migration for jittered backoff.
- README doctest compilation failures.

### Dependencies
- `tokio` (1.49)
- `reqwest` (0.13)
- `tokio-tungstenite` (0.26)
- `serde` (1.0)
- `chrono` (0.4)
- `governor` (0.7)
- `tracing` (0.1)
- `chacha20poly1305` (0.10)
- `keyring` (3.6)
- `secrecy` (0.10)
