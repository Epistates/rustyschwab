use anyhow::Result;
use chrono::{Duration, Utc};
use clap::{Parser, Subcommand};
use schwab_rs::auth::{AuthManager, OAuthConfig, TokenNotification};
use schwab_rs::client::SchwabClient;
use schwab_rs::config::SchwabConfig;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(name = "auth_test")]
#[command(about = "Test Schwab OAuth authentication flow")]
struct Cli {
    /// App key (32 characters)
    #[arg(short = 'k', long, env = "SCHWAB_APP_KEY", hide_env_values = true)]
    app_key: String,

    /// App secret (16 characters)
    #[arg(short = 's', long, env = "SCHWAB_APP_SECRET", hide_env_values = true)]
    app_secret: String,

    /// Callback URL (must match registered URL exactly)
    #[arg(short = 'c', long, default_value = "https://127.0.0.1:8080", env = "SCHWAB_CALLBACK_URL")]
    callback_url: String,

    /// Path to tokens file
    #[arg(short = 't', long, default_value = "schwab_tokens.json")]
    tokens_file: PathBuf,

    /// Use sandbox environment
    #[arg(long)]
    sandbox: bool,

    /// Allow external callbacks (required for cloudflared tunnels)
    #[arg(long)]
    allow_external_callback: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start OAuth flow (opens browser for authorization)
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
    /// Test token refresh
    Refresh,
    /// Check token status
    Status,
    /// Test API access
    TestApi,
    /// Simulate expired tokens
    SimulateExpiry {
        /// Expire access token only
        #[arg(long)]
        access_only: bool,
        /// Expire refresh token (7 days)
        #[arg(long)]
        refresh_token: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if it exists
    if let Ok(_) = dotenvy::from_filename("../../.env") {
        info!("Loaded environment variables from .env file");
    } else if let Ok(_) = dotenvy::dotenv() {
        info!("Loaded environment variables from default .env location");
    }
    
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("schwab_rs=debug".parse()?)
                .add_directive("auth_test=debug".parse()?)
        )
        .init();

    let cli = Cli::parse();
    
    // Validate key lengths
    if cli.app_key.len() != 32 {
        error!("App key must be exactly 32 characters, got {}", cli.app_key.len());
        return Err(anyhow::anyhow!("Invalid app key length"));
    }
    
    if cli.app_secret.len() != 16 {
        error!("App secret must be exactly 16 characters, got {}", cli.app_secret.len());
        return Err(anyhow::anyhow!("Invalid app secret length"));
    }
    
    // Create OAuth config with notification callback
    let mut oauth_config = OAuthConfig {
        app_key: cli.app_key.clone(),
        app_secret: cli.app_secret.clone(),
        callback_url: cli.callback_url.clone(),
        auth_url: "https://api.schwabapi.com/v1/oauth/authorize".to_string(),
        token_url: "https://api.schwabapi.com/v1/oauth/token".to_string(),
        tokens_file: cli.tokens_file.clone(),
        capture_callback: false,
        allow_external_callback: cli.allow_external_callback,
        auto_refresh: true,
        refresh_buffer_seconds: 61, // Refresh 61 seconds before expiry
        on_token_notification: Some(Arc::new(|notification| {
            match notification {
                TokenNotification::AccessTokenExpiring { seconds_remaining } => {
                    info!("⏰ Access token expiring in {} seconds", seconds_remaining);
                }
                TokenNotification::RefreshTokenExpiring { hours_remaining } => {
                    warn!("⚠️  Refresh token expiring in {} hours! Please re-authenticate soon.", hours_remaining);
                }
                TokenNotification::RefreshTokenExpired => {
                    error!("❌ Refresh token has expired! Full OAuth flow required.");
                }
                TokenNotification::TokenFileCorrupted => {
                    warn!("⚠️  Token file was corrupted and needs to be recreated.");
                }
                TokenNotification::SessionRecreated => {
                    info!("🔄 HTTP session recreated after token refresh.");
                }
            }
        })),
        ..Default::default()
    };
    
    if cli.sandbox {
        oauth_config.auth_url = "https://api.schwabapi.com/v1/oauth/authorize".to_string();
        oauth_config.token_url = "https://api.schwabapi.com/v1/oauth/token".to_string();
    }
    
    match cli.command {
        Commands::Authorize { capture } => {
            oauth_config.capture_callback = capture;
            let auth_manager = AuthManager::new(oauth_config)?;
            
            info!("Starting OAuth authorization flow...");
            let (auth_url, code) = auth_manager.authorize().await?;
            
            if capture && !code.is_empty() {
                info!("Authorization successful! Code captured: {}", code);
                info!("Tokens have been saved to {:?}", cli.tokens_file);
            } else {
                println!("\n{}\n", "=".repeat(80));
                println!("Please visit this URL to authorize:");
                println!("{}", auth_url);
                println!("{}", "=".repeat(80));
                println!("\nAfter authorization, run:");
                println!("  {} exchange <CODE>", std::env::args().next().unwrap());
            }
        }
        
        Commands::Exchange { code } => {
            let auth_manager = AuthManager::new(oauth_config)?;
            
            info!("Exchanging authorization code for tokens...");
            let tokens = auth_manager.exchange_code(code).await?;
            
            println!("\n{}\n", "=".repeat(80));
            println!("Tokens obtained successfully!");
            println!("Access token expires at: {}", tokens.expires_at);
            println!("Refresh token expires at: {}", 
                tokens.refresh_token_expires_at
                    .map(|dt| dt.to_string())
                    .unwrap_or_else(|| "Not set".to_string())
            );
            println!("Tokens saved to: {:?}", cli.tokens_file);
            println!("{}", "=".repeat(80));
        }
        
        Commands::Refresh => {
            let auth_manager = AuthManager::new(oauth_config)?;
            auth_manager.start().await?;
            
            info!("Testing token refresh...");
            
            // Force a refresh by calling ensure_valid_tokens
            match auth_manager.ensure_valid_tokens().await {
                Ok(_) => {
                    info!("Token refresh successful!");
                    let access_token = auth_manager.get_access_token().await?;
                    println!("\n{}\n", "=".repeat(80));
                    println!("Token refreshed successfully!");
                    println!("New access token (first 20 chars): {}...", &access_token[..20.min(access_token.len())]);
                    println!("{}", "=".repeat(80));
                }
                Err(e) => {
                    error!("Token refresh failed: {}", e);
                    println!("\n{}\n", "=".repeat(80));
                    println!("Token refresh failed!");
                    println!("Error: {}", e);
                    println!("You may need to restart the OAuth flow.");
                    println!("{}", "=".repeat(80));
                }
            }
        }
        
        Commands::Status => {
            let auth_manager = AuthManager::new(oauth_config)?;
            auth_manager.start().await?;
            
            println!("\n{}\n", "=".repeat(80));
            
            if auth_manager.has_valid_tokens() {
                // Load and check tokens
                if let Ok(json) = std::fs::read_to_string(&cli.tokens_file) {
                    if let Ok(tokens) = serde_json::from_str::<serde_json::Value>(&json) {
                        let now = Utc::now();
                        
                        if let Some(expires_at) = tokens.get("expires_at").and_then(|v| v.as_str()) {
                            if let Ok(expires) = expires_at.parse::<chrono::DateTime<Utc>>() {
                                let remaining = expires - now;
                                println!("Access token status: VALID");
                                println!("  Expires at: {}", expires);
                                println!("  Time remaining: {} minutes", remaining.num_minutes());
                            }
                        }
                        
                        if let Some(refresh_expires) = tokens.get("refresh_token_expires_at").and_then(|v| v.as_str()) {
                            if let Ok(expires) = refresh_expires.parse::<chrono::DateTime<Utc>>() {
                                let remaining = expires - now;
                                println!("\nRefresh token status: {}", 
                                    if remaining.num_seconds() > 0 { "VALID" } else { "EXPIRED" });
                                println!("  Expires at: {}", expires);
                                println!("  Time remaining: {} days, {} hours", 
                                    remaining.num_days(), 
                                    remaining.num_hours() % 24);
                            }
                        } else if let Some(issued_at) = tokens.get("issued_at").and_then(|v| v.as_str()) {
                            if let Ok(issued) = issued_at.parse::<chrono::DateTime<Utc>>() {
                                let expires = issued + Duration::days(7);
                                let remaining = expires - now;
                                println!("\nRefresh token status: {}", 
                                    if remaining.num_seconds() > 0 { "VALID" } else { "EXPIRED" });
                                println!("  Expires at: {} (estimated)", expires);
                                println!("  Time remaining: {} days, {} hours", 
                                    remaining.num_days(), 
                                    remaining.num_hours() % 24);
                            }
                        }
                    }
                }
            } else {
                println!("No valid tokens found.");
                println!("Run 'authorize' command to start OAuth flow.");
            }
            
            println!("{}", "=".repeat(80));
        }
        
        Commands::TestApi => {
            // Create client config
            let mut config = SchwabConfig::default();
            config.oauth = oauth_config;
            
            info!("Creating Schwab client...");
            let client = SchwabClient::new(config)?;
            
            info!("Testing API access with a simple request...");
            
            // Try to get quotes for SPY
            match client.get_quotes(&["SPY"]).await {
                Ok(quotes) => {
                    println!("\n{}\n", "=".repeat(80));
                    println!("API access successful!");
                    println!("Retrieved quotes for SPY");
                    if let Ok(json) = serde_json::to_string_pretty(&quotes) {
                        println!("Response (truncated):");
                        let lines: Vec<_> = json.lines().take(10).collect();
                        for line in lines {
                            println!("{}", line);
                        }
                        println!("...");
                    }
                    println!("{}", "=".repeat(80));
                }
                Err(e) => {
                    error!("API request failed: {}", e);
                    println!("\n{}\n", "=".repeat(80));
                    println!("API access failed!");
                    println!("Error: {}", e);
                    println!("Check your tokens and try refreshing or reauthorizing.");
                    println!("{}", "=".repeat(80));
                }
            }
        }
        
        Commands::SimulateExpiry { access_only, refresh_token } => {
            // Load tokens, modify expiry, and save back
            if let Ok(json) = std::fs::read_to_string(&cli.tokens_file) {
                if let Ok(mut tokens) = serde_json::from_str::<serde_json::Value>(&json) {
                    let now = Utc::now();
                    
                    if access_only || !refresh_token {
                        // Expire access token
                        let expired = (now - Duration::minutes(1)).to_rfc3339();
                        tokens["expires_at"] = serde_json::Value::String(expired);
                        info!("Set access token to expired state");
                    }
                    
                    if refresh_token {
                        // Expire refresh token (simulate 7+ days old)
                        let expired = (now - Duration::days(8)).to_rfc3339();
                        tokens["issued_at"] = serde_json::Value::String(expired.clone());
                        if tokens.get("refresh_token_expires_at").is_some() {
                            tokens["refresh_token_expires_at"] = serde_json::Value::String(expired);
                        }
                        info!("Set refresh token to expired state (>7 days)");
                    }
                    
                    // Save modified tokens
                    let json = serde_json::to_string_pretty(&tokens)?;
                    std::fs::write(&cli.tokens_file, json)?;
                    
                    println!("\n{}\n", "=".repeat(80));
                    println!("Token expiry simulated successfully!");
                    if access_only || !refresh_token {
                        println!("- Access token marked as expired");
                    }
                    if refresh_token {
                        println!("- Refresh token marked as expired (>7 days)");
                    }
                    println!("\nNow try running:");
                    println!("  {} refresh", std::env::args().next().unwrap());
                    println!("  {} test-api", std::env::args().next().unwrap());
                    println!("{}", "=".repeat(80));
                } else {
                    error!("Failed to parse tokens file");
                }
            } else {
                error!("No tokens file found at {:?}", cli.tokens_file);
                println!("Run 'authorize' command first to obtain tokens.");
            }
        }
    }
    
    Ok(())
}