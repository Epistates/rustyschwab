use futures_util::{SinkExt, StreamExt};
use schwab_mcp::stream_cache::{
    RecoveryPolicy, StreamCache, spawn_recovering_cache_feeder_with_policy,
};
use schwab_rs::{
    SchwabClient, SchwabConfig, StreamClient,
    auth::{FileTokenStore, TokenSet, TokenStore, TokenStoreKind},
    config::StreamConfig,
    streaming::StreamerIdentity,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
#[tokio::test]
async fn rest_deadline_keeps_trading_closed_then_quotes_and_accounts_recover_together() {
    let dir = tempfile::tempdir().unwrap();
    let rest = MockServer::start().await;
    let healthy = Arc::new(AtomicBool::new(false));
    let response_health = healthy.clone();
    Mock::given(path("/marketdata/v1/quotes"))
        .respond_with(move |_: &wiremock::Request| {
            let mut quotes: Value =
                serde_json::from_str(include_str!("../../../tools/fixtures/quotes.json")).unwrap();
            quotes["AAPL"]["quote"]["quoteTime"] = json!(chrono::Utc::now().timestamp_millis());
            ResponseTemplate::new(200).set_body_json(quotes).set_delay(
                if response_health.load(Ordering::SeqCst) {
                    Duration::ZERO
                } else {
                    Duration::from_millis(300)
                },
            )
        })
        .mount(&rest)
        .await;
    Mock::given(path("/trader/v1/accounts/accountNumbers"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"accountNumber":"123","hashValue":"A"}])),
        )
        .mount(&rest)
        .await;
    Mock::given(path("/trader/v1/accounts/A/orders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&rest)
        .await;
    Mock::given(path("/trader/v1/accounts/A"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"securitiesAccount":{"positions":[],"currentBalances":{"availableFunds":100}}}),
        ))
        .mount(&rest)
        .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        while let Some(Ok(frame)) = ws.next().await {
            let Ok(text) = frame.into_text() else {
                continue;
            };
            let Ok(request) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            for r in request["requests"].as_array().unwrap() {
                let ack = json!({"response":[{"service":r["service"],"command":r["command"],"requestid":r["requestid"],"SchwabClientCorrelId":"correl","timestamp":1,"content":{"code":0,"msg":"synthetic"}}]});
                if ws
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        ack.to_string().into(),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    });
    let mut config = SchwabConfig::default();
    config.oauth.app_key = "k".repeat(32);
    config.oauth.app_secret = "s".repeat(16);
    config.oauth.tokens_file = dir.path().join("tokens.json");
    config.oauth.token_store_kind = TokenStoreKind::File;
    config.oauth.auto_refresh = false;
    config.oauth.capture_callback = false;
    config.client.base_url = rest.uri();
    config.client.rate_limit.enabled = false;
    config.streaming = None;
    FileTokenStore::new(config.oauth.tokens_file.clone())
        .save(&TokenSet::new(
            "synthetic".into(),
            "synthetic".into(),
            "".into(),
            "Bearer".into(),
            1800,
            "".into(),
            chrono::Utc::now(),
        ))
        .unwrap();
    let client = Arc::new(SchwabClient::new(config).unwrap());
    client
        .halt_submissions("operator investigation", "supervisor")
        .await
        .unwrap();
    let stream = StreamClient::new(
        StreamConfig {
            websocket_url: format!("ws://{addr}"),
            ..Default::default()
        },
        client.auth_manager(),
        StreamerIdentity::new("customer", "correl"),
    )
    .unwrap()
    .with_coordinator(client.coordinator())
    .unwrap();
    stream.connect().await.unwrap();
    stream
        .subscribe_level_one_equities(&["AAPL"])
        .await
        .unwrap();
    stream.subscribe_account_activity().await.unwrap();
    let cache = StreamCache::new();
    let feeder = spawn_recovering_cache_feeder_with_policy(
        stream.get_receiver().unwrap(),
        cache.clone(),
        client.clone(),
        stream.clone(),
        RecoveryPolicy {
            deadline: Duration::from_millis(100),
            interval: Duration::from_millis(20),
        },
    );
    tokio::time::sleep(Duration::from_millis(160)).await;
    assert!(client.submissions_suspended());
    assert!(cache.snapshot("LEVELONE_EQUITIES", None).is_empty());
    healthy.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if !client.submissions_suspended() && cache.events()["reconciliationRequired"] == false
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        cache.snapshot("LEVELONE_EQUITIES", None)["AAPL"]["fresh"],
        true
    );
    assert!(
        client
            .execution_store()
            .account_state("A")
            .await
            .unwrap()
            .unwrap()
            .reconciled
    );
    assert!(
        client.operator_halt().await.unwrap().is_some(),
        "automatic recovery must preserve the operator halt"
    );
    stream.disconnect().await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while !client.submissions_suspended() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(cache.snapshot("LEVELONE_EQUITIES", None).is_empty());
    feeder.abort();
    server.abort();
}
