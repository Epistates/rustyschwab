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

The NautilusTrader adapter tests (`tests/nautilus`) run when `nautilus_trader` is installed (`pip install "nautilus_trader>=1.231,<2"` on Python 3.12 or later) and skip themselves otherwise.

CI builds the wheel and runs the suite on Linux and macOS (`ci.yml`, job `python`), with the `nautilus` extra on one Linux row. Tags build the release wheels and sdist and publish them through PyPI trusted publishing (`publish-python.yml`, behind the `pypi` environment).
