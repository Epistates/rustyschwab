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

```rust
use schwab_rs::streaming::StreamMessage;

if let Some(mut receiver) = stream_client.get_receiver() {
    while let Some(msg) = receiver.recv().await {
        match msg {
            StreamMessage::Data(data) => {
                println!("Market Data: {:?}", data);
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

## Available Services

You can subscribe to various services using the `StreamService` enum:

- `LeveloneEquities`: Real-time quotes for stocks.
- `LeveloneOptions`: Real-time quotes for options.
- `LeveloneFutures`: Real-time quotes for futures.
- `ChartEquity`: Real-time chart data (OHLC).
- `AcctActivity`: Real-time notifications for account activity (orders, fills).

## Best Practices

### Custom Field Selection
By default, the SDK requests a standard set of fields. You can customize this to reduce bandwidth.

```rust
// Request only Symbol (0), Bid (1), and Ask (2)
stream_client.set_service_fields(
    StreamService::LeveloneEquities, 
    "0,1,2".to_string()
);
```

### Resource Management
Ensure the receiver loop is running in a spawned task if your main thread needs to perform other work. The `StreamClient` will continue to run in the background as long as it's not dropped.

### Error Handling
The receiver will return `None` if the sender is dropped or the connection is permanently closed. Check for this to exit your processing loop gracefully.
