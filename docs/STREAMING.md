# Real-time Streaming

The Schwab Rust SDK provides a robust WebSocket client for streaming real-time market data.

## Features

- **Reconnection Logic**: Automatically reconnects with jittered exponential backoff if the connection is lost. A login Schwab denies after the stream was healthy is retried once with a refreshed token.
- **Subscription Persistence**: Re-subscribes to your active services upon reconnection, one service at a time, and delivers the first snapshot each one returns.
- **Heartbeat Monitoring**: Monitors the connection with a 20s heartbeat and 30s timeout (the timeout must exceed the interval).
- **Backpressure Control**: A bounded queue (4,096 messages by default; `ChannelKind::Unbounded` is capped at 65,536) with counted, visible loss.
- **A visible end**: when the stream stops for good, the receiver reports why and then ends.

## Getting Started

### 1. Configure and Build the Client

```rust
use rustyschwab::streaming::{StreamClient, StreamConfig, ChannelKind};
use std::time::Duration;

let config = StreamConfig {
    channel_kind: ChannelKind::Bounded(1000), // Limit memory usage
    heartbeat_interval: Duration::from_secs(20),
    ..Default::default()
};

let stream_client = StreamClient::builder()
    .config(config)
    .auth_manager(auth_manager)
    .customer_id(customer_id)
    .build()?;
```

### 2. Connect and Subscribe

```rust
use rustyschwab::types::streaming::StreamService;

// Connect to the WebSocket
stream_client.connect().await?;

// Subscribe to Equities Level 1 data
stream_client.subscribe(
    StreamService::LeveloneEquities, 
    vec!["AAPL".into(), "MSFT".into()]
).await?;
```

### 3. Handle Incoming Messages

The `StreamClient` provides a receiver that you can use to process messages.
Data frames arrive with numbered fields whose meaning depends on the service
(`"1"` is an equity's bid price but an option's description); `decode()`
turns each item into a typed payload whose fields are named like the REST
quote types.

```rust
use rustyschwab::streaming::StreamMessage;
use rustyschwab::types::streaming::StreamPayload;

if let Some(mut receiver) = stream_client.get_receiver() {
    while let Some(msg) = receiver.recv().await {
        match msg {
            StreamMessage::Data(data) => {
                for item in &data.data {
                    for payload in item.decode()? {
                        match payload {
                            StreamPayload::LevelOneEquity(quote) => {
                                // Deltas carry only the fields that changed.
                                println!("{} bid {:?}", quote.key, quote.bid_price);
                            }
                            StreamPayload::Book(book) => {
                                println!("{} {:?}", book.key, book.bid_side_levels);
                            }
                            other => println!("{other:?}"),
                        }
                    }
                }
            }
            StreamMessage::Response(resp) => {
                println!("Server Response: {:?}", resp);
            }
            StreamMessage::Notify(hb) => {
                // Heartbeat notification
            }
            StreamMessage::State(status) => {
                // A gap (reconnect or overflow) or the end of the stream:
                // re-baseline from REST before trusting derived state.
                if let Some(reason) = status.ended {
                    eprintln!("stream ended: {reason:?}");
                }
            }
            // `StreamMessage` is non-exhaustive.
            _ => {}
        }
    }
}
```

Each service's payload (`LevelOneEquity`, `LevelOneOption`, `LevelOneFutures`,
`LevelOneFuturesOption`, `LevelOneForex`, `Book`, `ChartEquity`, `ChartFutures`,
`Screener`, `AccountActivity`) also has a `from_content()` constructor for a
single `DataContent`, and `DataContent::named_fields()` renames without typing
when you want to keep working with JSON.

## Available Services

You can subscribe to various services using the `StreamService` enum:

- `LeveloneEquities`, `LeveloneOptions`, `LeveloneFutures`,
  `LeveloneFuturesOptions`, `LeveloneForex`: level-one quotes (deltas carry
  only the fields that changed).
- `NyseBook`, `NasdaqBook`, `OptionsBook`: level-two books (each frame is a
  whole book).
- `ChartEquity`, `ChartFutures`: one-minute bars.
- `ScreenerEquity`, `ScreenerOption`: movers by screener key.
- `AcctActivity`: account activity (orders, fills) under one fixed key.
  Subscribed with `SUBS` as Schwab documents; it has no `VIEW`.

## Best Practices

### Custom Field Selection
By default, the SDK requests every field of a service (`StreamService::default_fields()`),
except for `LEVELONE_EQUITIES`, which starts with a quote-sized subset. Narrow the list
with the service's field table; field 0 is always the key.

```rust
use rustyschwab::types::streaming::LevelOneEquityFields as F;

// Request only the symbol, bid and ask
let fields = [F::SYMBOL, F::BID_PRICE, F::ASK_PRICE]
    .map(|field| field.to_string())
    .join(",");
stream_client.set_service_fields(StreamService::LeveloneEquities, fields);
```

`set_service_fields` applies to later subscriptions and reconnects. To change
the fields of keys already streaming, send Schwab's `VIEW` command, which also
records the fields for later:

```rust,ignore
stream_client.view(StreamService::LeveloneEquities, fields).await?;
```

### Slow consumers
The queue is bounded (4,096 messages by default). When it fills, frames are
dropped and counted (`lost_messages`, `schwab_stream_dropped_messages_total`).
The frames already queued are delivered first, in order; then the receiver
yields `StreamMessage::State` with `ready: false`. The connection stays up.
Once your consumer has drained half the queue, delivery resumes under a new
`generation`; the dropped frames are never delivered. Treat any generation
change as a gap: re-read a REST baseline before trusting derived state.

### Reconnects and replay
A new session is not ready until every tracked subscription has been
replayed and acknowledged. Data that arrives meanwhile (the first full
snapshot of each key) is held and delivered once the session is ready. A
service Schwab refuses on replay is dropped from the tracker and its
refusal delivered as a `StreamMessage::Response`, so one bad subscription
cannot fail every reconnect.

### Resource Management
Ensure the receiver loop is running in a spawned task if your main thread needs to perform other work. The `StreamClient` will continue to run in the background as long as it's not dropped.

### Error Handling
When the stream stops for good, the receiver delivers a `StreamMessage::State`
whose `ended` says why, then returns `None`:

| `StreamEnd` | Cause |
|---|---|
| `Disconnected` | You called `disconnect()` (or dropped the client). |
| `Replaced` | Another session took over this login's streamer slot (code 12). Schwab allows one per user. |
| `Idle` | Schwab stopped streaming to a session with no subscriptions (code 30). |
| `ServerClosed` | The server closed the socket normally without saying why. |
| `LoginDenied` | Schwab refused the login, even after a token refresh. |
| `ConnectFailed` | The first connection never got as far as a login. |
| `ReconnectExhausted` | The connection was lost and reconnecting is disabled or out of attempts. |

A later `connect()` starts a new session the same receiver can keep reading.
