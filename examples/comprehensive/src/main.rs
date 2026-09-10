use anyhow::Result;
use schwab_rs::auth::{AuthManager, OAuthConfig, TokenNotification, TokenStoreKind};
use schwab_rs::prelude::*;
use schwab_rs::streaming::{StreamClient, StreamMessage, StreamerIdentity};
use schwab_rs::types::streaming::StreamService;

use std::sync::Arc;
use tokio::time::Duration;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("schwab_rs=debug".parse()?)
                .add_directive("comprehensive=debug".parse()?),
        )
        .init();

    info!("Starting comprehensive Schwab SDK example");

    // Load .env file if it exists
    if dotenvy::from_filename("../../.env").is_ok() {
        info!("Loaded environment variables from .env file");
    } else if dotenvy::dotenv().is_ok() {
        info!("Loaded environment variables from default .env location");
    }

    // Load configuration from environment
    let app_key = std::env::var("SCHWAB_APP_KEY")?;
    let app_secret = std::env::var("SCHWAB_APP_SECRET")?;
    let callback_url = std::env::var("SCHWAB_CALLBACK_URL")
        .unwrap_or_else(|_| "https://127.0.0.1:8080".to_string());

    // Create OAuth config
    let oauth_config = OAuthConfig {
        app_key: app_key.clone(),
        app_secret: app_secret.clone(),
        callback_url: callback_url.clone(),
        tokens_file: "schwab_tokens.json".into(),
        capture_callback: cfg!(feature = "callback-server"),
        auto_refresh: true,
        refresh_buffer_seconds: 61,
        // PKCE is enabled by default in v0.1.0+
        token_store_kind: TokenStoreKind::File,
        on_token_notification: Some(Arc::new(|notification| match notification {
            TokenNotification::AccessTokenExpiring { seconds_remaining } => {
                info!("⏰ Access token expiring in {} seconds", seconds_remaining);
            }
            TokenNotification::RefreshTokenExpiring { hours_remaining } => {
                warn!(
                    "⚠️ Refresh token expiring in {} hours! Re-authenticate soon.",
                    hours_remaining
                );
            }
            TokenNotification::RefreshTokenExpired => {
                error!("❌ Refresh token expired! Full OAuth flow required.");
            }
            TokenNotification::TokenFileCorrupted => {
                warn!("⚠️ Token file was corrupted, recreating...");
            }
            TokenNotification::SessionRecreated => {
                info!("🔄 HTTP session recreated after token refresh");
            }
        })),
        ..OAuthConfig::default()
    };

    // Pre-run OAuth if needed using AuthManager directly
    let auth_manager = AuthManager::new(oauth_config.clone())?;

    if !auth_manager.has_valid_tokens() {
        info!("Starting OAuth flow...");
        let (auth_url, captured_code) = auth_manager.authorize().await?;

        if captured_code.is_empty() {
            println!("\nPlease visit this URL to authorize:");
            println!("{}", auth_url);
            println!("\nAfter authorization, copy the code from the callback URL");

            print!("Enter authorization code: ");
            use std::io::{self, Write};
            io::stdout().flush()?;

            let mut code = String::new();
            io::stdin().read_line(&mut code)?;
            let code = code.trim().to_string();

            auth_manager.exchange_code(code).await?;
        }
        info!("✅ Authentication successful");
    } else {
        info!("✅ Found existing valid tokens");
    }

    let config = SchwabConfig {
        oauth: oauth_config,
        ..Default::default()
    };

    info!("Creating Schwab client...");
    let client = SchwabClient::new(config)?;
    client.init().await?;

    // Demonstrate API usage
    info!("Testing API endpoints...");

    // 1. Get quotes
    match client.get_quotes(&["AAPL", "MSFT", "GOOGL"]).await {
        Ok(quotes) => {
            info!("✅ Successfully retrieved quotes");
            for (symbol, quote) in quotes.quotes {
                if let Some(q) = quote.quote {
                    println!("{}: ${:.2}", symbol, q.last_price.unwrap_or(0.0));
                }
            }
        }
        Err(e) => error!("Failed to get quotes: {}", e),
    }

    // 2. Get accounts
    match client.get_accounts(false).await {
        Ok(accounts) => {
            info!("✅ Successfully retrieved {} accounts", accounts.len());
            if let Some(account) = accounts.first() {
                let hash = &account.securities_account.account_number; // This is actually the hash in API
                info!("Using account hash: {}", hash);

                // 3. Preview an order
                let order: schwab_rs::types::trading::OrderRequest = serde_json::from_value(
                    serde_json::json!({
                        "session":"NORMAL", "duration":"DAY", "orderType":"LIMIT", "price":1.0,
                        "orderStrategyType":"SINGLE", "orderLegCollection":[{"instruction":"BUY", "quantity":1,
                        "instrument":{"assetType":"EQUITY", "symbol":"AAPL"}}]
                    }),
                )?;

                match client.preview_order(hash, &order).await {
                    Ok(resp) => info!("✅ Order preview success: {}", resp),
                    Err(e) => warn!(
                        "Order preview failed (expected if sandbox or no funds): {}",
                        e
                    ),
                }
            }
        }
        Err(e) => error!("Failed to get accounts: {}", e),
    }

    // 4. Demonstrate streaming
    if let Some(stream_config) = SchwabConfig::default().streaming {
        info!("Setting up streaming connection...");

        // In a real app, build the identity from `get_user_preferences()`
        // via `StreamerIdentity::from_streamer_info` — the demo values
        // below will not authenticate against the live streamer.
        let stream_client = StreamClient::new(
            stream_config,
            auth_manager.clone(),
            StreamerIdentity::new("demo_customer_id", "demo_correl_id"),
        )?;

        match stream_client.connect().await {
            Ok(_) => {
                info!("✅ Stream connected");
                stream_client
                    .subscribe(
                        StreamService::LeveloneEquities,
                        vec!["AAPL".into(), "MSFT".into()],
                    )
                    .await?;

                if let Some(mut receiver) = stream_client.get_receiver() {
                    info!("Listening for messages (5 seconds)...");
                    let _ = tokio::time::timeout(Duration::from_secs(5), async {
                        while let Some(msg) = receiver.recv().await {
                            match msg {
                                StreamMessage::State(status) => {
                                    info!(?status, "Stream health changed")
                                }
                                StreamMessage::Data(data) => {
                                    info!("📊 Market data: {} items", data.data.len())
                                }
                                StreamMessage::Response(resp) => {
                                    info!("📨 Stream response: {:?}", resp)
                                }
                                StreamMessage::Notify(_) => info!("💓 Heartbeat"),
                            }
                        }
                    })
                    .await;
                }
            }
            Err(e) => error!("Failed to connect stream: {}", e),
        }
    }

    info!("\n✅ Comprehensive example completed!");
    Ok(())
}
