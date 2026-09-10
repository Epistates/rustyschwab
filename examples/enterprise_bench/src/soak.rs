use futures_util::{SinkExt, StreamExt};
use schwab_rs::{
    auth::{AuthManager, FileTokenStore, OAuthConfig, TokenSet, TokenStore, TokenStoreKind},
    config::{ChannelKind, StreamConfig},
    streaming::{StreamClient, StreamerIdentity},
    types::streaming::StreamMessage,
};
use serde_json::json;
use std::time::Duration;
pub async fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let seconds: u64 = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(30);
    let delay_ms: u64 = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(0);
    if !(1..=86400).contains(&seconds) {
        return Err("soak duration must be 1..=86400 seconds".into());
    }
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("tokens.json");
    FileTokenStore::new(path.clone()).save(&TokenSet::new(
        "synthetic".into(),
        "synthetic".into(),
        "".into(),
        "Bearer".into(),
        seconds as i64 + 600,
        "".into(),
        chrono::Utc::now(),
    ))?;
    let auth = AuthManager::new(OAuthConfig {
        app_key: "k".repeat(32),
        app_secret: "s".repeat(16),
        tokens_file: path,
        token_store_kind: TokenStoreKind::File,
        auto_refresh: false,
        token_url: "http://127.0.0.1:1/synthetic-only".into(),
        ..Default::default()
    })?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let mut total = 0u64;
        while let Ok((tcp, _)) = listener.accept().await {
            let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
                continue;
            };
            let Some(Ok(login)) = ws.next().await else {
                continue;
            };
            let Ok(text) = login.into_text() else {
                continue;
            };
            let Ok(login) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let r = &login["requests"][0];
            let ack = json!({"response":[{"service":"ADMIN","command":"LOGIN","requestid":r["requestid"],"SchwabClientCorrelId":"correl","timestamp":1,"content":{"code":0,"msg":"synthetic"}}]});
            if ws
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    ack.to_string().into(),
                ))
                .await
                .is_err()
            {
                continue;
            }
            for _ in 0..2000 {
                total += 1;
                let frame = json!({"data":[{"service":"LEVELONE_EQUITIES","timestamp":chrono::Utc::now().timestamp_millis(),"command":"SUBS","content":[{"key":"AAPL","1":100.01,"2":100.02,"3":total}]}]});
                if ws
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        frame.to_string().into(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            // Deliberate abrupt disconnect exercises replay/recovery repeatedly.
        }
    });
    let mut config = StreamConfig {
        websocket_url: format!("ws://{addr}"),
        channel_kind: ChannelKind::Bounded(64),
        ..Default::default()
    };
    config.reconnect.initial_backoff = Duration::from_millis(5);
    config.reconnect.max_backoff = Duration::from_millis(20);
    let client = StreamClient::new(config, auth, StreamerIdentity::new("customer", "correl"))?;
    client.connect().await?;
    let mut rx = client.get_receiver().ok_or("receiver missing")?;
    let start = std::time::Instant::now();
    let duration = Duration::from_secs(seconds);
    let mut delivered = 0u64;
    let mut last_delivery = start;
    let mut max_delivery_gap = Duration::ZERO;
    let mut loss = 0;
    let mut generation = 0;
    while let Some(remaining) = duration.checked_sub(start.elapsed()) {
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(StreamMessage::Data(_))) => {
                let now = std::time::Instant::now();
                max_delivery_gap = max_delivery_gap.max(now.duration_since(last_delivery));
                last_delivery = now;
                delivered += 1;
            }
            Ok(Some(StreamMessage::State(state))) => {
                loss = loss.max(state.lost_messages);
                generation = generation.max(state.generation);
            }
            Ok(Some(_)) => {}
            _ => break,
        }
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }
    max_delivery_gap = max_delivery_gap.max(last_delivery.elapsed());
    client.disconnect().await;
    server.abort();
    println!(
        "{}",
        json!({"mode":"stream-soak","seconds":start.elapsed().as_secs_f64(),"delivered":delivered,"lost_messages":loss,"generation":generation,"consumer_delay_ms":delay_ms,"queue_capacity":64,"max_delivery_gap_seconds":max_delivery_gap.as_secs_f64()})
    );
    Ok(())
}
