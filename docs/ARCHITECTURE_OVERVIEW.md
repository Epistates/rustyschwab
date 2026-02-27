# Schwab Rust SDK - Comprehensive Architecture Overview

## Executive Summary

The **rustyschwab** project is a production-grade Rust SDK for Charles Schwab's Trading and Market Data APIs. It provides comprehensive coverage of OAuth2 authentication, REST API endpoints, WebSocket streaming, and includes world-class security features. Total codebase: **6,485 lines of Rust** across **30 files** organized into 2 main crates.

---

## 1. Project Purpose and Architecture

### Core Mission
Provide a **type-safe, secure, and performant** Rust binding for the Charles Schwab API ecosystem, enabling:
- **OAuth2 authentication** with advanced security (PKCE, token refresh, keychain storage)
- **RESTful API calls** to market data, trading, and account endpoints
- **WebSocket streaming** for real-time market data with automatic reconnection
- **Enterprise-grade resilience** through rate limiting, retries, and backpressure control

### High-Level Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                   Applications/Examples                      │
└─────────────────────────────────────────────────────────────┘
                            ↑
┌─────────────────────────────────────────────────────────────┐
│                  schwab-rs (Main SDK)                       │
├─────────────────────────────────────────────────────────────┤
│                                                              │
│  ┌──────────────────┐     ┌──────────────────────────────┐ │
│  │   SchwabClient   │     │   StreamClient/WebSocket     │ │
│  │  (REST Access)   │     │   (Real-time Data)           │ │
│  └──────────────────┘     └──────────────────────────────┘ │
│         ↓                           ↓                        │
│  ┌──────────────────┐     ┌──────────────────────────────┐ │
│  │  AuthManager     │     │  SubscriptionManager         │ │
│  │  (OAuth2 + Tkn)  │     │  (Service subscriptions)     │ │
│  └──────────────────┘     └──────────────────────────────┘ │
│         ↓                           ↓                        │
│  ┌──────────────────┐     ┌──────────────────────────────┐ │
│  │ HttpTransport    │     │ WebSocketTransport           │ │
│  │ (REST over HTTP) │     │ (Streaming over WS)          │ │
│  └──────────────────┘     └──────────────────────────────┘ │
│         ↓                           ↓                        │
│  ┌──────────────────┐     ┌──────────────────────────────┐ │
│  │ Retry + RateLimit│     │ Heartbeat + Reconnection     │ │
│  │ (Resilience)     │     │ (Connection Management)      │ │
│  └──────────────────┘     └──────────────────────────────┘ │
│                                                              │
└─────────────────────────────────────────────────────────────┘
                            ↑
┌─────────────────────────────────────────────────────────────┐
│             schwab-types (Type Definitions)                │
├─────────────────────────────────────────────────────────────┤
│  Market Data | Trading | Accounts | Streaming | Common      │
└─────────────────────────────────────────────────────────────┘
                            ↑
┌─────────────────────────────────────────────────────────────┐
│        Security Layer (Encryption + Auth)                   │
├─────────────────────────────────────────────────────────────┤
│  ChaCha20Poly1305 | Keychain | PKCE | SecretString         │
└─────────────────────────────────────────────────────────────┘
```

### Key Architectural Principles

1. **Layered Design**: Separation of concerns (auth, transport, business logic)
2. **Security by Default**: Multiple storage backends, encrypted tokens, memory safety
3. **Resilience Pattern**: Built-in retries, rate limiting, and circuit breaking
4. **Type Safety**: Full Rust type system utilization for compile-time guarantees
5. **Async-First**: Tokio-based async runtime for concurrent operations
6. **Pluggable Storage**: Multiple token storage backends (File, EncryptedFile, Keychain)

---

## 2. Main Crates and Modules Structure

### Crate 1: `schwab-rs` (5,244 LOC) - Core SDK

The main SDK implementation containing all business logic and integrations.

```
schwab-rs/
├── src/
│   ├── lib.rs (52 LOC)
│   │   └─ Public API surface and module exports
│   │
│   ├── client.rs (596 LOC) ⭐ CORE
│   │   ├─ SchwabClient: Main entry point (REST operations)
│   │   ├─ SchwabClientBuilder: Fluent configuration
│   │   ├─ ClientInner: Arc-wrapped internal state
│   │   ├─ Rate limiting (governor crate)
│   │   ├─ Retry policy application
│   │   ├─ Request/response handling
│   │   └─ Token refresh on 401 errors
│   │
│   ├── config.rs (365 LOC)
│   │   ├─ SchwabConfig: Top-level configuration
│   │   ├─ ClientConfig: HTTP client settings
│   │   ├─ StreamConfig: WebSocket configuration
│   │   ├─ RetryConfig: Exponential backoff settings
│   │   ├─ RateLimitConfig: Rate limiting parameters
│   │   ├─ ReconnectConfig: WebSocket reconnection strategy
│   │   └─ ChannelKind: Bounded vs Unbounded channels for backpressure
│   │
│   ├── error.rs (256 LOC)
│   │   ├─ Error: Main error enum (HttpError, Auth, WebSocket, etc.)
│   │   ├─ AuthError: Authentication-specific errors
│   │   ├─ StreamError: Streaming-specific errors
│   │   └─ Comprehensive error handling and conversion
│   │
│   ├── retry.rs (133 LOC)
│   │   ├─ RetryPolicy: Implements exponential backoff
│   │   ├─ Configurable max retries and backoff strategy
│   │   ├─ Status-code based retry decisions
│   │   └─ 5-minute max elapsed time cap
│   │
│   ├── security.rs (391 LOC) ⭐ SECURITY CRITICAL
│   │   ├─ EncryptedToken: ChaCha20Poly1305 AEAD wrapper
│   │   ├─ Encryption: Random nonce generation, encryption/decryption
│   │   ├─ Key management: Random key generation, PBKDF2 derivation
│   │   ├─ File permissions: Unix 0600 enforcement
│   │   ├─ PKCE support: Code verifier/challenge generation
│   │   └─ OWASP-compliant encryption (600k PBKDF2 iterations)
│   │
│   ├── utils.rs (224 LOC)
│   │   ├─ format_list/format_list_str: Symbol list formatting
│   │   ├─ format_time: Timestamp formatting
│   │   ├─ TimeFormat enum: Multiple time format options
│   │   └─ Utility functions for API parameter construction
│   │
│   ├── auth/ (918 + 483 = 1,401 LOC) ⭐ AUTHENTICATION
│   │   ├── mod.rs (918 LOC)
│   │   │   ├─ AuthManager: OAuth2 flow orchestration
│   │   │   ├─ OAuthConfig: Detailed OAuth configuration
│   │   │   ├─ TokenNotification: Event callbacks for token lifecycle
│   │   │   ├─ TokenStoreKind: Enum for storage backend selection
│   │   │   ├─ CallbackServer: Axum-based local callback capture
│   │   │   ├─ Token refresh background task
│   │   │   ├─ PKCE support (S256 method)
│   │   │   ├─ 7-day refresh token expiration tracking
│   │   │   ├─ Auto-refresh with configurable buffer
│   │   │   └─ State management for concurrent operations
│   │   │
│   │   └── token_store.rs (483 LOC) ⭐ SECURE STORAGE
│   │       ├─ TokenSet: Token container with SecretString wrapping
│   │       ├─ TokenStore trait: Pluggable storage abstraction
│   │       ├─ FileTokenStore: Plain JSON with 0600 permissions
│   │       ├─ EncryptedFileTokenStore: ChaCha20Poly1305 encrypted
│   │       ├─ KeychainTokenStore: Cross-platform OS-native storage
│   │       ├─ Token validation (expiry checking)
│   │       └─ Secure serialization/deserialization
│   │
│   ├── streaming/ (1,547 LOC) ⭐ REAL-TIME DATA
│   │   ├── client.rs (1,396 LOC)
│   │   │   ├─ StreamClient: Main streaming entry point
│   │   │   ├─ StreamClientBuilder: Fluent builder pattern
│   │   │   ├─ ConnectionState: Tracks connection lifecycle
│   │   │   ├─ MessageSender/MessageReceiver: Bounded/Unbounded channels
│   │   │   ├─ Ping/Pong heartbeat (20s interval, 30s timeout)
│   │   │   ├─ Exponential backoff reconnection (2s-128s)
│   │   │   ├─ 90-second crash detection
│   │   │   ├─ Subscription persistence across reconnections
│   │   │   ├─ Service-specific field selection
│   │   │   ├─ 13 streaming services fully implemented
│   │   │   └─ Request ID tracking and ACK handling
│   │   │
│   │   ├── subscription.rs (147 LOC)
│   │   │   ├─ SubscriptionManager: Tracks active subscriptions
│   │   │   ├─ Subscription: Individual subscription state
│   │   │   ├─ Field-based subscription batching
│   │   │   └─ Service state tracking
│   │   │
│   │   └── mod.rs (6 LOC)
│   │       └─ Module re-exports
│   │
│   ├── transport/ (160 LOC)
│   │   ├── mod.rs (4 LOC)
│   │   │   └─ Module re-exports
│   │   │
│   │   ├── http.rs (74 LOC)
│   │   │   ├─ HttpTransport: Thin wrapper over reqwest
│   │   │   ├─ Request building (headers, query, body)
│   │   │   ├─ Response handling
│   │   │   └─ HTTP status code → Error mapping
│   │   │
│   │   └── websocket.rs (82 LOC)
│   │       ├─ WebSocketTransport: WebSocket wrapper
│   │       ├─ Connection establishment
│   │       ├─ Message send/receive operations
│   │       └─ Stream splitting for concurrent read/write
│   │
│   └── endpoints/ (101 LOC - Thin wrappers)
│       ├── mod.rs (7 LOC)
│       ├── accounts.rs (16 LOC) → SchwabClient methods
│       ├── instruments.rs (15 LOC) → SchwabClient methods
│       ├── market_data.rs (18 LOC) → SchwabClient methods
│       ├── movers.rs (14 LOC) → SchwabClient methods
│       ├── options.rs (11 LOC) → SchwabClient methods
│       ├── price_history.rs (11 LOC) → SchwabClient methods
│       ├── quotes.rs (11 LOC) → SchwabClient methods
│       └── trading.rs (14 LOC) → SchwabClient methods
│
└── Cargo.toml
    ├─ Dependencies version: 0.1.0
    ├─ Features: rustls-tls, native-tls, callback-server, full
    └─ See Cargo.toml section below
```

### Crate 2: `schwab-types` (1,241 LOC) - Type Definitions

Pure data types crate with no business logic (no dependencies on SDK, async, or network code).

```
schwab-types/
├── src/
│   ├── lib.rs (12 LOC)
│   │   └─ Module re-exports for public API
│   │
│   ├── market_data.rs (470 LOC) ⭐ QUOTES & CHAINS
│   │   ├─ QuotesResponse: Quote data wrapper
│   │   ├─ QuoteItem: Individual quote with 4 sub-types (Quote, Fundamental, Extended, Reference)
│   │   ├─ QuoteQuote: OHLCV, spreads, Greeks
│   │   ├─ QuoteFundamental: P/E, dividend, margin metrics
│   │   ├─ OptionChainResponse: Option data structure
│   │   ├─ OptionExpirationDate: Expiration info
│   │   ├─ OptionChain: Underlying + expiration map
│   │   ├─ OptionData: Strike, bid/ask, Greeks (Delta, Gamma, Theta, Vega)
│   │   ├─ PriceHistoryResponse: OHLCV bars with timestamps
│   │   ├─ PriceHistoryItem: Individual bar data
│   │   └─ Serde + serde_with for flexible deserialization
│   │
│   ├── trading.rs (242 LOC) ⭐ ORDERS & POSITIONS
│   │   ├─ Order: Complete order structure
│   │   ├─ OrderLeg: Individual leg in multi-leg order
│   │   ├─ OrderInstrument: Equity, option, future identifiers
│   │   ├─ OrderInstruction: BUY/SELL/BUY_TO_COVER/SELL_SHORT
│   │   ├─ OrderType: MARKET/LIMIT/STOP/STOP_LIMIT/TRAILING_STOP/etc
│   │   ├─ OrderSession: NORMAL/EXTENDED
│   │   ├─ OrderDuration: DAY/GOOD_TIL_CANCEL/FILL_OR_KILL/etc
│   │   ├─ OrderStatus: AWAITING_PARENT_ORDER/PENDING_ACTIVATION/etc
│   │   ├─ OrderStrategyType: SINGLE/OCO/BTO/etc
│   │   ├─ Position: Holdings data
│   │   ├─ PositionEffect: OPEN/CLOSE/AUTOMATIC
│   │   └─ ComplexOrderStrategyType: Advanced multi-leg types
│   │
│   ├── accounts.rs (147 LOC) ⭐ ACCOUNT DATA
│   │   ├─ AccountsResponse: List of accounts wrapper
│   │   ├─ Account: Core account structure
│   │   ├─ AccountFields: Account data (balances, trading power, etc.)
│   │   ├─ Position: Holdings information
│   │   ├─ SecuritiesAccount: Unified container for account data
│   │   ├─ BalanceFields: Cash, margin, options buying power
│   │   └─ TransactionItem: Trade history entry
│   │
│   ├── streaming.rs (308 LOC) ⭐ REAL-TIME PROTOCOL
│   │   ├─ StreamRequest: Service subscription request
│   │   ├─ StreamRequests: Batch request wrapper
│   │   ├─ StreamCommand: LOGIN/LOGOUT/SUBS/ADD/UNSUBS/VIEW
│   │   ├─ StreamParameters: Dynamic request parameters
│   │   ├─ StreamMessage: Enum for all response types
│   │   │   ├─ Data: Streaming data (market quotes, orders, etc.)
│   │   │   ├─ Response: Acknowledgment to subscription request
│   │   │   ├─ Notify: Heartbeat and connection events
│   │   │   └─ Snapshot: Initial data on subscription
│   │   ├─ StreamService: 13 services (LevelOneEquities, OptionChains, etc.)
│   │   ├─ StreamDataType: Data type discriminator
│   │   ├─ Service-specific data structures (EquityData, OptionData, etc.)
│   │   └─ serde_json::Value for flexible deserialization
│   │
│   └── common.rs (62 LOC)
│       ├─ Common enums: AssetType, OrderLegType, Instruction
│       └─ Shared type definitions
│
└── Cargo.toml
    └─ Minimal dependencies (serde, chrono, uuid)
```

---

## 3. Key Dependencies in Cargo.toml Files

### Workspace Dependencies (Root)

```toml
# Async Runtime (Tokio)
tokio = "1.41" (features: full)
tokio-tungstenite = "0.24" (features: native-tls)
futures-util = "0.3"
async-trait = "0.1"

# HTTP and Networking
reqwest = "0.12" (features: json, rustls-tls, cookies, stream)
hyper = "1.5"
url = "2.5"
headers = "0.4"

# Serialization (serde ecosystem)
serde = "1.0" (features: derive)
serde_json = "1.0"
serde_urlencoded = "0.7"
serde_with = "3.11"

# OAuth2
oauth2 = "4.4"
base64 = "0.22"

# Error Handling
thiserror = "2.0"
anyhow = "1.0"

# Logging (facade pattern - user chooses impl)
log = "0.4"

# Time and Date
chrono = "0.4" (features: serde)
humantime-serde = "1.1"

# Utilities
once_cell = "1.20"
parking_lot = "0.12" (RwLock for thread-safe shared state)
pin-project = "1.1"
bytes = "1.8"
uuid = "1.11" (features: v4, serde)

# Security (Best-in-class 2025)
ring = "0.17" (CSPRNG, cryptography)
rustls = "0.23" (Memory-safe TLS)
webpki-roots = "0.26" (Root CA certificates)
chacha20poly1305 = "0.10" (AEAD encryption - NCC Group audited)
keyring = "3.6" (OS-native credential storage)
secrecy = "0.10" (features: serde, Memory-safe secret handling)
zeroize = "1.8" (features: derive, aarch64, Automatic memory clearing)

# CLI and Config
clap = "4.5" (features: derive, env)
config = "0.14"
dotenvy = "0.15"

# Testing
wiremock = "0.6"
mockito = "1.6"
mockall = "0.13"
tower = "0.5"

# Rate Limiting & Retry
backoff = "0.4" (features: tokio, Exponential backoff)
governor = "0.7" (Rate limiting)

# Web Framework (Optional)
axum = "0.7" (optional, for callback-server feature)
```

### Security Dependency Rationale

| Crate | Purpose | Security Audits | Notes |
|-------|---------|-----------------|-------|
| `ring` | CSPRNG, cryptography primitives | Used by Google, Mozilla | Random nonce/PKCE generation |
| `rustls` | TLS implementation | Multiple audits | Memory-safe replacement for OpenSSL |
| `chacha20poly1305` | AEAD encryption | NCC Group 2020 | Token file encryption |
| `keyring` | OS credential storage | Cross-platform | Keychain, Credential Manager, Secret Service |
| `secrecy` | Secret handling | De facto Rust standard | Automatic token zeroization |
| `zeroize` | Memory clearing | Industry standard | Prevents token leakage in memory dumps |

---

## 4. Main Source Files and Their Purposes

### Critical Files (>100 LOC each)

| File | LOC | Purpose | Key Components |
|------|-----|---------|-----------------|
| `streaming/client.rs` | 1,396 | WebSocket streaming client | StreamClient, heartbeat, reconnection, subscriptions |
| `auth/mod.rs` | 918 | OAuth2 authentication flow | AuthManager, token refresh, callback capture |
| `client.rs` | 596 | REST API client | SchwabClient, rate limiting, retry logic, endpoints |
| `auth/token_store.rs` | 483 | Secure token storage | TokenSet, FileTokenStore, EncryptedFileTokenStore, KeychainTokenStore |
| `types/market_data.rs` | 470 | Quote and option chain types | Quotes, Options, Greeks, Price History |
| `security.rs` | 391 | Encryption and key mgmt | ChaCha20Poly1305, PKCE, file permissions |
| `config.rs` | 365 | Configuration structures | SchwabConfig, ClientConfig, StreamConfig |
| `types/trading.rs` | 242 | Order and position types | Order, OrderLeg, Position, Transaction |
| `utils.rs` | 224 | Utility functions | Symbol formatting, time formatting |
| `error.rs` | 256 | Error types | Error, AuthError, StreamError enums |
| `types/streaming.rs` | 308 | WebSocket message types | StreamRequest, StreamMessage, StreamService |

### Lightweight Files (<50 LOC each) - Thin Wrappers

All endpoint files in `endpoints/` are thin wrappers that delegate to `SchwabClient` methods. This design centralizes logic in the client while maintaining clean separation of concerns:

- `endpoints/accounts.rs` (16 LOC)
- `endpoints/instruments.rs` (15 LOC)
- `endpoints/market_data.rs` (18 LOC)
- `endpoints/movers.rs` (14 LOC)
- `endpoints/options.rs` (11 LOC)
- `endpoints/price_history.rs` (11 LOC)
- `endpoints/quotes.rs` (11 LOC)
- `endpoints/trading.rs` (14 LOC)

**Design Pattern**: These files exist for API organization but forward calls to methods in `SchwabClient`, keeping implementation centralized.

---

## 5. Architectural Patterns and Decisions

### 1. **Builder Pattern**
```rust
// SchwabClientBuilder for fluent configuration
let client = SchwabClient::builder()
    .config(config)
    .build()?;

// StreamClientBuilder for streaming setup
let stream = StreamClient::builder()
    .config(stream_config)
    .auth_manager(auth_manager)
    .customer_id(customer_id)
    .build()?;
```

### 2. **Arc<RwLock<T>> for Thread-Safe State**
```rust
pub struct SchwabClient {
    inner: Arc<ClientInner>,  // Shareable, cloneable
}

pub struct StreamClient {
    inner: Arc<StreamClientInner>,
}
```
**Rationale**: Allows `SchwabClient` to be cloned and shared across threads while maintaining single ownership of internal state.

### 3. **Pluggable Storage Backends (Strategy Pattern)**

```rust
pub enum TokenStoreKind {
    File,           // Plain JSON (development)
    EncryptedFile,  // ChaCha20Poly1305 (production recommended)
    Keychain,       // OS-native (production best)
}
```
**Rationale**: Allows users to choose security/complexity tradeoff without code changes.

### 4. **Trait-Based Abstraction for Transport**

```rust
// HTTP transport (thin wrapper over reqwest)
pub struct HttpTransport { ... }

// WebSocket transport (thin wrapper over tokio-tungstenite)
pub struct WebSocketTransport { ... }
```
**Rationale**: Separates networking details from business logic; enables testing with mocks.

### 5. **Exponential Backoff Retry Policy**

```rust
pub struct RetryPolicy {
    config: RetryConfig,
}

// 1. Initial backoff: 1s
// 2. Multiplier: 2.0
// 3. Max backoff: 30s
// 4. Max elapsed: 5 minutes
```
**Rationale**: Prevents thundering herd; respects rate limits gracefully.

### 6. **Rate Limiting with Governor**

```rust
let quota = Quota::per_second(120).allow_burst(20);
let rate_limiter = RateLimiter::direct(quota);
```
**Default**: 120 req/s with 20-request burst (Schwab's limits)
**Pattern**: Async-aware using `rate_limiter.until_ready().await`

### 7. **Bounded vs Unbounded Channels for Backpressure**

```rust
pub enum MessageReceiver {
    Unbounded(mpsc::UnboundedReceiver<StreamMessage>),  // No backpressure
    Bounded(mpsc::Receiver<StreamMessage>),             // Backpressure
}
```
**Rationale**: 
- **Unbounded**: Fast, compatible with slow consumers (memory at risk)
- **Bounded**: Memory-bounded, slows producer when buffer full

### 8. **Heartbeat + Ping/Pong for Connection Stability**

```rust
// Streaming client sends Ping every 20s, expects Pong within 30s
// Automatic reconnection if timeout detected
config.heartbeat_interval = Duration::from_secs(20);
config.ping_timeout = Duration::from_secs(30);
```
**Rationale**: Detects dead connections before application logic fails; prevents hanging subscriptions.

### 9. **90-Second Crash Detection**

```rust
connection_start: Arc<RwLock<Option<Instant>>>,

// If connection crashes and reconnects within 90s, 
// restore subscriptions automatically
```
**Rationale**: Schwab broker-side behavior; SDK mirrors it for transparent failover.

### 10. **Security Layering (Defense-in-Depth)**

```
Layer 1: Memory      → SecretString (auto-zeroize on drop)
Layer 2: Storage     → ChaCha20Poly1305 AEAD (EncryptedFile) or Keychain
Layer 3: Transport   → HTTPS/WSS only (rustls, no OpenSSL)
Layer 4: Filesystem  → 0600 permissions (owner-only on Unix)
```

### 11. **PKCE Support (OAuth 2.1)**

```rust
if config.pkce_enabled {
    // S256 method:
    // verifier = 43-char random string
    // challenge = base64url(sha256(verifier))
}
```
**Rationale**: Protects against authorization code interception attacks.

### 12. **Token Refresh Background Task**

```rust
// Automatic token refresh before expiry
pub async fn start(&self) -> AuthResult<()> {
    // Spawn background task that:
    // 1. Calculates refresh time (expires_in - buffer)
    // 2. Waits until refresh time
    // 3. Calls Schwab refresh endpoint
    // 4. Saves new tokens securely
    // 5. Notifies on expiry warnings
}
```
**Rationale**: Transparent token management; application doesn't need to handle refresh.

### 13. **Subscription Batching by Field**

```rust
// Group subscriptions by service and field set
// Single request to Schwab for multiple symbols with same fields
// Reduces API call overhead
```
**Rationale**: Efficient use of Schwab API; fewer messages, same data.

---

## 6. External Integrations (APIs, Services, etc.)

### A. Charles Schwab APIs

#### REST Endpoints
- **Market Data APIs**
  - `/marketdata/v1/quotes` (GET) - Symbol quotes
  - `/marketdata/v1/chains` (GET) - Option chains
  - `/marketdata/v1/pricehistory` (GET) - OHLCV bars
  - `/marketdata/v1/movers/{index}` (GET) - Market movers
  - `/marketdata/v1/instruments` (GET) - Symbol search

- **Trading APIs**
  - `/trader/v1/accounts/{accountId}/orders` (GET/POST) - Order management
  - `/trader/v1/accounts/{accountId}/positions` (GET) - Holdings

- **Account APIs**
  - `/trader/v1/accounts/{accountId}` (GET) - Account details
  - `/trader/v1/accounts` (GET) - List accounts

#### OAuth2 Endpoints
- **Authorization**: `https://auth.schwabapi.com/v1/oauth/authorize`
- **Token Exchange**: `https://api.schwabapi.com/v1/oauth/token`
- **Token Refresh**: `https://api.schwabapi.com/v1/oauth/token` (with refresh_token grant)

#### WebSocket Streaming
- **URL**: `wss://streamer-api.schwabapi.com/streaming` (WSS = WebSocket Secure)
- **Services**: 13 streaming services
  - LevelOneEquities
  - LevelOneOptions
  - OptionChains
  - OptionExpires
  - NewsHeadline
  - NewsStory
  - ChartData
  - ChartHistory
  - IntraDay
  - Levelone
  - Account
  - Trades
  - Break

### B. External Crates and Services

#### Security Services
- **OS Keychains**
  - macOS: Keychain (via `keyring` crate)
  - Windows: Credential Manager (via `keyring` crate)
  - Linux: Secret Service API (GNOME Keyring/KWallet via `keyring` crate)

#### Cryptographic Services
- **Ring**: OS-level CSPRNG for random number generation
- **ChaCha20Poly1305**: Hardware-accelerated AEAD encryption (when available)

#### HTTP/WebSocket
- **Reqwest**: HTTP client with rustls TLS
- **Tokio-Tungstenite**: WebSocket client

### C. Configuration Sources

```rust
// 1. Environment variables
SCHWAB_APP_KEY
SCHWAB_APP_SECRET
SCHWAB_CALLBACK_URL
SCHWAB_TOKENS_FILE
SCHWAB_TOKEN_STORE (file|encrypted_file|keychain)
SCHWAB_PKCE_ENABLED

// 2. .env files (via dotenvy crate)
// 3. Code configuration (SchwabConfig)
```

### D. OAuth Callback Mechanisms

#### Callback Server (Feature: `callback-server`)
- **Framework**: Axum (Rust web framework)
- **Purpose**: Captures authorization code from redirect
- **URL**: `https://127.0.0.1:8080` (default) or `https://0.0.0.0:8080` (with `allow_external_callback`)
- **Use Cases**:
  - Local development
  - Cloudflared tunnels
  - ngrok tunnels

#### Manual Code Entry
```rust
// For headless servers, user enters code manually:
auth_manager.exchange_code(code_from_user).await?
```

---

## 7. Data Flow Diagrams

### OAuth2 Flow (Initial Authentication)

```
Application
    ↓
AuthManager::authorize()
    ↓
[Option 1: Callback Server]  [Option 2: Manual Code Entry]
    ↓                               ↓
Open https://auth.schwabapi.com → User logs in
    ↓                               ↓
Schwab redirects to callback URL ← User provides code
    ↓                               ↓
Callback server captures code ← [Manual input]
    ↓                               ↓
AuthManager::exchange_code(code)
    ↓
POST /v1/oauth/token (with PKCE verifier if enabled)
    ↓
Receive: access_token, refresh_token, expires_in
    ↓
Store tokens securely (Keychain|EncryptedFile|File)
    ↓
TokenSet saved with crypto signature
```

### Token Refresh Flow (Automatic)

```
SchwabClient initialized
    ↓
AuthManager::start() spawns background task
    ↓
[Wait until: (expires_at - refresh_buffer)]
    ↓
TokenSet::is_access_token_expired()?
    ↓
Call Schwab: POST /v1/oauth/token (refresh_token grant)
    ↓
Receive new tokens
    ↓
TokenSet updated in memory
    ↓
Secure storage updated
    ↓
HttpClient recreated (per README requirement)
    ↓
Callback fired: TokenNotification::SessionRecreated
    ↓
Repeat loop
```

### REST API Request Flow

```
Application calls: client.get_quotes(&["AAPL"])
    ↓
SchwabClient::request_with_query()
    ↓
[Rate Limiter] → rate_limiter.until_ready().await
    ↓
[Get Access Token] → auth_manager.get_access_token()
    ↓
[Build Request]
    ├─ URL: base_url + path
    ├─ Headers: Authorization: Bearer {token}
    ├─ Method: GET
    └─ Query params: [symbols=AAPL]
    ↓
[Retry Policy] → apply exponential backoff on failure
    ↓
HttpTransport::request<T>()
    ├─ reqwest sends HTTP request
    ├─ Response.json() deserializes to T
    └─ Error handling for non-2xx status
    ↓
[On 401 Unauthorized]
    ├─ If first attempt: auth_manager.refresh_token()
    ├─ Retry request with new token
    └─ If second 401: return Auth error
    ↓
Return Result<T>
```

### WebSocket Streaming Flow

```
Application calls: stream_client.connect()
    ↓
StreamClient::establish_connection()
    ├─ Get access token from auth_manager
    ├─ WebSocketTransport::connect(wss://streamer-api...)
    └─ Verify connection established
    ↓
StreamClient::subscribe(StreamService::LeveloneEquities, &["AAPL"])
    ↓
[Build subscription request]
    ├─ Command: SUBS
    ├─ Service: LEVELONE_EQUITIES
    ├─ Keys: AAPL
    └─ Fields: (configurable)
    ↓
[Batch subscriptions by field]
    ├─ Group by StreamService + field_set
    └─ Send single request for multiple symbols
    ↓
Send to WebSocket
    ↓
[Receive responses]
    ├─ StreamMessage::Response (ACK)
    ├─ StreamMessage::Snapshot (initial data)
    └─ StreamMessage::Data (updates)
    ↓
[Heartbeat thread]
    ├─ Every 20s: Send Ping frame
    ├─ Wait for Pong response
    └─ If no Pong in 30s: Reconnect
    ↓
[On disconnect]
    ├─ Start exponential backoff (2s, 4s, 8s... 128s)
    ├─ Reestablish WebSocket connection
    ├─ Restore subscriptions
    └─ Send any pending messages
    ↓
[Application receives]
    while let Some(msg) = receiver.recv().await {
        match msg {
            StreamMessage::Data(data) => process_data(),
            StreamMessage::Response(resp) => handle_ack(),
            StreamMessage::Notify(_) => handle_heartbeat(),
        }
    }
```

---

## 8. Code Statistics

### Lines of Code by Component

| Component | LOC | % |
|-----------|-----|---|
| schwab-rs | 5,244 | 81% |
| schwab-types | 1,241 | 19% |
| **Total** | **6,485** | **100%** |

### Breaking Down schwab-rs by Module

| Module | LOC | Purpose |
|--------|-----|---------|
| streaming/client.rs | 1,396 | WebSocket streaming |
| auth/mod.rs | 918 | OAuth2 authentication |
| client.rs | 596 | REST API client |
| auth/token_store.rs | 483 | Token storage backends |
| security.rs | 391 | Encryption & PKCE |
| config.rs | 365 | Configuration |
| error.rs | 256 | Error types |
| utils.rs | 224 | Utility functions |
| streaming/subscription.rs | 147 | Subscription mgmt |
| retry.rs | 133 | Retry logic |
| transport/http.rs | 74 | HTTP transport |
| transport/websocket.rs | 82 | WebSocket transport |
| lib.rs | 52 | Module exports |
| endpoints/* | 101 | Thin wrappers |
| streaming/mod.rs | 6 | Module exports |
| transport/mod.rs | 4 | Module exports |
| **Total** | **5,244** | |

### Types by Category

| Category | LOC | Types | Examples |
|----------|-----|-------|----------|
| Market Data | 470 | Quote, Chain, Greeks, History | QuoteItem, OptionChain, PriceHistoryItem |
| Trading | 242 | Order, Position, Transaction | Order, OrderLeg, Position |
| Streaming | 308 | StreamRequest, StreamMessage | StreamMessage::Data, StreamMessage::Response |
| Accounts | 147 | Account, SecuritiesAccount | Account, BalanceFields |
| Common | 62 | Enums, Shared types | AssetType, OrderInstruction |
| **Total** | **1,241** | | |

---

## 9. Key Design Trade-offs

| Decision | Pros | Cons | Rationale |
|----------|------|------|-----------|
| **Arc<RwLock<T>> for client state** | Shareable, cloneable | RwLock overhead | Allows concurrent access without channels |
| **Thin endpoint wrappers** | Clean API organization | Extra indirection | Easier to maintain, centralized logic |
| **Unbounded channels by default** | Backward compatible, fast | Risk of memory growth | Can use Bounded(N) when needed |
| **Keychain as default (macOS)** | Best security | Platform-specific | Leverages OS capabilities |
| **EncryptedFile as default (Linux/Windows)** | Portable, encrypted | Key management complexity | Balances security and usability |
| **ChaCha20 over AES** | Faster, memory-safe | Less adoption | NCC Group audited, immune to cache timing |
| **PKCE opt-in (not default)** | Backward compatible | OAuth 2.1 not enforced | Users must enable for strictest security |
| **90-second crash detection** | Automatic recovery | Matches Schwab behavior | Transparent failover |
| **Exponential backoff retry** | Respectful rate limiting | Longer recovery time | Prevents thundering herd |

---

## 10. Security Architecture Summary

### Threat Model

```
┌─────────────────────────────────────────────────────────┐
│  Threat                   │ Mitigation                  │
├─────────────────────────────────────────────────────────┤
│ Token exposed in memory   │ SecretString + Zeroize      │
│ Token written to disk     │ ChaCha20Poly1305 AEAD       │
│ Token tampered on disk    │ Poly1305 authentication tag │
│ Unencrypted credentials   │ Keychain (OS-native)        │
│ Weak file perms (Unix)    │ Enforced 0600               │
│ Auth code interception    │ PKCE (S256 method)          │
│ TLS MITM attacks          │ rustls, no OpenSSL vulns    │
│ Token exposed in logs     │ SecretString hides on Debug │
│ Timing attacks            │ ChaCha20 immune             │
└─────────────────────────────────────────────────────────┘
```

### Security Grade: A+ (per README)

- No high/medium severity vulnerabilities
- Best-in-class cryptography
- Memory-safe secret handling
- OS-native credential storage
- RFC 7636 PKCE support
- Thread-safe token management

---

## 11. Future Extensibility Points

### Designed-in Extension Points

1. **Additional Token Storage Backends**
   - Implement TokenStore trait
   - Add to TokenStoreKind enum

2. **Custom Retry Strategies**
   - Extend RetryPolicy
   - Customize backoff curves

3. **Rate Limit Strategies**
   - Alternative governor configurations
   - Custom per-endpoint limits

4. **Transport Adapters**
   - Custom HTTP client
   - Custom WebSocket implementation

5. **Streaming Service Expansion**
   - Add new StreamService variants
   - Define service-specific data types

---

## 12. Testing Architecture

### Test Coverage

```
├─ Unit tests (in-file with #[cfg(test)])
│  ├─ auth module: Token refresh, expiry handling
│  ├─ retry module: Backoff calculation, retry decisions
│  └─ types: Serialization/deserialization
│
├─ Integration tests (examples/)
│  ├─ oauth_flow: Full OAuth2 dance
│  ├─ streaming_demo: All 13 services
│  ├─ comprehensive: Combined REST + streaming
│  └─ auth_test: Token scenarios
│
└─ Mocking support
   ├─ wiremock: Mock HTTP responses
   ├─ mockito: HTTP mocking
   └─ mockall: Mock traits
```

### Test Utilities

```rust
#[cfg(test)]
mod tests {
    use wiremock::{Mock, ResponseTemplate};
    use mockall::*;
    use pretty_assertions::assert_eq!;
}
```

---

## Summary Table: Quick Reference

| Aspect | Technology | Details |
|--------|-----------|---------|
| **Language** | Rust 2024 edition | Type-safe, memory-safe |
| **Async Runtime** | Tokio | Full-featured async ecosystem |
| **HTTP** | Reqwest + Rustls | Memory-safe TLS, no OpenSSL |
| **WebSocket** | Tokio-Tungstenite | WebSocket with TLS support |
| **Serialization** | Serde + serde_json | Zero-copy where possible |
| **Encryption** | ChaCha20Poly1305 | NCC Group audited AEAD |
| **Key Storage** | Keyring (3 backends) | OS-native (macOS/Windows/Linux) |
| **CSPRNG** | Ring | Google/Mozilla standard |
| **Rate Limiting** | Governor | Token bucket algorithm |
| **Retry Logic** | Backoff + Exponential | Jitter + configurable strategy |
| **Error Handling** | Thiserror | Type-safe error types |
| **Logging** | Log facade | User chooses implementation |
| **Total LOC** | 6,485 | 2 crates, 30 files |

