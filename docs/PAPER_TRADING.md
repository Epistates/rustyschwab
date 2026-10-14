# Paper trading and deterministic replay

`PaperBroker` provides persistent simulated long-equity execution. It owns no HTTP transport or OAuth credentials. `schwab-paper` runs offline command replays, an explicitly enabled read-only market feed, or turns price history into a synthetic quote stream for [backtesting](#backtesting-over-price-history); `schwab-authority --paper` exposes the same engine through verified identity, policy, approval and lease enforcement. Paper execution never routes to Schwab or thinkorswim paperMoney.

## Run the included replay

From the repository root:

```sh
cargo run -p schwab-mcp --bin schwab-paper --locked -- \
  --config ops/paper/config.json \
  --state ./paper-demo-state/state.sqlite3 \
  --commands ops/paper/replay.jsonl
```

Use a new state directory for a new replay. The eight-command fixture opens a synthetic session, submits five shares, fills two from a later quote, then cancels the remainder. Each output line is explicitly labeled `paper`. The final account has two ABC shares, cash `99799.9700`, no cash reservation, and a canceled order with two filled and three unfilled shares. Decimal representations may retain trailing zeros.

A command error stops the CLI with a nonzero exit code. Earlier commands remain committed. Replaying the entire file against the same database deliberately rejects duplicate order tags; use a new database for full-run reproduction. Exact event retries are idempotent and return their original result with `duplicate: true`; consumers must not count those fills twice.

## Verify a recorded transcript

Keep the original command JSONL and `schwab-paper` stdout as separate artifacts. Re-execute them with:

```sh
schwab-paper-verify --config ops/paper/config.json \
  --commands ops/paper/replay.jsonl --expected recorded-output.jsonl
```

The verifier creates private temporary state, requires replay clock mode, compares every output as structured JSON, and fails at the first mismatch or command failure. Object key order and whitespace do not matter; decimal string representations do. It rejects missing/extra output, empty transcripts, oversized records and archive commands. Final snapshots for **every configured account after the last mutating command** are required, followed by a logical integrity check. Temporary state is removed on ordinary exit.

This independently re-executes the supplied successful command transcript using the current engine. It is not an independently implemented financial oracle, does not reconstruct unrecorded gateway commands from audit events, and cannot verify a wall-clock run directly. File transcripts with failed commands still need a future expected-error format; backup replay below supports the two committed injected-fault outcomes. Preserve source/build/configuration alongside the transcript and review the expected output before adopting it as a regression baseline.

## Verify captured execution history from a backup

New paper databases capture execution mutations in the same transaction as the event/order/ledger change. Capture includes accepted events, order placement/replacement, cancellation, and the committed `reject_next` and lost-acknowledgement outcomes. Capture-write failure rolls back the mutation. Exact event/cancel retries that make no state change do not add a capture record.

```sh
schwab-admin --state ./paper-service-state/state.sqlite3 backup ./paper-backups/run.sqlite3
schwab-paper-verify --config ops/paper/config.json --backup ./paper-backups/run.sqlite3
```

Use the exact run configuration. Backup verification requires replay clock mode. It restores into private temporary state, checks capture continuity, replays commands in commit order, compares recorded results (including the two injected fault outcomes), then compares every account snapshot and all historical orders. The original backup is opened read-only by restore. Event payload compaction does not prevent this verification because full captured commands remain in the audit journal.

This checks execution-state reproducibility. It does not re-enact gateway authentication, approvals, leases, operator halts, drain state or rejected requests that committed nothing. It does not compare every metadata/audit field or provide a separate financial-model implementation. Wall-clock runs are captured but cannot yet be verified this way because admission uses system time.

Capture is complete only for new runs initialized with capture metadata. Older databases remain usable but cannot claim complete historical capture. Missing capture metadata on a captured run blocks further capture; reopen rejects incomplete metadata or a capture count/history mismatch. Do not add metadata manually to label historical state as complete. Capture increases retained audit volume; monitor storage and retain complete backups.

## SDK

```rust,no_run
use schwab_rs::paper::{PaperBroker, PaperConfig, PaperEvent};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let config: PaperConfig = serde_json::from_slice(
    &std::fs::read("ops/paper/config.json")?
)?;
let broker = PaperBroker::open("paper-state/state.sqlite3".into(), config).await?;
let event: PaperEvent = serde_json::from_str(
    r#"{"sequence":1,"at_ms":0,"kind":{"kind":"session","session_id":"test","open":true}}"#
)?;
broker.apply(event).await?;
let snapshot = broker.snapshot("paper-demo").await?;
assert_eq!(snapshot.mode, "paper");
# Ok(()) }
```

`place_order`, `replace_order`, `cancel_order`, `get_order`, `orders` and `snapshot` are available directly. The fenced variants enforce a current lease in the same transaction as risk/admission and acceptance. `TradingBackend` provides the common gateway command interface for `SchwabClient` and `PaperBroker`. Direct SDK callers remain trusted application code; verified user approval is enforced by the gateway.

## Execution and accounting contract

- Model `equity-bbo-v2` supports one EQUITY leg, whole shares, BUY or inventory-backed SELL, NORMAL session, MARKET/LIMIT/STOP/STOP_LIMIT, DAY/GTC. It rejects all other order types, strategies, optional routing/tax instructions and fractional quantities. Limit prices are at least one monetary unit with at most two decimal places.
- Market orders are **protected simulated market orders**: acceptance fixes a collar around the fresh ask for buys or bid for sells. An eligible later quote outside that collar rejects the remaining quantity. This is a declared model assumption, not Schwab market-order behavior.
- Stops trigger on an eligible later ask at/above the buy stop or bid at/below the sell stop. Trigger state persists across restart; a subsequent event must satisfy a new latency deadline before filling. STOP retains its acceptance-time protective collar; STOP_LIMIT retains its limit. These are explicit simulation trigger rules, not a claim about Schwab election rules. A consequence to weigh when testing stop-based exits: a price that gaps through a STOP past its collar rejects the stop instead of filling it, so the position stays open where a live stop-market order would have filled at the gap. Paper therefore understates gap losses for strategies that rely on stops.
- Orders never fill from the quote used to accept them. A later source timestamp and event sequence must arrive, and configured execution latency must elapse. Buys execute at ask plus configured slippage; sells at bid minus slippage. Limit bounds are never violated. An eligible order with insufficient event liquidity remains open.
- Each event offers only its explicit bid/ask share sizes. Orders consume that budget in acceptance-ID order across all accounts. A replacement gets a new ID and loses priority. This models finite liquidity, not actual exchange queue position, market impact or hidden liquidity. A quote update can offer new liquidity only when its source timestamp advances; an adapter must not fabricate updates from repeated snapshots.
- Replacement atomically retires a working order and validates the new one. Failure preserves the original order. Replacement quantity means **new remaining quantity**, excluding prior fills. Replacement acknowledgement is immediate; fill latency restarts. Pending cancels cannot be replaced.
- Cancel requests can remain pending until a later event reaches the configured cancel deadline. Fills before that deadline remain possible. At equal timestamps, the effective cancel wins. Inspect the order state after cancellation acknowledgement.
- Buys reserve collar/limit notional plus configured per-share fees. Sells reserve symbol-specific inventory. The limits mean what they mean live (`PortfolioLimits`): open quantity and notional bound what working buys could still add, the position cap bounds one symbol's position once its working buys fill, and a sell may sell only the symbol's position less its other working sells. The account freshness limit also applies at admission. New acceptance and risk checks commit together.
- The daily-loss control measures each session from the previous session's last complete mark (its close), as the live venue measures from its start-of-day value, so a gap at the open counts.
- In replay mode, default windows (recent orders, the trade journal, the portfolio summary) end at the venue's clock, not the wall clock, so a backtest sees its own working orders.
- A wall-clock feed that gets one unusable quote (absent, crossed, stale, under a dollar) skips that symbol for the tick; only calendar or transport failures disconnect the venue.
- An optional `max_daily_loss` in an account's limits is the daily-loss control. Every event marks the capped account at bid (cash, pending credits and holdings); the session's first complete mark, the first event at which every held position has a fresh quote, is kept as the session mark, and once equity falls further below it than the cap the event trips the durable operator halt in the same transaction (audited as `operator_halt_changed` by `daily-loss-control`, with the figures in the reason). Snapshots report the mark (`ledger.session_mark`) and the running `daily_loss`. Clearing the halt does not reset the day: the next event trips it again while the loss still exceeds the cap; a new session marks afresh. The cap is part of the pinned configuration, so trading on past it means a new run.
- Accounting uses checked `rust_decimal` arithmetic, with average acquisition cost including buy fees. Partial sales allocate cost proportionally; the final sale removes any residual rounding. Snapshots report realized P&L, fees, settled/available/reserved cash, pending credits, and conservative bid-marked equity/unrealized P&L. Marks are absent when an owned symbol lacks a fresh quote. Storage failures propagate as errors instead of being presented as missing marks.
- Buys debit settled cash immediately. Sell proceeds use the explicit `settlement_calendar` mapping from session ID to UTC epoch milliseconds. With `settlement_required: true`, a missing session entry denies admission and sell execution. Pending credits contribute to equity but cannot fund buys until a later event reaches maturity. The default is immediate proceeds for synthetic replay; the live adapter does not supply settlement dates. Populate and validate the required calendar before starting a cash-account run. A calendar is pinned for the run and is not silently extended.
- `Dividend` captures the current holding entitlement once under a durable action ID and credits it at `pay_at_ms`; subsequent sales do not erase it. The producer must supply the correct entitlement event and amount. Dividend income enters aggregate realized P&L; it is not tax classification.
- `Split` cancels working orders in the symbol, invalidates its quote, adjusts whole-share holdings and basis, and schedules fractional cash-in-lieu using the supplied price and payment time. It does not guess broker order adjustments. Terms and action IDs are validated; failures roll back the whole event.
- **Borrow, margin, tax lots, options and other corporate actions remain unsupported.** Fees are explicit simulation inputs, not a schedule the venue keeps current: `fee_per_share` and `fee_bps` (of notional) apply to every fill, and `sell_fee_bps` and `sell_fee_per_share` (capped per fill by `sell_fee_per_share_cap`) to sells only, for regulatory fees such as the SEC's Section 31 fee and FINRA's trading activity fee. Supply the rates in force. A buy reserves its price bound plus every buy fee; a sell whose price protection cannot cover its fees is rejected. Required settlement dates should follow the applicable product/account calendar; most covered U.S. transactions use T+1, but that is not a weekday-counting algorithm. See the [SEC settlement guidance](https://www.sec.gov/exams/educationhelpguidesfaqs/t1-faq).

## Replay versus live-data paper trading

The immutable configuration selects `"clock": "replay"` or `"clock": "wall"`:

| Mode | Time and input contract |
| --- | --- |
| Replay | Virtual time advances through ordered events. Replaying the same commands and events in a new database reproduces order IDs, fills and account values. There is no randomness or hidden fill probability. |
| Wall | Freshness also uses current system time. Stopped feeds cause submission/readiness denial without needing an explicit disconnect. New event receive timestamps must be recent and not future dated. Keep the feed and authority clocks synchronized. |

Replay callers supply session transitions and normalized quotes. Wall mode also supports `SchwabPaperFeed`, which uses a separate `SchwabClient` for quote and equity market-hours reads. It honors the returned regular-session windows, including offsets, early closes and closed days (a holiday or weekend closes the venue's session instead of faulting the feed), rejects missing calendar data and delayed/incomplete quotes, requires both side timestamps, and suppresses snapshots at or below durable source watermarks. Size units must be supplied explicitly; do not assume lots or shares. Upstream quote prices use the SDK's `f64` representation before decimal normalization; this is not lossless raw-JSON decimal ingestion.

To start the optional REST feed, copy the example configuration to a new file, select `"clock": "wall"`, and configure settlement as required. Supply the usual Schwab read-client environment configuration with **separate live-client state/token paths**. For a feed whose documented sizes are shares:

```sh
schwab-paper --config paper-wall.json --state ./paper-service-state/state.sqlite3 \
  --live-feed --symbols AAPL,MSFT --quote-size-multiplier 1 --poll-seconds 2
```

Only this explicit mode initializes OAuth. The authority can run separately against the same paper state and exact configuration. Neither path routes simulated orders to Schwab. Each network read has a ten-second deadline; feed errors append a durable disconnect after completed local mutations. Poll intervals skip missed ticks. Use one feed driver per run and provision request budget for both reads. This is REST snapshot polling, not a full-tick WebSocket feed or exchange queue simulator. Live Schwab conformance has not been executed in local qualification.

`Clock` events drive settlement and delayed cancels without quotes; `Disconnect` invalidates quotes while retaining watermarks. Session close expires DAY orders; GTC orders wait for an open session and new quote. The REST driver emits clock/session events on successful calendar polls. On disconnect, pending cash remains pending until another event advances time. Calendar exceptions are supplied by the broker rather than inferred from weekdays; see the [NYSE calendar](https://www.nyse.com/trade/hours-calendars?ecid=null).

Events start at sequence one and are contiguous across symbols and sessions. Reordered, conflicting duplicate, stale and crossed quote events fail without advancing the durable cursor. Give one trusted producer ownership of that sequence. `append_live` assigns wall time and the next sequence within the write transaction; explicit replay events still require a positive sequence. A stopped feed, missing sequence or unsupported event must be resolved by the producer; the engine does not guess missing market history.

## Backtesting over price history

The venue is quote-driven and Schwab's price history is OHLCV, so a backtest needs quotes the history never recorded. `schwab-paper --synthesize` derives them under a declared model, `candle-bbo-v1`, and writes them as `event` commands for the replay above. **This is a modeling assumption, not broker conformance**: a candle says where a bar opened, ranged and closed, not what was bid or offered, in what size, or in which order its extremes printed. The model fixes each of those by rule, and every session it emits carries the model's name in its id (`candle-bbo-v1:2024-05-17`), so a run built on synthetic quotes is never mistaken for one fed by observed ones. The engine's own model version is unchanged; fills follow the [execution contract](#execution-and-accounting-contract) exactly as they do for any other event.

The rules, each an assumption:

- A candle becomes four marks a quarter of a candle apart: the open, then the low and the high (low first when the bar closed at or above its open, high first when it closed below), then the close.
- Around each mark the bid and ask sit half of `--spread-bps` below and above it, the bid rounded down to the cent and the ask rounded up. A mark whose bid would fall below the venue's one-unit minimum is an error, not a clipped quote.
- Each mark offers, on each side, `--participation-bps` of the candle's volume divided across its four marks, in whole shares. A candle without volume quotes prices nobody trades against.
- A candle belongs to the New York calendar date it opened on, and each date is one venue session across every symbol: it opens at the date's first candle and closes when its last candle ends (`--candle-ms` after it opened), or when the next date's first candle opens if that comes first. DAY orders expire at that close, as at any session close.

An order accepted at one mark fills at a later mark's synthetic bid or ask plus configured slippage, never at its own limit price, and only up to the liquidity that mark offers; a resting limit therefore fills inside the first bar that trades through it, at that bar's extreme rather than at the limit. Marks within a candle are evenly spaced and carry no information about when the bar's extremes actually printed.

Each input file is one `PriceHistoryResponse` as `SchwabClient::get_price_history` returns it and as the `get_price_history` tool carries under `data`; candles must be in time order at least one candle apart, and no symbol may appear twice. Multiple symbols merge into one time-ordered stream, symbols in name order at a shared instant, numbered contiguously from `--first-sequence` (default 1):

```sh
schwab-paper --synthesize aapl.json msft.json \
  --candle-ms 60000 --spread-bps 5 --participation-bps 1000 > events.jsonl
```

Nothing is opened or written; there is no OAuth and no state. To backtest offline, interleave `place`, `cancel` and `snapshot` commands between the events and replay the file with `--commands` against a fresh replay-mode state, then verify the transcript as usual. To backtest a model, start the MCP server or the authority over that state and replay only the events, paced:

```sh
schwab-paper --config paper-replay.json --state ./backtest/state.sqlite3 \
  --commands events.jsonl --pace-ms 1000
```

`--pace-ms` waits that long between commands, so virtual time advances one mark per second while the model trades through the server against the same venue. The event and command transcripts, the histories and the three model parameters together reproduce the run.

## Performance reports

`schwab_rs::performance::analyze(&fills, &equity, &options)` turns a run's fills, and optionally its mark-to-market equity curve, into a `PerformanceReport`. It is a pure function over decimals: no I/O, no clock, so the same inputs give the same report. A paper venue fill (`PaperFill`) lacks the symbol and side of its order; `performance::Fill` adds them, and accepts a paper fill's `order_id` and `event_sequence` as they are. The Python binding is `rustyschwab.performance` ([PYTHON.md](PYTHON.md#performance-reports)).

Accounting rules, all fixed by the module:

- Fills match first in first out at the share level, long and short; a fill that crosses through flat closes the old position and opens the new one. Each match of one entry lot against one closing fill is a round trip, so partial fills, scale-ins and scale-outs count separately.
- A fill's fee is spread over the quantities it opens and closes in proportion to quantity, the last piece taking the exact remainder. A round trip carries its entry and exit portions; a round trip wins when its P&L after fees is positive. The `multiplier` (100 for options) scales P&L.
- Open lots are valued at `options.marks` or, for a symbol without one, its last fill price. That is an assumption; pass marks for a real valuation.
- Sharpe, Sortino, volatility and Calmar use the curve's last value per New York trading date, annualized by `periods_per_year` (default 252), and are `None` with fewer than two daily returns or no deviation. Drawdown uses the full-resolution curve, seeded at `starting_equity`. With no curve supplied, one is built from the fills and the report says so in `equity_source`.

A paper run's own curve is `PaperSnapshot::equity` after each event, which is `None` while any position lacks a fresh quote; skip those points. For a run that ends flat, the report's `realized_pnl_net` equals the ledger's `realized_pnl`; `tests/test_performance.py` checks this.

## Authenticated gateway

```sh
schwab-authority --identity identity.json --policy paper-policy.json \
  --paper ops/paper/config.json --paper-state ./paper-service-state/state.sqlite3
```

Paper startup does not load Schwab environment configuration or initialize broker OAuth. It still verifies enterprise JWTs for remote users. Use a policy that explicitly names the configured `paper-...` accounts. The existing `/v1/orders`, `/v1/operator` and `/v1/lease` protocols apply; approved order arguments and request IDs use the same schema as live execution. Replies include mode. Principal approvals and audit state are separate from live state. Existing principal constraints still apply, including denial of unbounded market intents when a principal requires a notional bound.

Additional paper-only routes:

| Route | Role | Body/result |
| --- | --- | --- |
| `POST /v1/paper/events` | Dedicated ingestion role | One `PaperEvent`; returns applied sequence and fills. See the role mapping below. |
| `POST /v1/paper/account` | Trader | `{"account":"paper-demo","after_id":0}`; authorized account snapshot and up to 100 historical orders, ascending numeric ID. Use the last ID as the next cursor. |

Market ingestion now requires distinct service subjects with explicit roles:

| Event | Required role |
| --- | --- |
| Quote, Clock, Disconnect | `paper_market_data` |
| Session | `paper_calendar` |
| Dividend, Split | `paper_corporate_actions` |
| Fault | `paper_fault` |

This is an authorization change: an `operator` grant alone no longer permits market ingestion. The authority retains its one-role-per-subject rule. Add only the needed subjects from the [ingestion grant example](../ops/paper/ingestion-grants.example.json) to the authority policy; fault access should exist only where intentional fault testing is required. A coordinated producer still owns the global event sequence when using separate service credentials.

The accepted event's audit record includes the verified authority subject, committed atomically with fills and the event cursor. Caller-supplied provenance in the HTTP event body is rejected. Local SDK events are labeled `local`, and the built-in REST adapter uses `schwab_rest`. SDK attribution is trusted application input, not cryptographic proof. Exact retries retain the original attribution and do not append another acceptance audit record. Pre-existing audit records remain without provenance.

The privileged generic `/v1/backend` API is disabled for paper authorities. Paper readiness reports `tokensValid: false` because it does not use broker tokens; `mode: paper` identifies the interpretation. Readiness indicates local engine availability and at least one fresh quote; each order still requires its own symbol's fresh quote and sufficient cash/inventory. The [shared authority guide](SHARED_AUTHORITY.md) describes JWT roles, approval consumption, HTTPS proxying and ambiguous gateway replies.

## MCP server

The MCP server runs over the same venue when `SCHWAB_MCP_PAPER_CONFIG` and `SCHWAB_MCP_PAPER_STATE` are set, with the same startup posture as the authority: no Schwab environment configuration is read and no OAuth is initialized unless `SCHWAB_APP_KEY` is present, which adds a read-only market-data client the venue never routes an order through. Every tool then answers from the venue in Schwab's shapes, reports `source: "paper"`, and the mode is published in `serverInfo.title`, `get_server_status` and `/readyz`. The MCP server does not ingest market events: drive the venue from a separate process against the same state and exact configuration, `schwab-paper --live-feed`, a paced `schwab-paper --commands` replay of [synthetic quotes](#backtesting-over-price-history), or the authority's ingestion routes, as the authority itself is driven. The [MCP integration review](MCP_LLM_INTEGRATION.md#paper-mode) lists what the venue answers and what it refuses.

`schwab-eval` builds on this to score a model's tool use: each of its scenarios seeds a private copy of the venue (a stale quote, a working order whose tag carries an injected instruction, a lost acknowledgement fault) behind the policy-gated server, serves it over stdio while recording the model's calls, and replays the transcript deterministically for the report. See [model evaluation](MCP_LLM_INTEGRATION.md#model-evaluation).

## Durability, failure tests and audit

SQLite IMMEDIATE transactions commit event cursors, fills, accounts, reservation changes and audit observations together. Blocking database work uses the existing bounded Tokio worker pool. Database configuration pins the model and costs; incompatible reopen attempts fail. Reopening also requires all paper tables, the configured account records, and event-history count/latest sequence matching the durable cursor. Missing state is rejected rather than recreated or funded again. Missing core execution tables, metadata or schema versions also fail closed; restore a verified complete backup. These structural checks do not substitute for full logical replay or detect every possible row-level corruption. Paper and live mode markers cannot be interchanged through normal client initialization. The state directory must be private. Trusted filesystem administrators can still alter files and remain outside the isolation boundary.

The engine reuses execution journals, durable operator halts, coordinator leases and audit delivery. It evaluates its exact paper ledger inside the transaction rather than calling a remote risk provider against simulated state. Halts block new/replacement orders while existing working orders may fill and cancellation remains available. Process drain blocks admission on that broker instance and its clones.

Two persisted one-shot fault events are available: `reject_next` and `lose_next_acknowledgement`. Lost acknowledgement commits the order and returns `SubmissionUnknown`; inspect durable order history/tag and the accepted execution record before proceeding. Storage/worker errors during order mutation are also classified conservatively as `SubmissionUnknown`; inspect the account/tag or order ID because a persistence error can obscure the commit outcome. No automatic mutation retry occurs. Gateway-level unknown principal attempts still require the existing evidenced operator resolution process.

Preserve the configuration, command file, event stream, model version and source/build revision for a run. `verify_integrity()` checks a consistent SQLite snapshot for event continuity, ledger conservation, nonnegative whole-share positions, reservation coverage, and order/tag/reservation agreement. It detects logical inconsistencies; it does not replay all commands or detect coordinated hostile rewrites.

`archive_events(destination, through_sequence)` first creates an exclusive private online backup, hashes it, verifies payloads against that backup, and replaces covered event payloads with SHA-256 receipts in one transaction. Exact event retry results and conflicting-retry rejection remain available. Results, receipt rows and audit history remain in the active database; this reduces payload storage but does **not** bound total disk use or immediately shrink the SQLite file. Retain all archive generations and their checksums: later archives may contain receipts for older payloads. An existing destination is never overwritten. External immutable retention, archive availability monitoring and restore drills remain deployment responsibilities.

The CLI accepts `{"command":"verify"}` and `{"command":"archive","destination":"./archive/run.sqlite3","through_sequence":100}` as JSONL commands. Use the existing online restore and acknowledged audit-delivery tools. Do not manually delete receipt rows.

Version 2 changes execution/accounting semantics. It rejects version 1 configurations and existing version 1 databases. Keep the original binary/configuration for historical reproduction, or start a new version 2 run; changing the version string on old state is not a migration.

Implementation references: [rusqlite transaction semantics](https://docs.rs/rusqlite/0.40.0/rusqlite/struct.Transaction.html), [checked decimal arithmetic](https://docs.rs/rust_decimal/1.43.0/rust_decimal/struct.Decimal.html), and [Tokio blocking-task behavior](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html). A simulation validates software behavior under declared assumptions; its profitability and fill quality are not live-broker conformance evidence.

Local validation and the executed replay output are recorded under `docs/audit/paper-trading/` and the follow-up diligence pass (initialization, timestamp and storage-error corrections) under `docs/audit/paper-diligence/`. That directory is git-ignored local evidence, regenerated by the commands above.

Version 2 validation and remaining scope are recorded in [v2 evidence](audit/paper-v2/README.md) and the [capability checklist](PAPER_LIMITATIONS.md).
