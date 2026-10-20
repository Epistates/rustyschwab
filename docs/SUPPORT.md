# Compatibility and support policy

rustyschwab is maintained by one person. This page says what is core, what is experimental and what has been checked against a real Schwab account, so you can decide what to depend on.

There is no response-time SLA, on-call coverage or regulatory certification. Production deployments must assign their own incident owner and broker escalation route.

## Support tiers

**Core.** Covered by the compatibility rules below. Regressions are fixed first.

| Component | Where |
| --- | --- |
| Wire types | `rustyschwab-types` |
| OAuth, token storage and refresh | `rustyschwab::auth` |
| REST client: accounts, orders, quotes, history, chains, movers, hours, instruments | `rustyschwab::SchwabClient` |
| Order journal (never-resend), risk limits, operator halt | `rustyschwab` order path |
| Streaming: connect, subscribe, reconnect, backpressure | `rustyschwab::streaming` |
| Paper venue v2 (`PaperBroker`) | `rustyschwab::paper`, `rustyschwab-paper` |
| MCP server, stdio transport, tool surface and policy | `rustyschwab-mcp` |
| Python client, auth, orders, streaming, paper | `rustyschwab` (top-level modules) |

**Experimental.** Usable and tested, but the API may change in any minor release without a deprecation period, and bug reports are handled after core ones.

| Component | Why it is experimental |
| --- | --- |
| Strategy runner, `rustyschwab.trader` (backtest, paper, live) | New in 0.2. The live path has not run against a real account. |
| NautilusTrader adapter, `rustyschwab.nautilus` | Tracks a fast-moving upstream (developed against 1.231). Not run live. |
| History cache (`rustyschwab::history`) and performance analytics (`get_performance`, `rustyschwab.performance`) | New in 0.2. |
| Shared authority and broker gateway (`rustyschwab-authority`, `rustyschwab-remote`, `RemoteBackend`) | Single writer; needs deployment and failover qualification. |
| MCP network transports (HTTP, WebSocket) | Built on TurboMCP `4.0.0-alpha.5`, a pre-release pin. The tool surface is core; the transport moves to stable TurboMCP 4.x when it ships. |
| Operator console and dashboard (`rustyschwab-console`, `rustyschwab-dash`) and run-time delegation | New after 0.3.0. The approval and delegation paths are tested over the paper venue; not run against a live account. |
| OpenTelemetry export (`otel` feature) | Optional; the Prometheus endpoint is the supported metrics path. |

## Live verification status

Everything above is tested against recorded broker responses and local fakes. No run against a real Schwab account has been recorded in this repository yet:

| Path | Live status |
| --- | --- |
| Read-only REST endpoints | Harness ready (`tests/live_conformance.rs`); no recorded run |
| Order placement and cancel (Rust, Python, MCP) | Not yet. See [the live smoke checklist](LIVE_SMOKE.md). |
| Streaming | Not yet |
| `rustyschwab.trader` live mode, Nautilus adapter | Not yet |

This table is updated from the report the live smoke run writes. Until a row says otherwise, treat that path as unproven with real money and start with the paper venue.

## Compatibility

The public Rust API follows Cargo's 0.x convention: a minor version can break; patch releases keep the minor version's public API and documented behavior. Safety fixes may deliberately reject previously accepted unsafe input; the changelog says so. CI compares the public API against the pull request base, and intentional breaks come with migration notes.

The Python package follows the same rule for its core modules.

The minimum supported Rust version is 1.89.0, checked in CI. Raising it requires a release note. Python 3.10 or newer is supported. Linux, macOS and Windows are CI targets; a platform claim requires a successful native run.

The 0.2 migration changes outbound order types, numeric precision, durable storage and network authorization. Follow [Operations](OPERATIONS.md): stop old writers, back up state and verify restoration before rollout. Rolling back to a JSON-only writer against migrated state is unsupported. Patch and schema upgrades must preserve pending intent, approval consumption, tag tombstones and audit ordering.

## Open acceptance gates

Code being complete is not deployment acceptance. These remain open:

- Live broker conformance for orders, including tag preservation and cancel, replace and fill races.
- Native Windows execution of the full suite and ACL behavior (currently compile-checked).
- Deployment identity, external risk and exposure, and cross-host coordinator or gateway integration.
- Backup restoration and disk exhaustion on the production filesystem.
- Sustained production-shaped load, recovery objectives and memory stability.
- A hosted release run with artifact provenance verification.

The history of how the 0.2 candidate got here is in [docs/history](history/).

## Reporting problems

Report reproducible defects with the version, platform, redacted diagnostics and a synthetic fixture where possible. Never attach live tokens, app secrets, account identifiers or private order history. Report vulnerabilities privately as described in [SECURITY.md](../SECURITY.md), not in a public issue.
