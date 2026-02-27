# Rustyschwab - Quick Start for Developers

## One-Minute Overview

**rustyschwab** is a production-grade Rust SDK for Charles Schwab trading APIs with:
- OAuth 2.0 authentication with PKCE support
- REST API access (quotes, orders, accounts)
- WebSocket streaming (real-time data)
- World-class security (A+ grade, audited)
- Enterprise resilience (retries, rate limiting)

**Stats:** 6,814 LOC Rust | 42 unit tests + 9 async integration tests | MIT/Apache-2.0

---

## Project Structure at a Glance

```
rustyschwab/
├── crates/
│   ├── schwab-rs/        # Main SDK (REST + WebSocket)
│   └── schwab-types/     # Type definitions
├── examples/             # 8 working examples
├── tests/                # Comprehensive test suite
└── .github/workflows/    # CI/CD pipeline
```

---

## Essential Commands

### Setup & Build
```bash
# Install Rust (if needed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Clone and build
git clone <repo>
cd rustyschwab
cargo build --all-features
```

### Testing
```bash
# Run all tests (42 unit + 9 async)
cargo test --workspace --all-targets --locked

# Run with logging
RUST_LOG=debug cargo test

# Specific test file
cargo test --test oauth_integration_test
```

### Code Quality (Pre-commit)
```bash
# Format
cargo fmt --all

# Check format
cargo fmt --all -- --check

# Lint
cargo clippy --all-targets -- -D warnings
```

### Running Examples
```bash
# OAuth flow example
cargo run --example oauth_flow

# Streaming example (requires credentials)
SCHWAB_APP_KEY=your_key SCHWAB_APP_SECRET=your_secret \
  cargo run --example comprehensive --features full
```

### Documentation
```bash
# Generate and open API docs
cargo doc --no-deps --open

# Read core docs
cat README.md
cat CONTRIBUTING.md
ls docs/
```

---

## Key Files to Know

| File | Purpose | Lines |
|------|---------|-------|
| `crates/schwab-rs/src/client.rs` | Main API client | 596 |
| `crates/schwab-rs/src/auth/mod.rs` | OAuth 2.0 | - |
| `crates/schwab-rs/src/config.rs` | Configuration | 365 |
| `crates/schwab-rs/src/streaming/client.rs` | WebSocket streaming | - |
| `crates/schwab-types/src/` | Type definitions | 1,240 |
| `crates/schwab-rs/tests/` | Test suite | - |
| `.github/workflows/ci.yml` | CI pipeline | - |
| `docs/INDEX.md` | Documentation entry point | - |

---

## Development Workflow

### 1. Create Feature Branch
```bash
git checkout -b feature/your-feature
```

### 2. Make Changes
```bash
# Edit code, add tests
vim crates/schwab-rs/src/client.rs
```

### 3. Test Locally
```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --workspace --all-targets --locked
```

### 4. Commit with Conventional Format
```bash
git commit -m "feat(auth): add PKCE support

Implement RFC 7636 for authorization code protection.
Closes #42"
```

### 5. Push and Create PR
```bash
git push origin feature/your-feature
# Open PR on GitHub
```

---

## Code Locations by Feature

### Adding an API Endpoint
1. **Types:** `crates/schwab-types/src/market_data.rs`
2. **Client:** `crates/schwab-rs/src/client.rs` (add method)
3. **Tests:** `crates/schwab-rs/tests/` (add test file)
4. **Docs:** Update `README.md` with example

### Fixing a Bug
1. **Locate bug:** Use `grep` or `rg` to search code
2. **Write test:** `crates/schwab-rs/tests/` (test the fix)
3. **Fix code:** Make the minimal change
4. **Verify:** All tests pass

### Adding Tests
```bash
# Create test file
vim crates/schwab-rs/tests/my_test.rs

# Run it
cargo test --test my_test
```

---

## Important Files

### Configuration
- **Workspace:** `Cargo.toml` - Dependencies and workspace config
- **Crates:** `crates/*/Cargo.toml` - Per-crate config
- **CI:** `.github/workflows/ci.yml` - GitHub Actions pipeline
- **.env:** `.env` - Local environment variables (not committed)

### Documentation
- **Main:** `README.md` - Project overview
- **Contributing:** `CONTRIBUTING.md` - Development guidelines
- **Index:** `docs/INDEX.md` - All technical guides
- **Architecture:** `docs/ARCHITECTURE_OVERVIEW.md` - Technical design
- **Security:** `docs/SECURITY_AUDIT_SUMMARY.md` - Security analysis
- **Developer Docs:** `docs/DEVELOPER_DOCUMENTATION.md` - Detailed reference

### Source Code
- **Main SDK:** `crates/schwab-rs/src/` - Core implementation
- **Types:** `crates/schwab-types/src/` - Data types
- **Examples:** `examples/` - Working code examples
- **Tests:** `crates/schwab-rs/tests/` - Test suite

---

## Environment Variables

```bash
# Required for examples
export SCHWAB_APP_KEY="your_32_char_key"
export SCHWAB_APP_SECRET="your_16_char_secret"
export SCHWAB_CALLBACK_URL="https://127.0.0.1:8080"

# Optional
export SCHWAB_TOKEN_STORE="encrypted_file"  # or: file, keychain
export SCHWAB_PKCE_ENABLED="true"
export RUST_LOG="debug"  # For debugging
```

---

## Common Questions

### Q: How do I run tests?
```bash
cargo test --workspace --all-targets --locked
```

### Q: How do I format my code?
```bash
cargo fmt --all
```

### Q: How do I check for lint warnings?
```bash
cargo clippy --all-targets -- -D warnings
```

### Q: Where are the tests?
```bash
crates/schwab-rs/tests/        # Integration tests
crates/schwab-types/src/       # Unit tests in lib.rs
```

### Q: How do I generate docs?
```bash
cargo doc --no-deps --open
```

### Q: What are the test frameworks?
- **Async tests:** `tokio::test` with `wiremock` for HTTP mocking
- **Unit tests:** Standard Rust `#[test]` macro
- **Benchmarks:** `criterion` crate

### Q: What testing libraries are used?
- `wiremock` - HTTP mock server
- `mockito` - Mocking framework
- `criterion` - Benchmarking
- `tokio-test` - Async test utilities

---

## CI/CD Pipeline

Runs automatically on push/PR to main:

```
1. Checkout code
2. Install Rust
3. Check formatting (cargo fmt)
4. Run clippy (cargo clippy)
5. Build workspace
6. Run tests
```

**Status:** Must pass all checks before merging to main.

---

## Security Quick Notes

- Token storage: Encrypted by default (ChaCha20Poly1305 AEAD)
- PKCE enabled by default (authorization code protection)
- Memory-safe secrets (SecretString + zeroize)
- No credentials in logs or git
- TLS/HTTPS only for OAuth callbacks
- File permissions enforced (0600 Unix)

---

## Feature Flags

```bash
# Default (memory-safe TLS)
cargo build

# With callback server (for local OAuth)
cargo build --features callback-server

# All features
cargo build --features full

# Native TLS (system OpenSSL)
cargo build --features native-tls
```

---

## Release Process

1. Update version in `Cargo.toml` files
2. Update `CHANGELOG.md`
3. Create git tag: `git tag v0.1.0`
4. Push: `git push && git push --tags`
5. Publish: `cargo publish`

---

## Architecture Layers

```
SchwabClient (REST APIs)
    ↓
HttpTransport (request/response)
    ↓
Retry + RateLimit (resilience)
    ↓
AuthManager (OAuth tokens)
    ↓
Encrypted Token Storage
```

And separately:

```
StreamClient (WebSocket)
    ↓
WebSocketTransport
    ↓
Subscriptions + Heartbeat
    ↓
AuthManager (token refresh)
```

---

## Module Breakdown

| Module | Purpose |
|--------|---------|
| `auth/` | OAuth 2.0, token management, PKCE |
| `client/` | Main REST API interface |
| `config/` | Configuration management |
| `endpoints/` | Individual API endpoints |
| `streaming/` | WebSocket streaming client |
| `transport/` | HTTP and WebSocket layers |
| `security/` | Encryption, key derivation |
| `error/` | Error types and handling |
| `retry/` | Exponential backoff |

---

## Next Steps

1. **Read:** `README.md` for project overview
2. **Explore:** `docs/INDEX.md` for specialized guides
3. **Understand:** `docs/ARCHITECTURE_OVERVIEW.md` for design
4. **Learn:** `CONTRIBUTING.md` for contribution guidelines
5. **Code:** Look at `examples/` for usage patterns
6. **Test:** Run `cargo test` to verify setup

---

## Getting Help

- GitHub Issues: Report bugs and request features
- Discussions: Ask questions
- Documentation: Check `README.md` and docs/
- Examples: See `examples/` directory
- Tests: Look at `tests/` for usage patterns
- Code Comments: Check doc comments with `cargo doc`

---

**For complete developer documentation, see:** `DEVELOPER_DOCUMENTATION.md`

Last Updated: November 2, 2025
