//! Exercise the OAuth lifecycle against Schwab: authorize, exchange, refresh,
//! inspect and deliberately expire tokens.
//!
//! Configuration comes from the same `SCHWAB_*` variables as the SDK
//! (`SchwabConfig::from_env`, see `.env.example`), including the token store
//! kind, so this tool reads and writes tokens exactly as the SDK does.

use anyhow::Result;
use chrono::{Duration, Utc};
use clap::{Parser, Subcommand};
use schwab_rs::auth::{AuthManager, TokenNotification, open_token_store};
use schwab_rs::client::SchwabClient;
use schwab_rs::config::SchwabConfig;
use std::sync::Arc;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(name = "auth_test")]
#[command(about = "Test Schwab OAuth authentication flow")]
struct Cli {
    /// Allow external callbacks (required for cloudflared tunnels)
    #[arg(long)]
    allow_external_callback: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start OAuth flow (prints the authorization URL)
    Authorize {
        /// Capture callback automatically with local server
        #[arg(long)]
        capture: bool,
    },
    /// Exchange authorization code for tokens
    Exchange {
        /// Authorization code from callback
        code: String,
    },
    /// Refresh the access token now, whether or not it is due
    Refresh,
    /// Check token status
    Status,
    /// Test API access
    TestApi,
    /// Mark saved tokens as expired
    SimulateExpiry {
        /// Also expire the refresh token (forces a new OAuth flow)
        #[arg(long)]
        refresh_token: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    if dotenvy::from_filename("../../.env").is_err() {
        let _ = dotenvy::dotenv();
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("schwab_rs=debug".parse()?)
                .add_directive("auth_test=debug".parse()?),
        )
        .init();

    let cli = Cli::parse();
    let mut config = SchwabConfig::from_env()?;
    config.oauth.allow_external_callback |= cli.allow_external_callback;
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
            error!("Refresh token has expired; a full OAuth flow is required");
        }
        TokenNotification::TokenFileCorrupted => {
            warn!("Saved tokens could not be read and need to be recreated");
        }
        TokenNotification::SessionRecreated => {
            info!("HTTP session recreated after token refresh");
        }
    }));
    let program = std::env::args()
        .next()
        .unwrap_or_else(|| "auth_test".into());
    let rule = "=".repeat(80);

    match cli.command {
        Commands::Authorize { capture } => {
            config.oauth.capture_callback = capture;
            let auth_manager = AuthManager::new(config.oauth)?;

            info!("Starting OAuth authorization flow...");
            let (auth_url, code) = auth_manager.authorize().await?;

            if capture && !code.is_empty() {
                // The code is a credential until exchanged: never print it.
                println!("Authorization captured and tokens saved.");
            } else {
                println!("\n{rule}\nPlease visit this URL to authorize:\n{auth_url}\n{rule}");
                println!("\nAfter authorization, run:\n  {program} exchange <CODE>");
            }
        }

        Commands::Exchange { code } => {
            let auth_manager = AuthManager::new(config.oauth)?;
            info!("Exchanging authorization code for tokens...");
            let tokens = auth_manager.exchange_code(code).await?;
            println!("\n{rule}\nTokens obtained and saved.");
            println!("Access token expires at: {}", tokens.expires_at);
            println!("Refresh token expires at: {}", tokens.refresh_expiry());
            println!("{rule}");
        }

        Commands::Refresh => {
            let auth_manager = AuthManager::new(config.oauth)?;
            // Presenting the current token as rejected refreshes it
            // unconditionally, through the same transaction a 401 uses.
            let current = auth_manager.get_access_token().await?;
            match auth_manager.refresh_rejected_token(&current).await {
                Ok(()) => {
                    let store = open_token_store(auth_manager.config());
                    let tokens = store.load()?;
                    println!("\n{rule}\nToken refreshed.");
                    println!("Access token expires at: {}", tokens.expires_at);
                    println!("{rule}");
                }
                Err(e) => {
                    error!("Token refresh failed: {}", e);
                    println!("\n{rule}\nToken refresh failed: {e}");
                    println!("You may need to restart the OAuth flow.\n{rule}");
                }
            }
        }

        Commands::Status => {
            println!("\n{rule}");
            match open_token_store(&config.oauth).load() {
                Ok(tokens) => {
                    let now = Utc::now();
                    let access_left = tokens.expires_at - now;
                    println!(
                        "Access token: {} (expires {}, {} minutes)",
                        if tokens.is_valid() {
                            "VALID"
                        } else {
                            "EXPIRED"
                        },
                        tokens.expires_at,
                        access_left.num_minutes()
                    );
                    let refresh_left = tokens.refresh_expiry() - now;
                    println!(
                        "Refresh token: {} (expires {}, {} days {} hours)",
                        if tokens.refresh_token_expired() {
                            "EXPIRED"
                        } else {
                            "VALID"
                        },
                        tokens.refresh_expiry(),
                        refresh_left.num_days(),
                        refresh_left.num_hours() % 24
                    );
                }
                Err(e) => {
                    println!("No readable tokens: {e}");
                    println!("Run '{program} authorize' to start the OAuth flow.");
                }
            }
            println!("{rule}");
        }

        Commands::TestApi => {
            info!("Creating Schwab client...");
            let client = SchwabClient::new(config)?;
            info!("Testing API access with a simple request...");
            match client.get_quotes(&["SPY"]).await {
                Ok(quotes) => {
                    println!("\n{rule}\nAPI access successful! Retrieved quotes for SPY");
                    if let Ok(json) = serde_json::to_string_pretty(&quotes) {
                        println!("Response (truncated):");
                        for line in json.lines().take(10) {
                            println!("{line}");
                        }
                        println!("...");
                    }
                    println!("{rule}");
                }
                Err(e) => {
                    error!("API request failed: {}", e);
                    println!("\n{rule}\nAPI access failed: {e}");
                    println!("Check your tokens and try refreshing or reauthorizing.\n{rule}");
                }
            }
        }

        Commands::SimulateExpiry { refresh_token } => {
            let store = open_token_store(&config.oauth);
            let mut tokens = match store.load() {
                Ok(tokens) => tokens,
                Err(e) => {
                    error!("No readable tokens: {e}");
                    println!("Run '{program} authorize' first to obtain tokens.");
                    return Ok(());
                }
            };
            let now = Utc::now();
            tokens.expires_at = now - Duration::minutes(1);
            if refresh_token {
                tokens.refresh_token_expires_at = Some(now - Duration::minutes(1));
            }
            store.save(&tokens)?;

            println!("\n{rule}\nToken expiry simulated:");
            println!("- Access token marked as expired");
            if refresh_token {
                println!("- Refresh token marked as expired");
            }
            println!("\nNow try running:\n  {program} refresh\n  {program} test-api\n{rule}");
        }
    }

    Ok(())
}
