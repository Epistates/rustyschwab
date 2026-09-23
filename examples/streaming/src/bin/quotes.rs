/// Simple Real-Time Quotes Example
///
/// This example demonstrates the most common streaming use case:
/// Getting real-time stock quotes for a watchlist.
///
/// ## Usage
///
/// ```bash
/// # SCHWAB_APP_KEY, SCHWAB_APP_SECRET, SCHWAB_OAUTH_CALLBACK_URL, ...
/// # from the environment or .env (see .env.example)
/// cargo run -p streaming-examples --bin streaming_quotes
/// ```
use schwab_rs::{
    StreamMessage,
    config::StreamConfig,
    types::streaming::{LevelOneEquity, LevelOneEquityFields, StreamService},
};
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    println!("\n╔═══════════════════════════════════════╗");
    println!("║   Real-Time Stock Quotes Streamer    ║");
    println!("╚═══════════════════════════════════════╝\n");

    println!("🔐 Authenticating...");
    let (_client, stream_client) =
        streaming_examples::stream_client(StreamConfig::default()).await?;

    // Connect
    println!("🔌 Connecting to stream...");
    stream_client.connect().await?;
    println!("✅ Connected!\n");

    // Define watchlist
    let watchlist = vec![
        "AAPL".to_string(),
        "MSFT".to_string(),
        "GOOGL".to_string(),
        "TSLA".to_string(),
        "AMD".to_string(),
        "NVDA".to_string(),
    ];

    // Request only the fields the table shows, by name rather than number.
    let fields = [
        LevelOneEquityFields::SYMBOL,
        LevelOneEquityFields::BID_PRICE,
        LevelOneEquityFields::ASK_PRICE,
        LevelOneEquityFields::LAST_PRICE,
        LevelOneEquityFields::TOTAL_VOLUME,
        LevelOneEquityFields::OPEN_PRICE,
        LevelOneEquityFields::NET_CHANGE,
    ]
    .map(|field| field.to_string())
    .join(",");
    stream_client.set_service_fields(StreamService::LeveloneEquities, fields);

    println!("📊 Subscribing to watchlist:");
    for symbol in &watchlist {
        println!("   • {}", symbol);
    }
    println!();

    stream_client
        .subscribe(StreamService::LeveloneEquities, watchlist)
        .await?;

    // Get receiver
    if let Some(mut receiver) = stream_client.get_receiver() {
        println!("🎯 Streaming quotes (press Ctrl+C to stop)...\n");
        println!("╔════════╦═══════╦═══════╦═══════╦═══════════╦═══════╦════════╗");
        println!("║ Symbol ║  Bid  ║  Ask  ║  Last ║   Volume  ║  Open ║ Change ║");
        println!("╠════════╬═══════╬═══════╬═══════╬═══════════╬═══════╬════════╣");

        let message_task = tokio::spawn(async move {
            while let Some(msg) = receiver.recv().await {
                if let StreamMessage::Data(data) = msg {
                    for item in &data.data {
                        if item.service() != Some(StreamService::LeveloneEquities) {
                            continue;
                        }
                        for content in &item.content {
                            match LevelOneEquity::from_content(content) {
                                Ok(quote) => display_quote(&quote),
                                Err(e) => eprintln!("undecodable quote for {}: {e}", content.key),
                            }
                        }
                    }
                }
            }
        });

        // Run for 60 seconds
        sleep(Duration::from_secs(60)).await;
        message_task.abort();
    }

    println!("╚════════╩═══════╩═══════╩═══════╩═══════════╩═══════╩════════╝");
    println!("\n✅ Stream stopped");

    Ok(())
}

/// Deltas carry only the fields that changed; a missing field prints as 0.
fn display_quote(quote: &LevelOneEquity) {
    println!(
        "║ {:6} ║ {:5.2} ║ {:5.2} ║ {:5.2} ║ {:9} ║ {:5.2} ║ {:+6.2} ║",
        quote.key,
        quote.bid_price.unwrap_or(0.0),
        quote.ask_price.unwrap_or(0.0),
        quote.last_price.unwrap_or(0.0),
        quote.total_volume.unwrap_or(0),
        quote.open_price.unwrap_or(0.0),
        quote.net_change.unwrap_or(0.0)
    );
}
