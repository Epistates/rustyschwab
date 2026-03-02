# Schwab Rust SDK

A Rust SDK for Charles Schwab's Trading and Market Data APIs, providing OAuth2 authentication, REST API access, and WebSocket streaming capabilities.

> [!WARNING]
> **RISK WARNING**: This SDK is for advanced users and is provided "as is" without warranty of any kind. Automated trading involves significant risk of financial loss. 
> **Always test your logic with a test account or "Paper Money" environment before using real capital.** 
> Use of this SDK is entirely at your own risk.

## Documentation

- **[Full Documentation Index](docs/INDEX.md)**: Main entry point for all guides.
- **[Setup Guide](docs/SETUP_GUIDE.md)**: Detailed instructions for getting started.
- **[Placing Orders](docs/ORDERS.md)**: Comprehensive guide for trading equities and options.
- **[Real-time Streaming](docs/STREAMING.md)**: Deep dive into the WebSocket client and services.
- **[Troubleshooting](docs/TROUBLESHOOTING.md)**: Common issues and solutions.
- **[Developer Documentation](docs/DEVELOPER_DOCUMENTATION.md)**: For contributors and architecture review.

## Features

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
  - **OWASP compliant**: Best-in-class encryption and key derivation (PBKDF2 600k iterations)

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
  - Rate limiting (120 req/s with burst of 20)
  - Exponential backoff retry logic
  - 90-second crash detection for streaming
  - **WebSocket Ping/Pong heartbeat monitoring** (v0.2.0)
  - **Timeout detection with automatic reconnection** (v0.2.0)
  - Field-based subscription batching (scoped)
  - Token expiry notifications
  - Comprehensive error handling
  - Type-safe bindings for commonly used endpoints (OpenAPI coverage in progress)

- **Streaming Advanced Features** (v0.2.0)
  - **Bounded/Unbounded channels** for backpressure control
  - Automatic Ping/Pong heartbeat (20s interval, 30s timeout)
  - Subscription persistence across reconnections
  - Custom field selection per service
  - All 13 streaming services fully implemented

## Security

This SDK implements **world-class security** with multiple layers of protection, significantly exceeding *typical* OAuth 2.0 implementations.

### Security Features Comparison

| Feature | rustyschwab | Python Reference | Advantage |
|---------|------------|------------------|-----------|
| **Encryption at Rest** | ✅ ChaCha20Poly1305 AEAD | ❌ Plaintext JSON | **100% improvement** |
| **Tamper Detection** | ✅ AEAD authentication tags | ❌ None | **Data integrity** |
| **Memory Protection** | ✅ SecretString + Zeroize | ❌ Plain strings | **Memory safety** |
| **Keychain Storage** | ✅ macOS/Windows/Linux | ❌ Not implemented | **Best-in-class** |
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

Protect against authorization code interception attacks:

```rust,ignore
use schwab_rs::auth::OAuthConfig;

let oauth = OAuthConfig {
    app_key: "YOUR_APP_KEY".into(),
    app_secret: "YOUR_SECRET".into(),
    callback_url: "https://127.0.0.1:8080".into(),

    pkce_enabled: true,  // Enabled by default (OAuth 2.1 compliant)
    ..Default::default()
};
```

**PKCE Security Benefits:**
- **Code verifier:** 256-bit random value (OS CSPRNG)
- **Code challenge:** SHA-256 hash with base64url encoding
- **Method:** S256 (cryptographically secure)
- **Protection:** Prevents authorization code interception/replay attacks

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
# - AWS KMS Secrets Manager
# - HashiCorp Vault
# - Azure Key Vault
```

**4. Production Deployment Checklist**

- [ ] Use `EncryptedFile` or `Keychain` storage backend
- [ ] Ensure PKCE is enabled (`pkce_enabled: true`, default)
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

**PKCE Code Verifier/Challenge (RFC 7636)**

```text
// Code verifier generation
1. Generate 32 bytes random (256 bits) from OS CSPRNG
2. Base64URL encode without padding
3. Result: 43-character verifier string

// Code challenge generation
1. SHA-256 hash of verifier
2. Base64URL encode without padding
3. Method: S256 (required by OAuth 2.1)
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
// Get quotes
let quotes = client.get_quotes(&["AAPL", "MSFT"]).await?;

// Advanced quotes: only request specific fields and enable indicative pricing
let adv_quotes = client.get_quotes_with_options(
    &["AAPL"], 
    Some("quote,reference"), 
    Some(true)
).await?;

// Get option chain
let chain = client.get_option_chain("SPY").await?;

// Preview an order (validate without executing)
let preview = client.preview_order("account_hash", &order).await?;

// Place an order
let response = client.place_order("account_hash", &order).await?;
```

### 4. Stream Market Data

```rust,ignore
use schwab_rs::streaming::{StreamClient, StreamMessage};
use schwab_rs::types::streaming::StreamService;

// Build streaming client
let stream_client = StreamClient::builder()
    .config(stream_config)
    .auth_manager(auth_manager.clone())
    .customer_id(customer_id)
    .build()?;

// Connect and (attempt to) subscribe
stream_client.connect().await?;
stream_client.subscribe(StreamService::LeveloneEquities, vec!["AAPL".into(), "MSFT".into()]).await?;

// Handle messages
if let Some(mut receiver) = stream_client.get_receiver() {
    while let Some(msg) = receiver.recv().await {
        match msg {
            StreamMessage::Data(data) => println!("Market data: {:?}", data),
            StreamMessage::Response(resp) => println!("Response: {:?}", resp),
            StreamMessage::Notify(hb) => println!("Heartbeat: {}", hb.heartbeat),
        }
    }
}
```

Note: Streaming authentication, reconnection, and subscription handling are fully implemented.

## OAuth Callback URL Options

For development, you need an HTTPS callback URL. Options:

### 1. Cloudflared Tunnel (Recommended)
```bash
# Install cloudflared
brew install cloudflared  # macOS

# Create tunnel to local port 8080
cloudflared tunnel --url http://localhost:8080

# Use the generated HTTPS URL as your callback_url
```

### 2. ngrok
```bash
ngrok http 8080
# Use the HTTPS URL provided
```

### 3. Self-Signed Certificate
```bash
# Generate certificate
openssl req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem -days 365 -nodes

# Configure in your app to use HTTPS
```

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

```bash
export SCHWAB_APP_KEY="your_32_character_app_key"
export SCHWAB_APP_SECRET="your_16_char_secret"
export SCHWAB_CALLBACK_URL="https://your-callback-url"
export SCHWAB_TOKENS_FILE="schwab_tokens.json"
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
            requests_per_second: 120,
            burst_size: 20,
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
2. **Key Lengths**: App key must be 32 chars, secret must be 16 chars
3. **Rate Limits**: SDK defaults to 120 requests/second with burst of 20
4. **Token Expiration**: Access tokens expire in ~30 minutes; refresh tokens in ~7 days
5. **Session Management**: HTTP client is recreated after token refresh
6. **Token Storage**: Defaults to Keychain (macOS) or EncryptedFile (other platforms)
7. **Token Notifications**: Optional callbacks for expiry warnings and session events

Known limitations and notes:
- Endpoints under `crates/schwab-rs/src/endpoints/` now delegate to `SchwabClient` methods (thin wrappers)
- Optional PKCE can be enabled via `OAuthConfig.pkce_enabled`
- `SchwabClient` uses `transport::http::HttpTransport` for REST

## Streaming: Advanced Features (v0.2.0)

### Bounded Channels for Backpressure Control

Control memory usage with bounded channels:

```rust,ignore
use schwab_rs::{StreamConfig, ChannelKind};

let mut config = StreamConfig::default();

// Unbounded (default - no backpressure)
config.channel_kind = ChannelKind::Unbounded;

// Bounded (recommended for production - prevents memory growth)
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
use schwab_rs::types::streaming::StreamService;

// Request lean field set (Symbol, Bid, Ask, Last, Volume)
stream.set_service_fields(
    StreamService::LeveloneEquities,
    "0,1,2,3,8".to_string()
);

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

See `examples/` directory for production-ready patterns:
- `streaming_demo.rs` - All 13 services demonstrated
- `streaming_data_processing.rs` - Async processing pattern
- `streaming_quotes.rs` - Real-time quotes dashboard

```bash
cargo run --example streaming_quotes --features callback-server
```

## Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

This project is dual-licensed under the MIT License and Apache License 2.0.

## Disclaimer

This SDK is not affiliated with, endorsed by, or officially connected with Charles Schwab & Co., Inc. Use at your own risk. Always test thoroughly with sandbox data before using in production.

## Support

For issues and questions:
- GitHub Issues: [Project Issues](../../issues)
- Documentation: [docs.rs/schwab-rs](https://docs.rs/schwab-rs)

## Development

### OAuth setup

Set required environment variables (or place in `.env`):

```bash
export SCHWAB_APP_KEY=...
export SCHWAB_APP_SECRET=...
export SCHWAB_CALLBACK_URL=https://127.0.0.1:8080
```

Optional:

```bash
export SCHWAB_PKCE_ENABLED=false
export SCHWAB_TOKEN_STORE=file          # Options: file, encrypted_file, keychain
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
