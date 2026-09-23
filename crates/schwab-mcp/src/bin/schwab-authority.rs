//! Single transactional authority for cross-host SDK workers and broker gateway.
use clap::Parser;
use schwab_rs::{
    SchwabClient, SchwabConfig,
    broker::TradingBackend,
    paper::{PaperBroker, PaperConfig},
    recovery::RecoveryWorker,
    risk::{PortfolioLimits, PortfolioRisk},
};
use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
#[derive(Parser)]
#[command(about = "Shared Schwab authority; expose through an authenticated HTTPS reverse proxy")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8766")]
    bind: SocketAddr,
    #[arg(long)]
    identity: PathBuf,
    #[arg(long)]
    policy: PathBuf,
    #[arg(long, required_unless_present = "paper", conflicts_with = "paper")]
    risk: Option<PathBuf>,
    /// Isolated paper configuration; never reads Schwab OAuth environment variables.
    #[arg(long, requires = "paper_state")]
    paper: Option<PathBuf>,
    #[arg(long, requires = "paper")]
    paper_state: Option<PathBuf>,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    anyhow::ensure!(
        args.bind.ip().is_loopback(),
        "Authority binds loopback; terminate external HTTPS in a same-host/pod proxy"
    );
    let identity = schwab_mcp::identity::JwtVerifier::new(serde_json::from_slice(&std::fs::read(
        &args.identity,
    )?)?)
    .await?;
    let client: Arc<dyn TradingBackend>;
    let authority;
    let _recovery;
    if let Some(path) = args.paper {
        let paper_config: PaperConfig = serde_json::from_slice(&std::fs::read(path)?)?;
        let paper = Arc::new(
            PaperBroker::open(
                args.paper_state
                    .ok_or_else(|| anyhow::anyhow!("Paper state required"))?,
                paper_config,
            )
            .await?,
        );
        authority = Arc::new(schwab_mcp::authority::Authority::new_paper(
            paper.clone(),
            identity,
            args.policy,
        ));
        client = paper;
        _recovery = None;
    } else {
        let config = SchwabConfig::from_env()?;
        let interval = schwab_mcp::server::parse_recovery_interval(
            std::env::var(schwab_mcp::server::RECOVERY_INTERVAL_ENV)
                .ok()
                .as_deref(),
        )
        .map_err(anyhow::Error::msg)?;
        schwab_mcp::status::parse_baseline_max_age(
            std::env::var(schwab_mcp::status::BASELINE_MAX_AGE_ENV)
                .ok()
                .as_deref(),
        )
        .map_err(anyhow::Error::msg)?;
        let live = SchwabClient::new(config)?;
        let limits: HashMap<String, PortfolioLimits> = serde_json::from_slice(&std::fs::read(
            args.risk
                .ok_or_else(|| anyhow::anyhow!("Risk configuration required"))?,
        )?)?;
        anyhow::ensure!(
            !limits.is_empty(),
            "Required recovery accounts/risk limits must be configured"
        );
        let accounts: Vec<String> = limits.keys().cloned().collect();
        let risk = Arc::new(PortfolioRisk {
            database: live.order_journal().database().clone(),
            accounts: limits,
        });
        let live = Arc::new(live.with_risk_provider(risk.clone())?);
        authority = Arc::new(schwab_mcp::authority::Authority::new(
            live.clone(),
            risk,
            identity,
            args.policy,
            accounts.clone(),
        ));
        authority.validate()?;
        live.init().await?;
        _recovery = Some(RecoveryWorker::start(live.clone(), accounts, 0, interval));
        client = live;
    }
    authority.validate()?;
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serve = axum::serve(listener, schwab_mcp::authority::router(authority))
        .with_graceful_shutdown(async {
            let _ = stopped.await;
        });
    let serving = std::future::IntoFuture::into_future(serve);
    tokio::pin!(serving);
    tokio::select! {
        result = &mut serving => result?,
        _ = shutdown() => {
            client.begin_drain();
            let _ = stop.send(());
            if let Ok(result) = tokio::time::timeout(Duration::from_secs(30), &mut serving).await {
                result?;
            }
        }
    }
    Ok(())
}
async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {_=term.recv()=>{},_=tokio::signal::ctrl_c()=>{}}
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
