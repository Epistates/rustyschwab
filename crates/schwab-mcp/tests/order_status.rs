//! `get_order_status` over a mocked Schwab: the reduced shape, and the
//! bounded wait reading until the order is terminal or the deadline.
mod common;

use common::{call, mocked_server};
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

fn order(status: &str, filled: f64) -> Value {
    let mut fills = Vec::new();
    if filled > 0.0 {
        fills.push(json!({"activityType": "EXECUTION", "executionType": "FILL",
            "quantity": filled, "executionLegs": [{"legId": 1, "quantity": filled,
            "price": 150.25, "time": "2025-01-15T14:32:00+0000"}]}));
    }
    json!({"orderId": 9, "status": status, "orderType": "LIMIT", "price": 150.5,
           "quantity": 10, "filledQuantity": filled, "remainingQuantity": 10.0 - filled,
           "cancelable": status == "WORKING", "editable": status == "WORKING",
           "enteredTime": "2025-01-15T14:31:22+0000",
           "orderLegCollection": [{"legId": 1, "orderLegType": "EQUITY", "instruction": "BUY",
               "quantity": 10, "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}}],
           "orderActivityCollection": fills})
}

#[tokio::test]
async fn status_is_reduced_and_waiting_stops_at_terminal_or_deadline() {
    let rest = MockServer::start().await;
    let (server, _dir) = mocked_server(&rest).await;
    let args = json!({"account_hash": "A", "order_id": "9"});

    // The first read sees a partial fill; the second, the whole order.
    Mock::given(path("/trader/v1/accounts/A/orders/9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(order("WORKING", 4.0)))
        .up_to_n_times(1)
        .mount(&rest)
        .await;
    Mock::given(path("/trader/v1/accounts/A/orders/9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(order("FILLED", 10.0)))
        .mount(&rest)
        .await;

    let partial = call(&server, "get_order_status", args.clone()).await;
    assert_eq!(partial["source"], "schwab_rest");
    let data = &partial["data"];
    assert_eq!(data["orderId"], "9");
    assert_eq!(data["status"], "WORKING");
    assert_eq!(data["terminal"], false);
    assert_eq!(data["cancelable"], true);
    assert_eq!(
        (data["filled"].as_f64(), data["remaining"].as_f64()),
        (Some(4.0), Some(6.0))
    );
    assert_eq!(data["avgFillPrice"], 150.25);
    assert_eq!(data["fills"], 1);
    assert_eq!(data["legs"][0]["symbol"], "AAPL");
    assert_eq!(data["legs"][0]["filled"], 4.0);
    assert_eq!(data["lastUpdate"], "2025-01-15T14:32:00+0000");
    assert_eq!(data["waited"], Value::Null);
    assert_eq!(rest.received_requests().await.unwrap().len(), 1);

    let filled = call(
        &server,
        "get_order_status",
        json!({"account_hash": "A", "order_id": "9", "wait_ms": 5000}),
    )
    .await;
    let data = &filled["data"];
    assert_eq!(data["status"], "FILLED");
    assert_eq!(data["terminal"], true);
    assert_eq!(data["remaining"], 0.0);
    assert_eq!(data["waited"]["reads"], 1, "terminal on the first read");
    assert_eq!(rest.received_requests().await.unwrap().len(), 2);

    // A working order is read at the deadline and reported as is.
    rest.reset().await;
    Mock::given(path("/trader/v1/accounts/A/orders/9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(order("WORKING", 0.0)))
        .mount(&rest)
        .await;
    let waited = call(
        &server,
        "get_order_status",
        json!({"account_hash": "A", "order_id": "9", "wait_ms": 300}),
    )
    .await;
    let data = &waited["data"];
    assert_eq!(data["terminal"], false);
    assert_eq!(data["avgFillPrice"], Value::Null);
    assert_eq!(data["waited"]["reads"], 2);
    assert!(data["waited"]["ms"].as_u64().unwrap() >= 300, "{data}");
    assert_eq!(rest.received_requests().await.unwrap().len(), 2);

    let too_long = call(
        &server,
        "get_order_status",
        json!({"account_hash": "A", "order_id": "9", "wait_ms": 30_001}),
    )
    .await;
    assert_eq!(too_long["error"]["code"], "invalid_arguments");
    assert_eq!(
        rest.received_requests().await.unwrap().len(),
        2,
        "refused before reading"
    );

    rest.reset().await;
    Mock::given(path("/trader/v1/accounts/A/orders/9"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&rest)
        .await;
    let missing = call(&server, "get_order_status", args).await;
    assert!(missing["error"]["code"].is_string(), "{missing}");
}
