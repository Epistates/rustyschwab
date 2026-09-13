use schwab_mcp::authority::{Authority, IdentityVerifier, ServiceIdentity, router};
use schwab_rs::{
    SchwabClient, SchwabConfig,
    auth::{FileTokenStore, TokenSet, TokenStore, TokenStoreKind},
    coordination::Coordinator,
    execution::{ExecutionStore, SubmissionOutcome},
    remote::RemoteBackend,
    risk::{PortfolioLimits, PortfolioRisk},
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};
struct TestIdentity;
#[async_trait::async_trait]
impl IdentityVerifier for TestIdentity {
    async fn verify(&self, token: &str) -> anyhow::Result<ServiceIdentity> {
        let subject = if token == "b".repeat(32) {
            "backend"
        } else if token == "o".repeat(32) {
            "operator"
        } else if token == "t".repeat(32) {
            "trader"
        } else if token == "m".repeat(32) {
            "market-feed"
        } else if token == "c".repeat(32) {
            "calendar-feed"
        } else {
            anyhow::bail!("invalid synthetic credential")
        };
        Ok(ServiceIdentity {
            subject: subject.into(),
            expires_at: chrono::Utc::now().timestamp() as u64 + 120,
        })
    }
}
struct Setup {
    _dir: tempfile::TempDir,
    broker: MockServer,
    client: Arc<SchwabClient>,
    url: String,
    policy: std::path::PathBuf,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Setup {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn grants() -> Value {
    json!([
        {"subject": "backend", "role": "backend"},
        {"subject": "operator", "role": "operator"},
        {
            "subject": "trader",
            "role": "trader",
            "policy": {
                "name": "desk",
                "identity_subject": "trader",
                "tools": ["place_order", "replace_order", "cancel_order"],
                "accounts": ["A"],
                "max_quantity": "2",
                "max_notional": "100",
                "require_approval": true,
            },
        }
    ])
}
async fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let broker = MockServer::start().await;
    let mut config = SchwabConfig::default();
    config.oauth.app_key = "k".repeat(32);
    config.oauth.app_secret = "s".repeat(16);
    config.oauth.tokens_file = dir.path().join("tokens.json");
    config.oauth.token_store_kind = TokenStoreKind::File;
    config.oauth.auto_refresh = false;
    config.oauth.capture_callback = false;
    config.client.base_url = broker.uri();
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
    let client = SchwabClient::new(config).unwrap();
    let db = client.order_journal().database().clone();
    db.reconcile_snapshot(
        "A",
        0,
        chrono::Utc::now().timestamp_millis(),
        json!({"securitiesAccount":{"positions":[],"currentBalances":{"availableFunds":1000}}}),
        vec![],
    )
    .await
    .unwrap();
    let risk = Arc::new(PortfolioRisk {
        database: db,
        accounts: HashMap::from([(
            "A".into(),
            PortfolioLimits {
                max_open_quantity: 2.into(),
                max_open_notional: 100.into(),
                max_position_quantity: 10.into(),
                max_snapshot_age_ms: 5000,
            },
        )]),
    });
    let client = Arc::new(client.with_risk_provider(risk.clone()).unwrap());
    let policy = dir.path().join("policy.json");
    std::fs::write(&policy, grants().to_string()).unwrap();
    let state = Arc::new(Authority::new(
        client.clone(),
        risk,
        Arc::new(TestIdentity),
        policy.clone(),
        vec!["A".into()],
    ));
    state.validate().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    Setup {
        _dir: dir,
        broker,
        client,
        url,
        policy,
        task,
    }
}
async fn post(setup: &Setup, route: &str, token: char, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}{route}", setup.url))
        .bearer_auth(token.to_string().repeat(32))
        .json(&body)
        .send()
        .await
        .unwrap()
}
fn order(tag: &str) -> Value {
    json!({"tag":tag,"orderStrategyType":"SINGLE","orderType":"LIMIT","session":"NORMAL","duration":"DAY","price":"10","orderLegCollection":[{"quantity":"1","instruction":"BUY","instrument":{"symbol":"AAPL","assetType":"EQUITY"}}]})
}
async fn approve(s: &Setup, order: &Value) {
    let digest = schwab_mcp::policy::intent_digest(
        "place_order",
        &json!({"account_hash":"A","order":order}),
    );
    assert_eq!(post(s,"/v1/operator",'o',json!({"action":"approve","principal":"desk","digest":digest,"expires_at":chrono::Utc::now().timestamp()+60})).await.status(),200);
}
#[tokio::test]
async fn two_remote_clients_share_intents_quotas_and_fencing() {
    let s = setup().await;
    let a = RemoteBackend::new(&format!("{}/v1/backend", s.url), "b".repeat(32).into()).unwrap();
    let b = a.clone();
    let id = a
        .begin_once("shared", "place_order", json!({"tag":"one"}), None)
        .await
        .unwrap();
    // Typed trading-control errors survive the wire; the worker sees the
    // same variant a local journal would return.
    assert!(matches!(
        b.begin_once("shared", "place_order", json!({"tag":"two"}), None)
            .await,
        Err(schwab_rs::Error::ReconciliationRequired { attempt_id }) if attempt_id == id
    ));
    let envelope = post(
        &s,
        "/v1/backend",
        'b',
        json!({"operation":"begin","arguments":{"account":"shared","operation":"place_order","request":{"tag":"two"},"dedup_key":null}}),
    )
    .await;
    assert_eq!(envelope.status(), 409);
    let envelope: Value = envelope.json().await.unwrap();
    assert_eq!(envelope["kind"], "reconciliation_required", "{envelope}");
    assert_eq!(envelope["attempt_id"], id, "{envelope}");
    a.finish(&id, SubmissionOutcome::Rejected).await.unwrap();
    assert!(matches!(
        a.finish(&id, SubmissionOutcome::Rejected).await,
        Err(schwab_rs::Error::Duplicate(_))
    ));
    let second = b
        .begin_once("shared", "place_order", json!({"tag":"two"}), None)
        .await
        .unwrap();
    b.finish(&second, SubmissionOutcome::Rejected)
        .await
        .unwrap();
    // A rejection releases its tag; an acceptance consumes it for every client.
    let retried = a
        .begin_once("shared", "place_order", json!({"tag":"one"}), None)
        .await
        .unwrap();
    a.finish(
        &retried,
        SubmissionOutcome::Accepted {
            order_id: Some("1".into()),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        b.begin_once("shared", "place_order", json!({"tag":"one"}), None)
            .await,
        Err(schwab_rs::Error::Duplicate(_))
    ));
    assert!(a.quota("shared-quota", 1, 1).await.unwrap().is_zero());
    assert!(!b.quota("shared-quota", 1, 1).await.unwrap().is_zero());
    let old = a
        .acquire("shared-owner", "worker-a", Duration::from_secs(30))
        .await
        .unwrap();
    Coordinator::release(&a, &old).await.unwrap();
    let new = b
        .acquire("shared-owner", "worker-b", Duration::from_secs(30))
        .await
        .unwrap();
    assert!(new.epoch > old.epoch);
    assert!(!a.valid(&old).await.unwrap());
    assert!(b.valid(&new).await.unwrap());
    a.set_operator_halt(Some("review".into()), "spoofed-name")
        .await
        .unwrap();
    assert!(b.operator_halt().await.unwrap().is_some());
    // Protocol refusals stay opaque and never masquerade as a broker reply.
    let refused = b.set_operator_halt(None, "backend").await.unwrap_err();
    assert!(
        matches!(
            &refused,
            schwab_rs::Error::Remote { status, .. } if *status == reqwest::StatusCode::FORBIDDEN
        ),
        "Only the verified operator endpoint may clear a remote halt: {refused:?}"
    );
    assert_eq!(refused.http_status(), None);
    assert!(!refused.is_retryable());
    s.client.resume_submissions();
    assert!(b.operator_halt().await.unwrap().is_some());
    let audit = a.audit_page(0, 100).await.unwrap();
    assert!(
        audit
            .iter()
            .any(|e| e.kind == "operator_halt_changed" && e.subject == "backend")
    );
    assert_eq!(
        post(&s, "/v1/backend", 't', json!({"operation":"health"}))
            .await
            .status(),
        403
    );
}
#[tokio::test]
async fn verified_approval_gateway_fencing_replay_and_operator_revocation() {
    let s = setup().await;
    Mock::given(method("POST"))
        .and(path("/trader/v1/accounts/A/orders"))
        .respond_with(ResponseTemplate::new(201).insert_header(
            "Location",
            "https://api.schwabapi.com/trader/v1/accounts/A/orders/7",
        ))
        .expect(1)
        .mount(&s.broker)
        .await;
    let first = order("gateway-one");
    let typed: schwab_rs::types::trading::OrderRequest =
        serde_json::from_value(first.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&typed).unwrap(),
        first,
        "gateway approval normalization changed input"
    );
    assert_eq!(post(&s,"/v1/operator",'t',json!({"action":"approve","principal":"desk","digest":"a".repeat(64),"expires_at":chrono::Utc::now().timestamp()+60})).await.status(),403);
    assert_eq!(
        post(
            &s,
            "/v1/operator",
            'b',
            json!({"action":"halt","reason":"spoof"})
        )
        .await
        .status(),
        403
    );
    approve(&s, &first).await;
    let digest = schwab_mcp::policy::intent_digest(
        "place_order",
        &json!({"account_hash":"A","order":first}),
    );
    let audit = schwab_rs::execution::OrderJournal::new(
        s.client
            .auth_manager()
            .config()
            .tokens_file
            .with_extension("orders.json")
            .with_extension(format!(
                "{}.audit.json",
                schwab_mcp::policy::digest(b"desk")
            )),
    );
    assert!(audit.is_approved("desk", &digest).await.unwrap());
    assert_eq!(
        audit.approval_operator(&digest).await.unwrap(),
        Some("operator".into())
    );
    let mut policy: schwab_mcp::policy::PrincipalPolicy =
        serde_json::from_value(grants()[2]["policy"].clone()).unwrap();
    policy.approved_intents.insert(digest);
    policy
        .authorize("place_order", &json!({"account_hash":"A","order":first}))
        .unwrap();
    let lease: Value = post(&s, "/v1/lease", 't', json!({"action":"acquire"}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(lease["owner"], "trader", "{lease}");
    let request = json!({"request_id":uuid::Uuid::new_v4().to_string(),"account_hash":"A","action":{"kind":"place","order":first},"lease":lease});
    let response = post(&s, "/v1/orders", 't', request.clone()).await;
    assert_eq!(response.status(), 200);
    let result: Value = response.json().await.unwrap();
    assert_eq!(result["status"], "accepted", "{result}");
    assert_eq!(post(&s, "/v1/orders", 't', request).await.status(), 403);
    assert_eq!(
        post(
            &s,
            "/v1/lease",
            't',
            json!({"action":"release","lease":lease})
        )
        .await
        .status(),
        200
    );
    let second = order("gateway-two");
    approve(&s, &second).await;
    let stale = json!({"request_id":uuid::Uuid::new_v4().to_string(),"account_hash":"A","action":{"kind":"place","order":second},"lease":lease});
    let refused = post(&s, "/v1/orders", 't', stale).await;
    assert_eq!(refused.status(), 409);
    let refused: Value = refused.json().await.unwrap();
    assert_eq!(refused["kind"], "ownership_lost", "{refused}");
    let new_lease: Value = post(&s, "/v1/lease", 't', json!({"action":"acquire"}))
        .await
        .json()
        .await
        .unwrap();
    let mut policy = grants();
    policy
        .as_array_mut()
        .unwrap()
        .retain(|g| g["role"] != "operator");
    std::fs::write(&s.policy, policy.to_string()).unwrap();
    let revoked = json!({"request_id":uuid::Uuid::new_v4().to_string(),"account_hash":"A","action":{"kind":"place","order":second},"lease":new_lease});
    assert_eq!(post(&s, "/v1/orders", 't', revoked).await.status(), 403);
    s.broker.verify().await;
}

#[tokio::test]
async fn gateway_reports_broker_rejections_with_a_typed_envelope() {
    let s = setup().await;
    Mock::given(method("POST"))
        .and(path("/trader/v1/accounts/A/orders"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "errors":[{
                "id":"e1",
                "status":"400",
                "title":"Bad Request",
                "detail":"Symbol not tradable"
            }]
        })))
        .expect(1)
        .mount(&s.broker)
        .await;
    let order = order("gateway-rejected");
    approve(&s, &order).await;
    let lease: Value = post(&s, "/v1/lease", 't', json!({"action":"acquire"}))
        .await
        .json()
        .await
        .unwrap();
    let request = json!({"request_id":uuid::Uuid::new_v4().to_string(),"account_hash":"A","action":{"kind":"place","order":order},"lease":lease});
    let response = post(&s, "/v1/orders", 't', request).await;
    assert_eq!(response.status(), 200);
    let result: Value = response.json().await.unwrap();
    assert_eq!(result["status"], "rejected", "{result}");
    assert_eq!(result["error"]["kind"], "broker", "{result}");
    assert_eq!(result["error"]["status"], 400, "{result}");
    assert!(
        result["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.contains("Symbol not tradable")),
        "broker detail must not cross the gateway: {result}"
    );
    s.broker.verify().await;
}

#[tokio::test]
async fn paper_gateway_uses_verified_approvals_and_isolates_market_ingestion() {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    async fn call(app: &axum::Router, route: &str, token: &str, body: Value) -> (u16, Value) {
        let reply = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(route)
                    .header("authorization", format!("Bearer {}", token.repeat(32)))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = reply.status().as_u16();
        let bytes = to_bytes(reply.into_body(), 8 * 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    let dir = tempfile::tempdir().unwrap();
    let config = serde_json::from_str(include_str!("../../../ops/paper/config.json")).unwrap();
    let paper = Arc::new(
        schwab_rs::paper::PaperBroker::open(dir.path().join("state/paper.sqlite3"), config)
            .await
            .unwrap(),
    );
    let policy = dir.path().join("policy.json");
    let mut roles = grants();
    roles[2]["policy"]["accounts"] = json!(["paper-demo"]);
    std::fs::write(&policy, roles.to_string()).unwrap();
    let app = router(Arc::new(Authority::new_paper(
        paper.clone(),
        Arc::new(TestIdentity),
        policy.clone(),
    )));
    let session =
        json!({"sequence":1,"at_ms":0,"kind":{"kind":"session","session_id":"test","open":true}});
    assert_eq!(
        call(&app, "/v1/paper/events", "t", session.clone()).await.0,
        403
    );
    // Operator authority no longer implicitly authorizes synthetic market input.
    assert_eq!(
        call(&app, "/v1/paper/events", "o", session.clone()).await.0,
        403
    );
    roles.as_array_mut().unwrap().extend([
        json!({"subject":"calendar-feed","role":"paper_calendar"}),
        json!({"subject":"market-feed","role":"paper_market_data"}),
    ]);
    std::fs::write(&policy, roles.to_string()).unwrap();
    assert_eq!(
        call(&app, "/v1/paper/events", "m", session.clone()).await.0,
        403
    );
    assert_eq!(call(&app, "/v1/paper/events", "c", session).await.0, 200);
    let fault = json!({"sequence":2,"at_ms":100,"kind":{"kind":"fault","fault":"reject_next"}});
    assert_eq!(
        call(&app, "/v1/paper/events", "m", fault.clone()).await.0,
        403
    );
    assert_eq!(call(&app, "/v1/paper/events", "o", fault).await.0, 403);
    let audit = paper.order_journal().audit_page(0, 100).await.unwrap();
    let event = audit
        .iter()
        .find(|entry| entry.kind == "paper_event")
        .unwrap();
    assert_eq!(event.payload["provenance"]["source"], "authority");
    assert_eq!(event.payload["provenance"]["subject"], "calendar-feed");
    assert_eq!(call(&app,"/v1/paper/events","m",json!({"sequence":2,"at_ms":100,"kind":{"kind":"quote","symbol":"ABC","source_at_ms":100,"bid":"10","ask":"10.01","bid_size":"10","ask_size":"10"}})).await.0,200);
    let (_, lease) = call(&app, "/v1/lease", "t", json!({"action":"acquire"})).await;
    let request:schwab_rs::types::trading::OrderRequest=serde_json::from_value(json!({"tag":"paper-approved","orderStrategyType":"SINGLE","orderType":"LIMIT","session":"NORMAL","duration":"DAY","price":"11","orderLegCollection":[{"quantity":"1","instruction":"BUY","instrument":{"symbol":"ABC","assetType":"EQUITY"}}]})).unwrap();
    let args = json!({"account_hash":"paper-demo","order":request});
    let digest = schwab_mcp::policy::intent_digest("place_order", &args);
    let gateway = json!({"request_id":uuid::Uuid::new_v4().to_string(),"account_hash":"paper-demo","lease":lease,"action":{"kind":"place","order":request}});
    assert_eq!(call(&app, "/v1/orders", "t", gateway.clone()).await.0, 403);
    assert_eq!(call(&app,"/v1/operator","o",json!({"action":"approve","principal":"desk","digest":digest,"expires_at":chrono::Utc::now().timestamp()+60})).await.0,200);
    let (status, accepted) = call(&app, "/v1/orders", "t", gateway.clone()).await;
    assert_eq!(status, 200, "{accepted}");
    assert_eq!(accepted["status"], "accepted");
    assert_eq!(accepted["mode"], "paper");
    assert_ne!(call(&app, "/v1/orders", "t", gateway).await.0, 200);
    assert_eq!(
        call(&app, "/v1/paper/account", "t", json!({"account":"other"}))
            .await
            .0,
        403
    );
    let (_, report) = call(
        &app,
        "/v1/paper/account",
        "t",
        json!({"account":"paper-demo"}),
    )
    .await;
    assert_eq!(report["snapshot"]["ledger"]["cash"], "100000");
    assert_eq!(report["orders"].as_array().unwrap().len(), 1);
    let (_,result)=call(&app,"/v1/paper/events","m",json!({"sequence":3,"at_ms":200,"kind":{"kind":"quote","symbol":"ABC","source_at_ms":200,"bid":"10","ask":"10.01","bid_size":"1","ask_size":"1"}})).await;
    assert_eq!(result["fills"].as_array().unwrap().len(), 1);
    assert_eq!(
        paper.get_order("paper-demo", "1").await.unwrap().status,
        schwab_rs::paper::PaperOrderStatus::Filled
    );
}
