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
//! fields are rejected and prices/quantities are exact decimals. Build one
//! with the constructors or deserialize the JSON Schwab documents.
//!
//! ```
//! use schwab_types::Decimal;
//! use schwab_types::trading::{OrderInstruction, OrderRequest, OrderRequestLeg, OrderType};
//!
//! let order = OrderRequest::limit(
//!     OrderRequestLeg::equity(OrderInstruction::Buy, "AAPL", 10),
//!     Decimal::new(15025, 2),
//! );
//! assert_eq!(order.order_type, Some(OrderType::Limit));
//! order.validate().unwrap();
//!
//! let same: OrderRequest = serde_json::from_value(serde_json::json!({
//!     "orderStrategyType": "SINGLE",
//!     "orderType": "LIMIT",
//!     "session": "NORMAL",
//!     "duration": "DAY",
//!     "price": "150.25",
//!     "orderLegCollection": [{
//!         "instruction": "BUY",
//!         "quantity": 10,
//!         "instrument": { "symbol": "AAPL", "assetType": "EQUITY" }
//!     }]
//! }))
//! .unwrap();
//! assert_eq!(serde_json::to_value(&same).unwrap(), serde_json::to_value(&order).unwrap());
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

/// Typed errors from request validation.
pub mod validation;
pub use validation::{ValidationError, ValidationErrorKind};

/// The exact-decimal type prices and quantities use, so callers do not need
/// their own `rust_decimal` dependency to build orders.
pub use rust_decimal::Decimal;
