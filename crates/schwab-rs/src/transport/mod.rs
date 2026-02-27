//! HTTP and WebSocket transport layer.
//!
//! Low-level network transports for REST and streaming APIs.

#![allow(missing_docs)] // Internal transport implementation

pub mod http;
pub mod websocket;

pub use http::HttpTransport;
pub use websocket::WebSocketTransport;