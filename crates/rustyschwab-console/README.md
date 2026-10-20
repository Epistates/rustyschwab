# rustyschwab-console

The operator's console core for agents trading through
[`rustyschwab-mcp`](https://crates.io/crates/rustyschwab-mcp), with no user
interface of its own. [`rustyschwab-dash`](../rustyschwab-dash) is the
terminal frontend; anything else (a web page, a chat bot) can be built on the
same two pieces:

- **A live `Snapshot`**: what every agent is doing (each tool call, refusals
  included, with the model's stated rationale), orders waiting for approval,
  each agent's approval mode, dead-man countdown and daily budget, accounts,
  positions and P&L, orders attributed to the agent that placed them,
  quotes, and the operator halt. It is plain `serde` data.
- **Commands**: approve or refuse a waiting order, delegate an approval mode
  to an agent for a time or until cancelled, withdraw it, halt, resume, and
  kill (halt, then cancel every working order).

The console runs as its own process and reads the same state files as the
servers, with the same trust as `rustyschwab-admin`; it opens no port.
Watching sends nothing to Schwab, apart from quotes in live mode (one request
every few seconds from the shared budget; `SCHWAB_DASH_QUOTES=0` turns it
off).

It reads the server's environment (`SCHWAB_TOKENS_FILE` for live,
`SCHWAB_MCP_PAPER_CONFIG` and `SCHWAB_MCP_PAPER_STATE` for paper,
`SCHWAB_MCP_POLICY_FILE` for the agents), plus `SCHWAB_OPERATOR` for the name
its commands are recorded under and `SCHWAB_DASH_WATCHLIST` for extra
symbols. See [docs/DASHBOARD.md](https://github.com/epistates/rustyschwab/blob/main/docs/DASHBOARD.md).

Unofficial; not affiliated with Charles Schwab & Co., Inc. MIT or Apache-2.0.
