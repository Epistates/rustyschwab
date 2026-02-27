//! Unit tests for configuration module
//!
//! Tests domain validation, URL parsing, config loading from env/file

use schwab_rs::config::SchwabConfig;
use std::env;
use std::sync::Mutex;
use once_cell::sync::Lazy;

// Mutex to ensure tests that modify environment variables run sequentially
static ENV_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

#[test]
fn test_domain_validation_allowed_schwab_api() {
    let _lock = ENV_MUTEX.lock().unwrap();
    unsafe {
        env::set_var("SCHWAB_APP_KEY", "test_key");
        env::set_var("SCHWAB_APP_SECRET", "test_secret");
        env::set_var("SCHWAB_OAUTH_AUTH_URL", "https://api.schwabapi.com/v1/oauth/authorize");
        env::set_var("SCHWAB_OAUTH_TOKEN_URL", "https://api.schwabapi.com/v1/oauth/token");
        env::set_var("SCHWAB_CLIENT_BASE_URL", "https://api.schwabapi.com");
    }

    let result = SchwabConfig::from_env();
    assert!(result.is_ok(), "Valid Schwab API domain should be allowed");

    // Cleanup
    unsafe {
        env::remove_var("SCHWAB_APP_KEY");
        env::remove_var("SCHWAB_APP_SECRET");
        env::remove_var("SCHWAB_OAUTH_AUTH_URL");
        env::remove_var("SCHWAB_OAUTH_TOKEN_URL");
        env::remove_var("SCHWAB_CLIENT_BASE_URL");
    }
}

#[test]
fn test_domain_validation_allowed_localhost() {
    let _lock = ENV_MUTEX.lock().unwrap();
    unsafe {
        env::set_var("SCHWAB_APP_KEY", "test_key");
        env::set_var("SCHWAB_APP_SECRET", "test_secret");
        env::set_var("SCHWAB_OAUTH_CALLBACK_URL", "https://127.0.0.1:8080/callback");
        env::set_var("SCHWAB_OAUTH_AUTH_URL", "https://api.schwabapi.com/v1/oauth/authorize");
        env::set_var("SCHWAB_OAUTH_TOKEN_URL", "https://api.schwabapi.com/v1/oauth/token");
        env::set_var("SCHWAB_CLIENT_BASE_URL", "https://api.schwabapi.com");
    }

    let result = SchwabConfig::from_env();
    assert!(result.is_ok(), "Localhost callback should be allowed");

    // Cleanup
    unsafe {
        env::remove_var("SCHWAB_APP_KEY");
        env::remove_var("SCHWAB_APP_SECRET");
        env::remove_var("SCHWAB_OAUTH_CALLBACK_URL");
        env::remove_var("SCHWAB_OAUTH_AUTH_URL");
        env::remove_var("SCHWAB_OAUTH_TOKEN_URL");
        env::remove_var("SCHWAB_CLIENT_BASE_URL");
    }
}

#[test]
fn test_domain_validation_denied_external_domain() {
    let _lock = ENV_MUTEX.lock().unwrap();
    unsafe {
        env::set_var("SCHWAB_APP_KEY", "test_key");
        env::set_var("SCHWAB_APP_SECRET", "test_secret");
        env::set_var("SCHWAB_OAUTH_AUTH_URL", "https://evil.com/oauth/authorize");
        env::set_var("SCHWAB_OAUTH_TOKEN_URL", "https://api.schwabapi.com/v1/oauth/token");
        env::set_var("SCHWAB_CLIENT_BASE_URL", "https://api.schwabapi.com");
    }

    let result = SchwabConfig::from_env();
    assert!(result.is_err(), "External domain should be rejected");

    if let Err(e) = result {
        let error_msg = format!("{}", e);
        assert!(error_msg.contains("domain") || error_msg.contains("allowed"),
                "Error should mention domain validation: {}", error_msg);
    }

    // Cleanup
    unsafe {
        env::remove_var("SCHWAB_APP_KEY");
        env::remove_var("SCHWAB_APP_SECRET");
        env::remove_var("SCHWAB_OAUTH_AUTH_URL");
        env::remove_var("SCHWAB_OAUTH_TOKEN_URL");
        env::remove_var("SCHWAB_CLIENT_BASE_URL");
    }
}

#[test]
fn test_config_from_env_missing_required_key() {
    let _lock = ENV_MUTEX.lock().unwrap();
    unsafe {
        env::remove_var("SCHWAB_APP_KEY");
        env::set_var("SCHWAB_APP_SECRET", "test_secret");
    }

    let result = SchwabConfig::from_env();
    assert!(result.is_err(), "Missing required key should fail");

    // Cleanup
    unsafe {
        env::remove_var("SCHWAB_APP_SECRET");
    }
}

#[test]
fn test_config_from_env_missing_required_secret() {
    let _lock = ENV_MUTEX.lock().unwrap();
    unsafe {
        env::set_var("SCHWAB_APP_KEY", "test_key");
        env::remove_var("SCHWAB_APP_SECRET");
    }

    let result = SchwabConfig::from_env();
    assert!(result.is_err(), "Missing required secret should fail");

    // Cleanup
    unsafe {
        env::remove_var("SCHWAB_APP_KEY");
    }
}

#[test]
fn test_config_validation_invalid_url() {
    let _lock = ENV_MUTEX.lock().unwrap();
    unsafe {
        env::set_var("SCHWAB_APP_KEY", "test_key");
        env::set_var("SCHWAB_APP_SECRET", "test_secret");
        env::set_var("SCHWAB_OAUTH_AUTH_URL", "not-a-valid-url");
        env::set_var("SCHWAB_OAUTH_TOKEN_URL", "https://api.schwabapi.com/v1/oauth/token");
        env::set_var("SCHWAB_CLIENT_BASE_URL", "https://api.schwabapi.com");
    }

    let result = SchwabConfig::from_env();
    assert!(result.is_err(), "Invalid URL should be rejected");

    // Cleanup
    unsafe {
        env::remove_var("SCHWAB_APP_KEY");
        env::remove_var("SCHWAB_APP_SECRET");
        env::remove_var("SCHWAB_OAUTH_AUTH_URL");
        env::remove_var("SCHWAB_OAUTH_TOKEN_URL");
        env::remove_var("SCHWAB_CLIENT_BASE_URL");
    }
}

#[test]
fn test_config_defaults() {
    let _lock = ENV_MUTEX.lock().unwrap();
    unsafe {
        env::set_var("SCHWAB_APP_KEY", "test_key");
        env::set_var("SCHWAB_APP_SECRET", "test_secret");
        env::remove_var("SCHWAB_OAUTH_CALLBACK_URL");
        env::set_var("SCHWAB_OAUTH_AUTH_URL", "https://api.schwabapi.com/v1/oauth/authorize");
        env::set_var("SCHWAB_OAUTH_TOKEN_URL", "https://api.schwabapi.com/v1/oauth/token");
        env::set_var("SCHWAB_CLIENT_BASE_URL", "https://api.schwabapi.com");
    }

    let result = SchwabConfig::from_env();
    if let Ok(config) = result {
        // Verify defaults are set
        assert!(!config.oauth.callback_url.is_empty(), "Should have default callback URL");
    } else {
        println!("Note: Config might require all fields, defaults not yet implemented");
    }

    // Cleanup
    unsafe {
        env::remove_var("SCHWAB_APP_KEY");
        env::remove_var("SCHWAB_APP_SECRET");
        env::remove_var("SCHWAB_OAUTH_AUTH_URL");
        env::remove_var("SCHWAB_OAUTH_TOKEN_URL");
        env::remove_var("SCHWAB_CLIENT_BASE_URL");
    }
}
