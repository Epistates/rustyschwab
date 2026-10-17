//! Outgoing WebSocket message types for the streaming client.
//!
//! The ack-resolution, heartbeat, and read-loop logic live on
//! `StreamClientInner` in `client.rs` — this module only defines the
//! frame types the writer task understands.

/// Messages that can be sent over the WebSocket
#[derive(Debug, Clone)]
pub enum OutgoingMessage {
    /// Text message (JSON)
    Text(String),
    /// WebSocket Ping frame for heartbeat
    Ping,
}
