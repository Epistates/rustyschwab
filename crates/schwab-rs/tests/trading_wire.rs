//! Order and transaction listings as they leave the client: Schwab's
//! timestamp spelling, comma-joined type lists, omitted options, and the
//! guards that stop bad requests before any bytes are sent.
mod common;

use chrono::{Duration, TimeZone, Utc};
use common::mock_client;
use schwab_rs::Error;
use schwab_rs::types::trading::{OrderStatus, OrdersParams, TransactionType, TransactionsParams};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// The one request the server saw, or a panic naming how many it got.
async fn only_request(server: &MockServer) -> Request {
    let mut requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "expected exactly one request: {requests:?}"
    );
    requests.pop().unwrap()
}

fn query_pairs(request: &Request) -> Vec<(String, String)> {
    request
        .url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

#[tokio::test]
async fn account_orders_send_schwab_timestamps_and_typed_status() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/HASH/orders"))
        .and(query_param("fromEnteredTime", "2025-01-01T00:00:00.000Z"))
        .and(query_param("toEnteredTime", "2025-01-15T16:00:00.250Z"))
        .and(query_param("maxResults", "50"))
        .and(query_param("status", "FILLED"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "orderId": 1001, "status": "FILLED", "orderLegCollection": []
        }])))
        .expect(1)
        .mount(&server)
        .await;

    let from = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
    let to = Utc.with_ymd_and_hms(2025, 1, 15, 16, 0, 0).unwrap() + Duration::milliseconds(250);
    let orders = client
        .get_account_orders(
            "HASH",
            &OrdersParams {
                max_results: Some(50),
                status: Some(OrderStatus::Filled),
                ..OrdersParams::new(from, to)
            },
        )
        .await
        .unwrap();
    assert_eq!(orders[0].order_id.as_deref(), Some("1001"));
    assert_eq!(orders[0].status, Some(OrderStatus::Filled));
}

#[tokio::test]
async fn all_orders_omit_unset_options() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    Mock::given(method("GET"))
        .and(path("/trader/v1/orders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;

    let from = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
    let to = from + Duration::days(30);
    client
        .get_all_orders(&OrdersParams::new(from, to))
        .await
        .unwrap();

    let pairs = query_pairs(&only_request(&server).await);
    let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["fromEnteredTime", "toEnteredTime"], "{pairs:?}");
}

#[tokio::test]
async fn transactions_join_types_with_commas() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    Mock::given(method("GET"))
        .and(path("/trader/v1/accounts/HASH/transactions"))
        .and(query_param("startDate", "2025-01-01T00:00:00.000Z"))
        .and(query_param("endDate", "2025-01-31T00:00:00.000Z"))
        .and(query_param("types", "TRADE,DIVIDEND_OR_INTEREST"))
        .and(query_param("symbol", "AAPL"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "activityId": 1, "type": "TRADE", "transferItems": []
        }])))
        .expect(1)
        .mount(&server)
        .await;

    let start = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
    let transactions = client
        .get_transactions(
            "HASH",
            &TransactionsParams {
                symbol: Some("AAPL".into()),
                ..TransactionsParams::new(
                    start,
                    start + Duration::days(30),
                    [TransactionType::Trade, TransactionType::DividendOrInterest],
                )
            },
        )
        .await
        .unwrap();
    assert_eq!(transactions[0].type_, Some(TransactionType::Trade));
}

#[tokio::test]
async fn invalid_listing_params_never_reach_the_server() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    let earlier = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
    let later = earlier + Duration::days(1);

    assert!(matches!(
        client
            .get_account_orders("HASH", &OrdersParams::new(later, earlier))
            .await,
        Err(Error::InvalidParameter(_))
    ));
    assert!(matches!(
        client
            .get_all_orders(&OrdersParams {
                max_results: Some(0),
                ..OrdersParams::new(earlier, later)
            })
            .await,
        Err(Error::InvalidParameter(_))
    ));
    assert!(matches!(
        client
            .get_transactions("HASH", &TransactionsParams::new(earlier, later, []))
            .await,
        Err(Error::InvalidParameter(_))
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}
