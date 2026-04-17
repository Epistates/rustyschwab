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

#[test]
fn every_expected_tool_is_registered() {
    // SchwabMcp::new doesn't touch the network — we can construct with a
    // non-functional client purely to exercise the tool registry.
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
    let server = SchwabMcp::new(Arc::new(dummy));

    let tools = server.list_tools();
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
