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
//! ```ignore
//! use schwab_types::{Order, OrderType, Duration, Session};
//!
//! let order = Order {
//!     order_type: OrderType::Limit,
//!     session: Session::Normal,
//!     duration: Duration::Day,
//!     // ... other fields
//! };
//! ```

#![warn(missing_docs)]

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

pub use market_data::*;
pub use streaming::*;
pub use accounts::*;
pub use trading::*;
pub use common::*;
