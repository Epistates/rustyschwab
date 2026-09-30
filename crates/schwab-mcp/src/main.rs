//! `schwab-mcp` — MCP server exposing the rustyschwab SDK as MCP tools.
//!
//! Live mode reads OAuth credentials + token store path from environment
//! variables (see README), refreshes tokens on the fly via `AuthManager`,
//! and offers every `SchwabClient` method as an MCP tool. Paper mode
//! (`SCHWAB_MCP_PAPER_CONFIG` + `SCHWAB_MCP_PAPER_STATE`) serves the same
//! tools over the isolated paper venue and needs no Schwab credentials.
//!
//! Transport is selected at runtime via `SCHWAB_MCP_TRANSPORT`:
//! - unset / `stdio` (default) — for Claude Desktop + local MCP clients
//! - `http` — bind `SCHWAB_MCP_BIND` (default `127.0.0.1:8765`)
//! - `websocket` — bind `SCHWAB_MCP_BIND` (default `127.0.0.1:8765`)

use anyhow::Context;
use metrics_exporter_prometheus::PrometheusBuilder;
use schwab_mcp::SchwabMcp;
use schwab_mcp::backend::{Backend, Paper};
use schwab_mcp::server::{RECOVERY_ACCOUNTS_ENV, Switches, recovery_accounts};
use schwab_rs::paper::{PaperBroker, PaperConfig};
use schwab_rs::{SchwabClient, SchwabConfig};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use turbomcp::prelude::*;

/// Env var that opts a network transport into binding beyond loopback.
const ALLOW_REMOTE_ENV: &str = "SCHWAB_MCP_ALLOW_REMOTE";

/// Env vars that select paper mode: the venue's configuration and its
/// durable state file, the same pair `schwab-paper` and `schwab-authority
/// --paper` take.
const PAPER_CONFIG_ENV: &str = "SCHWAB_MCP_PAPER_CONFIG";
const PAPER_STATE_ENV: &str = "SCHWAB_MCP_PAPER_STATE";

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

    let switches = Switches::from_env();
    let (backend, _recovery) = match std::env::var_os(PAPER_CONFIG_ENV) {
        Some(config) => (paper_backend(PathBuf::from(config)).await?, None),
        None => live_backend(switches).await?,
    };
    tracing::info!(mode = backend.mode(), "MCP execution backend ready");
    let _health = if let Ok(bind) = std::env::var("SCHWAB_HEALTH_BIND") {
        let addr: SocketAddr = bind.parse()?;
        anyhow::ensure!(
            addr.ip().is_loopback(),
            "Health listener must bind to loopback; expose it through your operations gateway"
        );
        let readiness_backend = backend.clone();
        let health = axum::Router::new()
            .route("/livez", axum::routing::get(|| async { "ok" }))
            .route(
                "/readyz",
                axum::routing::get(move || {
                    let backend = readiness_backend.clone();
                    async move {
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(2),
                            backend.readiness(schwab_mcp::status::baseline_max_age_ms()),
                        )
                        .await
                        {
                            Ok(Ok(report)) => {
                                let ready = if switches.trading {
                                    report.trading_ready
                                } else {
                                    report.reads_ready
                                };
                                let mut body = serde_json::to_value(report).unwrap_or_default();
                                body["mode"] = backend.mode().into();
                                (
                                    if ready {
                                        axum::http::StatusCode::OK
                                    } else {
                                        axum::http::StatusCode::SERVICE_UNAVAILABLE
                                    },
                                    axum::Json(body),
                                )
                            }
                            _ => (
                                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                                axum::Json(serde_json::json!({
                                    "readsReady": false,
                                    "tradingReady": false,
                                    "storageHealthy": false,
                                    "mode": backend.mode(),
                                })),
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
            Err(_) if transport == "stdio" && switches == Switches::default() => {
                let inner = SchwabMcp::new(backend, switches);
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
        .map(|policy| schwab_mcp::policy::AuthorizedServer::new(backend.clone(), switches, policy))
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
                backend.trading().begin_drain();
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

/// The live Schwab backend: credentials, risk policy when trading is on,
/// and the recovery worker that keeps the trading accounts' baselines.
async fn live_backend(
    switches: Switches,
) -> anyhow::Result<(Backend, Option<schwab_rs::recovery::RecoveryWorker>)> {
    let config = SchwabConfig::from_env().context(
        "failed to load SchwabConfig from environment \
         (set SCHWAB_APP_KEY, SCHWAB_APP_SECRET, etc.; or SCHWAB_MCP_PAPER_CONFIG for paper mode)",
    )?;
    let requests_per_second = config
        .client
        .rate_limit
        .enabled
        .then_some(config.client.rate_limit.requests_per_second);
    let (recovery_interval, baseline_max_age) =
        schwab_mcp::status::freshness_from_env().map_err(anyhow::Error::msg)?;

    let mut client = SchwabClient::new(config).context("failed to construct SchwabClient")?;
    match std::env::var("SCHWAB_RISK_POLICY_FILE") {
        Ok(path) => {
            let accounts = serde_json::from_slice(&std::fs::read(path)?)?;
            schwab_mcp::status::check_risk_freshness(
                recovery_interval,
                baseline_max_age,
                &accounts,
            )
            .map_err(anyhow::Error::msg)?;
            let risk = schwab_rs::risk::PortfolioRisk {
                database: client.order_journal().database().clone(),
                accounts,
            };
            client = client.with_risk_provider(Arc::new(risk))?;
        }
        Err(_) if switches.trading => anyhow::bail!(
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
    let recovery_accounts = recovery_accounts();
    anyhow::ensure!(
        !switches.trading || !recovery_accounts.is_empty(),
        "MCP trading requires {RECOVERY_ACCOUNTS_ENV}"
    );
    let recovery = if recovery_accounts.is_empty() {
        None
    } else {
        if let Some(rps) = requests_per_second {
            let share =
                schwab_rs::recovery::budget_share(recovery_accounts.len(), recovery_interval, rps);
            if share > 0.5 {
                tracing::warn!(
                    accounts = recovery_accounts.len(),
                    interval_ms = recovery_interval.as_millis() as u64,
                    share = format!("{:.0}%", share * 100.0),
                    "Reconciliation alone uses over half the request budget; raise \
                     SCHWAB_RECOVERY_INTERVAL_MS (and the risk policy's \
                     max_snapshot_age_ms with SCHWAB_BASELINE_MAX_AGE_MS) or reconcile fewer accounts"
                );
            }
        }
        Some(schwab_rs::recovery::RecoveryWorker::start(
            client.clone(),
            recovery_accounts,
            0,
            recovery_interval,
        ))
    };
    Ok((Backend::Live(client), recovery))
}

/// The paper venue from its configuration and state files. No Schwab
/// credential is read unless `SCHWAB_APP_KEY` is set, in which case a
/// client is built for market data only; the venue never routes an order
/// to it.
async fn paper_backend(config: PathBuf) -> anyhow::Result<Backend> {
    let state = std::env::var_os(PAPER_STATE_ENV)
        .map(PathBuf::from)
        .with_context(|| format!("{PAPER_CONFIG_ENV} requires {PAPER_STATE_ENV}"))?;
    let config: PaperConfig = serde_json::from_slice(
        &std::fs::read(&config).with_context(|| format!("failed to read {}", config.display()))?,
    )
    .context("invalid paper configuration")?;
    let broker = Arc::new(
        PaperBroker::open(state, config)
            .await
            .context("failed to open the paper broker")?,
    );
    let market_data = if std::env::var_os("SCHWAB_APP_KEY").is_some() {
        let config =
            SchwabConfig::from_env().context("failed to load SchwabConfig for market data")?;
        let client = SchwabClient::new(config).context("failed to construct SchwabClient")?;
        client.init().await.context(
            "SchwabClient::init failed — are OAuth tokens present at SCHWAB_TOKENS_FILE?",
        )?;
        tracing::info!("Schwab market data ready for the paper venue — tokens loaded");
        Some(Arc::new(client))
    } else {
        tracing::info!("Paper venue without Schwab market data: quotes are the venue's own");
        None
    };
    Ok(Backend::Paper(Paper {
        broker,
        market_data,
    }))
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
