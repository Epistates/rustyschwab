# Schwab Rust SDK

A Rust SDK for Charles Schwab's Trading and Market Data APIs, providing OAuth2 authentication, REST API access, and WebSocket streaming capabilities.

> [!WARNING]
> **RISK WARNING**: This SDK is for advanced users and is provided "as is" without warranty of any kind. Automated trading involves significant risk of financial loss.
> **Test your logic in isolated simulation before using real capital.** The [paper trading mode](https://github.com/epistates/rustyschwab/blob/main/docs/PAPER_TRADING.md) runs locally; it does not connect to thinkorswim paperMoney.
> Use of this SDK is entirely at your own risk.

## Documentation

- **[Full Documentation Index](https://github.com/epistates/rustyschwab/blob/main/docs/INDEX.md)**: Main entry point for all guides.
- **[Setup Guide](https://github.com/epistates/rustyschwab/blob/main/docs/SETUP_GUIDE.md)**: Detailed instructions for getting started.
- **[Placing Orders](https://github.com/epistates/rustyschwab/blob/main/docs/ORDERS.md)**: Comprehensive guide for trading equities and options.
- **[Real-time Streaming](https://github.com/epistates/rustyschwab/blob/main/docs/STREAMING.md)**: Deep dive into the WebSocket client and services.
- **[Troubleshooting](https://github.com/epistates/rustyschwab/blob/main/docs/TROUBLESHOOTING.md)**: Common issues and solutions.
- **[Architecture Overview](https://github.com/epistates/rustyschwab/blob/main/docs/ARCHITECTURE_OVERVIEW.md)** and **[Contributing](https://github.com/epistates/rustyschwab/blob/main/CONTRIBUTING.md)**: For contributors.

## Features

- **[Paper trading and deterministic replay](https://github.com/epistates/rustyschwab/blob/main/docs/PAPER_TRADING.md)** — isolated long-equity execution, finite quote liquidity, durable accounting, simulated latency and authenticated gateway support. Start with `ops/paper/replay.jsonl`.

- **OAuth2 Authentication**
  - Authorization code flow with built-in callback server
  - Automatic token refresh with configurable buffer
  - 7-day refresh token expiration handling with notifications
  - HTTPS validation and security enforcement
  - HTTP client session management
  - Token file corruption recovery

- **Security & Token Storage**
  - **Memory-safe token handling** with automatic zeroing (SecretString)
  - **Three storage backends**: File (basic), EncryptedFile (ChaCha20Poly1305), Keychain (OS-native)
  - **Secure by default**: macOS uses Keychain, other platforms use EncryptedFile
  - **File permissions**: Unix 0600 enforcement on token files
  - **Key derivation**: PBKDF2-HMAC-SHA256, 600k iterations (OWASP 2023 guidance)

- **Market Data APIs**
  - Real-time quotes and price history
  - **Advanced quote options**: Filter by fields and enable `indicative` quotes
  - Option chains with Greeks
  - Market movers and instruments
  - WebSocket streaming for live data

- **Trading APIs**
  - Account management
  - Order placement and management
  - **Order Preview**: Validate orders without execution
  - Position tracking
  - Transaction history

- **Runtime Resilience**
  - Rate limiting tuned to Schwab's 120 requests/minute limit (2 req/s, burst of 10)
  - Exponential backoff retry logic
  - **WebSocket Ping/Pong heartbeat monitoring** (v0.2.0)
  - **Timeout detection with automatic reconnection** (v0.2.0)
  - Field-based subscription batching (scoped)
  - Token expiry notifications
  - Comprehensive error handling
  - Type-safe bindings for commonly used endpoints (OpenAPI coverage in progress)

- **Streaming Advanced Features** (v0.2.0)
  - **Bounded channels** with explicit gap notifications
  - Automatic Ping/Pong heartbeat (20s interval, 30s timeout)
  - Subscription persistence across reconnections
  - Custom field selection per service
  - All 13 streaming services fully implemented

## 🔒 Security

Token handling is layered: secrets are zeroized in memory, encrypted or
keychain-held at rest, and never written to logs. The table compares the
defaults with the Python reference client (Schwabdev).

### Security Features Comparison

| Feature | rustyschwab | Python Reference | Advantage |
|---------|------------|------------------|-----------|
| **Encryption at Rest** | ✅ ChaCha20Poly1305 AEAD | ❌ Plaintext JSON | **Confidentiality** |
| **Tamper Detection** | ✅ AEAD authentication tags | ❌ None | **Data integrity** |
| **Memory Protection** | ✅ SecretString + Zeroize | ❌ Plain strings | **Memory safety** |
| **Keychain Storage** | ✅ macOS/Windows/Linux | ❌ Not implemented | **OS-managed secrets** |
| **File Permissions** | ✅ Enforced 0600 (Unix) | ❌ Default perms | **Access control** |
| **PKCE (RFC 7636)** | ✅ S256 method | ❌ Not implemented | **OAuth 2.1 ready** |
| **Thread Safety** | ✅ Compile-time safe | ⚠️ GIL-dependent | **Concurrency** |
| **Secret Logging** | ✅ Redacted in logs | ⚠️ Potential exposure | **Audit safety** |

### Defense-in-Depth Architecture

**Layer 1: Memory Safety**
```rust,ignore
// Tokens wrapped in SecretString with automatic zeroization
pub struct TokenSet {
    access_token: Option<SecretString>,   // Auto-cleared on drop
    refresh_token: Option<SecretString>,  // Never logged
}
```
- Prevents memory dumps from exposing tokens
- Not visible in core dumps or swap files
- Explicit `expose_secret()` required for access
- Compiler prevents accidental logging

**Layer 2: Encryption at Rest**
```text
// ChaCha20Poly1305 AEAD (authenticated encryption)
- Algorithm: ChaCha20Poly1305 (NCC Group audited, 2020)
- Key: 256-bit random (OS CSPRNG)
- Nonce: 96-bit random per encryption
- Authentication: 128-bit tag prevents tampering
```

**Layer 3: OS-Native Credential Storage**
```text
// Cross-platform secure storage
- macOS: Keychain (secure enclave integration)
- Windows: Credential Manager (DPAPI encryption)
- Linux: Secret Service API (GNOME Keyring/KWallet)
```

**Layer 4: File Permissions**
```text
// Unix: Enforced 0600 (owner read/write only)
- Verified before every read
- Applied after every write
- Prevents unauthorized access on shared systems
```

**Layer 5: Transport Security**
```text
// TLS/HTTPS enforcement
- rustls: Memory-safe TLS (no OpenSSL vulnerabilities)
- HTTPS-only callback URLs (validated at config time)
- WSS for streaming (TLS over WebSocket)
```

### PKCE Support (RFC 7636)

PKCE is implemented (S256) but **opt-in** — Schwab's public OAuth docs
and the Schwabdev Python reference client do not use it, so we default
to `pkce_enabled: false`. Turn it on only if you've verified your
Schwab app accepts `code_challenge` / `code_verifier`:

```rust,ignore
use schwab_rs::auth::OAuthConfig;

let oauth = OAuthConfig {
    app_key: "YOUR_APP_KEY".into(),
    app_secret: "YOUR_SECRET".into(),
    callback_url: "https://127.0.0.1:8080".into(),
    pkce_enabled: true,  // opt in after verifying Schwab accepts it
    ..Default::default()
};
```

When enabled, the flow uses a cryptographically-random verifier and
an `S256` (SHA-256) challenge to mitigate authorization-code
interception.

### Security Best Practices

**1. Choose the Right Token Storage Backend**

```rust,ignore
use schwab_rs::auth::TokenStoreKind;

// Development: Quick start with basic security
token_store_kind: TokenStoreKind::File,  // 0600 permissions only

// Production (Recommended): Encrypted file storage
token_store_kind: TokenStoreKind::EncryptedFile,  // ChaCha20Poly1305 AEAD

// Production (Best): OS-native credential storage
token_store_kind: TokenStoreKind::Keychain,  // Default on macOS
```

**Security Levels:**
- **File:** Basic (0600 permissions) - Development/testing only
- **EncryptedFile:** High (AES-equivalent encryption + tamper detection) - Production recommended
- **Keychain:** Best (OS-level encryption + user authentication) - Production default on macOS

**2. Enable Token Notifications**

```rust,ignore
use schwab_rs::auth::{OAuthConfig, TokenNotification};
use std::sync::Arc;

let oauth = OAuthConfig {
    on_token_notification: Some(Arc::new(|notification| {
        match notification {
            TokenNotification::RefreshTokenExpiring { hours_remaining } => {
                eprintln!("⚠️  Refresh token expires in {} hours!", hours_remaining);
                // Send alert, log to monitoring, etc.
            }
            TokenNotification::TokenFileCorrupted => {
                eprintln!("🔒 Token file was corrupted and recreated");
            }
            _ => {}
        }
    })),
    ..Default::default()
};
```

**3. Secure Your Credentials**

```bash
# NEVER commit credentials to version control
echo "schwab_tokens.json" >> .gitignore
echo "schwab_tokens.json.key" >> .gitignore
echo ".env" >> .gitignore

# Use environment variables
export SCHWAB_APP_KEY="your_32_char_key"
export SCHWAB_APP_SECRET="your_16_char_secret"

# Or use a secret management service
# - AWS শর্ Secrets Manager
# - HashiCorp Vault
# - Azure Key Vault
```

**4. Production Deployment Checklist**

- [ ] Use `EncryptedFile` or `Keychain` storage backend
- [ ] Set up token expiry notifications
- [ ] Configure proper file permissions (0600 on Unix)
- [ ] Use HTTPS-only callback URLs
- [ ] Never log credentials or tokens
- [ ] Rotate credentials regularly
- [ ] Monitor for token expiry events
- [ ] Use rate limiting in production
- [ ] Enable retry logic with exponential backoff

### Cryptographic Implementation Details

**ChaCha20Poly1305 AEAD (EncryptedFile Backend)**

```text
// Encryption process
1. Generate random 256-bit key (once, stored separately)
2. For each encryption:
   - Generate random 96-bit nonce (OS CSPRNG)
   - Encrypt plaintext with ChaCha20
   - Compute Poly1305 authentication tag
   - Output: [nonce(12) || ciphertext || tag(16)]

// Security properties
- Confidentiality: ChaCha20 stream cipher (256-bit key)
- Authenticity: Poly1305 MAC (128-bit tag)
- Tamper detection: Any modification causes decryption failure
```

### Vulnerability Reporting

If you discover a security vulnerability, please email `security@epistates.com` or open a confidential GitHub Security Advisory. Do not open public issues for security vulnerabilities.

### Security Dependencies

All security-critical dependencies are industry-standard and regularly audited:

| Crate | Purpose | Security Track Record |
|-------|---------|----------------------|
| `ring` | Cryptography (CSPRNG, PKCE) | Used by Google, Chromium, Firefox |
| `rustls` | TLS implementation | Memory-safe, no OpenSSL CVEs |
| `chacha20poly1305` | Encryption | NCC Group audit (2020) |
| `keyring` | OS credential storage | 1M+ downloads, cross-platform |
| `secrecy` | Secret handling | De facto Rust standard |
| `zeroize` | Memory clearing | Prevents secret leakage |

**No known vulnerabilities** in any security-critical dependency.

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
schwab-rs = { version = "0.1", features = ["callback-server"] }
schwab-types = "0.1"
```

## Quick Start

### 1. Set up OAuth Authentication

```rust,ignore
use schwab_rs::{SchwabClient, SchwabConfig, auth::{OAuthConfig, TokenNotification}};
use std::sync::Arc;

// Configure OAuth with notifications
let oauth_config = OAuthConfig {
    app_key: "YOUR_32_CHAR_APP_KEY".to_string(),
    app_secret: "YOUR_16_CHAR_SECRET".to_string(),
    callback_url: "https://127.0.0.1:8080".to_string(),
    capture_callback: true,  // Auto-capture auth code
    auto_refresh: true,      // Auto-refresh tokens
    refresh_buffer_seconds: 61, // Refresh before expiry
    on_token_notification: Some(Arc::new(|notification| {
        if let TokenNotification::RefreshTokenExpiring { hours_remaining } = notification {
            println!("Warning: Refresh token expires in {} hours", hours_remaining);
        }
    })),
    ..Default::default()
};

// Create client
let config = SchwabConfig { oauth: oauth_config, ..Default::default() };
let client = SchwabClient::new(config)?;
// Start background token management
client.init().await?;
```

### 2. Authorize and Get Tokens

```rust,ignore
use schwab_rs::auth::AuthManager;

// For initial OAuth, create an AuthManager and run the flow once
let auth = AuthManager::new(client_config.oauth.clone())?;
let (auth_url, _code) = auth.authorize().await?;
println!("Open this URL in a browser: {}", auth_url);
// After authorization, tokens are saved to the configured file and auto-refreshed by client.init()
```

### 3. Use the API

```rust,ignore
use chrono::{Duration, Utc};
use schwab_rs::types::Decimal;
use schwab_rs::types::market_data::{
    FrequencyType, OptionChainParams, PeriodType, PriceHistoryParams, QuoteField,
};
use schwab_rs::types::trading::{
    OrderInstruction, OrderRequest, OrderRequestLeg, OrderStatus, OrdersParams,
    TransactionType, TransactionsParams,
};

// Get quotes
let quotes = client.get_quotes(&["AAPL", "MSFT"]).await?;

// Advanced quotes: only request specific sections and enable indicative pricing
let adv_quotes = client.get_quotes_with_options(
    &["AAPL"],
    &[QuoteField::Quote, QuoteField::Reference],
    Some(true)
).await?;

// Price history — period/frequency combinations are validated against
// Schwab's tables before the request is sent
let history = client.get_price_history(&PriceHistoryParams {
    period_type: Some(PeriodType::Day),
    period: Some(10),
    frequency_type: Some(FrequencyType::Minute),
    frequency: Some(5),
    ..PriceHistoryParams::new("AAPL")
}).await?;

// Get option chain
let chain = client.get_option_chain(&OptionChainParams::new("SPY")).await?;

// Build an order — buy 10 AAPL at 150.25, then take profit at 160 or
// stop out at 145 (a TRIGGER → OCO tree; see docs/ORDERS.md)
let order = OrderRequest::limit(
    OrderRequestLeg::equity(OrderInstruction::Buy, "AAPL", 10),
    "150.25".parse()?,
)
.bracket(Decimal::from(160), Decimal::from(145))?;

// Preview an order (validate without executing)
let preview = client.preview_order("account_hash", &order).await?;

// Place an order — Schwab returns the new order's ID via the
// `Location` header (no JSON body)
let response = client.place_order("account_hash", &order).await?;
println!("order id: {:?}", response.order_id);

// List what filled this month, and the trades and dividends behind it.
// Windows are `DateTime<Utc>`; the client emits Schwab's exact timestamp
// spelling and rejects an inverted window before sending anything.
let month_start = Utc::now() - Duration::days(30);
let filled = client.get_account_orders("account_hash", &OrdersParams {
    status: Some(OrderStatus::Filled),
    ..OrdersParams::new(month_start, Utc::now())
}).await?;
let activity = client.get_transactions("account_hash", &TransactionsParams::new(
    month_start,
    Utc::now(),
    [TransactionType::Trade, TransactionType::DividendOrInterest],
)).await?;
```

### 4. Stream Market Data

```rust,ignore
use schwab_rs::streaming::{StreamClient, StreamMessage};
use schwab_rs::types::streaming::{StreamPayload, StreamService};

// Build streaming client
// customer/correl IDs, channel, and function ID come from
// `get_user_preferences()` → `streamerInfo`
let stream_client = StreamClient::builder()
    .config(stream_config)
    .auth_manager(auth_manager.clone())
    .streamer_info(&streamer_info)
    .build()?;

// Connect and (attempt to) subscribe
stream_client.connect().await?;
stream_client.subscribe(StreamService::LeveloneEquities, vec!["AAPL".into(), "MSFT".into()]).await?;

// Handle messages. Frames arrive with numbered fields; `decode()` turns
// them into typed payloads named like the REST quotes.
if let Some(mut receiver) = stream_client.get_receiver() {
    while let Some(msg) = receiver.recv().await {
        match msg {
            StreamMessage::Data(data) => {
                for item in &data.data {
                    for payload in item.decode()? {
                        if let StreamPayload::LevelOneEquity(quote) = payload {
                            println!("{} bid {:?} ask {:?}", quote.key, quote.bid_price, quote.ask_price);
                        }
                    }
                }
            }
            StreamMessage::Response(resp) => println!("Response: {:?}", resp),
            StreamMessage::Notify(hb) => println!("Heartbeat: {}", hb.heartbeat),
        }
    }
}
```

Every data service has a field table (`LevelOneEquityFields`, `BookFields`, …)
for building subscription field lists and a payload type (`LevelOneEquity`,
`Book`, `ChartEquity`, `AccountActivity`, …) in `schwab_rs::types::streaming`.

Note: Streaming authentication, reconnection, and subscription handling are fully implemented.

## OAuth Callback URL

Schwab requires an HTTPS callback URL. For every normal development
and agent-on-dev-machine setup, use:

```bash
SCHWAB_OAUTH_CALLBACK_URL=https://127.0.0.1:8080
```

Register that exact string in the Schwab Developer Portal, enable the
built-in callback server (`cargo run --features callback-server`),
click through your browser's self-signed-cert warning on first run,
and you're done. No tunnel, no external binary, no rotating URL.

### Advanced: public HTTPS tunnels

You only need a tunnel (ngrok, cloudflared, a reverse proxy in front
of a VM, etc.) if the machine running the OAuth flow can't host the
browser doing the Schwab consent dance — e.g. headless servers, or
corporate networks that block localhost callbacks.

In that case:

1. Stand up your tunnel however you like, pointing at local port
   `8080` (or wherever you're binding the callback server).
2. Register the resulting public HTTPS URL in the Developer Portal.
   Free `trycloudflare.com` URLs rotate each launch, so prefer a
   named / stable tunnel.
3. Set `SCHWAB_ALLOW_EXTERNAL_CALLBACK=true` so the built-in server
   binds to `0.0.0.0` instead of `127.0.0.1`.

This is genuinely an advanced case — 127.0.0.1 is the supported
happy path.

## Token Management

### Access Token (30 minutes)
- Automatically refreshed before expiry
- Background task handles refresh seamlessly

### Refresh Token (7 days)
- Must restart OAuth flow when expired
- SDK detects expiration and returns appropriate error
- Optional notifications warn before expiry

### Testing Token Scenarios

```bash
# Run the auth test example
cd examples/auth_test
cargo run -- --app-key YOUR_KEY --app-secret YOUR_SECRET status
```

## Environment Variables

Required:

```bash
export SCHWAB_APP_KEY="your_32_character_app_key"
export SCHWAB_APP_SECRET="your_16_char_secret"
export SCHWAB_OAUTH_CALLBACK_URL="https://127.0.0.1:8080"
```

Common optional overrides:

```bash
# Token persistence (defaults to ./schwab_tokens.json; platform-secure backend chosen automatically)
export SCHWAB_TOKENS_FILE="$HOME/.config/schwab/tokens.json"
export SCHWAB_TOKEN_STORE="keychain"   # keychain | encrypted | file

# OAuth tuning
export SCHWAB_PKCE_ENABLED=false       # Schwab's reference client does not use PKCE
export SCHWAB_REFRESH_BUFFER=61        # seconds before expiry to refresh

# MCP observability + PII
export SCHWAB_METRICS_BIND=127.0.0.1:9090
export SCHWAB_MCP_REDACT_PII=1         # account-number masking (creds/identity always dropped)
export SCHWAB_MCP_ALLOW_REMOTE=0       # allow non-loopback http/ws bind (bearer policy and TLS proxy required)
```

See `.env.example` for the full, commented catalog.

## MCP Server (`schwab-mcp`)

The `schwab-mcp` crate wraps the full SDK as a [Model Context Protocol](https://modelcontextprotocol.io/)
server — every REST endpoint and every streaming subscription becomes a tool
an LLM agent (Claude Desktop, IDEs, custom clients) can invoke.

### Build & run

```bash
# STDIO transport (default — for Claude Desktop, mcp-inspector, etc.)
cargo run -p schwab-mcp --release

# HTTP transport
SCHWAB_MCP_TRANSPORT=http SCHWAB_MCP_BIND=127.0.0.1:8765 cargo run -p schwab-mcp --release

# WebSocket transport
SCHWAB_MCP_TRANSPORT=websocket SCHWAB_MCP_BIND=127.0.0.1:8765 cargo run -p schwab-mcp --release
```

Tokens must already exist on disk (run the `oauth_flow` example first, or any
`AuthManager`-based flow that persists to `SCHWAB_TOKENS_FILE`).

Network transports require `SCHWAB_MCP_POLICY_FILE` and a bearer token on
every request. Principals have explicit account/tool permissions and isolated
sessions. Remote access also requires a TLS proxy and
`SCHWAB_MCP_ALLOW_REMOTE=1` for non-loopback binding. See the
[policy configuration and migration guide](https://github.com/epistates/rustyschwab/blob/main/docs/ENTERPRISE_REMEDIATION.md).


### Guardrail for real trades

Write-side order tools (`place_order`, `cancel_order`, `replace_order`) are
**disabled by default**. Place/replace require `SCHWAB_MCP_ALLOW_TRADING=1`
and principal policy approval. Cancellation can be enabled independently with
`SCHWAB_MCP_ALLOW_CANCEL=1`. All policy-bound tools, including preview,
require an explicit tool permission.

### Tool surface

- **Market data** — `get_quotes`, `get_quote`, `get_price_history`,
  `get_option_chain`, `get_option_expiration_chain`, `get_movers`,
  `search_instruments`, `get_instrument`, `get_markets`, `get_market`
- **Accounts** — `get_account_numbers`, `get_accounts`, `get_account`,
  `get_user_preferences`
- **Projections** — `get_candles` (`[t,o,h,l,c,v]` rows),
  `get_option_contracts` (one row per contract, ordered by expiration and
  strike) and `get_positions` (one row per position) take the same
  parameters as their raw tools and answer in a fraction of the bytes:
  rows under a `columns` header, paged within
  `SCHWAB_MCP_RESPONSE_BUDGET_BYTES` (default 64 KiB) or an explicit
  `limit`, with `truncated: true` and a `nextCursor` to continue from
  rather than silent clipping
- **Orders (read)** — `get_order`, `get_account_orders`, `get_all_orders`
- **Orders (write)** — `build_order(spec)` expands a compact spec
  (`kind`: `limit`, `bracket`, `oco`, …; option legs by OSI symbol or
  contract parts) into the exact `order` JSON the write tools accept, locally
  and without contacting Schwab; `check_order` reports every local reason a
  submission would fail (validity, policy limits, approval, trading switch,
  halts) with the intent digest to approve, also without contacting Schwab;
  `preview_order`, `place_order`, `cancel_order`, `replace_order` *(last
  three gated)*
- **Transactions** — `get_transactions`, `get_transaction`
- **Streaming** — `stream_connect`, `stream_disconnect`, `stream_status`,
  `stream_subscribe(service, keys)`, `stream_unsubscribe(service, keys)`,
  `stream_set_service_fields(service, fields_csv)`, `stream_snapshot(service, keys?)`
- **Server** — `get_server_status` reports the mode (`live`), the trading
  switches and operator halt, readiness (the `/readyz` body), this process's
  request budget, stream health, the principal's own permissions and limits,
  and the equity session phase from Schwab's calendar (cached per day), so a
  model can plan a session before spending calls on it

The time-windowed reads (`get_account_orders`, `get_all_orders`,
`get_transactions`, `get_price_history`) take their bounds as a calendar
date (`YYYY-MM-DD`), RFC 3339, or epoch milliseconds; orders and
transactions default to the last seven days ending now (transactions to
`TRADE`), so `params` may be omitted entirely.

### Metrics (Prometheus)

Metrics are **opt-in**. Set `SCHWAB_METRICS_BIND=127.0.0.1:9090` and the
MCP will spawn a dedicated Prometheus scrape listener on that address;
unset, no listener and every `metrics::counter!`/`histogram!` call is
a no-op (the SDK depends only on the `metrics` facade, so library-only
consumers are free to install their own recorder — StatsD, OTLP, etc.).

```bash
SCHWAB_METRICS_BIND=127.0.0.1:9090 cargo run -p schwab-mcp --release
# in another shell
curl http://127.0.0.1:9090/metrics
```

**Metric catalog** (the source of truth is `schwab_rs::metrics::CATALOG` and
`schwab_mcp::metrics::CATALOG`; a workspace test keeps call sites and docs in
line with them, and the server registers their help text with Prometheus):

| Name | Kind | Labels | Description |
|------|------|--------|-------------|
| `schwab_http_requests_total` | counter | `op`, `method`, `status` | Logical request completions. `status` is a class bucket (`2xx`…`5xx`, `unknown`) or `error` when no response was obtained. |
| `schwab_http_request_duration_seconds` | histogram | `op`, `method` | Wall-clock per request (includes retries + token refresh). |
| `schwab_submission_unknown_total` | counter | — | Order submissions sent with an undetermined outcome; each needs reconciliation. |
| `schwab_retry_attempts_total` | counter | `outcome` | `succeeded` / `exhausted`. Sums the retries that were *actually fired*. |
| `schwab_circuit_state` | gauge | — | `0=Closed`, `1=HalfOpen`, `2=Open`. |
| `schwab_circuit_transitions_total` | counter | `from`, `to` | State changes. |
| `schwab_token_refresh_total` | counter | `outcome`, `grant` | One per `/oauth/token` request: `success`/`failure` × `authorization_code`/`refresh_token`. |
| `schwab_reconciliation_total` | counter | `outcome` | Account reconciliation runs: `ready` / `unresolved`. |
| `schwab_pending_submissions` | gauge | — | Submissions awaiting reconciliation at the last readiness probe. |
| `schwab_unreconciled_accounts` | gauge | — | Required accounts without a fresh baseline at the last readiness probe. |
| `schwab_baseline_age_seconds` | gauge | — | Age of the oldest required-account baseline at the last readiness probe. |
| `schwab_trading_ready` | gauge | — | `1` when the last readiness probe found trading admissible. |
| `schwab_recovery_ready` | gauge | — | `1` while the recovery worker has no accounts left to reconcile. |
| `schwab_stream_connects_total` | counter | `outcome` | `success` / `failure` per connection attempt. |
| `schwab_stream_reconnects_total` | counter | — | Reconnect loop iterations. |
| `schwab_stream_connected` | gauge | — | `1` while authenticated, `0` otherwise. |
| `schwab_stream_messages_total` | counter | `service`, `kind` | `kind ∈ {data, response, notify, dropped}`. Control frames use `service="_control"`, unparseable ones `"_unparsed"`. |
| `schwab_stream_dropped_messages_total` | counter | — | Frames dropped because the consumer queue was full or closed. |
| `schwab_mcp_tool_calls_total` | counter | `tool`, `outcome` | `ok` / `error` per MCP tool invocation. |
| `schwab_mcp_tool_call_duration_seconds` | histogram | `tool` | End-to-end latency per tool. |
| `schwab_stream_recovery_seconds` | histogram | — | Stream cache recovery time after a reconnect. |
| `schwab_stream_recovery_ready` | gauge | — | `1` when the last stream recovery completed within its deadline. |

**Starter PromQL:**

```promql
# 5xx error rate per operation
sum by (op) (rate(schwab_http_requests_total{status="5xx"}[5m]))

# p99 latency by op (Prometheus histogram quantile)
histogram_quantile(0.99,
  sum by (le, op) (rate(schwab_http_request_duration_seconds_bucket[5m])))

# Tool call error ratio
sum by (tool) (rate(schwab_mcp_tool_calls_total{outcome="error"}[5m]))
 / sum by (tool) (rate(schwab_mcp_tool_calls_total[5m]))

# Circuit breaker currently open (alert on > 0)
max_over_time(schwab_circuit_state[1m]) == 2

# Stream healthy (alert on = 0 during market hours)
schwab_stream_connected

# Token refresh failures — if this is non-zero sustained, wake someone up
increase(schwab_token_refresh_total{outcome="failure"}[15m])
```

### Claude Desktop config

Add to `~/Library/Application Support/Claude/claude_desktop_config.json`
(macOS) — adjust paths to your checkout:

```json
{
  "mcpServers": {
    "schwab": {
      "command": "/path/to/rustyschwab/target/release/schwab-mcp",
      "env": {
        "SCHWAB_APP_KEY": "…",
        "SCHWAB_APP_SECRET": "…",
        "SCHWAB_OAUTH_CALLBACK_URL": "https://127.0.0.1:8080",
        "SCHWAB_TOKENS_FILE": "/Users/you/.config/schwab/tokens.json"
      }
    }
  }
}
```

## Examples

See the `examples/` directory for working examples:

- `auth_test/` - OAuth flow testing and token management
- `oauth_flow/` - Minimal end-to-end OAuth
- `oauth_callback_server/` - Local callback capture
- `comprehensive/` - Combined flows and API usage

### Feature flags
- `callback-server`: runs a local Axum server to capture OAuth callback

### Token Storage Backends

The SDK provides three secure token storage options (automatically selected by platform):

1. **File** (Basic security)
   - Plain JSON with 0600 permissions (owner read/write only)
   - Useful for quick migration from older versions

2. **EncryptedFile** (High security) ⭐ **Default on Linux/Windows**
   - ChaCha20Poly1305 authenticated encryption (NCC Group audited)
   - Tamper detection via AEAD authentication tag
   - Random key generation with secure storage
   - 0600 permissions on both token and key files

3. **Keychain** (Best security) ⭐ **Default on macOS**
   - OS-native secure credential storage
   - macOS: Keychain, Windows: Credential Manager, Linux: Secret Service
   - Protected by user authentication
   - Cross-platform via `keyring` crate

### Configure Token Storage

```rust,ignore
use schwab_rs::{SchwabClient, SchwabConfig};
use schwab_rs::auth::{OAuthConfig, TokenStoreKind};

let oauth = OAuthConfig {
    app_key: "YOUR_APP_KEY".into(),
    app_secret: "YOUR_SECRET".into(),
    callback_url: "https://127.0.0.1:8080".into(),

    // Choose storage backend (defaults to Keychain on macOS, EncryptedFile elsewhere)
    token_store_kind: TokenStoreKind::Keychain,        // OS-native (best)
    // token_store_kind: TokenStoreKind::EncryptedFile, // Encrypted file (recommended)
    // token_store_kind: TokenStoreKind::File,          // Plain file (basic)

    ..Default::default()
};

let cfg = SchwabConfig { oauth, ..Default::default() };
let client = SchwabClient::new(cfg)?;
client.init().await?; // Tokens automatically use selected storage
```

**Security Features:**
- **Memory safety**: Tokens use `SecretString` with automatic zeroing on drop
- **Encryption**: ChaCha20Poly1305 AEAD (EncryptedFile backend)
- **File permissions**: 0600 enforcement on Unix systems
- **Keychain support**: Cross-platform OS-native storage
- **No logging**: Tokens never appear in debug/log output

## Configuration

The SDK provides sensible defaults that can be customized:

```rust,ignore
use schwab_rs::config::{SchwabConfig, ClientConfig, RateLimitConfig, RetryConfig};
use std::time::Duration;

let config = SchwabConfig {
    client: ClientConfig {
        timeout: Duration::from_secs(10),
        rate_limit: RateLimitConfig {
            enabled: true,
            // Schwab's documented limit is 120 requests per minute
            requests_per_second: 2,
            burst_size: 10,
        },
        retry: RetryConfig {
            max_retries: 3,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            backoff_multiplier: 2.0,
            retry_on_status: vec![429, 500, 502, 503, 504],
        },
        ..Default::default()
    },
    ..Default::default()
};
```

## Error Handling

The SDK provides comprehensive error types for robust error handling:

```rust,ignore
use schwab_rs::error::Error;

match client.get_quotes(&["AAPL"]).await {
    Ok(quotes) => println!("Success: {:?}", quotes),
    Err(Error::Auth(e)) => println!("Authentication error: {}", e),
    Err(Error::RateLimit { retry_after }) => println!("Rate limit exceeded, retry after {}s", retry_after),
    Err(Error::Network(e)) => println!("Network error: {}", e),
    Err(e) => println!("Other error: {}", e),
}
```

## Important Notes

1. **Exact Callback URL Match**: Must exactly match registered URL (HTTPS only, no trailing slash)
2. **Key Lengths**: Schwab issues several variants; keys/secrets must be even-length and at least 32 chars combined
3. **Rate Limits**: Schwab allows 120 requests/minute; the SDK defaults to 2 req/s with a burst of 10
4. **Token Expiration**: Access tokens expire in ~30 minutes; refresh tokens in ~7 days
5. **Session Management**: HTTP client is recreated after token refresh
6. **Token Storage**: Defaults to Keychain (macOS) or EncryptedFile (other platforms)
7. **Token Notifications**: Optional callbacks for expiry warnings and session events

Known limitations and notes:
- Optional PKCE can be enabled via `OAuthConfig.pkce_enabled` (off by default)
- `SchwabClient` uses `transport::http::HttpTransport` for REST

## Streaming: Advanced Features (v0.2.0)

### Bounded Channels for Backpressure Control

Control memory usage with bounded channels:

```rust,ignore
use schwab_rs::{StreamConfig, ChannelKind};

let mut config = StreamConfig::default();

// Legacy compatibility setting (now capped at 65,536 messages)
config.channel_kind = ChannelKind::Unbounded;

// Bounded (default capacity: 4,096; overflow invalidates stream health)
config.channel_kind = ChannelKind::Bounded(10000); // 10k message buffer

let stream = StreamClient::builder()
    .config(config)
    .auth_manager(auth_manager)
    .customer_id(customer_id)
    .build()?;
```

### Custom Field Selection

Request only needed fields for bandwidth optimization:

```rust,ignore
use schwab_rs::types::streaming::{LevelOneEquityFields as F, StreamService};

// Request a lean field set by name; field 0 is always the key.
let fields = [F::SYMBOL, F::BID_PRICE, F::ASK_PRICE, F::LAST_PRICE, F::TOTAL_VOLUME]
    .map(|field| field.to_string())
    .join(",");
stream.set_service_fields(StreamService::LeveloneEquities, fields);

stream.subscribe_level_one_equities(&["AAPL", "MSFT"]).await?;
```

### Heartbeat Monitoring

Automatic Ping/Pong heartbeat with timeout detection (v0.2.0):

```rust,ignore
let mut config = StreamConfig::default();
config.heartbeat_interval = Duration::from_secs(20); // Ping every 20s
config.ping_timeout = Duration::from_secs(30);       // Reconnect if no Pong

// Automatic timeout detection and reconnection!
```

### Complete Examples

The `streaming-examples` package in `examples/streaming/` has three bins:
- `streaming_demo` - all 13 services subscribed once
- `streaming_processing` - decoupled async processing pattern
- `streaming_quotes` - real-time quotes for a watchlist

```bash
cargo run -p streaming-examples --bin streaming_quotes
```

## Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](https://github.com/epistates/rustyschwab/blob/main/CONTRIBUTING.md) for guidelines.

## License

This project is dual-licensed under the MIT License and Apache License 2.0.

## Disclaimer

This SDK is not affiliated with, endorsed by, or officially connected with Charles Schwab & Co., Inc. Use at your own risk. Always test thoroughly with sandbox data before using in production.

## Support

For issues and questions:
- GitHub Issues: [Project Issues](https://github.com/epistates/rustyschwab/issues)
- Documentation: [docs.rs/schwab-rs](https://docs.rs/schwab-rs)

## Development

### OAuth setup

Set required environment variables (or place in `.env`):

```bash
export SCHWAB_APP_KEY=...
export SCHWAB_APP_SECRET=...
export SCHWAB_OAUTH_CALLBACK_URL=https://127.0.0.1:8080
```

Optional:

```bash
export SCHWAB_PKCE_ENABLED=false
export SCHWAB_TOKEN_STORE=keychain      # Options: keychain | encrypted | file
```

### Examples

```bash
# Basic OAuth flow
cargo run -p oauth-flow

# Comprehensive SDK demonstration
cargo run -p comprehensive

# Authenticate and test token management
cargo run -p auth_test -- --app-key KEY --app-secret SECRET status

# Streaming demos
cargo run -p streaming-examples --bin streaming_demo
cargo run -p streaming-examples --bin streaming_quotes
cargo run -p streaming-examples --bin streaming_processing
```

See [enterprise remediation and migration notes](https://github.com/epistates/rustyschwab/blob/main/docs/ENTERPRISE_REMEDIATION.md)
for execution reconciliation, decimal order requests, principal authorization,
stream freshness, compiler support, and release validation requirements.


The 0.2.0 candidate adds transactional state, reconciliation, shared local
coordination, short-lived identity, portfolio risk integration and operator
commands. See [enterprise operations](https://github.com/epistates/rustyschwab/blob/main/docs/OPERATIONS.md) for migration,
configuration and external release gates. This candidate has not been published.

For multi-host workers, verified operator controls and broker dispatch through a shared authority, see [the shared authority guide](https://github.com/epistates/rustyschwab/blob/main/docs/SHARED_AUTHORITY.md).

See the [MCP and LLM integration review](https://github.com/epistates/rustyschwab/blob/main/docs/MCP_LLM_INTEGRATION.md) for client boundaries, paper-mode gaps, and the TurboMCP v4 migration criteria.
