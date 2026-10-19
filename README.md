# rustyschwab

An unofficial Rust SDK, Python package and MCP server for the Charles Schwab Trader and Market Data APIs, built for automated strategies and LLM agents that place real orders.

> [!WARNING]
> Automated trading can lose money. This software is provided as is, without warranty. Start with the [paper venue](docs/PAPER_TRADING.md), which runs locally and does not connect to thinkorswim paperMoney. Not affiliated with or endorsed by Charles Schwab & Co., Inc.

**Status:** 0.2.0. The Rust crates are on [crates.io](https://crates.io/crates/rustyschwab); the Python package is not on PyPI yet, so build it from this repository. One maintainer. Everything is tested against recorded Schwab responses and local fakes, but no live-account run has been recorded yet, so treat live trading as unproven. See [what is core, what is experimental, and what has run live](docs/SUPPORT.md).

## What is in it

| Part | For | Docs |
| --- | --- | --- |
| [`rustyschwab`](crates/rustyschwab) | Rust applications: OAuth, REST, the streamer, the order journal, risk limits, paper venue, history cache, performance analytics | [crate guide](crates/rustyschwab/README.md) |
| [`rustyschwab`](crates/schwab-py) (Python) | The same SDK from asyncio or blocking Python, order templates, one strategy for backtest, paper and live, a NautilusTrader adapter | [Python guide](docs/PYTHON.md) |
| [`rustyschwab-mcp`](crates/rustyschwab-mcp) | LLM agents: an MCP server with 40 tools over the SDK, behind per-principal policy, approvals and a paper mode, plus operator and gateway commands | [MCP guide](docs/MCP_LLM_INTEGRATION.md) |

## What it does differently

The hard part of broker automation is what happens when something goes wrong. rustyschwab is built around that:

- **An order is sent at most once.** Every submission is journaled before it leaves the process. A timeout or dropped connection becomes `SubmissionUnknown`, never a retry, and the account's next submission waits until the journal reconciles the order against Schwab.
- **Limits hold outside the strategy.** Risk limits, a daily-loss halt, an operator halt and a dead-man switch that cancels a silent agent's orders are enforced by the SDK and the server, not by the code placing the orders.
- **Agents get a narrow, checked surface.** MCP write tools are off by default. Each principal is held to its accounts, tools, symbols, price collar and daily budget; orders can require operator or user approval; `check_order` explains every local reason an order would fail before anything reaches Schwab; and every write is recorded, with the model's stated rationale, in a hash-chained audit journal.
- **Simulation uses the same code path.** The paper venue is deterministic and durable (SQLite), with finite liquidity, latency, fees and settlement in exact decimals. A Python strategy runs unchanged as a backtest, against the paper venue on live data, or live.
- **Credentials stay out of reach.** Tokens live in the OS keychain or an encrypted file, are zeroized in memory, and never appear in logs, errors or reprs.

## Quickstart

Register an app at [developer.schwab.com](https://developer.schwab.com/) with callback URL `https://127.0.0.1:8080`, then:

```bash
git clone https://github.com/epistates/rustyschwab && cd rustyschwab
cp .env.example .env            # set SCHWAB_APP_KEY, SCHWAB_APP_SECRET
cargo run -p oauth-flow         # sign in once; tokens are stored securely
```

The [setup guide](docs/SETUP_GUIDE.md) covers each step, the token stores, and a one-time [mkcert setup](docs/SETUP_GUIDE.md#a-a-trusted-local-certificate-macos) that removes the browser's certificate warning from sign-in.

### Rust

```toml
[dependencies]
rustyschwab = "0.2"
tokio = { version = "1", features = ["full"] }
```

```rust,ignore
use rustyschwab::prelude::*;

let client = SchwabClient::new(SchwabConfig::from_env()?)?;
client.init().await?;
let quotes = client.get_quotes(&["AAPL", "MSFT"]).await?;
```

Placing orders, brackets and options: [ORDERS.md](docs/ORDERS.md). Streaming: [STREAMING.md](docs/STREAMING.md).

### Python

```bash
pip install ./crates/schwab-py   # from the clone; builds the extension, needs a Rust toolchain
```

```python
import asyncio
from rustyschwab import Client, orders
from rustyschwab.trader import Trader

def strategy(tc, events):
    for event in events:
        if event["type"] == "candle" and not tc.positions.get(event["symbol"]):
            tc.order(orders.equity_buy_market(event["symbol"], 1))

async def main():
    run = await Trader(Client.from_env()).backtest(strategy, ["AAPL"], days=5, cash=10_000)
    print(run.report())

asyncio.run(main())
```

`trader.paper(...)` and `trader.live(...)` run the same function. [PYTHON.md](docs/PYTHON.md) has the rest; [coming from Schwabdev](docs/MIGRATING_FROM_SCHWABDEV.md) maps every Schwabdev call to its equivalent.

### MCP

```bash
cargo install rustyschwab-mcp    # installs the server and its operator tools
claude mcp add schwab -e SCHWAB_APP_KEY=… -e SCHWAB_APP_SECRET=… \
  -e SCHWAB_TOKENS_FILE="$PWD/schwab_tokens.json" \
  -- "$(command -v rustyschwab-mcp)"
```

Order tools stay off until you enable them. To let an agent trade without real money, run the server over the paper venue:

```bash
SCHWAB_MCP_PAPER_CONFIG=ops/paper/config.json SCHWAB_MCP_PAPER_STATE=state/paper.sqlite3 \
SCHWAB_MCP_ALLOW_TRADING=1 rustyschwab-mcp
```

Claude Desktop configuration, the tool catalog and the policy model: [MCP guide](docs/MCP_LLM_INTEGRATION.md). Policies, approvals, risk limits, metrics and recovery: [Operations](docs/OPERATIONS.md).

## Documentation

[docs/INDEX.md](docs/INDEX.md) lists every guide. The ones most people need:

- [Setup](docs/SETUP_GUIDE.md) and [callback URLs](docs/APP_CALLBACK.md)
- [Orders](docs/ORDERS.md) and [streaming](docs/STREAMING.md)
- [Python](docs/PYTHON.md) and [coming from Schwabdev](docs/MIGRATING_FROM_SCHWABDEV.md)
- [MCP and LLM integration](docs/MCP_LLM_INTEGRATION.md)
- [Paper trading](docs/PAPER_TRADING.md) and [what it does not model](docs/PAPER_LIMITATIONS.md)
- [Operations](docs/OPERATIONS.md) and [shared authority](docs/SHARED_AUTHORITY.md) for multi-process deployments
- [Troubleshooting](docs/TROUBLESHOOTING.md)
- [Architecture](docs/ARCHITECTURE_OVERVIEW.md), for contributors

Runnable programs are under [`examples/`](examples): `cargo run -p oauth-flow`, `comprehensive`, `auth_test`, and `streaming-examples` (`--bin streaming_quotes`).

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md) and the [changelog](CHANGELOG.md). Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## License

MIT or Apache-2.0, at your option.
