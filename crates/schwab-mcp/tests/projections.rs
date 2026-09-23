//! The projection tools over a mocked Schwab: envelope, redaction, paging
//! and the raw tools' unchanged shape.
mod common;

use common::{call, mocked_server as server};
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

fn candle(t: i64) -> Value {
    json!({"datetime": t, "open": 1.0, "high": 2.0, "low": 0.5, "close": 1.5, "volume": 100})
}

#[tokio::test]
async fn candles_page_by_time_and_leave_the_raw_tool_alone() {
    let rest = MockServer::start().await;
    Mock::given(path("/marketdata/v1/pricehistory"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "symbol": "AAPL", "empty": false, "previousClose": 149.5,
            "candles": (1..=5).map(|i| candle(i * 60_000)).collect::<Vec<_>>()
        })))
        .mount(&rest)
        .await;
    let (server, _dir) = server(&rest).await;

    let first = call(
        &server,
        "get_candles",
        json!({"params": {"symbol": "AAPL", "periodType": "day", "period": 1}, "limit": 2}),
    )
    .await;
    assert_eq!(first["source"], "schwab_rest");
    assert!(first["asOf"].as_str().unwrap().ends_with('Z'));
    let data = &first["data"];
    assert_eq!(data["symbol"], "AAPL");
    assert_eq!(data["previousClose"], 149.5);
    assert_eq!(data["columns"], json!(["t", "o", "h", "l", "c", "v"]));
    assert_eq!(
        data["rows"],
        json!([
            [60000, 1.0, 2.0, 0.5, 1.5, 100],
            [120000, 1.0, 2.0, 0.5, 1.5, 100]
        ])
    );
    assert_eq!(data["count"], 2);
    assert_eq!(data["total"], 5);
    assert_eq!(data["truncated"], true);
    assert_eq!(data["nextCursor"], "120000");

    let rest_of = call(
        &server,
        "get_candles",
        json!({"params": {"symbol": "AAPL", "periodType": "day", "period": 1},
               "cursor": data["nextCursor"]}),
    )
    .await;
    assert_eq!(rest_of["data"]["count"], 3);
    assert_eq!(rest_of["data"]["rows"][0][0], 180000);
    assert_eq!(rest_of["data"]["truncated"], false);
    assert!(rest_of["data"].get("nextCursor").is_none());

    let bad_cursor = call(
        &server,
        "get_candles",
        json!({"params": {"symbol": "AAPL"}, "cursor": "yesterday"}),
    )
    .await;
    assert_eq!(bad_cursor["error"]["code"], "invalid_arguments");

    // The raw tool still returns Schwab's shape.
    let raw = call(
        &server,
        "get_price_history",
        json!({"params": {"symbol": "AAPL", "periodType": "day", "period": 1}}),
    )
    .await;
    assert_eq!(raw["data"]["candles"].as_array().unwrap().len(), 5);
    assert_eq!(raw["data"]["candles"][0]["open"], 1.0);
}

#[tokio::test]
async fn option_contracts_are_ordered_and_spreads_are_refused() {
    let rest = MockServer::start().await;
    Mock::given(path("/marketdata/v1/chains"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "symbol": "AAPL", "status": "SUCCESS", "isDelayed": false,
            "underlying": {"last": 150.1, "bid": 150.0, "ask": 150.2, "mark": 150.1, "quoteTime": 5},
            "callExpDateMap": {"2025-01-17:3": {
                "155.0": [{"putCall": "CALL", "symbol": "AAPL  250117C00155000",
                    "strikePrice": 155.0, "expirationDate": "2025-01-17T00:00:00.000+00:00",
                    "bid": 0.5, "ask": 0.6, "delta": 0.2, "daysToExpiration": 3}],
                "150.0": [{"putCall": "CALL", "symbol": "AAPL  250117C00150000",
                    "strikePrice": 150.0, "expirationDate": "2025-01-17T00:00:00.000+00:00",
                    "bid": 2.0, "ask": 2.1, "delta": 0.5, "daysToExpiration": 3}]}},
            "putExpDateMap": {"2025-01-17:3": {
                "150.0": [{"putCall": "PUT", "symbol": "AAPL  250117P00150000",
                    "strikePrice": 150.0, "expirationDate": "2025-01-17T00:00:00.000+00:00",
                    "bid": 1.9, "ask": 2.0, "delta": -0.5, "daysToExpiration": 3}]}}
        })))
        .mount(&rest)
        .await;
    let (server, _dir) = server(&rest).await;

    let page = call(
        &server,
        "get_option_contracts",
        json!({"params": {"symbol": "AAPL", "strikeCount": 2}, "limit": 2}),
    )
    .await;
    let data = &page["data"];
    assert_eq!(data["underlying"]["last"], 150.1);
    assert_eq!(data["columns"][0], "symbol");
    assert_eq!(data["rows"][0][0], "AAPL  250117C00150000");
    assert_eq!(data["rows"][1][0], "AAPL  250117P00150000");
    assert_eq!(data["rows"][1][10], -0.5);
    assert_eq!(data["total"], 3);
    assert_eq!(
        data["nextCursor"],
        "2025-01-17|150.000|P|AAPL  250117P00150000"
    );

    let rest_of = call(
        &server,
        "get_option_contracts",
        json!({"params": {"symbol": "AAPL", "strikeCount": 2}, "cursor": data["nextCursor"]}),
    )
    .await;
    assert_eq!(rest_of["data"]["rows"][0][0], "AAPL  250117C00155000");
    assert_eq!(rest_of["data"]["truncated"], false);

    let spread = call(
        &server,
        "get_option_contracts",
        json!({"params": {"symbol": "AAPL", "strategy": "VERTICAL"}}),
    )
    .await;
    assert_eq!(spread["error"]["code"], "invalid_arguments");
    assert_eq!(rest.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn positions_are_tabulated_and_the_account_number_is_masked() {
    let rest = MockServer::start().await;
    Mock::given(path("/trader/v1/accounts/A"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"securitiesAccount": {
            "accountNumber": "123456789", "type": "MARGIN",
            "positions": [
                {"longQuantity": 10.0, "shortQuantity": 0.0, "averagePrice": 100.0,
                 "marketValue": 1500.0, "currentDayProfitLoss": 5.0, "longOpenProfitLoss": 500.0,
                 "instrument": {"assetType": "EQUITY", "symbol": "MSFT"}},
                {"longQuantity": 2.0, "shortQuantity": 0.0,
                 "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}}
            ],
            "currentBalances": {"liquidationValue": 2000.0}
        }})))
        .mount(&rest)
        .await;
    let (server, _dir) = server(&rest).await;

    let page = call(&server, "get_positions", json!({"account_hash": "A"})).await;
    let data = &page["data"];
    assert_eq!(data["accountNumber"], "***6789");
    assert_eq!(data["type"], "MARGIN");
    assert_eq!(data["columns"][0], "symbol");
    assert_eq!(
        data["rows"],
        json!([
            ["AAPL", "EQUITY", 2.0, 0.0, null, null, null, null, null],
            ["MSFT", "EQUITY", 10.0, 0.0, 100.0, 1500.0, 5.0, 500.0, null]
        ])
    );
    assert_eq!(data["truncated"], false);
    assert!(data.get("currentBalances").is_none());
    let query: Vec<String> = rest
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.query().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(query, ["fields=positions"]);
}
