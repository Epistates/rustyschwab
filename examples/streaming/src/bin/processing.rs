/// Streaming Data Processing Pattern
///
/// This example demonstrates how to process streaming data efficiently.
/// Uses channels for async processing to decouple receiving from heavy computation.
///
/// ## Usage
///
/// ```bash
/// export SCHWAB_APP_KEY="your_32_character_app_key"
/// export SCHWAB_APP_SECRET="your_16_char_secret"
/// export SCHWAB_CALLBACK_URL="https://localhost:8080"
///
/// cargo run --example streaming_processing --features callback-server
/// ```

use schwab_rs::{
    auth::{AuthManager, OAuthConfig},
    config::{ChannelKind, StreamConfig},
    StreamClient, StreamMessage,
    types::streaming::StreamService,
};
use std::env;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use std::collections::HashMap;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    println!("═══════════════════════════════════════════════════");
    println!("  Streaming Data Processing Pattern");
    println!("═══════════════════════════════════════════════════\n");

    // Load configuration
    let app_key = env::var("SCHWAB_APP_KEY").expect("SCHWAB_APP_KEY required");
    let app_secret = env::var("SCHWAB_APP_SECRET").expect("SCHWAB_APP_SECRET required");
    let callback_url = env::var("SCHWAB_CALLBACK_URL")
        .unwrap_or_else(|_| "https://127.0.0.1:8080".to_string());

    // Create OAuth config
    let oauth_config = OAuthConfig {
        app_key,
        app_secret,
        callback_url,
        capture_callback: true,
        auto_refresh: true,
        ..Default::default()
    };

    // Authenticate and get customer ID
    println!("🔐 Authenticating...");
    let auth_manager = AuthManager::new(oauth_config)?;
    let client = schwab_rs::SchwabClient::new(schwab_rs::SchwabConfig {
        oauth: auth_manager.config().clone(),
        ..Default::default()
    })?;
    client.init().await?;

    let prefs = client.get_user_preferences().await?;
    let customer_id = prefs
        .streamer_info
        .and_then(|info| info.first().map(|i| i.schwab_client_customer_id.clone()))
        .ok_or("Could not get customer ID")?;

    println!("✅ Authenticated!\n");

    // Create streaming client with BOUNDED channel for backpressure
    let mut stream_config = StreamConfig::default();
    stream_config.channel_kind = ChannelKind::Bounded(1000);

    let stream_client = StreamClient::builder()
        .config(stream_config)
        .auth_manager(auth_manager)
        .customer_id(customer_id)
        .build()?;

    // Connect and subscribe
    stream_client.connect().await?;
    stream_client
        .subscribe(StreamService::LeveloneEquities, vec!["AMD".into(), "INTC".into()])
        .await?;

    // Create processing channel
    let (processing_tx, mut processing_rx) = mpsc::channel::<ProcessedData>(100);

    // Get message receiver
    if let Some(mut receiver) = stream_client.get_receiver() {
        let tx = processing_tx.clone();
        // Task 1: Receive from stream and enqueue for processing
        let receive_task = tokio::spawn(async move {
            while let Some(msg) = receiver.recv().await {
                if let StreamMessage::Data(data) = msg {
                    for service in &data.data {
                        let service_type = service.service.clone();
                        let timestamp = service.timestamp;
                        for content in &service.content {
                            let processed = ProcessedData {
                                service: service_type.clone(),
                                symbol: content.key.clone(),
                                timestamp,
                                fields: content.fields.clone(),
                            };
                            if tx.send(processed).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        });

        // Task 2: Process data from channel (simulates computation)
        let process_task = tokio::spawn(async move {
            let mut processed_count = 0;
            while let Some(data) = processing_rx.recv().await {
                processed_count += 1;
                if processed_count <= 10 || processed_count % 50 == 0 {
                    print_market_data(&data);
                }
                sleep(Duration::from_millis(10)).await;
            }
        });

        // Run for 30 seconds
        sleep(Duration::from_secs(30)).await;
        receive_task.abort();
        drop(processing_tx);
        let _ = process_task.await;
    }

    println!("\n✅ Shutdown complete!");
    Ok(())
}

#[derive(Debug, Clone)]
struct ProcessedData {
    service: String,
    symbol: String,
    timestamp: i64,
    fields: HashMap<String, serde_json::Value>,
}

fn print_market_data(data: &ProcessedData) {
    use chrono::{DateTime, Utc};
    let dt = DateTime::<Utc>::from_timestamp(data.timestamp / 1000, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "Unknown".to_string());

    println!("📈 [{} - {}] ({})", data.service, data.symbol, dt);
    for (key, value) in &data.fields {
        println!("   {}: {}", key, value);
    }
}
