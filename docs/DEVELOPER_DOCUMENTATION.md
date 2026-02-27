# Rustyschwab Codebase Analysis & Developer Documentation

**Generated:** 2026-02-26  
**Project:** Schwab Rust SDK  
**Version:** 0.1.0  
**Repository:** rustyschwab  

---

## Table of Contents

1. [Executive Summary](#executive-summary)
2. [Project Type & Purpose](#project-type--purpose)
3. [Overall Architecture](#overall-architecture)
4. [Project Structure](#project-structure)
5. [Build System & Dependencies](#build-system--dependencies)
6. [Testing Framework & Setup](#testing-framework--setup)
7. [Key Files & Directories](#key-files--directories)
8. [Build/Test/Lint Commands](#buildtestlint-commands)
9. [GitHub Workflows](#github-workflows)
10. [Development Guidelines](#development-guidelines)

---

## Executive Summary

**rustyschwab** is a **production-grade Rust SDK** for Charles Schwab's Trading and Market Data APIs. It provides comprehensive OAuth2 authentication, REST API access, WebSocket streaming, and world-class security features.

### Key Statistics
- **Total Code:** ~7,500 lines of Rust (excluding tests)
- **Language:** Rust 2024 edition
- **Architecture:** Workspace with 2 main crates + examples
- **Test Coverage:** 110+ automated tests (unit, integration, and doc-tests)
- **Security Grade:** A+ (Production-ready, independently audited)
- **Async Runtime:** Tokio-based
- **License:** MIT OR Apache-2.0

---

## Project Type & Purpose

### Type
- **SDK/Library** - A Rust native binding for financial trading APIs
- **Category:** API bindings, Finance, Trading Tools

### Purpose
Provide developers with a **type-safe, secure, and performant** Rust interface to:

1. **OAuth 2.0 & OIDC Authentication**
   - Authorization code flow with PKCE support (RFC 7636)
   - OpenID Connect (OIDC) `id_token` support
   - Automatic token refresh with configurable buffer
   - 7-day refresh token expiration handling
   - Three token storage backends: File, EncryptedFile (ChaCha20Poly1305), Keychain (OS-native)

2. **REST API Access**
   - Market data endpoints (quotes, price history, options chains)
   - **Advanced Market Data**: Indicative quotes and field-level filtering
   - Trading endpoints (orders, positions, accounts)
   - **Order Lifecycle**: Place, Replace, Cancel, and **Preview Order**
   - Account management and transaction history

3. **WebSocket Streaming (v0.2.0 Engine)**
   - Real-time market data across all 13 Schwab streaming services
   - **Automatic Reconnection**: Intelligent backoff and recovery
   - **Subscription Persistence**: Automatic re-subscription of active services on reconnect
   - Heartbeat monitoring with 90-second crash detection
   - Bounded/unbounded channel backpressure control

4. **Enterprise-Grade Resilience**
   - Rate limiting (120 req/s with burst of 20) via `governor`
   - Exponential backoff retry logic with jitter
   - Circuit breaker for upstream failure protection
   - Comprehensive error handling and structured telemetry

### Target Users
- Rust developers building high-frequency or retail trading applications
- Quantitative traders and algorithmic systems
- Financial data processing pipelines
- Enterprise-grade trading platforms requiring SOTA security

---

## Overall Architecture

### High-Level Design

```
┌──────────────────────────────────────────────────────┐
│            User Applications/Examples                │
└──────────────────────────────────────────────────────┘
                         │
┌──────────────────────────────────────────────────────┐
│          schwab-rs: Main SDK Implementation          │
├──────────────────────────────────────────────────────┤
│                                                      │
│  ┌──────────────┐              ┌──────────────────┐ │
│  │ SchwabClient │              │ StreamClient     │ │
│  │ (REST APIs)  │              │ (WebSocket)      │ │
│  └──────────────┘              └──────────────────┘ │
│         │                              │             │
│  ┌──────────────┐              ┌──────────────────┐ │
│  │ AuthManager  │              │ Subscription     │ │
│  │ (OAuth/OIDC) │              │ Manager          │ │
│  └──────────────┘              └──────────────────┘ │
│         │                              │             │
│  ┌──────────────┐              ┌──────────────────┐ │
│  │ HttpTransport│              │ WebSocketTrans.  │ │
│  │ (REST/HTTP)  │              │ (WS over TLS)    │ │
│  └──────────────┘              └──────────────────┘ │
│         │                              │             │
│  ┌──────────────┐              ┌──────────────────┐ │
│  │ Retry+RateL. │              │ Heartbeat+Reconn.│ │
│  │ (Resilience) │              │ (Reliability)    │ │
│  └──────────────┘              └──────────────────┘ │
│                                                      │
└──────────────────────────────────────────────────────┘
                         │
┌──────────────────────────────────────────────────────┐
│       schwab-types: Type Definitions & Models       │
├──────────────────────────────────────────────────────┤
│  Market Data │ Trading │ Accounts │ Streaming │ Common│
└──────────────────────────────────────────────────────┘
                         │
┌──────────────────────────────────────────────────────┐
│    Security Layer: Crypto & Authentication          │
├──────────────────────────────────────────────────────┤
│ ChaCha20Poly1305 AEAD │ Keychain │ PKCE │ SecretStr │
└──────────────────────────────────────────────────────┘
```

---

## Project Structure

```
rustyschwab/
├── Cargo.toml                    # Workspace root configuration
├── README.md                     # Main documentation
├── CONTRIBUTING.md               # Contribution guidelines
├── CHANGELOG.md                  # Version history
├── LICENSE                       # MIT/Apache 2.0
│
├── docs/                         # User guides and technical docs
│   ├── INDEX.md                  # Main documentation entry point
│   ├── SETUP_GUIDE.md            # Onboarding guide
│   ├── ORDERS.md                 # Trading documentation
│   ├── STREAMING.md              # WebSocket documentation
│   └── DEVELOPER_DOCUMENTATION.md # This file
│
├── crates/
│   ├── schwab-rs/                # Main SDK crate
│   │   ├── src/
│   │   │   ├── auth/             # OAuth 2.0 & OIDC implementation
│   │   │   ├── endpoints/        # REST API implementations
│   │   │   ├── streaming/        # WebSocket streaming engine
│   │   │   ├── transport/        # Low-level HTTP/WS transport
│   │   │   ├── circuit_breaker.rs # Resilience patterns
│   │   │   ├── retry.rs          # Backoff logic
│   │   │   └── security.rs       # Encryption & memory safety
│   │
│   └── schwab-types/             # Shared data models
│       └── src/                  # Accounts, Trading, MarketData, Streaming
│
├── examples/                     # Working usage examples
│   ├── auth_test/                # CLI for token testing
│   ├── comprehensive/            # End-to-end SDK usage
│   ├── streaming_quotes.rs       # Real-time dashboard
│   └── [Various specialized examples]
```

---

## Build System & Dependencies

### Cargo Workspace Configuration

**File:** `Cargo.toml`

```toml
[workspace]
resolver = "2"
edition = "2024" # Q1 2026 SOTA Standard
```

### Key Dependencies (Q1 2026 Stack)

#### Async & Networking
- **tokio** (1.49) - High-performance async runtime
- **reqwest** (0.13) - HTTP client with `rustls`
- **tokio-tungstenite** (0.26) - Secure WebSocket protocol
- **hyper** (1.6) - Low-level HTTP/2 support

#### Security & Cryptography
- **ring** (0.17) - Audited cryptographic primitives
- **chacha20poly1305** (0.10) - AEAD encryption for token storage
- **keyring** (3.6) - OS-native secure credential storage
- **secrecy** (0.10) - Zeroization and secret handling
- **zeroize** (1.8) - Guaranteed memory clearing

#### Resilience
- **governor** (0.7) - GCRA-based rate limiting
- **tower** (0.5) - Service middleware and abstraction

---

## Testing Framework & Setup

### Test Suite (110+ Tests)

| Category | Count | Command |
|----------|-------|---------|
| **Unit Tests** | 66 | `cargo test -p schwab-rs --lib` |
| **Integration** | 18 | `cargo test --test oauth_integration_test` |
| **Config/Retry**| 16 | `cargo test --test config_unit_test --test retry_unit_test` |
| **Doc-tests**   | 10 | `cargo test --doc` |

---

## Release Process (v0.1.0)

1. **Version Alignment**: All crates synced to `0.1.0`.
2. **Metadata Verification**: Readme, License, and Repository links verified.
3. **Clean Build**: All targets pass `cargo check` on 2024 edition.
4. **Deterministic Tests**: No flaky tests (Mutex-sequenced env tests).
5. **Security Check**: Dependencies audited for CVEs via `cargo audit`.

---

**Last Updated:** February 26, 2026  
**Status:** RELEASE READY  
