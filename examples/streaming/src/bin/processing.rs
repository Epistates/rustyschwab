/// Streaming Data Processing Pattern
///
/// This example demonstrates how to process streaming data efficiently.
/// Uses channels for async processing to decouple receiving from heavy computation.
///
/// ## Usage
///
/// ```bash
/// # SCHWAB_APP_KEY, SCHWAB_APP_SECRET, SCHWAB_OAUTH_CALLBACK_URL, ...
/// # from the environment or .env (see .env.example)
/// cargo run -p streaming-examples --bin streaming_processing
/// ```
use schwab_rs::{
    StreamMessage,
    config::{ChannelKind, StreamConfig},
    types::streaming::StreamService,
};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    println!("═══════════════════════════════════════════════════");
    println!("  Streaming Data Processing Pattern");
    println!("═══════════════════════════════════════════════════\n");

    // A bounded channel for backpressure.
    println!("🔐 Authenticating...");
    let stream_config = StreamConfig {
        channel_kind: ChannelKind::Bounded(1000),
        ..Default::default()
    };
    let (_client, stream_client) = streaming_examples::stream_client(stream_config).await?;
    println!("✅ Authenticated!\n");

    // Connect and subscribe
    stream_client.connect().await?;
    stream_client
        .subscribe(
            StreamService::LeveloneEquities,
            vec!["AMD".into(), "INTC".into()],
        )
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
                    for item in &data.data {
                        // Name the numbered fields; an unknown service keeps
                        // its numbers.
                        let table = item.service().map_or(&[][..], StreamService::fields);
                        for content in &item.content {
                            let processed = ProcessedData {
                                service: item.service.clone(),
                                symbol: content.key.clone(),
                                timestamp: item.timestamp,
                                fields: content.named_fields(table),
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
    fields: serde_json::Map<String, serde_json::Value>,
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
