//! Regression tests for the enterprise audit: assert safe observable behavior.
mod common;

use common::mock_client as client;
use schwab_rs::auth::{FileTokenStore, TokenStore, TokenStoreKind};
use schwab_rs::circuit_breaker::{CircuitBreaker, CircuitState};
use schwab_rs::config::CircuitBreakerConfig;
use schwab_rs::types::{
    Decimal,
    accounts::Account,
    trading::{Order, OrderDuration, OrderInstruction, OrderRequest, OrderRequestLeg},
};
use schwab_rs::{Error, SchwabClient, SchwabConfig};
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn ambiguous_order_submission_is_never_replayed_and_blocks_until_reconciled() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/trader/v1/accounts/hash/orders"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    let (client, _dir, _) = client(&server, false);
    let order = limit_order();
    let error = client.place_order("hash", &order).await.unwrap_err();
    assert!(matches!(error, Error::SubmissionUnknown { .. }));
    assert!(matches!(
        client.place_order("hash", &order).await,
        Err(Error::ReconciliationRequired { .. })
    ));
    let records = client.order_journal().records().await.unwrap();
    assert_eq!(records.len(), 1);
    assert!(records[0].outcome.is_none());
    server.verify().await;
}

#[tokio::test]
async fn rejected_unexpired_token_is_refreshed_once() {
    use wiremock::matchers::header;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/accountNumbers"))
        .and(header("authorization", "Bearer old_access"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(serde_json::json!({"errors":[{"title":"Unauthorized"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(header("authorization", "Bearer new_access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/oauth/token")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"access_token":"new_access","token_type":"Bearer","expires_in":1800}))).expect(1).mount(&server).await;
    let (client, _dir, _) = client(&server, false);
    assert!(client.get_account_numbers().await.unwrap().is_empty());
    server.verify().await;
}

#[tokio::test]
async fn refresh_preserves_original_refresh_token_expiry() {
    let server = MockServer::start().await;
    Mock::given(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token":"new_access", "token_type":"Bearer", "expires_in":1800
        })))
        .expect(1)
        .mount(&server)
        .await;
    let (client, dir, old) = client(&server, true);
    client.auth_manager().ensure_valid_tokens().await.unwrap();
    let new = FileTokenStore::new(dir.path().join("tokens.json"))
        .load()
        .unwrap();
    assert_eq!(old.refresh_token().unwrap(), new.refresh_token().unwrap());
    assert_eq!(
        new.refresh_token_expires_at.unwrap().timestamp(),
        old.refresh_token_expires_at.unwrap().timestamp()
    );
}

#[tokio::test]
async fn default_breaker_recovers_after_sequential_successful_probes() {
    let breaker = CircuitBreaker::new(CircuitBreakerConfig {
        open_duration: Duration::ZERO,
        ..Default::default()
    });
    for _ in 0..5 {
        let _ = breaker
            .call(|| async { Err::<(), _>(Error::ConnectionClosed) })
            .await;
    }
    assert_eq!(breaker.state(), CircuitState::Open);
    assert!(breaker.call(|| async { Ok(()) }).await.is_ok());
    assert!(breaker.call(|| async { Ok(()) }).await.is_ok());
    for _ in 0..10 {
        assert!(breaker.call(|| async { Ok(()) }).await.is_ok());
    }
    assert_eq!(breaker.state(), CircuitState::Closed);
}

#[test]
fn numeric_order_ids_and_good_till_cancel_are_supported() {
    let mut value = serde_json::to_value(Order::default()).unwrap();
    value["orderId"] = 123456789.into();
    assert_eq!(
        serde_json::from_value::<Order>(value.clone())
            .unwrap()
            .order_id
            .as_deref(),
        Some("123456789")
    );
    value.as_object_mut().unwrap().remove("orderId");
    value["duration"] = "GOOD_TILL_CANCEL".into();
    assert!(serde_json::from_value::<Order>(value).is_ok());
    assert_eq!(
        serde_json::to_value(OrderDuration::Gtc).unwrap(),
        "GOOD_TILL_CANCEL"
    );
}

#[test]
fn account_type_is_retained() {
    let account: Account = serde_json::from_value(
        serde_json::json!({"securitiesAccount":{"accountNumber":"123", "type":"MARGIN"}}),
    )
    .unwrap();
    assert_eq!(
        account.securities_account.account_type.as_deref(),
        Some("MARGIN")
    );
}

#[tokio::test]
async fn search_and_movers_use_broker_wire_values() {
    use schwab_rs::types::market_data::{MoverIndex, MoverSort, Projection};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&server)
        .await;
    let (client, _dir, _) = client(&server, false);
    let _ = client
        .search_instruments("AAPL", Projection::SymbolSearch)
        .await;
    let _ = client
        .get_movers(MoverIndex::Dji, Some(MoverSort::PercentChangeUp), None)
        .await;
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests[0]
            .url
            .query()
            .unwrap()
            .contains("projection=symbol-search")
    );
    assert!(
        requests[1]
            .url
            .query()
            .unwrap()
            .contains("sort=PERCENT_CHANGE_UP")
    );
}

#[test]
fn option_and_fundamental_reference_shapes_preserve_risk_data() {
    use schwab_rs::types::market_data::{ExpirationChainResponse, OptionContract, QuotesResponse};
    let option = serde_json::json!({"putCall":"CALL","symbol":"AAPL  241018C00020000","strikePrice":20.0,"expirationDate":"2024-10-18T20:00:00.000+00:00","multiplier":100.0,"delta":1.0,"inTheMoney":true,"futureField":17});
    let contract: OptionContract = serde_json::from_value(option).unwrap();
    assert_eq!(contract.greeks.delta, Some(1.0));
    assert_eq!(contract.multiplier, Some(100.0));
    assert_eq!(contract.is_in_the_money, Some(true));
    assert_eq!(contract.extra["futureField"], 17);
    let expiration: ExpirationChainResponse =
        serde_json::from_value(serde_json::json!({"expirationList":[]})).unwrap();
    assert!(expiration.expirations.is_empty());
    let quotes: QuotesResponse = serde_json::from_value(serde_json::json!({"AAPL":{"symbol":"AAPL","fundamental":{"divPayDate":"2024-08-15T04:00:00Z"}}})).unwrap();
    assert_eq!(
        quotes.quotes["AAPL"]
            .fundamental
            .as_ref()
            .unwrap()
            .div_pay_date
            .as_deref(),
        Some("2024-08-15T04:00:00Z")
    );
}

#[test]
fn structured_errors_preserve_http_status() {
    use schwab_rs::error::parse_api_error;
    let body = r#"{"errors":[{"id":"upstream","title":"failed","detail":"try later"}]}"#;
    assert!(parse_api_error(reqwest::StatusCode::SERVICE_UNAVAILABLE, body).is_retryable());
    assert!(parse_api_error(reqwest::StatusCode::UNAUTHORIZED, body).is_auth_error());
}

fn limit_order() -> OrderRequest {
    OrderRequest::limit(
        OrderRequestLeg::equity(OrderInstruction::Buy, "AAPL", 1),
        Decimal::from(10),
    )
}

/// A caller that stops waiting abandons only the answer: the submission
/// still runs to its outcome, so the account is not left blocked behind an
/// intent nobody will finish.
#[tokio::test]
async fn an_abandoned_submission_still_records_its_outcome() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/trader/v1/accounts/hash/orders"))
        .respond_with(
            ResponseTemplate::new(201)
                .insert_header(
                    "Location",
                    "https://api.schwabapi.com/trader/v1/accounts/hash/orders/4242",
                )
                .set_delay(Duration::from_millis(300)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let (client, _dir, _) = client(&server, false);
    let submission = tokio::spawn({
        let client = client.clone();
        async move { client.place_order("hash", &limit_order()).await }
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while server.received_requests().await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    submission.abort();
    let journal = client.order_journal();
    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(outcome) = journal.records().await.unwrap()[0].outcome.clone() {
                return outcome;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        outcome,
        schwab_rs::execution::SubmissionOutcome::Accepted { order_id: Some(id) } if id == "4242"
    ));
    assert!(journal.records().await.unwrap()[0].armed);
    server.verify().await;
}

#[tokio::test]
async fn cancelled_submission_remains_pending_on_restart() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/trader/v1/accounts/hash/orders"))
        .respond_with(ResponseTemplate::new(201).set_delay(Duration::from_secs(10)))
        .expect(1)
        .mount(&server)
        .await;
    let (client, _dir, _) = client(&server, false);
    let submission = tokio::spawn({
        let client = client.clone();
        async move { client.place_order("hash", &limit_order()).await }
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while server.received_requests().await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    submission.abort();
    assert!(submission.await.unwrap_err().is_cancelled());
    let restarted =
        schwab_rs::execution::OrderJournal::new(client.order_journal().path().to_owned());
    let records = restarted.records().await.unwrap();
    assert!(records[0].outcome.is_none());
    assert!(
        restarted
            .begin("hash", "place_order", serde_json::json!({}))
            .await
            .is_err()
    );
    restarted
        .finish(
            &records[0].attempt_id,
            schwab_rs::execution::SubmissionOutcome::Reconciled {
                evidence: "Synthetic broker lookup confirmed rejection".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        restarted
            .begin("hash", "place_order", serde_json::json!({}))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn cancelled_half_open_probe_releases_slot() {
    let breaker = CircuitBreaker::new(CircuitBreakerConfig {
        failure_threshold: 1,
        open_duration: Duration::ZERO,
        ..Default::default()
    });
    let _ = breaker
        .call(|| async { Err::<(), _>(Error::ConnectionClosed) })
        .await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(1),
            breaker.call(std::future::pending::<Result<(), Error>>)
        )
        .await
        .is_err()
    );
    for _ in 0..3 {
        breaker.call(|| async { Ok(()) }).await.unwrap();
    }
    assert_eq!(breaker.state(), CircuitState::Closed);
}

#[tokio::test]
async fn request_deadline_includes_retry_backoff() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "60"))
        .expect(1)
        .mount(&server)
        .await;
    let (_, dir, _) = client(&server, false);
    let mut config = SchwabConfig::default();
    config.oauth.app_key = "k".repeat(32);
    config.oauth.app_secret = "s".repeat(16);
    config.oauth.token_store_kind = TokenStoreKind::File;
    config.oauth.tokens_file = dir.path().join("tokens.json");
    config.oauth.auto_refresh = false;
    config.client.base_url = server.uri();
    config.client.timeout = Duration::from_millis(80);
    config.streaming = None;
    let client = SchwabClient::new(config).unwrap();
    let error = tokio::time::timeout(Duration::from_secs(1), client.get_account_numbers())
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, Error::Timeout { .. }));
}

#[tokio::test]
async fn concurrent_managers_share_refresh_and_persist_rotation() {
    use schwab_rs::auth::{AuthManager, OAuthConfig};
    let server = MockServer::start().await;
    Mock::given(path("/oauth/token")).respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(30)).set_body_json(serde_json::json!({"access_token":"new_access","refresh_token":"rotated_refresh","token_type":"Bearer","expires_in":1800}))).expect(1).mount(&server).await;
    let (_, dir, old) = client(&server, true);
    let config = OAuthConfig {
        app_key: "k".repeat(32),
        app_secret: "s".repeat(16),
        token_store_kind: TokenStoreKind::File,
        tokens_file: dir.path().join("tokens.json"),
        token_url: format!("{}/oauth/token", server.uri()),
        auto_refresh: false,
        ..Default::default()
    };
    let a = AuthManager::new(config.clone()).unwrap();
    let b = AuthManager::new(config).unwrap();
    let (first, second) = tokio::join!(a.ensure_valid_tokens(), b.ensure_valid_tokens());
    first.unwrap();
    second.unwrap();
    let saved = FileTokenStore::new(dir.path().join("tokens.json"))
        .load()
        .unwrap();
    assert_eq!(saved.refresh_token().unwrap(), "rotated_refresh");
    assert_eq!(
        saved.refresh_token_expires_at.unwrap().timestamp(),
        old.refresh_token_expires_at.unwrap().timestamp()
    );
    assert_eq!(a.get_access_token().await.unwrap(), "new_access");
    assert_eq!(b.get_access_token().await.unwrap(), "new_access");
}

#[tokio::test]
async fn location_and_cusip_wrappers_decode_at_http_boundary() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(201)
                .insert_header("Location", "/trader/v1/accounts/hash/orders/123456"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"instruments":[{"symbol":"AAPL","cusip":"037833100","assetType":"EQUITY"}]}))).expect(1).mount(&server).await;
    let (client, _dir, _) = client(&server, false);
    assert_eq!(
        client
            .place_order("hash", &limit_order())
            .await
            .unwrap()
            .order_id
            .as_deref(),
        Some("123456")
    );
    assert_eq!(
        client.get_instrument("037833100").await.unwrap().symbol,
        "AAPL"
    );
}

#[tokio::test]
async fn approval_consumption_is_atomic_and_survives_reopening() {
    use schwab_rs::execution::{OrderJournal, SubmissionOutcome};
    let dir = tempfile::tempdir().unwrap();
    let a = OrderJournal::new(dir.path().join("audit.json"));
    let b = OrderJournal::new(a.path().to_owned());
    let (first, second) = tokio::join!(
        a.begin_once(
            "A",
            "place_order",
            serde_json::json!({}),
            Some("approved".into())
        ),
        b.begin_once(
            "B",
            "place_order",
            serde_json::json!({}),
            Some("approved".into())
        )
    );
    assert_ne!(first.is_ok(), second.is_ok());
    let id = first.or(second).unwrap();
    a.finish(&id, SubmissionOutcome::Rejected).await.unwrap();
    assert!(
        b.begin_once(
            "B",
            "place_order",
            serde_json::json!({}),
            Some("approved".into())
        )
        .await
        .is_err()
    );
}

// Re-enter the test executable to exercise OS locks across process boundaries.
#[tokio::test]
async fn journal_process_probe() {
    use schwab_rs::execution::OrderJournal;
    let local = tempfile::tempdir().unwrap();
    let path = std::env::var_os("SCHWAB_TEST_JOURNAL_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| local.path().join("probe.json"));
    let journal = OrderJournal::new(path);
    let result = journal
        .begin_once(
            "A",
            "place_order",
            serde_json::json!({}),
            Some("one-use".into()),
        )
        .await;
    assert!(result.is_ok() || matches!(result, Err(Error::Duplicate(_))));
}

#[test]
fn journal_initialization_is_serialized_across_processes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("process-journal.json");
    let spawn = || {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "journal_process_probe"])
            .env("SCHWAB_TEST_JOURNAL_PATH", &path)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    };
    let mut a = spawn();
    let mut b = spawn();
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());
    let records = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(schwab_rs::execution::OrderJournal::new(path).records())
        .unwrap();
    assert_eq!(records.len(), 1);
}

#[tokio::test]
async fn recovery_and_process_drain_close_submission_admission_before_wire() {
    let server = MockServer::start().await;
    let (client, _dir, _) = client(&server, false);
    client.suspend_submissions("stream gap");
    assert!(matches!(
        client.place_order("hash", &limit_order()).await,
        Err(Error::Suspended(_))
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
    client.resume_submissions();
    assert!(!client.submissions_suspended());
    client.begin_drain();
    client.resume_submissions();
    assert!(client.clone().submissions_suspended());
    assert!(matches!(
        client.place_order("hash", &limit_order()).await,
        Err(Error::Suspended(_))
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn operator_halt_survives_recovery_and_reopening() {
    let server = MockServer::start().await;
    let (client, _dir, _) = client(&server, false);
    client
        .halt_submissions("operator investigation", "verified-supervisor")
        .await
        .unwrap();
    client.suspend_submissions("stream gap");
    client.resume_submissions();
    assert!(client.operator_halt().await.unwrap().is_some());
    let reopened =
        schwab_rs::execution::OrderJournal::new(client.order_journal().path().to_owned());
    use schwab_rs::execution::ExecutionStore;
    assert!(reopened.operator_halt().await.unwrap().is_some());
    assert!(matches!(
        client.place_order("hash", &limit_order()).await,
        Err(Error::Halted(_))
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
    client
        .clear_operator_halt("verified-supervisor")
        .await
        .unwrap();
    assert!(reopened.operator_halt().await.unwrap().is_none());
}

#[tokio::test]
async fn dispatch_revalidation_runs_after_quota_and_before_wire() {
    use schwab_rs::{
        coordination::{Coordinator, Lease},
        risk::RiskProvider,
        storage::StateDatabase,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    #[derive(Debug)]
    struct DelayedQuota {
        db: StateDatabase,
        waited: Arc<AtomicBool>,
    }
    #[async_trait::async_trait]
    impl Coordinator for DelayedQuota {
        async fn quota(&self, _: &str, _: u32, _: u32) -> schwab_rs::Result<Duration> {
            self.waited.store(true, Ordering::SeqCst);
            Ok(Duration::ZERO)
        }
        async fn acquire(&self, r: &str, o: &str, t: Duration) -> schwab_rs::Result<Lease> {
            self.db.acquire(r, o, t).await
        }
        async fn renew(&self, l: &Lease, t: Duration) -> schwab_rs::Result<Lease> {
            self.db.renew(l, t).await
        }
        async fn release(&self, l: &Lease) -> schwab_rs::Result<()> {
            self.db.release(l).await
        }
        async fn valid(&self, l: &Lease) -> schwab_rs::Result<bool> {
            self.db.valid(l).await
        }
    }
    struct Risk {
        waited: Arc<AtomicBool>,
        checked: Arc<AtomicBool>,
    }
    #[async_trait::async_trait]
    impl RiskProvider for Risk {
        async fn reserve(&self, _: &str, _: &str, _: &OrderRequest) -> schwab_rs::Result<()> {
            assert!(!self.waited.load(Ordering::SeqCst));
            Ok(())
        }
        async fn revalidate(
            &self,
            account: &str,
            _: &str,
            _: &OrderRequest,
        ) -> schwab_rs::Result<()> {
            assert!(self.waited.load(Ordering::SeqCst));
            self.checked.store(true, Ordering::SeqCst);
            Err(Error::BaselineStale {
                account: account.into(),
                reason: "baseline expired during wait".into(),
            })
        }
        async fn release(&self, _: &str) -> schwab_rs::Result<()> {
            Ok(())
        }
    }
    let server = MockServer::start().await;
    let (base, _dir, _) = client(&server, false);
    let mut config = SchwabConfig {
        oauth: base.auth_manager().config().clone(),
        client: base.client_config().clone(),
        streaming: None,
    };
    config.client.rate_limit.enabled = true;
    let waited = Arc::new(AtomicBool::new(false));
    let checked = Arc::new(AtomicBool::new(false));
    let client = SchwabClient::new(config)
        .unwrap()
        .with_coordinator(Arc::new(DelayedQuota {
            db: base.order_journal().database().clone(),
            waited: waited.clone(),
        }))
        .unwrap()
        .with_risk_provider(Arc::new(Risk {
            waited,
            checked: checked.clone(),
        }))
        .unwrap();
    assert!(matches!(
        client.place_order("hash", &limit_order()).await,
        Err(Error::BaselineStale { account, .. }) if account == "hash"
    ));
    assert!(checked.load(Ordering::SeqCst));
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(matches!(
        client.order_journal().records().await.unwrap()[0].outcome,
        Some(schwab_rs::execution::SubmissionOutcome::Rejected)
    ));
}

#[tokio::test]
async fn reconciliation_splits_full_pages_deduplicates_and_fetches_known_old_orders() {
    use schwab_rs::reconciliation::ReconciliationOptions;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let server = MockServer::start().await;
    let (client, _dir, _) = client(&server, false);
    client
        .execution_store()
        .track_order("hash", "77")
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let n = calls.clone();
    Mock::given(path("/trader/v1/accounts/hash/orders"))
        .respond_with(move |_: &wiremock::Request| {
            let values = if n.fetch_add(1, Ordering::SeqCst) == 0 {
                vec![serde_json::json!({"orderId":1,"status":"FILLED"}); 3000]
            } else {
                vec![serde_json::json!({"orderId":1,"status":"FILLED"})]
            };
            ResponseTemplate::new(200).set_body_json(values)
        })
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(path("/trader/v1/accounts/hash/orders/77"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"orderId":77,"status":"CANCELED"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/trader/v1/accounts/hash")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"securitiesAccount":{"positions":[],"currentBalances":{"availableFunds":100}}}))).expect(1).mount(&server).await;
    let report = client
        .reconcile_account_with_options(
            "hash",
            1,
            ReconciliationOptions {
                lookback_days: 2,
                max_pages: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(report.reconciled);
    assert_eq!(report.orders_observed, 2);
    assert!(
        client
            .execution_store()
            .reconciliation_targets("hash")
            .await
            .unwrap()
            .is_empty()
    );
    server.verify().await;
}

#[tokio::test]
async fn reconciliation_deadline_and_missing_target_leave_account_unready() {
    use schwab_rs::reconciliation::ReconciliationOptions;
    let server = MockServer::start().await;
    let (client, _dir, _) = client(&server, false);
    Mock::given(path("/trader/v1/accounts/hash/orders"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!([]))
                .set_delay(Duration::from_millis(100)),
        )
        .mount(&server)
        .await;
    assert!(
        client
            .reconcile_account_with_options(
                "hash",
                1,
                ReconciliationOptions {
                    timeout_ms: 10,
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    assert!(
        !client
            .execution_store()
            .account_state("hash")
            .await
            .unwrap()
            .unwrap()
            .reconciled
    );
    client
        .execution_store()
        .track_order("hash", "77")
        .await
        .unwrap();
    assert!(
        client
            .reconcile_account_with_options(
                "hash",
                1,
                ReconciliationOptions {
                    timeout_ms: 1000,
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    assert!(
        !client
            .execution_store()
            .account_state("hash")
            .await
            .unwrap()
            .unwrap()
            .reconciled
    );
}

/// Submissions for different accounts on one credential take turns on the
/// execution lease; the second waits for the first instead of failing
/// (and being journaled as refused) without reaching the broker.
#[tokio::test]
async fn concurrent_submissions_share_the_execution_lease() {
    let server = MockServer::start().await;
    for (account, delay) in [("a", 300), ("b", 0)] {
        Mock::given(method("POST"))
            .and(path(format!("/trader/v1/accounts/{account}/orders")))
            .respond_with(
                ResponseTemplate::new(201)
                    .insert_header(
                        "Location",
                        format!("https://api.schwabapi.com/trader/v1/accounts/{account}/orders/7"),
                    )
                    .set_delay(Duration::from_millis(delay)),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    let (client, _dir, _) = client(&server, false);
    let order = limit_order();
    let slow = client.place_order("a", &order);
    let fast = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        client.place_order("b", &order).await
    };
    let (slow, fast) = tokio::join!(slow, fast);
    assert_eq!(slow.unwrap().order_id.as_deref(), Some("7"));
    assert_eq!(fast.unwrap().order_id.as_deref(), Some("7"));
    server.verify().await;
}

/// One account's unresolved intent blocks that account, not its neighbours.
#[tokio::test]
async fn pending_submissions_are_counted_per_account() {
    let server = MockServer::start().await;
    let (client, _dir, _) = client(&server, false);
    let store = client.execution_store();
    store
        .begin_once("a", "place_order", serde_json::json!({"tag": "x"}), None)
        .await
        .unwrap();
    assert_eq!(store.pending_count().await.unwrap(), 1);
    assert_eq!(store.pending_count_for(&["a".into()]).await.unwrap(), 1);
    assert_eq!(store.pending_count_for(&["b".into()]).await.unwrap(), 0);
    let readiness = client.readiness(&["b".into()], 5000).await.unwrap();
    assert_eq!(readiness.pending_submissions, 0);
}

/// After a complete pass, the order list covers only the time since it (less
/// the overlap), and a working order the list returned is not fetched again.
#[tokio::test]
async fn incremental_reconciliation_narrows_the_window_and_skips_listed_targets() {
    use schwab_rs::reconciliation::{ReconciliationOptions, SINCE_OVERLAP};
    let server = MockServer::start().await;
    let (client, _dir, _) = client(&server, false);
    // Order 5 is working and already tracked; the list will return it.
    client
        .execution_store()
        .track_order("hash", "5")
        .await
        .unwrap();
    Mock::given(path("/trader/v1/accounts/hash/orders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"orderId": 5, "status": "WORKING", "orderStrategyType": "SINGLE"}
        ])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/trader/v1/accounts/hash/orders/5"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(path("/trader/v1/accounts/hash"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "securitiesAccount": {"positions": [], "currentBalances": {"availableFunds": 100}}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let since = chrono::Utc::now() - chrono::Duration::hours(1);
    let report = client
        .reconcile_account_with_options(
            "hash",
            1,
            ReconciliationOptions {
                since: Some(since),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(report.reconciled);
    assert!(report.started_at >= since.timestamp_millis());
    let list = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.url.path() == "/trader/v1/accounts/hash/orders")
        .unwrap();
    let from = list
        .url
        .query_pairs()
        .find(|(k, _)| k == "fromEnteredTime")
        .unwrap()
        .1
        .into_owned();
    let from = chrono::DateTime::parse_from_rfc3339(&from).unwrap();
    let expected = since - SINCE_OVERLAP;
    assert!(
        (from.timestamp_millis() - expected.timestamp_millis()).abs() < 1000,
        "window starts {from}, expected {expected}"
    );
    server.verify().await;
}

/// A replacement reads the order it replaces first, so the risk check judges
/// an order placed elsewhere and not reconciled yet by its real payload, and
/// a replacement of an order Schwab no longer has is refused unsent.
#[tokio::test]
async fn replacing_an_unreconciled_order_reads_it_before_the_risk_check() {
    use schwab_rs::risk::{PortfolioLimits, PortfolioRisk};
    use std::collections::HashMap;
    use std::sync::Arc;
    let server = MockServer::start().await;
    let (base, _dir, _) = client(&server, false);
    let db = base.order_journal().database().clone();
    db.invalidate_account("hash", 1).await.unwrap();
    db.reconcile_snapshot(
        "hash",
        1,
        chrono::Utc::now().timestamp_millis(),
        serde_json::json!({"securitiesAccount":{"currentBalances":{"availableFunds":"1000"},"positions":[]}}),
        vec![],
    )
    .await
    .unwrap();
    let limits = PortfolioLimits {
        max_open_quantity: 10.into(),
        max_open_notional: 1000.into(),
        max_position_quantity: 10.into(),
        max_snapshot_age_ms: 60_000,
        max_daily_loss: None,
    };
    let client = base
        .with_risk_provider(Arc::new(PortfolioRisk {
            database: db,
            accounts: HashMap::from([("hash".into(), limits)]),
        }))
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/hash/orders/77"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "orderId": 77, "status": "WORKING", "orderStrategyType": "SINGLE",
            "orderType": "LIMIT", "price": 10.0, "quantity": 1.0,
            "filledQuantity": 0.0, "remainingQuantity": 1.0,
            "orderLegCollection": [{"orderLegType": "EQUITY", "legId": 1,
                "instruction": "BUY", "quantity": 1.0,
                "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/trader/v1/accounts/hash/orders/77"))
        .respond_with(ResponseTemplate::new(201).insert_header(
            "Location",
            "https://api.schwabapi.com/trader/v1/accounts/hash/orders/78",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let replaced = client
        .replace_order("hash", "77", &limit_order())
        .await
        .unwrap();
    assert_eq!(replaced.order_id.as_deref(), Some("78"));

    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/hash/orders/99"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    assert!(
        client
            .replace_order("hash", "99", &limit_order())
            .await
            .is_err()
    );
    // The refusal leaves no trace: no placeholder for reconciliation to
    // chase, and the account still admits orders.
    use schwab_rs::execution::ExecutionStore;
    assert!(
        !client
            .order_journal()
            .reconciliation_targets("hash")
            .await
            .unwrap()
            .contains(&"99".to_string())
    );
    Mock::given(method("POST"))
        .and(path("/trader/v1/accounts/hash/orders"))
        .respond_with(ResponseTemplate::new(201).insert_header(
            "Location",
            "https://api.schwabapi.com/trader/v1/accounts/hash/orders/79",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let mut other = limit_order();
    other.tag = Some("after-refusal".into());
    client.place_order("hash", &other).await.unwrap();
    server.verify().await;
}
