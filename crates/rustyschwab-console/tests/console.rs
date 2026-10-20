//! The console beside an MCP server over the paper venue: what the agent
//! does shows up, and the operator's commands take effect on the server.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rust_decimal::Decimal;
use rustyschwab::paper::{MODEL_VERSION, PaperBroker, PaperConfig, PaperEvent, PaperEventKind};
use rustyschwab_console::{Command, Console, ConsoleConfig, PaperFiles};
use rustyschwab_mcp::backend::{Backend, Paper};
use rustyschwab_mcp::delegation::Mode;
use rustyschwab_mcp::policy::{AuthorizedServer, PrincipalPolicy};
use rustyschwab_mcp::server::Switches;
use serde_json::{Value, json};

const ACCOUNT: &str = "paper-test";

fn paper_config() -> Value {
    json!({
        "model_version": MODEL_VERSION,
        "accounts": {ACCOUNT: {"initial_cash": "1000", "limits": {
            "max_open_quantity": "100", "max_open_notional": "10000",
            "max_position_quantity": "100", "max_snapshot_age_ms": 5000}}},
        "execution_latency_ms": 10,
        "cancel_latency_ms": 100,
        "quote_max_age_ms": 60000,
        "slippage_bps": "0",
        "market_collar_bps": "500",
        "fee_per_share": "0.01",
        "max_working_orders": 100,
    })
}

fn policies() -> Value {
    json!([{
        "name": "desk",
        "token_sha256": "a".repeat(64),
        "tools": ["get_server_status", "get_quotes", "place_order", "cancel_order"],
        "accounts": [ACCOUNT],
        "max_quantity": "10",
        "max_notional": "1000",
        "require_approval": true,
        "max_delegation": {"mode": "autonomous"},
        "dead_man_secs": 600,
    }])
}

/// The venue, open, with a fresh `ABC` quote, as the server and the
/// console both see it.
async fn venue(dir: &Path) -> Arc<PaperBroker> {
    let config: PaperConfig = serde_json::from_value(paper_config()).unwrap();
    let broker = PaperBroker::open(dir.join("state/paper.sqlite3"), config)
        .await
        .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let events = [
        PaperEventKind::Session {
            session_id: "s1".into(),
            open: true,
        },
        PaperEventKind::Quote {
            symbol: "ABC".into(),
            source_at_ms: now,
            bid: "10".parse().unwrap(),
            ask: "10.01".parse().unwrap(),
            bid_size: "100".parse().unwrap(),
            ask_size: "100".parse().unwrap(),
        },
    ];
    for (sequence, kind) in events.into_iter().enumerate() {
        broker
            .apply(PaperEvent {
                sequence: sequence as u64 + 1,
                at_ms: now,
                kind,
            })
            .await
            .unwrap();
    }
    Arc::new(broker)
}

async fn call(server: &AuthorizedServer, tool: &str, args: Value) -> Value {
    let result = rustyschwab_mcp::testing::call_tool(server, tool, args)
        .await
        .unwrap();
    rustyschwab_mcp::testing::reading(&result)
}

fn buy(quantity: &str) -> Value {
    json!({"account_hash": ACCOUNT, "rationale": "testing the console",
        "order": {"orderStrategyType": "SINGLE", "orderType": "LIMIT", "session": "NORMAL",
            "duration": "DAY", "price": "9",
            "orderLegCollection": [{"quantity": quantity, "instruction": "BUY",
                "instrument": {"symbol": "ABC", "assetType": "EQUITY"}}]}})
}

#[tokio::test]
async fn the_console_follows_the_agent_and_its_commands_take_effect() {
    let dir = tempfile::tempdir().unwrap();
    let broker = venue(dir.path()).await;
    let config_file = dir.path().join("paper.json");
    let policy_file = dir.path().join("policy.json");
    std::fs::write(&config_file, paper_config().to_string()).unwrap();
    std::fs::write(&policy_file, policies().to_string()).unwrap();
    let policy: PrincipalPolicy = serde_json::from_value(policies()[0].clone()).unwrap();
    let switches = Switches {
        trading: true,
        cancel: true,
    };
    let desk = AuthorizedServer::new(
        Backend::Paper(Paper {
            broker,
            market_data: None,
        }),
        switches,
        policy,
    );
    let mut console = Console::open(ConsoleConfig {
        paper: Some(PaperFiles {
            config: config_file,
            state: dir.path().join("state/paper.sqlite3"),
        }),
        policy_file: Some(policy_file),
        operator: "ops".into(),
        watchlist: vec!["ABC".into()],
        refresh: Duration::from_secs(1),
        quote_refresh: Duration::ZERO,
        live_quotes: false,
    })
    .await
    .unwrap();

    // The agent asks for an order that needs approval.
    let asked = call(&desk, "place_order", buy("1")).await;
    assert_eq!(asked["error"]["code"], "approval_required", "{asked}");
    let view = settle(&mut console, |v| !v.activity.is_empty()).await;
    assert_eq!(view.mode, "paper");
    let principal = &view.principals[0];
    assert_eq!(principal.name, "desk");
    assert_eq!(principal.approval, Mode::Supervised);
    assert_eq!(principal.ceiling, Mode::Autonomous);
    assert!(principal.dead_man.as_ref().unwrap().seconds_left > 590);
    let call_view = &view.activity[0];
    assert_eq!(call_view.tool, "place_order");
    assert_eq!(call_view.outcome, "approval_required");
    assert_eq!(call_view.rationale.as_deref(), Some("testing the console"));
    assert!(call_view.summary.contains("ABC"), "{}", call_view.summary);
    assert_eq!(view.approvals.len(), 1);
    let waiting = view.approvals[0].clone();
    assert!(
        waiting.description.contains("ABC"),
        "{}",
        waiting.description
    );
    assert_eq!(view.quotes[0].symbol, "ABC");
    assert_eq!(view.quotes[0].bid, Some(10.0));

    // The operator approves it; the agent's retry goes through.
    let approved = console
        .execute(Command::Approve {
            principal: "desk".into(),
            digest: waiting.digest.clone(),
        })
        .await
        .unwrap();
    assert!(approved.starts_with("Approved"), "{approved}");
    let placed = call(&desk, "place_order", buy("1")).await;
    let order_id = placed["data"]["orderId"].as_str().unwrap().to_owned();
    let view = settle(&mut console, |v| v.activity.len() >= 2).await;
    assert!(view.approvals.is_empty());
    let order = view
        .orders
        .iter()
        .find(|o| o.order_id == order_id)
        .expect("the order is shown");
    assert!(order.working);
    assert_eq!(order.principal.as_deref(), Some("desk"));
    assert_eq!(view.principals[0].budget.orders_used, 1);

    // Delegated up to $50, an order of $18 goes unapproved.
    console
        .execute(Command::Delegate {
            principal: "desk".into(),
            mode: Mode::Threshold {
                above: Decimal::from(50),
            },
            until: None,
        })
        .await
        .unwrap();
    let view = console.refresh().await;
    assert!(matches!(
        view.principals[0].approval,
        Mode::Threshold { .. }
    ));
    assert_eq!(
        view.principals[0].delegation.as_ref().unwrap().operator,
        "ops"
    );
    let delegated = call(&desk, "place_order", buy("2")).await;
    assert!(delegated["data"]["orderId"].is_string(), "{delegated}");
    // Beyond the policy's ceiling nothing is granted.
    assert!(
        console
            .execute(Command::Delegate {
                principal: "nobody".into(),
                mode: Mode::Autonomous,
                until: None,
            })
            .await
            .is_err()
    );

    // Halt stops the agent; resume lets it go again.
    console
        .execute(Command::Halt {
            reason: "lunch".into(),
        })
        .await
        .unwrap();
    assert_eq!(console.refresh().await.halt.as_deref(), Some("lunch"));
    let halted = call(&desk, "place_order", buy("3")).await;
    assert_eq!(halted["error"]["code"], "halted", "{halted}");
    console.execute(Command::Resume).await.unwrap();
    assert_eq!(console.refresh().await.halt, None);

    // Kill halts and cancels everything working.
    let killed = console
        .execute(Command::Kill {
            reason: "stop".into(),
        })
        .await
        .unwrap();
    assert!(killed.contains("2 orders cancelled"), "{killed}");
    assert_eq!(console.refresh().await.halt.as_deref(), Some("stop"));
}

/// Refresh until `ready` holds (activity notes are written on their own
/// task), or fail after a few seconds.
async fn settle(
    console: &mut Console,
    ready: impl Fn(&rustyschwab_console::Snapshot) -> bool,
) -> rustyschwab_console::Snapshot {
    for _ in 0..100 {
        let view = console.refresh().await;
        if ready(&view) {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("the console never saw it");
}
