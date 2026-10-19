//! Durable-state guarantees around order submission: intents that never
//! left resolve themselves, reservations follow the journal, tags the
//! broker holds stay spent, and risk judges sells and replacements by what
//! they can actually do.
use rustyschwab::{
    coordination::Coordinator,
    execution::{ExecutionStore, OrderJournal, SubmissionOutcome},
    risk::{PortfolioLimits, PortfolioRisk, RiskProvider},
    types::trading::OrderRequest,
};
use serde_json::{Value, json};
use std::{collections::HashMap, time::Duration};

fn order(instruction: &str, symbol: &str, quantity: u32, price: &str) -> OrderRequest {
    serde_json::from_value(json!({
        "orderStrategyType": "SINGLE", "orderType": "LIMIT", "session": "NORMAL",
        "duration": "DAY", "price": price,
        "orderLegCollection": [{"quantity": quantity, "instruction": instruction,
            "instrument": {"symbol": symbol, "assetType": "EQUITY"}}]
    }))
    .unwrap()
}

fn limits() -> PortfolioLimits {
    PortfolioLimits {
        max_open_quantity: 100.into(),
        max_open_notional: 10_000.into(),
        max_position_quantity: 200.into(),
        max_snapshot_age_ms: 5000,
        max_daily_loss: None,
        option_multipliers: Default::default(),
    }
}

async fn baseline(journal: &OrderJournal, account: Value, orders: Vec<Value>) {
    journal
        .database()
        .reconcile_snapshot(
            "A",
            1,
            chrono::Utc::now().timestamp_millis(),
            json!({ "securitiesAccount": account }),
            orders,
        )
        .await
        .unwrap();
}

fn risk(journal: &OrderJournal, limits: PortfolioLimits) -> PortfolioRisk {
    PortfolioRisk {
        database: journal.database().clone(),
        accounts: HashMap::from([("A".into(), limits)]),
    }
}

fn funded() -> Value {
    json!({"securitiesAccount": {"currentBalances": {"availableFunds": "1000"}, "positions": []}})
}

fn age_submissions(journal: &OrderJournal, by: Duration) {
    let connection = rusqlite::Connection::open(journal.path()).unwrap();
    connection
        .execute(
            "UPDATE submissions SET created_at=created_at-?1",
            [by.as_millis() as i64],
        )
        .unwrap();
}

/// A process that journaled an intent and died before sending it leaves it
/// unarmed; recovery resolves it as rejected once its deadline has long
/// passed, frees its tag and reservation, and a late dispatcher can no
/// longer arm it.
#[tokio::test]
async fn an_unarmed_intent_resolves_as_never_dispatched() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    baseline(&journal, funded()["securitiesAccount"].clone(), vec![]).await;
    let buy = order("BUY", "AAPL", 1, "10");
    let mut tagged = serde_json::to_value(&buy).unwrap();
    tagged["tag"] = json!("t1");
    let id = journal
        .begin("A", "place_order", tagged.clone())
        .await
        .unwrap();
    risk(&journal, limits())
        .reserve("A", &id, &buy)
        .await
        .unwrap();

    // A fresh unarmed intent is left alone: its dispatcher may be running.
    // Unsent, it cannot be at the broker, so the account stays reconciled.
    let report = journal
        .database()
        .reconcile_snapshot(
            "A",
            1,
            chrono::Utc::now().timestamp_millis(),
            funded(),
            vec![],
        )
        .await
        .unwrap();
    assert!(report.unresolved_attempts.is_empty());
    assert!(report.never_dispatched_attempts.is_empty());
    assert!(report.reconciled);
    assert!(
        journal
            .record(&id)
            .await
            .unwrap()
            .unwrap()
            .outcome
            .is_none()
    );

    age_submissions(&journal, Duration::from_secs(11 * 60));
    let report = journal
        .database()
        .reconcile_snapshot(
            "A",
            1,
            chrono::Utc::now().timestamp_millis(),
            funded(),
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(report.never_dispatched_attempts, vec![id.clone()]);
    assert!(report.reconciled);
    assert!(matches!(
        journal.record(&id).await.unwrap().unwrap().outcome,
        Some(SubmissionOutcome::Rejected)
    ));
    assert!(matches!(
        journal.arm(&id).await,
        Err(rustyschwab::Error::Duplicate(_))
    ));
    // The tag and the reservation are free again.
    assert!(!journal.tag_spent("A", "t1").await.unwrap());
    let retry = journal.begin("A", "place_order", tagged).await.unwrap();
    risk(&journal, limits())
        .reserve("A", &retry, &buy)
        .await
        .unwrap();
}

/// An armed intent may have reached the broker: recovery never decides it
/// was not sent.
#[tokio::test]
async fn an_armed_intent_stays_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let id = journal
        .begin("A", "place_order", json!({"tag": "t1"}))
        .await
        .unwrap();
    journal.arm(&id).await.unwrap();
    age_submissions(&journal, Duration::from_secs(60 * 60));
    let report = journal
        .database()
        .reconcile_snapshot(
            "A",
            1,
            chrono::Utc::now().timestamp_millis(),
            json!({}),
            vec![],
        )
        .await
        .unwrap();
    assert!(report.never_dispatched_attempts.is_empty());
    assert_eq!(report.unresolved_attempts, vec![id]);
    assert!(!report.reconciled);
}

/// Only the tag the SDK sent resolves an unknown submission. An order that
/// fits the request but carries another tag (the broker may stamp its own,
/// such as `API_TOS:TraderAPI`) could be a manual twin, so it stays unresolved for
/// an operator; under the SDK's tag it resolves.
#[tokio::test]
async fn only_our_tag_resolves_an_unknown_submission() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let mut request = serde_json::to_value(order("BUY", "AAPL", 1, "10.00")).unwrap();
    request["tag"] = json!("rs-ours");
    let id = journal
        .begin("A", "place_order", request.clone())
        .await
        .unwrap();
    journal.arm(&id).await.unwrap();
    age_submissions(&journal, Duration::from_secs(60 * 60));
    let listed = |tag: &str| {
        let mut broker = request.clone();
        broker["tag"] = json!(tag);
        broker["orderId"] = json!(42);
        broker["status"] = json!("WORKING");
        broker
    };
    let reconcile = |orders: Vec<Value>| {
        let database = journal.database().clone();
        async move {
            database
                .reconcile_snapshot(
                    "A",
                    1,
                    chrono::Utc::now().timestamp_millis(),
                    json!({}),
                    orders,
                )
                .await
                .unwrap()
        }
    };

    let report = reconcile(vec![listed("API_TOS:TraderAPI")]).await;
    assert_eq!(report.unresolved_attempts, vec![id.clone()]);
    assert!(report.resolved_attempts.is_empty());

    let report = reconcile(vec![listed("rs-ours")]).await;
    assert_eq!(report.resolved_attempts, vec![id]);
}

/// An operator settling an unknown submission releases its reservation in
/// the same transaction; the account's capacity does not leak.
#[tokio::test]
async fn an_operator_resolution_releases_the_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    baseline(
        &journal,
        json!({"currentBalances": {"availableFunds": "100"}, "positions": []}),
        vec![],
    )
    .await;
    let risk = risk(&journal, limits());
    let buy = order("BUY", "AAPL", 9, "10");
    let id = journal
        .begin("A", "place_order", serde_json::to_value(&buy).unwrap())
        .await
        .unwrap();
    risk.reserve("A", &id, &buy).await.unwrap();
    assert!(risk.reserve("A", "next", &buy).await.is_err(), "funds held");
    journal
        .finish(
            &id,
            SubmissionOutcome::Reconciled {
                evidence: "broker shows no such order".into(),
            },
        )
        .await
        .unwrap();
    risk.reserve("A", "next", &buy).await.unwrap();
}

/// A cash account has `cashAvailableForTrading`, not `availableFunds`:
/// it can buy within its cash and sell what it holds.
#[tokio::test]
async fn a_cash_account_can_buy_and_sell() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    baseline(
        &journal,
        json!({
            "type": "CASH",
            "currentBalances": {"cashAvailableForTrading": 1000},
            "positions": [{"longQuantity": 100, "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}}]
        }),
        vec![],
    )
    .await;
    let risk = risk(&journal, limits());
    risk.reserve("A", "sell", &order("SELL", "AAPL", 100, "150"))
        .await
        .unwrap();
    risk.reserve("A", "buy", &order("BUY", "MSFT", 1, "400"))
        .await
        .unwrap();
    assert!(matches!(
        risk.reserve("A", "too-much", &order("BUY", "MSFT", 2, "400"))
            .await,
        Err(rustyschwab::Error::RiskDenied(_))
    ));
}

/// A sell is judged against its own symbol's position and working sells,
/// not the account's other orders, and draws on no buy capacity.
#[tokio::test]
async fn sells_are_judged_per_symbol() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let working_buy = json!({"orderId": 7, "status": "WORKING", "orderStrategyType": "SINGLE",
        "orderType": "LIMIT", "quantity": 1, "filledQuantity": 0, "remainingQuantity": 1,
        "price": 400, "orderLegCollection": [{"instruction": "BUY", "quantity": 1,
            "instrument": {"assetType": "EQUITY", "symbol": "MSFT"}}]});
    baseline(
        &journal,
        json!({
            "currentBalances": {"availableFunds": 1000},
            "positions": [{"longQuantity": 100, "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}}]
        }),
        vec![working_buy],
    )
    .await;
    // Open-order caps far below the position: selling it is not "opening".
    let risk = risk(
        &journal,
        PortfolioLimits {
            max_open_quantity: 10.into(),
            max_open_notional: 1000.into(),
            ..limits()
        },
    );
    risk.reserve("A", "half", &order("SELL", "AAPL", 50, "150"))
        .await
        .unwrap();
    risk.reserve("A", "rest", &order("SELL", "AAPL", 50, "150"))
        .await
        .unwrap();
    assert!(matches!(
        risk.reserve("A", "oversold", &order("SELL", "AAPL", 1, "150"))
            .await,
        Err(rustyschwab::Error::RiskDenied(_))
    ));
    assert!(matches!(
        risk.reserve("A", "unheld", &order("SELL", "MSFT", 1, "400"))
            .await,
        Err(rustyschwab::Error::RiskDenied(_))
    ));
}

/// Replacing a working order does not count it alongside its replacement:
/// a protective sell of the whole position can be moved.
#[tokio::test]
async fn a_replacement_does_not_count_the_order_it_replaces() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let working_sell = json!({"orderId": 9, "status": "WORKING", "orderStrategyType": "SINGLE",
        "orderType": "LIMIT", "quantity": 100, "filledQuantity": 0, "remainingQuantity": 100,
        "price": 140, "orderLegCollection": [{"instruction": "SELL", "quantity": 100,
            "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}}]});
    baseline(
        &journal,
        json!({
            "currentBalances": {"availableFunds": 0},
            "positions": [{"longQuantity": 100, "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}}]
        }),
        vec![working_sell],
    )
    .await;
    let risk = risk(&journal, limits());
    let moved = order("SELL", "AAPL", 100, "135");
    assert!(risk.reserve("A", "as-new", &moved).await.is_err());
    risk.reserve_replacement("A", "moved", &moved, "9")
        .await
        .unwrap();
    // The replacement remembers what it replaces when rechecked at dispatch.
    risk.revalidate("A", "moved", &moved).await.unwrap();
}

/// A tag the broker is seen to hold is spent, even with no submission in
/// this store (a restored backup that predates the order).
#[tokio::test]
async fn a_tag_the_broker_holds_cannot_be_reused() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    baseline(
        &journal,
        json!({}),
        vec![json!({"orderId": 5, "status": "WORKING", "tag": "mcp-abc"})],
    )
    .await;
    assert!(journal.tag_spent("A", "mcp-abc").await.unwrap());
    assert!(matches!(
        journal
            .begin("A", "place_order", json!({"tag": "mcp-abc"}))
            .await,
        Err(rustyschwab::Error::Duplicate(_))
    ));
    // Nor does a rejection of a different intent free it.
    let id = journal
        .begin("A", "place_order", json!({"tag": "other"}))
        .await
        .unwrap();
    journal
        .finish(&id, SubmissionOutcome::Rejected)
        .await
        .unwrap();
    assert!(journal.tag_spent("A", "mcp-abc").await.unwrap());
}

/// A placeholder for an order the broker turns out not to have is dropped;
/// an order the store saw answered is kept.
#[tokio::test]
async fn only_unobserved_placeholders_are_forgotten() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    journal.track_order("A", "77").await.unwrap();
    baseline(
        &journal,
        json!({}),
        vec![json!({"orderId": 78, "status": "WORKING", "orderType": "LIMIT"})],
    )
    .await;
    assert!(journal.forget_placeholder("A", "77").await.unwrap());
    assert!(!journal.forget_placeholder("A", "78").await.unwrap());
    assert_eq!(
        journal.reconciliation_targets("A").await.unwrap(),
        vec!["78"]
    );
}

/// A pass that changes nothing is not an audit event.
#[tokio::test]
async fn unchanged_reconciliation_passes_are_not_audited() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let passes = || async {
        journal
            .database()
            .reconcile_snapshot(
                "A",
                1,
                chrono::Utc::now().timestamp_millis(),
                json!({}),
                vec![],
            )
            .await
            .unwrap();
    };
    for _ in 0..3 {
        passes().await;
    }
    let reconciled = journal
        .audit_page(0, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "account_reconciled")
        .count();
    assert_eq!(reconciled, 1);
}

/// History pruning drops terminal orders and their activity, and keeps
/// working ones and every spent tag.
#[tokio::test]
async fn prune_history_keeps_working_orders_and_tags() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    baseline(
        &journal,
        json!({}),
        vec![
            json!({"orderId": 1, "status": "FILLED", "tag": "done",
                "orderActivityCollection": [{"executionLegs": [{"quantity": 1}]}]}),
            json!({"orderId": 2, "status": "WORKING", "tag": "live"}),
        ],
    )
    .await;
    let pruned = journal
        .prune_history(chrono::Utc::now().timestamp_millis() + 1)
        .await
        .unwrap();
    assert_eq!(pruned, 1);
    assert_eq!(
        journal.reconciliation_targets("A").await.unwrap(),
        vec!["2"]
    );
    assert!(journal.tag_spent("A", "done").await.unwrap());
    let connection = rusqlite::Connection::open(journal.path()).unwrap();
    let executions: i64 = connection
        .query_row("SELECT COUNT(*) FROM executions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(executions, 0);
}

/// A lost acquire reply does not lock the owner out of its own lease, and a
/// changed request quota is taken over rather than refused.
#[tokio::test]
async fn leases_reacquire_for_their_owner_and_quotas_follow_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let db = journal.database();
    let first = db
        .acquire("execution", "me", Duration::from_secs(30))
        .await
        .unwrap();
    let again = db
        .acquire("execution", "me", Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(first.epoch, again.epoch);
    assert!(matches!(
        db.acquire("execution", "other", Duration::from_secs(30))
            .await,
        Err(rustyschwab::Error::LeaseBusy)
    ));
    assert!(matches!(
        db.renew(
            &rustyschwab::coordination::Lease {
                epoch: first.epoch + 1,
                ..first.clone()
            },
            Duration::from_secs(30)
        )
        .await,
        Err(rustyschwab::Error::OwnershipLost)
    ));
    db.quota("credential", 2, 10).await.unwrap();
    db.quota("credential", 3, 10).await.unwrap();
    assert!(
        journal
            .audit_page(0, 100)
            .await
            .unwrap()
            .iter()
            .any(|e| e.kind == "quota_config_changed")
    );
}

/// A restored backup trusts none of its account baselines.
#[tokio::test]
async fn a_restore_invalidates_every_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    baseline(&journal, json!({}), vec![]).await;
    assert!(
        journal
            .database()
            .account_state("A")
            .await
            .unwrap()
            .unwrap()
            .reconciled
    );
    let backups = dir.path().join("backups");
    journal
        .database()
        .backup(backups.join("state.sqlite3"))
        .await
        .unwrap();
    let restored = rustyschwab::storage::StateDatabase::restore(
        backups.join("state.sqlite3"),
        dir.path().join("restored/state.sqlite3"),
    )
    .await
    .unwrap();
    assert!(
        !restored
            .account_state("A")
            .await
            .unwrap()
            .unwrap()
            .reconciled
    );
}

/// A deposit does not hide a trading loss: the day's net transfers come out
/// of the current value before it is compared with the start of the day.
#[tokio::test]
async fn a_deposit_does_not_hide_a_daily_loss() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let account = |transfers: &str| {
        json!({
            "securitiesAccount": {
                "initialBalances": {"liquidationValue": 50000},
                "currentBalances": {"liquidationValue": 51000, "availableFunds": 1000},
                "positions": []
            },
            "_dayNetTransfers": transfers
        })
    };
    let risk = risk(
        &journal,
        PortfolioLimits {
            max_daily_loss: Some(500.into()),
            option_multipliers: Default::default(),
            ..limits()
        },
    );
    // Without the deposit the account looks a thousand up.
    journal
        .database()
        .reconcile_snapshot(
            "A",
            1,
            chrono::Utc::now().timestamp_millis(),
            account("0"),
            vec![],
        )
        .await
        .unwrap();
    risk.reserve("A", "fine", &order("BUY", "AAPL", 1, "10"))
        .await
        .unwrap();
    // With a 10,000 deposit it is 9,000 down: halted.
    journal
        .database()
        .reconcile_snapshot(
            "A",
            1,
            chrono::Utc::now().timestamp_millis(),
            account("10000"),
            vec![],
        )
        .await
        .unwrap();
    assert!(matches!(
        risk.reserve("A", "halted", &order("BUY", "AAPL", 1, "10")).await,
        Err(rustyschwab::Error::Halted(reason)) if reason.contains("Daily loss 9000")
    ));
}

/// A lapsed, unused approval can be granted again; a live, spent or
/// revoked one cannot.
#[tokio::test]
async fn only_a_lapsed_approval_can_be_granted_again() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("audit.json"));
    let digest = "a".repeat(64);
    let later = || chrono::Utc::now().timestamp() + 600;
    journal
        .approve("desk", &digest, "operator", later())
        .await
        .unwrap();
    assert!(matches!(
        journal.approve("desk", &digest, "operator", later()).await,
        Err(rustyschwab::Error::Duplicate(_))
    ));
    let lapse = digest.clone();
    journal
        .database()
        .run(move |c| {
            c.execute("UPDATE approvals SET expires_at=1 WHERE digest=?1", [lapse])
                .map_err(|e| rustyschwab::Error::Storage(e.to_string()))
        })
        .await
        .unwrap();
    assert!(!journal.is_approved("desk", &digest).await.unwrap());
    journal
        .approve("desk", &digest, "operator", later())
        .await
        .unwrap();
    assert!(journal.is_approved("desk", &digest).await.unwrap());
    journal.revoke_approval(&digest, "operator").await.unwrap();
    assert!(matches!(
        journal.approve("desk", &digest, "operator", later()).await,
        Err(rustyschwab::Error::Duplicate(_))
    ));
}

/// An intent awaiting approval is recorded with its arguments, and its
/// state follows the approval through grant, use and lapse.
#[tokio::test]
async fn intents_awaiting_approval_are_recorded_with_their_state() {
    use rustyschwab::execution::IntentState;
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("audit.json"));
    let digest = "b".repeat(64);
    let args = json!({"account_hash": "A", "order": {"tag": "mcp-x"}});
    for _ in 0..2 {
        journal
            .record_intent("desk", &digest, "place_order", &args)
            .await
            .unwrap();
    }
    let state = |journal: &OrderJournal| {
        let journal = journal.clone();
        let digest = digest.clone();
        async move {
            journal
                .intents(Some(&digest), 0, 1)
                .await
                .unwrap()
                .pop()
                .unwrap()
        }
    };
    let intent = state(&journal).await;
    assert_eq!(intent.seen, 2);
    assert_eq!(intent.state, IntentState::Awaiting);
    assert_eq!(intent.arguments, args);
    journal
        .approve(
            "desk",
            &digest,
            "operator",
            chrono::Utc::now().timestamp() + 600,
        )
        .await
        .unwrap();
    assert!(matches!(
        state(&journal).await.state,
        IntentState::Approved { ref operator, .. } if operator == "operator"
    ));
    journal
        .begin_once("desk", "place_order", args.clone(), Some(digest.clone()))
        .await
        .unwrap();
    assert_eq!(state(&journal).await.state, IntentState::Spent);
}

/// An option strategy counts the most it can lose at expiration against the
/// account's funds and buy caps, a working one at the broker its unfilled
/// share; one with no bound, or with a leg whose multiplier the operator
/// has not verified, is refused.
#[tokio::test]
async fn option_strategies_reserve_their_defined_risk() {
    const C100: &str = "XYZ   260116C00100000";
    const C105: &str = "XYZ   260116C00105000";
    fn strategy(kind: &str, price: &str, legs: &[(&str, &str, u32)]) -> OrderRequest {
        let legs: Vec<Value> = legs
            .iter()
            .map(|(instruction, symbol, quantity)| {
                json!({"instruction": instruction, "quantity": quantity,
                    "instrument": {"symbol": symbol, "assetType": "OPTION"}})
            })
            .collect();
        serde_json::from_value(json!({"orderStrategyType": "SINGLE", "orderType": kind,
            "session": "NORMAL", "duration": "DAY", "price": price,
            "orderLegCollection": legs}))
        .unwrap()
    }
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    // Two units of a 2.00 debit vertical, one filled: 200 still at risk.
    let working = json!({"orderId": 8, "status": "WORKING", "orderStrategyType": "SINGLE",
        "orderType": "NET_DEBIT", "session": "NORMAL", "duration": "DAY", "price": 2,
        "quantity": 2, "filledQuantity": 1, "remainingQuantity": 1,
        "orderLegCollection": [
            {"orderLegType": "OPTION", "instruction": "BUY_TO_OPEN", "quantity": 2,
             "instrument": {"assetType": "OPTION", "symbol": C100}},
            {"orderLegType": "OPTION", "instruction": "SELL_TO_OPEN", "quantity": 2,
             "instrument": {"assetType": "OPTION", "symbol": C105}}]});
    baseline(
        &journal,
        funded()["securitiesAccount"].clone(),
        vec![working],
    )
    .await;
    let verified = PortfolioLimits {
        option_multipliers: HashMap::from([
            (C100.to_owned(), 100.into()),
            (C105.to_owned(), 100.into()),
        ]),
        ..limits()
    };
    let debit = |units| {
        strategy(
            "NET_DEBIT",
            "2",
            &[("BUY_TO_OPEN", C100, units), ("SELL_TO_OPEN", C105, units)],
        )
    };

    // Unverified multipliers: the working spread cannot be bounded, so
    // nothing is admitted, not even an equity buy.
    let blind = risk(&journal, limits());
    assert!(matches!(
        blind
            .reserve("A", "blind", &order("BUY", "AAPL", 1, "10"))
            .await,
        Err(rustyschwab::Error::RiskDenied(_))
    ));

    let risk = risk(&journal, verified);
    // 200 working + 600 + 200 = the 1000 of funds; one more unit is 1200.
    risk.reserve("A", "three", &debit(3)).await.unwrap();
    risk.reserve("A", "one", &debit(1)).await.unwrap();
    assert!(matches!(
        risk.check("A", &debit(1), None).await,
        Err(rustyschwab::Error::RiskDenied(_))
    ));
    // A credit vertical risks its width less the credit: (5 - 4.9) × 100.
    let credit = strategy(
        "NET_CREDIT",
        "4.9",
        &[("SELL_TO_OPEN", C100, 1), ("BUY_TO_OPEN", C105, 1)],
    );
    assert!(
        risk.check("A", &credit, None).await.is_err(),
        "10 more is past the funds"
    );
    risk.release("one").await.unwrap();
    risk.check("A", &credit, None).await.unwrap();

    let naked = strategy(
        "NET_CREDIT",
        "1",
        &[("SELL_TO_OPEN", C100, 2), ("BUY_TO_OPEN", C105, 1)],
    );
    let refused = risk.check("A", &naked, None).await.unwrap_err();
    assert!(refused.to_string().contains("no bound"), "{refused}");
}
