# Live smoke test

The first check of order placement against a real Schwab account, run by a person, before a release. It places an order that should not fill, confirms Schwab is working it, cancels it and confirms the cancel, once through the Python bindings and once through the MCP server. Results go in a JSON report that is committed and summarized in [SUPPORT.md](SUPPORT.md#live-verification-status).

> [!WARNING]
> This sends real orders. Each is `BUY 1 <symbol> LIMIT <half the bid> DAY`, which should not fill, and is cancelled within seconds. Run it during regular market hours, on an account you are happy to hold one share in, and watch it.

## What you need

- A Schwab app with Accounts and Trading enabled, and tokens from `cargo run -p oauth-flow` (see the [setup guide](SETUP_GUIDE.md)).
- An account with no other open buy orders. The MCP server's risk limits count every open buy on the account, so the script skips the MCP half when it finds one.
- A liquid stock priced above $2, such as `F` or `T`.
- The Python bindings in a virtualenv (`pip install ./crates/schwab-py`) and the server (`cargo build -p schwab-mcp --release`).

## 1. Read-only conformance

This issues only `GET`s and checks every typed binding against the live API:

```bash
SCHWAB_LIVE_CONFORMANCE=record cargo test -p schwab-rs --test live_conformance -- --nocapture
```

Review the fixture diff under `crates/schwab-rs/tests/fixtures/conformance/` and keep it for the commit.

## 2. Place and cancel

```bash
python tools/live_smoke.py --env-file .env accounts          # last four digits of each account
python tools/live_smoke.py --env-file .env run --account 1234 --symbol F
```

The script prints the order it will place and waits for you to type the symbol. Then, for each path:

| Step | Python bindings | MCP server (stdio) |
| --- | --- | --- |
| Ready | | `get_server_status` until trading is ready (first reconciliation done) |
| Build and check | `orders.equity_buy_limit` | `build_order`, `check_order` with a rationale |
| Preview | `preview_order` (a failure here is a warning) | `preview_order` (warning) |
| Place | `place_order` | `place_order` with a rationale |
| Working | `order` until it leaves `PENDING_ACTIVATION` | `get_order_status` |
| Cancel | `cancel_order`, then `order` until `CANCELED` | `cancel_order`, then `get_order_status` until `CANCELED` |

For the MCP half, the script starts `target/release/schwab-mcp` with a temporary principal policy named `live-smoke` (this account, one share, no approval, three orders a day so a rerun works) and risk limits that admit exactly that order, from the repository root so the server reads the same `.env`. The principal's audit journal stays beside your token file, as every principal's does; `schwab-admin` reads it.

If anything goes wrong after an order reaches Schwab, the script sends a cancel before it exits and tells you so.

## 3. Read the result

| Exit | Meaning |
| --- | --- |
| 0 | Every required step passed. |
| 1 | A step failed, and no order was left open. The report says which. |
| 2 | **Check the account now.** An order may still be open, its outcome is unknown, or it filled. The message names the order. |

On a failure the MCP server's log is kept and its path printed. It can contain account details; read it, do not commit it.

The report is written to `docs/live/smoke-<date>.json`. Account numbers, hashes and app credentials are redacted; symbol, prices, statuses, timings and error codes are kept. Read it before committing.

## 4. Record it

1. Commit the report and the conformance fixture diff together.
2. Update the table in [SUPPORT.md](SUPPORT.md#live-verification-status): the date, version and what passed.
3. File an issue for each failed step, with the report's step detail.

Order placement through the Rust SDK directly is the same code the Python bindings and the MCP server call; this run covers it through both.
