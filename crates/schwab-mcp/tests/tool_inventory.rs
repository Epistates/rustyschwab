//! Smoke test: every tool we intend to expose must appear in `list_tools`.
//!
//! This catches accidental `#[tool]` removal or macro mis-registration.

use schwab_mcp::server::SchwabMcp;
use std::sync::Arc;
use turbomcp::McpHandler;

/// List of every tool the MCP server should expose. Update this list when
/// adding or removing `#[tool]` methods in `src/server.rs`.
const EXPECTED_TOOLS: &[&str] = &[
    // Market data
    "get_quotes",
    "get_quote",
    "get_price_history",
    "get_candles",
    "get_option_chain",
    "get_option_contracts",
    "get_option_expiration_chain",
    "get_movers",
    "search_instruments",
    "get_instrument",
    "get_markets",
    "get_market",
    // Accounts
    "get_account_numbers",
    "get_accounts",
    "get_account",
    "get_positions",
    "get_user_preferences",
    // Orders (read)
    "get_order",
    "get_account_orders",
    "get_all_orders",
    // Orders (write)
    "build_order",
    "check_order",
    "preview_order",
    "place_order",
    "cancel_order",
    "replace_order",
    // Transactions
    "get_transactions",
    "get_transaction",
    // Streaming
    "stream_connect",
    "stream_disconnect",
    "stream_status",
    "stream_subscribe",
    "stream_unsubscribe",
    "stream_set_service_fields",
    "stream_snapshot",
    // Server
    "get_server_status",
];

/// `SchwabMcp::new` doesn't touch the network, so a non-functional client
/// is enough to exercise the tool registry and server metadata.
fn server() -> SchwabMcp {
    let dummy = schwab_rs::SchwabClient::new(schwab_rs::SchwabConfig {
        oauth: schwab_rs::OAuthConfig {
            app_key: "x".repeat(32),
            app_secret: "y".repeat(16),
            ..schwab_rs::OAuthConfig::default()
        },
        client: schwab_rs::ClientConfig::default(),
        streaming: None,
    })
    .expect("dummy client");
    SchwabMcp::new(Arc::new(dummy))
}

/// The `#[turbomcp::server]` macro only takes string literals, so the
/// version it advertises is a copy of the crate version.
#[test]
fn server_info_matches_crate_metadata() {
    let info = server().server_info();
    assert_eq!(info.name, env!("CARGO_PKG_NAME"));
    assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
}

#[test]
fn every_expected_tool_is_registered() {
    let tools = server().list_tools();
    for tool in &tools {
        let annotations = tool
            .annotations
            .as_ref()
            .expect("explicit LLM behavior hints");
        let mutates = matches!(
            tool.name.as_str(),
            "place_order"
                | "replace_order"
                | "cancel_order"
                | "stream_connect"
                | "stream_disconnect"
                | "stream_subscribe"
                | "stream_unsubscribe"
                | "stream_set_service_fields"
        );
        assert_eq!(annotations.read_only_hint, Some(!mutates), "{}", tool.name);
        assert_eq!(annotations.destructive_hint, Some(mutates), "{}", tool.name);
        assert_eq!(annotations.idempotent_hint, Some(!mutates), "{}", tool.name);
        assert_eq!(
            annotations.open_world_hint,
            Some(!matches!(
                tool.name.as_str(),
                "build_order" | "check_order" | "stream_status" | "stream_snapshot"
            ))
        );
        let schema = serde_json::to_value(tool).unwrap();
        assert!(schema["inputSchema"].is_object(), "{}", tool.name);
    }
    let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();

    for expected in EXPECTED_TOOLS {
        assert!(
            names.iter().any(|n| n == expected),
            "missing tool `{expected}` — registered tools: {names:?}"
        );
    }

    assert_eq!(
        tools.len(),
        EXPECTED_TOOLS.len(),
        "registered tools count mismatch — registered: {names:?}",
    );
}

/// `build_order` runs locally: its output is the strict `OrderRequest`
/// JSON the write tools accept, and its input schema exposes the `kind`
/// discriminator so clients can compose specs from the schema alone.
#[tokio::test]
async fn build_order_expands_specs_into_order_requests() {
    let server = server();
    let schema = serde_json::to_value(
        server
            .list_tools()
            .into_iter()
            .find(|t| t.name == "build_order")
            .unwrap(),
    )
    .unwrap();
    assert!(
        schema["inputSchema"].to_string().contains("\"kind\""),
        "{}",
        schema["inputSchema"]
    );
    let result = server
        .call_tool(
            "build_order",
            serde_json::json!({"spec": {
                "kind": "bracket",
                "entry": {"kind": "limit", "price": "150.25",
                    "leg": {"instruction": "BUY", "quantity": "10",
                        "instrument": {"asset_type": "EQUITY", "symbol": "AAPL"}}},
                "take_profit": "160",
                "stop_loss": "145"
            }}),
            &turbomcp::RequestContext::stdio(),
        )
        .await
        .unwrap();
    let result = serde_json::to_value(result).unwrap();
    let order = match &result["structuredContent"] {
        serde_json::Value::Null => {
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
        }
        structured => structured.clone(),
    };
    let request: schwab_rs::types::trading::OrderRequest =
        serde_json::from_value(order.clone()).unwrap();
    request.validate().unwrap();
    assert_eq!(order["orderStrategyType"], "TRIGGER");
    assert_eq!(order["childOrderStrategies"][0]["orderStrategyType"], "OCO");

    let rejected = server
        .call_tool(
            "build_order",
            serde_json::json!({"spec": {"kind": "iceberg"}}),
            &turbomcp::RequestContext::stdio(),
        )
        .await;
    let rejected = serde_json::to_value(rejected.unwrap()).unwrap();
    assert_eq!(rejected["isError"], true, "unknown kinds must be rejected");

    // A spec that parses but cannot expand fails inside the body, so it
    // carries the failure contract.
    let empty_trigger = server
        .call_tool(
            "build_order",
            serde_json::json!({"spec": {
                "kind": "trigger",
                "entry": {"kind": "limit", "price": "150.25",
                    "leg": {"instruction": "BUY", "quantity": "10",
                        "instrument": {"asset_type": "EQUITY", "symbol": "AAPL"}}},
                "children": []
            }}),
            &turbomcp::RequestContext::stdio(),
        )
        .await
        .unwrap();
    let error = serde_json::to_value(empty_trigger).unwrap();
    assert_eq!(error["isError"], true);
    assert_eq!(
        error["structuredContent"]["error"]["code"],
        "invalid_arguments"
    );
    assert_eq!(
        error["structuredContent"]["error"]["disposition"],
        "fix_arguments"
    );
    assert_eq!(error["structuredContent"]["error"]["retryable"], false);
}

/// The time-windowed reads advertise the model-facing query shape: the
/// window is optional (as is `params` itself where a default window
/// exists), each bound accepts a date, RFC 3339 or epoch milliseconds,
/// and the schema says so.
#[test]
fn windowed_reads_advertise_flexible_optional_bounds() {
    let tools = server().list_tools();
    let schema = |name: &str| {
        serde_json::to_value(tools.iter().find(|t| t.name == name).unwrap()).unwrap()["inputSchema"]
            .clone()
    };
    let required =
        |schema: &serde_json::Value| schema["required"].as_array().cloned().unwrap_or_default();
    for (tool, field) in [
        ("get_account_orders", "fromEnteredTime"),
        ("get_all_orders", "toEnteredTime"),
        ("get_transactions", "startDate"),
        ("get_price_history", "endDate"),
    ] {
        let schema = schema(tool);
        let text = schema.to_string();
        assert!(text.contains("YYYY-MM-DD"), "{tool}: {text}");
        assert!(text.contains(field), "{tool}: {text}");
        let params = &schema["properties"]["params"];
        let params = params["anyOf"]
            .as_array()
            .and_then(|options| options.iter().find(|o| o["type"] == "object"))
            .unwrap_or(params);
        assert!(
            !required(params).iter().any(|r| r == field),
            "{tool}: {field} must be optional: {text}"
        );
        assert_eq!(
            required(&schema).iter().any(|r| r == "params"),
            tool == "get_price_history",
            "{tool}: only a symbol makes params required: {text}"
        );
    }
    assert_eq!(
        schema("get_price_history")["properties"]["params"]["required"],
        serde_json::json!(["symbol"])
    );
}

/// Without a principal, `check_order` still reports what the server itself
/// enforces: the order's validity and exposure, the trading switch and the
/// halt, with no limits or approval to speak of. Nothing reaches Schwab.
#[tokio::test]
async fn check_order_reports_a_local_verdict_without_a_principal() {
    let server = server();
    let ctx = turbomcp::RequestContext::stdio();
    let order = serde_json::json!({
        "orderStrategyType": "SINGLE", "orderType": "LIMIT", "session": "NORMAL",
        "duration": "DAY", "price": "50",
        "orderLegCollection": [{"quantity": "2", "instruction": "BUY",
            "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}}]
    });
    let result = server
        .call_tool(
            "check_order",
            serde_json::json!({"account_hash": "A", "order": order}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.is_error(), "{result:?}");
    let verdict: serde_json::Value = serde_json::from_str(result.first_text().unwrap()).unwrap();
    assert_eq!(verdict["allowed"], false);
    assert_eq!(verdict["reasons"].as_array().unwrap().len(), 1);
    assert_eq!(verdict["reasons"][0]["code"], "not_permitted");
    assert_eq!(verdict["tradingEnabled"], false);
    assert_eq!(verdict["requiresApproval"], false);
    assert_eq!(verdict["limits"], serde_json::Value::Null);
    assert_eq!(
        verdict["exposure"],
        serde_json::json!({"quantity": "2", "notional": "100"})
    );
    assert_eq!(verdict["intentDigest"].as_str().unwrap().len(), 64);
    assert!(
        verdict["tag"]
            .as_str()
            .unwrap()
            .starts_with(schwab_mcp::policy::INTENT_TAG_PREFIX)
    );
    assert!(verdict.get("source").is_none());
}

/// Data that came from Schwab or the stream cache is wrapped with its
/// provenance; local computations and control acknowledgements are bare.
#[tokio::test]
async fn responses_carry_provenance_envelopes() {
    let server = server();
    let ctx = turbomcp::RequestContext::stdio();
    let text = |result: turbomcp::ToolResult| -> serde_json::Value {
        assert!(!result.is_error());
        serde_json::from_str(result.first_text().unwrap()).unwrap()
    };
    let snapshot = text(
        server
            .call_tool(
                "stream_snapshot",
                serde_json::json!({"service": "LEVELONE_EQUITIES", "keys": ["AAPL"]}),
                &ctx,
            )
            .await
            .unwrap(),
    );
    assert_eq!(snapshot["source"], "stream_cache");
    assert!(snapshot["asOf"].as_str().unwrap().ends_with('Z'));
    assert_eq!(snapshot["data"]["count"], 0);
    let status = text(
        server
            .call_tool("stream_status", serde_json::json!({}), &ctx)
            .await
            .unwrap(),
    );
    assert_eq!(status["connected"], false);
    assert!(status.get("source").is_none());
}

/// `get_server_status` answers without a Schwab session: the local
/// sections are complete, the calendar is null with an `ask_operator`
/// warning, and nothing is wrapped in a provenance envelope.
#[tokio::test]
async fn server_status_is_complete_without_a_session() {
    let server = server();
    let result = server
        .call_tool(
            "get_server_status",
            serde_json::json!({}),
            &turbomcp::RequestContext::stdio(),
        )
        .await
        .unwrap();
    assert!(!result.is_error(), "{result:?}");
    let status: serde_json::Value = serde_json::from_str(result.first_text().unwrap()).unwrap();
    assert_eq!(status["mode"], "live");
    assert_eq!(status["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        status["serverTime"]["rfc3339"]
            .as_str()
            .unwrap()
            .ends_with('Z')
    );
    assert!(status["serverTime"]["epochMs"].as_i64().unwrap() > 0);
    assert_eq!(status["principal"], serde_json::Value::Null);
    assert_eq!(status["trading"]["enabled"], false);
    assert_eq!(status["trading"]["admissible"], false);
    assert_eq!(status["trading"]["operatorHalt"], serde_json::Value::Null);
    assert_eq!(status["trading"]["circuit"], "closed");
    assert_eq!(status["readiness"]["tokensValid"], false);
    assert_eq!(status["readiness"]["readsReady"], false);
    assert_eq!(status["readiness"]["pendingSubmissions"], 0);
    assert_eq!(status["requestBudget"]["perMinute"], 120);
    assert_eq!(status["requestBudget"]["remaining"], 120);
    assert_eq!(status["stream"]["connected"], false);
    assert_eq!(status["equitySession"], serde_json::Value::Null);
    let warnings = status["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0]["code"], "unavailable");
    assert_eq!(warnings[0]["disposition"], "ask_operator");
    assert!(status.get("source").is_none());
}
