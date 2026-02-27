/// Simple Real-Time Quotes Example
///
/// This example demonstrates the most common streaming use case:
/// Getting real-time stock quotes for a watchlist.
///
/// ## Usage
///
/// ```bash
/// export SCHWAB_APP_KEY="your_32_character_app_key"
/// export SCHWAB_APP_SECRET="your_16_char_secret"
/// export SCHWAB_CALLBACK_URL="https://localhost:8080"
///
/// cargo run --example streaming_quotes --features callback-server
/// ```

use schwab_rs::{
    auth::{AuthManager, OAuthConfig},
    config::StreamConfig,
    StreamClient, StreamMessage,
    types::streaming::StreamService,
};
use std::collections::HashMap;
use std::env;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    println!("\n╔═══════════════════════════════════════╗");
    println!("║   Real-Time Stock Quotes Streamer    ║");
    println!("╚═══════════════════════════════════════╝\n");

    // Configuration
    let oauth_config = OAuthConfig {
        app_key: env::var("SCHWAB_APP_KEY").expect("SCHWAB_APP_KEY required"),
        app_secret: env::var("SCHWAB_APP_SECRET").expect("SCHWAB_APP_SECRET required"),
        callback_url: env::var("SCHWAB_CALLBACK_URL")
            .unwrap_or_else(|_| "https://localhost:8080".to_string()),
        capture_callback: true,
        auto_refresh: true,
        ..Default::default()
    };

    // Authenticate
    println!("🔐 Authenticating...");
    let auth_manager = AuthManager::new(oauth_config)?;
    let client = schwab_rs::SchwabClient::new(schwab_rs::SchwabConfig {
        oauth: auth_manager.config().clone(),
        ..Default::default()
    })?;
    client.init().await?;

    // Get customer ID
    let prefs = client.get_user_preferences().await?;
    let customer_id = prefs
        .streamer_info
        .and_then(|info| info.first().map(|i| i.schwab_client_customer_id.clone()))
        .ok_or("Could not get customer ID")?;

    // Create stream
    let stream_client = StreamClient::builder()
        .config(StreamConfig::default())
        .auth_manager(auth_manager)
        .customer_id(customer_id)
        .build()?;

    // Connect
    println!("🔌 Connecting to stream...");
    stream_client.connect().await?;
    println!("✅ Connected!\n");

    // Define watchlist
    let watchlist = vec!["AAPL".to_string(), "MSFT".to_string(), "GOOGL".to_string(), "TSLA".to_string(), "AMD".to_string(), "NVDA".to_string()];

    // Request only essential fields for quotes
    stream_client.set_service_fields(
        StreamService::LeveloneEquities,
        "0,1,2,3,8,28,29".to_string(),
    );

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

        // Track latest quotes
        let mut _quotes: HashMap<String, Quote> = HashMap::new();

        let message_task = tokio::spawn(async move {
            while let Some(msg) = receiver.recv().await {
                if let StreamMessage::Data(data) = msg {
                    for service in &data.data {
                        for content in &service.content {
                            if let Some(quote) = Quote::from_content(content) {
                                display_quote(&quote);
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

#[derive(Debug, Clone)]
struct Quote {
    symbol: String,
    bid: Option<f64>,
    ask: Option<f64>,
    last: Option<f64>,
    volume: Option<i64>,
    open: Option<f64>,
    net_change: Option<f64>,
}

impl Quote {
    fn from_content(content: &schwab_rs::types::streaming::DataContent) -> Option<Self> {
        let symbol = content.key.clone();

        Some(Quote {
            symbol,
            bid: content.fields.get("1").and_then(|v| v.as_f64()),
            ask: content.fields.get("2").and_then(|v| v.as_f64()),
            last: content.fields.get("3").and_then(|v| v.as_f64()),
            volume: content.fields.get("8").and_then(|v| v.as_i64()),
            open: content.fields.get("28").and_then(|v| v.as_f64()),
            net_change: content.fields.get("29").and_then(|v| v.as_f64()),
        })
    }
}

fn display_quote(quote: &Quote) {
    println!(
        "║ {:6} ║ {:5.2} ║ {:5.2} ║ {:5.2} ║ {:9} ║ {:5.2} ║ {:+6.2} ║",
        quote.symbol,
        quote.bid.unwrap_or(0.0),
        quote.ask.unwrap_or(0.0),
        quote.last.unwrap_or(0.0),
        quote.volume.unwrap_or(0),
        quote.open.unwrap_or(0.0),
        quote.net_change.unwrap_or(0.0)
    );
}
