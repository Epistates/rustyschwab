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
    "get_option_chain",
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
    "get_user_preferences",
    // Orders (read)
    "get_order",
    "get_account_orders",
    "get_all_orders",
    // Orders (write)
    "build_order",
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
                "build_order" | "stream_status" | "stream_snapshot"
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
