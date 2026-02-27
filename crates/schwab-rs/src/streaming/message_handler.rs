//! Message handling and processing for streaming client
//!
//! This module handles incoming WebSocket messages, processes acknowledgments,
//! manages heartbeat/pong monitoring, and routes messages to subscribers.

use crate::error::{Error, Result, StreamError};
use crate::types::streaming::{StreamMessage, StreamResponseCode};
use futures_util::StreamExt;
use futures_util::stream::SplitStream;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::{interval, Instant};
use tracing::{debug, error, info, warn};
use crate::transport::websocket::WsStream;

/// Messages that can be sent over the WebSocket
#[derive(Debug, Clone)]
pub enum OutgoingMessage {
    /// Text message (JSON)
    Text(String),
    /// WebSocket Ping frame for heartbeat
    Ping,
}

/// Wrapper for handling message sending to subscribers
pub struct MessageHandler {
    /// Pending acknowledgments keyed by request ID
    pending_acks: Arc<RwLock<HashMap<String, oneshot::Sender<Result<()>>>>>,
    /// Last time a Pong frame was received
    last_pong: Arc<RwLock<Instant>>,
}

impl MessageHandler {
    /// Create a new message handler
    pub fn new(
        pending_acks: Arc<RwLock<HashMap<String, oneshot::Sender<Result<()>>>>>,
        last_pong: Arc<RwLock<Instant>>,
    ) -> Self {
        Self {
            pending_acks,
            last_pong,
        }
    }

    /// Process an incoming stream message and resolve pending acks
    pub fn process_response(&self, stream_msg: &StreamMessage) -> Result<()> {
        if let StreamMessage::Response(resp) = stream_msg {
            if let Some(item) = resp.response.first() {
                let req_id = item.requestid.clone();
                if let Some(tx) = self.pending_acks.write().remove(&req_id) {
                    let outcome = if let Some(code) = StreamResponseCode::from_code(item.content.code) {
                        if code.is_success() {
                            Ok(())
                        } else {
                            Err(Error::Stream(StreamError::SubscriptionFailed {
                                service: item.service.clone(),
                                code: item.content.code,
                                message: item.content.msg.clone(),
                            }))
                        }
                    } else {
                        Err(Error::Stream(StreamError::SubscriptionFailed {
                            service: item.service.clone(),
                            code: -1,
                            message: "Unknown response code".to_string(),
                        }))
                    };
                    let _ = tx.send(outcome);
                }
            }
        }
        Ok(())
    }

    /// Update the last pong timestamp
    pub fn update_last_pong(&self) {
        *self.last_pong.write() = Instant::now();
        debug!("Pong received, heartbeat alive");
    }

    /// Check if heartbeat timeout has been exceeded
    pub fn is_heartbeat_timeout(&self, timeout: Duration) -> bool {
        let elapsed = self.last_pong.read().elapsed();
        elapsed > timeout
    }
}

/// Start a heartbeat monitoring task
///
/// This spawns a task that periodically sends ping frames and monitors for timeout.
/// If no pong is received within the timeout period, the task will terminate
/// and the connection should be reconnected.
///
/// # Arguments
/// * `interval_duration` - How often to send ping frames
/// * `timeout_duration` - How long to wait for pong before timing out
/// * `outgoing_tx` - Channel to send ping messages through
/// * `last_pong` - Arc to track last pong time
///
/// # Returns
/// A JoinHandle for the spawned heartbeat task
pub fn start_heartbeat(
    interval_duration: Duration,
    timeout_duration: Duration,
    outgoing_tx: Arc<RwLock<Option<tokio::sync::mpsc::UnboundedSender<OutgoingMessage>>>>,
    last_pong: Arc<RwLock<Instant>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut heartbeat = interval(interval_duration);
        heartbeat.tick().await; // Skip first immediate tick

        loop {
            heartbeat.tick().await;

            // Check if last pong was too long ago
            let elapsed = last_pong.read().elapsed();
            if elapsed > timeout_duration {
                error!(
                    "WebSocket ping timeout ({:?} since last pong, limit: {:?}), connection will reconnect",
                    elapsed, timeout_duration
                );
                // Connection will be closed when this task is aborted
                break;
            }

            // Send ping via writer channel
            if let Some(sender) = outgoing_tx.read().clone() {
                if let Err(e) = sender.send(OutgoingMessage::Ping) {
                    warn!("Failed to send ping (connection likely closed): {}", e);
                    break;
                }
                debug!("Sent WebSocket ping (last pong: {:?} ago)", elapsed);
            } else {
                warn!("No active connection for heartbeat");
                break;
            }
        }
    })
}

/// Process incoming WebSocket messages from the stream
///
/// This function runs the main message receiving loop, handling:
/// - Text messages (JSON stream messages)
/// - Pong responses (for heartbeat monitoring)
/// - Close frames (graceful disconnection)
/// - Errors (connection failures)
///
/// # Arguments
/// * `read_half` - The read half of the WebSocket stream
/// * `message_handler` - Handler for processing messages
/// * `message_tx` - Channel to send decoded messages to subscribers
///
/// # Returns
/// `Ok(())` on successful completion or connection close
/// `Err()` on WebSocket error
pub async fn process_message_loop(
    read_half: &mut SplitStream<WsStream>,
    message_handler: &MessageHandler,
    message_tx: &crate::streaming::client::MessageSender,
) -> Result<()> {
    while let Some(msg) = read_half.next().await {
        match msg {
            Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                // In tokio-tungstenite 0.26+, text is Utf8Bytes, convert to String
                if let Ok(stream_msg) = serde_json::from_str::<StreamMessage>(&text.to_string()) {
                    // Resolve acks for response frames
                    let _ = message_handler.process_response(&stream_msg);

                    if let Err(e) = message_tx.send(stream_msg) {
                        warn!("Failed to send message to receiver: {}", e);
                    }
                }
            }
            Ok(tokio_tungstenite::tungstenite::Message::Pong(_)) => {
                message_handler.update_last_pong();
            }
            Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => {
                info!("WebSocket close frame received");
                break;
            }
            Err(e) => {
                error!("WebSocket error: {}", e);
                return Err(Error::WebSocket(e));
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_handler_creation() {
        let pending_acks = Arc::new(RwLock::new(HashMap::new()));
        let last_pong = Arc::new(RwLock::new(Instant::now()));
        let handler = MessageHandler::new(pending_acks, last_pong);
        assert!(!handler.is_heartbeat_timeout(Duration::from_secs(1)));
    }

    #[test]
    fn test_pong_update() {
        let pending_acks = Arc::new(RwLock::new(HashMap::new()));
        let last_pong = Arc::new(RwLock::new(Instant::now() - Duration::from_secs(10)));
        let handler = MessageHandler::new(pending_acks, last_pong);

        // Should timeout before update
        assert!(handler.is_heartbeat_timeout(Duration::from_secs(1)));

        // Update pong and check again
        handler.update_last_pong();
        assert!(!handler.is_heartbeat_timeout(Duration::from_secs(1)));
    }
}
