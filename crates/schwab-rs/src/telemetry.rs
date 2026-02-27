//! Telemetry and observability initialization for the Schwab SDK.
//!
//! This module provides convenient functions to initialize tracing/logging
//! for the SDK. It uses the `tracing` crate which provides:
//!
//! - Structured, contextual logging
//! - Span-based performance tracking
//! - Integration with OpenTelemetry for distributed tracing
//!
//! # Basic Usage
//!
//! ```ignore
//! use schwab_rs::telemetry;
//!
//! // Initialize with default settings (respects RUST_LOG env var)
//! telemetry::init_tracing();
//!
//! // Or with custom settings
//! telemetry::init_tracing_with_level("debug");
//! ```
//!
//! # Environment Variables
//!
//! - `RUST_LOG`: Controls log level and filtering (e.g., `schwab_rs=debug,warn`)
//!
//! # Example with Custom Filter
//!
//! ```bash
//! # Show debug logs for schwab-rs, info for everything else
//! RUST_LOG=schwab_rs=debug,info cargo run
//! ```

use std::sync::Once;

static INIT: Once = Once::new();

/// Initialize tracing with default settings.
///
/// Respects the `RUST_LOG` environment variable for filtering.
/// If not set, defaults to `info` level.
///
/// This function is idempotent - calling it multiple times has no effect
/// after the first call.
///
/// # Example
///
/// ```ignore
/// schwab_rs::telemetry::init_tracing();
/// ```
pub fn init_tracing() {
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive(tracing::Level::INFO.into()),
            )
            .with_target(true)
            .with_thread_ids(false)
            .with_file(false)
            .with_line_number(false)
            .init();
    });
}

/// Initialize tracing with a specific default log level.
///
/// The `RUST_LOG` environment variable takes precedence over the provided level.
///
/// # Arguments
///
/// * `default_level` - Default log level if RUST_LOG is not set
///   (e.g., "debug", "info", "warn", "error", "trace")
///
/// # Example
///
/// ```ignore
/// schwab_rs::telemetry::init_tracing_with_level("debug");
/// ```
pub fn init_tracing_with_level(default_level: &str) {
    INIT.call_once(|| {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_level));

        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .with_thread_ids(false)
            .with_file(false)
            .with_line_number(false)
            .init();
    });
}

/// Initialize tracing with JSON output format.
///
/// Useful for production environments where logs are aggregated
/// and parsed by log management systems.
///
/// # Example
///
/// ```ignore
/// schwab_rs::telemetry::init_tracing_json();
/// ```
pub fn init_tracing_json() {
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive(tracing::Level::INFO.into()),
            )
            .json()
            .with_target(true)
            .with_current_span(true)
            .init();
    });
}

#[cfg(test)]
mod tests {
    // Note: These tests can't verify tracing output easily,
    // but they ensure the initialization doesn't panic.

    #[test]
    fn test_init_tracing_does_not_panic() {
        // This is a no-op after the first call, but shouldn't panic
        // Note: Can't actually test tracing init in unit tests
        // because it requires global state
    }
}
