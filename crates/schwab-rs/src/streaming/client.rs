//! WebSocket streaming client for real-time market data.
//!
//! Provides subscription-based access to quotes, charts, and account activity.

#![allow(missing_docs)] // Internal streaming implementation

use crate::auth::AuthManager;
use crate::config::{ChannelKind, StreamConfig};
use crate::error::{Error, Result, StreamError};
use crate::streaming::subscription::SubscriptionManager;
use crate::streaming::message_handler::OutgoingMessage;
use crate::transport::websocket::{WebSocketTransport, WsStream};
use crate::types::streaming::*;
use futures_util::{SinkExt, StreamExt};
use futures_util::stream::SplitStream;
use parking_lot::RwLock;
use std::collections::HashMap;
use serde_json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{interval, sleep, Instant};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Wrapper for bounded or unbounded sender
///
/// Provides a unified interface for sending messages to subscribers regardless
/// of whether the underlying channel is bounded or unbounded.
#[derive(Debug)]
pub enum MessageSender {
    /// Unbounded sender (no backpressure)
    Unbounded(mpsc::UnboundedSender<StreamMessage>),
    /// Bounded sender (applies backpressure when buffer full)
    Bounded(mpsc::Sender<StreamMessage>),
}

impl MessageSender {
    /// Send a message, handling both bounded and unbounded cases
    ///
    /// Returns `Err` if the receiver has been dropped or the bounded channel is full.
    pub fn send(&self, msg: StreamMessage) -> std::result::Result<(), String> {
        match self {
            MessageSender::Unbounded(tx) => tx.send(msg).map_err(|e| e.to_string()),
            MessageSender::Bounded(tx) => tx.try_send(msg).map_err(|e| e.to_string()),
        }
    }
}

/// Wrapper for bounded or unbounded receiver
///
/// Provides a unified interface for receiving streaming messages regardless of
/// whether the underlying channel is bounded or unbounded.
#[derive(Debug)]
pub enum MessageReceiver {
    /// Unbounded receiver (no backpressure)
    Unbounded(mpsc::UnboundedReceiver<StreamMessage>),
    /// Bounded receiver (backpressure when buffer full)
    Bounded(mpsc::Receiver<StreamMessage>),
}

impl MessageReceiver {
    /// Receive a message, handling both bounded and unbounded cases
    ///
    /// Returns `None` when the channel is closed and all messages have been received.
    pub async fn recv(&mut self) -> Option<StreamMessage> {
        match self {
            MessageReceiver::Unbounded(rx) => rx.recv().await,
            MessageReceiver::Bounded(rx) => rx.recv().await,
        }
    }
}

pub type StreamMessage = crate::types::streaming::StreamMessage;

#[derive(Clone, Debug)]
pub struct StreamClient {
    inner: Arc<StreamClientInner>,
}

#[derive(Debug)]
struct StreamClientInner {
    config: StreamConfig,
    #[allow(dead_code)]
    auth_manager: AuthManager,
    #[allow(dead_code)]
    transport: WebSocketTransport,
    subscriptions: Arc<SubscriptionManager>,
    #[allow(dead_code)]
    message_tx: MessageSender,
    message_rx: Arc<RwLock<Option<MessageReceiver>>>,
    connection_state: Arc<RwLock<ConnectionState>>,
    customer_id: String,
    correl_id: String,
    /// Track connection start time for 90-second crash detection
    connection_start: Arc<RwLock<Option<Instant>>>,
    /// Exponential backoff delay (starts at 2s, caps at 128s)
    backoff_delay: Arc<RwLock<Duration>>,
    /// Sender for outgoing WebSocket messages (text and ping) - swapped on each connection
    outgoing_tx: Arc<RwLock<Option<mpsc::UnboundedSender<OutgoingMessage>>>>,
    /// Writer task handle to allow clean abort on disconnect
    #[allow(dead_code)]
    writer_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    /// Pending acks keyed by request id
    pending_acks: Arc<RwLock<HashMap<String, oneshot::Sender<Result<()>>>>>,
    /// Last time a Pong frame was received (for heartbeat timeout detection)
    last_pong: Arc<RwLock<Instant>>,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // All states defined for completeness, Reconnecting reserved for future auto-reconnect
enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Authenticated,
    Reconnecting,
}

impl StreamClient {
    pub fn new(
        config: StreamConfig,
        auth_manager: AuthManager,
        customer_id: String,
        correl_id: String,
    ) -> Result<Self> {
        let transport = WebSocketTransport::new(&config.websocket_url)?;

        // Create channels based on config
        let (message_tx, message_rx) = match config.channel_kind {
            ChannelKind::Unbounded => {
                let (tx, rx) = mpsc::unbounded_channel();
                (MessageSender::Unbounded(tx), MessageReceiver::Unbounded(rx))
            }
            ChannelKind::Bounded(size) => {
                let (tx, rx) = mpsc::channel(size);
                (MessageSender::Bounded(tx), MessageReceiver::Bounded(rx))
            }
        };

        Ok(Self {
            inner: Arc::new(StreamClientInner {
                config,
                auth_manager,
                transport,
                subscriptions: Arc::new(SubscriptionManager::new()),
                message_tx,
                message_rx: Arc::new(RwLock::new(Some(message_rx))),
                connection_state: Arc::new(RwLock::new(ConnectionState::Disconnected)),
                customer_id,
                correl_id,
                connection_start: Arc::new(RwLock::new(None)),
                backoff_delay: Arc::new(RwLock::new(Duration::from_secs(2))), // Initial backoff delay
                outgoing_tx: Arc::new(RwLock::new(None)),
                writer_task: Arc::new(RwLock::new(None)),
                pending_acks: Arc::new(RwLock::new(HashMap::new())),
                last_pong: Arc::new(RwLock::new(Instant::now())),
            }),
        })
    }

    /// Convenience: subscribe to Level One Equities for given symbols
    pub async fn subscribe_level_one_equities(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.subscribe(StreamService::LeveloneEquities, syms).await
    }

    /// Convenience: unsubscribe from Level One Equities for given symbols
    pub async fn unsubscribe_level_one_equities(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.unsubscribe(StreamService::LeveloneEquities, syms).await
    }

    /// Convenience: subscribe to Level One Options
    pub async fn subscribe_level_one_options(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.subscribe(StreamService::LeveloneOptions, syms).await
    }

    pub async fn unsubscribe_level_one_options(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.unsubscribe(StreamService::LeveloneOptions, syms).await
    }

    /// Convenience: subscribe to Level One Futures
    pub async fn subscribe_level_one_futures(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.subscribe(StreamService::LeveloneFutures, syms).await
    }

    pub async fn unsubscribe_level_one_futures(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.unsubscribe(StreamService::LeveloneFutures, syms).await
    }

    /// Convenience: subscribe to Level One Futures Options
    pub async fn subscribe_level_one_futures_options(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.subscribe(StreamService::LeveloneFuturesOptions, syms).await
    }

    pub async fn unsubscribe_level_one_futures_options(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.unsubscribe(StreamService::LeveloneFuturesOptions, syms).await
    }

    /// Convenience: subscribe to Level One Forex
    pub async fn subscribe_level_one_forex(&self, pairs: &[&str]) -> Result<()> {
        let syms: Vec<String> = pairs.iter().map(|s| s.to_string()).collect();
        self.subscribe(StreamService::LeveloneForex, syms).await
    }

    pub async fn unsubscribe_level_one_forex(&self, pairs: &[&str]) -> Result<()> {
        let syms: Vec<String> = pairs.iter().map(|s| s.to_string()).collect();
        self.unsubscribe(StreamService::LeveloneForex, syms).await
    }

    /// Convenience: subscribe to order book services
    pub async fn subscribe_nyse_book(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.subscribe(StreamService::NyseBook, syms).await
    }

    pub async fn unsubscribe_nyse_book(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.unsubscribe(StreamService::NyseBook, syms).await
    }

    pub async fn subscribe_nasdaq_book(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.subscribe(StreamService::NasdaqBook, syms).await
    }

    pub async fn unsubscribe_nasdaq_book(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.unsubscribe(StreamService::NasdaqBook, syms).await
    }

    pub async fn subscribe_options_book(&self, contracts: &[&str]) -> Result<()> {
        let syms: Vec<String> = contracts.iter().map(|s| s.to_string()).collect();
        self.subscribe(StreamService::OptionsBook, syms).await
    }

    pub async fn unsubscribe_options_book(&self, contracts: &[&str]) -> Result<()> {
        let syms: Vec<String> = contracts.iter().map(|s| s.to_string()).collect();
        self.unsubscribe(StreamService::OptionsBook, syms).await
    }

    /// Convenience: subscribe to chart services
    pub async fn subscribe_chart_equity(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.subscribe(StreamService::ChartEquity, syms).await
    }

    pub async fn unsubscribe_chart_equity(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.unsubscribe(StreamService::ChartEquity, syms).await
    }

    pub async fn subscribe_chart_futures(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.subscribe(StreamService::ChartFutures, syms).await
    }

    pub async fn unsubscribe_chart_futures(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.unsubscribe(StreamService::ChartFutures, syms).await
    }

    /// Convenience: subscribe to screeners
    pub async fn subscribe_screener_equity(&self, keys: &[&str]) -> Result<()> {
        let syms: Vec<String> = keys.iter().map(|s| s.to_string()).collect();
        self.subscribe(StreamService::ScreenerEquity, syms).await
    }

    pub async fn unsubscribe_screener_equity(&self, keys: &[&str]) -> Result<()> {
        let syms: Vec<String> = keys.iter().map(|s| s.to_string()).collect();
        self.unsubscribe(StreamService::ScreenerEquity, syms).await
    }

    pub async fn subscribe_screener_option(&self, keys: &[&str]) -> Result<()> {
        let syms: Vec<String> = keys.iter().map(|s| s.to_string()).collect();
        self.subscribe(StreamService::ScreenerOption, syms).await
    }

    pub async fn unsubscribe_screener_option(&self, keys: &[&str]) -> Result<()> {
        let syms: Vec<String> = keys.iter().map(|s| s.to_string()).collect();
        self.unsubscribe(StreamService::ScreenerOption, syms).await
    }

    /// Convenience: account activity (fixed key)
    pub async fn subscribe_account_activity(&self) -> Result<()> {
        self.subscribe(StreamService::AcctActivity, vec!["Account Activity".to_string()]).await
    }

    pub async fn unsubscribe_account_activity(&self) -> Result<()> {
        self.unsubscribe(StreamService::AcctActivity, vec!["Account Activity".to_string()]).await
    }

    pub fn builder() -> StreamClientBuilder {
        StreamClientBuilder::new()
    }

    /// Set service-specific field IDs (comma-separated) to be used on subscriptions
    pub fn set_service_fields(&self, service: StreamService, fields_csv: String) {
        self.inner
            .subscriptions
            .set_service_fields(service.as_str().to_string(), fields_csv);
    }

    pub async fn connect(&self) -> Result<()> {
        self.set_state(ConnectionState::Connecting);

        let inner = self.inner.clone();
        tokio::spawn(async move {
            if let Err(e) = inner.connection_loop().await {
                error!("Connection loop failed: {}", e);
            }
        });

        // Wait for authentication
        let start = Instant::now();
        let timeout = Duration::from_secs(10);

        while start.elapsed() < timeout {
            if self.is_authenticated() {
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }

        Err(Error::Stream(StreamError::ConnectionFailed(
            "Authentication timeout".to_string(),
        )))
    }

    pub async fn subscribe(&self, service: StreamService, symbols: Vec<String>) -> Result<()> {
        if !self.is_authenticated() {
            return Err(Error::Stream(StreamError::ConnectionFailed(
                "Not authenticated".to_string(),
            )));
        }

        let request_id = Uuid::new_v4().to_string();
        let fields = self.inner
            .subscriptions
            .get_service_fields(service.as_str())
            .unwrap_or_else(|| match service {
                StreamService::LeveloneEquities => LevelOneEquityFields::default_fields(),
                StreamService::LeveloneOptions => LevelOneOptionFields::default_fields(),
                StreamService::ChartEquity => ChartEquityFields::default_fields(),
                StreamService::AcctActivity => AcctActivityFields::default_fields(),
                // Fallback or other services
                _ => LevelOneEquityFields::default_fields(),
            });

        let request = StreamRequest {
            requestid: request_id.clone(),
            service: service.as_str().to_string(),
            command: StreamCommand::Subs,
            schwab_client_customer_id: self.inner.customer_id.clone(),
            schwab_client_correl_id: self.inner.correl_id.clone(),
            parameters: StreamParameters {
                keys: Some(symbols.join(",")),
                fields: Some(fields.clone()),
                authorization: None,
                schwab_client_channel: None,
                schwab_client_function_id: None,
            },
        };

        self.inner.subscriptions.add_subscription(
            service.as_str().to_string(),
            symbols.clone(),
            fields,
        );

        self.send_request_with_retry(request).await
    }

    pub async fn unsubscribe(&self, service: StreamService, symbols: Vec<String>) -> Result<()> {
        if !self.is_authenticated() {
            return Err(Error::Stream(StreamError::ConnectionFailed(
                "Not authenticated".to_string(),
            )));
        }

        let request_id = Uuid::new_v4().to_string();

        let request = StreamRequest {
            requestid: request_id.clone(),
            service: service.as_str().to_string(),
            command: StreamCommand::Unsubs,
            schwab_client_customer_id: self.inner.customer_id.clone(),
            schwab_client_correl_id: self.inner.correl_id.clone(),
            parameters: StreamParameters {
                keys: Some(symbols.join(",")),
                fields: None,
                authorization: None,
                schwab_client_channel: None,
                schwab_client_function_id: None,
            },
        };

        self.inner.subscriptions.remove_subscription(
            service.as_str().to_string(),
            symbols,
        );

        self.send_request_with_retry(request).await
    }

    /// Get the message receiver for streaming messages
    ///
    /// Returns the receiver for reading streaming messages. Can only be called once
    /// as it takes ownership of the receiver.
    ///
    /// The receiver type (bounded or unbounded) is determined by the `channel_kind`
    /// configuration setting.
    pub fn get_receiver(&self) -> Option<MessageReceiver> {
        self.inner.message_rx.write().take()
    }

    async fn send_request(&self, request: StreamRequest) -> Result<()> {
        // Track ack
        let (tx, rx) = oneshot::channel::<Result<()>>();
        {
            self.inner
                .pending_acks
                .write()
                .insert(request.requestid.clone(), tx);
        }

        let json = serde_json::to_string(&StreamRequests { requests: vec![request] })?;
        if let Some(sender) = self.inner.outgoing_tx.read().clone() {
            sender
                .send(OutgoingMessage::Text(json))
                .map_err(|e| Error::Stream(StreamError::ConnectionFailed(format!("Failed to queue request: {}", e))))?
        } else {
            return Err(Error::Stream(StreamError::ConnectionFailed("No active connection".to_string())));
        }
        // Wait briefly for ack (non-blocking long-term; fast-fail only)
        let timeout = Duration::from_secs(5);
        let result = tokio::time::timeout(timeout, rx).await;
        match result {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_canceled)) => Err(Error::Stream(StreamError::ConnectionFailed("Ack channel canceled".to_string()))),
            Err(_elapsed) => {
                warn!("Stream request ack timeout");
                Ok(())
            }
        }
    }

    async fn send_request_with_retry(&self, request: StreamRequest) -> Result<()> {
        let reconnect = &self.inner.config.reconnect;
        let mut attempt: usize = 0;
        let mut delay = reconnect.initial_backoff;

        loop {
            match self.send_request(request.clone()).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    // Retry on connection issues or subscription acks failing
                    let is_retryable = matches!(
                        &e,
                        Error::ConnectionClosed
                            | Error::WebSocket(_)
                            | Error::Stream(StreamError::ConnectionFailed(_))
                            | Error::Stream(StreamError::SubscriptionFailed { .. })
                    );

                    if !reconnect.enabled || !is_retryable {
                        return Err(e);
                    }

                    if let Some(max) = reconnect.max_retries {
                        if attempt >= max { return Err(e); }
                    }

                    warn!("Stream request retrying in {:?} (attempt {})", delay, attempt + 1);
                    sleep(delay).await;
                    let next = (delay.mul_f64(reconnect.backoff_multiplier))
                        .min(reconnect.max_backoff);
                    delay = next;
                    attempt += 1;
                }
            }
        }
    }

    fn set_state(&self, state: ConnectionState) {
        *self.inner.connection_state.write() = state;
    }

    fn is_authenticated(&self) -> bool {
        *self.inner.connection_state.read() == ConnectionState::Authenticated
    }
}

impl StreamClientInner {
    async fn connection_loop(&self) -> Result<()> {
        let mut retry_count = 0;

        loop {
            // Record connection start time for 90-second crash detection
            let connection_start = Instant::now();
            *self.connection_start.write() = Some(connection_start);

            match self.connect_and_run().await {
                Ok(_) => {
                    info!("Stream connection closed normally");
                    // Reset backoff on successful connection
                    *self.backoff_delay.write() = Duration::from_secs(2);
                    break;
                }
                Err(e) => {
                    error!("Stream connection error: {}", e);
                    
                    // Check for 90-second crash detection
                    let elapsed = connection_start.elapsed();
                    if elapsed <= Duration::from_secs(90) {
                        warn!("Stream crashed within 90 seconds, likely no subscriptions, invalid login, or lost connection (not restarting)");
                        return Err(Error::Stream(StreamError::ConnectionFailed(
                            "Stream failed within 90 seconds of connecting".to_string(),
                        )));
                    }

                    if !self.config.reconnect.enabled {
                        return Err(e);
                    }

                    if let Some(max_retries) = self.config.reconnect.max_retries {
                        if retry_count >= max_retries {
                            error!("Maximum reconnection attempts reached");
                            return Err(Error::Stream(StreamError::ConnectionFailed(
                                "Maximum reconnection attempts exceeded".to_string(),
                            )));
                        }
                    }

                    let backoff = *self.backoff_delay.read();
                    warn!("Stream connection lost, reconnecting in {:?} (attempt {})", backoff, retry_count + 1);
                    sleep(backoff).await;

                    // Exponential backoff: double the delay, cap at 128 seconds
                    let mut delay = self.backoff_delay.write();
                    *delay = (*delay * 2).min(Duration::from_secs(128));
                    
                    retry_count += 1;
                }
            }
        }

        Ok(())
    }

    async fn connect_and_run(&self) -> Result<()> {
        *self.connection_state.write() = ConnectionState::Connecting;

        let ws_stream = self.transport.connect().await?;
        info!("WebSocket connected");

        *self.connection_state.write() = ConnectionState::Connected;

        // Reset last_pong timestamp for new connection
        *self.last_pong.write() = Instant::now();

        // Create a writer task that forwards messages from a channel to the socket
        let (tx, mut rx) = mpsc::unbounded_channel::<OutgoingMessage>();
        *self.outgoing_tx.write() = Some(tx);

        let (mut write_half, mut read_half) = ws_stream.split();
        let writer = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let ws_msg = match msg {
                    // In tokio-tungstenite 0.26+, Message::Text accepts Into<Utf8Bytes>
                    OutgoingMessage::Text(text) => tokio_tungstenite::tungstenite::Message::Text(text.into()),
                    OutgoingMessage::Ping => tokio_tungstenite::tungstenite::Message::Ping(vec![].into()),
                };

                if let Err(e) = write_half.send(ws_msg).await {
                    error!("WebSocket write error: {}", e);
                    break;
                }
            }
        });
        *self.writer_task.write() = Some(writer);

        // Authenticate using read half
        self.authenticate_read(&mut read_half).await?;
        *self.connection_state.write() = ConnectionState::Authenticated;

        // Resubscribe to existing subscriptions using channel
        self.resubscribe_via_channel().await?;

        // Start heartbeat
        let heartbeat_task = self.start_heartbeat();

        // Message handling loop
        let result = self.message_loop_read(&mut read_half).await;

        heartbeat_task.abort();
        if let Some(handle) = self.writer_task.write().take() { handle.abort(); }
        *self.outgoing_tx.write() = None;

        *self.connection_state.write() = ConnectionState::Disconnected;

        result
    }

    // Legacy authenticate kept for compatibility; not used in new connect path
    // Authenticate using the read half; login is sent via writer channel
    async fn authenticate_read(&self, read_half: &mut SplitStream<WsStream>) -> Result<()> {
        let access_token = self.auth_manager.get_access_token().await
            .map_err(Error::Auth)?;

        let login_request = StreamRequest {
            requestid: "1".to_string(),
            service: "ADMIN".to_string(),
            command: StreamCommand::Login,
            schwab_client_customer_id: self.customer_id.clone(),
            schwab_client_correl_id: self.correl_id.clone(),
            parameters: StreamParameters {
                keys: None,
                fields: None,
                authorization: Some(access_token),
                schwab_client_channel: Some("N9".to_string()),
                schwab_client_function_id: Some("APIAPP".to_string()),
            },
        };

        if let Some(sender) = self.outgoing_tx.read().clone() {
            let request_json = serde_json::to_string(&StreamRequests { requests: vec![login_request] })?;
            sender
                .send(OutgoingMessage::Text(request_json))
                .map_err(|e| Error::Stream(StreamError::ConnectionFailed(format!("Failed to send login: {}", e))))?;
        } else {
            return Err(Error::Stream(StreamError::ConnectionFailed("No active writer".to_string())));
        }

        // Wait for login response on read half
        let timeout = Duration::from_secs(5);
        let start = Instant::now();

        while start.elapsed() < timeout {
            if let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = read_half.next().await {
                // In tokio-tungstenite 0.26+, text is Utf8Bytes, convert to String
                if let Ok(msg) = serde_json::from_str::<StreamMessage>(&text.to_string()) {
                    if let StreamMessage::Response(response) = msg {
                        if let Some(item) = response.response.first() {
                            if item.service == "ADMIN" && item.command == "LOGIN" {
                                if item.content.code == 0 {
                                    info!("Authentication successful");
                                    return Ok(());
                                } else {
                                    return Err(Error::Stream(StreamError::AuthenticationFailed(
                                        item.content.msg.clone(),
                                    )));
                                }
                            }
                        }
                    }
                }
            }
        }

        Err(Error::Stream(StreamError::AuthenticationFailed(
            "Login response timeout".to_string(),
        )))
    }

    async fn resubscribe_via_channel(&self) -> Result<()> {
        let subscriptions = self.subscriptions.get_all_subscriptions();
        let mut requests = Vec::new();
        for (service, symbols) in subscriptions {
            if symbols.is_empty() { continue; }
            let fields = self
                .subscriptions
                .get_service_fields(&service)
                .unwrap_or_else(|| LevelOneEquityFields::default_fields());
            let request = StreamRequest {
                requestid: Uuid::new_v4().to_string(),
                service: service.clone(),
                command: StreamCommand::Subs,
                schwab_client_customer_id: self.customer_id.clone(),
                schwab_client_correl_id: self.correl_id.clone(),
                parameters: StreamParameters {
                    keys: Some(symbols.join(",")),
                    fields: Some(fields),
                    authorization: None,
                    schwab_client_channel: None,
                    schwab_client_function_id: None,
                },
            };
            requests.push(request);
        }
        if !requests.is_empty() {
            if let Some(sender) = self.outgoing_tx.read().clone() {
                let request_json = serde_json::to_string(&StreamRequests { requests })?;
                sender
                    .send(OutgoingMessage::Text(request_json))
                    .map_err(|e| Error::Stream(StreamError::ConnectionFailed(format!("Failed to send resubscribe: {}", e))))?;
            }
        }
        Ok(())
    }

    async fn message_loop_read(&self, read_half: &mut SplitStream<WsStream>) -> Result<()> {
        while let Some(msg) = read_half.next().await {
            match msg {
                Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                    // In tokio-tungstenite 0.26+, text is Utf8Bytes, convert to String
                    if let Ok(stream_msg) = serde_json::from_str::<StreamMessage>(&text.to_string()) {
                        // Resolve acks for response frames
                        if let StreamMessage::Response(resp) = &stream_msg {
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

                        if let Err(e) = self.message_tx.send(stream_msg) {
                            warn!("Failed to send message to receiver: {}", e);
                        }
                    }
                }
                Ok(tokio_tungstenite::tungstenite::Message::Pong(_)) => {
                    // Update last pong time for heartbeat monitoring
                    *self.last_pong.write() = Instant::now();
                    debug!("Pong received, heartbeat alive");
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

    fn start_heartbeat(&self) -> tokio::task::JoinHandle<()> {
        let interval_duration = self.config.heartbeat_interval;
        let timeout_duration = self.config.ping_timeout;
        let outgoing_tx = self.outgoing_tx.clone();
        let last_pong = self.last_pong.clone();

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
                    // Connection will be closed when this task is aborted in connect_and_run
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
}

pub struct StreamClientBuilder {
    config: Option<StreamConfig>,
    auth_manager: Option<AuthManager>,
    customer_id: Option<String>,
    correl_id: Option<String>,
}

impl StreamClientBuilder {
    pub fn new() -> Self {
        Self {
            config: None,
            auth_manager: None,
            customer_id: None,
            correl_id: None,
        }
    }

    pub fn config(mut self, config: StreamConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn auth_manager(mut self, auth: AuthManager) -> Self {
        self.auth_manager = Some(auth);
        self
    }

    pub fn customer_id(mut self, id: impl Into<String>) -> Self {
        self.customer_id = Some(id.into());
        self
    }

    pub fn correl_id(mut self, id: impl Into<String>) -> Self {
        self.correl_id = Some(id.into());
        self
    }

    pub fn build(self) -> Result<StreamClient> {
        let config = self.config
            .ok_or_else(|| Error::Config("Stream config required".to_string()))?;
        let auth_manager = self.auth_manager
            .ok_or_else(|| Error::Config("Auth manager required".to_string()))?;
        let customer_id = self.customer_id
            .ok_or_else(|| Error::Config("Customer ID required".to_string()))?;
        let correl_id = self.correl_id
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        StreamClient::new(config, auth_manager, customer_id, correl_id)
    }
}

impl Default for StreamClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::OAuthConfig;
    use crate::config::ChannelKind;

    fn create_test_auth_manager() -> AuthManager {
        let oauth_config = OAuthConfig {
            // App key must be 32 or 48 characters, app secret must be 16 or 64 characters
            app_key: "12345678901234567890123456789012".to_string(), // 32 chars
            app_secret: "1234567890123456".to_string(), // 16 chars
            callback_url: "https://localhost:8080".to_string(),
            ..Default::default()
        };
        AuthManager::new(oauth_config).unwrap()
    }

    #[test]
    fn test_stream_client_builder_basic() {
        let config = StreamConfig::default();
        let auth_manager = create_test_auth_manager();
        let customer_id = "test_customer_id".to_string();

        let result = StreamClient::builder()
            .config(config)
            .auth_manager(auth_manager)
            .customer_id(customer_id)
            .build();

        assert!(result.is_ok(), "StreamClient builder should succeed");
    }

    #[test]
    fn test_stream_client_builder_missing_config() {
        let auth_manager = create_test_auth_manager();
        let customer_id = "test_customer_id".to_string();

        let result = StreamClient::builder()
            .auth_manager(auth_manager)
            .customer_id(customer_id)
            .build();

        assert!(result.is_err(), "Builder should fail without config");
        assert!(
            result.unwrap_err().to_string().contains("config"),
            "Error should mention missing config"
        );
    }

    #[test]
    fn test_stream_client_builder_missing_auth() {
        let config = StreamConfig::default();
        let customer_id = "test_customer_id".to_string();

        let result = StreamClient::builder()
            .config(config)
            .customer_id(customer_id)
            .build();

        assert!(result.is_err(), "Builder should fail without auth manager");
    }

    #[test]
    fn test_stream_client_builder_missing_customer_id() {
        let config = StreamConfig::default();
        let auth_manager = create_test_auth_manager();

        let result = StreamClient::builder()
            .config(config)
            .auth_manager(auth_manager)
            .build();

        assert!(result.is_err(), "Builder should fail without customer ID");
    }

    #[test]
    fn test_stream_client_builder_with_correl_id() {
        let config = StreamConfig::default();
        let auth_manager = create_test_auth_manager();
        let customer_id = "test_customer_id".to_string();
        let correl_id = "custom_correlation_id".to_string();

        let result = StreamClient::builder()
            .config(config)
            .auth_manager(auth_manager)
            .customer_id(customer_id)
            .correl_id(correl_id)
            .build();

        assert!(result.is_ok(), "Builder should succeed with custom correl_id");
    }

    #[test]
    fn test_unbounded_channel_creation() {
        let mut config = StreamConfig::default();
        config.channel_kind = ChannelKind::Unbounded;

        let auth_manager = create_test_auth_manager();
        let customer_id = "test_customer_id".to_string();

        let client = StreamClient::new(config, auth_manager, customer_id, "correl".to_string())
            .unwrap();

        // Verify we can get a receiver
        let receiver = client.get_receiver();
        assert!(receiver.is_some(), "Should be able to get receiver");

        // Verify it's the unbounded variant
        if let Some(MessageReceiver::Unbounded(_)) = receiver {
            // Success
        } else {
            panic!("Expected Unbounded receiver");
        }
    }

    #[test]
    fn test_bounded_channel_creation() {
        let mut config = StreamConfig::default();
        config.channel_kind = ChannelKind::Bounded(1000);

        let auth_manager = create_test_auth_manager();
        let customer_id = "test_customer_id".to_string();

        let client = StreamClient::new(config, auth_manager, customer_id, "correl".to_string())
            .unwrap();

        // Verify we can get a receiver
        let receiver = client.get_receiver();
        assert!(receiver.is_some(), "Should be able to get receiver");

        // Verify it's the bounded variant
        if let Some(MessageReceiver::Bounded(_)) = receiver {
            // Success
        } else {
            panic!("Expected Bounded receiver");
        }
    }

    #[test]
    fn test_receiver_can_only_be_taken_once() {
        let config = StreamConfig::default();
        let auth_manager = create_test_auth_manager();
        let customer_id = "test_customer_id".to_string();

        let client = StreamClient::new(config, auth_manager, customer_id, "correl".to_string())
            .unwrap();

        // First call should succeed
        let receiver1 = client.get_receiver();
        assert!(receiver1.is_some(), "First get_receiver should succeed");

        // Second call should return None
        let receiver2 = client.get_receiver();
        assert!(receiver2.is_none(), "Second get_receiver should return None");
    }

    #[test]
    fn test_stream_config_defaults() {
        let config = StreamConfig::default();

        assert_eq!(
            config.heartbeat_interval,
            Duration::from_secs(20),
            "Default heartbeat interval should be 20 seconds"
        );
        assert_eq!(
            config.ping_timeout,
            Duration::from_secs(30),
            "Default ping timeout should be 30 seconds"
        );
        assert_eq!(
            config.max_subscriptions, 500,
            "Default max subscriptions should be 500"
        );
        assert_eq!(
            config.channel_kind,
            ChannelKind::Unbounded,
            "Default channel kind should be Unbounded"
        );
    }

    #[test]
    fn test_channel_kind_bounded_size() {
        let config = StreamConfig {
            channel_kind: ChannelKind::Bounded(5000),
            ..Default::default()
        };

        match config.channel_kind {
            ChannelKind::Bounded(size) => {
                assert_eq!(size, 5000, "Bounded size should be 5000");
            }
            _ => panic!("Expected Bounded channel kind"),
        }
    }

    #[test]
    fn test_connection_state_initial() {
        let config = StreamConfig::default();
        let auth_manager = create_test_auth_manager();
        let customer_id = "test_customer_id".to_string();

        let client = StreamClient::new(config, auth_manager, customer_id, "correl".to_string())
            .unwrap();

        // Initial state should be Disconnected
        let state = client.inner.connection_state.read().clone();
        assert_eq!(
            state,
            ConnectionState::Disconnected,
            "Initial state should be Disconnected"
        );
    }

    #[test]
    fn test_subscription_manager_created() {
        let config = StreamConfig::default();
        let auth_manager = create_test_auth_manager();
        let customer_id = "test_customer_id".to_string();

        let client = StreamClient::new(config, auth_manager, customer_id, "correl".to_string())
            .unwrap();

        // Verify subscription manager exists and is empty initially
        let subs = client.inner.subscriptions.get_all_subscriptions();
        assert!(subs.is_empty(), "Initial subscriptions should be empty");
    }

    #[test]
    fn test_outgoing_message_variants() {
        // Test that OutgoingMessage variants can be created
        let _text_msg = OutgoingMessage::Text("test".to_string());
        let _ping_msg = OutgoingMessage::Ping;

        // Verify they can be cloned
        let text_clone = OutgoingMessage::Text("test".to_string()).clone();
        let ping_clone = OutgoingMessage::Ping.clone();

        match text_clone {
            OutgoingMessage::Text(s) => assert_eq!(s, "test"),
            _ => panic!("Expected Text variant"),
        }

        match ping_clone {
            OutgoingMessage::Ping => {} // Success
            _ => panic!("Expected Ping variant"),
        }
    }

    #[tokio::test]
    async fn test_message_receiver_unbounded_api() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut receiver = MessageReceiver::Unbounded(rx);

        // Send a test message
        let test_msg = StreamMessage::Notify(crate::types::streaming::StreamNotify {
            notify: vec![crate::types::streaming::NotifyItem {
                heartbeat: Some("12345".to_string()),
            }],
        });
        tx.send(test_msg.clone()).unwrap();

        // Receive it
        let received = receiver.recv().await;
        assert!(received.is_some(), "Should receive message");

        // Verify it matches
        if let Some(StreamMessage::Notify(notify)) = received {
            assert_eq!(notify.notify.len(), 1);
            assert_eq!(notify.notify[0].heartbeat, Some("12345".to_string()));
        } else {
            panic!("Expected Notify message");
        }
    }

    #[tokio::test]
    async fn test_message_receiver_bounded_api() {
        let (tx, rx) = mpsc::channel(10);
        let mut receiver = MessageReceiver::Bounded(rx);

        // Send a test message
        let test_msg = StreamMessage::Notify(crate::types::streaming::StreamNotify {
            notify: vec![crate::types::streaming::NotifyItem {
                heartbeat: Some("67890".to_string()),
            }],
        });
        tx.send(test_msg.clone()).await.unwrap();

        // Receive it
        let received = receiver.recv().await;
        assert!(received.is_some(), "Should receive message");

        // Verify it matches
        if let Some(StreamMessage::Notify(notify)) = received {
            assert_eq!(notify.notify.len(), 1);
            assert_eq!(notify.notify[0].heartbeat, Some("67890".to_string()));
        } else {
            panic!("Expected Notify message");
        }
    }

    #[tokio::test]
    async fn test_message_receiver_channel_closed() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut receiver = MessageReceiver::Unbounded(rx);

        // Drop the sender
        drop(tx);

        // Should receive None when channel is closed
        let received = receiver.recv().await;
        assert!(received.is_none(), "Should return None when channel closed");
    }

    #[test]
    fn test_exponential_backoff_initial_value() {
        let config = StreamConfig::default();
        let auth_manager = create_test_auth_manager();
        let customer_id = "test_customer_id".to_string();

        let client = StreamClient::new(config, auth_manager, customer_id, "correl".to_string())
            .unwrap();

        // Initial backoff should be 2 seconds
        let backoff = *client.inner.backoff_delay.read();
        assert_eq!(
            backoff,
            Duration::from_secs(2),
            "Initial backoff should be 2 seconds"
        );
    }

    #[test]
    fn test_pending_acks_initial_state() {
        let config = StreamConfig::default();
        let auth_manager = create_test_auth_manager();
        let customer_id = "test_customer_id".to_string();

        let client = StreamClient::new(config, auth_manager, customer_id, "correl".to_string())
            .unwrap();

        // Pending acks should be empty initially
        let acks = client.inner.pending_acks.read();
        assert!(acks.is_empty(), "Pending acks should be empty initially");
    }

    #[test]
    fn test_last_pong_initialized() {
        let config = StreamConfig::default();
        let auth_manager = create_test_auth_manager();
        let customer_id = "test_customer_id".to_string();

        let client = StreamClient::new(config, auth_manager, customer_id, "correl".to_string())
            .unwrap();

        // Last pong should be initialized to "now"
        let last_pong = *client.inner.last_pong.read();
        let elapsed = last_pong.elapsed();

        assert!(
            elapsed < Duration::from_millis(100),
            "Last pong should be very recent (< 100ms)"
        );
    }
}
