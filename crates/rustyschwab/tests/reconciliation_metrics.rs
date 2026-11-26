//! What reconciliation counts for an operator. Only our tag resolves a
//! submission; a broker order that fits the request under another tag, or
//! none, might be this intent or an identical manual one, so it is counted
//! as evidence for an operator to reconcile.
//!
//! The test installs the process's global metrics recorder (reconciliation
//! writes from a blocking thread, which a thread-local recorder would not
//! see), so it is the only test in this binary.
use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};
use rustyschwab::{execution::OrderJournal, metrics::UNTAGGED_MATCHES_TOTAL};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Counts increments of the untagged-match counter and ignores the rest.
#[derive(Debug)]
struct UntaggedMatches(Arc<AtomicU64>);
impl Recorder for UntaggedMatches {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        if key.name() == UNTAGGED_MATCHES_TOTAL {
            Counter::from_arc(self.0.clone())
        } else {
            Counter::noop()
        }
    }
    fn register_gauge(&self, _: &Key, _: &Metadata<'_>) -> Gauge {
        Gauge::noop()
    }
    fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
        Histogram::noop()
    }
}

/// An unresolved submission is counted once per pass when the broker lists
/// an order that fits its request under another tag or none. Nothing that
/// fits is nothing to count, and orders under our tag that leave the
/// submission ambiguous are the evidence themselves: an untagged twin
/// beside them is not counted.
#[tokio::test]
async fn an_order_that_fits_without_our_tag_is_counted_for_an_operator() {
    let untagged = Arc::new(AtomicU64::new(0));
    metrics::set_global_recorder(UntaggedMatches(untagged.clone())).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    let request = json!({
        "orderStrategyType": "SINGLE", "orderType": "LIMIT", "session": "NORMAL",
        "duration": "DAY", "price": "10", "tag": "rs-ours",
        "orderLegCollection": [{"quantity": 1, "instruction": "BUY",
            "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}}]
    });
    let id = journal
        .begin("A", "place_order", request.clone())
        .await
        .unwrap();
    journal.arm(&id).await.unwrap();
    let listed = |tag: Option<&str>, order_id: u64| {
        let mut order = request.clone();
        match tag {
            Some(tag) => order["tag"] = json!(tag),
            None => {
                order.as_object_mut().unwrap().remove("tag");
            }
        }
        order["orderId"] = json!(order_id);
        order["status"] = json!("WORKING");
        order
    };
    let counted = |orders: Vec<Value>| {
        let database = journal.database().clone();
        let untagged = untagged.clone();
        let id = id.clone();
        async move {
            let before = untagged.load(Ordering::SeqCst);
            let report = database
                .reconcile_snapshot(
                    "A",
                    1,
                    chrono::Utc::now().timestamp_millis(),
                    json!({}),
                    orders,
                )
                .await
                .unwrap();
            assert_eq!(report.unresolved_attempts, vec![id]);
            untagged.load(Ordering::SeqCst) - before
        }
    };

    assert_eq!(
        counted(vec![listed(Some("API_TOS:TraderAPI"), 42)]).await,
        1
    );
    assert_eq!(counted(vec![listed(None, 42)]).await, 1);
    assert_eq!(counted(vec![]).await, 0);
    assert_eq!(
        counted(vec![
            listed(Some("rs-ours"), 42),
            listed(Some("rs-ours"), 43),
            listed(None, 44),
        ])
        .await,
        0
    );
}
