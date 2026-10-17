# rustyschwab

An unofficial Rust SDK for the Charles Schwab Trader and Market Data APIs,
not affiliated with or endorsed by Charles Schwab & Co., Inc.: OAuth with
encrypted or keychain token storage, a REST client with retries, a shared
rate budget and a circuit breaker, the streamer protocol, a durable order
journal, and a paper-trading venue.

Wire types live in [`rustyschwab-types`](https://docs.rs/rustyschwab-types) and are
re-exported as `rustyschwab::types`. The repository
[README](https://github.com/epistates/rustyschwab) covers the MCP server,
operations and the full documentation set.

```toml
[dependencies]
rustyschwab = { version = "0.2", features = ["callback-server"] }
tokio = { version = "1", features = ["full"] }
```

`callback-server` adds the local HTTPS listener that captures the OAuth
redirect. It serves the certificate in `OAuthConfig::callback_cert_file`
and `callback_key_file` (for example one from `mkcert 127.0.0.1`, which the
browser trusts), or a self-signed one the browser warns about.
`AuthManager::authorize_with` hands you the consent URL before it waits.
Without the feature, `AuthManager::authorize` returns the URL and you pass
the redirect to `exchange_code` yourself.

## Configure and authorize

`SchwabConfig::from_env` reads the `SCHWAB_*` variables listed in the
repository's `.env.example` (`SCHWAB_APP_KEY`, `SCHWAB_APP_SECRET`,
`SCHWAB_OAUTH_CALLBACK_URL`, `SCHWAB_TOKENS_FILE`, `SCHWAB_TOKEN_STORE`, …)
and validates them.

```rust,no_run
use rustyschwab::prelude::*;

# async fn run() -> rustyschwab::Result<()> {
let client = SchwabClient::new(SchwabConfig::from_env()?)?;
let auth = client.auth_manager();
if !auth.has_valid_tokens() {
    // Opens nothing by itself: print the URL, sign in, and the callback
    // server (or `exchange_code`) stores the tokens.
    let (url, _code) = auth.authorize().await?;
    println!("Sign in at {url}");
}
client.init().await?; // background refresh
# Ok(())
# }
```

## Read market data and accounts

```rust,no_run
use rustyschwab::prelude::*;
use rustyschwab::types::market_data::{FrequencyType, PeriodType, PriceHistoryParams};

# async fn run(client: SchwabClient) -> rustyschwab::Result<()> {
let quotes = client.get_quotes(&["AAPL", "MSFT"]).await?;
let history = client
    .get_price_history(&PriceHistoryParams {
        period_type: Some(PeriodType::Day),
        period: Some(10),
        frequency_type: Some(FrequencyType::Minute),
        frequency: Some(5),
        ..PriceHistoryParams::new("AAPL")
    })
    .await?;
let accounts = client.get_account_numbers().await?;
# let _ = (quotes, history, accounts);
# Ok(())
# }
```

## Place an order

Orders are built from exact decimals and validated before anything leaves
the process. A submission is sent once and never retried: an ambiguous
outcome is `Error::SubmissionUnknown`, and the account's next submission
waits until the journal reconciles it against the broker.

```rust,no_run
use rustyschwab::SchwabClient;
use rustyschwab::types::Decimal;
use rustyschwab::types::trading::{OrderInstruction, OrderRequest, OrderRequestLeg};

# async fn run(client: SchwabClient, account_hash: &str) -> Result<(), Box<dyn std::error::Error>> {
// Buy 10 AAPL at 150.25, then take profit at 160 or stop out at 145.
let order = OrderRequest::limit(
    OrderRequestLeg::equity(OrderInstruction::Buy, "AAPL", 10),
    "150.25".parse()?,
)
.bracket(Decimal::from(160), Decimal::from(145))?;

let response = client.place_order(account_hash, &order).await?;
println!("order id: {:?}", response.order_id);
# Ok(())
# }
```

## Stream quotes

The streamer identity (customer ID, channel, function ID) comes from the
user-preferences response. Consumers must handle `StreamMessage::State`:
when `ready` is false or the generation changes, derived state is stale.

```rust,no_run
use rustyschwab::{SchwabClient, StreamClient, StreamMessage};
use rustyschwab::config::StreamConfig;
use rustyschwab::types::streaming::{LevelOneEquity, StreamService};

# async fn run(client: SchwabClient) -> Result<(), Box<dyn std::error::Error>> {
let prefs = client.get_user_preferences().await?;
let info = prefs
    .streamer_info
    .as_deref()
    .and_then(<[_]>::first)
    .ok_or("no streamer info")?;
let stream = StreamClient::builder()
    .config(StreamConfig::default())
    .auth_manager(client.auth_manager())
    .streamer_info(info)
    .build()?;
stream.connect().await?;
stream
    .subscribe(StreamService::LeveloneEquities, vec!["AAPL".into()])
    .await?;

let mut receiver = stream.get_receiver().ok_or("receiver already taken")?;
while let Some(message) = receiver.recv().await {
    match message {
        StreamMessage::State(status) if !status.ready => {
            // Session lost or messages dropped: re-baseline from REST.
        }
        StreamMessage::Data(data) => {
            for item in data.data.iter().filter(|i| i.service() == Some(StreamService::LeveloneEquities)) {
                for content in &item.content {
                    let quote = LevelOneEquity::from_content(content)?;
                    println!("{} bid {:?} ask {:?}", quote.key, quote.bid_price, quote.ask_price);
                }
            }
        }
        _ => {}
    }
}
# Ok(())
# }
```

## Features

| Feature | Default | Adds |
|---|---|---|
| `rustls` | yes | rustls TLS for HTTP and WebSocket |
| `native-tls` | no | the platform TLS stack instead |
| `callback-server` | no | local HTTPS OAuth callback capture |
| `schemars` | no | JSON Schema derives on the request types |
| `telemetry` | yes | `rustyschwab::telemetry`: ready-made `tracing-subscriber` setups |
| `otel` | no | `telemetry::init_otel`: OTLP/HTTP export of spans and, through the `metrics` facade, metrics, configured by the standard `OTEL_*` environment |
| `full` | no | `rustls`, `callback-server`, `schemars`, `telemetry` |

## License

MIT or Apache-2.0, at your option.
