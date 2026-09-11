//! # Schwab Types
//!
//! Type definitions for the Charles Schwab Trading and Market Data APIs.
//!
//! This crate provides strongly-typed Rust structures for all API request and response
//! types used by the Schwab APIs. These types are designed to:
//!
//! - Provide type safety for API interactions
//! - Support serialization/deserialization via serde
//! - Match the official Schwab API documentation
//!
//! ## Modules
//!
//! - [`market_data`] - Quotes, price history, and market data types
//! - [`streaming`] - Real-time streaming WebSocket message types
//! - [`accounts`] - Account information and positions
//! - [`trading`] - Order types, transactions, and trading operations
//! - [`common`] - Shared enums and utility types
//!
//! ## Example
//!
//! Outbound orders use the strict [`trading::OrderRequest`] shape: unknown
//! fields are rejected and prices/quantities are exact decimals.
//!
//! ```
//! use schwab_types::trading::{OrderRequest, OrderType};
//!
//! let order: OrderRequest = serde_json::from_value(serde_json::json!({
//!     "orderStrategyType": "SINGLE",
//!     "orderType": "LIMIT",
//!     "session": "NORMAL",
//!     "duration": "DAY",
//!     "price": "150.25",
//!     "orderLegCollection": [{
//!         "instruction": "BUY",
//!         "quantity": "10",
//!         "instrument": { "symbol": "AAPL", "assetType": "EQUITY" }
//!     }]
//! }))
//! .unwrap();
//!
//! assert_eq!(order.order_type, Some(OrderType::Limit));
//! order.validate().unwrap();
//! ```

/// Market data types including quotes, price history, and market hours.
pub mod market_data;

/// Real-time streaming WebSocket message types and subscription management.
pub mod streaming;

/// Account information, positions, and balances.
pub mod accounts;

/// Trading types including orders, transactions, and execution reports.
pub mod trading;

/// Common types shared across multiple modules.
pub mod common;
