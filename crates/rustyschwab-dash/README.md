# rustyschwab-dash

A terminal dashboard for following and supervising agents that trade through
[`rustyschwab-mcp`](https://crates.io/crates/rustyschwab-mcp): every tool call
as it happens with the model's reasons, orders waiting for your approval,
positions, P&L and quotes, and the operator's controls: approve, refuse,
delegate an approval mode for a time or until cancelled, halt, resume and
kill.

```bash
cargo install rustyschwab-dash
rustyschwab-dash        # reads the same .env as rustyschwab-mcp
```

The guide is [docs/DASHBOARD.md](https://github.com/epistates/rustyschwab/blob/main/docs/DASHBOARD.md).
It is built on [`rustyschwab-console`](https://crates.io/crates/rustyschwab-console),
which other frontends can use too.

Unofficial; not affiliated with Charles Schwab & Co., Inc. MIT or Apache-2.0.
