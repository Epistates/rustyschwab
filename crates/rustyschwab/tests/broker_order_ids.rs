//! A broker order ID is a whole number that fits in a `u64`: Schwab's are,
//! and so are the paper venue's. As a JSON string it is that number's own
//! digits (no sign, space or leading zero); as a JSON number, that number.
//! No other ID enters the journal or reaches Schwab in a request, and one a
//! state file already holds (an earlier release stored them) is set aside
//! by the next reconciliation pass.
mod common;

use common::mock_client as client;
use rustyschwab::{
    Error,
    execution::{ExecutionStore, OrderJournal, SubmissionOutcome, is_broker_order_id},
    risk::{PortfolioRisk, RiskProvider},
    types::{
        Decimal,
        trading::{OrderInstruction, OrderRequest, OrderRequestLeg},
    },
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// IDs no broker issues, each of which an earlier release could store.
const NOT_ORDER_IDS: [&str; 3] = ["abc", "-5", ""];
/// One past `u64::MAX`: digits that no `u64` holds.
const PAST_U64: &str = "18446744073709551616";
/// Strings that are not how any `u64` is written: past the largest, with a
/// sign or a space a lenient parser skips, or with a leading zero.
const NOT_U64_DIGITS: [&str; 4] = [PAST_U64, "+5", " 5", "007"];

fn accepted(order_id: Option<&str>) -> SubmissionOutcome {
    SubmissionOutcome::Accepted {
        order_id: order_id.map(str::to_owned),
    }
}

fn limit_order() -> OrderRequest {
    OrderRequest::limit(
        OrderRequestLeg::equity(OrderInstruction::Buy, "AAPL", 1),
        Decimal::from(10),
    )
}

/// A broker order ID is a `u64` written in its own digits, up to and
/// including the largest, and nothing else.
#[test]
fn a_broker_order_id_is_a_u64_in_its_own_digits() {
    assert_eq!(PAST_U64, (u128::from(u64::MAX) + 1).to_string());
    let max = u64::MAX.to_string();
    for id in ["0", "7", "1004356247", max.as_str()] {
        assert!(is_broker_order_id(id), "{id:?}");
    }
    let thirty_digits = "9".repeat(30);
    for id in [
        "",
        "abc",
        "-5",
        "7.0",
        "7 ",
        "1e3",
        "٧",
        "7/8",
        "00",
        thirty_digits.as_str(),
    ]
    .into_iter()
    .chain(NOT_U64_DIGITS)
    {
        assert!(!is_broker_order_id(id), "{id:?}");
    }
}

/// An ID means the same whether written as a JSON string or a number, up
/// to the largest `u64` and no further: one past it is refused in both
/// forms, and so is any string that is not exactly a `u64`'s digits.
#[tokio::test]
async fn string_and_number_forms_share_the_u64_bound() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let refused =
        |result: rustyschwab::Result<()>| matches!(result, Err(Error::InvalidParameter(_)));
    let number = |digits: &str| serde_json::from_str::<Value>(digits).unwrap();
    let max = u64::MAX.to_string();
    for (account, order_id) in [("A", json!(max)), ("B", number(&max))] {
        journal
            .record_order(account, json!({"orderId": order_id, "status": "WORKING"}))
            .await
            .unwrap();
        assert_eq!(
            journal.reconciliation_targets(account).await.unwrap(),
            vec![max.clone()]
        );
    }
    for order_id in NOT_U64_DIGITS
        .into_iter()
        .map(|id| json!(id))
        .chain([number(PAST_U64)])
    {
        let order = json!({"orderId": order_id, "status": "WORKING"});
        assert!(
            refused(journal.record_order("C", order).await),
            "{order_id}"
        );
    }
    let attempt = journal
        .begin("C", "place_order", json!({"tag": "t1"}))
        .await
        .unwrap();
    for id in NOT_U64_DIGITS {
        assert!(refused(journal.track_order("C", id).await), "{id:?}");
        assert!(
            refused(journal.finish(&attempt, accepted(Some(id))).await),
            "{id:?}"
        );
    }
    assert!(
        journal
            .reconciliation_targets("C")
            .await
            .unwrap()
            .is_empty()
    );
}

/// The journal takes a broker order ID, whether the broker's view of an
/// order, a reference to one, or the order an accepted submission names,
/// only as a whole number. Anything else is refused and leaves nothing to
/// reconcile.
#[tokio::test]
async fn the_journal_takes_order_ids_only_as_whole_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let refused =
        |result: rustyschwab::Result<()>| matches!(result, Err(Error::InvalidParameter(_)));
    for id in [
        json!("abc"),
        json!("-5"),
        json!(""),
        json!(" 7"),
        json!("7.0"),
        json!(-5),
        json!(2.5),
    ] {
        let order = json!({"orderId": id, "status": "WORKING", "tag": "unidentified"});
        assert!(refused(journal.record_order("A", order).await), "{id}");
    }
    for id in NOT_ORDER_IDS.into_iter().chain([" 7", "٧"]) {
        assert!(refused(journal.track_order("A", id).await), "{id:?}");
    }
    let id = journal
        .begin("A", "place_order", json!({"tag": "t1"}))
        .await
        .unwrap();
    for order_id in NOT_ORDER_IDS {
        assert!(
            refused(journal.finish(&id, accepted(Some(order_id))).await),
            "{order_id:?}"
        );
    }
    assert!(
        journal
            .record(&id)
            .await
            .unwrap()
            .unwrap()
            .outcome
            .is_none()
    );
    assert!(
        journal
            .reconciliation_targets("A")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!journal.tag_spent("A", "unidentified").await.unwrap());

    journal
        .record_order("A", json!({"orderId": "501", "status": "WORKING"}))
        .await
        .unwrap();
    journal
        .record_order("A", json!({"orderId": 502, "status": "WORKING"}))
        .await
        .unwrap();
    journal.track_order("A", "503").await.unwrap();
    journal.finish(&id, accepted(Some("504"))).await.unwrap();
    let mut targets = journal.reconciliation_targets("A").await.unwrap();
    targets.sort();
    assert_eq!(targets, vec!["501", "502", "503", "504"]);
}

/// A pass keeps no order the broker lists under an ID that is not a whole
/// number: it is not stored, not a target, and does not spend its tag.
#[tokio::test]
async fn reconciliation_keeps_no_order_under_an_id_that_is_not_a_whole_number() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    journal
        .database()
        .reconcile_snapshot(
            "A",
            1,
            chrono::Utc::now().timestamp_millis(),
            json!({}),
            vec![
                json!({"orderId": "abc", "status": "WORKING", "tag": "unidentified"}),
                json!({"orderId": 9, "status": "WORKING", "tag": "listed"}),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        journal.reconciliation_targets("A").await.unwrap(),
        vec!["9"]
    );
    assert!(!journal.tag_spent("A", "unidentified").await.unwrap());
    assert!(journal.tag_spent("A", "listed").await.unwrap());
}

/// The client puts only a whole number in an order's path: a read, a
/// cancel or a replacement naming anything else is refused before any
/// request is made, and a refused replacement journals nothing.
#[tokio::test]
async fn the_client_sends_only_whole_number_order_ids() {
    let server = MockServer::start().await;
    let (client, _dir, _) = client(&server, false);
    for id in NOT_ORDER_IDS
        .into_iter()
        .chain(NOT_U64_DIGITS)
        .chain(["7.0"])
    {
        assert!(
            matches!(
                client.get_order("hash", id).await,
                Err(Error::InvalidParameter(_))
            ),
            "{id:?}"
        );
        assert!(
            matches!(
                client.cancel_order("hash", id).await,
                Err(Error::InvalidParameter(_))
            ),
            "{id:?}"
        );
        assert!(
            matches!(
                client.replace_order("hash", id, &limit_order()).await,
                Err(Error::InvalidParameter(_))
            ),
            "{id:?}"
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(client.order_journal().records().await.unwrap().is_empty());
}

/// An accepted order whose `Location` does not end in a whole number has no
/// order ID the journal can use: it is recorded as accepted without one, to
/// be placed by its tag, as an accepted order with no `Location` is.
#[tokio::test]
async fn a_location_without_an_order_id_leaves_the_order_to_its_tag() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/trader/v1/accounts/hash/orders"))
        .respond_with(ResponseTemplate::new(201).insert_header(
            "Location",
            "https://api.schwabapi.com/trader/v1/accounts/hash/orders/abc",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let (client, _dir, _) = client(&server, false);
    let placed = client.place_order("hash", &limit_order()).await.unwrap();
    assert_eq!(placed.order_id, None);
    let journal = client.order_journal();
    let record = journal.records().await.unwrap().pop().unwrap();
    assert!(matches!(
        record.outcome,
        Some(SubmissionOutcome::Accepted { order_id: None })
    ));
    assert_eq!(
        journal.unresolved_records("hash").await.unwrap()[0].attempt_id,
        record.attempt_id
    );
    server.verify().await;
}

/// A state file an earlier release wrote can hold orders stored under IDs
/// that are not whole numbers, and an accepted submission naming one. No
/// broker order has such an ID, so the next pass sets each aside instead of
/// failing on it: the order stored under it is dropped and audited with
/// what was stored, a submission that named it goes back to being placed by
/// its tag, and the account reconciles. Nothing is fetched for them.
#[tokio::test]
async fn a_state_file_holding_ids_that_are_not_whole_numbers_reconciles() {
    let server = MockServer::start().await;
    let (client, _dir, _) = client(&server, false);
    let journal = client.order_journal();
    let lost = journal
        .begin("hash", "place_order", json!({"tag": "rs-lost"}))
        .await
        .unwrap();
    // What the earlier release stored, written as it wrote it.
    let state = rusqlite::Connection::open(journal.database().path()).unwrap();
    for id in NOT_ORDER_IDS {
        let order = json!({"orderId": id, "status": "WORKING"}).to_string();
        state
            .execute(
                "INSERT INTO orders VALUES('hash',?1,0,?2)",
                rusqlite::params![id, order],
            )
            .unwrap();
    }
    let outcome = json!({"status": "accepted", "order_id": "abc"}).to_string();
    state
        .execute(
            "UPDATE submissions SET outcome=?1 WHERE id=?2",
            rusqlite::params![outcome, lost],
        )
        .unwrap();
    drop(state);
    // Schwab lists the order the submission placed, under its tag.
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/hash/orders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"orderId": 4242, "status": "WORKING", "tag": "rs-lost"}
        ])))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/hash"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "securitiesAccount": {"positions": [], "currentBalances": {"availableFunds": 1000}}
        })))
        .expect(2)
        .mount(&server)
        .await;

    let report = client.reconcile_account("hash", 1).await.unwrap();
    assert!(report.reconciled);
    assert_eq!(report.set_aside_order_ids, vec!["", "-5", "abc"]);
    assert_eq!(report.resolved_attempts, vec![lost.clone()]);
    assert!(matches!(
        journal.record(&lost).await.unwrap().unwrap().outcome,
        Some(SubmissionOutcome::Accepted { order_id: Some(order_id) }) if order_id == "4242"
    ));
    assert_eq!(
        journal.reconciliation_targets("hash").await.unwrap(),
        vec!["4242"]
    );
    let set_aside = || async {
        journal
            .audit_page(0, 1000)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "order_id_set_aside")
            .map(|e| (e.subject, e.payload))
            .collect::<BTreeMap<String, Value>>()
    };
    let audited = set_aside().await;
    assert_eq!(
        audited.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["", "-5", "abc"]
    );
    assert_eq!(
        audited["abc"],
        json!({"account": "hash", "order": {"orderId": "abc", "status": "WORKING"},
            "attempts": [lost]})
    );
    assert_eq!(
        audited["-5"],
        json!({"account": "hash", "order": {"orderId": "-5", "status": "WORKING"},
            "attempts": []})
    );

    // Nothing is left to set aside, and the account stays reconciled.
    let again = client.reconcile_account("hash", 1).await.unwrap();
    assert!(again.reconciled);
    assert!(again.set_aside_order_ids.is_empty());
    assert_eq!(set_aside().await, audited);
    server.verify().await;
}

/// Write an order into `journal`'s state file under `order_id`, as an
/// earlier release could, observed at `observed_at`.
fn store_order_as_written_before(
    journal: &OrderJournal,
    account: &str,
    order_id: &str,
    observed_at: i64,
    payload: &Value,
) {
    rusqlite::Connection::open(journal.database().path())
        .unwrap()
        .execute(
            "INSERT INTO orders VALUES(?1,?2,?3,?4)",
            rusqlite::params![account, order_id, observed_at, payload.to_string()],
        )
        .unwrap();
}

/// The store repairs stored IDs itself, as an authority does for its
/// workers, and a pass whose only change is setting one aside, or writing
/// one in its own digits, is audited as a change to the account like any
/// other.
#[tokio::test]
async fn a_pass_that_only_repairs_a_stored_id_is_audited() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let database = journal.database();
    let now = chrono::Utc::now().timestamp_millis();
    let pass = |started_at: i64| {
        let database = database.clone();
        async move {
            database
                .reconcile_snapshot("A", 1, started_at, json!({}), vec![])
                .await
                .unwrap()
        }
    };
    pass(now - 2000).await;
    for (order_id, started_at) in [("abc", now - 1000), ("007", now)] {
        let working = json!({"orderId": order_id, "status": "WORKING"});
        store_order_as_written_before(&journal, "A", order_id, 0, &working);
        assert!(pass(started_at).await.reconciled, "{order_id:?}");
    }
    assert_eq!(
        journal.reconciliation_targets("A").await.unwrap(),
        vec!["7"]
    );
    let audited: Vec<_> = journal
        .audit_page(0, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .filter(|kind| kind != "intent")
        .collect();
    assert_eq!(
        audited,
        vec![
            "account_reconciled",
            "order_id_set_aside",
            "account_reconciled",
            "order_id_canonicalized",
            "account_reconciled"
        ]
    );
}

/// A stored ID that is a whole number written another way (an earlier
/// release took `"007"`) names a real order, 7, so it is never set aside:
/// the next pass rewrites it in its own digits, audited with the text it
/// had, and follows the order under its own ID from then on. A submission
/// accepted under the old text is accepted under the order's own ID.
#[tokio::test]
async fn a_stored_id_written_another_way_still_names_its_order() {
    let server = MockServer::start().await;
    let (client, _dir, _) = client(&server, false);
    let journal = client.order_journal();
    let placed = journal
        .begin("hash", "place_order", json!({"tag": "rs-seven"}))
        .await
        .unwrap();
    let working = json!({"orderId": "007", "status": "WORKING"});
    store_order_as_written_before(journal, "hash", "007", 0, &working);
    let outcome = json!({"status": "accepted", "order_id": "007"}).to_string();
    rusqlite::Connection::open(journal.database().path())
        .unwrap()
        .execute(
            "UPDATE submissions SET outcome=?1 WHERE id=?2",
            rusqlite::params![outcome, placed],
        )
        .unwrap();
    // Schwab lists the order under its own ID.
    let listed = json!({"orderId": 7, "status": "WORKING", "tag": "rs-seven"});
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/hash/orders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([listed])))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/hash"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "securitiesAccount": {"positions": [], "currentBalances": {"availableFunds": 1000}}
        })))
        .expect(2)
        .mount(&server)
        .await;
    let audited = || async {
        journal
            .audit_page(0, 1000)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "order_id_canonicalized" || e.kind == "order_id_set_aside")
            .map(|e| (e.kind, e.subject, e.payload))
            .collect::<Vec<_>>()
    };

    let report = client.reconcile_account("hash", 1).await.unwrap();
    assert!(report.reconciled);
    assert_eq!(report.canonicalized_order_ids, vec!["007"]);
    assert!(report.set_aside_order_ids.is_empty());
    let repaired = audited().await;
    assert_eq!(
        repaired,
        vec![(
            "order_id_canonicalized".to_string(),
            "007".to_string(),
            json!({"account": "hash", "order_id": "7", "merged": false, "attempts": [placed]})
        )]
    );
    assert_eq!(
        journal.reconciliation_targets("hash").await.unwrap(),
        vec!["7"]
    );
    assert_eq!(
        journal.database().known_orders("hash", 10).await.unwrap(),
        vec![listed]
    );
    assert!(matches!(
        journal.record(&placed).await.unwrap().unwrap().outcome,
        Some(SubmissionOutcome::Accepted { order_id: Some(order_id) }) if order_id == "7"
    ));

    // Nothing is left to repair, and the account stays reconciled.
    assert!(
        client
            .reconcile_account("hash", 1)
            .await
            .unwrap()
            .reconciled
    );
    assert_eq!(audited().await, repaired);
    server.verify().await;
}

/// A stored ID written another way is fetched as the order it names, so a
/// working order older than the order list's window is still read in the
/// same pass. A reference the broker has no order for is dropped under the
/// text it was stored as; an order the store did observe vanishing still
/// needs an operator.
#[tokio::test]
async fn stored_ids_written_another_way_are_read_as_the_orders_they_name() {
    let server = MockServer::start().await;
    let (client, _dir, _) = client(&server, false);
    let journal = client.order_journal();
    // An observed working order stored as "+8", and a reference to order 9
    // that the broker never answered for, stored as "09".
    store_order_as_written_before(
        journal,
        "hash",
        "+8",
        0,
        &json!({"orderId": "+8", "status": "WORKING"}),
    );
    store_order_as_written_before(
        journal,
        "hash",
        "09",
        0,
        &json!({"orderId": "09", "status": "UNKNOWN"}),
    );
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/hash/orders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/hash"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "securitiesAccount": {"positions": [], "currentBalances": {"availableFunds": 1000}}
        })))
        .mount(&server)
        .await;
    let fetched =
        json!({"orderId": 8, "status": "WORKING", "enteredTime": "2026-01-02T15:00:00+0000"});
    let order_eight = || {
        Mock::given(method("GET"))
            .and(path("/trader/v1/accounts/hash/orders/8"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fetched.clone()))
    };
    let read_each_pass = order_eight().expect(2).mount_as_scoped(&server).await;
    let no_order_nine = Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/hash/orders/9"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    for _ in 0..2 {
        assert!(
            client
                .reconcile_account("hash", 1)
                .await
                .unwrap()
                .reconciled
        );
        assert_eq!(
            journal.reconciliation_targets("hash").await.unwrap(),
            vec!["8"]
        );
        assert_eq!(
            journal.database().known_orders("hash", 10).await.unwrap(),
            vec![fetched.clone()]
        );
    }
    drop(read_each_pass);
    drop(no_order_nine);

    // An observed working order the broker no longer has, stored as "010".
    order_eight().mount(&server).await;
    store_order_as_written_before(
        journal,
        "hash",
        "010",
        0,
        &json!({"orderId": "010", "status": "WORKING", "orderType": "LIMIT"}),
    );
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/hash/orders/10"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    assert!(client.reconcile_account("hash", 1).await.is_err());
    let mut targets = journal.reconciliation_targets("hash").await.unwrap();
    targets.sort();
    assert_eq!(targets, vec!["010", "8"]);
}

/// A stored ID written another way may name an order also stored under its
/// own digits. The two records merge into one under the order's own ID:
/// the broker's latest view of the order is kept (on a tie, the record
/// under its own digits), each observed activity is kept once, every
/// submission accepted under either text is accepted under the order's own
/// ID, and a reservation for a replacement of it names that ID.
#[tokio::test]
async fn a_stored_id_written_another_way_merges_with_its_order() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let fill = |quantity: u32| {
        json!({"activityType": "EXECUTION", "executionType": "FILL", "quantity": quantity,
            "executionLegs": [{"legId": 1, "quantity": quantity, "price": 10}]})
    };
    let working = |order_id: Value, filled: u32, fills: Vec<Value>| {
        json!({"orderId": order_id, "status": "WORKING", "quantity": 3,
            "filledQuantity": filled, "orderActivityCollection": fills})
    };
    // Order 7 under its own digits with its first fill, and the same order
    // as an earlier release stored it, "007", observed later with both
    // fills. Its activities are keyed as that release keyed them: recorded
    // under a scratch ID, then moved under "007".
    journal
        .record_order("A", working(json!(7), 1, vec![fill(1)]))
        .await
        .unwrap();
    journal
        .record_order("A", working(json!(70), 2, vec![fill(1), fill(2)]))
        .await
        .unwrap();
    let state = rusqlite::Connection::open(journal.database().path()).unwrap();
    state
        .execute_batch(
            "UPDATE orders SET order_id='007', observed_at=observed_at+1000, \
               payload=json_set(payload,'$.orderId','007') WHERE account='A' AND order_id='70';
             UPDATE executions SET execution_id='007'||substr(execution_id,3) \
               WHERE account='A' AND execution_id LIKE '70:%';",
        )
        .unwrap();
    // Order 8 under both texts, observed at the same moment.
    let observed_at: i64 = state
        .query_row(
            "SELECT observed_at FROM orders WHERE account='A' AND order_id='7'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    store_order_as_written_before(
        &journal,
        "A",
        "8",
        observed_at,
        &working(json!(8), 0, vec![]),
    );
    store_order_as_written_before(
        &journal,
        "A",
        "08",
        observed_at,
        &working(json!("08"), 1, vec![]),
    );
    // Submissions accepted under each text, and a replacement of "007".
    let mut attempts = vec![];
    for order_id in ["7", "007"] {
        let attempt = journal.begin("A", "place_order", json!({})).await.unwrap();
        state
            .execute(
                "UPDATE submissions SET outcome=?1 WHERE id=?2",
                rusqlite::params![
                    json!({"status": "accepted", "order_id": order_id}).to_string(),
                    attempt
                ],
            )
            .unwrap();
        attempts.push(attempt);
    }
    state
        .execute(
            "INSERT INTO reservations(id,account,quantity,notional,state,symbol,side,replaces) \
             VALUES('moving','A','1','10','held','AAPL','BUY','007')",
            [],
        )
        .unwrap();
    let activities = |state: &rusqlite::Connection| {
        let mut stmt = state
            .prepare("SELECT execution_id FROM executions WHERE account='A' ORDER BY execution_id")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|id| id.unwrap().split(':').next().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(activities(&state), vec!["007", "007", "7"]);

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
    assert_eq!(report.canonicalized_order_ids, vec!["007", "08"]);
    assert!(report.set_aside_order_ids.is_empty());
    let mut known = journal.database().known_orders("A", 10).await.unwrap();
    known.sort_by_key(|order| order["orderId"].to_string());
    assert_eq!(
        known,
        vec![
            working(json!("7"), 2, vec![fill(1), fill(2)]),
            working(json!(8), 0, vec![]),
        ]
    );
    assert_eq!(activities(&state), vec!["7", "7"]);
    for attempt in &attempts {
        assert!(matches!(
            journal.record(attempt).await.unwrap().unwrap().outcome,
            Some(SubmissionOutcome::Accepted { order_id: Some(order_id) }) if order_id == "7"
        ));
    }
    let replaces: String = state
        .query_row(
            "SELECT replaces FROM reservations WHERE id='moving'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(replaces, "7");
    let canonicalized: BTreeMap<String, Value> = journal
        .audit_page(0, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "order_id_canonicalized")
        .map(|e| (e.subject, e.payload))
        .collect();
    assert_eq!(
        canonicalized,
        BTreeMap::from([
            (
                "007".to_string(),
                json!({"account": "A", "order_id": "7", "merged": true, "attempts": [attempts[1]]})
            ),
            (
                "08".to_string(),
                json!({"account": "A", "order_id": "8", "merged": true, "attempts": []})
            ),
        ])
    );
}

/// A replacement names the order it replaces by its broker order ID: risk
/// neither reserves for nor judges one naming anything else, and goes on to
/// judge one that names a whole number (here, refused for want of a policy).
#[tokio::test]
async fn a_replacement_names_the_order_it_replaces_by_a_whole_number() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let risk = PortfolioRisk {
        database: journal.database().clone(),
        accounts: HashMap::new(),
    };
    for id in NOT_ORDER_IDS {
        assert!(
            matches!(
                risk.reserve_replacement("A", "r", &limit_order(), id).await,
                Err(Error::InvalidParameter(_))
            ),
            "{id:?}"
        );
        assert!(
            matches!(
                risk.check("A", &limit_order(), Some(id)).await,
                Err(Error::InvalidParameter(_))
            ),
            "{id:?}"
        );
    }
    assert!(matches!(
        risk.reserve_replacement("A", "r", &limit_order(), "77")
            .await,
        Err(Error::RiskDenied(_))
    ));
}
