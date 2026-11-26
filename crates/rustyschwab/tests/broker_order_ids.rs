//! A broker order ID is a whole number that fits in a `u64`: Schwab's are,
//! and so are the paper venue's. As a JSON string it is that number's own
//! digits (no sign, space or leading zero); as a JSON number, that number.
//! No other ID enters the journal or reaches Schwab in a request. One a
//! state file already holds (an earlier release stored them) is rewritten
//! in its own digits, or set aside if it names no order, when the store
//! first opens the file, and the store refuses to open a file that holds
//! one after that.
mod common;

use common::mock_client as client;
use rustyschwab::{
    Error, SchwabClient, SchwabConfig,
    execution::{ExecutionStore, OrderJournal, SubmissionOutcome, is_broker_order_id},
    risk::{PortfolioRisk, RiskProvider},
    types::{
        Decimal,
        trading::{OrderInstruction, OrderRequest, OrderRequestLeg},
    },
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};
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

/// Wind the state file at `path` back to schema version 6, the last release
/// before order IDs were canonical, as that release left it: the canonical
/// ID migration changes no tables, so this is exactly such a file. Returns
/// a connection to write what that release could store.
fn as_left_by_an_earlier_release(path: &Path) -> rusqlite::Connection {
    let state = rusqlite::Connection::open(path).unwrap();
    state
        .execute_batch(
            "DELETE FROM schema_migrations WHERE version>6;
             UPDATE meta SET value='6' WHERE key='schema';",
        )
        .unwrap();
    state
}

/// Store an order under `order_id`, observed at `observed_at`, as an
/// earlier release could.
fn store_order(
    state: &rusqlite::Connection,
    account: &str,
    order_id: &str,
    observed_at: i64,
    payload: &Value,
) {
    state
        .execute(
            "INSERT INTO orders VALUES(?1,?2,?3,?4)",
            rusqlite::params![account, order_id, observed_at, payload.to_string()],
        )
        .unwrap();
}

/// Record submission `attempt` as accepted under order `order_id`, as an
/// earlier release could.
fn accept_under(state: &rusqlite::Connection, attempt: &str, order_id: &str) {
    let outcome = json!({"status": "accepted", "order_id": order_id}).to_string();
    state
        .execute(
            "UPDATE submissions SET outcome=?1 WHERE id=?2",
            rusqlite::params![outcome, attempt],
        )
        .unwrap();
}

/// Each row `query` returns, its one column as text.
fn rows(state: &rusqlite::Connection, query: &str) -> Vec<String> {
    let mut statement = state.prepare(query).unwrap();
    statement
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// The client again, on the same state file, as a process started after an
/// upgrade opens it.
fn reopened(client: &SchwabClient) -> SchwabClient {
    SchwabClient::new(SchwabConfig {
        oauth: client.auth_manager().config().clone(),
        client: client.client_config().clone(),
        streaming: None,
    })
    .unwrap()
}

/// A state file an earlier release wrote can hold orders stored under IDs
/// that are not whole numbers, and an accepted submission naming one. No
/// broker order has such an ID, so opening the file sets each aside rather
/// than leave every pass to fail on it: the order stored under it is
/// dropped and audited with what was stored, a submission that named it
/// goes back to being placed by its tag, and the account reconciles.
/// Nothing is fetched for them.
#[tokio::test]
async fn a_state_file_holding_ids_that_are_not_whole_numbers_reconciles() {
    let server = MockServer::start().await;
    let (earlier, _dir, _) = client(&server, false);
    let lost = earlier
        .order_journal()
        .begin("hash", "place_order", json!({"tag": "rs-lost"}))
        .await
        .unwrap();
    let state = as_left_by_an_earlier_release(earlier.order_journal().database().path());
    for id in NOT_ORDER_IDS {
        let working = json!({"orderId": id, "status": "WORKING"});
        store_order(&state, "hash", id, 0, &working);
    }
    accept_under(&state, &lost, "abc");
    drop(state);
    let client = reopened(&earlier);
    let journal = client.order_journal();
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
    assert!(
        client
            .reconcile_account("hash", 1)
            .await
            .unwrap()
            .reconciled
    );
    assert_eq!(set_aside().await, audited);
    server.verify().await;
}

/// A stored ID that is a whole number written another way (an earlier
/// release took `"007"`) names a real order, 7, so it is never set aside:
/// opening the file rewrites it in its own digits, audited with the text it
/// had, and the account's passes follow the order under its own ID. A
/// submission accepted under the old text is accepted under the order's
/// own ID.
#[tokio::test]
async fn a_stored_id_written_another_way_still_names_its_order() {
    let server = MockServer::start().await;
    let (earlier, _dir, _) = client(&server, false);
    let placed = earlier
        .order_journal()
        .begin("hash", "place_order", json!({"tag": "rs-seven"}))
        .await
        .unwrap();
    let state = as_left_by_an_earlier_release(earlier.order_journal().database().path());
    let working = json!({"orderId": "007", "status": "WORKING"});
    store_order(&state, "hash", "007", 0, &working);
    accept_under(&state, &placed, "007");
    drop(state);
    let client = reopened(&earlier);
    let journal = client.order_journal();
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

    // Opening the file rewrote it, before any pass.
    let repaired = audited().await;
    assert_eq!(
        repaired,
        vec![(
            "order_id_canonicalized".to_string(),
            "007".to_string(),
            json!({"account": "hash", "order_id": "7", "merged": false, "attempts": [placed]})
        )]
    );
    assert!(
        client
            .reconcile_account("hash", 1)
            .await
            .unwrap()
            .reconciled
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

/// A store that was never migrated (an authority still on an earlier
/// release, say) can return stored IDs written another way. The client
/// reads each as the order it names, once however many ways it is stored,
/// so a working order older than the order list's window is still read in
/// the same pass. A reference the broker has no order for is dropped under
/// the text it was stored as; an order the store did observe vanishing
/// still needs an operator.
#[tokio::test]
async fn stored_ids_written_another_way_are_read_as_the_orders_they_name() {
    let server = MockServer::start().await;
    let (client, _dir, _) = client(&server, false);
    let journal = client.order_journal();
    // Written into the open store, which returns them as such a store would:
    // an observed working order stored as "+8", and a reference to order 9
    // that the broker never answered for, stored as "09".
    let state = rusqlite::Connection::open(journal.database().path()).unwrap();
    store_order(
        &state,
        "hash",
        "+8",
        0,
        &json!({"orderId": "+8", "status": "WORKING"}),
    );
    store_order(
        &state,
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
    let fetched = json!({"orderId": 8, "status": "WORKING"});
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
        let targets = journal.reconciliation_targets("hash").await.unwrap();
        assert!(!targets.contains(&"09".to_string()), "{targets:?}");
    }
    drop(read_each_pass);
    drop(no_order_nine);

    // An observed working order the broker no longer has, stored as "010".
    order_eight().mount(&server).await;
    store_order(
        &state,
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
    let targets = journal.reconciliation_targets("hash").await.unwrap();
    assert!(targets.contains(&"010".to_string()), "{targets:?}");
}

/// When an earlier release's file holds one order under its own digits and
/// under another text, opening it merges the two into one record under the
/// order's own ID: the broker's latest view of the order is kept (on a tie,
/// the record under its own digits), each observed activity is kept once,
/// every submission accepted under either text is accepted under the
/// order's own ID, and a reservation for a replacement of it names that ID.
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
    let mut attempts = vec![];
    for _ in 0..2 {
        let attempt = journal.begin("A", "place_order", json!({})).await.unwrap();
        journal.finish(&attempt, accepted(Some("1"))).await.unwrap();
        attempts.push(attempt);
    }
    let path = journal.path().to_owned();
    let state = as_left_by_an_earlier_release(&path);
    state
        .execute_batch(
            "UPDATE orders SET order_id='007', observed_at=observed_at+1000, \
               payload=json_set(payload,'$.orderId','007') WHERE account='A' AND order_id='70';
             UPDATE executions SET execution_id='007'||substr(execution_id,3) \
               WHERE account='A' AND execution_id LIKE '70:%';
             INSERT INTO reservations(id,account,quantity,notional,state,symbol,side,replaces) \
               VALUES('moving','A','1','10','held','AAPL','BUY','007');",
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
    store_order(&state, "A", "8", observed_at, &working(json!(8), 0, vec![]));
    store_order(
        &state,
        "A",
        "08",
        observed_at,
        &working(json!("08"), 1, vec![]),
    );
    accept_under(&state, &attempts[0], "7");
    accept_under(&state, &attempts[1], "007");
    let activities = || {
        rows(
            &state,
            "SELECT substr(execution_id,1,instr(execution_id,':')-1) FROM executions \
             WHERE account='A' ORDER BY execution_id",
        )
    };
    assert_eq!(activities(), vec!["007", "007", "7"]);

    let upgraded = OrderJournal::new(path);
    let mut known = upgraded.database().known_orders("A", 10).await.unwrap();
    known.sort_by_key(|order| order["orderId"].to_string());
    assert_eq!(
        known,
        vec![
            working(json!("7"), 2, vec![fill(1), fill(2)]),
            working(json!(8), 0, vec![]),
        ]
    );
    assert_eq!(activities(), vec!["7", "7"]);
    for attempt in &attempts {
        assert!(matches!(
            upgraded.record(attempt).await.unwrap().unwrap().outcome,
            Some(SubmissionOutcome::Accepted { order_id: Some(order_id) }) if order_id == "7"
        ));
    }
    assert_eq!(
        rows(
            &state,
            "SELECT replaces FROM reservations WHERE id='moving'"
        ),
        vec!["7"]
    );
    let canonicalized: BTreeMap<String, Value> = upgraded
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

/// Opening an earlier release's state file rewrites every order ID it holds
/// outside the audit trail, once, in the transaction that records the new
/// schema version: finished orders and their observed activity as well as
/// working orders and references, and every link to them (accepted
/// submissions, reservations for a replacement). Records for the same
/// number merge, and an ID that names no order at all is set aside. Each
/// change is audited; the audit rows already written stay as they were and
/// the chain still verifies; and opening the file again changes nothing.
#[tokio::test]
async fn opening_an_earlier_state_file_canonicalizes_every_order_id() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let fill = |quantity: u32| json!({"activityType": "EXECUTION", "quantity": quantity});
    let order = |order_id: Value, status: &str, fills: Vec<Value>| json!({"orderId": order_id, "status": status, "orderActivityCollection": fills});
    // Recorded under scratch IDs so that their activities are keyed as this
    // release keys them, then moved under the text the earlier release
    // stored: "007", finished, the same order as the working "7" and
    // observed after it; "+8", finished, alone.
    for (order_id, status, fills) in [
        (7, "WORKING", vec![fill(1)]),
        (70, "FILLED", vec![fill(1), fill(2)]),
        (80, "CANCELED", vec![fill(3)]),
    ] {
        journal
            .record_order("A", order(json!(order_id), status, fills))
            .await
            .unwrap();
    }
    let mut attempts = BTreeMap::new();
    for order_id in ["7", "007", "+8", "abc"] {
        let attempt = journal.begin("A", "place_order", json!({})).await.unwrap();
        journal.finish(&attempt, accepted(Some("1"))).await.unwrap();
        attempts.insert(order_id, attempt);
    }
    let path = journal.path().to_owned();
    let state = as_left_by_an_earlier_release(&path);
    state
        .execute_batch(
            "UPDATE orders SET order_id='007', observed_at=observed_at+1000, \
               payload=json_set(payload,'$.orderId','007') WHERE order_id='70';
             UPDATE executions SET execution_id='007'||substr(execution_id,3) \
               WHERE execution_id LIKE '70:%';
             UPDATE orders SET order_id='+8', payload=json_set(payload,'$.orderId','+8') \
               WHERE order_id='80';
             UPDATE executions SET execution_id='+8'||substr(execution_id,3) \
               WHERE execution_id LIKE '80:%';
             INSERT INTO reservations(id,account,quantity,notional,state,symbol,side,replaces) \
               VALUES('replaces-007','A','1','10','held','AAPL','BUY','007'),\
                     ('replaces-abc','A','1','10','held','AAPL','BUY','abc');",
        )
        .unwrap();
    store_order(
        &state,
        "A",
        "09",
        0,
        &json!({"orderId": "09", "status": "UNKNOWN"}),
    );
    store_order(
        &state,
        "A",
        "abc",
        0,
        &order(json!("abc"), "FILLED", vec![]),
    );
    for (order_id, attempt) in &attempts {
        accept_under(&state, attempt, order_id);
    }
    let audit = || {
        rows(
            &state,
            "SELECT sequence||' '||kind||' '||subject||' '||payload||' '||hash FROM audit \
             ORDER BY sequence",
        )
    };
    let written_before = audit();

    // A process started after the upgrade opens the file.
    let upgraded = OrderJournal::new(path.clone());
    upgraded.records().await.unwrap();
    assert_eq!(
        rows(&state, "SELECT value FROM meta WHERE key='schema'"),
        vec!["7"]
    );
    // Every table that links to orders holds only their own IDs.
    assert_eq!(
        rows(&state, "SELECT order_id FROM orders ORDER BY order_id"),
        vec!["7", "8", "9"]
    );
    assert_eq!(
        rows(
            &state,
            "SELECT substr(execution_id,1,instr(execution_id,':')-1) FROM executions \
             ORDER BY execution_id"
        ),
        vec!["7", "7", "8"]
    );
    for (order_id, linked) in [
        ("7", Some("7")),
        ("007", Some("7")),
        ("+8", Some("8")),
        ("abc", None),
    ] {
        let outcome = upgraded
            .record(&attempts[order_id])
            .await
            .unwrap()
            .unwrap()
            .outcome;
        assert!(
            matches!(&outcome, Some(SubmissionOutcome::Accepted { order_id }) if order_id.as_deref() == linked),
            "{order_id}: {outcome:?}"
        );
    }
    assert_eq!(
        rows(
            &state,
            "SELECT id||' '||coalesce(replaces,'none') FROM reservations ORDER BY id"
        ),
        vec!["replaces-007 7", "replaces-abc none"]
    );
    // The finished "007" was observed after the working "7": its view of
    // the order stands, under the order's own ID.
    let mut known = upgraded.database().known_orders("A", 10).await.unwrap();
    known.sort_by_key(|order| order["orderId"].to_string());
    assert_eq!(
        known,
        vec![
            order(json!("7"), "FILLED", vec![fill(1), fill(2)]),
            order(json!("8"), "CANCELED", vec![fill(3)]),
            json!({"orderId": "9", "status": "UNKNOWN"}),
        ]
    );
    let repairs: BTreeMap<(String, String), Value> = upgraded
        .audit_page(0, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "order_id_canonicalized" || e.kind == "order_id_set_aside")
        .map(|e| ((e.kind, e.subject), e.payload))
        .collect();
    let canonicalized = |from: &str| ("order_id_canonicalized".to_string(), from.to_string());
    assert_eq!(
        repairs,
        BTreeMap::from([
            (
                canonicalized("007"),
                json!({"account": "A", "order_id": "7", "merged": true,
                    "attempts": [attempts["007"]]})
            ),
            (
                canonicalized("+8"),
                json!({"account": "A", "order_id": "8", "merged": false,
                    "attempts": [attempts["+8"]]})
            ),
            (
                canonicalized("09"),
                json!({"account": "A", "order_id": "9", "merged": false, "attempts": []})
            ),
            (
                ("order_id_set_aside".to_string(), "abc".to_string()),
                json!({"account": "A", "order": order(json!("abc"), "FILLED", vec![]),
                    "attempts": [attempts["abc"]]})
            ),
        ])
    );
    // The audit rows already written are as they were, and the chain still
    // verifies with the new entries after them.
    assert_eq!(audit()[..written_before.len()], written_before[..]);
    upgraded.verify_audit().await.unwrap();

    // Opening it again changes nothing.
    let everything = || {
        [
            audit(),
            rows(
                &state,
                "SELECT order_id||' '||payload FROM orders ORDER BY 1",
            ),
            rows(&state, "SELECT execution_id FROM executions ORDER BY 1"),
            rows(
                &state,
                "SELECT id||' '||coalesce(outcome,'') FROM submissions ORDER BY 1",
            ),
        ]
        .concat()
    };
    let migrated = everything();
    OrderJournal::new(path).records().await.unwrap();
    assert_eq!(everything(), migrated);
}

/// After its one-time migration, a state file can hold an order ID that is
/// not one only if it was edited by hand or damaged. Such a file does not
/// open: nothing is read or written through it, and the error names the
/// table, the row and the text it found.
#[tokio::test]
async fn a_state_file_that_breaks_the_order_id_invariant_does_not_open() {
    for (table, write) in [
        (
            "orders",
            "INSERT INTO orders VALUES('A','007',0,'{\"orderId\":\"007\",\"status\":\"WORKING\"}')",
        ),
        (
            "executions",
            "INSERT INTO executions VALUES('A','007:0a1b','{}')",
        ),
        (
            "submissions",
            "INSERT INTO submissions(id,account,operation,created_at,request,outcome) \
             VALUES('s1','A','place_order',1,'{}','{\"status\":\"accepted\",\"order_id\":\"007\"}')",
        ),
        (
            "reservations",
            "INSERT INTO reservations(id,account,quantity,notional,state,replaces) \
             VALUES('r1','A','1','10','held','007')",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let journal = OrderJournal::new(dir.path().join("orders.json"));
        journal.records().await.unwrap();
        rusqlite::Connection::open(journal.path())
            .unwrap()
            .execute(write, [])
            .unwrap();
        let refused = OrderJournal::new(journal.path().to_owned())
            .records()
            .await
            .unwrap_err();
        let message = refused.to_string();
        assert!(
            message.contains(table) && message.contains("\"007\""),
            "{table}: {message}"
        );
        assert!(
            matches!(
                &refused,
                Error::NonCanonicalOrderId { table: found, row, order_id }
                    if *found == table && !row.is_empty() && order_id == "007"
            ),
            "{refused:?}"
        );
        assert_eq!(refused.kind(), rustyschwab::ErrorKind::Storage);
    }
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
