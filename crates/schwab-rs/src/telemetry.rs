//! Telemetry and observability initialization for the Schwab SDK.
//!
//! This module provides convenient functions to initialize tracing/logging
//! for the SDK. It uses the `tracing` crate which provides:
//!
//! - Structured, contextual logging
//! - Span-based performance tracking
//!
//! Applications that export traces install their own subscriber instead;
//! these helpers then leave it in place. With the `otel` feature,
//! [`init_otel`] is that subscriber for OTLP: it exports spans to whatever
//! the standard `OTEL_EXPORTER_OTLP_*` environment names, and behaves like
//! the helpers below when it names nothing.
//!
//! # Basic Usage
//!
//! ```no_run
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

#[cfg(feature = "otel")]
mod otel;
#[cfg(feature = "otel")]
pub use otel::{OpenTelemetryRecorder, OtelError, OtelGuard, init_otel};

/// `RUST_LOG` when set and valid, else `default`.
fn filter(default: &str) -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default))
}

/// Initialize tracing with default settings.
///
/// Respects the `RUST_LOG` environment variable for filtering.
/// If not set, defaults to `info` level.
///
/// Idempotent, and harmless when the application already installed a
/// global subscriber: that one is kept.
///
/// # Example
///
/// ```no_run
/// schwab_rs::telemetry::init_tracing();
/// ```
pub fn init_tracing() {
    init_tracing_with_level("info");
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
/// ```no_run
/// schwab_rs::telemetry::init_tracing_with_level("debug");
/// ```
pub fn init_tracing_with_level(default_level: &str) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter(default_level))
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .try_init();
}

/// Initialize tracing with JSON output format.
///
/// Useful for production environments where logs are aggregated
/// and parsed by log management systems.
///
/// # Example
///
/// ```no_run
/// schwab_rs::telemetry::init_tracing_json();
/// ```
pub fn init_tracing_json() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter("info"))
        .json()
        .with_target(true)
        .with_current_span(true)
        .try_init();
}
