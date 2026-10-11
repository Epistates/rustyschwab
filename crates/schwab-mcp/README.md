# schwab-mcp

A [Model Context Protocol](https://modelcontextprotocol.io/) server over the
[`schwab-rs`](https://docs.rs/schwab-rs) SDK: Schwab's REST endpoints and
streamer as tools an agent can call, behind per-principal policy, operator
approval and a durable audit journal. It serves stdio, HTTP and WebSocket
transports, and a local paper venue for trading without real money.

> [!WARNING]
> Automated trading can lose money. Order tools are off until you turn them
> on (`SCHWAB_MCP_ALLOW_TRADING=1`, `SCHWAB_MCP_ALLOW_CANCEL=1`); try your
> setup in paper mode first.

```bash
cargo install schwab-mcp
schwab-mcp                     # stdio, for desktop MCP clients
```

The server reads the SDK's `SCHWAB_*` settings and expects tokens on disk
(`SCHWAB_TOKENS_FILE`). Network transports need `SCHWAB_MCP_POLICY_FILE`
and a bearer token on every request.

## Binaries

| Binary | Role |
|--------|------|
| `schwab-mcp` | The MCP server. |
| `schwab-authority` | Holds the broker session and gates orders for workers that do not. |
| `schwab-admin` | Operator commands: approvals, halts, reconciliation, audit delivery, backup and restore. |
| `schwab-remote` | Sends one request to an authority. |
| `schwab-paper`, `schwab-paper-verify` | Run and verify the paper venue and its replays. |
| `schwab-eval` | Scores a model against seeded paper scenarios. |

## Features

| Feature | Default | Adds |
|---------|---------|------|
| `otel` | no | OTLP/HTTP export of spans and metrics from `schwab-mcp` and `schwab-authority`, configured by the standard `OTEL_*` environment (`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SERVICE_NAME`, ...). Without an endpoint it changes nothing; the Prometheus endpoint (`SCHWAB_METRICS_BIND`) works either way. See [docs/OPERATIONS.md](https://github.com/epistates/rustyschwab/blob/main/docs/OPERATIONS.md#observability-opentelemetry). |

The repository [README](https://github.com/epistates/rustyschwab#mcp-server-schwab-mcp)
covers configuration, the tool surface and metrics, and
[docs/INDEX.md](https://github.com/epistates/rustyschwab/blob/main/docs/INDEX.md)
lists the operations, paper trading and shared-authority guides.
