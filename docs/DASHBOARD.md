# The dashboard

`rustyschwab-dash` is a terminal dashboard for following and supervising agents that trade through `rustyschwab-mcp`. Run it in a second terminal, or a split pane next to Claude Code, and you see each agent's tool calls as they happen, with the model's stated reason for every order. From the same screen you approve or refuse orders waiting on you, hand an agent more or less authority, halt trading, or stop everything.

```text
 RUSTYSCHWAB  LIVE   Tue Oct 20 11:25:01 ET   TRADING   operator nick
┌ 4 AGENTS ─────────────────────────────────────────────────────────────────────────────────────────────┐
│AGENT     APPROVAL        DELEGATED          CEILING          DEADMAN BUDGET TODAY               LAST  │
│desk      APPROVE>$2500   by nick to 14:25   max autonomous   4:12    3/20 orders  $4120/25000   4s ago│
└───────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌ 1 AGENT TAPE ───────────────────────────────────────────────┐┌ 2 APPROVALS (1 WAITING) ────────────────┐
│11:24:46 desk  place_order  approval_required  BUY 40 NVDA … ││desk  BUY 40 NVDA LIMIT 131.20 DAY      │
│11:24:28 desk  place_order  ok                 BUY 10 AAPL … │└─────────────────────────────────────────┘
│11:24:10 desk  get_quotes   ok                 symbols=AAPL… │┌ 3 ORDERS ───────────────────────────────┐
├─────────────────────────────────────────────────────────────┤│4021  WORKING  BUY 10 AAPL  LIMIT 189.50 │
│WHY Momentum continuation after earnings gap                 ││4019  FILLED   SELL 5 MSFT  LIMIT 431.10 │
└─────────────────────────────────────────────────────────────┘└─────────────────────────────────────────┘
  POSITIONS  …4821 AAPL 30 +54.30 | QUOTES AAPL 189.48 +0.42% | ACCOUNT value 104233.17  day +412.30
 a approve  x refuse  d delegate  u end deleg.  h halt  r resume  K kill  ? help  q quit
```

## Running it

It reads the same environment as the server, `.env` included, so start it from the same directory with the same settings:

```bash
cargo install rustyschwab-dash           # or: cargo run -p rustyschwab-dash
rustyschwab-dash                         # live: SCHWAB_TOKENS_FILE, SCHWAB_MCP_POLICY_FILE
SCHWAB_MCP_PAPER_CONFIG=ops/paper/config.json SCHWAB_MCP_PAPER_STATE=state/paper.sqlite3 \
  SCHWAB_MCP_POLICY_FILE=policy.json rustyschwab-dash      # paper
```

| Setting | |
| --- | --- |
| `SCHWAB_MCP_POLICY_FILE` | The agents (principals) to follow. Without it the dashboard shows the accounts and orders alone. |
| `SCHWAB_TOKENS_FILE`, or `SCHWAB_MCP_PAPER_CONFIG` with `SCHWAB_MCP_PAPER_STATE` | The venue, as for the server. |
| `SCHWAB_OPERATOR` or `--operator` | The name your approvals, delegations and halts are recorded under. Defaults to your login name; it must differ from every agent's name. |
| `SCHWAB_DASH_WATCHLIST` or `--watch` | Symbols to quote besides those held or ordered. |
| `SCHWAB_DASH_QUOTES=0` | Live mode: no quote requests (see below). |
| `--log` | Where the dashboard writes its own log (default `rustyschwab-dash.log`); the terminal belongs to the screen. |

Quitting (`q`) closes the dashboard only; the agents and servers keep running.

## What it shows

- **Agents.** Each principal's approval mode, who delegated it and until when, the most the policy lets you grant (the ceiling), its dead-man countdown, its daily budget used, submissions whose outcome is still open, and when any server last heard from it.
- **Agent tape.** Every tool call by every agent, newest first: reads, orders, and refusals with their code. Select a row for the detail line: the order, how long the call took, the venue order id, the model's rationale (`WHY`), and a refusal's message.
- **Approvals.** Orders an agent is waiting on you for, described in words, with the model's reason.
- **Orders.** Working orders first, then recent ones, each with the agent that placed it.
- **Positions, quotes, account.** Holdings with day and open P&L, quotes for what is held, ordered or on the watchlist, and the account's value, cash and buying power.

Everything is read from the state the servers keep (each agent's journal and activity log, the venue's reconciled account and orders, the operator halt), about once a second. In live mode the account figures are those reconciliation last read, at most a few seconds old (`as of` shows how old); the dashboard sends no account request of its own. Quotes are the exception: in live mode they cost one Schwab request every five seconds from the request budget the servers share, and only while there is something to quote. Set `SCHWAB_DASH_QUOTES=0` to send none.

## Acting

Every action asks for confirmation first, and every one is recorded in the audit trail under your operator name.

| Key | |
| --- | --- |
| `a` / `x` | Approve or refuse the selected waiting order. An approval lasts 15 minutes; the agent's next submission of that exact order goes through (one already holding with `wait_ms` goes at once). A refusal is permanent for that order. |
| `d` | Delegate an approval mode to the selected agent (below). |
| `u` | End the agent's delegation: its policy's own mode again. |
| `h` / `r` | Halt or resume all trading, on every server that shares this state. |
| `K` | Kill: halt, then cancel every working order on every account shown. Asks for a reason, then for you to type `kill`. In live mode it logs in to Schwab to cancel; if it cannot, trading stays halted and it says so (use `rustyschwab-admin kill`). |
| `Tab`, `1`-`4`, `↑↓` | Move between and within the tape, approvals, orders and agents. |

## Delegation

How much an agent may do without you approving each order is its approval mode:

- **Supervised**: you approve every order.
- **Threshold**: you approve only orders whose bounded notional is over an amount; smaller ones go.
- **Autonomous**: no approvals.

Every other limit applies in every mode: the agent's tools, accounts, symbols, price collar, order size and daily budget, the risk limits, halts and the dead-man switch. Delegation only decides approval.

`d` opens a form: pick the mode, the amount for a threshold, and how long it lasts: good till cancelled, an hour, four hours, or until today's close (16:00 New York). When it lapses, or you end it with `u`, the agent's policy's own mode applies again.

The policy file decides how far you can go. An agent's own mode comes from `require_approval` and `approval_threshold`; `max_delegation` is the widest you may grant from the dashboard. You can always tighten an agent (put an autonomous one under supervision), but you can widen it only up to `max_delegation`, which defaults to the policy's own mode. To be able to grant autonomy to a supervised agent:

```json
{"name": "desk", "require_approval": true,
 "max_delegation": {"mode": "autonomous"}, "...": "..."}
```

or at most a threshold: `{"mode": "threshold", "above": "2500"}`. A grant is stored in the agent's journal, so every server process serving the agent applies it at once, and the agent sees it in `get_server_status` (`principal.approval`).

## Trust

The dashboard opens the same state files as the servers and `rustyschwab-admin`, with the same trust: whoever can run it as the account that owns those files can approve, delegate, halt and kill. It opens no network port. Run it as that account, on that machine.

## Building another frontend

The dashboard is a thin layer over [`rustyschwab-console`](../crates/rustyschwab-console), which has no user interface: `Console::open(ConsoleConfig::from_env()?)` then `.spawn()` gives a handle with the latest `Snapshot` (plain serializable data) and `execute(Command)`. A web page, a chat bot or a different terminal UI can be built on the same two calls.
