//! `get_performance` over a mocked Schwab: the order read, the executions
//! of options, shorts and nested child orders turned into round trips, open
//! lots valued at quotes, the starting equity from the account, the order
//! horizon, and the principal's verified option multipliers.
mod common;

use common::{call, mocked_client, mocked_server};
use rust_decimal::Decimal;
use rustyschwab_mcp::backend::Backend;
use rustyschwab_mcp::policy::{AuthorizedServer, PrincipalPolicy};
use rustyschwab_mcp::server::Switches;
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

/// A money field: a decimal string whose trailing zeros mean nothing.
fn money(value: &Value) -> Decimal {
    value
        .as_str()
        .unwrap_or_else(|| panic!("not a decimal string: {value}"))
        .parse()
        .unwrap()
}

fn d(v: &str) -> Decimal {
    v.parse().unwrap()
}

fn leg(instruction: &str, quantity: u32, asset: &str, symbol: &str) -> Value {
    json!({"legId": 1, "orderLegType": asset, "instruction": instruction, "quantity": quantity,
           "instrument": {"assetType": asset, "symbol": symbol}})
}

fn filled(order_id: u32, entered: &str, leg: Value, quantity: u32, price: f64) -> Value {
    json!({
        "orderId": order_id, "status": "FILLED", "enteredTime": entered,
        "orderLegCollection": [leg],
        "orderActivityCollection": [
            {"activityType": "EXECUTION", "executionType": "FILL", "quantity": quantity,
             "executionLegs": [{"legId": 1, "quantity": quantity, "price": price, "time": entered}]}
        ]
    })
}

const OPTION: &str = "ABC  260116C00100000";

async fn schwab() -> MockServer {
    let rest = MockServer::start().await;
    Mock::given(path("/trader/v1/accounts/A/orders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            // Newest first, as Schwab lists them.
            {"orderId": 104, "status": "FILLED", "orderStrategyType": "TRIGGER",
             "enteredTime": "2026-01-09T14:00:00+0000",
             "childOrderStrategies": [
                filled(105, "2026-01-09T14:00:00+0000", leg("BUY", 1, "EQUITY", "QQQ"), 1, 300.0)]},
            filled(103, "2026-01-08T14:00:00+0000", leg("BUY_TO_COVER", 4, "EQUITY", "XYZ"), 4, 45.0),
            filled(102, "2026-01-07T14:00:00+0000", leg("SELL_SHORT", 10, "EQUITY", "XYZ"), 10, 50.0),
            filled(101, "2026-01-06T14:00:00+0000", leg("SELL_TO_CLOSE", 2, "OPTION", OPTION), 2, 2.0),
            filled(100, "2026-01-05T14:00:00+0000", leg("BUY_TO_OPEN", 2, "OPTION", OPTION), 2, 1.5),
            {"orderId": 99, "status": "CANCELED", "enteredTime": "2026-01-05T13:00:00+0000",
             "orderLegCollection": [leg("BUY", 5, "EQUITY", "ABC")],
             "orderActivityCollection": [
                {"activityType": "EXECUTION", "executionType": "CANCELED",
                 "executionLegs": [{"legId": 1, "quantity": 5, "price": 0.0,
                                    "time": "2026-01-05T13:01:00+0000"}]}]}
        ])))
        .mount(&rest)
        .await;
    Mock::given(path("/trader/v1/accounts/A"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"securitiesAccount": {
                "accountNumber": "123456789", "type": "MARGIN",
                "positions": [
                    {"shortQuantity": 6.0, "instrument": {"assetType": "EQUITY", "symbol": "XYZ"}},
                    // The window's fills leave one share; two were held before it.
                    {"longQuantity": 3.0, "instrument": {"assetType": "EQUITY", "symbol": "QQQ"}}
                ],
                "currentBalances": {"liquidationValue": 10000.0}
            }})),
        )
        .mount(&rest)
        .await;
    // QQQ has no quote: its open lot stays at the last fill.
    Mock::given(path("/marketdata/v1/quotes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "XYZ": {"symbol": "XYZ", "quote": {"mark": 44.0, "bidPrice": 43.9, "askPrice": 44.1}},
            "errors": {"invalidSymbols": ["QQQ"]}
        })))
        .mount(&rest)
        .await;
    rest
}

#[tokio::test]
async fn performance_matches_options_shorts_and_nested_fills_and_values_what_is_open() {
    let rest = schwab().await;
    let (server, _dir) = mocked_server(&rest).await;

    let report = call(&server, "get_performance", json!({"account_hash": "A"})).await;
    assert_eq!(report["source"], "schwab_rest", "{report}");
    let data = &report["data"];
    assert_eq!(data["ordersRead"], 6);
    assert_eq!(data["ordersWithFills"], 5);
    assert_eq!(data["fills"], 5);
    assert_eq!(data["skippedExecutions"], 0);

    // The option: (2.00 - 1.50) * 2 contracts * 100. The short: (50 - 45) * 4.
    assert_eq!(data["roundTripsTotal"], 2);
    let trips = data["roundTrips"].as_array().unwrap();
    let option = trips.iter().find(|t| t["symbol"] == OPTION).unwrap();
    assert_eq!(money(&option["multiplier"]), d("100"));
    assert_eq!(money(&option["grossPnl"]), d("100"));
    let short = trips.iter().find(|t| t["symbol"] == "XYZ").unwrap();
    assert_eq!(short["direction"], "short");
    assert_eq!(money(&short["grossPnl"]), d("20"));
    assert_eq!(money(&data["realizedPnl"]), d("120"));

    // Six shares short from 50 marked at 44; the nested QQQ lot at its fill.
    let marks = &data["marks"];
    assert_eq!(marks["XYZ"]["source"], "quote");
    assert_eq!(money(&marks["XYZ"]["price"]), d("44"));
    assert_eq!(marks["QQQ"]["source"], "last_fill");
    assert_eq!(money(&marks["QQQ"]["price"]), d("300"));
    assert_eq!(money(&data["unrealizedPnl"]), d("36"));
    assert_eq!(money(&data["symbols"]["XYZ"]["openQuantity"]), d("-6"));
    assert_eq!(money(&data["symbols"]["QQQ"]["openQuantity"]), d("1"));
    assert_eq!(money(&data["netPnl"]), d("156"));

    // Started from the account's value less the window's profit.
    assert_eq!(money(&data["startEquity"]), d("9844"));
    assert_eq!(money(&data["endEquity"]), d("10000"));
    assert_eq!(data["netReturn"].as_f64().unwrap(), 156.0 / 9844.0);

    // The account holds three QQQ where the fills leave one.
    let warnings = data["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0]
            .as_str()
            .unwrap()
            .starts_with("QQQ: the account holds 3"),
        "{warnings:?}"
    );
    assert!(data["notes"].as_str().unwrap().contains("window"));

    // The orders were asked for the last 30 days; open symbols were quoted.
    let requests = rest.received_requests().await.unwrap();
    let orders = requests
        .iter()
        .find(|r| r.url.path() == "/trader/v1/accounts/A/orders")
        .unwrap();
    let query = orders.url.query().unwrap();
    assert!(query.contains("fromEnteredTime=") && query.contains("toEnteredTime="));
    let quotes = requests
        .iter()
        .find(|r| r.url.path() == "/marketdata/v1/quotes")
        .unwrap();
    let asked = quotes.url.query().unwrap();
    assert!(asked.contains("QQQ") && asked.contains("XYZ"), "{asked}");
    assert!(!asked.contains("ABC"), "{asked}");

    // Narrowed to one symbol, only its fills count.
    let narrowed = call(
        &server,
        "get_performance",
        json!({"account_hash": "A", "symbols": ["xyz"]}),
    )
    .await;
    assert_eq!(narrowed["data"]["fills"], 2);
    assert_eq!(money(&narrowed["data"]["realizedPnl"]), d("20"));
    assert_eq!(narrowed["data"]["symbolsFilter"], json!(["XYZ"]));
}

#[tokio::test]
async fn performance_asks_no_further_back_than_schwabs_order_list() {
    let rest = schwab().await;
    let (server, _dir) = mocked_server(&rest).await;

    let report = call(
        &server,
        "get_performance",
        json!({"account_hash": "A", "since": "2020-01-01"}),
    )
    .await;
    let window = &report["data"]["window"];
    assert_eq!(window["clampedToOrderHorizon"], true, "{report}");
    let since = chrono::DateTime::parse_from_rfc3339(window["since"].as_str().unwrap()).unwrap();
    let age = chrono::Utc::now() - since.with_timezone(&chrono::Utc);
    assert_eq!(age.num_days(), 59);

    let ended = call(
        &server,
        "get_performance",
        json!({"account_hash": "A", "until": "2020-01-01"}),
    )
    .await;
    assert_eq!(ended["error"]["code"], "invalid_arguments", "{ended}");
}

#[tokio::test]
async fn a_failed_account_read_leaves_the_fills_and_nulls_what_needs_equity() {
    // The orders answer; the account does not.
    let rest = MockServer::start().await;
    Mock::given(path("/trader/v1/accounts/A/orders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            filled(
                1,
                "2026-01-05T14:00:00+0000",
                leg("BUY", 1, "EQUITY", "ABC"),
                1,
                10.0
            ),
            filled(
                2,
                "2026-01-06T14:00:00+0000",
                leg("SELL", 1, "EQUITY", "ABC"),
                1,
                12.0
            ),
        ])))
        .mount(&rest)
        .await;
    Mock::given(path("/trader/v1/accounts/A"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&rest)
        .await;
    let (server, _dir) = mocked_server(&rest).await;

    let report = call(&server, "get_performance", json!({"account_hash": "A"})).await;
    let data = &report["data"];
    assert_eq!(data["realizedPnl"], "2", "{report}");
    assert_eq!(data["roundTripsTotal"], 1);
    for field in ["startEquity", "netReturn", "maxDrawdown", "sharpe"] {
        assert_eq!(data[field], Value::Null, "{field}");
    }
    let warnings: Vec<&str> = data["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        warnings
            .iter()
            .any(|w| w.starts_with("The account was not read"))
    );
    assert!(warnings.iter().any(|w| w.starts_with("No starting equity")));
}

/// A principal's verified multiplier prices its options, its accounts scope
/// the read, and the tool is granted like any other.
#[tokio::test]
async fn a_principals_verified_option_multiplier_and_accounts_apply() {
    let rest = schwab().await;
    let dir = tempfile::tempdir().unwrap();
    let backend = Backend::Live(mocked_client(&rest.uri(), dir.path()));
    let policy = |tools: Value| -> PrincipalPolicy {
        serde_json::from_value(json!({
            "name": "analyst", "token_sha256": "b".repeat(64), "tools": tools,
            "accounts": ["A"], "option_multipliers": {OPTION: "10"},
        }))
        .unwrap()
    };
    let server = AuthorizedServer::new(
        backend.clone(),
        Switches::default(),
        policy(json!(["get_performance"])),
    );

    let report = call(&server, "get_performance", json!({"account_hash": "A"})).await;
    let trips = report["data"]["roundTrips"].as_array().unwrap();
    let option = trips.iter().find(|t| t["symbol"] == OPTION).unwrap();
    assert_eq!(money(&option["multiplier"]), d("10"), "{report}");
    assert_eq!(money(&option["grossPnl"]), d("10"));

    let other = call(&server, "get_performance", json!({"account_hash": "B"})).await;
    assert_eq!(other["error"]["code"], "not_permitted", "{other}");
    let malformed = call(&server, "get_performance", json!({"since": "2026-01-01"})).await;
    assert_eq!(
        malformed["error"]["code"], "invalid_arguments",
        "{malformed}"
    );

    // Without the grant the tool is refused as any other.
    let ungranted =
        AuthorizedServer::new(backend, Switches::default(), policy(json!(["get_account"])));
    let refused = call(&ungranted, "get_performance", json!({"account_hash": "A"})).await;
    assert_eq!(refused["error"]["code"], "not_permitted", "{refused}");
}
