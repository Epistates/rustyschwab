use futures_util::{SinkExt, StreamExt};
use schwab_rs::{
    auth::{AuthManager, FileTokenStore, OAuthConfig, TokenSet, TokenStore, TokenStoreKind},
    config::{ChannelKind, StreamConfig},
    streaming::{StreamClient, StreamerIdentity},
    types::streaming::{StreamMessage, StreamService},
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

fn client(url: String, capacity: usize) -> (StreamClient, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tokens.json");
    FileTokenStore::new(path.clone())
        .save(&TokenSet::new(
            "access".into(),
            "refresh".into(),
            "".into(),
            "Bearer".into(),
            1800,
            "".into(),
            chrono::Utc::now(),
        ))
        .unwrap();
    let auth = AuthManager::new(OAuthConfig {
        app_key: "k".repeat(32),
        app_secret: "s".repeat(16),
        tokens_file: path,
        token_store_kind: TokenStoreKind::File,
        auto_refresh: false,
        ..Default::default()
    })
    .unwrap();
    let mut config = StreamConfig {
        websocket_url: url,
        channel_kind: ChannelKind::Bounded(capacity),
        ..Default::default()
    };
    config.reconnect.initial_backoff = Duration::from_millis(10);
    config.reconnect.max_backoff = Duration::from_millis(20);
    config.reconnect.max_retries = Some(1);
    (
        StreamClient::new(config, auth, StreamerIdentity::new("customer", "correl")).unwrap(),
        dir,
    )
}
async fn request(ws: &mut WebSocketStream<TcpStream>) -> Value {
    let text = ws.next().await.unwrap().unwrap().into_text().unwrap();
    serde_json::from_str(&text).unwrap()
}
fn ack(request: &Value, code: i32) -> Value {
    let r = &request["requests"][0];
    json!({"service":r["service"],"command":r["command"],"requestid":r["requestid"],"SchwabClientCorrelId":"correl","timestamp":1,"content":{"code":code,"msg":"synthetic"}})
}
async fn login(listener: &TcpListener) -> WebSocketStream<TcpStream> {
    let (tcp, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
    let req = request(&mut ws).await;
    ws.send(Message::Text(
        json!({"response":[ack(&req,0)]}).to_string().into(),
    ))
    .await
    .unwrap();
    ws
}

#[tokio::test]
async fn subscription_ack_is_processed_when_it_is_second_in_a_frame() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, _dir) = client(format!("ws://{}", listener.local_addr().unwrap()), 16);
    let server = tokio::spawn(async move {
        let mut ws = login(&listener).await;
        let req = request(&mut ws).await;
        let mut unrelated = ack(&req, 0);
        unrelated["requestid"] = "unrelated".into();
        ws.send(Message::Text(
            json!({"response":[unrelated,ack(&req,0)]})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        let _ = ws.next().await;
    });
    client.connect().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        client.subscribe(StreamService::LeveloneEquities, vec!["AAPL".into()]),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        client.confirmed_subscriptions()["LEVELONE_EQUITIES"],
        vec!["AAPL"]
    );
    client.disconnect().await;
    server.await.unwrap();
}

#[tokio::test]
async fn rejected_subscription_is_not_committed_locally() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, _dir) = client(format!("ws://{}", listener.local_addr().unwrap()), 16);
    let server = tokio::spawn(async move {
        let mut ws = login(&listener).await;
        let req = request(&mut ws).await;
        ws.send(Message::Text(
            json!({"response":[ack(&req,21)]}).to_string().into(),
        ))
        .await
        .unwrap();
        let _ = ws.next().await;
    });
    client.connect().await.unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            client.subscribe(StreamService::LeveloneEquities, vec!["AAPL".into()])
        )
        .await
        .unwrap()
        .is_err()
    );
    assert!(client.confirmed_subscriptions().is_empty());
    client.disconnect().await;
    server.await.unwrap();
}

#[tokio::test]
async fn queue_overflow_is_visible_to_a_slow_receiver() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, _dir) = client(format!("ws://{}", listener.local_addr().unwrap()), 1);
    let (go, wait) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut ws = login(&listener).await;
        wait.await.unwrap();
        for i in 0..8 {
            let frame = json!({"data":[{"service":"LEVELONE_EQUITIES","timestamp":i,"command":"SUBS","content":[{"key":"AAPL","1":i}]}]});
            if ws
                .send(Message::Text(frame.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    client.connect().await.unwrap();
    go.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while client.status().lost_messages == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut rx = client.get_receiver().unwrap();
    match rx.recv().await.unwrap() {
        StreamMessage::State(state) => {
            assert!(state.lost_messages > 0);
            assert!(!state.ready);
        }
        other => panic!("Expected explicit loss notification, got {other:?}"),
    }
    client.disconnect().await;
    server.await.unwrap();
}

#[tokio::test]
async fn silent_login_times_out_and_stops_connection_task() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, _dir) = client(format!("ws://{}", listener.local_addr().unwrap()), 16);
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let _ = request(&mut ws).await;
        tokio::time::sleep(Duration::from_secs(10)).await;
    });
    assert!(
        tokio::time::timeout(Duration::from_secs(7), client.connect())
            .await
            .unwrap()
            .is_err()
    );
    assert!(!client.is_running());
    assert!(!client.status().ready);
    server.abort();
}

#[tokio::test]
async fn reconnect_is_not_ready_until_subscription_replay_is_acknowledged() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, _dir) = client(format!("ws://{}", listener.local_addr().unwrap()), 16);
    let (replaying, replay) = tokio::sync::oneshot::channel();
    let (release, wait) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut ws = login(&listener).await;
        let req = request(&mut ws).await;
        ws.send(Message::Text(
            json!({"response":[ack(&req,0)]}).to_string().into(),
        ))
        .await
        .unwrap();
        // Abrupt close must recover; desired state only includes acknowledged subscriptions.
        drop(ws);
        let mut ws = login(&listener).await;
        let req = request(&mut ws).await;
        assert_eq!(req["requests"][0]["parameters"]["keys"], "AAPL");
        replaying.send(()).unwrap();
        wait.await.unwrap();
        ws.send(Message::Text(
            json!({"response":[ack(&req,0)]}).to_string().into(),
        ))
        .await
        .unwrap();
        let _ = ws.next().await;
    });
    client.connect().await.unwrap();
    client
        .subscribe(StreamService::LeveloneEquities, vec!["AAPL".into()])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), replay)
        .await
        .unwrap()
        .unwrap();
    assert!(!client.status().ready);
    assert!(client.status().generation >= 2);
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !client.status().ready {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    client.disconnect().await;
    server.await.unwrap();
}

/// `view` re-fields a live subscription with Schwab's VIEW command.
#[tokio::test]
async fn view_changes_fields_of_live_subscriptions() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, _dir) = client(format!("ws://{}", listener.local_addr().unwrap()), 16);
    let server = tokio::spawn(async move {
        let mut ws = login(&listener).await;
        let subscribe = request(&mut ws).await;
        ws.send(Message::Text(
            json!({"response":[ack(&subscribe,0)]}).to_string().into(),
        ))
        .await
        .unwrap();
        let view = request(&mut ws).await;
        ws.send(Message::Text(
            json!({"response":[ack(&view,29)]}).to_string().into(),
        ))
        .await
        .unwrap();
        let _ = ws.next().await;
        view
    });
    client.connect().await.unwrap();
    client
        .subscribe(StreamService::LeveloneEquities, vec!["AAPL".into()])
        .await
        .unwrap();
    assert!(
        client
            .view(StreamService::LeveloneEquities, "0,1,x".into())
            .await
            .is_err()
    );
    client
        .view(StreamService::LeveloneEquities, "0,1,2".into())
        .await
        .unwrap();
    client.disconnect().await;
    let view = server.await.unwrap();
    let sent = &view["requests"][0];
    assert_eq!(sent["command"], "VIEW");
    assert_eq!(sent["service"], "LEVELONE_EQUITIES");
    assert_eq!(sent["parameters"]["fields"], "0,1,2");
    assert!(sent["parameters"].get("keys").is_none());
}
