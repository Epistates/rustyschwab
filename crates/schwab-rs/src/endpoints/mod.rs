//! REST API endpoint implementations.
//!
//! Provides typed methods for all Schwab API endpoints including accounts,
//! trading, market data, quotes, options, and instruments.

#![allow(missing_docs)] // Endpoint methods documented via Schwab API docs

pub mod accounts;
pub mod instruments;
pub mod market_data;
pub mod movers;
pub mod options;
pub mod price_history;
pub mod quotes;
pub mod trading;