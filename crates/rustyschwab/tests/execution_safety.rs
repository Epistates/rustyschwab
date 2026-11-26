//! Durable-state guarantees around order submission: intents that never
//! left resolve themselves, reservations follow the journal, tags the
//! broker holds stay spent, and risk judges sells and replacements by what
//! they can actually do.
use rustyschwab::{
    coordination::Coordinator,
    execution::{ExecutionStore, OrderJournal, SubmissionOutcome, SubmissionRecord},
    risk::{PortfolioLimits, PortfolioRisk, RiskProvider},
    types::trading::OrderRequest,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    time::Duration,
};

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
    PortfolioRisk::new(
        journal.database().clone(),
        HashMap::from([("A".into(), limits)]),
    )
}

fn funded() -> Value {
    json!({"securitiesAccount": {"currentBalances": {"availableFunds": "1000"}, "positions": []}})
}

/// Two calls on one underlying and one expiration, a vertical's legs.
const C100: &str = "XYZ   260116C00100000";
const C105: &str = "XYZ   260116C00105000";

/// An option strategy priced `kind` at `price`, with legs of
/// `(instruction, symbol, quantity)`.
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

/// Limits that know both legs' contract multipliers (100), so a vertical
/// on them can be bounded.
fn verified() -> PortfolioLimits {
    PortfolioLimits {
        option_multipliers: HashMap::from([
            (C100.to_owned(), 100.into()),
            (C105.to_owned(), 100.into()),
        ]),
        ..limits()
    }
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

/// A record written before intents were armed (an older authority's answer,
/// say) has no `armed`. Nothing shows its request stayed on the host, so it
/// reads as armed: it may have reached the broker.
#[test]
fn a_record_from_before_arming_reads_as_possibly_sent() {
    let record: SubmissionRecord = serde_json::from_value(json!({
        "attempt_id": "older", "account": "A", "operation": "place_order",
        "request": {"tag": "t1"}, "created_at": 1, "outcome": null, "dedup_key": null
    }))
    .unwrap();
    assert!(record.armed);
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

/// Under our tag, the broker's copy of our order resolves the intent:
/// Schwab adds fields to the order and to each leg, and writes decimals as
/// numbers, and none of that is a difference. An order under our tag that
/// differs from the request in anything the request said (the quantity,
/// the side) is not one we can claim, so the intent stays unresolved for an
/// operator.
#[tokio::test]
async fn only_an_order_that_fits_the_request_resolves_it() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let mut request = serde_json::to_value(order("BUY", "AAPL", 1, "10.00")).unwrap();
    request["tag"] = json!("rs-ours");
    let id = journal.begin("A", "place_order", request).await.unwrap();
    journal.arm(&id).await.unwrap();
    let listed = |instruction: &str, quantity: f64| {
        json!({
            "orderId": 42, "accountNumber": 12345678, "status": "WORKING",
            "enteredTime": "2026-11-25T15:00:00+0000", "tag": "rs-ours",
            "session": "NORMAL", "duration": "DAY", "orderType": "LIMIT",
            "complexOrderStrategyType": "NONE", "orderStrategyType": "SINGLE",
            "price": 10.0, "quantity": quantity, "filledQuantity": 0.0,
            "remainingQuantity": quantity, "cancelable": true, "editable": true,
            "orderLegCollection": [{"orderLegType": "EQUITY", "legId": 1,
                "instruction": instruction, "quantity": quantity,
                "positionEffect": "OPENING",
                "instrument": {"assetType": "EQUITY", "symbol": "AAPL",
                    "cusip": "037833100", "instrumentId": 1973757747}}]
        })
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

    for differs in [listed("BUY", 2.0), listed("SELL", 1.0)] {
        let report = reconcile(vec![differs.clone()]).await;
        assert_eq!(report.unresolved_attempts, vec![id.clone()], "{differs}");
    }
    let report = reconcile(vec![listed("BUY", 1.0)]).await;
    assert_eq!(report.resolved_attempts, vec![id.clone()]);
    assert!(matches!(
        journal.record(&id).await.unwrap().unwrap().outcome,
        Some(SubmissionOutcome::Accepted { order_id: Some(order_id) }) if order_id == "42"
    ));
}

/// What reconciliation last read can be shown without a request: the
/// accounts it holds a baseline for, in order, and each account's orders
/// as Schwab reported them, most recently observed first and at most
/// `limit` of them.
#[tokio::test]
async fn reconciliation_keeps_what_it_read_for_each_account() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let database = journal.database();
    assert!(database.known_accounts().await.unwrap().is_empty());
    let now = chrono::Utc::now().timestamp_millis();
    let filled = json!({"orderId": 1, "status": "FILLED", "tag": "first"});
    let working = json!({"orderId": 2, "status": "WORKING", "tag": "second"});
    for (account, started_at, orders) in [
        ("B", now - 1000, vec![filled.clone()]),
        ("B", now, vec![working.clone()]),
        ("A", now, vec![]),
    ] {
        database
            .reconcile_snapshot(account, 1, started_at, json!({}), orders)
            .await
            .unwrap();
    }
    assert_eq!(database.known_accounts().await.unwrap(), vec!["A", "B"]);
    assert_eq!(
        database.known_orders("B", 10).await.unwrap(),
        vec![working.clone(), filled]
    );
    assert_eq!(database.known_orders("B", 1).await.unwrap(), vec![working]);
    assert!(database.known_orders("A", 10).await.unwrap().is_empty());
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

/// An operator may release the risk held for a submission only once it is
/// resolved (while it is not, its order may be live), and only by name and
/// with evidence. The release frees the account's capacity, is audited
/// under the operator, and leaves nothing to release a second time.
#[tokio::test]
async fn an_operator_releases_a_reservation_only_for_a_resolved_submission() {
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
    let refused = |result: rustyschwab::Result<()>| {
        matches!(result, Err(rustyschwab::Error::InvalidParameter(_)))
    };
    let evidence = "order 42 filled and settled";

    assert!(refused(
        journal.release_reservation(&id, "alice", evidence).await
    ));
    journal
        .finish(
            &id,
            SubmissionOutcome::Accepted {
                order_id: Some("42".into()),
            },
        )
        .await
        .unwrap();
    assert!(refused(
        journal.release_reservation(&id, " ", evidence).await
    ));
    assert!(refused(journal.release_reservation(&id, "alice", "").await));
    assert!(refused(
        journal
            .release_reservation("no-such-attempt", "alice", evidence)
            .await
    ));
    assert!(risk.reserve("A", "next", &buy).await.is_err(), "funds held");

    journal
        .release_reservation(&id, "alice", evidence)
        .await
        .unwrap();
    risk.reserve("A", "next", &buy).await.unwrap();
    assert!(refused(
        journal.release_reservation(&id, "alice", evidence).await
    ));
    let released: Vec<_> = journal
        .audit_page(0, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "risk_released_by_operator")
        .map(|e| (e.subject, e.payload))
        .collect();
    assert_eq!(
        released,
        vec![(id, json!({"operator": "alice", "evidence": evidence}))]
    );
}

/// A refusal or an operator's resolution frees the reservation held for
/// the intent and audits the release as `risk_released`; an acceptance
/// keeps it, and an intent that holds none releases nothing and audits no
/// release.
#[tokio::test]
async fn a_resolution_audits_the_release_of_the_reservation_it_frees() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    baseline(&journal, funded()["securitiesAccount"].clone(), vec![]).await;
    let risk = risk(&journal, limits());
    let buy = order("BUY", "AAPL", 1, "10");
    let resolve = |reserved: bool, outcome: SubmissionOutcome| {
        let (journal, risk, buy) = (&journal, &risk, &buy);
        async move {
            let id = journal
                .begin("A", "place_order", serde_json::to_value(buy).unwrap())
                .await
                .unwrap();
            if reserved {
                risk.reserve("A", &id, buy).await.unwrap();
            }
            journal.finish(&id, outcome).await.unwrap();
            id
        }
    };
    let refused = resolve(true, SubmissionOutcome::Rejected).await;
    resolve(false, SubmissionOutcome::Rejected).await;
    let accepted = SubmissionOutcome::Accepted {
        order_id: Some("42".into()),
    };
    resolve(true, accepted).await;
    let evidence = "broker shows no such order".into();
    let settled = resolve(true, SubmissionOutcome::Reconciled { evidence }).await;
    let released: Vec<_> = journal
        .audit_page(0, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "risk_released")
        .map(|e| e.subject)
        .collect();
    assert_eq!(released, vec![refused, settled]);
}

/// A dispatcher that comes back late to an intent recovery already
/// resolved as never sent may still report it refused: the same answer is
/// not a conflict. Any other answer for it is.
#[tokio::test]
async fn a_late_refusal_of_an_intent_recovery_resolved_is_not_a_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let id = journal
        .begin("A", "place_order", json!({"tag": "t1"}))
        .await
        .unwrap();
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
    journal
        .finish(&id, SubmissionOutcome::Rejected)
        .await
        .unwrap();
    for (case, outcome) in [
        SubmissionOutcome::Accepted {
            order_id: Some("42".into()),
        },
        SubmissionOutcome::Accepted { order_id: None },
        SubmissionOutcome::Reconciled {
            evidence: "broker shows order 42".into(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        assert!(
            matches!(
                journal.finish(&id, outcome).await,
                Err(rustyschwab::Error::Duplicate(_))
            ),
            "case {case}"
        );
    }
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

/// A sell only reduces what the account holds, whatever it holds, so the
/// sell rule alone judges it: an ETF, labelled as Schwab lists the position
/// (`COLLECTIVE_INVESTMENT`) or as `EQUITY`, sells up to the position less
/// its other working sells, and no further. A buy of one is still refused,
/// so the provider can trap no one in a fund it would not let them buy.
#[tokio::test]
async fn a_held_etf_sells_down_to_nothing_and_no_further() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    baseline(
        &journal,
        json!({
            "currentBalances": {"availableFunds": 1000},
            "positions": [{"longQuantity": 10,
                "instrument": {"symbol": "TQQQ", "assetType": "COLLECTIVE_INVESTMENT"}}]
        }),
        vec![],
    )
    .await;
    let risk = risk(&journal, limits());
    let leg = |instruction: &str, asset: &str, quantity: u32| -> OrderRequest {
        let mut order = order(instruction, "TQQQ", quantity, "50");
        order.order_leg_collection[0].instrument.asset_type =
            serde_json::from_value(json!(asset)).unwrap();
        order
    };
    for asset in ["COLLECTIVE_INVESTMENT", "EQUITY"] {
        risk.check("A", &leg("SELL", asset, 10), None)
            .await
            .unwrap();
    }
    risk.reserve("A", "most", &leg("SELL", "COLLECTIVE_INVESTMENT", 6))
        .await
        .unwrap();
    risk.reserve("A", "rest", &leg("SELL", "EQUITY", 4))
        .await
        .unwrap();
    for asset in ["COLLECTIVE_INVESTMENT", "EQUITY"] {
        let oversold = risk.reserve("A", "more", &leg("SELL", asset, 1)).await;
        assert!(
            matches!(&oversold, Err(rustyschwab::Error::RiskDenied(r))
                if r == "Sell exceeds the position less the symbol's working sells"),
            "{asset}: {oversold:?}"
        );
    }
    let buy = risk
        .check("A", &leg("BUY", "COLLECTIVE_INVESTMENT", 1), None)
        .await;
    assert!(
        matches!(&buy, Err(rustyschwab::Error::RiskDenied(r))
            if r.starts_with("Built-in portfolio risk covers long equity only; ETFs")),
        "{buy:?}"
    );
}

/// Working buys of a symbol, ours or at the broker, add to the position
/// rather than offer any of it: the whole position may still be sold, and
/// only a working sell takes shares out of what may be.
#[tokio::test]
async fn working_buys_do_not_reduce_what_may_be_sold() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let working_buy = json!({"orderId": 7, "status": "WORKING", "orderStrategyType": "SINGLE",
        "orderType": "LIMIT", "quantity": 5, "filledQuantity": 0, "remainingQuantity": 5,
        "price": 150, "orderLegCollection": [{"instruction": "BUY", "quantity": 5,
            "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}}]});
    baseline(
        &journal,
        json!({
            "currentBalances": {"availableFunds": 1000},
            "positions": [{"longQuantity": 100, "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}}]
        }),
        vec![working_buy],
    )
    .await;
    let risk = risk(&journal, limits());
    risk.reserve("A", "buy", &order("BUY", "AAPL", 1, "150"))
        .await
        .unwrap();
    risk.reserve("A", "all", &order("SELL", "AAPL", 100, "150"))
        .await
        .unwrap();
    assert!(matches!(
        risk.check("A", &order("SELL", "AAPL", 1, "150"), None)
            .await,
        Err(rustyschwab::Error::RiskDenied(_))
    ));
}

/// One symbol's working orders do not count against another's limits: a
/// working MSFT buy is no part of the AAPL position the cap bounds, and a
/// working MSFT sell offers none of the AAPL shares.
#[tokio::test]
async fn another_symbols_working_orders_do_not_count_against_it() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let working = |order_id: u32, instruction: &str, quantity: u32| {
        json!({"orderId": order_id, "status": "WORKING", "orderStrategyType": "SINGLE",
            "orderType": "LIMIT", "quantity": quantity, "filledQuantity": 0,
            "remainingQuantity": quantity, "price": 1,
            "orderLegCollection": [{"instruction": instruction, "quantity": quantity,
                "instrument": {"assetType": "EQUITY", "symbol": "MSFT"}}]})
    };
    baseline(
        &journal,
        json!({
            "currentBalances": {"availableFunds": 1000},
            "positions": [
                {"longQuantity": 100, "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}},
                {"longQuantity": 100, "instrument": {"symbol": "MSFT", "assetType": "EQUITY"}}
            ]
        }),
        vec![working(7, "BUY", 50), working(8, "SELL", 100)],
    )
    .await;
    let risk = risk(
        &journal,
        PortfolioLimits {
            max_open_quantity: 1000.into(),
            ..limits()
        },
    );
    // Up to the 200-share cap on AAPL, whatever MSFT has working.
    risk.reserve("A", "buy", &order("BUY", "AAPL", 100, "1"))
        .await
        .unwrap();
    // All 100 AAPL shares held, while every MSFT share is offered.
    risk.reserve("A", "sell", &order("SELL", "AAPL", 100, "1"))
        .await
        .unwrap();
}

/// A risk policy that leaves out `max_snapshot_age_ms` trusts a reconciled
/// baseline for five seconds: an order is judged against a younger one,
/// and refused as stale against an older one.
#[tokio::test]
async fn a_policy_without_a_baseline_age_trusts_a_baseline_for_five_seconds() {
    let limits: PortfolioLimits = serde_json::from_value(json!({
        "max_open_quantity": "100", "max_open_notional": "10000",
        "max_position_quantity": "200"
    }))
    .unwrap();
    assert_eq!(limits.max_snapshot_age_ms, 5000);
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let reconciled_ago = |age_ms: i64| {
        let database = journal.database().clone();
        async move {
            database
                .reconcile_snapshot(
                    "A",
                    1,
                    chrono::Utc::now().timestamp_millis() - age_ms,
                    funded(),
                    vec![],
                )
                .await
                .unwrap();
        }
    };
    let risk = risk(&journal, limits);
    let buy = order("BUY", "AAPL", 1, "10");
    reconciled_ago(7_500).await;
    assert!(matches!(
        risk.reserve("A", "stale", &buy).await,
        Err(rustyschwab::Error::BaselineStale { .. })
    ));
    reconciled_ago(2_500).await;
    risk.reserve("A", "fresh", &buy).await.unwrap();
}

/// A policy whose caps are not positive, whose baseline age is outside 1
/// to 60000 milliseconds, or whose daily-loss cap is not positive is a
/// misconfiguration: any one of them refuses every order on the account
/// as `Config`, a sell of what it holds included.
#[tokio::test]
async fn a_policy_with_any_limit_out_of_range_refuses_every_order() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    baseline(
        &journal,
        json!({
            "currentBalances": {"availableFunds": 1000},
            "positions": [{"longQuantity": 100, "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}}]
        }),
        vec![],
    )
    .await;
    let sell = order("SELL", "AAPL", 1, "10");
    for (case, limits) in [
        PortfolioLimits {
            max_open_quantity: 0.into(),
            ..limits()
        },
        PortfolioLimits {
            max_open_quantity: (-1).into(),
            ..limits()
        },
        PortfolioLimits {
            max_open_notional: 0.into(),
            ..limits()
        },
        PortfolioLimits {
            max_position_quantity: 0.into(),
            ..limits()
        },
        PortfolioLimits {
            max_snapshot_age_ms: 0,
            ..limits()
        },
        PortfolioLimits {
            max_snapshot_age_ms: 60_001,
            ..limits()
        },
        PortfolioLimits {
            max_daily_loss: Some(0.into()),
            ..limits()
        },
        PortfolioLimits {
            max_daily_loss: Some((-1).into()),
            ..limits()
        },
    ]
    .into_iter()
    .enumerate()
    {
        assert!(
            matches!(
                risk(&journal, limits).reserve("A", "sell", &sell).await,
                Err(rustyschwab::Error::Config(_))
            ),
            "case {case}"
        );
    }
    // A minute is the longest a policy may trust a baseline.
    let longest = PortfolioLimits {
        max_snapshot_age_ms: 60_000,
        ..limits()
    };
    risk(&journal, longest)
        .reserve("A", "sell", &sell)
        .await
        .unwrap();
}

/// A baseline dated after now, as it is once the clock steps back after
/// the reconciliation that wrote it, is not trusted: an order against it
/// is refused as stale.
#[tokio::test]
async fn a_baseline_dated_after_now_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    baseline(&journal, funded()["securitiesAccount"].clone(), vec![]).await;
    let risk = risk(&journal, limits());
    let buy = order("BUY", "AAPL", 1, "10");
    risk.check("A", &buy, None).await.unwrap();
    // As if the clock had stepped back a minute since the pass.
    rusqlite::Connection::open(journal.path())
        .unwrap()
        .execute("UPDATE accounts SET observed_at=observed_at+60000", [])
        .unwrap();
    assert!(matches!(
        risk.reserve("A", "ahead", &buy).await,
        Err(rustyschwab::Error::BaselineStale { .. })
    ));
}

/// The daily-loss control halts once the day's loss exceeds its cap: a
/// loss of exactly the cap still trades, judged on reconciliation and on
/// reservation alike, and a cent more halts.
#[tokio::test]
async fn a_daily_loss_of_exactly_the_cap_still_trades() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let reconciled_at = |liquidation_value: &str| {
        let database = journal.database().clone();
        let snapshot = json!({
            "securitiesAccount": {
                "initialBalances": {"liquidationValue": "10000"},
                "currentBalances": {"liquidationValue": liquidation_value, "availableFunds": "1000"},
                "positions": []
            },
            "_dayNetTransfers": "0"
        });
        async move {
            database
                .reconcile_snapshot(
                    "A",
                    1,
                    chrono::Utc::now().timestamp_millis(),
                    snapshot,
                    vec![],
                )
                .await
                .unwrap();
        }
    };
    let risk = risk(
        &journal,
        PortfolioLimits {
            max_daily_loss: Some(500.into()),
            ..limits()
        },
    );
    let buy = order("BUY", "AAPL", 1, "10");

    reconciled_at("9500").await;
    risk.observe_baseline("A").await.unwrap();
    assert_eq!(journal.operator_halt().await.unwrap(), None);
    risk.reserve("A", "at-the-cap", &buy).await.unwrap();
    assert_eq!(journal.operator_halt().await.unwrap(), None);

    reconciled_at("9499.99").await;
    assert!(matches!(
        risk.reserve("A", "past-the-cap", &buy).await,
        Err(rustyschwab::Error::Halted(reason))
            if reason.starts_with("Daily loss 500.01 exceeds the 500 limit")
    ));
    journal.set_operator_halt(None, "operator").await.unwrap();
    risk.observe_baseline("A").await.unwrap();
    assert!(journal.operator_halt().await.unwrap().is_some());
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

/// A risk provider that judges nothing and keeps each reservation it is
/// asked for: `(account, attempt, order)`.
#[derive(Default)]
struct Reservations(std::sync::Mutex<Vec<(String, String, OrderRequest)>>);

#[async_trait::async_trait]
impl RiskProvider for Reservations {
    async fn reserve(
        &self,
        account: &str,
        attempt_id: &str,
        order: &OrderRequest,
    ) -> rustyschwab::Result<()> {
        let reserved = (account.to_owned(), attempt_id.to_owned(), order.clone());
        self.0.lock().unwrap().push(reserved);
        Ok(())
    }
    async fn release(&self, _: &str) -> rustyschwab::Result<()> {
        Ok(())
    }
}

/// A provider that cannot tell a replacement apart from the order it
/// replaces reserves it as a new order: the trait's default reserves it
/// through `reserve`, which counts both and so only over-reserves.
#[tokio::test]
async fn a_provider_reserves_a_replacement_as_a_new_order_by_default() {
    let provider = Reservations::default();
    let moved = order("SELL", "AAPL", 100, "135");
    provider
        .reserve_replacement("A", "moved", &moved, "9")
        .await
        .unwrap();
    assert_eq!(
        *provider.0.lock().unwrap(),
        vec![("A".to_owned(), "moved".to_owned(), moved)]
    );
}

/// Reading the day's cash transfers costs reconciliation a request per
/// transfer type, and only the daily-loss control needs them: the built-in
/// provider asks for them for an account whose policy has a daily-loss
/// cap, and for no other account, one without a policy included.
#[test]
fn only_an_account_with_a_daily_loss_cap_needs_the_days_transfers() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let capped = PortfolioLimits {
        max_daily_loss: Some(500.into()),
        ..limits()
    };
    let risk = PortfolioRisk::new(
        journal.database().clone(),
        HashMap::from([("capped".into(), capped), ("uncapped".into(), limits())]),
    );
    assert!(risk.needs_day_transfers("capped"));
    assert!(!risk.needs_day_transfers("uncapped"));
    assert!(!risk.needs_day_transfers("unknown"));
}

/// The built-in provider bounds an equity order by its limit price, so it
/// judges LIMIT and STOP_LIMIT orders only: a market or stop order has no
/// price to bound it, and a trailing stop limit's price moves with the
/// market. It judges long equity only: a short sale or a cover, an ETF
/// (judged as a stock, a leveraged or inverse fund's risk would be
/// understated), or another asset, needs a deployment's own provider. Each
/// is refused as `RiskDenied`, however small.
#[tokio::test]
async fn the_built_in_provider_judges_only_long_equity_limit_orders() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    baseline(&journal, funded()["securitiesAccount"].clone(), vec![]).await;
    let risk = risk(&journal, limits());
    let single = |kind: &str, instruction: &str, asset: &str, symbol: &str, prices: Value| {
        let mut order = json!({"orderStrategyType": "SINGLE", "orderType": kind,
            "session": "NORMAL", "duration": "DAY",
            "orderLegCollection": [{"quantity": 1, "instruction": instruction,
                "instrument": {"symbol": symbol, "assetType": asset}}]});
        order
            .as_object_mut()
            .unwrap()
            .extend(prices.as_object().unwrap().clone());
        serde_json::from_value::<OrderRequest>(order).unwrap()
    };
    let equity = |kind: &str, instruction: &str, prices: Value| {
        single(kind, instruction, "EQUITY", "AAPL", prices)
    };
    let trailing = json!({"stopPriceLinkBasis": "LAST", "stopPriceLinkType": "VALUE",
        "stopPriceOffset": "1"});
    let mut trailing_limit = trailing.clone();
    trailing_limit["price"] = json!("1");
    let limit = json!({"price": "10"});
    let fund = single("LIMIT", "BUY", "MUTUAL_FUND", "VFIAX", limit.clone());
    let etf = single(
        "LIMIT",
        "BUY",
        "COLLECTIVE_INVESTMENT",
        "SPY",
        limit.clone(),
    );
    let unpriced = "Built-in portfolio risk requires a limit order";
    let not_long_equity = "Built-in portfolio risk supports long equity exposure";
    let not_etfs = "Built-in portfolio risk covers long equity only; ETFs need a \
        deployment's own risk provider";
    for (case, (order, reason)) in [
        (equity("MARKET", "BUY", json!({})), unpriced),
        (equity("STOP", "BUY", json!({"stopPrice": "10"})), unpriced),
        (equity("TRAILING_STOP", "BUY", trailing), unpriced),
        (
            equity("TRAILING_STOP_LIMIT", "BUY", trailing_limit),
            unpriced,
        ),
        (
            equity("LIMIT", "SELL_SHORT", limit.clone()),
            not_long_equity,
        ),
        (
            equity("LIMIT", "SELL_SHORT_EXEMPT", limit.clone()),
            not_long_equity,
        ),
        (
            equity("LIMIT", "BUY_TO_COVER", limit.clone()),
            not_long_equity,
        ),
        (fund, not_long_equity),
        (etf, not_etfs),
    ]
    .into_iter()
    .enumerate()
    {
        let refused = risk.check("A", &order, None).await;
        assert!(
            matches!(&refused, Err(rustyschwab::Error::RiskDenied(r)) if r.starts_with(reason)),
            "case {case}: {refused:?}"
        );
    }
    risk.check("A", &equity("LIMIT", "BUY", limit), None)
        .await
        .unwrap();
    risk.check(
        "A",
        &equity(
            "STOP_LIMIT",
            "BUY",
            json!({"price": "10", "stopPrice": "9"}),
        ),
        None,
    )
    .await
    .unwrap();
}

/// The position cap bounds one symbol's shares once its working buys fill,
/// at the broker and here: a buy that would hold exactly the cap goes, and
/// one share more is refused. An option strategy holds contracts, not the
/// symbol's shares, so the cap does not bound it.
#[tokio::test]
async fn the_position_cap_bounds_shares_and_not_option_contracts() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let working_buy = json!({"orderId": 7, "status": "WORKING", "orderStrategyType": "SINGLE",
        "orderType": "LIMIT", "quantity": 30, "filledQuantity": 0, "remainingQuantity": 30,
        "price": 1, "orderLegCollection": [{"instruction": "BUY", "quantity": 30,
            "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}}]});
    baseline(
        &journal,
        json!({
            "currentBalances": {"availableFunds": 10000},
            "positions": [{"longQuantity": 150, "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}}]
        }),
        vec![working_buy],
    )
    .await;
    let capped = |max_position_quantity: u32| {
        risk(
            &journal,
            PortfolioLimits {
                max_open_quantity: 1000.into(),
                max_position_quantity: max_position_quantity.into(),
                ..verified()
            },
        )
    };
    let risk = capped(200);
    // 150 held, 30 working at the broker and 15 reserved here: 5 more is 200.
    risk.reserve("A", "ours", &order("BUY", "AAPL", 15, "1"))
        .await
        .unwrap();
    risk.check("A", &order("BUY", "AAPL", 5, "1"), None)
        .await
        .unwrap();
    assert!(matches!(
        risk.check("A", &order("BUY", "AAPL", 6, "1"), None).await,
        Err(rustyschwab::Error::RiskDenied(reason)) if reason.starts_with("Position quantity")
    ));
    // Two contracts of a vertical against a cap of one share.
    let vertical = strategy(
        "NET_DEBIT",
        "2",
        &[("BUY_TO_OPEN", C100, 1), ("SELL_TO_OPEN", C105, 1)],
    );
    capped(1).check("A", &vertical, None).await.unwrap();
}

/// Just before an order is sent, it is rechecked against the reservation
/// made for that attempt: one whose quantity or notional differs from what
/// was reserved is refused, however far within the limits, and so is an
/// attempt that reserved nothing.
#[tokio::test]
async fn dispatch_refuses_an_order_other_than_the_one_reserved() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    baseline(&journal, funded()["securitiesAccount"].clone(), vec![]).await;
    let risk = risk(&journal, limits());
    let reserved = order("BUY", "AAPL", 2, "10");
    risk.reserve("A", "held", &reserved).await.unwrap();
    for (case, (attempt, sent)) in [
        // The same quantity at another price.
        ("held", order("BUY", "AAPL", 2, "11")),
        // The same notional in another quantity.
        ("held", order("BUY", "AAPL", 4, "5")),
        ("held", order("BUY", "AAPL", 1, "10")),
        ("unreserved", reserved.clone()),
    ]
    .into_iter()
    .enumerate()
    {
        let refused = risk.revalidate("A", attempt, &sent).await;
        assert!(
            matches!(&refused, Err(rustyschwab::Error::Storage(reason))
                if reason == "Dispatch reservation missing or changed"),
            "case {case}: {refused:?}"
        );
    }
    risk.revalidate("A", "held", &reserved).await.unwrap();
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

/// The broker's view of one order, recorded outside a reconciliation pass
/// (a replacement reads the order it replaces), is kept as a pass keeps
/// it: among the account's known orders as Schwab reported it, a target
/// of the next pass while it works, and its tag spent. A payload with no
/// order ID, or with a number that is not a whole, non-negative one, is
/// refused and leaves nothing behind.
#[tokio::test]
async fn a_recorded_order_is_kept_as_reconciliation_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let working = json!({"orderId": 501, "status": "WORKING", "tag": "elsewhere"});
    journal.record_order("A", working.clone()).await.unwrap();
    assert_eq!(
        journal.database().known_orders("A", 10).await.unwrap(),
        vec![working]
    );
    assert_eq!(
        journal.reconciliation_targets("A").await.unwrap(),
        vec!["501"]
    );
    assert!(journal.tag_spent("A", "elsewhere").await.unwrap());

    for unidentified in [
        json!({"status": "WORKING", "tag": "unidentified"}),
        json!({"orderId": -502, "status": "WORKING", "tag": "unidentified"}),
        json!({"orderId": 502.5, "status": "WORKING", "tag": "unidentified"}),
    ] {
        assert!(
            matches!(
                journal.record_order("A", unidentified.clone()).await,
                Err(rustyschwab::Error::InvalidParameter(_))
            ),
            "{unidentified}"
        );
    }
    assert_eq!(
        journal
            .database()
            .known_orders("A", 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(!journal.tag_spent("A", "unidentified").await.unwrap());
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

/// An account that holds nothing may come back without `positions`. Its
/// first buy goes when the balances agree it holds nothing; when they show
/// a holding, or do not say, the missing positions still fail closed.
#[tokio::test]
async fn an_empty_account_without_positions_can_make_its_first_buy() {
    async fn reserve(account: Value) -> rustyschwab::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let journal = OrderJournal::new(dir.path().join("orders.json"));
        baseline(&journal, account, vec![]).await;
        let buy = order("BUY", "AAPL", 1, "10");
        let id = journal
            .begin("A", "place_order", serde_json::to_value(&buy).unwrap())
            .await
            .unwrap();
        risk(&journal, limits()).reserve("A", &id, &buy).await
    }
    reserve(
        json!({"currentBalances": {"availableFunds": 1000, "longMarketValue": 0,
        "shortMarketValue": 0, "moneyMarketFund": 0}}),
    )
    .await
    .unwrap();
    for account in [
        json!({"currentBalances": {"availableFunds": 1000, "longMarketValue": 250}}),
        json!({"currentBalances": {"availableFunds": 1000, "longMarketValue": 0,
            "mutualFundValue": 40}}),
        json!({"currentBalances": {"availableFunds": 1000}}),
    ] {
        let refused = reserve(account.clone()).await.unwrap_err();
        assert!(
            matches!(&refused, rustyschwab::Error::BaselineStale { reason, .. }
                if reason.contains("Positions missing")),
            "{account}: {refused:?}"
        );
    }
}

/// A clock that steps back does not refuse requests (cancels included) for
/// the length of the step: attempts dated after now count from now, so the
/// next request waits at most a window, and a burst that was full stays full.
#[tokio::test]
async fn a_clock_step_back_delays_requests_by_a_window_at_most() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let db = journal.database();
    assert_eq!(db.quota("credential", 2, 2).await.unwrap(), Duration::ZERO);
    // As if the clock had been ten minutes ahead when two attempts were made.
    let ahead = chrono::Utc::now().timestamp_millis() + 600_000;
    let connection = rusqlite::Connection::open(journal.path()).unwrap();
    connection
        .execute(
            "INSERT INTO quota VALUES('credential',?1),('credential',?1)",
            [ahead],
        )
        .unwrap();
    drop(connection);
    let wait = db.quota("credential", 2, 2).await.unwrap();
    assert!(
        wait > Duration::ZERO && wait <= Duration::from_secs(1),
        "the full burst is still counted, for one second from now: {wait:?}"
    );
    tokio::time::sleep(wait + Duration::from_millis(50)).await;
    assert_eq!(db.quota("credential", 2, 2).await.unwrap(), Duration::ZERO);
}

/// When a window holds more attempts than its cap, the wait lasts until
/// enough of them have left it, not only the oldest: after the clock steps
/// back, an attempt half a second old and two counted from now hold a burst
/// of two for a whole second.
#[tokio::test]
async fn the_wait_lasts_until_the_window_is_below_its_cap() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let db = journal.database();
    assert_eq!(db.quota("credential", 2, 2).await.unwrap(), Duration::ZERO);
    let now = chrono::Utc::now().timestamp_millis();
    let connection = rusqlite::Connection::open(journal.path()).unwrap();
    connection
        .execute_batch(&format!(
            "DELETE FROM quota;
             INSERT INTO quota VALUES('credential',{earlier}),('credential',{ahead}),('credential',{ahead});",
            earlier = now - 500,
            ahead = now + 600_000,
        ))
        .unwrap();
    drop(connection);
    assert_eq!(
        db.quota("credential", 2, 2).await.unwrap(),
        Duration::from_secs(1)
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

/// An approval names the operator who granted it, so it is honoured only
/// while that person still holds the operator's role: after a lapse, the
/// operator of the new grant. An intent never approved names nobody.
#[tokio::test]
async fn an_approval_names_the_operator_who_granted_it() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("audit.json"));
    let digest = "c".repeat(64);
    let later = || chrono::Utc::now().timestamp() + 600;
    assert_eq!(journal.approval_operator(&digest).await.unwrap(), None);
    journal
        .approve("desk", &digest, "alice", later())
        .await
        .unwrap();
    assert_eq!(
        journal.approval_operator(&digest).await.unwrap().as_deref(),
        Some("alice")
    );
    let lapse = digest.clone();
    journal
        .database()
        .run(move |c| {
            c.execute("UPDATE approvals SET expires_at=1 WHERE digest=?1", [lapse])
                .map_err(|e| rustyschwab::Error::Storage(e.to_string()))
        })
        .await
        .unwrap();
    journal
        .approve("desk", &digest, "bob", later())
        .await
        .unwrap();
    assert_eq!(
        journal.approval_operator(&digest).await.unwrap().as_deref(),
        Some("bob")
    );
}

/// An approval needs an operator who is not the principal, by name, for
/// the intent's SHA-256 digest: exactly 64 hexadecimal digits. It must
/// expire after now and at most an hour from now. Anything else is refused
/// and grants nothing.
#[tokio::test]
async fn an_approval_needs_another_operator_a_digest_and_an_expiry_within_the_hour() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("audit.json"));
    let digest = "d".repeat(64);
    let (short, long, not_hex) = ("d".repeat(63), "d".repeat(65), "g".repeat(64));
    let now = chrono::Utc::now().timestamp();
    for (case, (operator, digest, expires_at)) in [
        ("desk", &digest, now + 600),
        (" ", &digest, now + 600),
        ("operator", &short, now + 600),
        ("operator", &long, now + 600),
        ("operator", &not_hex, now + 600),
        ("operator", &digest, now - 1),
        ("operator", &digest, now),
        ("operator", &digest, now + 7200),
    ]
    .into_iter()
    .enumerate()
    {
        assert!(
            matches!(
                journal.approve("desk", digest, operator, expires_at).await,
                Err(rustyschwab::Error::InvalidParameter(_))
            ),
            "case {case}"
        );
    }
    assert_eq!(journal.approval_operator(&digest).await.unwrap(), None);
    // An hour from now is the longest an approval may last.
    journal
        .approve("desk", &digest, "operator", now + 3600)
        .await
        .unwrap();
    assert!(journal.is_approved("desk", &digest).await.unwrap());
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

/// An intent reads as lapsed once its approval expires unused, and as
/// revoked once an operator revokes it, whether it was approved, spent, or
/// never approved at all.
#[tokio::test]
async fn an_intent_reads_as_lapsed_or_revoked() {
    use rustyschwab::execution::IntentState;
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("audit.json"));
    let state = |digest: &str| {
        let (journal, digest) = (journal.clone(), digest.to_owned());
        async move {
            journal
                .intents(Some(&digest), 0, 1)
                .await
                .unwrap()
                .pop()
                .unwrap()
                .state
        }
    };
    let (lapsed, spent, unapproved) = ("e".repeat(64), "f".repeat(64), "0".repeat(64));
    for digest in [&lapsed, &spent, &unapproved] {
        journal
            .record_intent("desk", digest, "place_order", &json!({}))
            .await
            .unwrap();
    }
    let later = chrono::Utc::now().timestamp() + 600;
    for digest in [&lapsed, &spent] {
        journal
            .approve("desk", digest, "operator", later)
            .await
            .unwrap();
    }
    let lapse = lapsed.clone();
    journal
        .database()
        .run(move |c| {
            c.execute("UPDATE approvals SET expires_at=1 WHERE digest=?1", [lapse])
                .map_err(|e| rustyschwab::Error::Storage(e.to_string()))
        })
        .await
        .unwrap();
    assert_eq!(state(&lapsed).await, IntentState::Lapsed);
    journal
        .begin_once("desk", "place_order", json!({}), Some(spent.clone()))
        .await
        .unwrap();
    assert_eq!(state(&spent).await, IntentState::Spent);
    for digest in [&lapsed, &spent, &unapproved] {
        journal.revoke_approval(digest, "operator").await.unwrap();
        assert_eq!(state(digest).await, IntentState::Revoked, "{digest}");
    }
}

/// Setting or clearing the operator halt names who did it, and a halt
/// gives its reason: a blank operator or reason is refused and changes
/// nothing. Each change is audited under its operator.
#[tokio::test]
async fn the_operator_halt_is_set_and_cleared_by_name_and_with_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let refused = |result: rustyschwab::Result<()>| {
        matches!(result, Err(rustyschwab::Error::InvalidParameter(_)))
    };
    let review = || Some("review".to_owned());
    assert!(refused(journal.set_operator_halt(review(), " ").await));
    assert!(refused(
        journal.set_operator_halt(Some(" ".into()), "alice").await
    ));
    assert_eq!(journal.operator_halt().await.unwrap(), None);
    journal.set_operator_halt(review(), "alice").await.unwrap();
    assert!(refused(journal.set_operator_halt(None, "").await));
    assert_eq!(journal.operator_halt().await.unwrap(), review());
    journal.set_operator_halt(None, "bob").await.unwrap();
    assert_eq!(journal.operator_halt().await.unwrap(), None);
    let changes: Vec<_> = journal
        .audit_page(0, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "operator_halt_changed")
        .map(|e| (e.subject, e.payload))
        .collect();
    assert_eq!(
        changes,
        vec![
            ("alice".to_owned(), json!({"reason": "review"})),
            ("bob".to_owned(), json!({"reason": null})),
        ]
    );
}

/// An option strategy counts the most it can lose at expiration against the
/// account's funds and buy caps, a working one at the broker its unfilled
/// share; one with no bound, or with a leg whose multiplier the operator
/// has not verified, is refused.
#[tokio::test]
async fn option_strategies_reserve_their_defined_risk() {
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

    let risk = risk(&journal, verified());
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

/// `base` with each field of `change` set to its value, or removed where
/// the value is null.
fn changed(mut base: Value, change: Value) -> Value {
    for (field, value) in change.as_object().unwrap() {
        if value.is_null() {
            base.as_object_mut().unwrap().remove(field);
        } else {
            base[field] = value.clone();
        }
    }
    base
}

/// How the provider judges a sell of one of the 100 MSFT shares the
/// account holds, with `working` at the broker and `limits`: a sell draws
/// on no buy capacity, so only a working order it cannot bound stops it.
async fn sell_beside(working: Value, limits: PortfolioLimits) -> rustyschwab::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    baseline(
        &journal,
        json!({
            "currentBalances": {"availableFunds": 1000},
            "positions": [{"longQuantity": 100, "instrument": {"symbol": "MSFT", "assetType": "EQUITY"}}]
        }),
        vec![working],
    )
    .await;
    risk(&journal, limits)
        .check("A", &order("SELL", "MSFT", 1, "10"), None)
        .await
}

/// Whether `result` refuses an order because a working order at the
/// broker cannot be bounded.
fn unbounded(result: &rustyschwab::Result<()>) -> bool {
    matches!(result, Err(rustyschwab::Error::RiskDenied(reason))
        if reason == "External open order needs a supported, bounded risk model")
}

/// A working order at the broker counts what its unfilled remainder could
/// still buy or sell: nothing once it is filled in full. One the provider
/// cannot bound stops every order on the account, even a sell of another
/// symbol: one with no status, that is not a single order, that no limit
/// price bounds, with no legs or a leg that is not a long equity buy or
/// sell, without a positive price, for no shares (as a working strategy
/// for no units is), or whose figures do not hold together. Each figure is
/// checked on its own, so a negative quantity is refused even where it is
/// too small to change a sum of very large ones.
#[tokio::test]
async fn a_working_order_the_provider_cannot_bound_stops_every_order() {
    let working = |change: Value| {
        changed(
            json!({"orderId": 7, "status": "WORKING", "orderStrategyType": "SINGLE",
                "orderType": "LIMIT", "quantity": 5, "filledQuantity": 0, "remainingQuantity": 5,
                "price": 10, "orderLegCollection": [{"instruction": "BUY", "quantity": 5,
                    "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}}]}),
            change,
        )
    };
    let leg = |instruction: &str, asset: &str, symbol: &str| {
        json!([{"instruction": instruction, "quantity": 5,
            "instrument": {"assetType": asset, "symbol": symbol}}])
    };
    // Shares past what a decimal holds to the billionth, so that a billionth
    // more or less rounds away in a sum.
    let huge = "100000000000000000000";
    for (case, change) in [
        json!({"status": null}),
        json!({"orderStrategyType": "TRIGGER"}),
        json!({"orderType": "MARKET", "price": null}),
        json!({"orderType": "TRAILING_STOP_LIMIT"}),
        json!({"orderLegCollection": []}),
        json!({"orderLegCollection": leg("BUY", "MUTUAL_FUND", "VFIAX")}),
        json!({"orderLegCollection": leg("SELL_SHORT", "EQUITY", "AAPL")}),
        json!({"price": 0}),
        json!({"filledQuantity": null}),
        // For no shares, by the order's count or by its one leg's.
        json!({"quantity": 0, "remainingQuantity": 0}),
        json!({"quantity": null, "remainingQuantity": 0,
            "orderLegCollection": [{"instruction": "BUY", "quantity": 0,
                "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}}]}),
        // Filled past the order, or a negative fill or remainder.
        json!({"filledQuantity": 6, "remainingQuantity": -1}),
        json!({"filledQuantity": -1, "remainingQuantity": 6}),
        json!({"quantity": -5, "remainingQuantity": -5}),
        // A remainder past the order, or parts that miss the total.
        json!({"remainingQuantity": 6}),
        json!({"filledQuantity": 2, "remainingQuantity": 2}),
        json!({"quantity": huge, "filledQuantity": "-0.000000001", "remainingQuantity": huge}),
        json!({"quantity": huge, "filledQuantity": huge, "remainingQuantity": "-0.000000001"}),
    ]
    .into_iter()
    .enumerate()
    {
        let judged = sell_beside(working(change), limits()).await;
        assert!(unbounded(&judged), "case {case}: {judged:?}");
    }
    for (case, change) in [
        json!({}),
        json!({"orderType": "STOP_LIMIT", "stopPrice": 9}),
        json!({"filledQuantity": 2, "remainingQuantity": null}),
        json!({"filledQuantity": 5, "remainingQuantity": 0}),
    ]
    .into_iter()
    .enumerate()
    {
        let judged = sell_beside(working(change), limits()).await;
        assert!(judged.is_ok(), "case {case}: {judged:?}");
    }
}

/// A working option strategy at the broker counts the unfilled share of
/// its defined risk, from all of it while unfilled to none once filled in
/// full. One the provider cannot bound stops every order on the account:
/// one with no status, that is not a single order, for no units, or whose
/// unfilled count is below zero or above the order's.
#[tokio::test]
async fn a_working_strategy_counts_its_unfilled_share_or_stops_every_order() {
    let working = |change: Value| {
        changed(
            json!({"orderId": 8, "status": "WORKING", "orderStrategyType": "SINGLE",
                "orderType": "NET_DEBIT", "session": "NORMAL", "duration": "DAY", "price": 2,
                "quantity": 2, "filledQuantity": 1, "remainingQuantity": 1,
                "orderLegCollection": [
                    {"orderLegType": "OPTION", "instruction": "BUY_TO_OPEN", "quantity": 2,
                     "instrument": {"assetType": "OPTION", "symbol": C100}},
                    {"orderLegType": "OPTION", "instruction": "SELL_TO_OPEN", "quantity": 2,
                     "instrument": {"assetType": "OPTION", "symbol": C105}}]}),
            change,
        )
    };
    for (case, change) in [
        json!({"status": null}),
        json!({"orderStrategyType": "TRIGGER"}),
        json!({"filledQuantity": 3, "remainingQuantity": -1}),
        json!({"filledQuantity": 0, "remainingQuantity": 3}),
        json!({"quantity": 0, "filledQuantity": 0, "remainingQuantity": 0}),
    ]
    .into_iter()
    .enumerate()
    {
        let judged = sell_beside(working(change), verified()).await;
        assert!(unbounded(&judged), "case {case}: {judged:?}");
    }
    for (case, change) in [
        json!({}),
        json!({"filledQuantity": 0, "remainingQuantity": 2}),
        json!({"filledQuantity": 2, "remainingQuantity": 0}),
        json!({"remainingQuantity": null}),
    ]
    .into_iter()
    .enumerate()
    {
        let judged = sell_beside(working(change), verified()).await;
        assert!(judged.is_ok(), "case {case}: {judged:?}");
    }
}

/// A store that implements only what [`ExecutionStore`] requires, over a
/// journal, so that the trait's own defaults answer everything else.
struct RequiredOnly(OrderJournal);

#[async_trait::async_trait]
impl ExecutionStore for RequiredOnly {
    async fn begin_once(
        &self,
        account: &str,
        operation: &str,
        request: Value,
        dedup_key: Option<String>,
    ) -> rustyschwab::Result<String> {
        self.0
            .begin_once(account, operation, request, dedup_key)
            .await
    }
    async fn finish(&self, id: &str, outcome: SubmissionOutcome) -> rustyschwab::Result<()> {
        self.0.finish(id, outcome).await
    }
    async fn arm(&self, id: &str) -> rustyschwab::Result<()> {
        self.0.arm(id).await
    }
    async fn records(&self) -> rustyschwab::Result<Vec<SubmissionRecord>> {
        self.0.records().await
    }
    async fn forget_placeholder(&self, account: &str, order_id: &str) -> rustyschwab::Result<bool> {
        self.0.forget_placeholder(account, order_id).await
    }
    async fn tag_spent(&self, account: &str, tag: &str) -> rustyschwab::Result<bool> {
        self.0.tag_spent(account, tag).await
    }
    async fn invalidate_account(&self, account: &str, generation: u64) -> rustyschwab::Result<()> {
        self.0.invalidate_account(account, generation).await
    }
    async fn account_state(
        &self,
        account: &str,
    ) -> rustyschwab::Result<Option<rustyschwab::reconciliation::AccountState>> {
        self.0.account_state(account).await
    }
    async fn reconcile_snapshot(
        &self,
        account: &str,
        generation: u64,
        started_at: i64,
        snapshot: Value,
        orders: Vec<Value>,
    ) -> rustyschwab::Result<rustyschwab::reconciliation::ReconciliationReport> {
        self.0
            .reconcile_snapshot(account, generation, started_at, snapshot, orders)
            .await
    }
    async fn audit_page(
        &self,
        after: i64,
        limit: u32,
    ) -> rustyschwab::Result<Vec<rustyschwab::execution::AuditEvent>> {
        self.0.audit_page(after, limit).await
    }
    async fn verify_audit(
        &self,
    ) -> rustyschwab::Result<rustyschwab::audit_chain::AuditChainReport> {
        self.0.verify_audit().await
    }
    async fn healthcheck(&self) -> rustyschwab::Result<()> {
        self.0.healthcheck().await
    }
    async fn pending_count(&self) -> rustyschwab::Result<usize> {
        self.0.pending_count().await
    }
    async fn reconciliation_targets(&self, account: &str) -> rustyschwab::Result<Vec<String>> {
        self.0.reconciliation_targets(account).await
    }
    async fn track_order(&self, account: &str, order_id: &str) -> rustyschwab::Result<()> {
        self.0.track_order(account, order_id).await
    }
    async fn record_order(&self, account: &str, order: Value) -> rustyschwab::Result<()> {
        self.0.record_order(account, order).await
    }
    async fn operator_halt(&self) -> rustyschwab::Result<Option<String>> {
        self.0.operator_halt().await
    }
    async fn set_operator_halt(
        &self,
        reason: Option<String>,
        operator: &str,
    ) -> rustyschwab::Result<()> {
        self.0.set_operator_halt(reason, operator).await
    }
}

/// The attempt IDs of five submissions, one of each standing a store is
/// asked about.
struct Standings {
    /// Accepted under the broker's order ID: placed.
    placed: String,
    /// Accepted without the broker's order ID: reconciliation must still
    /// place it.
    unplaced: String,
    /// Refused under tag `t3`, a minute before `retried` took it again.
    refused: String,
    /// The retry of `refused`, unresolved.
    retried: String,
    /// On account B under tag `t3` as well, unresolved.
    neighbour: String,
}

/// Journal [`Standings`] through `store`, which writes to `journal`.
async fn standings(store: &dyn ExecutionStore, journal: &OrderJournal) -> Standings {
    let begin = |account: &'static str, tag: &'static str| async move {
        store
            .begin_once(account, "place_order", json!({"tag": tag}), None)
            .await
            .unwrap()
    };
    let accepted = |order_id: Option<&str>| SubmissionOutcome::Accepted {
        order_id: order_id.map(str::to_owned),
    };
    let placed = begin("A", "t1").await;
    store.finish(&placed, accepted(Some("42"))).await.unwrap();
    let unplaced = begin("A", "t2").await;
    store.finish(&unplaced, accepted(None)).await.unwrap();
    let refused = begin("A", "t3").await;
    store
        .finish(&refused, SubmissionOutcome::Rejected)
        .await
        .unwrap();
    age_submissions(journal, Duration::from_secs(60));
    Standings {
        placed,
        unplaced,
        refused,
        retried: begin("A", "t3").await,
        neighbour: begin("B", "t3").await,
    }
}

/// What reconciliation, operators and an audit sink ask of any store: every
/// submission; an account's intents still to place (unresolved, or
/// accepted without the broker's order ID); the latest submission on an
/// account under a tag; how many are unresolved; and the audit trail.
async fn assert_answers(store: &dyn ExecutionStore, s: &Standings) {
    let attempts = |records: Vec<SubmissionRecord>| {
        records
            .into_iter()
            .map(|r| r.attempt_id)
            .collect::<BTreeSet<_>>()
    };
    let every = BTreeSet::from([
        s.placed.clone(),
        s.unplaced.clone(),
        s.refused.clone(),
        s.retried.clone(),
        s.neighbour.clone(),
    ]);
    assert_eq!(attempts(store.records().await.unwrap()), every);
    assert_eq!(
        attempts(store.unresolved_records("A").await.unwrap()),
        BTreeSet::from([s.unplaced.clone(), s.retried.clone()])
    );
    assert_eq!(
        attempts(store.unresolved_records("B").await.unwrap()),
        BTreeSet::from([s.neighbour.clone()])
    );
    for (case, (account, tag, latest)) in [
        ("A", "t1", Some(&s.placed)),
        ("A", "t3", Some(&s.retried)),
        ("B", "t3", Some(&s.neighbour)),
        ("B", "t1", None),
        ("A", "t9", None),
    ]
    .into_iter()
    .enumerate()
    {
        let found = store.submission_by_tag(account, tag).await.unwrap();
        assert_eq!(found.map(|r| r.attempt_id).as_ref(), latest, "case {case}");
    }
    assert_eq!(store.pending_count().await.unwrap(), 2);
    let intents: BTreeSet<_> = store
        .audit_page(0, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "intent")
        .map(|e| e.subject)
        .collect();
    assert_eq!(intents, every);
}

/// The journal answers from its own tables, and counts what is pending per
/// account: only A's unresolved intent blocks A.
#[tokio::test]
async fn the_journal_answers_what_reconciliation_asks() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let standings = standings(&journal, &journal).await;
    assert_answers(&journal, &standings).await;
    assert_eq!(journal.pending_count_for(&["A".into()]).await.unwrap(), 1);
}

/// A store that implements only what the trait requires gets the same
/// answers from the trait's defaults, except that the default pending
/// count for some accounts counts every account's: it may over-report what
/// blocks them, never under-report it.
#[tokio::test]
async fn the_store_defaults_answer_as_the_journal_does() {
    let dir = tempfile::tempdir().unwrap();
    let store = RequiredOnly(OrderJournal::new(dir.path().join("orders.json")));
    let standings = standings(&store, &store.0).await;
    assert_answers(&store, &standings).await;
    assert_eq!(store.pending_count_for(&["A".into()]).await.unwrap(), 2);
}

/// An account's submissions from a time on (Unix milliseconds, inclusive),
/// oldest first: the window a daily cap or a dead man's switch counts.
/// Another account's submissions are not in it.
#[tokio::test]
async fn records_since_lists_an_accounts_submissions_from_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let earlier = journal.begin("A", "place_order", json!({})).await.unwrap();
    journal
        .finish(&earlier, SubmissionOutcome::Rejected)
        .await
        .unwrap();
    age_submissions(&journal, Duration::from_secs(60));
    let later = journal.begin("A", "place_order", json!({})).await.unwrap();
    journal.begin("B", "place_order", json!({})).await.unwrap();
    let at = journal.record(&later).await.unwrap().unwrap().created_at;
    let since = |from: i64| {
        let journal = journal.clone();
        async move {
            journal
                .records_since("A", from)
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.attempt_id)
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(since(0).await, vec![earlier, later.clone()]);
    assert_eq!(since(at).await, vec![later]);
    assert!(since(at + 1).await.is_empty());
}

/// The health check is a write. While another writer keeps the state file
/// for longer than a write waits for its turn, no submission could be
/// journaled, so the store is not healthy.
#[tokio::test(start_paused = true)]
async fn a_state_file_that_cannot_take_a_write_is_unhealthy() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    journal.healthcheck().await.unwrap();
    // Another process in the middle of a write.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(journal.database().path().with_extension("write.lock"))
        .unwrap();
    held.lock().unwrap();
    assert!(matches!(
        journal.healthcheck().await,
        Err(rustyschwab::Error::StateBusy(_))
    ));
    held.unlock().unwrap();
}
