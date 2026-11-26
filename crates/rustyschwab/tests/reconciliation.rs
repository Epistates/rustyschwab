//! What reconciliation must decide about an account from the broker's
//! orders and positions: which passes may write a baseline, what each pass
//! records, what stays reserved against the risk limits, and how much of
//! the broker's order history a pass reads before it may call the account
//! ready.
mod common;

use common::mock_client;
use rustyschwab::{
    Error,
    execution::{OrderJournal, SubmissionOutcome},
    reconciliation::{ReconciliationOptions, ReconciliationReport},
    risk::{PortfolioLimits, PortfolioRisk, RiskProvider},
    types::{Decimal, trading::OrderRequest},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path},
};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn buy(quantity: u32) -> OrderRequest {
    serde_json::from_value(json!({
        "orderStrategyType": "SINGLE", "orderType": "LIMIT", "session": "NORMAL",
        "duration": "DAY", "price": "10",
        "orderLegCollection": [{"quantity": quantity, "instruction": "BUY",
            "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}}]
    }))
    .unwrap()
}

/// `order` as journaled under order tag `tag`.
fn tagged(order: &OrderRequest, tag: &str) -> Value {
    let mut request = serde_json::to_value(order).unwrap();
    request["tag"] = json!(tag);
    request
}

/// How Schwab lists `request`: under its own order ID, working.
fn listed(request: &Value, order_id: u64) -> Value {
    let mut order = request.clone();
    order["orderId"] = json!(order_id);
    order["status"] = json!("WORKING");
    order
}

fn funded() -> Value {
    json!({"securitiesAccount": {"currentBalances": {"availableFunds": "1000"}, "positions": []}})
}

fn age_submissions(journal: &OrderJournal, by: Duration) {
    rusqlite::Connection::open(journal.path())
        .unwrap()
        .execute(
            "UPDATE submissions SET created_at=created_at-?1",
            [by.as_millis() as i64],
        )
        .unwrap();
}

/// Account "A" in a fresh journal, with a reconciled, funded baseline and
/// the risk policy that reserves against it.
struct Account {
    _dir: tempfile::TempDir,
    journal: OrderJournal,
    risk: PortfolioRisk,
}

impl Account {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let journal = OrderJournal::new(dir.path().join("orders.json"));
        let risk = PortfolioRisk {
            database: journal.database().clone(),
            accounts: HashMap::from([(
                "A".into(),
                PortfolioLimits {
                    max_open_quantity: 100.into(),
                    max_open_notional: 10_000.into(),
                    max_position_quantity: 200.into(),
                    max_snapshot_age_ms: 5000,
                    max_daily_loss: None,
                    option_multipliers: Default::default(),
                },
            )]),
        };
        let account = Self {
            _dir: dir,
            journal,
            risk,
        };
        account.pass(vec![]).await;
        account
    }

    /// Run a pass that lists `orders`, with its report and how many times
    /// it was audited as a change to the account.
    async fn pass(&self, orders: Vec<Value>) -> (ReconciliationReport, usize) {
        let before = self.events("account_reconciled").await;
        let report = self
            .journal
            .database()
            .reconcile_snapshot(
                "A",
                1,
                chrono::Utc::now().timestamp_millis(),
                funded(),
                orders,
            )
            .await
            .unwrap();
        (report, self.events("account_reconciled").await - before)
    }

    /// How many audit events of `kind` the journal holds.
    async fn events(&self, kind: &str) -> usize {
        self.journal
            .audit_page(0, 10_000)
            .await
            .unwrap()
            .iter()
            .filter(|event| event.kind == kind)
            .count()
    }

    /// An intent to buy `quantity` at 10 under `tag`, with its risk
    /// reserved, that Schwab accepted as order `order_id`.
    async fn accepted(&self, tag: &str, quantity: u32, order_id: u64) -> String {
        let order = buy(quantity);
        let id = self
            .journal
            .begin("A", "place_order", tagged(&order, tag))
            .await
            .unwrap();
        self.risk.reserve("A", &id, &order).await.unwrap();
        self.journal.arm(&id).await.unwrap();
        self.journal
            .finish(
                &id,
                SubmissionOutcome::Accepted {
                    order_id: Some(order_id.to_string()),
                },
            )
            .await
            .unwrap();
        id
    }

    /// The quantity and notional reservation `id` holds.
    fn reservation(&self, id: &str) -> (Decimal, Decimal) {
        let (quantity, notional): (String, String) =
            rusqlite::Connection::open(self.journal.path())
                .unwrap()
                .query_row(
                    "SELECT quantity,notional FROM reservations WHERE id=?1 AND state='held'",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
        (quantity.parse().unwrap(), notional.parse().unwrap())
    }
}

/// A pass dated more than five seconds ahead of this store's clock (a
/// remote worker whose clock runs fast) would make its baseline look
/// fresher than it is: it is refused and writes nothing. A pass dated less
/// far ahead is dated by this store's clock instead.
#[tokio::test]
async fn a_pass_dated_ahead_of_the_store_is_refused_or_dated_now() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let database = journal.database();
    let now = chrono::Utc::now().timestamp_millis();
    let refused = database
        .reconcile_snapshot("A", 1, now + 60_000, funded(), vec![])
        .await
        .unwrap_err();
    assert!(matches!(refused, Error::Storage(_)), "{refused:?}");
    assert!(database.account_state("A").await.unwrap().is_none());

    let report = database
        .reconcile_snapshot("A", 1, now + 1000, funded(), vec![])
        .await
        .unwrap();
    let after = chrono::Utc::now().timestamp_millis();
    assert!(
        report.started_at <= after,
        "dated ahead of the store's clock"
    );
    let state = database.account_state("A").await.unwrap().unwrap();
    assert!(state.reconciled);
    assert_eq!(state.observed_at, report.started_at);
}

/// A pass that began before the pass behind the stored baseline is
/// superseded: it writes nothing, and the newer baseline stands. A pass that
/// began at the same moment is not older, and it commits.
#[tokio::test]
async fn a_pass_older_than_the_baseline_is_superseded() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let database = journal.database();
    let now = chrono::Utc::now().timestamp_millis();
    let pass = |started_at: i64, snapshot: &str| {
        database.reconcile_snapshot("A", 1, started_at, json!({"pass": snapshot}), vec![])
    };
    pass(now, "newer").await.unwrap();

    let older = pass(now - 1000, "older").await.unwrap();
    assert!(older.superseded);
    let state = database.account_state("A").await.unwrap().unwrap();
    assert_eq!(state.snapshot, json!({"pass": "newer"}));
    assert_eq!(state.observed_at, now);

    let same = pass(now, "same").await.unwrap();
    assert!(!same.superseded);
    let state = database.account_state("A").await.unwrap().unwrap();
    assert_eq!(state.snapshot, json!({"pass": "same"}));
}

/// A pass that resolves an attempt is audited, even when the account was
/// unreconciled before it and still is after it (another attempt is still
/// waiting for its order).
#[tokio::test]
async fn a_pass_that_resolves_an_attempt_is_audited() {
    let account = Account::new().await;
    let journal = &account.journal;
    let first = tagged(&buy(1), "first");
    let accepted = journal
        .begin("A", "place_order", first.clone())
        .await
        .unwrap();
    journal.arm(&accepted).await.unwrap();
    journal
        .finish(&accepted, SubmissionOutcome::Accepted { order_id: None })
        .await
        .unwrap();
    let waiting = journal
        .begin("A", "place_order", tagged(&buy(2), "second"))
        .await
        .unwrap();
    journal.arm(&waiting).await.unwrap();
    let (report, _) = account.pass(vec![]).await;
    assert!(!report.reconciled);

    let (report, audited) = account.pass(vec![listed(&first, 42)]).await;
    assert_eq!(report.resolved_attempts, vec![accepted]);
    assert_eq!(report.unresolved_attempts, vec![waiting]);
    assert!(!report.reconciled);
    assert_eq!(audited, 1);
}

/// A pass that releases a reservation because its order reached a
/// terminal status is audited.
#[tokio::test]
async fn a_pass_that_releases_a_terminal_order_is_audited() {
    let account = Account::new().await;
    let id = account.accepted("done", 1, 42).await;
    let (report, audited) = account
        .pass(vec![json!({"orderId": 42, "status": "FILLED"})])
        .await;
    assert_eq!(report.terminal_attempts, vec![id]);
    assert!(report.reconciled);
    assert_eq!(audited, 1);
}

/// A pass that resolves an intent as never dispatched is audited.
#[tokio::test]
async fn a_pass_that_resolves_an_unsent_intent_is_audited() {
    let account = Account::new().await;
    let id = account
        .journal
        .begin("A", "place_order", tagged(&buy(1), "unsent"))
        .await
        .unwrap();
    age_submissions(&account.journal, Duration::from_secs(11 * 60));
    let (report, audited) = account.pass(vec![]).await;
    assert_eq!(report.never_dispatched_attempts, vec![id]);
    assert!(report.reconciled);
    assert_eq!(audited, 1);
}

/// A pass that observes a new order activity (a fill) is audited; one that
/// sees only activity it has already observed is not.
#[tokio::test]
async fn a_pass_that_observes_new_activity_is_audited() {
    let account = Account::new().await;
    let fill = |price: u32| {
        json!({"activityType": "EXECUTION",
            "executionLegs": [{"legId": 1, "quantity": 1, "price": price}]})
    };
    let working = |activity: Vec<Value>| json!({"orderId": 7, "status": "WORKING", "orderActivityCollection": activity});
    account.pass(vec![working(vec![fill(10)])]).await;

    let (report, audited) = account.pass(vec![working(vec![fill(10), fill(11)])]).await;
    assert_eq!(report.newly_observed_activities, 1);
    assert_eq!(audited, 1);
    let (report, audited) = account.pass(vec![working(vec![fill(10), fill(11)])]).await;
    assert_eq!(report.newly_observed_activities, 0);
    assert_eq!(audited, 0);
}

/// A pass that releases a reservation left held for a refused submission
/// (as releases were before `finish` made them) is audited, and the
/// reservation no longer counts against the account.
#[tokio::test]
async fn a_pass_that_releases_an_orphaned_reservation_is_audited() {
    let account = Account::new().await;
    let order = buy(1);
    let id = account
        .journal
        .begin("A", "place_order", tagged(&order, "refused"))
        .await
        .unwrap();
    account.risk.reserve("A", &id, &order).await.unwrap();
    account
        .journal
        .finish(&id, SubmissionOutcome::Rejected)
        .await
        .unwrap();
    rusqlite::Connection::open(account.journal.path())
        .unwrap()
        .execute("UPDATE reservations SET state='held' WHERE id=?1", [&id])
        .unwrap();

    let (report, audited) = account.pass(vec![]).await;
    assert!(report.reconciled);
    assert_eq!(audited, 1);
    let held: i64 = rusqlite::Connection::open(account.journal.path())
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM reservations WHERE state='held'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(held, 0);
}

/// A pass that shrinks a reservation to a partial fill is audited. An
/// unfilled order, or a fill the reservation already reflects, changes
/// nothing, and such a pass is not.
#[tokio::test]
async fn a_pass_that_shrinks_a_reservation_is_audited() {
    let account = Account::new().await;
    account.accepted("part", 10, 42).await;
    let working = |filled: u32| {
        json!({"orderId": 42, "status": "WORKING", "quantity": 10,
            "filledQuantity": filled, "remainingQuantity": 10 - filled})
    };
    let (_, audited) = account.pass(vec![working(0)]).await;
    assert_eq!(audited, 0);
    let (_, audited) = account.pass(vec![working(6)]).await;
    assert_eq!(audited, 1);
    let (_, audited) = account.pass(vec![working(6)]).await;
    assert_eq!(audited, 0);
}

/// A held reservation shrinks to what its working order can still fill,
/// valued at the reservation's own price. A reading that would not shrink
/// it (nothing filled, more left than reserved, more filled than the order
/// holds) leaves it whole and records no fill: holding too much is the safe
/// error.
#[tokio::test]
async fn a_reservation_shrinks_only_to_what_its_order_can_still_fill() {
    let account = Account::new().await;
    let id = account.accepted("part", 10, 42).await;
    let working = |fill: Value| {
        let mut order = json!({"orderId": 42, "status": "WORKING"});
        order
            .as_object_mut()
            .unwrap()
            .extend(fill.as_object().unwrap().clone());
        order
    };
    assert_eq!(account.reservation(&id), (10.into(), 100.into()));
    for whole in [
        json!({"quantity": 10, "filledQuantity": 0, "remainingQuantity": 10}),
        json!({"remainingQuantity": 12}),
        json!({"quantity": 10, "filledQuantity": 12}),
    ] {
        account.pass(vec![working(whole.clone())]).await;
        assert_eq!(account.reservation(&id), (10.into(), 100.into()), "{whole}");
    }
    assert_eq!(account.events("risk_reservation_filled").await, 0);

    account
        .pass(vec![working(json!({"quantity": 10, "filledQuantity": 6}))])
        .await;
    assert_eq!(account.reservation(&id), (4.into(), 40.into()));
    assert_eq!(account.events("risk_reservation_filled").await, 1);
}

/// Mount Schwab's answers for an account with no orders, so a pass over it
/// completes.
async fn mount_empty_account(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/hash/orders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(server)
        .await;
    mount_positions(server).await;
}

async fn mount_positions(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/hash"))
        .respond_with(ResponseTemplate::new(200).set_body_json(funded()))
        .mount(server)
        .await;
}

/// The window each order-list request asked for, as `(fromEnteredTime,
/// toEnteredTime)` in milliseconds since the epoch, in the order sent, with
/// the page size it asked for.
async fn listed_windows(server: &MockServer) -> Vec<((i64, i64), Option<String>)> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/trader/v1/accounts/hash/orders")
        .map(|request| {
            let param = |name: &str| {
                request
                    .url
                    .query_pairs()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.into_owned())
            };
            let time = |name: &str| {
                chrono::DateTime::parse_from_rfc3339(&param(name).unwrap())
                    .unwrap()
                    .timestamp_millis()
            };
            (
                (time("fromEnteredTime"), time("toEnteredTime")),
                param("maxResults"),
            )
        })
        .collect()
}

/// Options out of their ranges are refused, each on its own, before
/// anything is read or written: no request reaches Schwab, and the
/// account's state is untouched.
#[tokio::test]
async fn reconciliation_bounds_are_refused_one_at_a_time() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    let valid = ReconciliationOptions::default();
    for options in [
        ReconciliationOptions {
            lookback_days: 0,
            ..valid.clone()
        },
        ReconciliationOptions {
            lookback_days: 3651,
            ..valid.clone()
        },
        ReconciliationOptions {
            max_pages: 0,
            ..valid.clone()
        },
        ReconciliationOptions {
            max_pages: 4097,
            ..valid.clone()
        },
        ReconciliationOptions {
            max_targeted_orders: 0,
            ..valid.clone()
        },
        ReconciliationOptions {
            max_targeted_orders: 10_001,
            ..valid.clone()
        },
        ReconciliationOptions {
            timeout_ms: 0,
            ..valid.clone()
        },
        ReconciliationOptions {
            timeout_ms: 300_001,
            ..valid.clone()
        },
    ] {
        let refused = client
            .reconcile_account_with_options("hash", 1, options.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(refused, Error::InvalidParameter(_)),
            "{options:?}: {refused:?}"
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(
        client
            .execution_store()
            .account_state("hash")
            .await
            .unwrap()
            .is_none()
    );
}

/// Schwab's order list reaches back 60 days, so the lookback is read from
/// its start to the start of the pass in consecutive windows of 59 days,
/// the last one shorter: a lookback of 59 days or less is one window. Each
/// asks for a page of 3000 orders, the size at which a page counts as
/// truncated.
#[tokio::test]
async fn the_lookback_is_read_in_consecutive_windows_inside_the_horizon() {
    // Each window as (days before the pass's start it begins, and ends).
    for (lookback_days, windows) in [
        (120, vec![(120, 61), (61, 2), (2, 0)]),
        (118, vec![(118, 59), (59, 0)]),
        (60, vec![(60, 1), (1, 0)]),
        (59, vec![(59, 0)]),
        (2, vec![(2, 0)]),
    ] {
        let server = MockServer::start().await;
        let (client, _dir, _) = mock_client(&server, false);
        mount_empty_account(&server).await;
        let report = client
            .reconcile_account_with_options(
                "hash",
                1,
                ReconciliationOptions {
                    lookback_days,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(report.reconciled);
        let before = |days: i64| report.started_at - days * DAY_MS;
        let listed = listed_windows(&server).await;
        assert_eq!(
            listed.iter().map(|(window, _)| *window).collect::<Vec<_>>(),
            windows
                .iter()
                .map(|&(from, to)| (before(from), before(to)))
                .collect::<Vec<_>>(),
            "a lookback of {lookback_days} days"
        );
        for (window, page) in &listed {
            assert_eq!(page.as_deref(), Some("3000"), "{window:?}");
        }
    }
}

/// Every window read counts against the page budget, so a lookback that
/// needs more windows than the budget allows fails once the budget is
/// spent, without asking for more.
#[tokio::test]
async fn every_window_counts_against_the_page_budget() {
    for (max_pages, completes) in [(2, false), (3, true)] {
        let server = MockServer::start().await;
        let (client, _dir, _) = mock_client(&server, false);
        mount_empty_account(&server).await;
        let pass = client
            .reconcile_account_with_options(
                "hash",
                1,
                ReconciliationOptions {
                    lookback_days: 120,
                    max_pages,
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(pass.is_ok(), completes, "{max_pages} pages: {pass:?}");
        assert_eq!(listed_windows(&server).await.len(), max_pages);
    }
}

/// A full page may have left orders out, so its window is read again as
/// its two halves, which together cover it exactly; a page with room to
/// spare is complete.
#[tokio::test]
async fn a_full_page_is_read_again_as_its_two_halves() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    let pages = AtomicUsize::new(0);
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/hash/orders"))
        .respond_with(move |_: &Request| {
            let orders = if pages.fetch_add(1, Ordering::SeqCst) == 0 {
                vec![json!({"orderId": 1, "status": "FILLED"}); 3000]
            } else {
                vec![]
            };
            ResponseTemplate::new(200).set_body_json(orders)
        })
        .mount(&server)
        .await;
    mount_positions(&server).await;
    let report = client
        .reconcile_account_with_options(
            "hash",
            1,
            ReconciliationOptions {
                lookback_days: 2,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(report.reconciled);
    let start = report.started_at;
    assert_eq!(
        listed_windows(&server)
            .await
            .into_iter()
            .map(|(window, _)| window)
            .collect::<Vec<_>>(),
        [
            (start - 2 * DAY_MS, start),
            (start - 2 * DAY_MS, start - DAY_MS),
            (start - DAY_MS, start),
        ]
    );
}

/// An intent still unresolved from before the incremental window widens
/// it: the order list is read from one second before the intent was
/// journaled, so the order it may have become is in the scan.
#[tokio::test]
async fn an_older_unresolved_intent_widens_the_scan_to_just_before_it() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    let journal = client.order_journal();
    let id = journal
        .begin("hash", "place_order", tagged(&buy(1), "older"))
        .await
        .unwrap();
    journal.arm(&id).await.unwrap();
    age_submissions(journal, Duration::from_secs(2 * 60 * 60));
    let created_at = journal.record(&id).await.unwrap().unwrap().created_at;
    mount_empty_account(&server).await;
    let report = client
        .reconcile_account_with_options(
            "hash",
            1,
            ReconciliationOptions {
                since: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(report.unresolved_attempts, vec![id]);
    let windows = listed_windows(&server).await;
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].0, (created_at - 1000, report.started_at));
}

/// Schwab's "no such order" for a reference this store never saw answered
/// (kept when the answer to a submission was lost) ends it: the reference
/// is forgotten, and the pass completes.
#[tokio::test]
async fn a_placeholder_the_broker_does_not_have_is_forgotten() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    let store = client.execution_store();
    store.track_order("hash", "77").await.unwrap();
    mount_empty_account(&server).await;
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/hash/orders/77"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let report = client.reconcile_account("hash", 1).await.unwrap();
    assert!(report.reconciled);
    assert!(
        store
            .reconciliation_targets("hash")
            .await
            .unwrap()
            .is_empty()
    );
    server.verify().await;
}

/// Only that answer ends a reference. An order this store has seen that
/// Schwab no longer has, or a reference Schwab cannot answer for, fails the
/// pass: the account is no longer ready, and the reference is kept for the
/// next pass.
#[tokio::test]
async fn a_reference_the_broker_cannot_account_for_fails_the_pass() {
    for (order_id, status) in [("78", 404), ("77", 503)] {
        let server = MockServer::start().await;
        let (client, _dir, _) = mock_client(&server, false);
        let store = client.execution_store();
        store
            .reconcile_snapshot(
                "hash",
                1,
                chrono::Utc::now().timestamp_millis(),
                funded(),
                vec![],
            )
            .await
            .unwrap();
        if order_id == "78" {
            store
                .record_order(
                    "hash",
                    json!({"orderId": 78, "status": "WORKING", "orderType": "LIMIT"}),
                )
                .await
                .unwrap();
        } else {
            store.track_order("hash", order_id).await.unwrap();
        }
        mount_empty_account(&server).await;
        Mock::given(method("GET"))
            .and(path(format!("/trader/v1/accounts/hash/orders/{order_id}")))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        assert!(
            client.reconcile_account("hash", 1).await.is_err(),
            "{order_id} answered {status}"
        );
        assert!(
            !store
                .account_state("hash")
                .await
                .unwrap()
                .unwrap()
                .reconciled
        );
        assert_eq!(
            store.reconciliation_targets("hash").await.unwrap(),
            [order_id]
        );
    }
}

/// A stored reference that is not a broker order ID (a damaged store, or
/// one shared with something else) fails the pass before Schwab is asked
/// about it.
#[tokio::test]
async fn a_stored_reference_that_is_not_an_order_id_fails_the_pass_unasked() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    rusqlite::Connection::open(client.order_journal().path())
        .unwrap()
        .execute(
            "INSERT INTO orders VALUES('hash','A1',0,?1)",
            [json!({"orderId": "A1", "status": "UNKNOWN"}).to_string()],
        )
        .unwrap();
    mount_empty_account(&server).await;
    assert!(client.reconcile_account("hash", 1).await.is_err());
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.url.path() != "/trader/v1/accounts/hash/orders/A1")
    );
}

/// Holds a pass's order-list request until the test lets it go, after
/// sending the test the pass's start (the window's end): a pass in flight.
struct HeldList {
    arrived: tokio::sync::mpsc::UnboundedSender<i64>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    answer: ResponseTemplate,
}
impl Respond for HeldList {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let (_, to) = request
            .url
            .query_pairs()
            .find(|(key, _)| key == "toEnteredTime")
            .unwrap();
        let started = chrono::DateTime::parse_from_rfc3339(&to)
            .unwrap()
            .timestamp_millis();
        self.arrived.send(started).unwrap();
        // A dropped sender (the test failed) lets the request go as well.
        let _ = self.release.lock().unwrap().recv();
        self.answer.clone()
    }
}

/// A pass that fails leaves a newer baseline standing: once a pass that
/// began after it has committed, the account stays as that pass left it.
#[tokio::test]
async fn a_failed_pass_leaves_a_newer_baseline_standing() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    let (arrived, mut started) = tokio::sync::mpsc::unbounded_channel();
    let (release, held) = std::sync::mpsc::channel();
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/hash/orders"))
        .respond_with(HeldList {
            arrived,
            release: Mutex::new(held),
            answer: ResponseTemplate::new(400),
        })
        .mount(&server)
        .await;
    let mut failing = tokio::spawn({
        let client = client.clone();
        async move { client.reconcile_account("hash", 1).await }
    });
    let started = tokio::select! {
        started = started.recv() => started.unwrap(),
        ended = &mut failing => panic!("the pass ended before it listed orders: {ended:?}"),
    };
    // The newer pass begins in a later millisecond than the held one: the
    // store dates baselines in milliseconds.
    while chrono::Utc::now().timestamp_millis() <= started {
        tokio::task::yield_now().await;
    }
    let store = client.execution_store();
    let newer = store
        .reconcile_snapshot(
            "hash",
            1,
            chrono::Utc::now().timestamp_millis(),
            funded(),
            vec![],
        )
        .await
        .unwrap();
    assert!(newer.reconciled);
    release.send(()).unwrap();

    assert!(failing.await.unwrap().is_err());
    let state = store.account_state("hash").await.unwrap().unwrap();
    assert!(state.reconciled);
    assert_eq!(state.observed_at, newer.started_at);
}
