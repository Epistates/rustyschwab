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
use schwab_rs::paper::{
    MODEL_VERSION, PaperBroker, PaperConfig, PaperEvent, PaperEventKind, PaperFault,
};
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
                  "get_portfolio_summary", "get_trade_journal", "check_order", "place_order",
                  "cancel_order"],
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
    venue_with(dir, config()).await
}

async fn venue_with(dir: &Path, config: PaperConfig) -> (Arc<PaperBroker>, i64) {
    let now = chrono::Utc::now().timestamp_millis();
    let broker = PaperBroker::open(dir.join("state/paper.sqlite3"), config)
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
    assert_eq!(
        status["trading"]["operatorHalt"],
        schwab_mcp::status::HALT_WITHHELD
    );
    assert_eq!(status["trading"]["admissible"], false);
    assert_eq!(status["readiness"]["tradingReady"], false);

    // Cleared, the same order is accepted.
    broker.clear_operator_halt("operator").await.unwrap();
    let placed = call(&server, "place_order", intent).await;
    assert_eq!(placed["source"], "paper", "{placed}");
    assert!(placed["data"]["orderId"].is_string());
}

/// A principal's guardrails over the paper venue: its symbol lists, a
/// price collar judged against the venue's quote, and a daily order
/// budget, each reported by `check_order` and `get_server_status`.
#[tokio::test]
async fn guardrails_bound_symbols_prices_and_the_days_orders() {
    use std::num::NonZeroU32;
    let dir = tempfile::tempdir().unwrap();
    let (broker, _) = venue(dir.path()).await;
    let mut desk = policy();
    desk.denied_symbols = ["XYZ".to_owned()].into();
    desk.price_collar_bps = NonZeroU32::new(100);
    desk.max_daily_orders = NonZeroU32::new(2);
    let server = AuthorizedServer::new(backend(broker, None), TRADING, desk);
    let place = |side: &str, price: &str| json!({"account_hash": ACCOUNT, "order": order(side, "1", price)});

    let status = call(&server, "get_server_status", json!({})).await;
    assert_eq!(status["principal"]["deniedSymbols"], json!(["XYZ"]));
    assert_eq!(status["principal"]["priceCollarBps"], 100);
    assert_eq!(status["principal"]["maxDailyOrders"], 2);

    let mut xyz = place("BUY", "10");
    xyz["order"]["orderLegCollection"][0]["instrument"]["symbol"] = json!("XYZ");
    let denied = call(&server, "place_order", xyz).await;
    assert_eq!(denied["error"]["code"], "not_permitted", "{denied}");

    // The ask is 10.01, so a one percent collar allows a buy up to 10.1101,
    // and the bid is 10, so a sell down to 9.90.
    for (side, price) in [("BUY", "10.12"), ("SELL", "9.89")] {
        let verdict = call(&server, "check_order", place(side, price)).await;
        assert_eq!(verdict["allowed"], false, "{verdict}");
        assert_eq!(verdict["reasons"][0]["code"], "risk_denied", "{verdict}");
        assert_eq!(verdict["limits"]["priceCollarBps"], 100);
        let refused = call(&server, "place_order", place(side, price)).await;
        assert_eq!(refused["error"]["code"], "risk_denied", "{refused}");
        assert!(
            refused["error"]["message"]
                .as_str()
                .unwrap()
                .contains("bps through"),
            "{refused}"
        );
    }

    // A passive price and one inside the collar both go, and use the budget;
    // the refusals above did not.
    for price in ["9", "10.11"] {
        let placed = call(&server, "place_order", place("BUY", price)).await;
        assert_eq!(placed["source"], "paper", "{placed}");
    }
    let verdict = call(&server, "check_order", place("BUY", "9.5")).await;
    assert_eq!(verdict["dailyBudget"]["maxOrders"], 2, "{verdict}");
    assert_eq!(verdict["dailyBudget"]["used"]["orders"], 2, "{verdict}");
    assert_eq!(verdict["allowed"], false);
    let over = call(&server, "place_order", place("BUY", "9.5")).await;
    assert_eq!(over["error"]["code"], "risk_denied", "{over}");
    assert!(
        over["error"]["message"]
            .as_str()
            .unwrap()
            .contains("orders for the trading day"),
        "{over}"
    );
    // The refused submission is journaled and does not count either.
    let verdict = call(&server, "check_order", place("BUY", "9.4")).await;
    assert_eq!(verdict["dailyBudget"]["used"]["orders"], 2, "{verdict}");
}

/// The venue's daily-loss control halts the desk the moment its marked
/// equity has fallen past the cap. The desk is told it is halted; the
/// figures behind it are for a principal that holds every account.
#[tokio::test]
async fn the_daily_loss_control_halts_paper_orders_and_withholds_its_figures() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config();
    config
        .accounts
        .get_mut(ACCOUNT)
        .unwrap()
        .limits
        .max_daily_loss = Some(d("5"));
    let (broker, now) = venue_with(dir.path(), config).await;
    let server = AuthorizedServer::new(backend(broker.clone(), None), TRADING, policy());
    let placed = call(
        &server,
        "place_order",
        json!({"account_hash": ACCOUNT, "order": order("BUY", "10", "11")}),
    )
    .await;
    assert_eq!(placed["source"], "paper", "{placed}");
    // Filled at 10.01 plus fees and marked at the 10 bid: 0.20 down, admissible.
    broker
        .apply(quote(3, now + 100, "10", "10.01", "10"))
        .await
        .unwrap();
    let status = call(&server, "get_server_status", json!({})).await;
    assert_eq!(status["trading"]["operatorHalt"], Value::Null);
    assert_eq!(status["trading"]["admissible"], true);
    // The bid falls to 9.40: 6.20 down, past the cap.
    broker
        .apply(quote(4, now + 200, "9.40", "9.41", "10"))
        .await
        .unwrap();
    let intent = json!({"account_hash": ACCOUNT, "order": order("BUY", "1", "11")});
    let halted = call(&server, "place_order", intent.clone()).await;
    assert_eq!(halted["error"]["code"], "halted", "{halted}");
    let verdict = call(&server, "check_order", intent).await;
    assert_eq!(verdict["allowed"], false);
    assert_eq!(verdict["reasons"][0]["code"], "halted", "{verdict}");
    let status = call(&server, "get_server_status", json!({})).await;
    assert_eq!(
        status["trading"]["operatorHalt"],
        schwab_mcp::status::HALT_WITHHELD
    );
    assert_eq!(status["trading"]["admissible"], false);
    assert_eq!(status["readiness"]["tradingReady"], false);

    // The account, the equity either side of the fall, the loss and the cap
    // are the operator's to read.
    let mut operator = policy();
    operator.accounts.clear();
    operator.all_accounts = true;
    let operator = AuthorizedServer::new(backend(broker, None), TRADING, operator);
    let status = call(&operator, "get_server_status", json!({})).await;
    let reason = status["trading"]["operatorHalt"].as_str().unwrap();
    assert!(
        reason.starts_with("Daily loss 6.20 exceeds the 5 limit for account paper-test: equity"),
        "{reason}"
    );
}

/// The journal keeps every dispatch this principal made for the account,
/// newest first, with the rationale given and the venue's side of it: a
/// denied dispatch never reaches it, a failed one keeps its code, and an
/// order the venue holds for a lost acknowledgement surfaces by its tag.
#[tokio::test]
async fn the_trade_journal_records_why_and_what_the_venue_did() {
    let dir = tempfile::tempdir().unwrap();
    let (broker, now) = venue(dir.path()).await;
    let server = AuthorizedServer::new(backend(broker.clone(), None), TRADING, policy());
    let with = |order: Value, rationale: &str| json!({"account_hash": ACCOUNT, "order": order, "rationale": rationale});

    // Denied by policy before any audit: a blank rationale, then exposure.
    let blank = call(&server, "place_order", with(order("BUY", "1", "11"), " ")).await;
    assert_eq!(blank["error"]["code"], "invalid_arguments", "{blank}");
    let oversized = call(
        &server,
        "place_order",
        with(order("BUY", "20", "11"), "Big."),
    )
    .await;
    assert_eq!(oversized["error"]["code"], "risk_denied", "{oversized}");

    let placed = call(
        &server,
        "place_order",
        with(order("BUY", "2", "11"), "Breakout above 10.90 on volume."),
    )
    .await;
    let first = placed["data"]["orderId"].as_str().unwrap().to_owned();

    // Admitted by policy, refused by the venue: journaled with its code.
    broker
        .halt_submissions("desk review", "operator")
        .await
        .unwrap();
    let halted = call(
        &server,
        "place_order",
        with(order("BUY", "1", "10.5"), "Add."),
    )
    .await;
    assert_eq!(halted["error"]["code"], "halted", "{halted}");
    broker.clear_operator_halt("operator").await.unwrap();

    // The venue keeps the order but the acknowledgement is lost. The clock
    // stays at `now` so the venue's entry times sit inside the journal's
    // window, which ends at wall time.
    broker
        .apply(PaperEvent {
            sequence: 3,
            at_ms: now,
            kind: PaperEventKind::Fault {
                fault: PaperFault::LoseNextAcknowledgement,
            },
        })
        .await
        .unwrap();
    let lost = call(
        &server,
        "place_order",
        with(order("BUY", "1", "10.25"), "Scale in."),
    )
    .await;
    assert_eq!(lost["error"]["code"], "reconciliation_required", "{lost}");

    let canceled = call(
        &server,
        "cancel_order",
        json!({"account_hash": ACCOUNT, "order_id": first, "rationale": "Thesis broke."}),
    )
    .await;
    assert_eq!(canceled["data"]["status"], "canceled", "{canceled}");

    let journal = call(
        &server,
        "get_trade_journal",
        json!({"account_hash": ACCOUNT}),
    )
    .await;
    assert_eq!(journal["source"], "paper", "{journal}");
    let data = &journal["data"];
    assert_eq!(data["principal"], "paper-desk");
    assert_eq!(data["warnings"], json!([]));
    assert_eq!(data["count"], 4, "{data}");
    assert_eq!(data["total"], 4);
    assert_eq!(data["truncated"], false);
    let columns: Vec<&str> = data["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    let column =
        |row: &Value, name: &str| row[columns.iter().position(|c| *c == name).unwrap()].clone();
    let rows = data["rows"].as_array().unwrap();
    let at = |row: &Value| column(row, "at").as_str().unwrap().to_owned();
    assert!(
        rows.windows(2).all(|pair| at(&pair[0]) >= at(&pair[1])),
        "newest first: {rows:?}"
    );
    let by_rationale = |text: &str| {
        rows.iter()
            .find(|row| column(row, "rationale") == text)
            .unwrap_or_else(|| panic!("no row for {text:?}: {rows:?}"))
    };

    let cancel = by_rationale("Thesis broke.");
    assert_eq!(column(cancel, "tool"), "cancel_order");
    assert_eq!(column(cancel, "dispatch"), "completed");
    assert_eq!(column(cancel, "orderId"), first);
    assert_eq!(column(cancel, "status"), "PENDING_CANCEL");
    assert_eq!(column(cancel, "legs")[0]["symbol"], "ABC");

    let unacknowledged = by_rationale("Scale in.");
    assert_eq!(column(unacknowledged, "tool"), "place_order");
    assert_eq!(column(unacknowledged, "dispatch"), "error");
    assert_eq!(column(unacknowledged, "failure"), "reconciliation_required");
    let held = column(unacknowledged, "orderId");
    assert!(
        held.is_string(),
        "the venue's order surfaces by tag: {unacknowledged}"
    );
    assert_ne!(held, first);
    assert_eq!(column(unacknowledged, "status"), "WORKING");
    assert_eq!(column(unacknowledged, "price"), "10.25");

    let refused = by_rationale("Add.");
    assert_eq!(column(refused, "dispatch"), "error");
    assert_eq!(column(refused, "failure"), "halted");
    assert_eq!(column(refused, "orderId"), Value::Null);
    assert_eq!(column(refused, "status"), Value::Null);

    let entry = by_rationale("Breakout above 10.90 on volume.");
    assert_eq!(column(entry, "dispatch"), "completed");
    assert_eq!(column(entry, "orderId"), first);
    assert_eq!(column(entry, "status"), "PENDING_CANCEL");
    assert_eq!(column(entry, "filled"), 0.0);
    assert_eq!(column(entry, "orderType"), "LIMIT");
    assert_eq!(column(entry, "price"), "11");
    assert_eq!(column(entry, "legs")[0]["quantity"], "2");
    assert_eq!(column(entry, "intentDigest").as_str().unwrap().len(), 64);
    assert!(
        column(entry, "tag")
            .as_str()
            .unwrap()
            .starts_with(schwab_mcp::policy::INTENT_TAG_PREFIX)
    );
    assert!(column(entry, "attemptId").is_string());

    // Paging continues into the past; a window excludes what came before it.
    let page = call(
        &server,
        "get_trade_journal",
        json!({"account_hash": ACCOUNT, "limit": 3}),
    )
    .await;
    assert_eq!(page["data"]["count"], 3);
    assert_eq!(page["data"]["truncated"], true);
    let rest = call(
        &server,
        "get_trade_journal",
        json!({"account_hash": ACCOUNT, "cursor": page["data"]["nextCursor"]}),
    )
    .await;
    assert_eq!(rest["data"]["count"], 1, "{rest}");
    assert_eq!(
        column(&rest["data"]["rows"][0], "rationale"),
        "Breakout above 10.90 on volume."
    );
    let future = call(
        &server,
        "get_trade_journal",
        json!({"account_hash": ACCOUNT, "since": now + 3_600_000}),
    )
    .await;
    assert_eq!(future["error"]["code"], "invalid_arguments", "{future}");
    let other = call(
        &server,
        "get_trade_journal",
        json!({"account_hash": "nobody"}),
    )
    .await;
    assert_eq!(other["error"]["code"], "not_permitted", "{other}");
}

/// A desk that requires approval can hold the submission while the
/// operator grants it out of band, instead of failing and being retried.
/// A hold that ends unapproved dispatches nothing and audits nothing.
#[tokio::test]
async fn a_submission_can_hold_for_an_approval_that_lands_out_of_band() {
    let dir = tempfile::tempdir().unwrap();
    let (broker, _) = venue(dir.path()).await;
    let mut policy = policy();
    policy.require_approval = true;
    let server = AuthorizedServer::new(backend(broker.clone(), None), TRADING, policy);
    let intent = json!({
        "account_hash": ACCOUNT,
        "order": order("BUY", "1", "11"),
        "rationale": "Breakout above 10.90 on volume.",
    });
    let digest = call(&server, "check_order", intent.clone()).await["intentDigest"]
        .as_str()
        .unwrap()
        .to_owned();

    // Held, but never approved: denied as it would have been immediately,
    // with nothing audited and nothing on the venue.
    let mut holding = intent.clone();
    holding["wait_ms"] = json!(1500);
    let started = std::time::Instant::now();
    let unapproved = call(&server, "place_order", holding.clone()).await;
    assert_eq!(unapproved["error"]["code"], "approval_required");
    assert_eq!(unapproved["error"]["intentDigest"], digest);
    assert!(started.elapsed() >= std::time::Duration::from_millis(1500));
    assert!(server.audit.records().await.unwrap().is_empty());
    assert!(broker.orders(ACCOUNT, 0, 10).await.unwrap().is_empty());

    // The same call holds while an operator approves the digest it was
    // told, and submits the moment the approval lands.
    let approving = server.audit.clone();
    let landing = digest.clone();
    tokio::spawn(async move {
        // Land after the hold has begun.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        approving
            .approve(
                "paper-desk",
                &landing,
                "operator",
                chrono::Utc::now().timestamp() + 60,
            )
            .await
            .unwrap();
    });
    holding["wait_ms"] = json!(10_000);
    let placed = call(&server, "place_order", holding).await;
    assert_eq!(placed["source"], "paper", "{placed}");
    assert!(placed["data"]["orderId"].is_string());
    assert_eq!(broker.orders(ACCOUNT, 0, 10).await.unwrap().len(), 1);

    // The journal records that the trade waited on a human.
    let records = server.audit.records().await.unwrap();
    assert_eq!(records.len(), 1);
    assert!(
        records[0].request["approvalWaitedMs"].as_u64().is_some(),
        "{}",
        records[0].request
    );
    assert_eq!(records[0].request["intentDigest"], digest);
}
