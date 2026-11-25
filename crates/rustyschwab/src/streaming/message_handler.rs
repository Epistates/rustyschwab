//! Outgoing WebSocket message types for the streaming client.
//!
//! The ack-resolution, heartbeat, and read-loop logic live on
//! `StreamClientInner` in `client.rs` — this module only defines the
//! messages the client queues for the writer task.

/// Messages that can be sent over the WebSocket
#[derive(Clone)]
pub enum OutgoingMessage {
    /// Text message (JSON)
    Text(String),
    /// WebSocket Ping frame for heartbeat
    Ping,
}

impl OutgoingMessage {
    /// The WebSocket frame the writer task sends for this message.
    pub(crate) fn into_frame(self) -> tokio_tungstenite::tungstenite::Message {
        use tokio_tungstenite::tungstenite::Message;
        match self {
            Self::Text(text) => Message::Text(text.into()),
            Self::Ping => Message::Ping(Vec::new().into()),
        }
    }
}

/// A text frame may be a LOGIN carrying the access token, so `Debug` shows
/// its size only.
impl std::fmt::Debug for OutgoingMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(text) => write!(f, "Text({} bytes)", text.len()),
            Self::Ping => f.write_str("Ping"),
        }
    }
}
