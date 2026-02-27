# Rustyschwab Documentation Index

**Last Updated:** February 26, 2026

## 📖 User Guides (Start Here)

### [Setup Guide](SETUP_GUIDE.md)
Step-by-step instructions for Schwab portal registration, environment setup, and first-time authentication.

### [Placing Orders](ORDERS.md)
Concrete examples for trading:
- Market and Limit orders
- Stop orders and GTC duration
- Option contract formatting
- Complex strategies (Vertical Spreads)
- Using the `preview_order` endpoint

### [Real-time Streaming](STREAMING.md)
Detailed guide for the WebSocket engine:
- Configuration and backpressure
- Reconnection and persistence
- Field-level customization
- Handling data and heartbeats

### [Troubleshooting](TROUBLESHOOTING.md)
Solutions for 401 Unauthorized, symbol formatting errors, SSL issues, and common async Rust pitfalls.

### [App Callback URLs](APP_CALLBACK.md)
Detailed technical requirements for OAuth redirect URIs.

---

## 🛠 Developer & Contributor Reference

### [README](../README.md)
The primary entry point: feature list, installation, and security highlights.

### [Developer Documentation](DEVELOPER_DOCUMENTATION.md)
Deep dive for contributors:
- Architecture diagrams
- LOC and codebase statistics
- Build system and dependency details
- Test suite architecture (110+ tests)

### [Quick Start Developer](QUICK_START.md)
Immediate productivity guide for making code changes, adding endpoints, and running tests.

### [Architecture Overview](ARCHITECTURE_OVERVIEW.md)
Comprehensive technical design, component interactions, and security layer details.

### [Contributing](../CONTRIBUTING.md)
Contribution guidelines, code style, and release process.

### [Security Audit Summary](SECURITY_AUDIT_SUMMARY.md)
Results from the latest independent security audit (Grade: A+).

---

## Navigation Tips

**"I want to trade options..."**
See [ORDERS.md](ORDERS.md) for symbol formatting and code snippets.

**"I need to debug a 401 error..."**
See [TROUBLESHOOTING.md](TROUBLESHOOTING.md) section on Authentication.

**"I want to add a new API endpoint..."**
See [QUICK_START.md](QUICK_START.md) section on Feature Addition.

**"I need help with callback URLs..."**
See [SETUP_GUIDE.md](SETUP_GUIDE.md) section on Cloudflared or [APP_CALLBACK.md](APP_CALLBACK.md).

**"Where are the examples?"**
Check the `examples/` directory:
- `oauth-flow`: Basic authentication
- `auth_test`: CLI tool for token lifecycle
- `comprehensive`: Full API & Streaming demo
- `streaming-examples`: Specialized WebSocket demos (quotes, processing)
