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
                "stream_status" | "stream_snapshot"
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
