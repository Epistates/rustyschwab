use anyhow::Result;
use schwab_rs::prelude::*;
use std::io::{self, Write};
use tracing::{Level, info};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();

    // Load .env file if it exists
    if dotenvy::from_filename("../../.env").is_ok() {
        info!("Loaded environment variables from .env file");
    } else if dotenvy::dotenv().is_ok() {
        info!("Loaded environment variables from default .env location");
    }

    info!("Starting Schwab OAuth flow example");
    info!("=====================================");

    // Initialize configuration from environment
    let mut config = match SchwabConfig::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Failed to load configuration from environment: {}", e);
            eprintln!("\nPlease set the following environment variables:");
            eprintln!("  SCHWAB_APP_KEY     - Your Schwab app key");
            eprintln!("  SCHWAB_APP_SECRET  - Your Schwab app secret");
            eprintln!(
                "  SCHWAB_OAUTH_CALLBACK_URL - Your callback URL (e.g., https://127.0.0.1:8080)"
            );
            eprintln!("\nOptional (see .env.example for the full list):");
            eprintln!("  SCHWAB_TOKENS_FILE - Path to store tokens (default: schwab_tokens.json)");
            eprintln!("  SCHWAB_TOKEN_STORE - file, encrypted_file or keychain");
            std::process::exit(1);
        }
    };

    // For this example, we'll try to use the built-in callback server if the feature is enabled
    if cfg!(feature = "callback-server") {
        config.oauth.capture_callback = true;
        info!("Built-in callback server enabled (feature 'callback-server' is active)");
    }

    // Create auth manager
    let auth_manager = AuthManager::new(config.oauth.clone())?;

    // Usable tokens, refreshing the 30-minute access token if needed: only
    // a missing, expired or refused refresh token needs the browser.
    if auth_manager.ensure_valid_tokens().await.is_ok() {
        info!("Found existing valid tokens, skipping OAuth flow");
    } else {
        info!("No valid tokens found, starting OAuth flow");

        // Start OAuth flow
        let (auth_url, captured_code) = auth_manager.authorize().await?;

        println!("\n🔐 OAuth Authorization Required");
        println!("================================");

        if !captured_code.is_empty() {
            println!("\n✅ Authorization successful!");
            println!("Code captured automatically by built-in server.");
        } else {
            println!("\nPlease open this URL in your browser:");
            println!("\n{}\n", auth_url);

            println!("After authorizing, you'll be redirected to your callback URL.");
            println!("Copy the 'code' parameter from the redirect URL.");
            println!("Example: https://127.0.0.1:8080?code=YOUR_CODE_HERE\n");

            print!("Enter the authorization code: ");
            io::stdout().flush()?;

            let mut code = String::new();
            io::stdin().read_line(&mut code)?;
            let code = code.trim().to_string();

            if code.is_empty() {
                eprintln!("Error: No code provided");
                std::process::exit(1);
            }

            println!("\n🔄 Exchanging authorization code for tokens...");
            auth_manager.exchange_code(code).await?;
        }

        info!("✅ OAuth flow completed successfully!");
    }

    // Initialize the client
    info!("\n📊 Initializing Schwab client...");
    let client = SchwabClient::new(config)?;
    client.init().await?;

    // Test the client with a simple API call
    info!("\n🔍 Testing API access...");
    println!("\nFetching quotes for SPY, AAPL...\n");

    match client.get_quotes(&["SPY", "AAPL"]).await {
        Ok(quotes) => {
            println!("📈 Market Quotes:");
            println!("==================");
            for (symbol, quote) in quotes.quotes {
                if let Some(q) = quote.quote {
                    println!(
                        "{}: ${:.2} ({}${:.2})",
                        symbol,
                        q.last_price.unwrap_or(0.0),
                        if q.net_change.unwrap_or(0.0) >= 0.0 {
                            "+"
                        } else {
                            ""
                        },
                        q.net_change.unwrap_or(0.0)
                    );
                }
            }
            println!("\n✅ API access successful!");
        }
        Err(e) => {
            eprintln!("❌ Failed to fetch quotes: {}", e);
        }
    }

    Ok(())
}
