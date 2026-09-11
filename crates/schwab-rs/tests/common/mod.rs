//! Shared fixtures for wiremock-backed integration tests.
#![allow(dead_code)]

use chrono::{Duration as CDuration, Utc};
use schwab_rs::auth::{FileTokenStore, TokenSet, TokenStore, TokenStoreKind};
use schwab_rs::{SchwabClient, SchwabConfig};
use std::time::Duration;
use wiremock::MockServer;

/// A client pointed at `server` with a pre-seeded file token store.
///
/// `expired` seeds an access token that is already past its expiry so the
/// first request must refresh; otherwise the token is good for 20 minutes.
pub fn mock_client(
    server: &MockServer,
    expired: bool,
) -> (SchwabClient, tempfile::TempDir, TokenSet) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = SchwabConfig::default();
    config.oauth.app_key = "12345678901234567890123456789012".into();
    config.oauth.app_secret = "1234567890123456".into();
    config.oauth.token_store_kind = TokenStoreKind::File;
    config.oauth.tokens_file = dir.path().join("tokens.json");
    config.oauth.auto_refresh = false;
    config.oauth.capture_callback = false;
    config.oauth.token_url = format!("{}/oauth/token", server.uri());
    config.client.base_url = server.uri();
    config.client.rate_limit.enabled = false;
    config.client.retry.initial_backoff = Duration::from_millis(1);
    config.client.retry.max_backoff = Duration::from_millis(2);
    config.streaming = None;
    let issued = Utc::now() - CDuration::days(2);
    let mut tokens = TokenSet::new(
        "old_access".into(),
        "same_refresh".into(),
        "".into(),
        "Bearer".into(),
        1800,
        "".into(),
        issued,
    );
    tokens.expires_at = if expired {
        Utc::now() - CDuration::minutes(1)
    } else {
        Utc::now() + CDuration::minutes(20)
    };
    FileTokenStore::new(config.oauth.tokens_file.clone())
        .save(&tokens)
        .unwrap();
    (SchwabClient::new(config).unwrap(), dir, tokens)
}
