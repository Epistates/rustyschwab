# rustyschwab

Python bindings for the [rustyschwab](https://github.com/epistates/rustyschwab) Schwab SDK: an asyncio-native client for Schwab's Trader and Market Data APIs, the streamer, and an offline paper-trading venue. Requests and responses are plain `dict` and `list` values in Schwab's JSON shape.

The work happens in Rust, so everything the SDK guarantees holds here. Orders go through a durable execution journal, an order whose outcome is unknown is never retried, risk checks and operator halts apply, and the request budget is shared. Error messages and reprs never contain tokens or secrets.

> **Risk warning.** Automated trading involves significant risk of financial loss. This software is provided as is, without warranty. Test against the paper venue before using real capital.

```sh
pip install rustyschwab        # CPython 3.10+; abi3 wheels for Linux, macOS and Windows
```

## A quick look

```python
import asyncio
import rustyschwab

async def main() -> None:
    client = rustyschwab.Client.from_env()          # SCHWAB_APP_KEY, SCHWAB_APP_SECRET, ...

    quotes = await client.quotes(["AAPL", "MSFT"])
    print(quotes["AAPL"]["quote"]["lastPrice"])

    account_hash = (await client.account_numbers())[0]["hashValue"]
    placed = await client.place_order(account_hash, {
        "orderStrategyType": "SINGLE", "orderType": "LIMIT", "session": "NORMAL",
        "duration": "DAY", "price": "189.50",
        "orderLegCollection": [{"instruction": "BUY", "quantity": 10,
                                "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}}],
    })
    print(placed["order_id"], placed["attempt_id"])

    stream = await client.open_stream()
    await stream.subscribe("LEVELONE_EQUITIES", ["AAPL"], fields=["bidPrice", "askPrice"])
    async for message in stream:
        if message["type"] == "data":
            print(message["data"][0]["content"])

asyncio.run(main())
```

An offline backtest needs no credentials:

```python
from rustyschwab import PaperBroker, paper

broker = await PaperBroker.open("state.sqlite3", config)
await broker.apply(paper.session(1, 0, "2026-01-02"))
await broker.apply(paper.quote(2, 60_000, "ABC", bid="10.00", ask="10.01", bid_size=100, ask_size=100))
await broker.place_order("paper", order)
```

## What is in the box

- `Client`: quotes, price history, option chains, movers, market hours, instruments, accounts and positions, orders, transactions, user preferences, order preview, and journaled place, replace and cancel.
- `Stream`: the streamer as an async iterator, with subscribe, unsubscribe and view, automatic reconnection and explicit gap notifications.
- `PaperBroker`: the SDK's deterministic paper venue, driven by sequenced events, for backtests and tests.
- A `SchwabError` hierarchy that mirrors the SDK's error kinds, such as `RiskDenied`, `Halted`, `RateLimited`, `AuthError`, `BrokerError` and `SubmissionUnknown`.
- `rustyschwab.nautilus`, an optional [NautilusTrader](https://nautilustrader.io/) adapter: instruments, quote and trade ticks, bars, order execution, reports and reconciliation for Schwab equities and options, and the same execution client on the offline paper venue. Install it with `pip install "rustyschwab[nautilus]"` (CPython 3.12 to 3.14, as NautilusTrader requires).
- Type stubs (`py.typed`).

The [Python guide](https://github.com/epistates/rustyschwab/blob/main/docs/PYTHON.md) covers installation, authorization, configuration, orders, streaming, paper trading, the NautilusTrader adapter, error handling and how asyncio and the Rust runtime fit together. The [SDK documentation](https://github.com/epistates/rustyschwab/blob/main/docs/INDEX.md) explains the model behind it.

## Development

The crate is outside the Cargo workspace and builds with [maturin](https://www.maturin.rs/):

```sh
maturin develop && pytest      # offline; a local mock server stands in for Schwab
pip install -e ".[nautilus]"   # then pytest also runs the NautilusTrader adapter tests
cargo test                     # Rust unit tests
```

Licensed under MIT or Apache-2.0, at your option.
