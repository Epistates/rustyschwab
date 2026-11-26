//! What reconciliation must decide about an account from the broker's
//! orders and positions: which passes may write a baseline, what each pass
//! records, and what stays reserved against the risk limits.
use rustyschwab::{
    Error,
    execution::{OrderJournal, SubmissionOutcome},
    reconciliation::ReconciliationReport,
    risk::{PortfolioLimits, PortfolioRisk, RiskProvider},
    types::{Decimal, trading::OrderRequest},
};
use serde_json::{Value, json};
use std::{collections::HashMap, time::Duration};

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
