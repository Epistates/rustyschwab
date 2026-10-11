# Documentation index

## Using the SDK

- [Setup guide](SETUP_GUIDE.md) — Schwab portal registration, environment, first authentication.
- [App callback URLs](APP_CALLBACK.md) — redirect URI requirements and local HTTPS.
- [Placing orders](ORDERS.md) — constructors for single-leg, option, multi-leg and composite orders; the JSON `OrderSpec`.
- [Real-time streaming](STREAMING.md) — services, field selection, backpressure and reconnect semantics.
- [Python bindings](PYTHON.md) — `pip install rustyschwab`: authorization, quotes, orders, streaming, paper backtests and error handling from asyncio.
- [Troubleshooting](TROUBLESHOOTING.md) — 401s, symbol formatting, TLS, async pitfalls.

## Running it in production

- [Operations](OPERATIONS.md) — deployment boundary, principal policy JSON, approvals, risk policy, recovery, health and metrics.
- [Shared authority and broker gateway](SHARED_AUTHORITY.md) — `schwab-authority`, `RemoteBackend`, `schwab-remote`, cross-host workers.
- [MCP and LLM integration](MCP_LLM_INTEGRATION.md) — tool behavior hints, transport choice, what an LLM client may and may not do.
- [Paper trading](PAPER_TRADING.md) and [paper limitations](PAPER_LIMITATIONS.md) — the deterministic simulator and what it does not model.
- [Support and compatibility policy](SUPPORT.md)

## Contributing

- [Architecture overview](ARCHITECTURE_OVERVIEW.md) — crates, request pipeline, durable state, streaming, MCP authorization boundary, where to look.
- [Contributing](../CONTRIBUTING.md) — workflow, checks, style.
- [Changelog](../CHANGELOG.md)

## Enterprise readiness record

- [Enterprise audit](ENTERPRISE_AUDIT.md) (2026-09-09, historical) → [remediation](ENTERPRISE_REMEDIATION.md) (historical) → [implementation status](ENTERPRISE_ROADMAP.md) (current).
- `docs/audit/` — probe sources and qualification evidence; git-ignored, produced locally by `tools/` and published as `qualification.yml` artifacts.

## Quick answers

- **Trade options** — [ORDERS.md](ORDERS.md#options).
- **Debug a 401** — [TROUBLESHOOTING.md](TROUBLESHOOTING.md).
- **Add an endpoint, tool or stream service** — the "Where to look" table in [ARCHITECTURE_OVERVIEW.md](ARCHITECTURE_OVERVIEW.md#where-to-look).
- **Callback URL problems** — [SETUP_GUIDE.md](SETUP_GUIDE.md) or [APP_CALLBACK.md](APP_CALLBACK.md).
- **Examples** — workspace packages under `examples/`, run with `cargo run -p <package>`: `oauth-flow`, `auth_test`, `comprehensive`, `streaming-examples` (bins `streaming_demo`, `streaming_quotes`, `streaming_processing`), `enterprise-bench`.
