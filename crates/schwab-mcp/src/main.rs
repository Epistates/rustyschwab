//! `schwab-mcp` — MCP server exposing the rustyschwab SDK as MCP tools.
//!
//! Reads OAuth credentials + token store path from environment variables
//! (see README), refreshes tokens on the fly via `AuthManager`, and offers
//! every `SchwabClient` method as an MCP tool.
//!
//! Transport is selected at runtime via `SCHWAB_MCP_TRANSPORT`:
//! - unset / `stdio` (default) — for Claude Desktop + local MCP clients
//! - `http` — bind `SCHWAB_MCP_BIND` (default `127.0.0.1:8765`)
//! - `websocket` — bind `SCHWAB_MCP_BIND` (default `127.0.0.1:8765`)

use anyhow::Context;
use metrics_exporter_prometheus::PrometheusBuilder;
use schwab_mcp::SchwabMcp;
use schwab_rs::{SchwabClient, SchwabConfig};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use turbomcp::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Optional `.env` — ignored if not present
    dotenvy::dotenv().ok();

    // MCP uses stdout for protocol traffic — tracing must go to stderr
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    // Install the Prometheus recorder + scrape listener when the user
    // opts in via `SCHWAB_METRICS_BIND=host:port`. Kept opt-in so
    // stdio-transport deployments (Claude Desktop etc.) don't
    // accidentally expose a network port.
    if let Ok(bind) = std::env::var("SCHWAB_METRICS_BIND") {
        let addr: SocketAddr = bind
            .parse()
            .with_context(|| format!("SCHWAB_METRICS_BIND={bind:?} is not a valid host:port"))?;
        PrometheusBuilder::new()
            .with_http_listener(addr)
            .install()
            .context("failed to install Prometheus metrics recorder / listener")?;
        tracing::info!(%addr, "metrics: Prometheus scrape endpoint live at http://<bind>/metrics");
    } else {
        tracing::debug!(
            "metrics: disabled (set SCHWAB_METRICS_BIND=127.0.0.1:9090 to expose Prometheus scrape endpoint)"
        );
    }

    let config = SchwabConfig::from_env()
        .context("failed to load SchwabConfig from environment (set SCHWAB_APP_KEY, SCHWAB_APP_SECRET, etc.)")?;

    let client = SchwabClient::new(config).context("failed to construct SchwabClient")?;
    client
        .init()
        .await
        .context("SchwabClient::init failed — are OAuth tokens present at SCHWAB_TOKENS_FILE?")?;
    tracing::info!("Schwab client ready — tokens loaded");

    let mcp = SchwabMcp::new(Arc::new(client));

    let transport = std::env::var("SCHWAB_MCP_TRANSPORT").unwrap_or_else(|_| "stdio".to_string());
    let bind = std::env::var("SCHWAB_MCP_BIND").unwrap_or_else(|_| "127.0.0.1:8765".to_string());

    match transport.as_str() {
        "http" => {
            tracing::info!(%bind, "starting MCP server on HTTP transport");
            mcp.run_http(&bind).await?;
        }
        "websocket" | "ws" => {
            tracing::info!(%bind, "starting MCP server on WebSocket transport");
            mcp.run_websocket(&bind).await?;
        }
        "stdio" | "" => {
            tracing::info!("starting MCP server on stdio transport");
            mcp.run_stdio().await?;
        }
        other => {
            anyhow::bail!(
                "unsupported SCHWAB_MCP_TRANSPORT={other:?} (expected stdio | http | websocket)"
            );
        }
    }

    Ok(())
}
