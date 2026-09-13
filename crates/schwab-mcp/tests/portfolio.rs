//! `get_portfolio_summary` over a mocked Schwab: the two reads, the
//! envelope and masking, the sections, and degradation when the orders
//! read fails.
mod common;

use common::{call, mocked_server};
use serde_json::json;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

#[tokio::test]
async fn summary_reads_orders_then_the_account_and_degrades_per_section() {
    let rest = MockServer::start().await;
    Mock::given(path("/trader/v1/accounts/A/orders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"orderId": 1, "status": "FILLED", "enteredTime": "2025-01-14T14:30:00+0000"},
            {"orderId": 2, "status": "WORKING", "orderType": "LIMIT", "price": 150.0,
             "duration": "DAY", "session": "NORMAL", "quantity": 10, "filledQuantity": 0,
             "remainingQuantity": 10, "enteredTime": "2025-01-15T14:30:00+0000",
             "orderLegCollection": [{"orderLegType": "EQUITY", "instruction": "BUY",
                "quantity": 10, "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}}]}
        ])))
        .mount(&rest)
        .await;
    Mock::given(path("/trader/v1/accounts/A"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"securitiesAccount": {
            "accountNumber": "123456789", "type": "MARGIN", "isDayTrader": false, "roundTrips": 1,
            "positions": [
                {"longQuantity": 10.0, "shortQuantity": 0.0, "averagePrice": 100.0,
                 "marketValue": 1500.0, "currentDayProfitLoss": 5.0, "longOpenProfitLoss": 500.0,
                 "instrument": {"assetType": "EQUITY", "symbol": "MSFT"}},
                {"longQuantity": 2.0, "shortQuantity": 0.0, "marketValue": 300.0,
                 "currentDayProfitLoss": -1.25, "longOpenProfitLoss": -10.0,
                 "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}}
            ],
            "currentBalances": {"liquidationValue": 2000.0, "cashBalance": 200.0,
                "availableFunds": 150.0, "buyingPower": 300.0, "dayTradingBuyingPower": 600.0,
                "longMarketValue": 1800.0, "maintenanceRequirement": 540.0, "maintenanceCall": 0.0},
            "projectedBalances": {"isInCall": false}
        }})))
        .mount(&rest)
        .await;
    let (server, _dir) = mocked_server(&rest).await;

    let summary = call(
        &server,
        "get_portfolio_summary",
        json!({"account_hash": "A"}),
    )
    .await;
    assert_eq!(summary["source"], "schwab_rest");
    let data = &summary["data"];
    assert_eq!(data["accountNumber"], "***6789");
    assert_eq!(data["type"], "MARGIN");
    assert_eq!(data["roundTrips"], 1);
    assert_eq!(data["balances"]["buyingPower"], 300.0);
    assert_eq!(data["balances"]["liquidationValue"], 2000.0);
    assert_eq!(data["balances"]["inCall"], false);
    assert_eq!(data["balances"]["unsettledCash"], json!(null));
    assert_eq!(data["pnl"], json!({"day": 3.75, "open": 490.0}));
    assert_eq!(data["positions"]["count"], 2);
    assert_eq!(data["positions"]["rows"][0][0], "AAPL");
    assert_eq!(data["positions"]["truncated"], false);
    let orders = &data["workingOrders"];
    assert_eq!(orders["columns"][0], "orderId");
    assert_eq!(orders["count"], 1);
    assert_eq!(orders["total"], 1);
    assert_eq!(orders["rows"][0][0], "2");
    assert_eq!(orders["rows"][0][2], "WORKING");
    assert_eq!(orders["rows"][0][13][0]["symbol"], "AAPL");
    assert!(data["ordersSince"].as_str().unwrap().ends_with('Z'));
    assert_eq!(data["pendingSubmissions"], 0);
    assert_eq!(data["warnings"], json!([]));

    // Orders first, then the account with positions, over Schwab's list horizon.
    let requests = rest.received_requests().await.unwrap();
    let paths: Vec<String> = requests.iter().map(|r| r.url.path().to_owned()).collect();
    assert_eq!(
        paths,
        ["/trader/v1/accounts/A/orders", "/trader/v1/accounts/A"]
    );
    let query = requests[0].url.query().unwrap();
    assert!(query.contains("fromEnteredTime=") && query.contains("toEnteredTime="));
    assert_eq!(requests[1].url.query(), Some("fields=positions"));

    // A failed orders read leaves that section null and explained; the
    // account still answers.
    rest.reset().await;
    Mock::given(path("/trader/v1/accounts/A/orders"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "errors": [{"id": "x", "status": "400", "title": "Bad Request",
                        "detail": "invalid window"}]
        })))
        .mount(&rest)
        .await;
    Mock::given(path("/trader/v1/accounts/A"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"securitiesAccount": {
                "accountNumber": "123456789", "type": "CASH",
                "currentBalances": {"cashAvailableForTrading": 42.0}
            }})),
        )
        .mount(&rest)
        .await;
    let degraded = call(
        &server,
        "get_portfolio_summary",
        json!({"account_hash": "A"}),
    )
    .await;
    let data = &degraded["data"];
    assert_eq!(data["workingOrders"], json!(null));
    assert_eq!(data["warnings"].as_array().unwrap().len(), 1);
    assert_eq!(data["balances"]["buyingPower"], 42.0);
    assert_eq!(data["pnl"], json!({"day": null, "open": null}));
    assert_eq!(data["positions"]["count"], 0);

    // A failed account read fails the tool.
    rest.reset().await;
    Mock::given(path("/trader/v1/accounts/A"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&rest)
        .await;
    let failed = call(
        &server,
        "get_portfolio_summary",
        json!({"account_hash": "A"}),
    )
    .await;
    assert!(failed["error"]["code"].is_string(), "{failed}");
}
