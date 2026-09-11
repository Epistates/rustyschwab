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
use schwab_mcp::server::trading_enabled;
use schwab_rs::{SchwabClient, SchwabConfig};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use turbomcp::prelude::*;

/// Env var that opts a network transport into binding beyond loopback.
const ALLOW_REMOTE_ENV: &str = "SCHWAB_MCP_ALLOW_REMOTE";

fn remote_bind_allowed() -> bool {
    matches!(
        std::env::var(ALLOW_REMOTE_ENV).as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

/// Remote binds require an explicit deployment opt-in and TLS termination.
/// Every network request is authenticated by `network::router`.
fn guard_network_bind(transport: &str, addr: SocketAddr, allow_remote: bool) -> anyhow::Result<()> {
    if !addr.ip().is_loopback() && !allow_remote {
        anyhow::bail!(
            "Remote {transport} bind {addr} requires SCHWAB_MCP_ALLOW_REMOTE=1 \
             and a TLS-terminating proxy"
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Optional `.env` — ignored if not present
    dotenvy::dotenv().ok();

    // MCP uses stdout for protocol traffic — tracing must go to stderr
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
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
        schwab_mcp::metrics::describe();
        tracing::info!(%addr, "metrics: Prometheus scrape endpoint live at http://<bind>/metrics");
    } else {
        tracing::debug!(
            "metrics: disabled (set SCHWAB_METRICS_BIND=127.0.0.1:9090 \
             to expose Prometheus scrape endpoint)"
        );
    }

    let config = SchwabConfig::from_env().context(
        "failed to load SchwabConfig from environment \
         (set SCHWAB_APP_KEY, SCHWAB_APP_SECRET, etc.)",
    )?;

    let mut client = SchwabClient::new(config).context("failed to construct SchwabClient")?;
    match std::env::var("SCHWAB_RISK_POLICY_FILE") {
        Ok(path) => {
            let accounts = serde_json::from_slice(&std::fs::read(path)?)?;
            let risk = schwab_rs::risk::PortfolioRisk {
                database: client.order_journal().database().clone(),
                accounts,
            };
            client = client.with_risk_provider(Arc::new(risk))?;
        }
        Err(_) if trading_enabled() => anyhow::bail!(
            "MCP trading requires SCHWAB_RISK_POLICY_FILE and a reconciled portfolio baseline"
        ),
        Err(_) => {}
    }
    client
        .init()
        .await
        .context("SchwabClient::init failed — are OAuth tokens present at SCHWAB_TOKENS_FILE?")?;
    tracing::info!("Schwab client ready — tokens loaded");

    let client = Arc::new(client);
    let recovery_accounts: Vec<String> = std::env::var("SCHWAB_RECOVERY_ACCOUNTS")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_owned())
        .collect();
    anyhow::ensure!(
        !trading_enabled() || !recovery_accounts.is_empty(),
        "MCP trading requires SCHWAB_RECOVERY_ACCOUNTS"
    );
    let _recovery = if recovery_accounts.is_empty() {
        None
    } else {
        Some(schwab_rs::recovery::RecoveryWorker::start(
            client.clone(),
            recovery_accounts.clone(),
            0,
            std::time::Duration::from_secs(2),
        ))
    };
    let _health = if let Ok(bind) = std::env::var("SCHWAB_HEALTH_BIND") {
        let addr: SocketAddr = bind.parse()?;
        anyhow::ensure!(
            addr.ip().is_loopback(),
            "Health listener must bind to loopback; expose it through your operations gateway"
        );
        let readiness_client = client.clone();
        let accounts = recovery_accounts.clone();
        let health = axum::Router::new()
            .route("/livez", axum::routing::get(|| async { "ok" }))
            .route(
                "/readyz",
                axum::routing::get(move || {
                    let client = readiness_client.clone();
                    let accounts = accounts.clone();
                    async move {
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(2),
                            client.readiness(&accounts, 5000),
                        )
                        .await
                        {
                            Ok(Ok(report)) => {
                                let ready = if trading_enabled() {
                                    report.trading_ready
                                } else {
                                    report.reads_ready
                                };
                                (
                                    if ready {
                                        axum::http::StatusCode::OK
                                    } else {
                                        axum::http::StatusCode::SERVICE_UNAVAILABLE
                                    },
                                    axum::Json(serde_json::to_value(report).unwrap_or_default()),
                                )
                            }
                            _ => (
                                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                                axum::Json(
                                    serde_json::json!({"ready":false,"storage_healthy":false}),
                                ),
                            ),
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind(addr).await?;
        Some(tokio::spawn(async move {
            let _ = axum::serve(listener, health).await;
        }))
    } else {
        None
    };
    let transport = std::env::var("SCHWAB_MCP_TRANSPORT").unwrap_or_else(|_| "stdio".into());
    let policies: Vec<schwab_mcp::policy::PrincipalPolicy> =
        match std::env::var("SCHWAB_MCP_POLICY_FILE") {
            Ok(path) => serde_json::from_slice(&std::fs::read(path)?)?,
            Err(_)
                if transport == "stdio"
                    && !trading_enabled()
                    && !std::env::var("SCHWAB_MCP_ALLOW_CANCEL")
                        .is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "TRUE")) =>
            {
                let inner = SchwabMcp::new(client.clone());
                inner.run_stdio().await?;
                return Ok(());
            }
            Err(_) => anyhow::bail!("Network access and trading require SCHWAB_MCP_POLICY_FILE"),
        };
    let identity_file = std::env::var_os("SCHWAB_MCP_IDENTITY_FILE");
    schwab_mcp::policy::validate_policies(
        &policies,
        transport != "stdio" && identity_file.is_none(),
    )?;
    let servers: Vec<_> = policies
        .into_iter()
        .map(|policy| schwab_mcp::policy::AuthorizedServer::new(client.clone(), policy))
        .collect();
    if transport == "stdio" {
        let principal = std::env::var("SCHWAB_MCP_PRINCIPAL")
            .context("Set SCHWAB_MCP_PRINCIPAL for policy-controlled stdio")?;
        let server = servers
            .into_iter()
            .find(|s| s.principal.name == principal)
            .context("Unknown stdio principal")?;
        server.run_stdio().await?;
    } else {
        anyhow::ensure!(
            matches!(transport.as_str(), "http" | "websocket" | "ws"),
            "Unsupported MCP transport"
        );
        let bind = std::env::var("SCHWAB_MCP_BIND").unwrap_or_else(|_| "127.0.0.1:8765".into());
        let addr: SocketAddr = bind.parse()?;
        guard_network_bind(&transport, addr, remote_bind_allowed())?;
        let identity = match identity_file {
            Some(path) => Some(
                schwab_mcp::identity::JwtVerifier::new(serde_json::from_slice(&std::fs::read(
                    path,
                )?)?)
                .await?,
            ),
            None => None,
        };
        let app = schwab_mcp::network::router_reloadable(
            servers,
            transport != "http",
            identity,
            std::env::var_os("SCHWAB_MCP_POLICY_FILE").map(std::path::PathBuf::from),
        );
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let serve = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = stopped.await;
        });
        let serving = std::future::IntoFuture::into_future(serve);
        tokio::pin!(serving);
        tokio::select! {
            result = &mut serving => result?,
            _ = shutdown_signal() => {
                client.begin_drain();
                let _ = stop.send(());
                // Long-lived transports must not prevent bounded process exit.
                let grace = std::time::Duration::from_secs(30);
                if let Ok(result) = tokio::time::timeout(grace, &mut serving).await {
                    result?;
                }
            }
        }
    }

    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("Install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn loopback_binds_pass_regardless_of_opt_in() {
        assert!(guard_network_bind("HTTP", addr("127.0.0.1:8765"), false).is_ok());
        assert!(guard_network_bind("HTTP", addr("[::1]:8765"), false).is_ok());
    }

    #[test]
    fn non_loopback_refused_without_opt_in() {
        assert!(guard_network_bind("HTTP", addr("0.0.0.0:8765"), false).is_err());
        assert!(guard_network_bind("WebSocket", addr("192.168.1.10:8765"), false).is_err());
    }

    #[test]
    fn non_loopback_allowed_with_opt_in() {
        assert!(guard_network_bind("HTTP", addr("0.0.0.0:8765"), true).is_ok());
    }
}
