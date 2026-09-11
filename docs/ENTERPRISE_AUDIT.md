**Enterprise trading readiness audit — 2026-09-09**

**Verdict: not ready for unattended production trading.** The architecture has useful foundations—shared async clients, typed interfaces, secret wrappers, encrypted storage, reconnect handling, metrics, and default-disabled MCP trading—but critical execution, authentication, recovery, and data-contract failures remain. Endpoint coverage alone does not establish parity with Schwabdev, and no comparative performance evidence establishes state-of-the-art status.

Scope: the current working tree based on `c4d6884ed44cbde8db782364d0a35893a394613a`, including the substantial pre-existing uncommitted changes. Reviewed SDK, types, MCP, CI, tests, and the local `../../reference/Schwabdev` implementation, validation, tests, and reference payloads. Production source was not changed. No live account requests or orders were made; probes used temporary token files, synthetic credentials, and localhost mocks. This is a source and local-test audit, not a live broker certification, regulatory assessment, penetration test, or latency benchmark.

**Validation evidence** (files under `docs/audit/` are local, git-ignored artifacts)

| Check | Result |
|---|---|
| `cargo test --workspace --all-targets --locked` | 125 passed, 3 ignored, 0 failed on macOS ARM64 / rustc 1.98.0 |
| `cargo clippy --workspace --all-targets --locked --offline -- -D warnings` | Passed |
| `cargo fmt --all -- --check` | Passed on the original audited tree |
| `cargo +1.85 check --workspace --all-targets --locked --offline` | Failed: locked dependencies require newer compilers; see `audit/msrv.log` |
| Targeted diagnostic probes | 9 passed: each asserts that a defect is present; see `audit/probes.rs` and `audit/probes.log` |
| `cargo audit --file Cargo.lock --json` | Failed: 6 vulnerability advisories plus 4 warnings; fresh RustSec database dated 2026-09-09; see `audit/security.json` |

The passing suite is not a readiness signal: token refresh tests include ignored placeholders, and the existing response fixtures omit the failing order, option, and fundamental-data contracts. Linux/Windows execution, feature combinations, rustdoc, load/soak tests, and broker-side behavior were not independently verified here.

**Prioritized findings**

P0 means a direct duplicate-execution risk. P1 means a release blocker for the affected trading workflow or security boundary. P2 means a reliability, deployment, or compatibility gap to close before enterprise rollout. “Reproduced” refers to isolated local probes; other findings are source-confirmed unless stated otherwise.

**F01 — P0: automatic retries can duplicate submitted orders. Reproduced.**

`crates/schwab-rs/src/client.rs:180–265` passes order POST/PUT through the same retry stack as reads. `retry.rs:64–124` and `error.rs:301–310` retry network errors and selected server errors without an execution-outcome classification. The probe observed four POST requests for one `place_order` call returning 503. If the broker accepts the first request but the response is lost or an intermediary returns an error, replay can create another order. The probe proves replay, not a live duplicate fill. Schwabdev `client.py:169–172,259–273` does not add this wrapper-level automatic replay.

Fix: operation-specific retry semantics, a distinct unknown-submission-outcome result, and reconciliation before any further submission. Record submission intent and broker IDs; do not promise exactly-once execution without broker support. Verify accept-then-drop, timeout, cancellation, and 5xx scenarios. This follows the constraints on automatic non-idempotent retries in [RFC 9110 §9.2.2](https://www.rfc-editor.org/rfc/rfc9110.html#section-9.2.2).

**F02 — P1: order models reject normal responses and cannot express protective strategies. Reproduced in part.**

`schwab-types/src/trading.rs:24–48,93–103` models `orderId` and `accountNumber` as strings, but the reference's order-details response uses numeric values (`Schwabdev/docs/pages-raw/api.md:340–390`). `Gtc` emits `GTC`, while reference orders use `GOOD_TILL_CANCEL`. Both numeric IDs and GTC responses fail decoding in the probe. One such order can fail an entire order-list response.

The request type also lacks `stopPrice`, trailing-stop parameters, and `childOrderStrategies`, despite exposing Stop, TrailingStop, Oco, and Trigger enums. A supplied bracket order deserialized into this type loses its children as unknown fields; a stop price is likewise discarded. Execution activity is absent, limiting fill reconciliation. Reference examples at `orders.md:212–357` can pass these fields through Python dictionaries.

Fix: separate validated order requests from broker responses, support the complete strategy tree and execution activity, normalize IDs explicitly, and reject unsupported request fields instead of silently stripping them. Golden fixtures must cover DAY/GTC, stops, trailing stops, OCO, triggers, and partial fills. Add finite/positive quantity and price validation plus explicit decimal/tick-size rules; `f64` by itself is not an execution validation policy.

**F03 — P1: option-chain, expiration, and fundamental quote contracts are incompatible. Reproduced.**

`schwab-types/src/market_data.rs:118–153,265–335` expects integer `divPayDate`, integer option `expirationDate`, string `multiplier`, nested `greeks`, and an expiration response with `symbol` and `expirations`. The reference supplies ISO date strings, numeric multiplier, flat delta/gamma/theta/vega/rho, and `expirationList` (`api.md:742,1025–1046,1268`). Date/multiplier mismatches fail decoding; flat Greeks disappear even when the other types are adapted. A default quote response containing a dividend date can therefore fail the entire quote call.

Fix: fixtures for every supported asset class and option strategy, correct wire types, retain unmodeled response fields, and validate risk fields semantically. Include quote subsets, nulls, large volumes, deliverables, and adjusted options.

**F04 — P1: browser callback does not validate OAuth state. Source-confirmed.**

`schwab-rs/src/auth/mod.rs:949–968` accepts the first callback code and consumes the oneshot sender without checking `CallbackParams.state`. The manual URL path at `666–704` validates state, but the automatic callback goes directly to the code exchange at `620–621`. PKCE is configurable and disabled by default. An unsolicited callback can consume the pending flow; accepting another authorization code is a CSRF/account-binding risk when PKCE is absent.

Fix: share one callback validator, verify expected state before consuming the pending transaction, expire and consume it once, and test missing, mismatched, repeated, and concurrent callbacks. Browser binding and PKCE support must be validated against the broker. [RFC 9700](https://www.rfc-editor.org/rfc/rfc9700.html#section-4.7) requires OAuth clients to prevent CSRF.

**F05 — P1: refresh changes the recorded lifetime of an unchanged refresh token. Reproduced.**

`auth/mod.rs:799–804,941–945` reuses the refresh token but constructs a new `TokenSet` with the current issue time. `auth/token_store.rs:125–132` sets its expiry to that time plus seven days. The probe advances the expiry by two days without changing the token. The seven-day reauthorization warning/guard consequently becomes unreliable under regular refresh. Python preserves `_refresh_token_issued` on access-token refresh (`tokens.py:320`).

The background path also replaces only in-memory tokens (`auth/mod.rs:895–900`), omitting persistence and the foreground refresh-runway guard. A restarted process reloads old state; rotated refresh tokens, if returned, can be lost. Fix with one refresh transaction that preserves the refresh-token epoch, validates runway, persists the result, and publishes it consistently. Add multi-process storage coordination; the current async mutex coordinates only one manager family.

**F06 — P1: 401 recovery and structured error handling break resilience. Reproduced.**

`client.rs:280–292` calls `ensure_valid_tokens` after 401, but `auth/mod.rs:498–500` returns immediately for an unexpired token. The probe observed two requests with the rejected token and zero refresh calls. Independently, `error.rs:328–337` turns structured 401/503 bodies into `Error::Api`, losing the HTTP status used for auth and retry decisions. Identical statuses behave differently depending on response-body format.

Fix: preserve status, broker error details, and correlation headers together. Implement forced refresh conditional on the rejected token generation so concurrent 401s share one refresh. Test structured and unstructured errors, revoked-but-unexpired tokens, and concurrent callers.

**F07 — P1: default circuit breaker becomes stuck half-open. Reproduced.**

`circuit_breaker.rs:119–139` counts half-open admissions cumulatively and never releases them. Defaults require three successes but allow one counted probe (`config.rs:125–133`); the transition request is uncounted. After five failures, two successful calls are admitted and all subsequent calls are refused forever in HalfOpen. The probe reproduces this without wall-clock sleeps. A transient incident can thus permanently block reads and order cancellations through the shared breaker.

Fix: count concurrent probes with cancellation-safe permits, use coherent state transitions/generations, and permit sequential recovery probes. Test cancellation, nonretryable responses, in-flight completions from an old state, concurrency, and recovery under the default configuration.

**F08 — P1: stream cache returns incomplete and stale data as snapshots. Source-confirmed.**

`schwab-mcp/src/stream_cache.rs:31–33,70–88` replaces the entire content object on each update. With a bid+ask update followed by a bid-only delta, the ask disappears. The feeder discards envelope timestamps, and snapshots have no age, session generation, or validity flag. Cache clearing occurs on explicit disconnect, not automatic connection loss; unsubscribe leaves old keys. This is unsafe input for automated decisions without external freshness checks. Reference documentation explicitly identifies quote deltas (`tc_details.md:55`).

Fix: service-aware delta merging, receive/broker timestamps, session identity, stale-state propagation, bounded retention, and fresh-baseline recovery. Keep account activity as events with reconciliation; last-value storage cannot preserve an execution history.

**F09 — P1: bounded streaming drops messages without consumer-visible gap notification. Source-confirmed.**

`streaming/client.rs:41–44` uses `try_send`; on a full bounded queue, `1098–1100` logs and continues. `config.rs:43–46` instead describes blocking backpressure. Unbounded mode is the default. Under a slow consumer, the choices are unbounded memory or missing updates; the receiver is not told that its state is now invalid.

Fix: explicit per-service overflow semantics. Coalesce appropriate quote updates; surface gaps and require reconciliation for order/account activity; isolate control responses from data queues; report queue depth, drops, and freshness. Test sustained bursts with deliberately slow consumers while keeping heartbeat/control processing responsive.

**F10 — P1: stream restoration can report health without restored subscriptions. Source-confirmed.**

`streaming/client.rs:527–532,553–557` updates local subscriptions before acknowledgement and does not roll back a rejection. `1010–1054` replays subscriptions without registering or validating their acknowledgements. `1064–1090` resolves only the first response item in a frame. Separately, `authenticate_read` checks elapsed time around an unbounded `read_half.next().await` (`978–1001`), so its five-second timeout is not enforced on a silent socket. The outer initial timeout leaves the spawned connection loop alive; reconnect authentication has no equivalent effective deadline.

Fix: distinguish desired/pending/confirmed subscriptions, handle every response item, gate readiness on replay acknowledgements, enforce actual async deadlines, and guarantee task cleanup on every failure/cancellation path. Mock tests should exercise rejected replay, multi-response frames, silent login, transport close, and repeated reconnect.

**F11 — P2: search/movers parameters, CUSIP response, and account type drift from reference. Reproduced in part.**

`client.rs:452,477` uses uppercased Debug formatting: `PERCENTCHANGEUP` and `SYMBOLSEARCH`, whereas Python validates `PERCENT_CHANGE_UP` and `symbol-search` (`validation.py:29–33`). All instrument projections are emitted in uppercase. `get_instrument` (`client.rs:490–499`) expects an unwrapped Instrument, while the reference CUSIP response is wrapped in `instruments` (`api.md:1833–1856`). `accounts.rs:33–35` maps account type to `accountType`, losing the broker's `type`; the probe shows MARGIN becoming None. Fix explicit wire mappings and shared endpoint contract fixtures, with assertions on meaningful fields rather than deserialization alone.

**F12 — P2: rate limits and deadlines do not bound actual request traffic. Source-confirmed.**

`client.rs:237–265` acquires one limiter permit per logical call; retries and 401 replay do not acquire another. Separate clients have separate budgets. The two-per-second burst-ten default is not a strict 120-request rolling-minute cap. `transport/http.rs:136–143` only parses integer Retry-After values; `retry.rs:102–124` can sleep for an arbitrarily long server-specified delay. Fix permit acquisition per wire attempt, credential-scoped/shared quota coordination where needed, HTTP-date parsing, and a caller-controlled overall deadline covering queueing, refresh, retries, and network I/O. Validate broker quotas separately rather than inferring compliance from average RPS.

**F13 — P1 for shared enterprise deployment: MCP lacks caller authorization and durable execution controls. Source-confirmed.**

`schwab-mcp/src/main.rs:35–66` explicitly documents unauthenticated HTTP/WebSocket access; remote exposure requires an opt-in and warns about a proxy. That guard and default-disabled trading are useful protections. Once enabled, however, the application has one global trading permission (`server.rs:24–48`), shared accounts, shared streaming state, and no principal/account policy or durable submission audit. An authenticating proxy alone does not implement account-level authorization or risk controls.

Fix before multi-user exposure: authenticated identity, per-account/operation authorization, request attribution, account/notional/quantity limits, controlled execution approval where required, and a durable intent/outcome journal. Cancellation permission should be independently configurable. These requirements concern the claimed enterprise deployment scope, not a claim that the safe default loopback configuration is remotely exploitable.

**F14 — P2: MCP status and account-activity subscription are misleading. Source-confirmed.**

`server.rs:589–595,734–735` reports connected from `Option::is_some`, even after the socket dies or retries are exhausted. `connect_stream` then refuses to rebuild an existing dead client. `stream_subscribe` documents empty keys for account activity but forwards them to generic `subscribe` (`638–658`), producing an empty key instead of the SDK helper's fixed `Account Activity` (`streaming/client.rs:357–362`). Fix status/readiness from actual lifecycle state and route account activity through its dedicated contract; verify disconnect/reconnect and this MCP tool end to end.

**F15 — P2: file storage is not equally durable or coordinated across platforms. Source-confirmed.**

Unix writes use restrictive creation permissions, randomized temporary files, and rename—good foundations. However, `security.rs:266–279` uses direct truncating writes and no enforced ACL on non-Unix platforms, contradicting stronger atomic-write/current-user-only descriptions. `auth/token_store.rs:289–315` uses check-then-create encryption keys without interprocess coordination, so simultaneous first writers can encrypt with different keys while overwriting the shared key file. Fix platform-appropriate atomic replacement, permissions, and process locks; test crash/restart and concurrent initialization. Encryption with a key stored beside ciphertext also requires directory and backup access controls.

**F16 — P2: redaction does not cover all error/log paths. Source-confirmed.**

`streaming/client.rs:1108–1114` logs up to 512 raw characters of an unparseable stream frame before MCP redaction. Account-activity data can therefore reach logs on schema drift. `client.rs:163,189` logs account-bearing paths; `server.rs:98–100` returns upstream error strings without the successful-response redaction walk. Fix structured, allowlisted diagnostics with safe correlation IDs and tests containing synthetic account/token markers. Do not rely on truncation to remove sensitive fields.

**F17 — P2: declared compiler support is broken. Reproduced.**

`Cargo.toml:15` declares Rust 1.85 and CI includes that toolchain, but Cargo rejects the lockfile: TurboMCP 3.1.5 requires 1.89 and several dependencies require 1.88. Source also uses let chains, stabilized in [Rust 1.88](https://blog.rust-lang.org/2025/06/26/Rust-1.88.0/). Raise and verify the supported minimum or consistently pin compatible dependencies and remove newer syntax. Fix the release claim and CI gate together.

**F18 — P1 release gate: the lockfile contains known vulnerable dependency versions. Scan-confirmed; exploitability not established.**

The fresh RustSec scan reports six advisories. These are lockfile matches, not six demonstrated exploits. `cargo tree -i h2 -e normal` confirms h2 in the active HTTP stack; `cargo tree -i rkyv -e normal` prints no active dependency in the tested default workspace graph. Assess every shipping feature/platform graph before assigning application-level exploitability.

| Locked package | Advisory | Patched constraint reported by RustSec |
|---|---|---|
| crossbeam-epoch 0.9.18 | [RUSTSEC-2026-0204](https://rustsec.org/advisories/RUSTSEC-2026-0204.html): invalid pointer dereference in pointer formatting | >=0.9.20 |
| h2 0.4.13 | [RUSTSEC-2026-0258](https://rustsec.org/advisories/RUSTSEC-2026-0258.html): unbounded empty DATA frames | >=0.4.16 |
| quinn-proto 0.11.14 | [RUSTSEC-2026-0185](https://rustsec.org/advisories/RUSTSEC-2026-0185.html): stream reassembly memory exhaustion | >=0.11.15 |
| rkyv 0.8.15 | [RUSTSEC-2026-0233](https://rustsec.org/advisories/RUSTSEC-2026-0233.html): crafted-archive use-after-free | >=0.8.17 |
| rkyv 0.8.15 | [RUSTSEC-2026-0234](https://rustsec.org/advisories/RUSTSEC-2026-0234.html): hash-table archive out-of-bounds reads | >=0.8.17 |
| rkyv 0.8.15 | [RUSTSEC-2026-0235](https://rustsec.org/advisories/RUSTSEC-2026-0235.html): Rc/Arc archive out-of-bounds reads | >=0.8.17 |

Additional warnings flag unmaintained rustls-pemfile, unsoundness advisories for anyhow and rkyv, and yanked chacha20 0.10.0. See the archived JSON for exact versions and advisory details. Fix through reviewed dependency updates/removal, validate compatibility and feature reachability, and rerun the fresh scan plus the contract/recovery suite. Do not silently suppress inactive-feature findings: record their disposition and prevent accidental activation. This audit does not change the existing lockfile.

**Comparison with the local Python reference**

| Capacity | Current assessment | Required improvement |
|---|---|---|
| REST endpoint inventory | Broad nominal parity | Exact method/path/query/body/header/response fixtures for every endpoint |
| Orders and complex strategies | Behind Python's arbitrary dictionaries and examples | Complete validated strategy model, protective orders, execution reconciliation |
| Data fidelity | Behind raw-response preservation | Correct all contracts; retain unknown response fields and offer metadata/raw access |
| Token lifecycle | Rust adds secret wrappers/storage backends; lifecycle regresses | Preserve refresh epoch, persist background refresh, coordinate processes |
| Streaming services | Broad helper coverage | Proven restoration, explicit gaps, correct deltas and readiness |
| Validation | Python has an explicit validation layer | Typed builders plus broker-rule validation and forward-compatible responses |
| Resilience | Rust has more mechanisms but unsafe interactions | Operation-aware retries, recoverable breaker, deadlines and request budgets |
| Integration/operations | Rust adds MCP and Prometheus | Caller authorization, reliable health, durable audit and risk controls |
| Throughput/latency | Unproven | Reproducible comparative benchmarks and sustained-load evidence |
| Developer experience | Rust types and examples are useful | Accurate docs, complete fixtures, supported compiler/platform matrix |

The Python reference is an oracle for comparison, not absolute authority. Its tests explicitly preserve some sync/async differences (for example `market_hours` query representation). Adjudicate disagreements using versioned broker documentation and sanitized broker captures. Reference trading-client documentation was inspected, but no corresponding implementation was present in the listed `schwabdev` package; those documentation claims were not counted as verified implemented capabilities.

**Release sequence and measurable gates**

1. **Make execution safe.** Close F01/F02/F04/F05/F06/F07. Require one automatic submission attempt after any ambiguous outcome, durable reconciliation state, complete protective-order fixtures, callback-negative tests, token restart/expiry tests, and recovery under default breaker settings. Preserve unknown outcomes rather than encouraging callers to blindly retry.
2. **Establish contract correctness.** Close F03/F11 and relevant F14 cases. Build a shared, sanitized fixture corpus from the reference and broker evidence. Test each endpoint through HTTP, not only serde structs; compare parsed values and serialized request bytes. Cover all supported asset classes, sparse/null fields, unknown enums, and execution events.
3. **Make streaming trustworthy.** Close F08–F10/F14. Introduce explicit transport/auth/subscription/data-freshness states. Require consumer-visible loss accounting, gap-triggered REST reconciliation, bounded memory, and acknowledged restoration. Replay disconnects, duplicate/out-of-order updates, silent peers, and slow consumers deterministically.
4. **Complete enterprise operations.** Close F12–F18. Deliver identity/policy boundaries, risk limits, durable audit/recovery, secure cross-platform persistence, redaction checks, readiness probes, feature/platform CI, supported MSRV, and fresh dependency scanning. Exercise process restart during order submission and refresh.
5. **Demonstrate superiority.** Benchmark Rust and Python against the same local replay workload and hardware: allocations, RSS plateau, messages/sec, p50/p95/p99 processing latency, queue delay, and reconnect-to-fresh-data time. Separate broker/network latency from SDK overhead. Run a market-session-length replay plus a 24-hour fault-injected soak. Require zero unexplained account-event loss and bounded memory; set numeric latency SLOs from intended deployment workloads, then publish measurements. No HFT or universal “faster” claim follows merely from using Rust.

**Reproducing the diagnostic probes**

The probe source is archived outside the normal test suite because its assertions intentionally confirm defects. Copy `docs/audit/probes.rs` to `crates/schwab-rs/tests/audit_probes.rs`, then run `cargo test -p schwab-rs --test audit_probes --locked --offline` with localhost binding permitted. Remove the temporary copied file afterward. As fixes land, invert the relevant assertions into permanent regression tests that require correct behavior. The archived probes are evidence of this audited snapshot and are expected to fail once the defects are fixed.
