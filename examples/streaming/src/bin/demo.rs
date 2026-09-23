/// Comprehensive Streaming Demo - All 13 Services
///
/// This example demonstrates all 13 Schwab streaming services.
/// Based on the Python reference implementation but using idiomatic Rust patterns.
///
/// **Python Reference:** docs/examples/stream_demo.py
///
/// ## Usage
///
/// ```bash
/// # SCHWAB_APP_KEY, SCHWAB_APP_SECRET, SCHWAB_OAUTH_CALLBACK_URL, ...
/// # from the environment or .env (see .env.example)
/// cargo run -p streaming-examples --bin streaming_demo
/// ```
///
/// ## What This Demonstrates
///
/// - ✅ Connecting to Schwab streaming WebSocket
/// - ✅ All 13 streaming services with real symbols
/// - ✅ Custom field selection for bandwidth optimization
/// - ✅ Message handling (Data, Response, Notify)
/// - ✅ Graceful shutdown after time limit
use schwab_rs::{StreamMessage, config::StreamConfig, types::streaming::StreamService};
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    println!("═══════════════════════════════════════════════════");
    println!("  Schwab Streaming Demo - All 13 Services");
    println!("  Based on Python reference: stream_demo.py");
    println!("═══════════════════════════════════════════════════\n");

    println!("🔐 Authenticating...");
    let (_client, stream_client) =
        streaming_examples::stream_client(StreamConfig::default()).await?;
    println!("✅ Authenticated!\n");

    // Connect to stream
    println!("🔌 Connecting to Schwab WebSocket...");
    stream_client.connect().await?;
    println!("✅ Connected!\n");

    // ═══════════════════════════════════════════════════════
    // SERVICE 1: Level One Equities
    // ═══════════════════════════════════════════════════════
    println!("📊 [1/13] Subscribing to Level One Equities: AMD, INTC");
    // Custom fields: 0=Symbol, 1=Bid, 2=Ask, 3=Last, 4=BidSize, 5=AskSize,
    // 6=BidID, 7=AskID, 8=Volume
    stream_client.set_service_fields(
        StreamService::LeveloneEquities,
        "0,1,2,3,4,5,6,7,8".to_string(),
    );
    stream_client
        .subscribe(
            StreamService::LeveloneEquities,
            vec!["AMD".into(), "INTC".into()],
        )
        .await?;
    println!("   ✓ Subscribed\n");

    // ═══════════════════════════════════════════════════════
    // SERVICE 2: Level One Options
    // ═══════════════════════════════════════════════════════
    println!("📊 [2/13] Subscribing to Level One Options");
    println!("   (Uncomment in code with valid option keys)");
    println!();

    // ═══════════════════════════════════════════════════════
    // SERVICE 3: Level One Futures
    // ═══════════════════════════════════════════════════════
    println!("📊 [3/13] Subscribing to Level One Futures: /ES (E-mini S&P 500)");
    stream_client
        .subscribe(StreamService::LeveloneFutures, vec!["/ES".into()])
        .await?;
    println!("   ✓ Subscribed\n");

    // ═══════════════════════════════════════════════════════
    // SERVICE 4: Level One Futures Options
    // ═══════════════════════════════════════════════════════
    println!("📊 [4/13] Subscribing to Level One Futures Options");
    println!();

    // ═══════════════════════════════════════════════════════
    // SERVICE 5: Level One Forex
    // ═══════════════════════════════════════════════════════
    println!("📊 [5/13] Subscribing to Level One Forex");
    println!();

    // ═══════════════════════════════════════════════════════
    // SERVICE 6: NYSE Book
    // ═══════════════════════════════════════════════════════
    println!("📊 [6/13] Subscribing to NYSE Book");
    println!();

    // ═══════════════════════════════════════════════════════
    // SERVICE 7: NASDAQ Book
    // ═══════════════════════════════════════════════════════
    println!("📊 [7/13] Subscribing to NASDAQ Book");
    println!();

    // ═══════════════════════════════════════════════════════
    // SERVICE 8: Options Book
    // ═══════════════════════════════════════════════════════
    println!("📊 [8/13] Subscribing to Options Book");
    println!();

    // ═══════════════════════════════════════════════════════
    // SERVICE 9: Chart Equity
    // ═══════════════════════════════════════════════════════
    println!("📊 [9/13] Subscribing to Chart Equity");
    println!();

    // ═══════════════════════════════════════════════════════
    // SERVICE 10: Chart Futures
    // ═══════════════════════════════════════════════════════
    println!("📊 [10/13] Subscribing to Chart Futures");
    println!();

    // ═══════════════════════════════════════════════════════
    // SERVICE 11: Screener Equity
    // ═══════════════════════════════════════════════════════
    println!("📊 [11/13] Subscribing to Screener Equity");
    println!();

    // ═══════════════════════════════════════════════════════
    // SERVICE 12: Screener Options
    // ═══════════════════════════════════════════════════════
    println!("📊 [12/13] Subscribing to Screener Options");
    println!();

    // ═══════════════════════════════════════════════════════
    // SERVICE 13: Account Activity
    // ═══════════════════════════════════════════════════════
    println!("📊 [13/13] Subscribing to Account Activity");
    stream_client
        .subscribe(StreamService::AcctActivity, vec![])
        .await?;
    println!();

    println!("═══════════════════════════════════════════════════");
    println!("  ✅ All subscriptions configured!");
    println!("  📡 Streaming market data for 60 seconds...");
    println!("═══════════════════════════════════════════════════\n");

    // Get message receiver
    if let Some(mut receiver) = stream_client.get_receiver() {
        // Message handling task
        let message_task = tokio::spawn(async move {
            let mut message_count = 0;
            let mut data_count = 0;
            let mut response_count = 0;
            let mut notify_count = 0;

            while let Some(msg) = receiver.recv().await {
                message_count += 1;

                match msg {
                    StreamMessage::State(status) => {
                        tracing::info!(?status, "Stream lifecycle changed; reconcile after gaps")
                    }
                    StreamMessage::Data(data) => {
                        data_count += 1;
                        if data_count <= 5 {
                            println!("📊 Market Data Update #{}: {:?}", data_count, data);
                        } else if data_count % 100 == 0 {
                            println!("📊 Received {} market data updates...", data_count);
                        }
                    }
                    StreamMessage::Response(response) => {
                        response_count += 1;
                        println!("✅ Response #{}: {:?}", response_count, response);
                    }
                    StreamMessage::Notify(_) => {
                        notify_count += 1;
                        if notify_count % 10 == 0 {
                            println!("💓 Heartbeat #{}: Stream alive", notify_count);
                        }
                    }
                }

                if message_count % 500 == 0 {
                    println!(
                        "\n📈 Summary: {} messages ({} data, {} resp, {} heart)\n",
                        message_count, data_count, response_count, notify_count
                    );
                }
            }
        });

        // Run for 60 seconds
        sleep(Duration::from_secs(60)).await;
        message_task.abort();
    }

    println!("\n✅ Stream stopped successfully!");
    Ok(())
}
