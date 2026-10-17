//! The recovery worker decides whether trading is ready: it must turn
//! ready once an account reconciles, turn unready when a pass fails, scan
//! incrementally after a full scan, and run sooner when activity asks.
mod common;

use common::mock_client;
use rustyschwab::recovery::RecoveryWorker;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

async fn broker(failing: Arc<AtomicBool>) -> MockServer {
    let server = MockServer::start().await;
    let orders_failing = failing.clone();
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/A/orders"))
        .respond_with(move |_: &Request| {
            if orders_failing.load(Ordering::SeqCst) {
                ResponseTemplate::new(503)
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!([]))
            }
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/A"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "securitiesAccount": {"currentBalances": {"availableFunds": 100}, "positions": []}
        })))
        .mount(&server)
        .await;
    server
}

async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test]
async fn readiness_follows_reconciliation_and_failures_unready_it() {
    let failing = Arc::new(AtomicBool::new(false));
    let server = broker(failing.clone()).await;
    let (client, _dir, _) = mock_client(&server, false);
    let client = Arc::new(client);
    let mut worker =
        RecoveryWorker::start(client.clone(), vec!["A".into()], 1, Duration::from_secs(1));
    tokio::time::timeout(Duration::from_secs(10), worker.wait_ready())
        .await
        .unwrap()
        .unwrap();
    let status = worker.status();
    assert!(status.ready);
    assert!(status.pending_accounts.is_empty());
    assert!(status.last_success.is_some());

    // A pass that cannot finish leaves the account unready, and says which.
    failing.store(true, Ordering::SeqCst);
    until("an unready status", || !worker.status().ready).await;
    assert_eq!(worker.status().pending_accounts, vec!["A".to_string()]);
    assert!(
        !client
            .execution_store()
            .account_state("A")
            .await
            .unwrap()
            .unwrap()
            .reconciled
    );

    // And it recovers once the broker answers again.
    failing.store(false, Ordering::SeqCst);
    until("a ready status", || worker.status().ready).await;
}

/// After the first full scan, passes list only orders entered since the
/// previous pass (less the overlap), not the whole lookback.
#[tokio::test]
async fn later_passes_scan_incrementally() {
    let server = broker(Arc::new(AtomicBool::new(false))).await;
    let (client, _dir, _) = mock_client(&server, false);
    let worker = RecoveryWorker::start(
        Arc::new(client),
        vec!["A".into()],
        1,
        Duration::from_secs(1),
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let scans = server
                .received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .filter(|r| r.url.path() == "/trader/v1/accounts/A/orders")
                .count();
            if scans >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("two order scans");
    let requests = server.received_requests().await.unwrap();
    let windows: Vec<chrono::Duration> = requests
        .iter()
        .filter(|r| r.url.path() == "/trader/v1/accounts/A/orders")
        .map(|r| {
            let param = |name: &str| {
                r.url
                    .query_pairs()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| chrono::DateTime::parse_from_rfc3339(&v).unwrap())
                    .unwrap()
            };
            param("toEnteredTime") - param("fromEnteredTime")
        })
        .collect();
    assert!(
        windows[0] > chrono::Duration::days(58),
        "the first pass scans the lookback"
    );
    assert!(
        windows[1] < chrono::Duration::hours(1),
        "the next pass scans since the last one: {:?}",
        windows[1]
    );
    drop(worker);
}

/// Account activity pulls the next pass forward from the interval.
#[tokio::test]
async fn requested_reconciliation_runs_before_the_interval() {
    let server = broker(Arc::new(AtomicBool::new(false))).await;
    let (client, _dir, _) = mock_client(&server, false);
    let client = Arc::new(client);
    let mut worker =
        RecoveryWorker::start(client.clone(), vec!["A".into()], 1, Duration::from_secs(4));
    tokio::time::timeout(Duration::from_secs(10), worker.wait_ready())
        .await
        .unwrap()
        .unwrap();
    let first = worker.status().last_success.unwrap();
    client.request_reconciliation();
    // Half the interval after the last pass, well before the full four.
    tokio::time::timeout(Duration::from_millis(3500), async {
        while worker.status().last_success == Some(first) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("activity pulls the pass forward");
}
