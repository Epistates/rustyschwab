# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
