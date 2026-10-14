# Coming from Schwabdev

This guide maps [Schwabdev](https://github.com/tylerebowers/Schwabdev) 4.0 to `rustyschwab`, the Python package built on this SDK. Every Schwabdev call has an equivalent. The sections below give the equivalent and say where behavior differs. The [Python guide](PYTHON.md) documents each piece in full.

- [At a glance](#at-a-glance)
- [Clients and authorization](#clients-and-authorization)
- [REST calls](#rest-calls)
- [Orders](#orders)
- [Streaming](#streaming)
- [Trader Context](#trader-context)
- [What does not carry over](#what-does-not-carry-over)

## At a glance

| | Schwabdev 4.0 | rustyschwab |
| --- | --- | --- |
| Clients | `Client` (requests) and `ClientAsync` (aiohttp) | `rustyschwab.Client` (asyncio) and `rustyschwab.sync.Client` (blocking), one Rust core under both |
| Return values | `requests.Response` / `aiohttp` response | parsed `dict`/`list`; failures raise typed exceptions (`RateLimited`, `AuthError`, `BrokerError`, ...) |
| Order placement | `POST`, read the order ID from the `Location` header yourself | `{"order_id", "location", "attempt_id", "tag"}`; every order is journaled first, and an order whose outcome is unknown is reconciled by its tag, never resent |
| Retries | urllib3 retry on GET, PUT and DELETE | retries with backoff and jitter, a circuit breaker and a shared per-minute request budget; order submission is never retried blindly |
| Parameter checks | runtime type and enum checks, can be turned off | the same checks in the SDK's types, plus Schwab's period/frequency tables for price history; they cannot be turned off |
| Tokens | SQLite file, optional Fernet encryption, `BEGIN EXCLUSIVE` across processes | macOS Keychain, an encrypted file (ChaCha20-Poly1305) or a plain file, with a cross-process lease for refreshes |
| Auth flow | browser plus pasted URL, or `call_on_auth` | the same (`rustyschwab login`, `call_on_auth`, `rustyschwab.auth`), with OAuth `state` checked; the Rust SDK also has a built-in HTTPS callback server |
| Stream | callbacks on a thread; subscriptions resent after a reconnect | async iterator, blocking iterator or callbacks on a thread; reconnects with replay, gap and loss accounting, named fields |
| Market-hours streaming | `start_auto` at fixed times on fixed weekdays | `rustyschwab.schedule` reads Schwab's calendar, so holidays and early closes are honored; a fixed-time mode remains |
| Stream field names | `stream_fields` translation table | named fields by default; `stream_fields(service)` and `rustyschwab fields SERVICE` print the table |
| Order building | hand-written dicts | `rustyschwab.orders` templates (equities, options, verticals, straddles, strangles, condors, calendars, OCO, triggers, brackets) built and validated by the SDK; `option_symbol` and `parse_option_symbol` |
| Backtest, paper, live | Trader Context (optional package) | `rustyschwab.trader`, plus a [NautilusTrader](PYTHON.md#nautilustrader) adapter |
| Simulated fills | in-process: market at a later bar's open, limits and stops off bar prices | the SDK's paper venue: quote-driven, finite liquidity, latency, slippage, fees, settlement and risk limits, all in decimals, with an audited transcript |
| Performance report | net return, drawdown, Sharpe, FIFO trades, win rate, profit factor | the same and more: Sortino, Calmar, volatility, expectancy, holding time, drawdown duration, per-symbol tables, buy-and-hold comparison |
| LLM agents | none | an [MCP server](MCP_LLM_INTEGRATION.md) with policy, approvals, risk limits, audit and a paper mode |
| Language | Python | Rust SDK with Python bindings; the same behavior in both |

## Clients and authorization

```python
# Schwabdev
import schwabdev
client = schwabdev.Client(app_key, app_secret, callback_url, tokens_db="~/.schwabdev/tokens.db")

# rustyschwab, blocking
from rustyschwab import sync
client = sync.Client.from_env()               # SCHWAB_APP_KEY, SCHWAB_APP_SECRET, SCHWAB_OAUTH_CALLBACK_URL, ...

# rustyschwab, asyncio
import rustyschwab
client = rustyschwab.Client.from_env()
```

| Schwabdev | rustyschwab |
| --- | --- |
| `app_key`, `app_secret`, `callback_url` arguments, or `save_env_global(...)` | `SCHWAB_APP_KEY`, `SCHWAB_APP_SECRET`, `SCHWAB_OAUTH_CALLBACK_URL`, or `Client({"oauth": {...}})` |
| `tokens_db=` | `SCHWAB_TOKENS_FILE` / `{"oauth": {"tokens_file": ...}}` |
| `encryption=<Fernet key>` | `SCHWAB_TOKEN_STORE=encrypted` (the default off macOS) or `keychain` (the default on macOS) |
| `timeout=` | `{"client": {"timeout": 15}}`; `sync.Client(..., timeout=)` bounds a whole call |
| `call_on_auth=` | `sync.Client(..., call_on_auth=)`, or `rustyschwab.auth.authorize_async(client, hook)` |
| `open_browser_for_auth=False` | `rustyschwab login --no-browser` |
| `validate_params=False` | no equivalent: requests are checked by their types |
| `client.update_tokens(...)` | automatic; an expired refresh token raises `AuthError` with `reauthorization_required` set |
| several clients sharing `tokens.db` | several processes sharing `tokens_file`; refreshes are coordinated by a lease, and the order journal and operator halt are shared too |

`rustyschwab login` runs the first authorization from a terminal.

## REST calls

Schwabdev returns a response and you call `.json()`. These return the data. Parameters keep Schwab's names. Where Schwabdev takes them as keyword arguments, `rustyschwab` takes a `dict`.

| Schwabdev | rustyschwab |
| --- | --- |
| `linked_accounts()` | `account_numbers()` |
| `account_details_all(fields="positions")` | `accounts(include_positions=True)` |
| `account_details(hash, fields="positions")` | `account(hash, include_positions=True)` |
| `account_orders(hash, from, to, maxResults, status)` | `account_orders(hash, {"fromEnteredTime": from, "toEnteredTime": to, "maxResults": n, "status": s})` |
| `account_orders_all(from, to, ...)` | `all_orders({...})` |
| `place_order(hash, order)` | `place_order(hash, order)` returns `order_id` |
| `order_details(hash, id)` | `order(hash, id)` |
| `cancel_order(hash, id)` | `cancel_order(hash, id)` |
| `replace_order(hash, id, order)` | `replace_order(hash, id, order)` |
| `preview_order(hash, order)` | `preview_order(hash, order)` |
| `transactions(hash, start, end, types, symbol)` | `transactions(hash, {"startDate": start, "endDate": end, "types": [...], "symbol": s})` |
| `transaction_details(hash, id)` | `transaction(hash, id)` |
| `preferences()` | `user_preferences()` |
| `quotes(symbols, fields, indicative)` | `quotes(symbols, fields=[...], indicative=...)` |
| `quote(symbol, fields)` | `quote(symbol, fields=[...])` |
| `option_chains(symbol, contractType=..., ...)` | `option_chain({"symbol": symbol, "contractType": ..., ...})` |
| `option_expiration_chain(symbol)` | `option_expirations(symbol)` |
| `price_history(symbol, periodType=..., ...)` | `price_history({"symbol": symbol, "periodType": ..., ...})`, or `HistoryStore.load` for a cached range of any length |
| `movers(symbol, sort, frequency)` | `movers(index, sort=..., frequency=...)` |
| `market_hours(symbols, date)` / `market_hour(id, date)` | `market_hours([...], date=...)` |
| `instruments(symbols, projection)` | `instruments(symbol, projection)` |
| `instrument_cusip(cusip)` | `instrument(cusip)` |

Dates and times may be `datetime` objects (timezone-aware), dates or Schwab's strings. Every method has the same signature in `rustyschwab.sync`.

## Orders

Schwabdev orders are dicts, and so are these: anything you send Schwabdev can be sent here. `rustyschwab.orders` builds them for you through the SDK's order builder, which checks them before they leave:

```python
from rustyschwab import orders

order = orders.equity_buy_limit("AAPL", 10, "190.50")
order = orders.bull_call_vertical_open("AAPL  261218C00200000", "AAPL  261218C00210000", 1, net_debit="3.10")
order = orders.bracket(orders.equity_buy_limit("AAPL", 10, "190.50"), take_profit="200", stop_loss="185")
rustyschwab.validate_order(my_hand_written_dict)   # the SDK's checks, offline
```

## Streaming

```python
# Schwabdev
stream = schwabdev.Stream(client)
stream.start(receiver)
stream.send(stream.level_one_equities("AAPL,MSFT", "0,1,2,3"))

# rustyschwab, blocking
stream = client.open_stream()
stream.subscribe("LEVELONE_EQUITIES", ["AAPL", "MSFT"], fields=["bidPrice", "askPrice", "lastPrice"])
stream.start(receiver)            # receiver(message) on a thread, like Schwabdev

# rustyschwab, asyncio
async with await client.open_stream() as stream:
    await stream.subscribe("LEVELONE_EQUITIES", ["AAPL", "MSFT"])
    async for message in stream:
        ...
```

| Schwabdev | rustyschwab |
| --- | --- |
| `stream.level_one_equities(keys, fields)` and the other request builders, then `stream.send(...)` | `stream.subscribe(service, keys, fields=...)` with the service name (`LEVELONE_EQUITIES`, `LEVELONE_OPTIONS`, `LEVELONE_FUTURES`, `LEVELONE_FUTURES_OPTIONS`, `LEVELONE_FOREX`, `NYSE_BOOK`, `NASDAQ_BOOK`, `OPTIONS_BOOK`, `CHART_EQUITY`, `CHART_FUTURES`, `SCREENER_EQUITY`, `SCREENER_OPTION`, `ACCT_ACTIVITY`) |
| `command="UNSUBS"` | `stream.unsubscribe(service, keys)` |
| `command="VIEW"` | `stream.view(service, fields)` |
| `stream.start(receiver, **kwargs)` | `sync` stream `start(receiver, **kwargs)`; async `rustyschwab.streaming.dispatch(stream, receiver)` |
| `stream.start_auto(receiver, start_time, stop_time, on_days)` | `rustyschwab.schedule.run_during_market_hours(client, plan, receiver=...)` (Schwab's calendar) or `run_during_hours(...)` (fixed times, like Schwabdev) |
| `stream.stop()` | `stop()` (thread) / `close()` (connection) |
| `stream.subscriptions` | `stream.subscriptions()` |
| field numbers in messages, `stream_fields` to translate | field names in messages (`named_fields=False` for numbers) |

Schwabdev's `CHART_EQUITY` table corrects Schwab's published field order. The SDK's table makes the same correction.

## Trader Context

`rustyschwab.trader` takes the same strategy shape, `strategy(tc, events)`:

```python
# Schwabdev
from schwabdev import Context
ctx = Context(client)
run = ctx.backtest(strategy, ["AMD"], cash=10_000, history_days=30, plot=True)
session = ctx.deploy(strategy, ["AMD"], cash=10_000)

# rustyschwab
from rustyschwab.trader import Trader
trader = Trader(async_client)
run = await trader.backtest(strategy, ["AMD"], cash=10_000, days=30)
run.to_html("run.html")
session = trader.paper(strategy, ["AMD"], cash=10_000)                     # paper on live data
session = trader.live(strategy, ["AMD"], account_hash=account_hash)          # real orders
result = await session.run()
```

| Schwabdev | rustyschwab |
| --- | --- |
| `Context(client)`, `Context(client, account_hash=...)` | `Trader(client)`; `trader.paper(...)` or `trader.live(..., account_hash=...)` |
| `ctx.backtest(..., history_days=30)` | `trader.backtest(..., days=30)` or `start=` and `end=`; `minutes=` 1, 5, 10, 15 or 30 |
| `ctx.deploy(...)` | `trader.paper(...)` / `trader.live(...)`, then `await session.run()`; `session.stop()` ends it |
| `ctx.record(...)` | `trader.record(...)` |
| `chart=`, `level1=`, `level2=` | the same names |
| `Costs(per_share, pct, half_spread, slip, sec_fee, taf_per_share)` | `Simulation(fee_per_share, fee_bps, spread_bps, slippage_bps, sell_fee_bps, sell_fee_per_share, sell_fee_cap)` and the liquidity a bar offers, `participation_bps` |
| `fill_delay=2` | `execution_latency_ms`; a backtest order fills on the next bar at the earliest |
| `report=True` / `run.report()` | `run.report()` |
| `run.stats["net_return"]`, `["max_drawdown"]`, `["sharpe"]`, `["realized_pnl"]`, `["fees_paid"]`, `["uniform_b&h"]` | `run.stats["net_return"]`, `["max_drawdown"]`, `["sharpe"]`, `["realized_pnl"]`, `["fees"]`, `["benchmark_return"]` |
| per-ticker `win_rate`, `profit_factor`, `trades` | `run.stats["symbols"][symbol]["trades"]` |
| `run.equity` | `run.equity` |
| `plot=True`, `run.serve()`, `run.plot()` | `run.to_html(path)`; for a running session, `session.snapshot().to_html(path, refresh_seconds=5)` |
| event types `"c"`, `"l1"`, `"l2"` | `"candle"`, `"quote"`, `"book"`, and `"fill"` for the strategy's own executions |
| `tc.candles`, `tc.quotes`, `tc.books`, `tc.positions`, `tc.cash`, `tc.sellable(sym)`, `tc.orders`, `tc.starting_cash`, `tc.plots` | the same names |
| `tc.portfolio_value()` | `tc.equity()` |
| `tc.order(order_dict)` returns an ID | `tc.order(order)` returns the order's tag; `tc.orders[tag]` follows it |
| `tc.cancel(order_id)` | `tc.cancel(tag)` |

Behavior that differs:

- **Fills come from a quote-driven venue, not bar prices.** Each bar becomes four quotes with a spread and a share of the bar's volume. An order fills against those quotes, only up to the size on offer, after its latency, and never at its own limit price unless the market reaches it. Partial fills happen. Limit, stop and stop-limit orders, DAY and GTC durations, and market-order price protection behave as on the live venue model.
- **Accounting is in decimals and is checked.** Buys reserve cash, sells need shares, sale proceeds can be made to settle on a calendar, and orders the account cannot pay for are rejected, not filled. Schwabdev's `tc.cash` is advisory.
- **Runs are reproducible and auditable.** The same history and strategy give the same fills. `keep_state=` keeps the venue's state file with an audited transcript.
- **Live trading uses the journaled order path.** Risk limits, operator halts, the request budget and unknown-outcome reconciliation all apply.
- **Warm-up.** `warmup_days=` loads recent history into `tc.candles` before a live session's first bar.

## What does not carry over

- **Raw responses.** Methods return data, not response objects, and errors raise. `client.get_json(path, query)` reads any GET endpoint as JSON.
- **Switching validation off.** Requests are validated by type.
- **Schwabdev's `parsed=` flag.** Results are always parsed.
- **The live-updating browser viewer.** Charts are static HTML files. A file rewritten from `session.snapshot()` with `refresh_seconds` reloads itself.
- **Multi-leg orders in the strategy runner.** The runner tracks single-leg orders. Place multi-leg orders through the client.
- **Options and futures in the simulated venue.** Paper and backtest fills are for long US equities. Live trading through the client supports whatever Schwab accepts.
