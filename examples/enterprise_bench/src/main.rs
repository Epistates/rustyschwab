//! Loopback-only benchmark; never contacts a broker or submits orders.
mod soak;
use rustyschwab::{
    SchwabClient, SchwabConfig,
    auth::{FileTokenStore, TokenSet, TokenStore, TokenStoreKind},
};
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).is_some_and(|s| s == "stream") {
        return soak::run(&args).await;
    }
    let url = args.get(1).ok_or("loopback URL required")?;
    if !url.starts_with("http://127.0.0.1:") {
        return Err("Only explicit loopback HTTP URLs are supported".into());
    }
    let count: usize = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(1000);
    if count == 0 {
        return Err("count must be positive".into());
    }
    let dir = tempfile::tempdir()?;
    let mut config = SchwabConfig::default();
    config.oauth.app_key = "k".repeat(32);
    config.oauth.app_secret = "s".repeat(16);
    config.oauth.tokens_file = dir.path().join("tokens.json");
    config.oauth.token_store_kind = TokenStoreKind::File;
    config.oauth.auto_refresh = false;
    config.client.base_url = url.clone();
    config.client.rate_limit.enabled = false;
    config.streaming = None;
    FileTokenStore::new(config.oauth.tokens_file.clone()).save(&TokenSet::new(
        "synthetic".into(),
        "synthetic".into(),
        "".into(),
        "Bearer".into(),
        1800,
        "".into(),
        chrono::Utc::now(),
    ))?;
    let client = SchwabClient::new(config)?;
    for _ in 0..20 {
        std::hint::black_box(client.get_quotes(&["AAPL"]).await?);
    }
    let mut samples = Vec::with_capacity(count);
    let start = std::time::Instant::now();
    for _ in 0..count {
        let at = std::time::Instant::now();
        let quotes = client.get_quotes(&["AAPL"]).await?;
        assert_eq!(quotes.quotes["AAPL"].symbol.as_deref(), Some("AAPL"));
        std::hint::black_box(quotes);
        samples.push(at.elapsed().as_secs_f64() * 1000.0);
    }
    let elapsed = start.elapsed().as_secs_f64();
    samples.sort_by(f64::total_cmp);
    let percentile = |p: f64| samples[((samples.len() - 1) as f64 * p).ceil() as usize];
    println!(
        "{}",
        serde_json::json!({"implementation":"rustyschwab","samples":count,"elapsed_seconds":elapsed,"requests_per_second":count as f64/elapsed,"p50_ms":percentile(0.50),"p95_ms":percentile(0.95),"p99_ms":percentile(0.99),"max_ms":samples.last()})
    );
    Ok(())
}
