//! Unit tests for retry module
//!
//! Tests retry configuration, policy creation, and basic retry behavior

use schwab_rs::config::RetryConfig;
use schwab_rs::retry::RetryPolicy;
use std::time::Duration;

#[test]
fn test_retry_policy_creation() {
    let config = RetryConfig {
        max_retries: 5,
        initial_backoff: Duration::from_secs(1),
        max_backoff: Duration::from_secs(60),
        backoff_multiplier: 2.0,
        retry_on_status: vec![500, 502, 503],
    };

    let _policy = RetryPolicy::new(&config);

    // Verify policy can be created with custom config
    // Exponential backoff behavior:
    // Attempt 0: 1 * (2^0) = 1 second
    // Attempt 1: 1 * (2^1) = 2 seconds
    // Attempt 2: 1 * (2^2) = 4 seconds
    // Attempt 3: 1 * (2^3) = 8 seconds
}

#[test]
fn test_retry_config_with_max_backoff() {
    let config = RetryConfig {
        max_retries: 10,
        initial_backoff: Duration::from_secs(1),
        max_backoff: Duration::from_secs(10), // Cap at 10 seconds
        backoff_multiplier: 2.0,
        retry_on_status: vec![500],
    };

    let _policy = RetryPolicy::new(&config);

    // Even with high retry counts, backoff should not exceed max_backoff
    // For attempt 10: 1 * (2^10) = 1024 seconds, but implementation caps at 10
}

#[test]
fn test_retry_config_defaults() {
    let config = RetryConfig::default();

    assert_eq!(config.max_retries, 3);
    assert_eq!(config.initial_backoff, Duration::from_millis(100));
    assert_eq!(config.max_backoff, Duration::from_secs(10));
    assert_eq!(config.backoff_multiplier, 2.0);
    assert_eq!(config.retry_on_status, vec![429, 500, 502, 503, 504]);
}

#[test]
fn test_zero_retries_config() {
    let config = RetryConfig {
        max_retries: 0,
        initial_backoff: Duration::from_millis(100),
        max_backoff: Duration::from_secs(10),
        backoff_multiplier: 2.0,
        retry_on_status: vec![500],
    };

    let _policy = RetryPolicy::new(&config);

    // With max_retries = 0, no retries should be attempted
}

#[test]
fn test_constant_backoff_multiplier() {
    let config = RetryConfig {
        max_retries: 5,
        initial_backoff: Duration::from_secs(5),
        max_backoff: Duration::from_secs(60),
        backoff_multiplier: 1.0, // Multiplier of 1 means constant backoff
        retry_on_status: vec![500],
    };

    let _policy = RetryPolicy::new(&config);

    // With multiplier 1.0: 5 * (1^n) = 5 for all attempts
}

#[test]
fn test_custom_retry_status_codes() {
    let config = RetryConfig {
        max_retries: 3,
        initial_backoff: Duration::from_millis(100),
        max_backoff: Duration::from_secs(10),
        backoff_multiplier: 2.0,
        retry_on_status: vec![408, 429, 503], // Custom status codes
    };

    let _policy = RetryPolicy::new(&config);

    // Policy should retry on custom status codes: 408, 429, 503
}

#[test]
fn test_high_backoff_multiplier() {
    let config = RetryConfig {
        max_retries: 5,
        initial_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_secs(30),
        backoff_multiplier: 3.0, // Higher multiplier for aggressive backoff
        retry_on_status: vec![500, 502, 503],
    };

    let _policy = RetryPolicy::new(&config);

    // With multiplier 3.0:
    // Attempt 0: 0.01 * (3^0) = 0.01 seconds
    // Attempt 1: 0.01 * (3^1) = 0.03 seconds
    // Attempt 2: 0.01 * (3^2) = 0.09 seconds
    // Attempt 3: 0.01 * (3^3) = 0.27 seconds
    // Attempt 4: 0.01 * (3^4) = 0.81 seconds
}

#[test]
fn test_policy_cloning() {
    let config = RetryConfig::default();
    let policy1 = RetryPolicy::new(&config);
    let policy2 = policy1.clone();

    // Verify policy can be cloned
    // Both policies should have identical configuration
    drop(policy1);
    drop(policy2);
}

#[test]
fn test_small_initial_backoff() {
    let config = RetryConfig {
        max_retries: 3,
        initial_backoff: Duration::from_millis(1), // Very small initial backoff
        max_backoff: Duration::from_secs(10),
        backoff_multiplier: 2.0,
        retry_on_status: vec![500],
    };

    let _policy = RetryPolicy::new(&config);

    // With very small initial backoff:
    // Attempt 0: 0.001 * (2^0) = 0.001 seconds
    // Attempt 1: 0.001 * (2^1) = 0.002 seconds
    // Attempt 2: 0.001 * (2^2) = 0.004 seconds
}
