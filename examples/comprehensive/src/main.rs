//! Tour of the SDK: authorize, read quotes and accounts, preview an order
//! and stream Level One quotes for a few seconds.
//!
//! Configuration comes from the `SCHWAB_*` variables (`SchwabConfig::from_env`,
//! see `.env.example`), including the callback URL and token store.

use anyhow::Result;
use schwab_rs::auth::TokenNotification;
use schwab_rs::config::StreamConfig;
use schwab_rs::prelude::*;
use schwab_rs::types::streaming::StreamService;
use schwab_rs::types::trading::{OrderInstruction, OrderRequest, OrderRequestLeg};

use std::sync::Arc;
use tokio::time::Duration;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("schwab_rs=debug".parse()?)
                .add_directive("comprehensive=debug".parse()?),
        )
        .init();

    info!("Starting comprehensive Schwab SDK example");

    if dotenvy::from_filename("../../.env").is_err() {
        let _ = dotenvy::dotenv();
    }

    let mut config = SchwabConfig::from_env()?;
    config.oauth.capture_callback |= cfg!(feature = "callback-server");
    config.oauth.on_token_notification = Some(Arc::new(|notification| match notification {
        TokenNotification::AccessTokenExpiring { seconds_remaining } => {
            info!("Access token expiring in {} seconds", seconds_remaining);
        }
        TokenNotification::RefreshTokenExpiring { hours_remaining } => {
            warn!(
                "Refresh token expiring in {} hours; re-authenticate soon",
                hours_remaining
            );
        }
        TokenNotification::RefreshTokenExpired => {
            error!("Refresh token expired; a full OAuth flow is required");
        }
        TokenNotification::TokenFileCorrupted => {
            warn!("Saved tokens could not be read and will be recreated");
        }
        TokenNotification::SessionRecreated => {
            info!("HTTP session recreated after token refresh");
        }
    }));

    info!("Creating Schwab client...");
    let client = SchwabClient::new(config)?;
    let auth_manager = client.auth_manager();

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
            auth_manager.exchange_code(code.trim().to_string()).await?;
        }
        info!("Authentication successful");
    } else {
        info!("Found existing valid tokens");
    }
    client.init().await?;

    info!("Testing API endpoints...");

    // 1. Quotes
    match client.get_quotes(&["AAPL", "MSFT", "GOOGL"]).await {
        Ok(quotes) => {
            info!("Retrieved quotes");
            for (symbol, quote) in quotes.quotes {
                if let Some(q) = quote.quote {
                    println!("{}: ${:.2}", symbol, q.last_price.unwrap_or(0.0));
                }
            }
        }
        Err(e) => error!("Failed to get quotes: {}", e),
    }

    // 2. Accounts. Trader endpoints take the encrypted hash from
    // `/accounts/accountNumbers`, never the plain account number.
    match client.get_account_numbers().await {
        Ok(accounts) => {
            info!("Retrieved {} linked accounts", accounts.len());
            if let Some(account) = accounts.first() {
                // 3. Preview an order (nothing is placed).
                let order = OrderRequest::limit(
                    OrderRequestLeg::equity(OrderInstruction::Buy, "AAPL", 1),
                    "1.00".parse()?,
                );
                match client.preview_order(&account.hash_value, &order).await {
                    Ok(resp) => info!("Order preview: {}", resp),
                    Err(e) => warn!("Order preview failed: {}", e),
                }
            }
        }
        Err(e) => error!("Failed to get accounts: {}", e),
    }

    // 4. Streaming. The streamer identity comes from user preferences.
    let prefs = client.get_user_preferences().await?;
    let Some(info) = prefs.streamer_info.as_deref().and_then(<[_]>::first) else {
        warn!("No streamer info in user preferences; skipping streaming");
        return Ok(());
    };
    info!("Setting up streaming connection...");
    let stream_client = StreamClient::builder()
        .config(StreamConfig::default())
        .auth_manager(auth_manager)
        .streamer_info(info)
        .build()?;

    match stream_client.connect().await {
        Ok(_) => {
            info!("Stream connected");
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
                                info!("Market data: {} items", data.data.len())
                            }
                            StreamMessage::Response(resp) => {
                                info!("Stream response: {:?}", resp)
                            }
                            StreamMessage::Notify(_) => info!("Heartbeat"),
                            // New message kinds may be added.
                            _ => {}
                        }
                    }
                })
                .await;
            }
        }
        Err(e) => error!("Failed to connect stream: {}", e),
    }

    info!("Comprehensive example completed");
    Ok(())
}
