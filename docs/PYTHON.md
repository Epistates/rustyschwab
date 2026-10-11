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
- [Paper trading and backtests](#paper-trading-and-backtests)
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
- There is no synchronous API. From a script, use `asyncio.run`.
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

CI builds the wheel and runs the suite on Linux and macOS (`ci.yml`, job `python`). Tags build the release wheels and sdist and publish them through PyPI trusted publishing (`publish-python.yml`, behind the `pypi` environment).
