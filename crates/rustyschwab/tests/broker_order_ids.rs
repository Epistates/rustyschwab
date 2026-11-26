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

/// The store sets such IDs aside itself, as an authority does for its
/// workers, and a pass whose only change is setting one aside is audited
/// as a change to the account like any other.
#[tokio::test]
async fn a_pass_that_only_sets_an_id_aside_is_audited() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let database = journal.database();
    let now = chrono::Utc::now().timestamp_millis();
    database
        .reconcile_snapshot("A", 1, now - 1000, json!({}), vec![])
        .await
        .unwrap();
    let state = rusqlite::Connection::open(database.path()).unwrap();
    state
        .execute(
            "INSERT INTO orders VALUES('A','abc',0,?1)",
            [json!({"orderId": "abc", "status": "WORKING"}).to_string()],
        )
        .unwrap();
    drop(state);
    let report = database
        .reconcile_snapshot("A", 1, now, json!({}), vec![])
        .await
        .unwrap();
    assert!(report.reconciled);
    assert_eq!(report.set_aside_order_ids, vec!["abc"]);
    assert!(
        journal
            .reconciliation_targets("A")
            .await
            .unwrap()
            .is_empty()
    );
    let audited: Vec<_> = journal
        .audit_page(0, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .filter(|kind| kind == "account_reconciled" || kind == "order_id_set_aside")
        .collect();
    assert_eq!(
        audited,
        vec![
            "account_reconciled",
            "order_id_set_aside",
            "account_reconciled"
        ]
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
