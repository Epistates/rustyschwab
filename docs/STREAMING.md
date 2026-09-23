# Real-time Streaming

The Schwab Rust SDK provides a robust WebSocket client for streaming real-time market data.

## Features

- **Reconnection Logic**: Automatically reconnects with exponential backoff if the connection is lost.
- **Subscription Persistence**: Automatically re-subscribes to your active services upon reconnection.
- **Heartbeat Monitoring**: Monitors the connection with a 20s heartbeat and 30s timeout.
- **Backpressure Control**: Choose between Bounded and Unbounded channels for receiving data.

## Getting Started

### 1. Configure and Build the Client

```rust
use schwab_rs::streaming::{StreamClient, StreamConfig, ChannelKind};
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
use schwab_rs::types::streaming::StreamService;

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
use schwab_rs::streaming::StreamMessage;
use schwab_rs::types::streaming::StreamPayload;

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

- `LeveloneEquities`: Real-time quotes for stocks.
- `LeveloneOptions`: Real-time quotes for options.
- `LeveloneFutures`: Real-time quotes for futures.
- `ChartEquity`: Real-time chart data (OHLC).
- `AcctActivity`: Real-time notifications for account activity (orders, fills).

## Best Practices

### Custom Field Selection
By default, the SDK requests every field of a service (`StreamService::default_fields()`),
except for `LEVELONE_EQUITIES`, which starts with a quote-sized subset. Narrow the list
with the service's field table; field 0 is always the key.

```rust
use schwab_rs::types::streaming::LevelOneEquityFields as F;

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
dropped and counted (`lost_messages`, `schwab_stream_dropped_messages_total`),
and the receiver yields `StreamMessage::State` with `ready: false`. The
connection stays up. Once your consumer has drained half the queue, delivery
resumes under a new `generation`; frames from the gap are never delivered.
Treat any generation change as a gap: re-read a REST baseline before trusting
derived state.

### Resource Management
Ensure the receiver loop is running in a spawned task if your main thread needs to perform other work. The `StreamClient` will continue to run in the background as long as it's not dropped.

### Error Handling
The receiver will return `None` if the sender is dropped or the connection is permanently closed. Check for this to exit your processing loop gracefully.
