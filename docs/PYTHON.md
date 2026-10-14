# Python bindings

`rustyschwab` is the Rust SDK for Python: an asyncio-native client for Schwab's Trader and Market Data APIs, the streamer, and the offline paper venue. Requests and responses are plain `dict` and `list` values in Schwab's JSON shape. The Rust code does the work, so the behavior documented for the SDK holds here too: orders go through the execution journal, risk checks and operator halts apply, the request budget is shared, and an uncertain submission is never retried.

> [!WARNING]
> Automated trading involves significant risk of financial loss. Test against the [paper venue](#paper-trading-and-backtests) first. The paper venue never reaches Schwab or thinkorswim paperMoney.

- [Install](#install)
- [Authorization](#authorization)
- [Configuration](#configuration)
- [Market data and accounts](#market-data-and-accounts)
- [Orders](#orders)
- [Streaming](#streaming)
- [Synchronous use](#synchronous-use)
- [Callbacks and market-hours scheduling](#callbacks-and-market-hours-scheduling)
- [Command line](#command-line)
- [Strategies: backtest, paper and live](#strategies-backtest-paper-and-live)
- [Paper trading and backtests](#paper-trading-and-backtests)
- [NautilusTrader](#nautilustrader)
- [Error handling](#error-handling)
- [How asyncio and the runtime work](#how-asyncio-and-the-runtime-work)
- [Building from source](#building-from-source)

## Install

```sh
pip install rustyschwab
```

Wheels are abi3: one wheel per platform (Linux x86_64 and aarch64, macOS arm64 and x86_64, Windows x86_64) works on CPython 3.10 and every later release. The package is typed (`py.typed`, with stubs for the native module).

## Authorization

Schwab issues a 30-minute access token and a 7-day refresh token. The client refreshes the access token itself. Only the refresh token needs a person: once it expires, or Schwab revokes it, calls raise `AuthError` with `reauthorization_required` set.

1. Register an app in the [Schwab developer portal](https://developer.schwab.com/) with an HTTPS callback URL, as in the [setup guide](SETUP_GUIDE.md). `https://127.0.0.1:8080` works; nothing has to listen there.
2. Put the credentials in the environment (a `.env` loaded by your own code works too):

   ```sh
   export SCHWAB_APP_KEY=...
   export SCHWAB_APP_SECRET=...
   export SCHWAB_OAUTH_CALLBACK_URL=https://127.0.0.1:8080
   export SCHWAB_TOKEN_STORE=encrypted   # keychain | encrypted | file
   export SCHWAB_TOKENS_FILE=~/.config/schwab/tokens.json
   ```

   The default store is the Keychain on macOS and an encrypted file elsewhere. [README](../README.md#environment-variables) lists every variable.
3. Authorize once:

   ```python
   import asyncio
   import rustyschwab

   async def main() -> None:
       client = rustyschwab.Client.from_env()
       print(await client.authorization_url())
       # Open the URL, sign in and approve. The browser then fails to load the
       # callback page; copy the full address from the address bar.
       redirect = input("Redirect URL: ")
       await client.complete_authorization(redirect)

   asyncio.run(main())
   ```

   `complete_authorization` checks the OAuth `state` against the one `authorization_url` issued, exchanges the code and stores the tokens. A bare `code` value works if the authorization was started from the same token file. The Rust `oauth-flow` example does the same with a built-in HTTPS callback server.

`rustyschwab login` runs this flow from the [command line](#command-line). In code, `rustyschwab.auth` has the pieces: `terminal_prompt()` is a hook that prints the URL, opens the browser and reads the pasted address; `authorize_async(client, hook)` runs the flow on an async client, and `ensure_authorized(client, hook)` runs it only when the stored tokens need a person. A hook is any function from the consent URL to the redirect URL, so it can ask somewhere other than a terminal. The blocking client takes one as `call_on_auth`, see [Synchronous use](#synchronous-use).

## Configuration

`Client.from_env()` reads the same `SCHWAB_*` variables as the Rust SDK. `Client(config)` takes a dict instead, laid over the SDK defaults, and `Client.from_env(overrides)` lays a dict over the environment. The dict has the shape of the SDK's `SchwabConfig`:

```python
client = rustyschwab.Client({
    "oauth": {
        "app_key": "...",
        "app_secret": "...",
        "callback_url": "https://127.0.0.1:8080",
        "tokens_file": "/var/lib/trader/tokens.json",   # str or os.PathLike
        "token_store_kind": "encrypted_file",            # file | encrypted_file | keychain
    },
    "client": {
        "timeout": 15,                                   # seconds
        "rate_limit": {"enabled": True, "requests_per_second": 2, "burst_size": 10},
        "retry": {"max_retries": 3, "initial_backoff": 0.1, "max_backoff": 10},
    },
    "streaming": {"max_subscriptions": 200},             # None turns streaming off
})
```

Only the keys you give change. Durations are seconds (`15`, `0.25`) or the SDK's strings (`"30s"`, `"250ms"`). An unknown key is a `ValidationError` naming it, so a misspelled `rate_limt` cannot run with the defaults. Base URLs must be Schwab's or loopback, as in Rust.

The order journal and the operator halt live beside `tokens_file`, exactly as for the Rust SDK; processes that share a token file share them.

## Market data and accounts

Every I/O method is a coroutine. Responses are Schwab's JSON.

```python
quotes = await client.quotes(["AAPL", "MSFT"], fields=["quote"])
print(quotes["AAPL"]["quote"]["lastPrice"])

candles = await client.price_history({
    "symbol": "AAPL", "periodType": "day", "period": 10,
    "frequencyType": "minute", "frequency": 5,
})

chain = await client.option_chain({
    "symbol": "AAPL", "contractType": "CALL", "strikeCount": 6,
    "fromDate": datetime.date(2026, 11, 1),
})

accounts = await client.account_numbers()              # [{"accountNumber", "hashValue"}]
account_hash = accounts[0]["hashValue"]
account = await client.account(account_hash, include_positions=True)
```

Also available: `quote`, `option_expirations`, `movers`, `market_hours`, `instruments`, `instrument`, `accounts`, `user_preferences`, `account_orders`, `all_orders`, `order`, `transactions`, `transaction`, and `get_json(path, query)` for any other read-only path. Account calls take the account's `hashValue`, never the account number; an all-digit value is refused before anything is sent.

Parameters use Schwab's query names. Beyond JSON types, request values may be `datetime.date`, timezone-aware `datetime.datetime` (a naive one is refused, not guessed to be UTC), `decimal.Decimal`, `enum.Enum` and `os.PathLike`. Order windows look like this:

```python
from datetime import datetime, timedelta, timezone

now = datetime.now(timezone.utc)
orders = await client.account_orders(account_hash, {
    "fromEnteredTime": now - timedelta(days=7),
    "toEnteredTime": now,
    "status": "FILLED",
})
```

Schwab lists only the last 60 days of orders.

## Orders

An order is a dict in Schwab's JSON shape, validated by the SDK before anything is sent. A misspelled field is a `ValidationError`, because the SDK rejects unknown order fields. Prices may be strings, `Decimal`, `int` or `float`; strings and `Decimal` keep every digit.

```python
order = {
    "orderStrategyType": "SINGLE",
    "orderType": "LIMIT",
    "session": "NORMAL",
    "duration": "DAY",
    "price": "189.50",
    "orderLegCollection": [{
        "instruction": "BUY",
        "quantity": 10,
        "instrument": {"symbol": "AAPL", "assetType": "EQUITY"},
    }],
}

preview = await client.preview_order(account_hash, order)    # Schwab's checks, nothing placed
placed = await client.place_order(account_hash, order)
# {"order_id": "1005...", "location": "https://...", "attempt_id": "...", "tag": "rs-..."}

replaced = await client.replace_order(account_hash, placed["order_id"], {**order, "price": "189.75"})
await client.cancel_order(account_hash, replaced["order_id"])
```

[ORDERS.md](ORDERS.md) covers options, multi-leg and composite orders; the JSON is the same.

### Building orders

You do not have to write that dict. `rustyschwab.orders` has a template for each common order, and every one is built by the SDK's own order builder: the template describes the order as an `OrderSpec` (the JSON behind the MCP `build_order` tool, see [ORDERS.md](ORDERS.md#json-specs-and-the-mcp-build_order-tool)), the Rust constructors expand it, and `validate()` runs on the result. A Python order has the same shape and the same checks as one made in Rust, and a bad one raises `ValidationError` before anything is sent.

```python
from rustyschwab import orders, option_symbol

buy = orders.equity_buy_limit("AAPL", 10, "189.50", duration="GOOD_TILL_CANCEL", tag="swing-1")
protected = orders.bracket(buy, take_profit="200", stop_loss="180")

long_call = option_symbol("AAPL", "2026-12-18", "CALL", 190)    # "AAPL  261218C00190000"
short_call = option_symbol("AAPL", "2026-12-18", "CALL", 195)
spread = orders.bull_call_vertical_open(long_call, short_call, 2, "1.20")   # NET_DEBIT, VERTICAL

await client.place_order(account_hash, protected)
```

Every template returns a plain `dict`, so the result goes to `place_order`, `preview_order` or `PaperBroker.place_order` unchanged, and `json.dumps` works on it.

| Family | Templates |
|---|---|
| Equities | `equity_{buy,sell,sell_short,buy_to_cover}_` + `market`, `limit`, `stop`, `stop_limit`, `trailing_stop` |
| Single options | `option_{buy_to_open,sell_to_open,buy_to_close,sell_to_close}_` + `market`, `limit` |
| Vertical spreads | `{bull_call,bear_call,bull_put,bear_put}_vertical_` + `open`, `close` |
| Other strategies | `long_straddle_*`, `short_straddle_*`, `long_strangle_*`, `short_strangle_*`, `iron_condor_*`, `long_call_calendar_*`, `long_put_calendar_*`, each with `open` and `close` |
| Composites | `one_cancels_other(a, b)`, `first_triggers_second(a, b)`, `bracket(entry, take_profit, stop_loss)` |

Names and argument order follow schwab-py where it has a template: `equity_buy_limit(symbol, quantity, price)`, `bull_call_vertical_open(long_call_symbol, short_call_symbol, quantity, net_debit)`. The spread templates take the price as `net_debit` or `net_credit`, whichever the strategy pays when it is opened or closed, so a bull call spread opens for a debit and closes for a credit and a bull put spread the reverse. They also check that the two symbols are the right kind of option on one underlying and expiration, with the strikes the right way round for the strategy, and refuse the order otherwise. Trailing stops take exactly one of `amount=` (a price difference) and `percent=`, and `basis=` (default `"LAST"`).

Every template accepts the same keywords, typed as `orders.OrderOptions`: `session`, `duration`, `tag`, `special_instruction`, `destination`, `tax_lot_method` and `stop_type`. The defaults are the SDK's, the `NORMAL` session and `DAY` duration. Extended-hours sessions take limit orders only, and options trade only in `NORMAL`.

**Amounts.** Quantities and prices may be `str`, `int`, `Decimal` or `float`, the same as a price in a hand-written order. A `str` or `Decimal` keeps every digit; a `float` is read as its shortest round-trip spelling, so `189.5` is `"189.5"` but `0.1 + 0.2` is `"0.30000000000000004"`. Build computed prices from `str` or `Decimal`. A `bool`, `NaN`, exponent form such as `"1e3"` and padded or underscored strings are refused.

**Composites.** They take any finished order, from a template or written by hand, and call the SDK's `one_cancels_other`, `triggers` and `bracket`:

```python
exits = orders.one_cancels_other(
    orders.equity_sell_limit("XYZ", 5, "15.27"),
    orders.equity_sell_stop_limit("XYZ", 5, "11.30", "11.27"),
)
staged = orders.first_triggers_second(orders.equity_buy_limit("XYZ", 5, "14.97"), exits)
```

`bracket` needs a single-leg order that opens a position and exits that sit on the right sides of its price; it adds a good-till-cancel take-profit limit and stop-loss as an `OCO` pair that closes the entry's full quantity.

**Option symbols.** `option_symbol(underlying, expiration, put_call, strike)` builds the 21-character OSI symbol Schwab uses, from a `datetime.date` or `"YYYY-MM-DD"`, `"CALL"` or `"PUT"`, and a `str`, `int` or `Decimal` strike. `parse_option_symbol(symbol)` is the inverse and returns `{"underlying", "expiration", "put_call", "strike"}`, with the strike as a decimal string. Both raise `ValidationError` for what the format cannot carry (a root over six characters, a strike finer than 0.001) and `parse_option_symbol` accepts only the canonical spelling.

**Your own specs and dicts.** `rustyschwab.build_order(spec)` takes any `OrderSpec` dict, so a spec written for the MCP `build_order` tool builds the same order in Python. `rustyschwab.validate_order(order)` runs the SDK's order checks on a hand-written dict offline, naming the failing field in `ValidationError.field`, which is what `place_order` does before it sends. It does not know Schwab's own rules (buying power, eligibility, tick sizes); `preview_order` does.

```python
try:
    rustyschwab.validate_order({**order, "stopPrice": "180"})
except rustyschwab.ValidationError as error:
    print(error.field, error)     # stopPrice  Invalid parameter: LIMIT does not take stopPrice
```

**The result.** `place_order` and `replace_order` return `order_id` (`None` if Schwab accepted the order without a `Location` header), `location`, `tag` and `attempt_id`. The tag is yours if the order carried one, and a generated `rs-<uuid>` otherwise. `attempt_id` is the execution-journal record of this submission, the same ID a `SubmissionUnknown` error carries, so store it with your own order record.

**Uncertain outcomes.** If a request may have reached Schwab but no answer came back (a timeout, a 5xx, a dropped connection), the call raises `SubmissionUnknown` and never retries. The journal then holds the attempt, and the next submission on that account raises `ReconciliationRequired` until it is resolved. Resolve it by reading the account's orders, not by resending:

```python
try:
    placed = await client.place_order(account_hash, order)
except rustyschwab.SubmissionUnknown as error:
    open_orders = await client.account_orders(account_hash, window)   # as in the previous section
    # Look for the order by the tag you sent. `schwab-admin reconcile` then
    # resolves error.attempt_id in the journal.
    raise
```

**Controls.** `client.halt(reason, operator)` stops new submissions for every client sharing the journal; cancellations still work. `client.clear_halt(operator)` lifts it and `client.halted()` reads it. `client.readiness(account_hashes)` reports reads and trading readiness from local evidence. `client.request_budget()` and `client.circuit_state()` are synchronous.

A client built from Python has no risk provider; portfolio limits, approvals and principals are configured through the Rust gateway ([OPERATIONS.md](OPERATIONS.md)). The paper venue enforces its own limits.

## Streaming

```python
stream = await client.open_stream()                 # logs in with the user-preference identity
await stream.subscribe("LEVELONE_EQUITIES", ["AAPL", "MSFT"], fields=["bidPrice", "askPrice", "lastPrice"])
await stream.subscribe("ACCT_ACTIVITY", ["Account Activity"])

async for message in stream:
    match message["type"]:
        case "data":
            for item in message["data"]:
                for content in item["content"]:
                    print(item["service"], content["key"], content.get("bidPrice"))
        case "state":
            if not message["ready"] or message["lost_messages"]:
                ...  # a gap: re-baseline from REST before trusting new data
        case _:
            pass

await stream.close()
```

`open_stream` also works as `async with await client.open_stream() as stream:`. The `Stream` is an async iterator; its messages are dicts with a `type`:

| `type` | Content |
| --- | --- |
| `data` | `{"data": [{"service", "timestamp", "command", "content": [...]}]}` |
| `response` | The streamer's answer to a login or subscription request |
| `notify` | Heartbeats and notices |
| `state` | `ready`, `generation`, `lost_messages` and `ended` once the stream stopped for good |

The SDK reconnects, replays subscriptions and reports every gap itself. A `state` message arrives whenever readiness or the generation changes, and data from before it is stale, so re-read what you track. A slow consumer does not stall the connection: a full queue drops frames, counts them in `lost_messages` and resumes under a new generation ([STREAMING.md](STREAMING.md) has the backpressure rules). After a `state` with `ended` (`{"reason": "disconnected" | "replaced" | "idle" | "login_denied" | ...}`) the iterator finishes.

Content fields arrive under their names (`bidPrice`) by default. Pass `named_fields=False` for the streamer's field numbers, and use `rustyschwab.stream_fields("LEVELONE_EQUITIES")` for the number-to-name table. `fields=` on `subscribe` and `view` takes numbers or names. Services are the streamer's: `LEVELONE_EQUITIES`, `LEVELONE_OPTIONS`, `LEVELONE_FUTURES`, `LEVELONE_FUTURES_OPTIONS`, `LEVELONE_FOREX`, `NYSE_BOOK`, `NASDAQ_BOOK`, `OPTIONS_BOOK`, `CHART_EQUITY`, `CHART_FUTURES`, `SCREENER_EQUITY`, `SCREENER_OPTION`, `ACCT_ACTIVITY`.

Schwab allows one streaming session per user. Opening a second one ends the first with `ended: {"reason": "replaced"}`.

`stream.subscriptions()`, `stream.status()` and `stream.is_authenticated` read the session; `asyncio.wait_for(anext(stream), timeout)` waits with a deadline and leaves the stream intact on timeout.

## Synchronous use

`rustyschwab.sync` is the same client with blocking methods, for scripts, notebooks and threaded programs that do not use asyncio. Every method of `rustyschwab.Client` has a twin with the same arguments and result, and a test fails if one is added without it.

```python
from rustyschwab import sync

with sync.Client.from_env() as client:
    print(client.quote("AAPL")["quote"]["lastPrice"])
    placed = client.place_order(account_hash, order)

    stream = client.open_stream()
    stream.subscribe("LEVELONE_EQUITIES", ["AAPL", "MSFT"], fields=["bidPrice", "askPrice"])

    def receiver(message, *, label):
        if message["type"] == "data":
            print(label, message["data"][0]["content"])

    stream.start(receiver, label="quotes")          # a daemon thread calls receiver(message, label="quotes")
    ...
    stream.stop()                                    # stop the thread; the connection and subscriptions stay
    stream.close()                                   # log out and disconnect
```

**Streams.** `sync.Stream` has `subscribe`, `unsubscribe`, `view`, `subscriptions`, `status` and `close`, and reads three ways. `for message in stream` blocks for each message and ends with the final `state` message. `stream.recv(timeout)` returns the next message, `None` once the stream has ended, or raises `TimeoutError` and leaves the stream usable. `stream.start(receiver, *, daemon=True, on_error="log", **kwargs)` calls `receiver(message, **kwargs)` on a background thread, in order, until `stop()`. Only one reader at a time: a started stream refuses `recv` and iteration. `stop()` is idempotent and joins the thread; `start()` may be called again afterwards, and messages that arrive in between queue up to the SDK's limit and are counted in `lost_messages` beyond it. A receiver must be a plain function. For a coroutine, use the async client with `rustyschwab.streaming.dispatch`.

**Errors in the receiver.** With `on_error="log"` an exception is logged to `rustyschwab.sync` and the next message is delivered, so one bad tick does not end the stream. With `on_error="raise"` the thread stops, `stream.error` holds the exception, and the next `stop()` or `join()` raises it once.

**One event loop thread.** Calls wait on one private asyncio event loop that runs on a daemon thread named `rustyschwab-sync`. It is created by the first blocking call and shared by every client, stream and schedule in the process, so any number of threads can call at once. It stops at interpreter exit, which first stops started streams and schedules, or earlier with `rustyschwab.sync.shutdown()`; the next call starts a new one. It is not inherited across `fork`. Do not call blocking methods from inside a running event loop of your own, which they would block, nor from a receiver on the loop thread, which raises `RuntimeError` instead of deadlocking.

**Timeouts.** `sync.Client(config, timeout=...)` bounds how long each call waits and raises `TimeoutError` after cancelling it. The default is no limit beyond the SDK's own request timeouts. As with a cancelled coroutine, a timed-out `place_order` still leaves its journal record resolved.

**Reauthorization.** `sync.Client(..., call_on_auth=hook)` calls `hook(consent_url)` when a call fails because the refresh token is missing or expired, completes the authorization with the redirect URL it returns, and retries the call once. `rustyschwab.auth.terminal_prompt()` is the terminal flow: print the URL, open the browser, read the pasted address. Several threads waiting on a refresh share one prompt. Order placement, replacement and cancellation never prompt or retry this way; call `client.authorize()` first (it runs the flow explicitly and returns whether it completed).

```python
client = sync.Client.from_env(call_on_auth=rustyschwab.auth.terminal_prompt())
client.account_numbers()      # asks for a new login only if one is needed
```

`client.native` and `stream.native` give the async objects, for use on a loop of your own.

## Callbacks and market-hours scheduling

**Callbacks on the async stream.** `rustyschwab.streaming.dispatch(stream, receiver, *, on_error="log", **kwargs)` reads the stream and calls `receiver(message, **kwargs)` for each message until it ends. `receiver` may be a function or a coroutine function; a coroutine is awaited before the next read, which keeps the SDK's backpressure accounting honest. `streaming.start(...)` runs it as a task you can cancel. `on_error` is as for the blocking stream.

```python
from rustyschwab import streaming

async with await client.open_stream() as stream:
    await stream.subscribe("LEVELONE_EQUITIES", ["AAPL"])
    await streaming.dispatch(stream, handle_message)
```

**Only during the market's sessions.** Schwabdev's `start_auto` runs the stream between fixed clock times on fixed weekdays. `rustyschwab.schedule` asks Schwab's market-hours calendar (`client.market_hours`, one request a day) for the real session windows instead, so a holiday has no session and an early close ends the stream at 13:00.

```python
from datetime import timedelta
from rustyschwab import schedule

plan = schedule.Subscriptions()
plan.add("LEVELONE_EQUITIES", ["AAPL", "MSFT"], fields=["bidPrice", "askPrice"])
plan.add("ACCT_ACTIVITY", ["Account Activity"])

async def receiver(message):
    ...

await schedule.run_during_market_hours(client, plan, receiver=receiver)   # until cancelled
```

It sleeps until `lead` (default one minute) before the next window opens, opens a stream with `plan(client)`, hands every message to the receiver, closes the stream when the window ends, sleeps until the next session and repeats. Each session opens a new stream, so what to subscribe to is a `Subscriptions` (replayed every time) or any async function that takes the client and returns an open, subscribed stream. Cancelling the task closes the open stream.

| Parameter | Meaning |
| --- | --- |
| `market` | `"equity"` (default), `"option"`, `"future"`, `"bond"` or `"forex"` |
| `sessions` | Which calendar sessions count: `("regularMarket",)` by default, or `"preMarket"`, `"postMarket"`. Touching sessions form one window, so the stream stays open across them |
| `product` | Keep one product code, such as `"EQ"`; by default every product of the market counts |
| `lead` | How long before the window to open (default one minute) |
| `tz` | Which day is "today" (default `America/New_York`) |
| `on_error` | `"log"` or `"raise"`; `"raise"` closes the stream and ends the schedule |
| `max_sessions` | Stop after this many sessions and return the count (default: run until cancelled) |
| `clock`, `sleep` | Replace the wall clock and `asyncio.sleep`, for tests |

It is built to run unattended. If the calendar cannot be read, it logs the failure to `rustyschwab.schedule` and retries after 5 seconds, doubling up to 5 minutes. A stream that cannot be opened, or that ends for good before the window does (for example `ended: {"reason": "idle"}`), is reopened the same way. After a long wait it re-reads its clock at least hourly, so a suspended laptop catches up. The SDK still reconnects inside a session on its own. Starting in the middle of a session opens at once.

The fixed-window mode is there for parity: `schedule.run_during_hours(client, plan, receiver=receiver, start_time=time(9, 29), stop_time=time(16, 0), on_days=(0, 1, 2, 3, 4), tz="America/New_York")` takes Schwabdev's arguments and defaults. It opens on holidays and runs past an early close, which is why the calendar version is the one to use.

**Blocking form.** `sync.Client.run_during_market_hours(plan, receiver, ...)` and `run_during_hours(...)` take the same parameters, return a `Schedule` at once and run in the background. The receiver is a plain function that a worker thread calls, in order. `plan` may also be a function that takes the `sync.Client` and returns an open, subscribed `sync.Stream`.

```python
from rustyschwab import schedule, sync

client = sync.Client.from_env()
plan = schedule.Subscriptions().add("LEVELONE_EQUITIES", ["AAPL"])
running = client.run_during_market_hours(plan, print)
...
running.stop()          # closes the stream; idempotent
# running.join() waits for it to end by itself (only with max_sessions) and raises what stopped it
```

## Command line

Installing the package adds a `rustyschwab` command (also `python -m rustyschwab`). It uses the standard library only.

```sh
rustyschwab login                    # authorize once and store the tokens
rustyschwab login --no-browser       # print the URL but do not open a browser
rustyschwab login --redirect-url 'https://127.0.0.1:8080/?code=...&state=...'
rustyschwab login --config overrides.json
rustyschwab fields LEVELONE_EQUITIES
rustyschwab fields CHART_EQUITY --json
```

`login` builds a client from the `SCHWAB_*` environment, as `Client.from_env()` does, with a JSON object from `--config` laid over it if given. It prints the consent URL, opens it in a browser, and reads the address Schwab redirects to (or just its `code`) from the terminal, or takes it from `--redirect-url`. It then completes the authorization and says so. It never prints tokens or the code it was given; errors print as `error: ...` and exit with status 1. Pasting the address ends the flow even though the browser shows an error page for it.

`fields SERVICE` prints the field numbers and names of a streamer service from the SDK's own table, the same one `rustyschwab.stream_fields(service)` returns, with nested tables (the order books' levels) indented. `--json` prints that list as JSON.

## Strategies: backtest, paper and live

`rustyschwab.trader` runs one strategy in three modes: a backtest over cached price history, paper trading on live data, and live trading on an account. The strategy code is the same in all three.

```python
import asyncio
from rustyschwab import Client, orders
from rustyschwab.trader import Trader

def crossover(tc, events):
    for event in events:
        if event["type"] != "candle":
            continue
        symbol = event["symbol"]
        closes = [c["close"] for c in tc.candles[symbol][-30:]]
        if len(closes) < 30 or tc.working(symbol):
            continue
        fast, slow = sum(closes[-10:]) / 10, sum(closes) / 30
        tc.plot(f"fast {symbol}", fast)
        held = tc.positions.get(symbol, 0)
        if fast > slow and not held:
            tc.order(orders.equity_buy_market(symbol, 10))
        elif fast < slow and held:
            tc.order(orders.equity_sell_market(symbol, held))

async def main():
    trader = Trader(Client.from_env())
    run = await trader.backtest(crossover, ["AMD", "INTC"], days=20, cash=10_000)
    print(run.report())
    run.to_html("crossover.html")

asyncio.run(main())
```

### The strategy

A strategy is any callable `strategy(tc, events)`, plain or `async`. An object may also define `on_start(tc)`, `on_fill(tc, fill)` and `on_stop(tc)`.

`events` is what is new since the last call, in time order. Each is a `dict` with a `type`:

| `type` | When | Fields |
| --- | --- | --- |
| `candle` | a bar completed | `symbol`, `time` (bar open, epoch ms), `open`, `high`, `low`, `close`, `volume` |
| `quote` | a level-one update, if `level1=True` | `symbol`, `time`, and the fields that changed: `bid`, `ask`, `last`, `bid_size`, `ask_size`, `last_size`, `volume` |
| `book` | a book snapshot, if `level2=True` | `symbol`, `time`, `bids`, `asks` (Schwab's price levels) |
| `fill` | one of the strategy's orders executed | `symbol`, `time`, `tag`, `order_id`, `side`, `quantity`, `price`, `fee` |

`tc` is a `Context`:

| Read | |
| --- | --- |
| `tc.candles[symbol]` | the bars so far, latest last (`max_candles=` bounds the history kept) |
| `tc.quotes[symbol]` | the latest level-one quote, merged from the updates |
| `tc.books[symbol]` | the latest book snapshot |
| `tc.positions[symbol]` | shares held (`Decimal`; absent when flat) |
| `tc.cash` | cash the account can spend now, less what open buys set aside |
| `tc.sellable(symbol)` | shares held less those committed to open sells |
| `tc.equity()` | all cash plus positions at their latest price |
| `tc.orders[tag]` | every order placed, as a `TrackedOrder` (`status`, `filled_quantity`, `average_price`, `fees`, `fills`, `reason`) |
| `tc.working(symbol=None)` | the open orders |
| `tc.now`, `tc.mode`, `tc.state` | the time (epoch ms), the mode, and a dict for the strategy's own state |

| Act | |
| --- | --- |
| `tc.order(order)` | place an order (Schwab's shape; the `rustyschwab.orders` templates build them) and get its tag back |
| `tc.cancel(tag)` | cancel an open order; `False` if it is not open |
| `tc.plot(name, value)` | add a point to an overlay; a name containing a symbol is drawn on that symbol's chart |

Orders placed during a call are sent in order when the strategy returns. `tc.orders[tag]` follows each from `PENDING` to `WORKING`, `FILLED`, `CANCELED`, `EXPIRED` or `REJECTED` (with the venue's `reason`). A rejection does not stop the run. The runner tracks single-leg orders; multi-leg orders go through the `Client` directly.

### Backtests

```python
run = await trader.backtest(strategy, ["AMD"], start=date(2026, 9, 1), end=date(2026, 9, 30),
                            minutes=1, cash=10_000, simulation=Simulation(fee_per_share="0.005"))
```

- **History.** Candles come from a `HistoryStore` cache (default `~/.rustyschwab/history.sqlite3`). The first run fetches what the cache lacks, in ten-day chunks; later runs over the same window make no request. Bars before today in New York are kept for good. `Trader(None)` backtests from the cache alone.
- **Fills.** Each bar becomes four quotes (open, low and high in the order the bar most likely traded, close) with a synthetic spread and a share of the bar's volume on offer, under the declared `candle-bbo-v1` model. These go through the same paper venue as everything else, so fills have latency, finite liquidity, slippage, fees and settlement, in decimals. The strategy sees a bar after its four quotes have printed, so an order fills no earlier than the next bar. Each New York date is a session; `DAY` orders expire at its end.
- **Costs and the market model** are a `Simulation`: `spread_bps` (default 2), `participation_bps` (share of bar volume offered, default 1000 = 10%), `slippage_bps`, `fee_per_share`, `execution_latency_ms`, `cancel_latency_ms`, `market_collar_bps` and `regular_hours_only` (default on).
- **Recorded data.** `level1=True` and `level2=True` replay quotes and books captured with `trader.record` as events. Fills still come from the bars.
- **Determinism.** The same history, strategy and `Simulation` give the same fills. `keep_state="dir"` keeps the venue's state file, whose audited transcript `schwab-paper-verify` can check.
- **Speed.** A 30-session, three-symbol, one-minute backtest (11,700 bars, 140,000 venue events, every one recorded and audited) takes about 15 seconds on a laptop.

`backtest` returns a `RunResult`: `stats` (the [performance report](#performance-reports): return, drawdown, Sharpe, Sortino, Calmar, buy and hold, round trips, win rate, profit factor, per symbol), `fills`, `orders`, `equity` (one point per bar), `plots`, `candles`, `rejected`, `report()` for the text summary, and `to_html(path)`, which writes the price charts with fills and overlays and the equity curve to one HTML file with no scripts and no network.

### Paper trading and live trading

```python
session = trader.paper(strategy, ["AMD"], cash=10_000)            # nothing reaches Schwab
task = asyncio.create_task(session.run())
...
session.stop()
result = await task                                               # a RunResult for the session

session = trader.live(strategy, ["AMD"], account_hash=ACCOUNT_HASH)   # real orders
```

`paper` feeds streamed level-one quotes to a paper venue on the system clock, and opens and closes its session with the market's real hours, so holidays and early closes are honoured. `level1` only decides whether quotes also wake the strategy. `size_multiplier` converts streamed sizes to shares if needed, and `state=` keeps the venue between runs.

`live` sends orders to the account through the journaled order path: the client's risk limits, halts and request budget apply, and an order whose outcome is unknown is reconciled by its tag, never resent. It subscribes to account activity and reads the open orders on each message and every `poll_seconds`. Start with small sizes.

Both stop on `session.stop()`, `run(until=datetime)` or cancellation, then run `on_stop` and return a `RunResult`. Schwab allows one streaming session per login, so do not open another stream (or the NautilusTrader data client) on the same login at the same time.

### Recording quotes and books

Schwab has no history API for quotes or books. `trader.record` writes them to the cache as they stream, so backtests can replay them:

```python
await trader.record(["AMD", "INTC"], chart=True, level1=True, level2=True)
```

With `market_hours=True` (the default) the stream opens and closes with the market's real sessions, so a recorder can run for days. Recorded minute candles go into the same cache the backtester reads. When a span is later fetched from the price-history API, the fetched bars replace the recorded ones.

## Paper trading and backtests

`PaperBroker` is the SDK's offline venue: durable, deterministic, long-equity only, with finite quote liquidity, simulated latency, fees and settlement. See [PAPER_TRADING.md](PAPER_TRADING.md) for the execution model and [PAPER_LIMITATIONS.md](PAPER_LIMITATIONS.md) for what it does not model. It needs no credentials and no network.

Time and market input are explicit events. `rustyschwab.paper` builds them; `sequence` starts at 1 and rises by one, `at_ms` is virtual time and never goes back.

```python
from decimal import Decimal
from rustyschwab import PaperBroker, paper

CONFIG = {
    "model_version": "equity-bbo-v2",
    "accounts": {"paper": {
        "initial_cash": "1000",
        "limits": {"max_open_quantity": "100", "max_open_notional": "10000",
                   "max_position_quantity": "100", "max_snapshot_age_ms": 5000},
    }},
    "execution_latency_ms": 10, "cancel_latency_ms": 100, "quote_max_age_ms": 5000,
    "slippage_bps": "0", "market_collar_bps": "500", "fee_per_share": "0.01",
    "max_working_orders": 100,
}

def limit(side, quantity, price, tag):
    return {"orderStrategyType": "SINGLE", "orderType": "LIMIT", "session": "NORMAL",
            "duration": "DAY", "price": price, "tag": tag,
            "orderLegCollection": [{"instruction": side, "quantity": quantity,
                                    "instrument": {"symbol": "ABC", "assetType": "EQUITY"}}]}

async def backtest(closes):
    broker = await PaperBroker.open("backtest/state.sqlite3", CONFIG)
    sequence = 1
    await broker.apply(paper.session(sequence, 0, "2026-01-02"))
    holding, previous = False, None
    for bar, close in enumerate(closes, start=1):
        price = Decimal(close)
        sequence += 1
        await broker.apply(paper.quote(
            sequence, bar * 60_000, "ABC",
            bid=price - Decimal("0.01"), ask=price + Decimal("0.01"),
            bid_size=1000, ask_size=1000))
        if previous is not None and not holding and price > previous:
            await broker.place_order("paper", limit("BUY", 5, price + Decimal("0.50"), f"buy-{bar}"))
            holding = True
        elif previous is not None and holding and price < previous:
            await broker.place_order("paper", limit("SELL", 5, price - Decimal("0.50"), f"sell-{bar}"))
            holding = False
        previous = price
    return await broker.snapshot("paper")
```

The same events and orders give the same fills and ledger in a fresh state file, which is what makes a run reproducible. `tests/test_backtest.py` in the repository runs this loop and asserts the result.

Things to know:

- Orders fill only on a *later* event than the one they were placed under, and only up to the size shown on the quote. A `paper.clock` event advances time without adding liquidity.
- Amounts the venue reports (`cash`, `quantity`, `price`) are decimal strings. Wrap them in `Decimal`.
- Paper orders need a tag. The binding assigns `rs-<uuid>` when there is none; a tag can be used once per account, and an accepted order consumes it.
- A rejection (no fresh quote, unsupported order) is a `BrokerError` with `status` 400 and the reason in `errors`, like a Schwab rejection. Cash, inventory and portfolio limits raise `RiskDenied`.
- `place_order` returns `attempt_id` as `paper-<order_id>`, and a lost acknowledgement (inject one with `{"kind": "fault", "fault": "lose_next_acknowledgement"}`) raises `SubmissionUnknown` with it. Reconcile by reading the order.
- `broker.snapshot(account)`, `order`, `orders(after_id=, limit=)`, `fills`, `quote(symbol)`, `venue_time_ms()`, `market_session()` and `verify()` read state; `halt`, `clear_halt`, `halted` and `begin_drain` are the controls.
- Reopening a state file with the same config resumes it; a different config is refused.

A venue configured with `"clock": "wall"` runs on the system clock instead: `broker.append_live({"kind": "quote", ...})` takes live market input and assigns its own sequence and time. That is how to paper trade against live quotes from `client.open_stream()` or `client.quotes()`.

### Performance reports

`rustyschwab.performance` reports on a run: net return, maximum drawdown and its duration, Sharpe, Sortino and Calmar ratios, and trade statistics (win rate, profit factor, expectancy, holding time) for the whole run and for each symbol. The arithmetic runs in the SDK (`schwab_rs::performance`), in decimals, and takes no I/O.

Closed trades are matched first in first out at the share level, long and short. A sell with nothing long to close opens a short, a fill that crosses through flat closes one position and opens the other, and each match of an entry lot against a closing fill is its own round trip, so partial fills, scale-ins and scale-outs are counted separately. A fill's fee is spread over the quantities it covers. Open lots are valued at `marks` (a symbol to price mapping) or, without one, at the symbol's last fill price.

```python
from rustyschwab import performance

fills = [  # a paper fill plus its order's symbol and side; see performance.paper_fills
    {"symbol": "ABC", "side": "BUY", "quantity": "5", "price": "10.61", "fee": "0.05", "at_ms": 180_000},
    {"symbol": "ABC", "side": "SELL", "quantity": "5", "price": "10.29", "fee": "0.05", "at_ms": 240_000},
]
report = performance.analyze(fills, starting_equity="1000")
print(performance.format_report(report))
```

`analyze(fills, equity=None, *, starting_equity, risk_free_rate=0.0, periods_per_year=252, benchmark=None, marks=None)` takes:

- `fills`: dicts of `symbol`, `side`, `quantity`, `price`, `at_ms` and optionally `fee` (for the whole fill, default 0) and `multiplier` (100 for options, default 1), in time order. `side` is `BUY` or `SELL` or any Schwab instruction (`SELL_SHORT`, `BUY_TO_COVER`, `SELL_TO_CLOSE`, ...).
- `equity`: your mark-to-market curve as `{"at_ms", "equity"}` points in time order, for example `snapshot["equity"]` after each bar. Without one, the curve is built from the fills (realized P&L less fees, one point per fill) and `report["equity_source"]` says `"fills"`.
- `benchmark`: a symbol to `(first_price, last_price)` mapping, for an equal-weight buy-and-hold return and `excess_return`.

Amounts may be `str`, `int`, `float` or `Decimal`. The report is a plain `dict` (a `PerformanceReport` `TypedDict`): money is a decimal string, as everywhere else in this package, and ratios and fractions are floats. Ratios are computed on the curve's last value for each New York trading date and annualized by `periods_per_year`; they are `None` with fewer than two trading days or no variation. Drawdown uses the full-resolution curve. Out-of-order fills or points, a non-positive quantity or price, or a negative fee raise `InvalidParameter`.

A paper venue fill has no symbol or side. `performance.paper_fills(order, fills)` adds them from the order record, and a run's fills combine in time order:

```python
fills = []
for order_id in order_ids:
    order = await broker.order("paper", order_id)
    fills += performance.paper_fills(order, await broker.fills("paper", order_id))
fills.sort(key=lambda fill: fill["at_ms"])
report = performance.analyze(fills, starting_equity="1000")
```

`tests/test_performance.py` runs this on the backtest above and checks the report against the venue's ledger.

## NautilusTrader

`rustyschwab.nautilus` connects [NautilusTrader](https://nautilustrader.io/) to Schwab: instruments, quote and trade ticks, historical bars, order execution, reports, reconciliation and account state, for US equities and equity options. Orders go through the same execution journal as everything else in this package, so an order of unknown outcome is never sent twice. The execution client can also run against the offline [paper venue](#paper-trading-and-backtests), which needs no Schwab account.

### Install

```sh
pip install "rustyschwab[nautilus]"
```

The extra installs `nautilus_trader>=1.231,<2`. NautilusTrader supports CPython 3.12 to 3.14 (the rest of `rustyschwab` runs on 3.10 and later) and is licensed LGPL-3.0, so it is an extra and not a dependency. The range stops before 2.0 because its release candidates restructure the live-client interfaces this adapter is written against. Authorize Schwab once as in [Authorization](#authorization); the clients read the same `SCHWAB_*` variables as `Client.from_env()`, which keeps the app secret out of the node's config and logs.

### The pieces

| Piece | What it is |
| --- | --- |
| `SchwabDataClientConfig`, `SchwabLiveDataClientFactory` | Build a `SchwabDataClient`: instruments, quote and trade ticks, bars |
| `SchwabExecClientConfig`, `SchwabLiveExecClientFactory` | Build a `SchwabExecutionClient`: orders, reports, account state |
| `SchwabPaperConfig` | Passed as `paper=` to the execution config to use the paper venue |
| `SchwabInstrumentProvider` | Loads `Equity` and `OptionContract` instruments from quotes and option chains |

The venue and client ID are `SCHWAB`. The account ID is `SCHWAB-<account hash>`: Schwab's `hashValue` from `client.account_numbers()`, never the account number. The account may be left out when the login has exactly one. OMS is `NETTING`, the base currency is USD, and the account type is `MARGIN` unless you say `CASH` (the client checks it against Schwab on connect and refuses to start on a mismatch). Register the factories under the name `SCHWAB`; a different name becomes the client ID and must not contain a hyphen, because it is the issuer in the account ID.

### A live node

```python
from nautilus_trader.config import InstrumentProviderConfig, LiveExecEngineConfig, LoggingConfig, TradingNodeConfig
from nautilus_trader.live.node import TradingNode
from nautilus_trader.model.identifiers import InstrumentId, TraderId

from rustyschwab.nautilus import (
    SCHWAB,
    SchwabDataClientConfig,
    SchwabExecClientConfig,
    SchwabLiveDataClientFactory,
    SchwabLiveExecClientFactory,
)

config = TradingNodeConfig(
    trader_id=TraderId("SCHWAB-001"),
    logging=LoggingConfig(log_level="INFO"),
    # Give an order whose outcome is unknown time to appear before the engine
    # decides Schwab has no record of it.
    exec_engine=LiveExecEngineConfig(reconciliation=True, inflight_check_threshold_ms=15_000),
    data_clients={
        SCHWAB: SchwabDataClientConfig(
            instrument_provider=InstrumentProviderConfig(
                load_ids=frozenset({InstrumentId.from_str("AAPL.SCHWAB")})
            ),
        ),
    },
    exec_clients={SCHWAB: SchwabExecClientConfig(account_hash="<hashValue>")},
)
node = TradingNode(config=config)
node.add_data_client_factory(SCHWAB, SchwabLiveDataClientFactory)
node.add_exec_client_factory(SCHWAB, SchwabLiveExecClientFactory)
node.trader.add_strategy(MyStrategy())   # any Strategy; it trades instruments on venue SCHWAB
node.build()
node.run()
```

A strategy uses the usual calls. `subscribe_quote_ticks(InstrumentId.from_str("AAPL.SCHWAB"))` and `subscribe_trade_ticks` open the Level One stream, `request_bars` reads price history, and `order_factory.limit(...)` with `submit_order` trades. An option's ID is its OCC symbol without the padding, `AAPL261218C00200000.SCHWAB`.

### A paper node

```python
from rustyschwab.nautilus import SchwabPaperConfig

aapl = InstrumentProviderConfig(load_ids=frozenset({InstrumentId.from_str("AAPL.SCHWAB")}))
config = TradingNodeConfig(
    trader_id=TraderId("PAPER-001"),
    data_clients={SCHWAB: SchwabDataClientConfig(instrument_provider=aapl)},    # the quotes
    exec_clients={
        SCHWAB: SchwabExecClientConfig(
            paper=SchwabPaperConfig(
                state_path="/var/lib/trader/paper/venue.sqlite3",   # a private (0700) directory
                account="paper-main",
                initial_cash="100000",
                fee_per_share="0.005",
            ),
        ),
    },
)
```

Everything else is the same node. The paper execution client never contacts Schwab. It subscribes to the quote ticks the node publishes for venue `SCHWAB` (`data.quotes.SCHWAB.*` on the message bus) and gives each to the venue, which fills orders against them. So any source of SCHWAB quote ticks drives it: the `SchwabDataClient` (which needs a Schwab app and authorization for market data, but not an account to trade), or your own actor that publishes `QuoteTick`s for instruments you have added to the cache. A strategy must subscribe to the quotes of what it trades, since a symbol with no fresh quote cannot be ordered. The venue state is durable: reopen the same `state_path` with the same settings and the account, positions and resting GTC orders are back, and Nautilus reconciles against them like any restart.

### Instruments

Schwab has no instrument listing, so nothing loads wholesale. The provider loads what it is asked for, from `load_ids` or from `filters` with `"symbols"` and `"option_chains"` (option-chain requests in Schwab's query names, such as `{"symbol": "AAPL", "contractType": "CALL", "strikeCount": 6}`). The execution client also loads the instruments of your open positions and working orders on connect, so reconciliation can use them. Schwab's data leaves some things open, so the adapter assumes:

| | Choice |
| --- | --- |
| Equity precision and tick | `0.01` at or above one dollar, `0.0001` below, decided from the last price when the instrument is loaded |
| Option tick | `0.01` for Penny Pilot classes (`pennyPilot` or `isPennyPilot`), `0.05` otherwise; the exchange's premium-tiered ticks are not modelled |
| Option multiplier | The contract's `multiplier`, `100` when absent; adjusted contracts report their own |
| Option expiry | The date in the OCC symbol at the 16:00 New York close; `activation_ns` is `0`, since Schwab reports no listing date |
| Sizes | Whole shares and contracts: `lot_size` 1, `size_precision` 0 |
| Skipped | Mutual funds, bonds, indexes and cash equivalents |

### Market data

The data client opens the one streaming session Schwab allows per user when the first quote or trade subscription arrives, not at connect. The stream is the SDK's, so reconnection and replay are its job; the client reads the `state` messages for what that leaves to it. Level One sends only the fields that changed, so the client keeps each instrument's last values and publishes a tick when a delta completes one.

| Nautilus | From |
| --- | --- |
| `QuoteTick` | `bidPrice`, `askPrice`, `bidSize`, `askSize`; stamped with `quoteTime`, else the frame's `timestamp`. Equity sizes are round lots and are multiplied by `streamer_lot_size` (default 100); option sizes are contracts |
| `TradeTick` | `lastPrice`, `lastSize` when `tradeTime` or `totalVolume` moved; trade ID is `<tradeTime>-<totalVolume>`; no aggressor |
| `Bar` | `price_history` candles for `1/5/10/15/30-MINUTE`, `1-DAY`, `1-WEEK`, `1-MONTH` `LAST` bars, `EXTERNAL`; stamped on the close unless `bars_timestamp_on_close=False` |

A gap is a `lost_messages` total that rose or a new `generation`. The client logs it, forgets the merged state, and re-reads the subscribed instruments' quotes over REST: the snapshot becomes a quote tick and the base for the next delta. When the stream ends, `replaced`, `login_denied` and `disconnected` are final (reopening would end the session that replaced this one), `idle` with nothing subscribed closes quietly, and anything else is reopened and resubscribed after one, two, four seconds, up to `stream_reconnect_attempts`, before the client marks itself disconnected.

### Orders

`MARKET`, `LIMIT`, `STOP_MARKET` and `STOP_LIMIT` orders, `DAY` and `GTC`, single-leg, on equities and options. Schwab's order JSON for them:

| Nautilus | Schwab |
| --- | --- |
| `MARKET`, `LIMIT`, `STOP_MARKET`, `STOP_LIMIT` | `MARKET`, `LIMIT`, `STOP`, `STOP_LIMIT` |
| `price` / `trigger_price` | `price` / `stopPrice`, as exact strings |
| `trigger_type` `DEFAULT`, `LAST_PRICE`, `MARK_PRICE` | `stopType` unset (standard), `LAST`, `MARK` |
| `DAY`, `GTC` | `DAY`, `GOOD_TILL_CANCEL` |
| quantity | `quantity`, a JSON number |
| (config `order_session`) | `session`, `NORMAL` by default |
| `client_order_id` | `tag` |

Schwab wants a short sale or an option opening named, and Nautilus nets, so the instruction comes from the net position in the cache:

| Order | Net position | Instruction |
| --- | --- | --- |
| Equity `BUY` | flat or long | `BUY` |
| Equity `BUY` | short, at least the quantity | `BUY_TO_COVER` |
| Equity `SELL` | long, at least the quantity | `SELL` |
| Equity `SELL` | flat or short | `SELL_SHORT` (only with `allow_short_positions=True`) |
| Option `BUY` | flat or long | `BUY_TO_OPEN` |
| Option `BUY` | short, at least the quantity | `BUY_TO_CLOSE` |
| Option `SELL` | long, at least the quantity | `SELL_TO_CLOSE` |
| Option `SELL` | flat or short | `SELL_TO_OPEN` (only with `allow_short_positions=True`) |

An order that would open a short without `allow_short_positions`, or cross zero in one order, is denied (`OrderDenied`) with the reason, as is anything Schwab cannot express: other order types, `IOC`, `FOK` and `GTD`, reduce-only, post-only, display quantity, quote-denominated quantity, fractional quantity, and order lists. Nothing is approximated. Option open and close rely on the cache knowing the position, so keep reconciliation on.

**Modify** is Schwab's replace: a new Schwab order that supersedes the old. The adapter sends the original's instruction with the new price, quantity or trigger under a new tag, `<client order id>-R1`, `-R2` and so on, because the journal spends each tag once; the answer becomes an `OrderUpdated` with the new venue order ID. A partly filled order cannot be modified (Schwab's replacement would restart the quantity), so the adapter answers `OrderModifyRejected` and the strategy cancels and resends the remainder.

**Cancel** is confirmed by Schwab, not assumed: the cancel request returns, and the `OrderCanceled` event follows when the order's state is read.

### How orders are tracked

Every order carries its `ClientOrderId` as the Schwab `tag`, and Schwab echoes the tag on every read, so any Schwab order maps back to its Nautilus order with no state kept, across restarts. (Schwab documents no limit on a tag's length or characters, so the client order ID goes out as it is; Nautilus' default IDs are about 30 characters.) The adapter also stores the journal `attempt_id` and the latest revision per order in Nautilus' cache (persisted if you configure a cache database), where an operator can find it.

The execution client does not use the streamer, which the data client holds. It reads the account's orders every `poll_interval_secs` (3 seconds), and at once after it places or cancels an order. A working order older than the `order_lookback_hours` window (24) is read individually. Each order read is matched to its Nautilus order by tag and the difference becomes events: accepted, fills, canceled, expired, and a replacement Schwab accepted whose acknowledgement was lost. An execution becomes an `OrderFilled` with trade ID `<order id>-<activityId>` (the execution's position when Schwab gives no `activityId`), so the same execution is the same fill on every read. Schwab's order JSON carries no commission, so live fills report zero commission.

**Uncertain outcomes.** If the SDK raises `SubmissionUnknown`, the order stays `SUBMITTED`, is never sent again, and the polls look for its tag. The `attempt_id` is logged and stored; if the order never appears, resolve the attempt with `schwab-admin reconcile`. Until then the SDK refuses further submissions on the account with `ReconciliationRequired`, which the client reports as `OrderRejected` ("nothing was sent"). If the engine gives up on the order before Schwab shows it and Schwab later does show it working, the client logs an error naming the Schwab order and leaves it alone: it does not cancel orders on its own judgement. The same holds for a replacement: `SubmissionUnknown` on modify leaves the order as it was, and the replacement is applied when its `-R<n>` tag is read.

### Reports and reconciliation

| Method | From |
| --- | --- |
| `generate_order_status_report(s)` | The account's orders (60 days at most, Schwab's limit), one report each; `REPLACED` orders are left out because their replacement is reported |
| `generate_fill_reports` | The executions in `orderActivityCollection` |
| `generate_position_status_reports` | The account's positions, netted long or short; a position Nautilus holds that the account does not is reported flat |
| Account state | `liquidationValue` as the total; `availableFunds` (margin) or `cashAvailableForTrading` (cash) as free; the difference as locked |

Reports carry the Nautilus `ClientOrderId` when the cache knows the order, and none otherwise, so an order placed in Schwab's own apps arrives as an external order. Orders the adapter does not model (multi-leg, trailing stops, mutual funds, fractional quantities) are left out of reports. Balances refresh after any fill and every `account_poll_interval_secs` (30).

### Known limits

- Equities and equity options only; single-leg orders only. No spreads, brackets, OCO, trailing stops or order lists.
- No live bar subscriptions, order books, mark or index prices, or historical quote and trade ticks: Schwab's Level One stream and price history are what exist. Bars come from price history only (no 2-minute, hourly or multi-day bars).
- Fills and cancels are found by polling, so they arrive up to `poll_interval_secs` late. Account activity over the streamer is not used.
- The adapter was written against Schwab's documented and fixture shapes. It has not run against a live account; test with `preview_order` and the paper venue first.
- Paper venue limits are the venue's own ([PAPER_LIMITATIONS.md](PAPER_LIMITATIONS.md)): long, whole-share equities, `NORMAL` session, no options, no shorts. Each node run is one trading session: DAY orders expire when it stops. A quote under one dollar, or crossed, is dropped (`PaperVenue.rejected_quotes` counts them). Stops are the venue's simulation rules, not Schwab's election rules.
- Schwab allows one streaming session per user. Starting a second consumer, including another node, ends the first with `replaced`.

## Error handling

Every failure is a `rustyschwab.SchwabError`. The subclasses mirror the SDK's `ErrorKind`:

| Exception | Meaning | Attributes |
| --- | --- | --- |
| `ValidationError` (`InvalidParameter`, also a `ValueError`) | A request or argument was refused before anything was sent | `field` |
| `ConfigError` | Invalid or incomplete configuration | |
| `AuthError` | Authentication failed | `reauthorization_required` |
| `BrokerError` | Schwab answered with an error | `status`, `errors`, `request_id` |
| `HttpError`, `ApiError`, `RemoteError` | A non-2xx response without Schwab's error body, an API error, a remote authority's refusal | `status`, `body`, `code` |
| `RateLimited` | Rate limit hit | `retry_after` (seconds) |
| `Timeout` (also a `TimeoutError`), `NetworkError`, `CircuitOpen` | Transport trouble | `duration` |
| `Halted`, `Suspended`, `RiskDenied`, `BaselineStale`, `DuplicateRequest`, `ApprovalInvalid`, `OwnershipLost`, `LeaseBusy` | Trading controls: resolve the condition, then submit a new intent | `account`, `reason` |
| `SubmissionUnknown`, `ReconciliationRequired` (both `PendingSubmissionError`) | An order may have reached Schwab | `attempt_id` |
| `StreamError`, `SubscriptionError`, `WebSocketError`, `ConnectionClosed` | The streamer | |
| `StorageError`, `IoError`, `SerializationError`, `InvalidUrl`, `UnknownError` | Local state and everything else | |

Every instance has `kind` (the SDK's `snake_case` classification), `retryable` (the SDK's judgement that the failure is transient) and `message`.

```python
try:
    await client.place_order(account_hash, order)
except rustyschwab.RiskDenied as error:
    log.warning("risk denied: %s", error)
except rustyschwab.PendingSubmissionError as error:
    reconcile(error.attempt_id)               # never resend first
except rustyschwab.AuthError as error:
    if error.reauthorization_required:
        alert_operator()
except rustyschwab.SchwabError as error:
    if error.retryable:
        schedule_retry()
```

Messages are safe to log. They never contain tokens, the app secret or broker response bodies, and neither do `repr`s: Schwab's own error objects are in `BrokerError.errors` and an unparsed body is in `HttpError.body`, never in `str(error)`. Treat those two attributes as you would a response body, since Schwab can echo request data in them. Exceptions pickle with their attributes.

## How asyncio and the runtime work

- Calls return coroutines, so `await`, `asyncio.create_task`, `gather` and `wait_for` all work, and nothing is sent until the coroutine runs. Awaiting one twice raises `RuntimeError`.
- One multi-threaded tokio runtime serves the whole process, whichever event loop awaits (asyncio, uvloop, or several loops in turn). Python's event loop only waits for results; the GIL is not held while a request is in flight, and slow requests overlap rather than queue. The SDK's request budget (Schwab's 120 requests a minute by default) still paces them, with a share kept for order submissions and cancels.
- Cancelling a task awaiting a call cancels the Rust future. The exception is order submission: it runs to its outcome on its own task, so a cancelled or timed-out `place_order` still leaves its journal record resolved, and an attempt you stopped waiting for is reconciled like any other.
- Code without asyncio uses the blocking [`rustyschwab.sync`](#synchronous-use), which runs these same coroutines on one private event loop thread.
- Results are converted to Python objects under the GIL when a call completes. Very large responses (full option chains) cost milliseconds of GIL time.
- The SDK's `tracing` events are not forwarded to Python `logging`.

## Building from source

The bindings live in `crates/schwab-py`, outside the Cargo workspace (their PyO3 dependency tree, the Python toolchain and cdylib linking stay out of the SDK's lockfile and CI jobs). With a recent Rust toolchain and Python 3.10+:

```sh
cd crates/schwab-py
python -m venv .venv && . .venv/bin/activate
pip install maturin pytest pytest-asyncio websockets
maturin develop            # editable install; add --release for speed
pytest                     # offline: a local mock server stands in for Schwab
cargo test                 # Rust unit tests (links the interpreter)
```

The NautilusTrader adapter tests (`tests/nautilus`) run when `nautilus_trader` is installed (`pip install "nautilus_trader>=1.231,<2"` on Python 3.12 or later) and skip themselves otherwise.

CI builds the wheel and runs the suite on Linux and macOS (`ci.yml`, job `python`), with the `nautilus` extra on one Linux row. Tags build the release wheels and sdist and publish them through PyPI trusted publishing (`publish-python.yml`, behind the `pypi` environment).
