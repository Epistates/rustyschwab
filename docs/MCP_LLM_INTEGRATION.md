# MCP and LLM integration review

Reviewed 2026-09-10. The MCP server now uses stable TurboMCP 3.3.0, with
explicit behavior annotations for all 38 tools (`build_order`, which expands
an `OrderSpec` locally, `check_order`, the local pre-flight described below,
`get_server_status`, the three projection tools, `get_portfolio_summary` and
`get_order_status` were added after the review). Reads and previews are marked
read-only; order and stream mutations are conservatively marked destructive
and non-idempotent. Cached stream reads are marked closed-world. These are
client hints, not authorization controls. Read-only does not imply public data.
Server discovery describes live trading, untrusted returned text, stale caches,
and the requirement to reconcile uncertain submissions before further action.

## Recommended integration boundary

Use MCP as the provider-neutral model interface. Keep model inference in the
host application; this server does not need model-provider credentials or an
embedded agent framework. Give each application a distinct principal and a
small tool allowlist. Research assistants normally need market-data reads;
account reads require separate account-scoped permission. Stream control is
stateful and should have one owner per Schwab login.

For network clients use authenticated Streamable HTTP, TLS termination, and
short-lived issuer tokens with the existing audience/issuer checks. Stdio is
appropriate for a trusted local host. WebSocket is a convenience transport,
not a substitute for standard MCP interoperability testing. See
[authorization and operations](OPERATIONS.md) and [shared authority](SHARED_AUTHORITY.md).

A model may propose an order, but operator-controlled policy and durable approval
must authorize the exact intent. A preview is neither approval nor a fill
prediction. Tool annotations, prompts, elicitation answers, and text in tool
results must never grant permission. Preserve decimal strings through the host;
do not round-trip prices through binary floating point. If execution is
uncertain, retain the attempt ID and reconcile; do not regenerate the request
or blindly retry a tool call. Treat descriptions and external market text as
untrusted data even when authenticated transport supplied them.

## Result contract

Every tool answers in one of two shapes (`crates/schwab-mcp/src/contract.rs`).

A success is compact JSON text. Data that came from Schwab or the stream
cache is wrapped as `{"asOf": <RFC 3339>, "source": "schwab_rest" |
"stream_cache", "data": <Schwab's raw shape, redacted>}`; local results
(`build_order`, `check_order`, `get_server_status`, stream control
acknowledgements, `stream_status`) are bare.
A response that would exceed the transport's 1 MiB message limit is refused
with `response_too_large` rather than clipped; the projection tools below
stay within a much smaller budget by paging.

A failure is a tool result with `isError: true` whose text and
`structuredContent` are both
`{"error": {"code", "retryable", "disposition", "message", "attemptId"?, "intentDigest"?}}`:

| `code` | `disposition` | `retryable` | Meaning |
| --- | --- | --- | --- |
| `invalid_arguments`, `not_found`, `risk_denied`, `response_too_large` | `fix_arguments` | false | Change the request; a smaller or bounded order may pass a risk denial |
| `not_permitted` | `not_permitted` | false | Policy or the server's trading switch forbids the call; do not retry |
| `approval_required` | `ask_operator` | false | Valid intent awaiting operator approval of `intentDigest` |
| `halted`, `internal` | `ask_operator` | false | Operator halt, or a fault logged under the reference id in `message` |
| `duplicate_intent`, `reconciliation_required` | `reconcile` | false | Read the order state first; never resubmit. `attemptId` names the uncertain submission |
| `unavailable`, `rate_limited`, `timeout` | `retry_later` | true | Nothing reached the broker; repeat the same call after a delay |

An unauthenticated Schwab session is `unavailable` with `ask_operator`, since
only the operator can re-run OAuth. Policy denials from the principal wrapper
use the same shape, so the model reads them instead of receiving a JSON-RPC
error; JSON-RPC errors are reserved for protocol faults. Schwab's own 4xx
rejection text (title and detail, describing the submitted request) is relayed
in `message`; every other upstream detail is logged under a reference id.
Client middleware that retries must key on `retryable`, never on `isError`.

## Query windows

The time-windowed reads (`get_account_orders`, `get_all_orders`,
`get_transactions`, `get_price_history`) take model-facing query types
(`crates/schwab-mcp/src/query.rs`) rather than the SDK's strict parameter
structs. A bound may be a calendar date (`YYYY-MM-DD`, covering that whole
UTC day), an RFC 3339 timestamp, a date-time without an offset (taken as
UTC), or epoch milliseconds. Orders and transactions default to the last
seven days ending now, and transactions to `TRADE`, so `params` may be
omitted. Unknown fields are refused rather than ignored, so a misspelled
bound cannot silently widen a window. The resolved request is still built
and validated by the SDK, and Schwab's one-year cap still applies.

## Projections

Three tools answer the bulk reads in a fraction of the raw tools' bytes
(`crates/schwab-mcp/src/projection.rs`), keeping the raw tools for the
full shape. Each takes the same parameters as its raw counterpart plus
`limit` and `cursor`, and returns, inside the usual envelope:

```json
{"asOf": "…", "source": "schwab_rest",
 "data": {"symbol": "AAPL", "previousClose": 149.5,
          "columns": ["t", "o", "h", "l", "c", "v"],
          "rows": [[1736950200000, 150.12, 150.5, 149.9, 150.3, 1234567], "…"],
          "count": 1300, "total": 3900, "truncated": true, "nextCursor": "1737028200000"}}
```

| Tool | Raw tool | Row | Order |
| --- | --- | --- | --- |
| `get_candles(params, limit?, cursor?)` | `get_price_history` | `[t, o, h, l, c, v]`, `t` in epoch milliseconds | time |
| `get_option_contracts(params, limit?, cursor?)` | `get_option_chain` | `symbol, putCall, expiration, strike, bid, ask, mark, last, volume, openInterest, delta, gamma, theta, vega, iv, itm, dte` | expiration, strike, calls before puts |
| `get_positions(account_hash, limit?, cursor?)` | `get_account` | `symbol, assetType, longQuantity, shortQuantity, averagePrice, marketValue, currentDayProfitLoss, longOpenProfitLoss, shortOpenProfitLoss` | symbol |

A page stops at `limit` rows or when its rows reach
`SCHWAB_MCP_RESPONSE_BUDGET_BYTES` (default 65536, clamped to 4 KiB..1 MiB),
whichever comes first, and always carries at least one row. `truncated:
true` means rows remain; `nextCursor` is the last row's ordering key, and
passing it back as `cursor` re-reads the query from Schwab and continues
after that key, so rows added or repriced between pages are neither
skipped nor duplicated. `total` counts the query's rows before paging.
Values are Schwab's, unconverted; only the shape is projected.
`get_option_contracts` tabulates single-leg (and `ANALYTICAL`) chains and
refuses spread strategies, which need the raw tool.

## Portfolio summary

`get_portfolio_summary(account_hash)` answers "where does this account
stand" in one bounded read (`crates/schwab-mcp/src/portfolio.rs`), so a
model does not chain `get_account`, `get_account_orders` and the
arithmetic between them:

```json
{"asOf": "…", "source": "schwab_rest",
 "data": {"accountNumber": "***6789", "type": "MARGIN", "isDayTrader": false,
          "roundTrips": 1, "isClosingOnlyRestricted": null,
          "balances": {"liquidationValue": 2000.0, "equity": null, "cashBalance": 200.0,
                       "availableFunds": 150.0, "buyingPower": 300.0,
                       "dayTradingBuyingPower": 600.0, "longMarketValue": 1800.0,
                       "shortMarketValue": null, "maintenanceRequirement": 540.0,
                       "maintenanceCall": 0.0, "regTCall": null, "cashCall": null,
                       "unsettledCash": null, "inCall": false},
          "pnl": {"day": 3.75, "open": 490.0},
          "positions": {"columns": ["symbol", "assetType", "…"], "rows": ["…"],
                        "count": 2, "total": 2, "truncated": false},
          "workingOrders": {"columns": ["orderId", "parentOrderId", "status", "strategy",
                                        "orderType", "duration", "session", "price",
                                        "stopPrice", "quantity", "filledQuantity",
                                        "remainingQuantity", "enteredTime", "legs"],
                            "rows": [["2", null, "WORKING", null, "LIMIT", "DAY", "NORMAL",
                                      150.0, null, 10.0, 0.0, 10.0, "2025-01-15T14:30:00+0000",
                                      [{"legId": 1, "instruction": "BUY", "quantity": 10.0,
                                        "symbol": "AAPL", "assetType": "EQUITY",
                                        "positionEffect": null}]]],
                            "count": 1, "total": 1, "truncated": false},
          "ordersSince": "2025-01-13T14:30:00.000Z",
          "pendingSubmissions": 0,
          "warnings": []}}
```

The summary is two Schwab reads, not an atomic snapshot: the orders
entered within the last 59 days (Schwab's list horizon; an older working
order is reachable by `get_order` only), then the account with its
positions, in that order so an order that fills in between shows as both
working and held rather than as neither. `balances` are Schwab's current
balances typed, with a cash account's `cashAvailableForTrading` standing in
for `buyingPower` and `availableFunds`, and `inCall` from the projected
block; `pnl` sums the per-position day and open (unrealized, long and
short) figures to cents and is null when no position reports one, and
realized profit and loss is not here: `get_transactions` has the trades.
`positions` is literally the first page of `get_positions`, whose
`nextCursor` continues there. `workingOrders` holds every order Schwab may
still change, children of trigger and one-cancels-other orders flattened
beside their `parentOrderId`, oldest first, and carries no cursor:
`truncated: true` says to list the rest with `get_account_orders`.
`pendingSubmissions` counts this server's own submissions for the account
whose outcome Schwab has not answered, so an order may exist that
`workingOrders` does not show; reconcile before acting on it. Positions and
working orders each get half of `SCHWAB_MCP_RESPONSE_BUDGET_BYTES`. The
account read is essential and fails the call; a failed orders or journal
read leaves that section null and explained in `warnings`, which carry the
failure shape above. The call is read-only and not audited.

## Order status

`get_order_status(account_hash, order_id, wait_ms?)` reduces one order to
what deciding the next step needs (`crates/schwab-mcp/src/order_status.rs`),
so a model neither walks `orderActivityCollection` itself nor polls
`get_order` in a loop:

```json
{"asOf": "…", "source": "schwab_rest",
 "data": {"orderId": "9", "status": "WORKING", "statusDescription": null, "terminal": false,
          "cancelable": true, "editable": true, "tag": "mcp-…",
          "quantity": 10.0, "filled": 4.0, "remaining": 6.0, "avgFillPrice": 150.25, "fills": 1,
          "legs": [{"legId": 1, "instruction": "BUY", "quantity": 10.0, "symbol": "AAPL",
                    "assetType": "EQUITY", "positionEffect": null,
                    "filled": 4.0, "avgFillPrice": 150.25}],
          "enteredTime": "2025-01-15T14:31:22+0000", "closeTime": null,
          "lastUpdate": "2025-01-15T14:32:00+0000",
          "replacedBy": [], "children": [],
          "waited": {"ms": 1004, "reads": 2}}}
```

`terminal` is true when Schwab will not change the order again (filled,
canceled, rejected, expired or replaced); a status this crate does not know
is reported as `UNKNOWN` and treated as not terminal, so a model keeps
reconciling rather than assuming. `filled`, `remaining` and `fills` come
from Schwab's own quantities when it reports them, otherwise from the
execution activities; `avgFillPrice` is quantity-weighted over the fills,
to four decimals, overall and per leg, and counts only executions whose
`executionType` is absent or `FILL` (a `CANCELED` activity carries a zero
price and is ignored). `lastUpdate` is the latest of the order's entered,
release, cancel and close times and its fill times, verbatim in Schwab's
`+0000` form. `replacedBy` lists the ids of orders that replaced this one;
`children` reports each child of a trigger or one-cancels-other order in
the same shape. With `wait_ms` (at most 30000; larger is refused as
`invalid_arguments`) the call re-reads once a second until the order is
terminal or the wait elapses, takes a last read at the deadline, and
reports `waited: {ms, reads}`; without it `waited` is null and the call is
one read. Call again to keep waiting; a working order at the deadline is
not a failure. The call reads the same endpoint as `get_order`, which
keeps Schwab's full shape, and is read-only and not audited.

## Pre-flight

`check_order(account_hash, order, order_id?)` evaluates a `place_order` (or,
with `order_id`, a `replace_order`) intent against everything this server
enforces locally and answers without contacting Schwab:

```json
{"allowed": false,
 "reasons": [{"code": "approval_required", "disposition": "ask_operator", "intentDigest": "…"}],
 "exposure": {"quantity": "2", "notional": "100"},
 "limits": {"maxQuantity": "2", "maxNotional": "100"},
 "requiresApproval": true, "approved": false,
 "intentDigest": "…", "tag": "mcp-…", "tradingEnabled": false}
```

`reasons` are the failures the submission would return, all of them, in the
order a dispatch finds them: argument validity, the principal's tool and
account permissions, risk limits, approval (policy or durable), the trading
switch, and operator halts or a recovery suspension. `intentDigest` is what
an operator approves with `schwab-admin approve`; `tag` is the order tag the
submission would carry. The verdict is advisory: every check is repeated on
submission, and a `duplicate_intent` is only detected there. Nothing is
audited or approved by checking. Grant `check_order` to any principal that
holds `place_order` or `replace_order`; without a principal (the local stdio
transport with trading off) `limits` is null and approval does not apply.
`preview_order` remains the way to obtain Schwab's own validation, fees and
buying-power impact.

## Server status

`get_server_status()` answers, from local evidence, what this server is and
what it will let the caller do right now, so a model can plan a session
instead of discovering the switches one denial at a time:

```json
{"mode": "live", "version": "0.2.0",
 "serverTime": {"rfc3339": "2026-09-10T14:30:00.000Z", "epochMs": 1789137000000},
 "principal": {"name": "desk", "tools": ["check_order", "get_quotes", "place_order"],
               "accounts": ["…"], "allAccounts": false, "maxQuantity": "2",
               "maxNotional": null, "requireApproval": true},
 "trading": {"enabled": true, "cancelEnabled": true, "operatorHalt": null,
             "submissionsSuspended": false, "circuit": "closed", "admissible": true},
 "readiness": {"readsReady": true, "tradingReady": true, "tokensValid": true,
               "storageHealthy": true, "pendingSubmissions": 0,
               "unreconciledAccounts": 0, "oldestBaselineAgeMs": 1200, "circuitOpen": false},
 "requestBudget": {"perMinute": 120, "usedLastMinute": 7, "remaining": 113},
 "stream": {"connected": false, "generation": 0, "lostMessages": 0, "reconciliationRequired": false},
 "equitySession": {"phase": "regular", "date": "2026-09-10", "tradingDay": true,
                   "nextTransition": {"phase": "post_market", "at": "2026-09-10T16:00:00-04:00"},
                   "fetchedAt": "2026-09-10T13:00:05Z"},
 "warnings": []}
```

`mode` is `live`: every order tool reaches Schwab. `principal` is the policy
the session runs as (null on the local stdio transport, where none applies).
`trading.admissible` means a submission would pass the trading switch,
operator halt and recovery suspension right now; validity, limits, approval
and duplicates are still judged per order by `check_order` and on
submission. `readiness` is the body of the `/readyz` probe, evaluated for
the accounts in `SCHWAB_RECOVERY_ACCOUNTS`. `requestBudget` is this process's
own quota (null when rate limiting is disabled). `equitySession` is Schwab's
equity calendar (`pre_market`, `regular`, `post_market`, `closed`), read at
most once per US-Eastern day and looked ahead up to four days for the next
opening; the tool never contacts Schwab for anything else. A section whose
probe failed is null and explained in `warnings`, which carry the failure
shape above; an unauthenticated session yields exactly one, asking the
operator to run OAuth. The call is read-only, not audited, and cheap:
grant it to every principal.

## Remaining implementation work

| Priority | Gap | Completion criteria |
| --- | --- | --- |
| High | Paper mode in MCP | Inject an explicit trading backend into MCP; publish mode in discovery and results; prove paper startup requires no live credentials and cannot reach order endpoints. The existing paper authority/CLI does not make the live MCP wrapper paper-capable. |
| Medium | Bounded model context | Done for candles, option contracts and positions (budgeted, paged projections with explicit truncation and provenance); oversized raw responses are refused with `response_too_large`. Remaining: projections for quotes across many symbols and for order and transaction lists. |
| High | Client interoperability | Exercise initialize, discovery, schemas, structured results, errors, cancellation, token rotation and cross-principal session denial with independent stdio and HTTP clients. Current regression tests are not a complete external conformance certification. |
| Medium | Model evaluation | Version scenarios for stale data, prompt injection, account confusion, denied approvals, repeated submissions and uncertain fills. Require zero unauthorized executions; record model/version/config, not just aggregate success. |
| Medium | Optional resources/prompts | Add account-scoped resources and workflow prompts only with matching lookup/call authorization and output budgets. Do not expose approval-granting tools to the proposing model. |

## TurboMCP v4 decision

The registry reports `4.0.0-alpha.4`, published alongside stable `3.3.0`.
V4 is a prerelease with new provider/lookup, dispatcher and session ownership
contracts. Its [migration guide](https://github.com/Epistates/turbomcp/blob/main/docs/DEPLOYMENT.md)
requires authoritative tool lookup, identity-bound sessions, and coordinated
HTTP session termination. It also validates input and structured output schemas.
The current `McpHandler` policy wrapper, direct core imports, HTTP router and
custom WebSocket dispatch must be ported together. A version-number-only upgrade
would not establish compatibility or preserve our authorization boundary.

Adopt v4 in a dedicated migration with the existing authorization regression
suite plus independent transport conformance, schema, overload, cancellation
and session-revocation tests. Assess whether its OAuth/session facilities can
replace custom plumbing while preserving account risk and approval rules.
No v4 runtime or model-provider integration was added in this pass.
