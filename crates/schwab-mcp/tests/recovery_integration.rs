mod common;

use common::mocked_client;
use futures_util::{SinkExt, StreamExt};
use schwab_mcp::stream_cache::{
    RecoveryPolicy, StreamCache, spawn_recovering_cache_feeder_with_policy,
};
use schwab_rs::{StreamClient, config::StreamConfig, streaming::StreamerIdentity};
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
                let mut frames = vec![ack];
                // Every subscription is answered with one numbered data frame,
                // as the broker sends them.
                let now = chrono::Utc::now().timestamp_millis();
                match (r["service"].as_str(), r["command"].as_str()) {
                    (Some("LEVELONE_EQUITIES"), Some("SUBS" | "ADD")) => {
                        frames.push(json!({"data":[{
                            "service":"LEVELONE_EQUITIES","timestamp":now,"command":"SUBS",
                            "content":[{"key":"AAPL","delayed":false,"assetMainType":"EQUITY",
                                "1":150.25,"2":150.5}]
                        }]}))
                    }
                    (Some("ACCT_ACTIVITY"), Some("SUBS" | "ADD")) => {
                        // Field 3 is JSON serialized into a string.
                        let message =
                            json!({"SchwabOrderID":"1","BaseEvent":{"AccountNumber":"123456789"}});
                        frames.push(json!({"data":[{
                            "service":"ACCT_ACTIVITY","timestamp":now,"command":"SUBS",
                            "content":[{"seq":1,"key":"Account Activity","1":"123456789",
                                "2":"OrderCreated","3":message.to_string()}]
                        }]}));
                    }
                    _ => {}
                }
                for frame in frames {
                    if ws
                        .send(tokio_tungstenite::tungstenite::Message::Text(
                            frame.to_string().into(),
                        ))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    });
    let client = mocked_client(&rest.uri(), dir.path());
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
    // The delta is cached but cannot be fresh until the REST baseline lands.
    assert_eq!(
        cache.snapshot("LEVELONE_EQUITIES", None)["AAPL"]["fresh"],
        false
    );
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
    let snapshot = cache.snapshot("LEVELONE_EQUITIES", None);
    assert_eq!(snapshot["AAPL"]["fresh"], true);
    // Snapshots carry field names, not numbers; the stream delta wins over
    // the REST baseline, which fills what the delta lacked.
    let content = &snapshot["AAPL"]["content"];
    assert_eq!(content["bidPrice"], 150.25);
    assert_eq!(content["askPrice"], 150.5);
    assert_eq!(content["lastPrice"], 100.01);
    assert_eq!(content["description"], "Synthetic fixture");
    assert!(content.get("1").is_none(), "{content}");
    let events = cache.events();
    let event = &events["events"][0]["content"];
    assert_eq!(event["messageType"], "OrderCreated");
    assert_eq!(event["accountNumber"], "***6789");
    // `messageData` arrives as JSON-in-a-string; the log stores it parsed.
    let message = &event["messageData"];
    assert!(message.is_object(), "{message}");
    assert_eq!(message["BaseEvent"]["AccountNumber"], "***6789");
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
