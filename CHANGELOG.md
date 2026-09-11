# Changelog

## Paper trading candidate

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


### Enterprise follow-up (0.2 candidate)

- Revalidate risk after admission waits; persist operator halts independently of recovery.
- Split full order-history windows, follow known old orders by ID, and include external open-order exposure.
- Add authenticated remote backends and a broker authority/gateway with verified operator roles and fenced risk validation.
- Deliver audit batches with exact durable acknowledgements and canonical object-key hashing.
- Bound REST recovery and synthetic delivery gaps; cover shared state, approval replay/revocation and recovery end to end.
- Existing risk plugins must implement dispatch revalidation; use durable halt APIs for operator control. Review outstanding approvals after canonical digest migration.


All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased — 0.2.0 candidate]

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
  `{"instruments": [...]}` wrapper.
- **Market hours**: `get_markets`/`get_market` return the doubly nested
  `{market_type: {product: hours}}` map Schwab actually sends.
- **Transactions**: parameter renamed `type` → `types` (the name Schwab
  accepts) and `start_date`/`end_date`/`types` are now required
  arguments, as the API mandates. `get_account_orders`/`get_all_orders`
  similarly require `fromEnteredTime`/`toEnteredTime`.
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

### MCP dependency and integration review

- Upgrade stable TurboMCP to 3.3 and refresh compatible locked dependencies.
- Add behavior hints to all MCP tools and live-execution guidance to discovery.
- Remove unused dependencies, configure weekly dependency checks, and document LLM integration gaps and v4 migration criteria.
