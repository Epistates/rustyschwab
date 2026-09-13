//! Shared fixtures for the MCP integration tests: a client over a mocked
//! Schwab holding a synthetic token, the server around it, and a tool call
//! that returns the payload a model would read.
#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use schwab_mcp::SchwabMcp;
use schwab_rs::{
    SchwabClient, SchwabConfig,
    auth::{FileTokenStore, TokenSet, TokenStore, TokenStoreKind},
};
use serde_json::Value;
use turbomcp::prelude::*;
use wiremock::MockServer;

/// A client whose REST base is `rest_uri`, with no streaming, no rate
/// limit, and a synthetic token stored under `dir`.
pub fn mocked_client(rest_uri: &str, dir: &Path) -> Arc<SchwabClient> {
    let mut config = SchwabConfig::default();
    config.oauth.app_key = "k".repeat(32);
    config.oauth.app_secret = "s".repeat(16);
    config.oauth.tokens_file = dir.join("tokens.json");
    config.oauth.token_store_kind = TokenStoreKind::File;
    config.oauth.auto_refresh = false;
    config.oauth.capture_callback = false;
    config.client.base_url = rest_uri.to_owned();
    config.client.rate_limit.enabled = false;
    config.streaming = None;
    FileTokenStore::new(config.oauth.tokens_file.clone())
        .save(&TokenSet::new(
            "synthetic".into(),
            "synthetic".into(),
            "".into(),
            "Bearer".into(),
            1800,
            "".into(),
            chrono::Utc::now(),
        ))
        .unwrap();
    Arc::new(SchwabClient::new(config).unwrap())
}

/// The MCP server over a mocked Schwab; keep the directory alive for the
/// test's duration.
pub async fn mocked_server(rest: &MockServer) -> (SchwabMcp, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let client = mocked_client(&rest.uri(), dir.path());
    (SchwabMcp::new(client), dir)
}

/// Call a tool and return what the model reads: the parsed success text,
/// or the structured failure.
pub async fn call(server: &SchwabMcp, tool: &str, args: Value) -> Value {
    let result = server
        .call_tool(tool, args, &RequestContext::stdio())
        .await
        .unwrap();
    let result = serde_json::to_value(result).unwrap();
    if result["isError"] == true {
        return result["structuredContent"].clone();
    }
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}
