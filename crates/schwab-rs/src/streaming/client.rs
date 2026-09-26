//! WebSocket streaming client for real-time market data.
//!
//! Provides subscription-based access to quotes, charts, and account activity.

use crate::auth::AuthManager;
use crate::config::{ChannelKind, StreamConfig};
use crate::error::{Error, Result, StreamError};
use crate::streaming::message_handler::OutgoingMessage;
use crate::streaming::subscription::SubscriptionManager;
use crate::transport::websocket::{WebSocketTransport, WsStream};
use crate::types::streaming::StreamStatus;
use crate::types::streaming::*;
use futures_util::stream::SplitStream;
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde_json;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, interval, sleep};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Generation-tagged sender feeding the subscriber queue.
///
/// Every message carries the generation it was produced under so the
/// receiver can discard data that predates a reconnect or a gap. Queue
/// overflow is never silent: it flips the shared [`StreamStatus`] to
/// not-ready and bumps `lost_messages`. The connection stays up: frames are
/// dropped (and counted) until the consumer has drained half the queue, then
/// delivery resumes under a new generation, so the consumer re-baselines as
/// after a reconnect without a new LOGIN or resubscription.
#[derive(Debug)]
pub(crate) struct MessageSender {
    tx: mpsc::Sender<(u64, StreamMessage)>,
    state: tokio::sync::watch::Sender<StreamStatus>,
    overflow: std::sync::atomic::AtomicU8,
}

/// [`MessageSender::overflow`] states.
const FLOWING: u8 = 0;
const OVERFLOWED_WHILE_READY: u8 = 1;
const OVERFLOWED_WHILE_UNREADY: u8 = 2;

impl MessageSender {
    pub(crate) fn new(
        tx: mpsc::Sender<(u64, StreamMessage)>,
        state: tokio::sync::watch::Sender<StreamStatus>,
    ) -> Self {
        Self {
            tx,
            state,
            overflow: std::sync::atomic::AtomicU8::new(FLOWING),
        }
    }

    /// Send a message tagged with the current generation. A full queue
    /// drops it and invalidates the stream until the consumer catches up.
    ///
    /// Returns `Err` only if the receiver has been dropped.
    pub(crate) fn send(&self, msg: StreamMessage) -> std::result::Result<(), String> {
        let overflow = self.overflow.load(Ordering::Acquire);
        if overflow != FLOWING {
            if self.tx.capacity() * 2 < self.tx.max_capacity() {
                self.record_loss();
                return Ok(());
            }
            // Drained: what follows starts a new generation, and it is ready
            // only if the stream was ready when the gap began.
            self.overflow.store(FLOWING, Ordering::Release);
            self.state.send_modify(|s| {
                s.generation += 1;
                s.ready = overflow == OVERFLOWED_WHILE_READY;
            });
        }
        let generation = self.state.borrow().generation;
        match self.tx.try_send((generation, msg)) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                let was_ready = self.state.borrow().ready;
                self.overflow.store(
                    if was_ready {
                        OVERFLOWED_WHILE_READY
                    } else {
                        OVERFLOWED_WHILE_UNREADY
                    },
                    Ordering::Release,
                );
                self.record_loss();
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.record_loss();
                Err("Stream receiver closed".into())
            }
        }
    }

    /// A new connection starts clean: any gap belongs to the old generation.
    pub(crate) fn reset_overflow(&self) {
        self.overflow.store(FLOWING, Ordering::Release);
    }

    fn record_loss(&self) {
        self.state.send_modify(|s| {
            s.lost_messages += 1;
            s.ready = false;
        });
        metrics::counter!(crate::metrics::STREAM_DROPPED_MESSAGES_TOTAL).increment(1);
    }
}

/// Subscriber-side handle for stream messages.
///
/// Only messages produced under the current, ready generation are delivered;
/// [`StreamMessage::State`] notifications are interleaved whenever readiness
/// or the generation changes so consumers can re-baseline.
#[derive(Debug)]
pub struct MessageReceiver {
    rx: mpsc::Receiver<(u64, StreamMessage)>,
    pending: Option<(u64, StreamMessage)>,
    state: tokio::sync::watch::Receiver<StreamStatus>,
}

impl MessageReceiver {
    pub(crate) fn new(
        rx: mpsc::Receiver<(u64, StreamMessage)>,
        state: tokio::sync::watch::Receiver<StreamStatus>,
    ) -> Self {
        Self {
            rx,
            pending: None,
            state,
        }
    }

    /// Receive the next deliverable message.
    ///
    /// Returns `None` when the stream client has been dropped and all
    /// messages have been received.
    pub async fn recv(&mut self) -> Option<StreamMessage> {
        let Self { rx, pending, state } = self;
        loop {
            if !state.has_changed().unwrap_or(true)
                && let Some((generation, message)) = pending.take()
            {
                let current = state.borrow();
                if current.ready && current.generation == generation {
                    return Some(message);
                }
            }
            tokio::select! {
                biased;
                changed = state.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                    let current = state.borrow_and_update().clone();
                    let stale = pending.as_ref().is_some_and(|(generation, _)| {
                        !current.ready || *generation != current.generation
                    });
                    if stale {
                        *pending = None;
                    }
                    if pending.is_none() {
                        while let Ok((generation, message)) = rx.try_recv() {
                            if current.ready && generation == current.generation {
                                *pending = Some((generation, message));
                                break;
                            }
                        }
                    }
                    return Some(StreamMessage::State(current));
                }
                item = rx.recv() => {
                    let (generation, msg) = item?;
                    let current = state.borrow();
                    if current.ready && current.generation == generation {
                        return Some(msg);
                    }
                }
            }
        }
    }
}

pub type StreamMessage = crate::types::streaming::StreamMessage;

/// Streamer login identity, sourced from `GET /userPreference`.
///
/// Schwab's docs require every streamer request to carry the
/// `schwabClientCustomerId`/`schwabClientCorrelId` pair, and the LOGIN
/// command to additionally carry `SchwabClientChannel` and
/// `SchwabClientFunctionId` — all four come from the `streamerInfo`
/// block of user preferences. `new()` defaults channel/function-id to
/// the values Schwab currently issues for API apps (`N9`/`APIAPP`);
/// prefer [`StreamerIdentity::from_streamer_info`] when you have the
/// real preferences response.
#[derive(Debug, Clone)]
pub struct StreamerIdentity {
    pub customer_id: String,
    pub correl_id: String,
    pub channel: String,
    pub function_id: String,
}

impl StreamerIdentity {
    pub fn new(customer_id: impl Into<String>, correl_id: impl Into<String>) -> Self {
        Self {
            customer_id: customer_id.into(),
            correl_id: correl_id.into(),
            channel: "N9".to_string(),
            function_id: "APIAPP".to_string(),
        }
    }

    /// Build the identity from the `streamerInfo` entry of a
    /// `GET /userPreference` response.
    pub fn from_streamer_info(info: &crate::types::accounts::StreamerInfo) -> Self {
        Self {
            customer_id: info.schwab_client_customer_id.clone(),
            correl_id: info.schwab_client_correl_id.clone(),
            channel: info.schwab_client_channel.clone(),
            function_id: info.schwab_client_function_id.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct StreamClient {
    inner: Arc<StreamClientInner>,
    _owner: Arc<StreamOwner>,
}

#[derive(Debug)]
struct StreamOwner(std::sync::Weak<StreamClientInner>);
impl Drop for StreamOwner {
    fn drop(&mut self) {
        if let Some(inner) = self.0.upgrade() {
            inner.shutting_down.store(true, Ordering::SeqCst);
            if let Some(task) = inner.connection_task.write().take() {
                task.abort();
            }
            inner.cleanup();
        }
    }
}

#[derive(Debug)]
struct StreamClientInner {
    coordinator: RwLock<Arc<dyn crate::coordination::Coordinator>>,
    config: StreamConfig,
    health: tokio::sync::watch::Sender<StreamStatus>,
    lifecycle_lock: tokio::sync::Mutex<()>,
    subscription_lock: tokio::sync::Mutex<()>,
    reconnect_requested: tokio::sync::Notify,
    auth_manager: AuthManager,
    transport: WebSocketTransport,
    subscriptions: Arc<SubscriptionManager>,
    message_tx: MessageSender,
    message_rx: Arc<RwLock<Option<MessageReceiver>>>,
    connection_state: Arc<RwLock<ConnectionState>>,
    identity: StreamerIdentity,
    /// Exponential backoff delay (starts at 2s, caps at 128s)
    backoff_delay: Arc<RwLock<Duration>>,
    /// Sender for outgoing WebSocket messages (text and ping) - swapped on each connection
    outgoing_tx: Arc<RwLock<Option<mpsc::UnboundedSender<OutgoingMessage>>>>,
    /// Writer task handle to allow clean abort on disconnect
    writer_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    /// Pending acks keyed by request id
    pending_acks: Arc<RwLock<HashMap<String, oneshot::Sender<Result<()>>>>>,
    /// Last time a Pong frame was received (for heartbeat timeout detection)
    last_pong: Arc<RwLock<Instant>>,
    /// Oneshot that `connect_and_run` fires once — the first time
    /// authentication succeeds. `connect()` awaits this instead of
    /// polling `is_authenticated()` at 100 ms cadence, which used to
    /// race when the socket flipped Authenticated → Disconnected inside
    /// one tick.
    initial_auth_tx: Arc<RwLock<Option<oneshot::Sender<()>>>>,
    /// Shutdown flag — set by `disconnect()` so the reconnect loop
    /// stops resurrecting the connection after the peer tears it down.
    shutting_down: Arc<AtomicBool>,
    /// Set once the streamer LOGIN has succeeded at least once on the
    /// current `connect()` lifecycle. The reconnect loop uses this to
    /// distinguish "never got in" (terminal — bad credentials or socket
    /// URL) from "was healthy, then dropped" (retry with backoff).
    authenticated_once: AtomicBool,
    /// Handle for the outer connection loop task (one per `connect()`
    /// call), so `disconnect()` can abort it cleanly instead of leaking
    /// the task for the lifetime of the process.
    connection_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

#[derive(Debug, Clone, PartialEq)]
enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Authenticated,
}

/// How a connection's read loop ended.
enum LoopExit {
    /// Server closed with WebSocket code 1000 (normal closure) — matches
    /// the reference client's "closed gracefully" case: do not reconnect.
    CleanClose,
    /// Stream ended without a normal-closure frame — treat as abnormal
    /// and let the reconnect loop decide.
    StreamEnded,
}

struct AbortTask(tokio::task::AbortHandle);
impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}
struct ConnectCancellation<'a> {
    inner: &'a StreamClientInner,
    armed: bool,
}
impl Drop for ConnectCancellation<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.inner.shutting_down.store(true, Ordering::SeqCst);
            if let Some(task) = self.inner.connection_task.write().take() {
                task.abort();
            }
            self.inner.cleanup();
        }
    }
}
struct AckGuard<'a> {
    inner: &'a StreamClientInner,
    id: String,
    confirmed: bool,
}
impl Drop for AckGuard<'_> {
    fn drop(&mut self) {
        self.inner.pending_acks.write().remove(&self.id);
        if !self.confirmed {
            self.inner.reconnect_requested.notify_one();
        }
    }
}

impl StreamClient {
    pub fn new(
        config: StreamConfig,
        auth_manager: AuthManager,
        identity: StreamerIdentity,
    ) -> Result<Self> {
        let transport = WebSocketTransport::new(&config.websocket_url)?;

        if config.heartbeat_interval.is_zero() || config.ping_timeout.is_zero() {
            return Err(Error::Config(
                "Stream heartbeat durations must be positive".into(),
            ));
        }
        let capacity = match config.channel_kind {
            ChannelKind::Bounded(0) => {
                return Err(Error::Config(
                    "Stream queue capacity must be positive".into(),
                ));
            }
            ChannelKind::Bounded(size) => size,
            // Legacy unbounded configuration is capped; no SDK path may
            // accumulate an unlimited backlog of account/market data.
            ChannelKind::Unbounded => 65536,
        };
        let (health, status_rx) = tokio::sync::watch::channel(StreamStatus::default());
        let (tx, rx) = mpsc::channel(capacity);
        let message_tx = MessageSender::new(tx, health.clone());
        let message_rx = MessageReceiver::new(rx, status_rx);

        let coordinator = crate::storage::StateDatabase::new(
            auth_manager
                .config()
                .tokens_file
                .with_extension("orders.json"),
        );
        let initial_backoff = config.reconnect.initial_backoff;
        let inner = Arc::new(StreamClientInner {
            coordinator: RwLock::new(Arc::new(coordinator)),
            config,
            health,
            lifecycle_lock: tokio::sync::Mutex::new(()),
            subscription_lock: tokio::sync::Mutex::new(()),
            reconnect_requested: tokio::sync::Notify::new(),
            auth_manager,
            transport,
            subscriptions: Arc::new(SubscriptionManager::new()),
            message_tx,
            message_rx: Arc::new(RwLock::new(Some(message_rx))),
            connection_state: Arc::new(RwLock::new(ConnectionState::Disconnected)),
            identity,
            backoff_delay: Arc::new(RwLock::new(initial_backoff)),
            outgoing_tx: Arc::new(RwLock::new(None)),
            writer_task: Arc::new(RwLock::new(None)),
            pending_acks: Arc::new(RwLock::new(HashMap::new())),
            last_pong: Arc::new(RwLock::new(Instant::now())),
            initial_auth_tx: Arc::new(RwLock::new(None)),
            shutting_down: Arc::new(AtomicBool::new(false)),
            authenticated_once: AtomicBool::new(false),
            connection_task: Arc::new(RwLock::new(None)),
        });
        Ok(Self {
            _owner: Arc::new(StreamOwner(Arc::downgrade(&inner))),
            inner,
        })
    }

    pub fn with_coordinator(
        self,
        coordinator: Arc<dyn crate::coordination::Coordinator>,
    ) -> Result<Self> {
        if self.is_running() {
            return Err(Error::Config(
                "Configure stream coordination before connecting".into(),
            ));
        }
        *self.inner.coordinator.write() = coordinator;
        Ok(self)
    }
    /// Convenience: subscribe to Level One Equities for given symbols
    pub async fn subscribe_level_one_equities(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.subscribe(StreamService::LeveloneEquities, syms).await
    }

    /// Convenience: unsubscribe from Level One Equities for given symbols
    pub async fn unsubscribe_level_one_equities(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.unsubscribe(StreamService::LeveloneEquities, syms)
            .await
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
        self.subscribe(StreamService::LeveloneFuturesOptions, syms)
            .await
    }

    pub async fn unsubscribe_level_one_futures_options(&self, symbols: &[&str]) -> Result<()> {
        let syms: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
        self.unsubscribe(StreamService::LeveloneFuturesOptions, syms)
            .await
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
        self.subscribe(
            StreamService::AcctActivity,
            vec!["Account Activity".to_string()],
        )
        .await
    }

    pub async fn unsubscribe_account_activity(&self) -> Result<()> {
        self.unsubscribe(
            StreamService::AcctActivity,
            vec!["Account Activity".to_string()],
        )
        .await
    }

    pub fn builder() -> StreamClientBuilder {
        StreamClientBuilder::new()
    }

    /// Set service-specific field IDs (comma-separated) for *later*
    /// subscriptions and reconnects. Keys already streaming keep their
    /// fields; use [`StreamClient::view`] to change those.
    pub fn set_service_fields(&self, service: StreamService, fields_csv: String) {
        self.inner
            .subscriptions
            .set_service_fields(service.as_str().to_string(), fields_csv);
    }

    /// Change the fields streamed for every subscribed key of `service` now
    /// (Schwab's `VIEW` command), and for later subscriptions and
    /// reconnects. With nothing subscribed on the service this only records
    /// the fields, like [`StreamClient::set_service_fields`].
    pub async fn view(&self, service: StreamService, fields_csv: String) -> Result<()> {
        let _subscription = self.inner.subscription_lock.lock().await;
        let valid = !fields_csv.is_empty()
            && fields_csv
                .split(',')
                .all(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()));
        if !valid {
            return Err(Error::InvalidParameter(
                "Fields must be comma-separated field numbers".into(),
            ));
        }
        let live = self
            .inner
            .subscriptions
            .get_subscription(service.as_str())
            .is_some_and(|s| !s.symbols.is_empty());
        if live {
            if !self.is_authenticated() {
                return Err(Error::Stream(StreamError::ConnectionFailed(
                    "Not authenticated".to_string(),
                )));
            }
            let request = self.inner.build_request(
                StreamCommand::View,
                &service,
                StreamParameters {
                    keys: None,
                    fields: Some(fields_csv.clone()),
                    authorization: None,
                    schwab_client_channel: None,
                    schwab_client_function_id: None,
                },
            );
            self.send_request(request).await?;
        }
        self.inner
            .subscriptions
            .set_service_fields(service.as_str().to_string(), fields_csv);
        Ok(())
    }

    pub async fn connect(&self) -> Result<()> {
        let _lifecycle = self.inner.lifecycle_lock.lock().await;
        // Reject concurrent connect() calls so we don't spawn two
        // reconnect loops competing for the same socket / auth slot.
        {
            let guard = self.inner.connection_task.read();
            if let Some(handle) = guard.as_ref()
                && !handle.is_finished()
            {
                return Err(Error::Stream(StreamError::ConnectionFailed(
                    "connect() already in progress".into(),
                )));
            }
        }

        self.inner.shutting_down.store(false, Ordering::SeqCst);
        self.set_state(ConnectionState::Connecting);

        // Subscribe to the first-auth signal BEFORE spawning so the
        // loop can't complete auth before we're listening.
        let (tx, rx) = oneshot::channel::<()>();
        *self.inner.initial_auth_tx.write() = Some(tx);

        let inner = self.inner.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = inner.connection_loop().await {
                error!("Connection loop failed");
                let _ = e;
            }
        });
        *self.inner.connection_task.write() = Some(handle);

        let mut cancellation = ConnectCancellation {
            inner: &self.inner,
            armed: true,
        };
        // The loop either fires `tx` on successful auth or drops it on
        // terminal failure — in both cases we unblock. Timeout guards
        // against network hangs that never produce either outcome.
        match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(())) => {
                cancellation.armed = false;
                Ok(())
            }
            Ok(Err(_)) => Err(Error::Stream(StreamError::AuthenticationFailed(
                "connection loop exited before authenticating \
                 (check logs for the underlying cause)"
                    .into(),
            ))),
            Err(_) => Err(Error::Stream(StreamError::ConnectionFailed(
                "Authentication timeout".into(),
            ))),
        }
    }

    /// Stop the reconnect loop and close the underlying socket.
    ///
    /// Sends a best-effort `ADMIN/LOGOUT` first (as the reference client
    /// does) so the streamer can release the session promptly.
    ///
    /// Idempotent. After calling `disconnect`, a subsequent `connect`
    /// will spin up a fresh loop.
    pub async fn disconnect(&self) {
        let _lifecycle = self.inner.lifecycle_lock.lock().await;
        self.inner.shutting_down.store(true, Ordering::SeqCst);

        // Best-effort LOGOUT: queue the frame and give the writer task a
        // moment to flush it before we abort everything.
        if self.is_authenticated() {
            let logout = self.inner.build_request(
                StreamCommand::Logout,
                &StreamService::Admin,
                StreamParameters {
                    keys: None,
                    fields: None,
                    authorization: None,
                    schwab_client_channel: None,
                    schwab_client_function_id: None,
                },
            );
            let queued = {
                let guard = self.inner.outgoing_tx.read();
                if let (Some(sender), Ok(json)) = (
                    guard.as_ref(),
                    serde_json::to_string(&StreamRequests {
                        requests: vec![logout],
                    }),
                ) {
                    sender.send(OutgoingMessage::Text(json)).is_ok()
                } else {
                    false
                }
            };
            if queued {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }

        if let Some(handle) = self.inner.connection_task.write().take() {
            handle.abort();
        }
        if let Some(handle) = self.inner.writer_task.write().take() {
            handle.abort();
        }
        *self.inner.outgoing_tx.write() = None;
        self.inner.cleanup();
    }

    /// Subscribe to `service` for the given keys.
    ///
    /// Uses the streamer `ADD` command (additive), matching the reference
    /// client — `SUBS` would *replace* the service's entire key list
    /// server-side, silently dropping earlier subscriptions while the
    /// local tracker still believed they were active.
    pub async fn subscribe(&self, service: StreamService, mut symbols: Vec<String>) -> Result<()> {
        let _subscription = self.inner.subscription_lock.lock().await;
        symbols.sort();
        symbols.dedup();
        if symbols.is_empty()
            || symbols
                .iter()
                .any(|s| s.trim().is_empty() || s.contains(','))
        {
            return Err(Error::InvalidParameter(
                "Subscription keys must be nonempty individual keys".into(),
            ));
        }
        if !self.is_authenticated() {
            return Err(Error::Stream(StreamError::ConnectionFailed(
                "Not authenticated".to_string(),
            )));
        }

        let new_symbols = symbols
            .iter()
            .filter(|s| !self.inner.subscriptions.is_subscribed(service.as_str(), s))
            .count();
        if self.inner.subscriptions.get_symbol_count() + new_symbols
            > self.inner.config.max_subscriptions
        {
            return Err(Error::Stream(StreamError::SymbolLimitReached {
                limit: self.inner.config.max_subscriptions,
            }));
        }

        let fields = self
            .inner
            .subscriptions
            .get_service_fields(service.as_str())
            .unwrap_or_else(|| service.default_fields());

        let request = self.inner.build_request(
            StreamCommand::Add,
            &service,
            StreamParameters {
                keys: Some(symbols.join(",")),
                fields: Some(fields.clone()),
                authorization: None,
                schwab_client_channel: None,
                schwab_client_function_id: None,
            },
        );

        self.send_request(request).await?;
        self.inner
            .subscriptions
            .add_subscription(service.as_str().to_string(), symbols, fields);
        Ok(())
    }

    pub async fn unsubscribe(&self, service: StreamService, symbols: Vec<String>) -> Result<()> {
        let _subscription = self.inner.subscription_lock.lock().await;
        if !self.is_authenticated() {
            return Err(Error::Stream(StreamError::ConnectionFailed(
                "Not authenticated".to_string(),
            )));
        }

        let request = self.inner.build_request(
            StreamCommand::Unsubs,
            &service,
            StreamParameters {
                keys: Some(symbols.join(",")),
                fields: None,
                authorization: None,
                schwab_client_channel: None,
                schwab_client_function_id: None,
            },
        );

        self.send_request(request).await?;
        self.inner
            .subscriptions
            .remove_subscription(service.as_str().to_string(), symbols);
        Ok(())
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
        // Track ack. Keep a local copy of the id so we can clean up
        // the pending-ack entry on timeout without needing the
        // now-moved request back.
        let request_id = request.requestid.clone();
        let (tx, rx) = oneshot::channel::<Result<()>>();
        {
            self.inner
                .pending_acks
                .write()
                .insert(request_id.clone(), tx);
        }

        let mut ack_guard = AckGuard {
            inner: &self.inner,
            id: request_id.clone(),
            confirmed: false,
        };
        let json = serde_json::to_string(&StreamRequests {
            requests: vec![request],
        })?;
        if let Some(sender) = self.inner.outgoing_tx.read().clone() {
            if let Err(e) = sender.send(OutgoingMessage::Text(json)) {
                self.inner.pending_acks.write().remove(&request_id);
                return Err(Error::Stream(StreamError::ConnectionFailed(format!(
                    "Failed to queue request: {}",
                    e
                ))));
            }
        } else {
            self.inner.pending_acks.write().remove(&request_id);
            return Err(Error::Stream(StreamError::ConnectionFailed(
                "No active connection".to_string(),
            )));
        }
        // Wait briefly for ack (non-blocking long-term; fast-fail only)
        let timeout = Duration::from_secs(5);
        let result = tokio::time::timeout(timeout, rx).await;
        if matches!(&result, Ok(Ok(_))) {
            ack_guard.confirmed = true;
        }
        match result {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_canceled)) => Err(Error::Stream(StreamError::ConnectionFailed(
                "Ack channel canceled".to_string(),
            ))),
            Err(_elapsed) => {
                // Previously returned Ok(()) on timeout, which silently
                // masked lost subscriptions and prevented
                // `send_request_with_retry` from ever retrying.
                // Surfacing ConnectionFailed lets the retry loop fire
                // and lets the caller see the failure.
                warn!("Stream request ack timeout");
                // Remove the pending-ack entry so a late response
                // doesn't try to signal a dropped receiver.
                self.inner.pending_acks.write().remove(&request_id);
                Err(Error::Stream(StreamError::ConnectionFailed(
                    "stream request ack timeout".to_string(),
                )))
            }
        }
    }

    fn set_state(&self, state: ConnectionState) {
        *self.inner.connection_state.write() = state;
    }

    pub fn confirmed_subscriptions(&self) -> HashMap<String, Vec<String>> {
        self.inner.subscriptions.get_all_subscriptions()
    }
    pub fn status(&self) -> StreamStatus {
        self.inner.health.borrow().clone()
    }
    pub fn is_running(&self) -> bool {
        self.inner
            .connection_task
            .read()
            .as_ref()
            .is_some_and(|h| !h.is_finished())
    }
    /// Logged in to the streamer on a live socket. Unlike
    /// [`StreamStatus::ready`], this holds while a slow consumer's queue has
    /// overflowed, since the connection stays up and takes requests.
    pub fn is_authenticated(&self) -> bool {
        *self.inner.connection_state.read() == ConnectionState::Authenticated
    }
}

/// Emit `schwab_stream_messages_total{service, kind}` for each
/// inbound frame. Response/Notify frames have no per-service payload,
/// so they get `service="_control"` rather than inflating real
/// service labels.
/// `delay` scaled by a uniform factor in `1 ± jitter` (clamped to 0..=1).
fn jittered(delay: Duration, jitter: f64) -> Duration {
    let jitter = if jitter.is_finite() {
        jitter.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let factor = 1.0 + jitter * (2.0 * rand::random::<f64>() - 1.0);
    delay.mul_f64(factor.max(0.0))
}

fn record_stream_frame(msg: &StreamMessage) {
    match msg {
        StreamMessage::State(_) => {}
        StreamMessage::Data(data) => {
            for item in &data.data {
                metrics::counter!(
                    crate::metrics::STREAM_MESSAGES_TOTAL,
                    "service" => item.service.clone(),
                    "kind" => "data",
                )
                .increment(item.content.len() as u64);
            }
        }
        StreamMessage::Response(_) => {
            metrics::counter!(
                crate::metrics::STREAM_MESSAGES_TOTAL,
                "service" => "_control",
                "kind" => "response",
            )
            .increment(1);
        }
        StreamMessage::Notify(notify) => {
            // Only heartbeats are documented; say so when something else
            // arrives (the fields may name the account's session).
            if notify.notify.iter().any(|item| item.heartbeat.is_none()) {
                warn!("Streamer sent a notification other than a heartbeat");
            }
            metrics::counter!(
                crate::metrics::STREAM_MESSAGES_TOTAL,
                "service" => "_control",
                "kind" => "notify",
            )
            .increment(1);
        }
    }
}

impl StreamClientInner {
    /// Build a streamer request frame carrying the client identity.
    fn build_request(
        &self,
        command: StreamCommand,
        service: &StreamService,
        parameters: StreamParameters,
    ) -> StreamRequest {
        StreamRequest {
            requestid: Uuid::new_v4().to_string(),
            service: service.as_str().to_string(),
            command,
            schwab_client_customer_id: self.identity.customer_id.clone(),
            schwab_client_correl_id: self.identity.correl_id.clone(),
            parameters,
        }
    }

    fn cleanup(&self) {
        if let Some(handle) = self.writer_task.write().take() {
            handle.abort();
        }
        *self.outgoing_tx.write() = None;
        self.pending_acks.write().clear();
        *self.connection_state.write() = ConnectionState::Disconnected;
        self.health.send_modify(|s| s.ready = false);
        metrics::gauge!(crate::metrics::STREAM_CONNECTED).set(0.0);
    }

    async fn connection_loop(&self) -> Result<()> {
        let mut retry_count = 0;
        let mut is_reconnect = false;
        self.authenticated_once.store(false, Ordering::SeqCst);

        loop {
            if self.shutting_down.load(Ordering::SeqCst) {
                info!("Stream shutdown requested; exiting reconnect loop");
                return Ok(());
            }

            if is_reconnect {
                metrics::counter!(crate::metrics::STREAM_RECONNECTS_TOTAL).increment(1);
            }

            let coordinator = self.coordinator.read().clone();
            let ownership = crate::coordination::LeaseSession::acquire(
                coordinator,
                &format!("stream:{}", self.identity.customer_id),
            )
            .await;
            let result = match ownership {
                Ok(mut lease) => {
                    // Never backwards: an overflow recovery may already have
                    // advanced the generation past this lease's epoch.
                    let base = lease.lease().epoch.saturating_sub(1);
                    self.health
                        .send_modify(|s| s.generation = s.generation.max(base));
                    lease.run(self.connect_and_run()).await
                }
                Err(error) => Err(error),
            };
            self.cleanup();
            match result {
                Ok(_) => {
                    // Server closed the socket with a normal-closure frame
                    // (code 1000). Matches the reference client: a graceful
                    // close is an instruction to stop, not a fault to retry
                    // — e.g. Schwab tears the session down when another
                    // login takes over the streamer slot.
                    info!("Stream connection closed gracefully; not reconnecting");
                    return Ok(());
                }
                Err(e) => {
                    error!("Stream connection error");

                    // A definitive login rejection never resolves by
                    // retrying with the same credentials.
                    let auth_rejected =
                        matches!(&e, Error::Stream(StreamError::AuthenticationFailed(_)));

                    // Never got in at all (bad socket URL, unreachable
                    // host, rejected login): retrying is pointless and
                    // `connect()` should fail fast.
                    let never_authenticated = !self.authenticated_once.load(Ordering::SeqCst);

                    if auth_rejected || never_authenticated {
                        // Drop the auth signal so `connect()` unblocks
                        // with an AuthenticationFailed error instead of
                        // hanging until the 10 s timeout.
                        drop(self.initial_auth_tx.write().take());
                        return Err(e);
                    }

                    if !self.config.reconnect.enabled {
                        drop(self.initial_auth_tx.write().take());
                        return Err(e);
                    }

                    if let Some(max_retries) = self.config.reconnect.max_retries
                        && retry_count >= max_retries
                    {
                        error!("Maximum reconnection attempts reached");
                        drop(self.initial_auth_tx.write().take());
                        return Err(Error::Stream(StreamError::ConnectionFailed(
                            "Maximum reconnection attempts exceeded".to_string(),
                        )));
                    }

                    let backoff =
                        jittered(*self.backoff_delay.read(), self.config.reconnect.jitter);
                    warn!(
                        "Stream connection lost, reconnecting in {:?} (attempt {})",
                        backoff,
                        retry_count + 1
                    );
                    sleep(backoff).await;

                    // Exponential backoff: double the delay, cap at 128 seconds
                    let mut delay = self.backoff_delay.write();
                    *delay = delay
                        .mul_f64(self.config.reconnect.backoff_multiplier)
                        .min(self.config.reconnect.max_backoff);

                    retry_count += 1;
                    is_reconnect = true;
                }
            }
        }
    }

    async fn connect_and_run(&self) -> Result<()> {
        *self.connection_state.write() = ConnectionState::Connecting;
        self.health.send_modify(|s| {
            s.generation += 1;
            s.ready = false;
        });
        self.message_tx.reset_overflow();

        let ws_stream = tokio::time::timeout(Duration::from_secs(5), self.transport.connect())
            .await
            .map_err(|_| {
                Error::Stream(StreamError::ConnectionFailed(
                    "WebSocket handshake timeout".into(),
                ))
            })??;
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
                    OutgoingMessage::Text(text) => {
                        tokio_tungstenite::tungstenite::Message::Text(text.into())
                    }
                    OutgoingMessage::Ping => {
                        tokio_tungstenite::tungstenite::Message::Ping(vec![].into())
                    }
                };

                if let Err(e) = write_half.send(ws_msg).await {
                    error!("WebSocket write error: {}", e);
                    break;
                }
            }
        });
        *self.writer_task.write() = Some(writer);

        // Authenticate using read half
        match tokio::time::timeout(
            Duration::from_secs(5),
            self.authenticate_read(&mut read_half),
        )
        .await
        .unwrap_or_else(|_| {
            Err(Error::Stream(StreamError::ConnectionFailed(
                "Login timeout".into(),
            )))
        }) {
            Ok(()) => {
                metrics::counter!(
                    crate::metrics::STREAM_CONNECTS_TOTAL,
                    "outcome" => "success",
                )
                .increment(1);
            }
            Err(e) => {
                metrics::counter!(
                    crate::metrics::STREAM_CONNECTS_TOTAL,
                    "outcome" => "failure",
                )
                .increment(1);
                return Err(e);
            }
        }
        self.resubscribe_via_channel(&mut read_half).await?;
        *self.connection_state.write() = ConnectionState::Authenticated;
        self.health.send_modify(|s| s.ready = true);
        self.authenticated_once.store(true, Ordering::SeqCst);
        // Login succeeded — the connection is healthy, so the next failure
        // starts from a fresh backoff schedule (the reference client resets
        // its backoff at the same point).
        *self.backoff_delay.write() = self.config.reconnect.initial_backoff;
        metrics::gauge!(crate::metrics::STREAM_CONNECTED).set(1.0);

        // Fire the one-shot `connect()` is awaiting. This is take()-ed,
        // so reconnects after the first success are no-ops.
        if let Some(tx) = self.initial_auth_tx.write().take() {
            let _ = tx.send(());
        }

        // Resubscribe to existing subscriptions using channel

        // Run the read loop and the heartbeat watchdog side by side.
        // The watchdog task completes when the ping timeout elapses (or
        // the writer dies), and the select! tears the connection down —
        // without this, a silently dead TCP connection would leave the
        // read loop blocked indefinitely with no reconnect.
        let mut heartbeat_task = self.start_heartbeat();
        let _heartbeat_guard = AbortTask(heartbeat_task.abort_handle());
        let result = tokio::select! {
            res = self.message_loop_read(&mut read_half) => match res {
                Ok(LoopExit::CleanClose) => Ok(()),
                Ok(LoopExit::StreamEnded) => Err(Error::ConnectionClosed),
                Err(e) => Err(e),
            },
            _ = self.reconnect_requested.notified() => Err(Error::ConnectionClosed),
            _ = &mut heartbeat_task => {
                Err(Error::Stream(StreamError::HeartbeatTimeout))
            }
        };

        heartbeat_task.abort();
        if let Some(handle) = self.writer_task.write().take() {
            handle.abort();
        }
        *self.outgoing_tx.write() = None;

        *self.connection_state.write() = ConnectionState::Disconnected;
        metrics::gauge!(crate::metrics::STREAM_CONNECTED).set(0.0);

        result
    }

    // Authenticate using the read half; login is sent via writer channel
    async fn authenticate_read(&self, read_half: &mut SplitStream<WsStream>) -> Result<()> {
        let access_token = self
            .auth_manager
            .get_access_token()
            .await
            .map_err(Error::Auth)?;

        let login_request = self.build_request(
            StreamCommand::Login,
            &StreamService::Admin,
            StreamParameters {
                keys: None,
                fields: None,
                authorization: Some(access_token),
                schwab_client_channel: Some(self.identity.channel.clone()),
                schwab_client_function_id: Some(self.identity.function_id.clone()),
            },
        );

        if let Some(sender) = self.outgoing_tx.read().clone() {
            let request_json = serde_json::to_string(&StreamRequests {
                requests: vec![login_request],
            })?;
            sender
                .send(OutgoingMessage::Text(request_json))
                .map_err(|e| {
                    Error::Stream(StreamError::ConnectionFailed(format!(
                        "Failed to send login: {}",
                        e
                    )))
                })?;
        } else {
            return Err(Error::Stream(StreamError::ConnectionFailed(
                "No active writer".to_string(),
            )));
        }

        // Wait for login response on read half
        let timeout = Duration::from_secs(5);
        let start = Instant::now();

        while start.elapsed() < timeout {
            let text = match read_half.next().await {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => text,
                None
                | Some(Err(_))
                | Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => {
                    return Err(Error::Stream(StreamError::ConnectionFailed(
                        "Connection closed during login".into(),
                    )));
                }
                _ => continue,
            };
            {
                if let Ok(StreamMessage::Response(response)) =
                    serde_json::from_str::<StreamMessage>(text.as_ref())
                    && let Some(item) = response.response.first()
                    && item.service == "ADMIN"
                    && item.command == "LOGIN"
                {
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

        Err(Error::Stream(StreamError::AuthenticationFailed(
            "Login response timeout".to_string(),
        )))
    }

    async fn resubscribe_via_channel(&self, read_half: &mut SplitStream<WsStream>) -> Result<()> {
        let _subscription = self.subscription_lock.lock().await;
        let subscriptions = self.subscriptions.get_all_subscriptions();
        let mut requests = Vec::new();
        for (service, symbols) in subscriptions {
            if symbols.is_empty() {
                continue;
            }
            let fields = self
                .subscriptions
                .get_service_fields(&service)
                .or_else(|| StreamService::from_api_str(&service).map(|s| s.default_fields()))
                .unwrap_or_else(LevelOneEquityFields::default_fields);
            let request = StreamRequest {
                requestid: Uuid::new_v4().to_string(),
                service: service.clone(),
                // ADD is additive and safe on a fresh session; SUBS would
                // also work here but ADD matches the reference client.
                command: StreamCommand::Add,
                schwab_client_customer_id: self.identity.customer_id.clone(),
                schwab_client_correl_id: self.identity.correl_id.clone(),
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
        let mut outstanding: std::collections::HashSet<String> =
            requests.iter().map(|r| r.requestid.clone()).collect();
        if !requests.is_empty()
            && let Some(sender) = self.outgoing_tx.read().clone()
        {
            let request_json = serde_json::to_string(&StreamRequests { requests })?;
            sender
                .send(OutgoingMessage::Text(request_json))
                .map_err(|e| {
                    Error::Stream(StreamError::ConnectionFailed(format!(
                        "Failed to send resubscribe: {}",
                        e
                    )))
                })?;
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while !outstanding.is_empty() {
                match read_half.next().await {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                        let message: StreamMessage = serde_json::from_str(text.as_ref())?;
                        if let StreamMessage::Response(response) = &message {
                            for item in &response.response {
                                if outstanding.remove(&item.requestid) && item.content.code != 0 {
                                    return Err(Error::Stream(StreamError::SubscriptionFailed {
                                        service: item.service.clone(),
                                        code: item.content.code,
                                        message: "Replay rejected".into(),
                                    }));
                                }
                            }
                        } else {
                            self.message_tx
                                .send(message)
                                .map_err(|_| Error::ConnectionClosed)?;
                        }
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Pong(_))) => {
                        *self.last_pong.write() = Instant::now()
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(_))) => {}
                    _ => return Err(Error::ConnectionClosed),
                }
            }
            Ok(())
        })
        .await
        .map_err(|_| {
            Error::Stream(StreamError::ConnectionFailed(
                "Subscription replay timeout".into(),
            ))
        })?
    }

    async fn message_loop_read(&self, read_half: &mut SplitStream<WsStream>) -> Result<LoopExit> {
        while let Some(msg) = read_half.next().await {
            match msg {
                Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                    match serde_json::from_str::<StreamMessage>(text.as_ref()) {
                        Ok(stream_msg) => {
                            // Resolve acks for response frames
                            if let StreamMessage::Response(resp) = &stream_msg {
                                for item in &resp.response {
                                    let req_id = item.requestid.clone();
                                    if let Some(tx) = self.pending_acks.write().remove(&req_id) {
                                        let outcome = if let Some(code) =
                                            StreamResponseCode::from_code(item.content.code)
                                        {
                                            if code.is_success() {
                                                Ok(())
                                            } else {
                                                Err(Error::Stream(
                                                    StreamError::SubscriptionFailed {
                                                        service: item.service.clone(),
                                                        code: item.content.code,
                                                        message: item.content.msg.clone(),
                                                    },
                                                ))
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

                            // Bucket the frame by (service, kind) before
                            // consuming — Data frames carry per-service
                            // payloads; Response/Notify are control-plane.
                            record_stream_frame(&stream_msg);

                            // A full queue is handled inside `send` (the
                            // stream stays up and resumes under a new
                            // generation); only a dropped receiver ends it.
                            if self.message_tx.send(stream_msg).is_err() {
                                warn!("Stream receiver dropped; closing the session");
                                return Err(Error::ConnectionClosed);
                            }
                        }
                        Err(e) => {
                            // A frame we can't model is information, not
                            // noise — silently dropping it hides schema
                            // drift (or new Schwab fields) from operators.
                            warn!(
                                error = %e,
                                frame_bytes = text.len(),
                                "dropping unparseable stream frame"
                            );
                            metrics::counter!(
                                crate::metrics::STREAM_MESSAGES_TOTAL,
                                "service" => "_unparsed",
                                "kind" => "dropped",
                            )
                            .increment(1);
                        }
                    }
                }
                Ok(tokio_tungstenite::tungstenite::Message::Pong(_)) => {
                    // Update last pong time for heartbeat monitoring
                    *self.last_pong.write() = Instant::now();
                    debug!("Pong received, heartbeat alive");
                }
                Ok(tokio_tungstenite::tungstenite::Message::Close(frame)) => {
                    let is_normal_closure = frame
                        .as_ref()
                        .map(|f| f.code == CloseCode::Normal)
                        .unwrap_or(false);
                    info!("WebSocket close frame received: {:?}", frame);
                    return if is_normal_closure {
                        Ok(LoopExit::CleanClose)
                    } else {
                        Ok(LoopExit::StreamEnded)
                    };
                }
                Err(e) => {
                    error!("WebSocket error: {}", e);
                    return Err(Error::from(e));
                }
                _ => {}
            }
        }
        // Stream ended without a close frame — abnormal.
        Ok(LoopExit::StreamEnded)
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
                        "WebSocket ping timeout ({elapsed:?} since last pong, \
                         limit: {timeout_duration:?}), connection will reconnect"
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
    channel: Option<String>,
    function_id: Option<String>,
}

impl StreamClientBuilder {
    pub fn new() -> Self {
        Self {
            config: None,
            auth_manager: None,
            customer_id: None,
            correl_id: None,
            channel: None,
            function_id: None,
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

    /// `schwabClientChannel` from user preferences (defaults to `N9`).
    pub fn channel(mut self, channel: impl Into<String>) -> Self {
        self.channel = Some(channel.into());
        self
    }

    /// `schwabClientFunctionId` from user preferences (defaults to `APIAPP`).
    pub fn function_id(mut self, function_id: impl Into<String>) -> Self {
        self.function_id = Some(function_id.into());
        self
    }

    /// Populate customer/correl IDs, channel, and function ID from a
    /// `streamerInfo` entry of the user-preferences response.
    pub fn streamer_info(mut self, info: &crate::types::accounts::StreamerInfo) -> Self {
        let identity = StreamerIdentity::from_streamer_info(info);
        self.customer_id = Some(identity.customer_id);
        self.correl_id = Some(identity.correl_id);
        self.channel = Some(identity.channel);
        self.function_id = Some(identity.function_id);
        self
    }

    pub fn build(self) -> Result<StreamClient> {
        let config = self
            .config
            .ok_or_else(|| Error::Config("Stream config required".to_string()))?;
        let auth_manager = self
            .auth_manager
            .ok_or_else(|| Error::Config("Auth manager required".to_string()))?;
        let customer_id = self
            .customer_id
            .ok_or_else(|| Error::Config("Customer ID required".to_string()))?;
        let correl_id = self.correl_id.unwrap_or_else(|| Uuid::new_v4().to_string());

        let mut identity = StreamerIdentity::new(customer_id, correl_id);
        if let Some(channel) = self.channel {
            identity.channel = channel;
        }
        if let Some(function_id) = self.function_id {
            identity.function_id = function_id;
        }

        StreamClient::new(config, auth_manager, identity)
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
            app_secret: "1234567890123456".to_string(),              // 16 chars
            callback_url: "https://localhost:8080".to_string(),
            ..Default::default()
        };
        AuthManager::new(oauth_config).unwrap()
    }

    #[test]
    fn reconnect_delays_are_jittered_within_bounds() {
        let base = Duration::from_secs(8);
        let delays: Vec<_> = (0..200).map(|_| jittered(base, 0.25)).collect();
        assert!(
            delays
                .iter()
                .all(|d| *d >= base.mul_f64(0.75) && *d <= base.mul_f64(1.25))
        );
        assert!(delays.iter().any(|d| *d != delays[0]), "no spread");
        assert_eq!(jittered(base, 0.0), base);
        assert!(jittered(base, f64::NAN) == base);
        assert!(jittered(base, 5.0) <= base * 2);
    }

    /// Overflow drops frames (counted) while the queue is full, then, once
    /// half of it has drained, resumes delivery under a new generation
    /// without tearing the connection down.
    #[test]
    fn overflow_recovers_under_a_new_generation_once_drained() {
        let (health, _status) = tokio::sync::watch::channel(StreamStatus {
            ready: true,
            generation: 1,
            lost_messages: 0,
        });
        let (tx, mut rx) = mpsc::channel(2);
        let sender = MessageSender::new(tx, health.clone());
        let beat =
            || StreamMessage::Notify(crate::types::streaming::StreamNotify { notify: vec![] });
        sender.send(beat()).unwrap();
        sender.send(beat()).unwrap();
        // Full: dropped, counted, not ready; the socket is not closed.
        sender.send(beat()).unwrap();
        let status = health.borrow().clone();
        assert!(!status.ready);
        assert_eq!((status.generation, status.lost_messages), (1, 1));
        // Still full: dropped again.
        sender.send(beat()).unwrap();
        assert_eq!(health.borrow().lost_messages, 2);
        // The consumer drains; the next frame opens generation 2, ready.
        assert_eq!(rx.try_recv().unwrap().0, 1);
        assert_eq!(rx.try_recv().unwrap().0, 1);
        sender.send(beat()).unwrap();
        let status = health.borrow().clone();
        assert!(status.ready);
        assert_eq!(status.generation, 2);
        assert_eq!(rx.try_recv().unwrap().0, 2);
        // A dropped receiver is the only error.
        drop(rx);
        assert!(sender.send(beat()).is_err());
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

        assert!(
            result.is_ok(),
            "Builder should succeed with custom correl_id"
        );
    }

    #[test]
    fn test_every_channel_kind_yields_a_receiver() {
        for channel_kind in [ChannelKind::Unbounded, ChannelKind::Bounded(1000)] {
            let config = StreamConfig {
                channel_kind,
                ..Default::default()
            };
            let client = StreamClient::new(
                config,
                create_test_auth_manager(),
                StreamerIdentity::new("test_customer_id", "correl"),
            )
            .unwrap();
            assert!(client.get_receiver().is_some());
        }
    }

    #[test]
    fn test_receiver_can_only_be_taken_once() {
        let config = StreamConfig::default();
        let auth_manager = create_test_auth_manager();
        let customer_id = "test_customer_id".to_string();

        let client = StreamClient::new(
            config,
            auth_manager,
            StreamerIdentity::new(customer_id, "correl"),
        )
        .unwrap();

        // First call should succeed
        let receiver1 = client.get_receiver();
        assert!(receiver1.is_some(), "First get_receiver should succeed");

        // Second call should return None
        let receiver2 = client.get_receiver();
        assert!(
            receiver2.is_none(),
            "Second get_receiver should return None"
        );
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
            ChannelKind::Bounded(4096),
            "Default channel kind should be bounded"
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

        let client = StreamClient::new(
            config,
            auth_manager,
            StreamerIdentity::new(customer_id, "correl"),
        )
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

        let client = StreamClient::new(
            config,
            auth_manager,
            StreamerIdentity::new(customer_id, "correl"),
        )
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

    /// A ready receiver plus the sender halves that feed it.
    fn tracked_receiver(
        capacity: usize,
        initial: StreamStatus,
    ) -> (
        mpsc::Sender<(u64, StreamMessage)>,
        tokio::sync::watch::Sender<StreamStatus>,
        MessageReceiver,
    ) {
        let (tx, rx) = mpsc::channel(capacity);
        let (status, state) = tokio::sync::watch::channel(initial);
        (tx, status, MessageReceiver::new(rx, state))
    }

    fn heartbeat(id: &str) -> StreamMessage {
        StreamMessage::Notify(crate::types::streaming::StreamNotify {
            notify: vec![crate::types::streaming::NotifyItem {
                heartbeat: Some(id.to_string()),
                extra: Default::default(),
            }],
        })
    }

    #[tokio::test]
    async fn receiver_delivers_current_generation_and_drops_stale_messages() {
        let ready = StreamStatus {
            ready: true,
            generation: 2,
            lost_messages: 0,
        };
        let (tx, _status, mut receiver) = tracked_receiver(10, ready);
        tx.send((1, heartbeat("stale"))).await.unwrap();
        tx.send((2, heartbeat("current"))).await.unwrap();

        match receiver.recv().await {
            Some(StreamMessage::Notify(notify)) => {
                assert_eq!(notify.notify[0].heartbeat.as_deref(), Some("current"));
            }
            other => panic!("expected the current-generation heartbeat, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn receiver_ends_when_sender_and_status_are_dropped() {
        let (tx, status, mut receiver) = tracked_receiver(1, StreamStatus::default());
        drop(tx);
        drop(status);
        assert!(receiver.recv().await.is_none());
    }

    #[tokio::test]
    async fn state_notifications_drain_stale_queue_without_losing_current_data() {
        let (tx, status, mut receiver) = tracked_receiver(
            2,
            StreamStatus {
                ready: false,
                generation: 1,
                lost_messages: 0,
            },
        );
        let message = || {
            StreamMessage::State(StreamStatus {
                ready: true,
                generation: 2,
                lost_messages: 0,
            })
        };
        tx.send((1, message())).await.unwrap();
        tx.send((2, message())).await.unwrap();
        status
            .send(StreamStatus {
                ready: true,
                generation: 2,
                lost_messages: 1,
            })
            .unwrap();
        assert!(
            matches!(receiver.recv().await, Some(StreamMessage::State(s)) if s.lost_messages == 1)
        );
        assert_eq!(
            tx.capacity(),
            2,
            "state delivery must free stale queue capacity"
        );
        assert!(matches!(
            receiver.recv().await,
            Some(StreamMessage::State(s)) if s.lost_messages == 0 && s.generation == 2
        ));
    }

    #[test]
    fn test_exponential_backoff_initial_value() {
        let config = StreamConfig::default();
        let auth_manager = create_test_auth_manager();
        let customer_id = "test_customer_id".to_string();

        let client = StreamClient::new(
            config,
            auth_manager,
            StreamerIdentity::new(customer_id, "correl"),
        )
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

        let client = StreamClient::new(
            config,
            auth_manager,
            StreamerIdentity::new(customer_id, "correl"),
        )
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

        let client = StreamClient::new(
            config,
            auth_manager,
            StreamerIdentity::new(customer_id, "correl"),
        )
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
