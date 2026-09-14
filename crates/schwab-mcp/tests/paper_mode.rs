//! The MCP server over the paper venue: reads and mutations answer from the
//! venue's state and say so, the venue's admission gates apply, what the
//! venue does not model is refused, and no Schwab trading endpoint is ever
//! reached, with or without a read-only market-data client.
//!
//! Reads run on the plain server (the unauthorized stdio transport); writes
//! run on `AuthorizedServer`, the only transport that can trade, which tags
//! each submission with its intent digest as the venue requires.
mod common;

use std::path::Path;
use std::sync::Arc;

use common::{call, mocked_client};
use rust_decimal::Decimal;
use schwab_mcp::SchwabMcp;
use schwab_mcp::backend::{Backend, Paper};
use schwab_mcp::policy::{AuthorizedServer, PrincipalPolicy};
use schwab_mcp::server::Switches;
use schwab_rs::SchwabClient;
use schwab_rs::broker::TradingBackend;
use schwab_rs::paper::{MODEL_VERSION, PaperBroker, PaperConfig, PaperEvent, PaperEventKind};
use serde_json::{Value, json};
use turbomcp::McpHandler;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

const ACCOUNT: &str = "paper-test";

fn d(v: &str) -> Decimal {
    v.parse().unwrap()
}

fn config() -> PaperConfig {
    serde_json::from_value(json!({
        "model_version": MODEL_VERSION,
        "accounts": {
            ACCOUNT: {
                "initial_cash": "1000",
                "limits": {
                    "max_open_quantity": "100",
                    "max_open_notional": "10000",
                    "max_position_quantity": "100",
                    "max_snapshot_age_ms": 5000,
                },
            },
        },
        "execution_latency_ms": 10,
        "cancel_latency_ms": 100,
        "quote_max_age_ms": 5000,
        "slippage_bps": "0",
        "market_collar_bps": "500",
        "fee_per_share": "0.01",
        "max_working_orders": 100,
    }))
    .unwrap()
}

/// A desk that may trade the paper account within its limits without an
/// approval step.
fn policy() -> PrincipalPolicy {
    serde_json::from_value(json!({
        "name": "paper-desk",
        "token_sha256": "a".repeat(64),
        "tools": ["get_server_status", "get_account", "get_quotes", "get_price_history",
                  "get_order", "get_order_status", "get_account_orders", "get_all_orders",
                  "get_portfolio_summary", "check_order", "place_order", "cancel_order"],
        "accounts": [ACCOUNT],
        "max_quantity": "10",
        "max_notional": "1000",
        "require_approval": false,
    }))
    .unwrap()
}

fn quote(sequence: u64, at_ms: i64, bid: &str, ask: &str, size: &str) -> PaperEvent {
    PaperEvent {
        sequence,
        at_ms,
        kind: PaperEventKind::Quote {
            symbol: "ABC".into(),
            source_at_ms: at_ms,
            bid: d(bid),
            ask: d(ask),
            bid_size: d(size),
            ask_size: d(size),
        },
    }
}

fn order(side: &str, quantity: &str, price: &str) -> Value {
    json!({
        "orderStrategyType": "SINGLE",
        "orderType": "LIMIT",
        "session": "NORMAL",
        "duration": "DAY",
        "price": price,
        "orderLegCollection": [{
            "quantity": quantity,
            "instruction": side,
            "instrument": {"symbol": "ABC", "assetType": "EQUITY"},
        }],
    })
}

/// A venue whose replay clock stands at wall time, so the default order
/// windows of the read tools cover what the test places; its session is
/// open and `ABC` has a fresh quote.
async fn venue(dir: &Path) -> (Arc<PaperBroker>, i64) {
    let now = chrono::Utc::now().timestamp_millis();
    let broker = PaperBroker::open(dir.join("state/paper.sqlite3"), config())
        .await
        .unwrap();
    broker
        .apply(PaperEvent {
            sequence: 1,
            at_ms: now,
            kind: PaperEventKind::Session {
                session_id: "session-1".into(),
                open: true,
            },
        })
        .await
        .unwrap();
    broker
        .apply(quote(2, now, "10", "10.01", "100"))
        .await
        .unwrap();
    (Arc::new(broker), now)
}

fn backend(broker: Arc<PaperBroker>, market_data: Option<Arc<SchwabClient>>) -> Backend {
    Backend::Paper(Paper {
        broker,
        market_data,
    })
}

const TRADING: Switches = Switches {
    trading: true,
    cancel: true,
};

#[tokio::test]
async fn status_and_reads_come_from_the_venue_without_schwab() {
    let dir = tempfile::tempdir().unwrap();
    let (broker, _) = venue(dir.path()).await;
    let server = SchwabMcp::new(backend(broker, None), Switches::default());

    let status = call(&server, "get_server_status", json!({})).await;
    assert_eq!(status["mode"], "paper");
    assert_eq!(status["trading"]["enabled"], false);
    assert_eq!(status["readiness"]["tradingReady"], true);
    assert_eq!(status["requestBudget"], Value::Null);
    assert_eq!(status["equitySession"], Value::Null);
    let paper = &status["paper"];
    assert_eq!(paper["clock"], "replay");
    assert_eq!(paper["session"], json!({"id": "session-1", "open": true}));
    assert_eq!(paper["accounts"], json!([ACCOUNT]));
    assert_eq!(paper["modelVersion"], MODEL_VERSION);
    // The calendar needs Schwab, which this venue does not have.
    assert!(
        status["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["code"] == "not_permitted"),
        "{status}"
    );

    let numbers = call(&server, "get_account_numbers", json!({})).await;
    assert_eq!(numbers["source"], "paper");
    assert_eq!(numbers["data"][0]["hashValue"], ACCOUNT);

    let account = call(
        &server,
        "get_account",
        json!({"account_hash": ACCOUNT, "include_positions": true}),
    )
    .await;
    assert_eq!(account["source"], "paper");
    let securities = &account["data"]["securitiesAccount"];
    assert_eq!(securities["type"], "CASH");
    assert_eq!(securities["currentBalances"]["cashBalance"], 1000.0);
    assert_eq!(
        securities["currentBalances"]["cashAvailableForTrading"],
        1000.0
    );
    assert_eq!(securities["positions"], json!([]));

    let quotes = call(&server, "get_quotes", json!({"symbols": ["ABC", "ZZZ"]})).await;
    assert_eq!(quotes["source"], "paper");
    assert_eq!(quotes["data"]["ABC"]["quote"]["bidPrice"], 10.0);
    assert_eq!(quotes["data"]["ABC"]["quote"]["askPrice"], 10.01);
    assert_eq!(quotes["data"]["ABC"]["fresh"], true);
    assert_eq!(quotes["data"]["errors"]["invalidSymbols"], json!(["ZZZ"]));
    let unknown = call(&server, "get_quote", json!({"symbol": "ZZZ"})).await;
    assert_eq!(unknown["error"]["code"], "not_found");

    // What the venue does not model, and what needs Schwab.
    for (tool, args) in [
        (
            "get_price_history",
            json!({"params": {"symbol": "ABC", "periodType": "day", "period": 1}}),
        ),
        (
            "preview_order",
            json!({"account_hash": ACCOUNT, "order": order("BUY", "1", "11")}),
        ),
        ("get_transactions", json!({"account_hash": ACCOUNT})),
        ("get_user_preferences", json!({})),
        (
            "stream_subscribe",
            json!({"service": "ACCT_ACTIVITY", "keys": [ACCOUNT]}),
        ),
    ] {
        let refused = call(&server, tool, args).await;
        assert_eq!(
            refused["error"]["code"], "not_permitted",
            "{tool}: {refused}"
        );
    }

    // An unknown account is a not-found failure, as Schwab's would be.
    let missing = call(
        &server,
        "get_account",
        json!({"account_hash": "nobody", "include_positions": false}),
    )
    .await;
    assert_eq!(missing["error"]["code"], "not_found", "{missing}");
}

#[tokio::test]
async fn orders_fill_and_cancel_inside_the_venue_and_never_reach_schwab() {
    let rest = MockServer::start().await;
    Mock::given(path("/marketdata/v1/pricehistory"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "symbol": "ABC", "empty": false,
            "candles": [{"datetime": 60_000, "open": 1.0, "high": 2.0, "low": 0.5,
                         "close": 1.5, "volume": 100}]
        })))
        .mount(&rest)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (broker, now) = venue(dir.path()).await;
    let market_data = mocked_client(&rest.uri(), dir.path());
    let server = AuthorizedServer::new(
        backend(broker.clone(), Some(market_data)),
        TRADING,
        policy(),
    );
    let account = json!({"account_hash": ACCOUNT});

    // A client learns the mode at initialize time, before any tool call.
    let info = server.server_info();
    assert_eq!(info.title.as_deref(), Some("Schwab MCP (paper)"));
    assert!(
        info.description
            .unwrap()
            .starts_with("Execution mode: paper.")
    );
    let status = call(&server, "get_server_status", json!({})).await;
    assert_eq!(status["mode"], "paper");
    assert_eq!(status["principal"]["name"], "paper-desk");
    assert_eq!(status["trading"]["enabled"], true);
    assert_eq!(status["trading"]["admissible"], true);

    // Schwab market data is read through the market-data client.
    let history = call(
        &server,
        "get_price_history",
        json!({"params": {"symbol": "ABC", "periodType": "day", "period": 1}}),
    )
    .await;
    assert_eq!(history["source"], "schwab_rest");
    assert_eq!(history["data"]["candles"].as_array().unwrap().len(), 1);

    // Pre-flight, then the order itself, both admitted by the venue.
    let intent = json!({"account_hash": ACCOUNT, "order": order("BUY", "2", "11")});
    let verdict = call(&server, "check_order", intent.clone()).await;
    assert_eq!(verdict["allowed"], true, "{verdict}");
    assert_eq!(verdict["tradingEnabled"], true);
    let placed = call(&server, "place_order", intent).await;
    assert_eq!(placed["source"], "paper", "{placed}");
    let order_id = placed["data"]["orderId"].as_str().unwrap().to_owned();
    let by_id = json!({"account_hash": ACCOUNT, "order_id": order_id});

    let working = call(&server, "get_order_status", by_id.clone()).await;
    assert_eq!(working["source"], "paper");
    assert_eq!(working["data"]["status"], "WORKING");
    assert_eq!(working["data"]["filled"], 0.0);
    assert_eq!(working["data"]["tag"], verdict["tag"]);

    // One share of liquidity fills half the order at the ask.
    broker
        .apply(quote(3, now + 100, "10", "10.01", "1"))
        .await
        .unwrap();
    let partial = call(&server, "get_order_status", by_id.clone()).await;
    assert_eq!(partial["data"]["status"], "WORKING");
    assert_eq!(partial["data"]["filled"], 1.0);
    assert_eq!(partial["data"]["remaining"], 1.0);
    assert_eq!(partial["data"]["fills"], 1);
    assert_eq!(partial["data"]["avgFillPrice"], 10.01);
    let raw = call(&server, "get_order", by_id.clone()).await;
    assert_eq!(raw["source"], "paper");
    let activities = raw["data"]["orderActivityCollection"]
        .as_array()
        .unwrap_or_else(|| panic!("{raw}"));
    assert_eq!(activities.len(), 1);
    assert_eq!(activities[0]["executionLegs"][0]["price"], 10.01);
    assert_eq!(
        raw["data"]["orderLegCollection"][0]["orderLegType"],
        "EQUITY"
    );

    // The cancel takes effect after the venue's cancel latency.
    let canceled = call(&server, "cancel_order", by_id.clone()).await;
    assert_eq!(canceled["source"], "paper", "{canceled}");
    assert_eq!(canceled["data"]["status"], "canceled");
    broker
        .apply(PaperEvent {
            sequence: 4,
            at_ms: now + 300,
            kind: PaperEventKind::Clock,
        })
        .await
        .unwrap();
    let done = call(&server, "get_order_status", by_id.clone()).await;
    assert_eq!(done["data"]["status"], "CANCELED");
    assert_eq!(done["data"]["terminal"], true);
    assert_eq!(done["data"]["filled"], 1.0);

    // The default windows of the list tools cover the venue's clock.
    let listed = call(&server, "get_account_orders", account.clone()).await;
    assert_eq!(listed["source"], "paper");
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    assert_eq!(listed["data"][0]["orderId"], by_id["order_id"]);
    // The principal's policy applies as in live mode: one account only.
    let all = call(&server, "get_all_orders", json!({})).await;
    assert_eq!(all["error"]["code"], "not_permitted");

    // The summary carries the venue's own ledger beside Schwab's shape.
    let summary = call(&server, "get_portfolio_summary", account.clone()).await;
    assert_eq!(summary["source"], "paper");
    let data = &summary["data"];
    assert_eq!(data["type"], "CASH");
    assert_eq!(data["balances"]["cashBalance"], 989.98);
    assert_eq!(data["positions"]["rows"][0][0], "ABC");
    assert_eq!(data["workingOrders"]["rows"], json!([]));
    assert_eq!(data["pendingSubmissions"], 0);
    assert_eq!(data["paper"]["fees"], 0.01);
    assert_eq!(data["paper"]["realizedPnl"], 0.0);
    assert_eq!(data["paper"]["modelVersion"], MODEL_VERSION);
    assert_eq!(data["warnings"], json!([]), "{data}");

    // Every request the Schwab mock saw was market data.
    let paths: Vec<String> = rest
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_owned())
        .collect();
    assert!(!paths.is_empty());
    assert!(
        paths.iter().all(|p| p.starts_with("/marketdata/")),
        "{paths:?}"
    );
}

#[tokio::test]
async fn the_operator_halt_and_the_write_switches_gate_paper_orders() {
    let dir = tempfile::tempdir().unwrap();
    let (broker, _) = venue(dir.path()).await;
    let intent = json!({"account_hash": ACCOUNT, "order": order("BUY", "1", "11")});

    // Trading off: the venue is a simulation, but the switches still apply.
    let locked =
        AuthorizedServer::new(backend(broker.clone(), None), Switches::default(), policy());
    let refused = call(&locked, "place_order", intent.clone()).await;
    assert_eq!(refused["error"]["code"], "not_permitted");
    let verdict = call(&locked, "check_order", intent.clone()).await;
    assert_eq!(verdict["allowed"], false);
    assert_eq!(verdict["tradingEnabled"], false);

    // Trading on, but the operator halted the venue.
    let server = AuthorizedServer::new(backend(broker.clone(), None), TRADING, policy());
    broker
        .halt_submissions("desk review", "operator")
        .await
        .unwrap();
    let verdict = call(&server, "check_order", intent.clone()).await;
    assert_eq!(verdict["allowed"], false);
    assert_eq!(verdict["reasons"][0]["code"], "halted", "{verdict}");
    let halted = call(&server, "place_order", intent.clone()).await;
    assert_eq!(halted["error"]["code"], "halted", "{halted}");
    let status = call(&server, "get_server_status", json!({})).await;
    assert_eq!(status["trading"]["operatorHalt"], "desk review");
    assert_eq!(status["trading"]["admissible"], false);
    assert_eq!(status["readiness"]["tradingReady"], false);

    // Cleared, the same order is accepted.
    broker.clear_operator_halt("operator").await.unwrap();
    let placed = call(&server, "place_order", intent).await;
    assert_eq!(placed["source"], "paper", "{placed}");
    assert!(placed["data"]["orderId"].is_string());
}
