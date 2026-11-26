//! What reconciliation must decide about an account from the broker's
//! orders and positions: which passes may write a baseline, what each pass
//! records, what stays reserved against the risk limits, and how much of
//! the broker's order history a pass reads before it may call the account
//! ready.
mod common;

use common::mock_client;
use rustyschwab::{
    Error, SchwabClient,
    execution::{OrderJournal, SubmissionOutcome},
    reconciliation::{NEVER_DISPATCHED_AFTER, ReconciliationOptions, ReconciliationReport},
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

/// An unarmed intent is resolved as never dispatched only once it is older
/// than `NEVER_DISPATCHED_AFTER` when the pass begins. At exactly that age
/// its dispatcher may still be about to arm it, so it is left alone.
#[tokio::test]
async fn an_unarmed_intent_is_never_dispatched_only_past_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let id = journal
        .begin("A", "place_order", tagged(&buy(1), "unsent"))
        .await
        .unwrap();
    age_submissions(&journal, Duration::from_secs(20 * 60));
    let created_at = journal.record(&id).await.unwrap().unwrap().created_at;
    let deadline = created_at + NEVER_DISPATCHED_AFTER.num_milliseconds();
    let pass = |started_at: i64| {
        journal
            .database()
            .reconcile_snapshot("A", 1, started_at, funded(), vec![])
    };

    let at_deadline = pass(deadline).await.unwrap();
    assert!(at_deadline.never_dispatched_attempts.is_empty());
    assert!(at_deadline.reconciled);
    assert!(
        journal
            .record(&id)
            .await
            .unwrap()
            .unwrap()
            .outcome
            .is_none()
    );

    let past_deadline = pass(deadline + 1).await.unwrap();
    assert_eq!(past_deadline.never_dispatched_attempts, vec![id]);
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
        let pass = client
            .reconcile_account_with_options(
                "hash",
                1,
                ReconciliationOptions {
                    lookback_days,
                    ..Default::default()
                },
            )
            .await;
        // The windows asked for come first: a wrong cut shows as the
        // windows it asked for, whatever it did to the pass.
        let listed = listed_windows(&server).await;
        let Some(((lookback_start, _), _)) = listed.first() else {
            panic!("a lookback of {lookback_days} days read no orders");
        };
        let start = lookback_start + i64::from(lookback_days) * DAY_MS;
        let before = |days: i64| start - days * DAY_MS;
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
        let report = pass.unwrap();
        assert!(report.reconciled);
        assert_eq!(report.started_at, start);
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
        assert_eq!(pass.is_ok(), completes, "{max_pages} pages");
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

/// Orders this store still follows that the list did not return are
/// fetched one by one, at most `max_targeted_orders` of them: a pass with
/// more fails before fetching any, and one with exactly that many fetches
/// them all.
#[tokio::test]
async fn one_by_one_fetches_are_bounded_by_their_budget() {
    for (max_targeted_orders, completes) in [(1, false), (2, true)] {
        let server = MockServer::start().await;
        let (client, _dir, _) = mock_client(&server, false);
        let store = client.execution_store();
        mount_empty_account(&server).await;
        for order_id in [77, 78] {
            store
                .track_order("hash", &order_id.to_string())
                .await
                .unwrap();
            Mock::given(method("GET"))
                .and(path(format!("/trader/v1/accounts/hash/orders/{order_id}")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"orderId": order_id, "status": "CANCELED"})),
                )
                .mount(&server)
                .await;
        }
        let pass = client
            .reconcile_account_with_options(
                "hash",
                1,
                ReconciliationOptions {
                    max_targeted_orders,
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(
            pass.is_ok(),
            completes,
            "{max_targeted_orders} targeted orders"
        );
        let fetched = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| {
                request
                    .url
                    .path()
                    .starts_with("/trader/v1/accounts/hash/orders/")
            })
            .count();
        assert_eq!(fetched, if completes { 2 } else { 0 });
    }
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

/// A pass of `generation` over account "hash" held at its order-list
/// request, which fails once let go: what a test does meanwhile happens
/// while the pass is in flight.
struct HeldPass {
    pass: tokio::task::JoinHandle<rustyschwab::Result<ReconciliationReport>>,
    /// When the pass began, in ms since the epoch.
    started: i64,
    release: std::sync::mpsc::Sender<()>,
}
impl HeldPass {
    async fn start(server: &MockServer, client: &SchwabClient, generation: u64) -> Self {
        let (arrived, mut started) = tokio::sync::mpsc::unbounded_channel();
        let (release, held) = std::sync::mpsc::channel();
        Mock::given(method("GET"))
            .and(path("/trader/v1/accounts/hash/orders"))
            .respond_with(HeldList {
                arrived,
                release: Mutex::new(held),
                answer: ResponseTemplate::new(400),
            })
            .mount(server)
            .await;
        let mut pass = tokio::spawn({
            let client = client.clone();
            async move { client.reconcile_account("hash", generation).await }
        });
        let started = tokio::select! {
            started = started.recv() => started.unwrap(),
            ended = &mut pass => panic!("the pass ended before it listed orders: {ended:?}"),
        };
        Self {
            pass,
            started,
            release,
        }
    }

    /// Let the pass go, and how it ended.
    async fn fail(
        self,
    ) -> Result<rustyschwab::Result<ReconciliationReport>, tokio::task::JoinError> {
        self.release.send(()).unwrap();
        self.pass.await
    }
}

/// A reconciled baseline for account "hash" of `generation`, taken at
/// `started_at`.
async fn baseline_of(
    client: &SchwabClient,
    generation: u64,
    started_at: i64,
) -> ReconciliationReport {
    let store = client.execution_store();
    store.invalidate_account("hash", generation).await.unwrap();
    let report = store
        .reconcile_snapshot("hash", generation, started_at, funded(), vec![])
        .await
        .unwrap();
    assert!(report.reconciled && !report.superseded, "{report:?}");
    report
}

/// A pass that fails leaves its own generation's baseline standing when a
/// pass that began after it has already replaced it: the account stays as
/// that pass left it.
#[tokio::test]
async fn a_failed_pass_leaves_a_newer_baseline_standing() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    let held = HeldPass::start(&server, &client, 1).await;
    baseline_of(&client, 1, held.started).await;
    // The baseline of a pass that began one millisecond after the held one.
    // The store never dates a baseline past its own clock, so the state
    // file is given that millisecond directly.
    let newer = held.started + 1;
    rusqlite::Connection::open(client.order_journal().path())
        .unwrap()
        .execute(
            "UPDATE accounts SET observed_at=?1 WHERE account='hash'",
            [newer],
        )
        .unwrap();

    assert!(held.fail().await.unwrap().is_err());
    let state = client
        .execution_store()
        .account_state("hash")
        .await
        .unwrap()
        .unwrap();
    assert!(state.reconciled);
    assert_eq!(state.observed_at, newer);
}

/// A pass that fails says nothing about a newer generation's baseline: one
/// that a pass of a newer generation committed while it ran stands, even
/// when that pass began before the failed one.
#[tokio::test]
async fn a_failed_pass_leaves_a_newer_generations_baseline_standing() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    baseline_of(&client, 1, chrono::Utc::now().timestamp_millis() - 10_000).await;
    let held = HeldPass::start(&server, &client, 1).await;
    let newer = baseline_of(&client, 2, held.started - 1000).await;

    assert!(held.fail().await.unwrap().is_err());
    let state = client
        .execution_store()
        .account_state("hash")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.generation, 2);
    assert!(state.reconciled);
    assert_eq!(state.observed_at, newer.started_at);
}

/// Refuses every baseline it is shown, as a daily-loss control that trips
/// does: the pass that wrote the baseline then fails.
struct RefusesBaselines;
#[async_trait::async_trait]
impl RiskProvider for RefusesBaselines {
    async fn reserve(&self, _: &str, _: &str, _: &OrderRequest) -> rustyschwab::Result<()> {
        Ok(())
    }
    async fn observe_baseline(&self, _: &str) -> rustyschwab::Result<()> {
        Err(Error::Halted("refused for the test".into()))
    }
    async fn release(&self, _: &str) -> rustyschwab::Result<()> {
        Ok(())
    }
}

/// A pass that fails invalidates its own generation's baseline when no
/// later pass replaced it: one taken before the pass began, and one the
/// pass itself wrote before a later step failed.
#[tokio::test]
async fn a_failed_pass_invalidates_its_own_generations_baseline() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    baseline_of(&client, 1, chrono::Utc::now().timestamp_millis()).await;
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/hash/orders"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&server)
        .await;
    assert!(client.reconcile_account("hash", 1).await.is_err());
    let state = client
        .execution_store()
        .account_state("hash")
        .await
        .unwrap()
        .unwrap();
    assert!(!state.reconciled, "a baseline from before the pass stood");

    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    let client = client
        .with_risk_provider(std::sync::Arc::new(RefusesBaselines))
        .unwrap();
    mount_empty_account(&server).await;
    let refused = client.reconcile_account("hash", 1).await.unwrap_err();
    assert!(matches!(refused, Error::Halted(_)), "{refused:?}");
    let state = client
        .execution_store()
        .account_state("hash")
        .await
        .unwrap()
        .unwrap();
    assert!(state.observed_at > 0, "the pass wrote its baseline");
    assert!(!state.reconciled, "the failed pass's own baseline stood");
}

/// An account's stored generation never falls below a pass's own: the pass
/// raised it to its own when it began. A store that breaks that rule anyway
/// is not trusted: the failed pass invalidates the baseline it finds, and a
/// debug build panics as well, so the broken assumption is seen.
#[tokio::test]
async fn a_failed_pass_fails_closed_on_an_older_stored_generation() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    baseline_of(&client, 1, chrono::Utc::now().timestamp_millis() - 10_000).await;
    let held = HeldPass::start(&server, &client, 1).await;
    rusqlite::Connection::open(client.order_journal().path())
        .unwrap()
        .execute("UPDATE accounts SET generation=0 WHERE account='hash'", [])
        .unwrap();

    let ended = held.fail().await;
    if cfg!(debug_assertions) {
        let panic = ended.unwrap_err().into_panic();
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        assert!(message.contains("generation"), "{message}");
    } else {
        assert!(ended.unwrap().is_err());
    }
    let state = client
        .execution_store()
        .account_state("hash")
        .await
        .unwrap()
        .unwrap();
    assert!(!state.reconciled, "a baseline of an older generation stood");
}
