//! Utility functions for working with the Schwab API
//!
//! This module provides helper functions for common operations like time formatting,
//! list formatting, and other utilities used throughout the library.

use chrono::{DateTime, Utc};

/// Time format options for API requests
///
/// Different Schwab API endpoints require different time formats. This enum
/// provides standardized conversion from DateTime to the required format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeFormat {
    /// ISO 8601 format: "2024-10-26T15:30:00Z"
    ///
    /// Used by most trading endpoints (orders, transactions)
    Iso8601,

    /// Unix epoch timestamp in seconds: 1698340200
    ///
    /// Used by some historical data endpoints
    Epoch,

    /// Unix epoch timestamp in milliseconds: 1698340200000
    ///
    /// Used by price history endpoint
    EpochMs,

    /// Date only format: "2024-10-26"
    ///
    /// Used by market hours and option chains
    YyyyMmDd,
}

/// Convert DateTime to the specified format
///
/// # Arguments
/// * `dt` - Optional DateTime to convert (None returns None)
/// * `format` - Target time format
///
/// # Returns
/// * `Some(String)` - Formatted time string
/// * `None` - If input was None (useful for optional parameters)
///
/// # Example
/// ```
/// use schwab_rs::utils::{format_time, TimeFormat};
/// use chrono::Utc;
///
/// let now = Utc::now();
///
/// // ISO 8601
/// let iso = format_time(Some(now), TimeFormat::Iso8601);
/// assert!(iso.unwrap().ends_with('Z'));
///
/// // Unix epoch (seconds)
/// let epoch = format_time(Some(now), TimeFormat::Epoch);
/// assert!(epoch.unwrap().parse::<i64>().is_ok());
///
/// // Unix epoch (milliseconds)
/// let epoch_ms = format_time(Some(now), TimeFormat::EpochMs);
/// assert!(epoch_ms.unwrap().parse::<i64>().is_ok());
///
/// // Date only
/// let date = format_time(Some(now), TimeFormat::YyyyMmDd);
/// assert!(date.unwrap().contains('-'));
/// ```
pub fn format_time(dt: Option<DateTime<Utc>>, format: TimeFormat) -> Option<String> {
    dt.map(|d| match format {
        TimeFormat::Iso8601 => {
            // Python: f"{dt.isoformat().split('+')[0][:-3]}Z"
            // Strips microseconds (keeps only milliseconds) and adds 'Z'
            let iso = d.to_rfc3339();

            // Split at '+' (timezone marker) to get timestamp part
            let timestamp = iso
                .split('+')
                .next()
                .unwrap_or(&iso) // Always safe: split() yields at least one element
                .split('Z')
                .next()
                .unwrap_or(&iso); // Fallback to original if split fails

            // Remove microseconds (keep only milliseconds): "2024-10-26T15:30:00.123456" → "2024-10-26T15:30:00.123"
            if let Some(dot_pos) = timestamp.rfind('.') {
                let (base, micros) = timestamp.split_at(dot_pos);
                format!("{}{:.3}Z", base, &micros[..4.min(micros.len())])
            } else {
                format!("{}Z", timestamp)
            }
        }
        TimeFormat::Epoch => {
            // Unix epoch in seconds
            d.timestamp().to_string()
        }
        TimeFormat::EpochMs => {
            // Unix epoch in milliseconds
            d.timestamp_millis().to_string()
        }
        TimeFormat::YyyyMmDd => {
            // Date only: "2024-10-26"
            d.format("%Y-%m-%d").to_string()
        }
    })
}

/// Format a list of strings as a comma-separated string
///
/// Converts a Rust `Vec<String>` to a comma-separated string suitable for
/// API query parameters. This is a common pattern in the Schwab API where
/// multiple values are passed as "VAL1,VAL2,VAL3".
///
/// # Arguments
/// * `list` - Vector of strings to format
///
/// # Returns
/// * Comma-separated string
///
/// # Example
/// ```
/// use schwab_rs::utils::format_list;
///
/// let symbols = vec!["AAPL".to_string(), "MSFT".to_string(), "GOOGL".to_string()];
/// let formatted = format_list(&symbols);
/// assert_eq!(formatted, "AAPL,MSFT,GOOGL");
///
/// // Empty list
/// let empty: Vec<String> = vec![];
/// assert_eq!(format_list(&empty), "");
/// ```
pub fn format_list(list: &[String]) -> String {
    list.join(",")
}

/// Format a slice of string references as a comma-separated string
///
/// Convenience function for formatting &str slices.
///
/// # Example
/// ```
/// use schwab_rs::utils::format_list_str;
///
/// let symbols = ["AAPL", "MSFT", "GOOGL"];
/// let formatted = format_list_str(&symbols);
/// assert_eq!(formatted, "AAPL,MSFT,GOOGL");
/// ```
pub fn format_list_str(list: &[&str]) -> String {
    list.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    #[test]
    fn test_format_time_iso8601() {
        // 2024-10-26 15:30:00.123456 UTC
        let dt = Utc.with_ymd_and_hms(2024, 10, 26, 15, 30, 0).unwrap()
            .with_nanosecond(123456789).unwrap();

        let result = format_time(Some(dt), TimeFormat::Iso8601);
        assert!(result.is_some());
        let formatted = result.unwrap();

        // Should start with date
        assert!(formatted.starts_with("2024-10-26T"));
        // Should end with Z
        assert!(formatted.ends_with('Z'));
        // Should contain time
        assert!(formatted.contains("15:30:00"));
    }

    #[test]
    fn test_format_time_epoch() {
        let dt = Utc.with_ymd_and_hms(2024, 10, 26, 15, 30, 0).unwrap();
        let result = format_time(Some(dt), TimeFormat::Epoch);
        assert!(result.is_some());

        let timestamp = result.unwrap().parse::<i64>().unwrap();
        assert!(timestamp > 1_700_000_000); // After 2023
        assert!(timestamp < 2_000_000_000); // Before 2033
    }

    #[test]
    fn test_format_time_epoch_ms() {
        let dt = Utc.with_ymd_and_hms(2024, 10, 26, 15, 30, 0).unwrap();
        let result = format_time(Some(dt), TimeFormat::EpochMs);
        assert!(result.is_some());

        let timestamp = result.unwrap().parse::<i64>().unwrap();
        assert!(timestamp > 1_700_000_000_000); // After 2023 in milliseconds
        assert!(timestamp < 2_000_000_000_000); // Before 2033 in milliseconds
    }

    #[test]
    fn test_format_time_yyyy_mm_dd() {
        let dt = Utc.with_ymd_and_hms(2024, 10, 26, 15, 30, 0).unwrap();
        let result = format_time(Some(dt), TimeFormat::YyyyMmDd);
        assert_eq!(result, Some("2024-10-26".to_string()));
    }

    #[test]
    fn test_format_time_none() {
        let result = format_time(None, TimeFormat::Iso8601);
        assert!(result.is_none());
    }

    #[test]
    fn test_format_list() {
        let symbols = vec!["AAPL".to_string(), "MSFT".to_string(), "GOOGL".to_string()];
        assert_eq!(format_list(&symbols), "AAPL,MSFT,GOOGL");

        let empty: Vec<String> = vec![];
        assert_eq!(format_list(&empty), "");

        let single = vec!["TSLA".to_string()];
        assert_eq!(format_list(&single), "TSLA");
    }

    #[test]
    fn test_format_list_str() {
        let symbols = ["AAPL", "MSFT", "GOOGL"];
        assert_eq!(format_list_str(&symbols), "AAPL,MSFT,GOOGL");

        let empty: &[&str] = &[];
        assert_eq!(format_list_str(empty), "");

        let single = ["TSLA"];
        assert_eq!(format_list_str(&single), "TSLA");
    }
}
