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

/// A queued message with the stream state it was produced under.
#[derive(Debug)]
pub(crate) struct Queued {
    generation: u64,
    /// Whether the stream was ready. Messages produced while it was not
    /// (between a reconnect and its resubscription) are never delivered.
    ready: bool,
    message: StreamMessage,
}

/// Generation-tagged sender feeding the subscriber queue.
///
/// Every message carries the generation and readiness it was produced under,
/// so the receiver can tell pre-gap data from what follows a reconnect or a
/// gap. Queue overflow is never silent: it flips the shared [`StreamStatus`]
/// to not-ready and bumps `lost_messages` once per dropped frame. The
/// connection stays up: frames are dropped (and counted) until the consumer
/// has drained half the queue, then delivery resumes under a new generation,
/// so the consumer re-baselines as after a reconnect without a new LOGIN or
/// resubscription.
#[derive(Debug)]
pub(crate) struct MessageSender {
    tx: mpsc::Sender<Queued>,
    state: tokio::sync::watch::Sender<StreamStatus>,
    /// Frames are being dropped until the consumer drains half the queue.
    overflowed: AtomicBool,
    /// The current session has logged in and replayed its subscriptions.
    /// An overflow that drains resumes ready only if it is; a session that
    /// becomes ready during an overflow is reported ready once it drains.
    session_ready: AtomicBool,
}

impl MessageSender {
    pub(crate) fn new(
        tx: mpsc::Sender<Queued>,
        state: tokio::sync::watch::Sender<StreamStatus>,
    ) -> Self {
        Self {
            tx,
            state,
            overflowed: AtomicBool::new(false),
            session_ready: AtomicBool::new(false),
        }
    }

    /// Send a message tagged with the current generation. A full queue
    /// drops it and invalidates the stream until the consumer catches up.
    ///
    /// Returns `Err` only if the receiver has been dropped.
    pub(crate) fn send(&self, msg: StreamMessage) -> std::result::Result<(), String> {
        if self.overflowed.load(Ordering::Acquire) {
            if self.tx.capacity() * 2 < self.tx.max_capacity() {
                self.record_loss();
                return Ok(());
            }
            // Drained: what follows starts a new generation, ready if the
            // session is.
            self.overflowed.store(false, Ordering::Release);
            let ready = self.session_ready.load(Ordering::Acquire);
            self.state.send_modify(|s| {
                s.generation += 1;
                s.ready = ready;
            });
        }
        let (generation, ready) = {
            let state = self.state.borrow();
            (state.generation, state.ready)
        };
        let queued = Queued {
            generation,
            ready,
            message: msg,
        };
        match self.tx.try_send(queued) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.overflowed.store(true, Ordering::Release);
                self.record_loss();
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.record_loss();
                Err("Stream receiver closed".into())
            }
        }
    }

    /// A new connection starts clean and not ready, under a new generation:
    /// any gap belongs to the old one.
    pub(crate) fn begin_session(&self) {
        self.session_ready.store(false, Ordering::Release);
        self.overflowed.store(false, Ordering::Release);
        self.state.send_modify(|s| {
            s.generation += 1;
            s.ready = false;
            s.ended = None;
        });
    }

    /// The session logged in and replayed its subscriptions. During an
    /// overflow the stream turns ready when the queue drains.
    pub(crate) fn session_ready(&self) {
        self.session_ready.store(true, Ordering::Release);
        if !self.overflowed.load(Ordering::Acquire) {
            self.state.send_modify(|s| s.ready = true);
        }
    }

    /// The session is gone (for now): nothing is ready until the next one.
    pub(crate) fn session_lost(&self) {
        self.session_ready.store(false, Ordering::Release);
        self.state.send_modify(|s| s.ready = false);
    }

    /// The stream stopped for good; receivers end once drained.
    pub(crate) fn end(&self, reason: StreamEnd) {
        self.session_ready.store(false, Ordering::Release);
        self.state.send_modify(|s| {
            s.ready = false;
            s.ended = Some(reason);
        });
    }

    pub(crate) fn record_dropped(&self) {
        self.record_loss();
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
/// Messages arrive in order, each under the [`StreamStatus`] last reported
/// to the consumer: a [`StreamMessage::State`] is interleaved whenever
/// readiness or the generation changes, so consumers can re-baseline. What
/// was queued before a gap (an overflow or a reconnect) is complete,
/// in-order data, so it is delivered ahead of the `State` that reports the
/// gap; `lost_messages` counts exactly the frames that were dropped.
/// Messages produced while the stream was not ready are never delivered.
#[derive(Debug)]
pub struct MessageReceiver {
    rx: mpsc::Receiver<Queued>,
    pending: Option<Queued>,
    state: tokio::sync::watch::Receiver<StreamStatus>,
    /// The status last reported to the consumer (at first, the status when
    /// the receiver was created).
    reported: StreamStatus,
    /// The status changed since it was last reported. Awaiting a change
    /// marks it seen, so this remembers it until it is reported.
    unreported: bool,
}

impl MessageReceiver {
    pub(crate) fn new(
        rx: mpsc::Receiver<Queued>,
        mut state: tokio::sync::watch::Receiver<StreamStatus>,
    ) -> Self {
        let reported = state.borrow_and_update().clone();
        Self {
            rx,
            pending: None,
            state,
            reported,
            unreported: false,
        }
    }

    /// Receive the next deliverable message.
    ///
    /// Returns `None` once the stream has ended (a [`StreamMessage::State`]
    /// with [`StreamStatus::ended`] set has been delivered) or the client
    /// has been dropped, and everything deliverable has been received. A
    /// later [`StreamClient::connect`] starts a new session this receiver
    /// can keep reading.
    pub async fn recv(&mut self) -> Option<StreamMessage> {
        loop {
            let next = self.pending.take().or_else(|| self.rx.try_recv().ok());
            if let Some(queued) = next {
                if self.deliverable(&queued) {
                    return Some(queued.message);
                }
                if self.changed() {
                    // Newer than what the consumer knows: report the change
                    // first, then judge the message against it.
                    self.pending = Some(queued);
                    return Some(self.report());
                }
                // Produced before a restart the consumer was already told
                // of, or while the stream was not ready.
                continue;
            }
            if self.changed() {
                return Some(self.report());
            }
            if self.reported.ended.is_some() {
                return None;
            }
            let Self {
                rx,
                state,
                unreported,
                pending,
                ..
            } = self;
            if state.has_changed().is_ok() {
                tokio::select! {
                    changed = state.changed() => *unreported |= changed.is_ok(),
                    item = rx.recv() => *pending = Some(item?),
                }
            } else {
                // The client is gone: drain what it queued, then end.
                *pending = Some(rx.recv().await?);
            }
        }
    }

    /// Whether the status changed since it was last reported.
    fn changed(&self) -> bool {
        self.unreported || self.state.has_changed().unwrap_or(false)
    }

    fn deliverable(&self, queued: &Queued) -> bool {
        self.reported.ready && queued.ready && queued.generation == self.reported.generation
    }

    /// Report the current status, dropping queued messages it makes stale so
    /// an overflowed sender sees the capacity it waits for.
    fn report(&mut self) -> StreamMessage {
        self.reported = self.state.borrow_and_update().clone();
        self.unreported = false;
        let mut next = self.pending.take().or_else(|| self.rx.try_recv().ok());
        loop {
            match next {
                Some(queued) if self.deliverable(&queued) => {
                    self.pending = Some(queued);
                    break;
                }
                Some(_) => next = self.rx.try_recv().ok(),
                None => break,
            }
        }
        StreamMessage::State(self.reported.clone())
    }
}

pub type StreamMessage = crate::types::streaming::StreamMessage;

/// Frames a session may have waiting for the socket: requests are
/// acknowledged one at a time and pings are periodic, so a full queue
/// means the writer has stalled and the session is broken.
const OUTGOING_CAPACITY: usize = 1024;

/// Frames held while subscriptions replay on a new session, delivered once
/// it is ready; beyond this they are dropped and counted as lost.
const REPLAY_HOLD_LIMIT: usize = 4096;

/// The command that subscribes `service`'s keys. `ADD` is additive (`SUBS`
/// would replace the service's key list server-side); account activity has
/// one fixed key and Schwab documents only `SUBS`/`UNSUBS` for it.
fn subscribe_command(service: &str) -> StreamCommand {
    if service == StreamService::AcctActivity.as_str() {
        StreamCommand::Subs
    } else {
        StreamCommand::Add
    }
}

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
            inner.message_tx.end(StreamEnd::Disconnected);
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
    outgoing_tx: Arc<RwLock<Option<mpsc::Sender<OutgoingMessage>>>>,
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
    /// The access token the last LOGIN carried, so a login Schwab denies
    /// can be answered with a refresh of exactly that token.
    login_token: parking_lot::Mutex<Option<secrecy::SecretString>>,
    /// Why the server is about to close the session, when it said so
    /// (response code 12 or 30) before closing it normally.
    closing: parking_lot::Mutex<Option<StreamEnd>>,
    /// Counts sessions, so an acknowledgement that times out can only tear
    /// down the session it was sent on.
    session: std::sync::atomic::AtomicU64,
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
            if self.inner.health.borrow().ended.is_none() {
                self.inner.message_tx.end(StreamEnd::ConnectFailed {
                    message: "connect() did not complete".into(),
                });
            }
        }
    }
}
struct AckGuard<'a> {
    inner: &'a StreamClientInner,
    id: String,
    /// The session the request was sent on.
    session: u64,
    confirmed: bool,
}
impl Drop for AckGuard<'_> {
    fn drop(&mut self) {
        self.inner.pending_acks.write().remove(&self.id);
        // An unanswered request means the session is broken, but only that
        // session: `notify_waiters` stores no permit, so a request that
        // outlived its session cannot tear down the next one.
        if !self.confirmed && self.inner.session.load(Ordering::SeqCst) == self.session {
            self.inner.reconnect_requested.notify_waiters();
        }
    }
}

impl StreamClient {
    pub fn new(
        config: StreamConfig,
        auth_manager: AuthManager,
        identity: StreamerIdentity,
    ) -> Result<Self> {
        // The LOGIN carries the access token: only to Schwab's streamer (or
        // loopback), however the configuration was built.
        crate::config::SchwabConfig::validate_url_domain(&config.websocket_url, "websocket_url")?;
        let transport = WebSocketTransport::new(&config.websocket_url)?;

        if config.heartbeat_interval.is_zero() || config.ping_timeout <= config.heartbeat_interval {
            return Err(Error::Config(
                "Stream heartbeat interval must be positive and shorter than the ping timeout"
                    .into(),
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
            login_token: parking_lot::Mutex::new(None),
            closing: parking_lot::Mutex::new(None),
            session: std::sync::atomic::AtomicU64::new(0),
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
    ///
    /// Account activity has a fixed field set and no `VIEW`; it is refused.
    pub async fn view(&self, service: StreamService, fields_csv: String) -> Result<()> {
        if service == StreamService::AcctActivity {
            return Err(Error::InvalidParameter(
                "Account activity streams a fixed field set; VIEW does not apply".into(),
            ));
        }
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
            let queued = serde_json::to_string(&StreamRequests {
                requests: vec![logout],
            })
            .is_ok_and(|json| self.inner.queue(OutgoingMessage::Text(json)).is_ok());
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
        self.inner.message_tx.end(StreamEnd::Disconnected);
    }

    /// Subscribe to `service` for the given keys.
    ///
    /// Uses the streamer `ADD` command (additive), matching the reference
    /// client — `SUBS` would *replace* the service's entire key list
    /// server-side, silently dropping earlier subscriptions while the
    /// local tracker still believed they were active. Account activity,
    /// which has one fixed key, uses `SUBS` as Schwab documents.
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
            subscribe_command(service.as_str()),
            &service,
            StreamParameters {
                keys: Some(symbols.join(",")),
                fields: Some(fields),
                authorization: None,
                schwab_client_channel: None,
                schwab_client_function_id: None,
            },
        );

        self.send_request(request).await?;
        self.inner
            .subscriptions
            .add_subscription(service.as_str().to_string(), symbols);
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
            session: self.inner.session.load(Ordering::SeqCst),
            confirmed: false,
        };
        let json = serde_json::to_string(&StreamRequests {
            requests: vec![request],
        })?;
        if let Err(error) = self.inner.queue(OutgoingMessage::Text(json)) {
            // Never sent, so no answer is owed: not a broken session.
            ack_guard.confirmed = true;
            return Err(error);
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
        _ => {}
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
        self.message_tx.session_lost();
        metrics::gauge!(crate::metrics::STREAM_CONNECTED).set(0.0);
    }

    async fn connection_loop(&self) -> Result<()> {
        let mut retry_count = 0;
        let mut is_reconnect = false;
        // A login denied after the stream was healthy is answered once with
        // a refresh of the token it carried (Schwab's docs: reconnect and
        // log in with a new token); a fresh token denied too is final.
        let mut login_retried = false;
        self.authenticated_once.store(false, Ordering::SeqCst);

        loop {
            if self.shutting_down.load(Ordering::SeqCst) {
                info!("Stream shutdown requested; exiting reconnect loop");
                self.message_tx.end(StreamEnd::Disconnected);
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
            if self.authenticated_once.load(Ordering::SeqCst) && result.is_ok() {
                login_retried = false;
            }
            // Schwab said it is ending the session (another login took the
            // streamer slot, or nothing is subscribed): reconnecting would
            // only fight the session that replaced this one.
            if let Some(reason) = self.closing.lock().take() {
                info!(?reason, "Streamer ended the session; not reconnecting");
                self.message_tx.end(reason);
                return Ok(());
            }
            match result {
                Ok(_) => {
                    // Server closed the socket with a normal-closure frame
                    // (code 1000). Matches the reference client: a graceful
                    // close is an instruction to stop, not a fault to retry
                    // — e.g. Schwab tears the session down when another
                    // login takes over the streamer slot (code 12).
                    info!("Stream connection closed gracefully; not reconnecting");
                    self.message_tx.end(StreamEnd::ServerClosed);
                    return Ok(());
                }
                Err(e) => {
                    error!(error = %e, "Stream connection error");

                    let denied = match &e {
                        Error::Stream(StreamError::AuthenticationFailed(message)) => {
                            Some(message.clone())
                        }
                        _ => None,
                    };
                    // Never got in at all (bad socket URL, unreachable
                    // host, rejected login): retrying is pointless and
                    // `connect()` should fail fast.
                    let never_authenticated = !self.authenticated_once.load(Ordering::SeqCst);
                    if never_authenticated {
                        drop(self.initial_auth_tx.write().take());
                        self.message_tx.end(match denied {
                            Some(message) => StreamEnd::LoginDenied { message },
                            None => StreamEnd::ConnectFailed {
                                message: e.to_string(),
                            },
                        });
                        return Err(e);
                    }
                    if let Some(message) = denied {
                        let token = self.login_token.lock().take();
                        let refreshed = match token {
                            Some(token) if !login_retried => self
                                .auth_manager
                                .refresh_rejected_token(secrecy::ExposeSecret::expose_secret(
                                    &token,
                                ))
                                .await
                                .is_ok(),
                            _ => false,
                        };
                        if !refreshed {
                            self.message_tx.end(StreamEnd::LoginDenied { message });
                            return Err(e);
                        }
                        warn!("Streamer denied the login; retrying with a refreshed token");
                        login_retried = true;
                    }

                    if !self.config.reconnect.enabled {
                        self.message_tx.end(StreamEnd::ReconnectExhausted);
                        return Err(e);
                    }

                    if let Some(max_retries) = self.config.reconnect.max_retries
                        && retry_count >= max_retries
                    {
                        error!("Maximum reconnection attempts reached");
                        self.message_tx.end(StreamEnd::ReconnectExhausted);
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
        self.session.fetch_add(1, Ordering::SeqCst);
        *self.closing.lock() = None;
        self.message_tx.begin_session();

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
        let (tx, mut rx) = mpsc::channel::<OutgoingMessage>(OUTGOING_CAPACITY);
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
        let replayed = self.resubscribe_via_channel(&mut read_half).await?;
        *self.connection_state.write() = ConnectionState::Authenticated;
        self.message_tx.session_ready();
        // What arrived during the replay belongs to this session: the first
        // full snapshot of each key, and any replay refusal.
        for message in replayed {
            if self.message_tx.send(message).is_err() {
                return Err(Error::ConnectionClosed);
            }
        }
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

        // Run the read loop and the heartbeat watchdog side by side.
        // The watchdog task completes when the ping timeout elapses (or
        // the writer dies), and the select! tears the connection down —
        // without this, a silently dead TCP connection would leave the
        // read loop blocked indefinitely with no reconnect.
        let reconnect = self.reconnect_requested.notified();
        let mut heartbeat_task = self.start_heartbeat();
        let _heartbeat_guard = AbortTask(heartbeat_task.abort_handle());
        let result = tokio::select! {
            res = self.message_loop_read(&mut read_half) => match res {
                Ok(LoopExit::CleanClose) => Ok(()),
                Ok(LoopExit::StreamEnded) => Err(Error::ConnectionClosed),
                Err(e) => Err(e),
            },
            _ = reconnect => Err(Error::ConnectionClosed),
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
        *self.login_token.lock() = Some(secrecy::SecretString::from(access_token.clone()));

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

        let request_json = serde_json::to_string(&StreamRequests {
            requests: vec![login_request],
        })?;
        self.queue(OutgoingMessage::Text(request_json))?;

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

    /// Queue a frame for the writer task.
    fn queue(&self, message: OutgoingMessage) -> Result<()> {
        let sender = self.outgoing_tx.read().clone().ok_or_else(|| {
            Error::Stream(StreamError::ConnectionFailed(
                "No active connection".to_string(),
            ))
        })?;
        sender.try_send(message).map_err(|e| {
            Error::Stream(StreamError::ConnectionFailed(format!(
                "Failed to queue request: {e}"
            )))
        })
    }

    /// Replay every tracked subscription on a fresh session, one service at
    /// a time as the reference client does: Schwab can fail one of several
    /// commands processed in parallel (code 22). Returns what arrived in
    /// the meantime, to be delivered once the session is ready: the first
    /// full snapshot of each replayed key, and the response for a service
    /// Schwab refused, which is also dropped from the tracker rather than
    /// failing every reconnect.
    async fn resubscribe_via_channel(
        &self,
        read_half: &mut SplitStream<WsStream>,
    ) -> Result<Vec<StreamMessage>> {
        let _subscription = self.subscription_lock.lock().await;
        let mut held = Vec::new();
        let mut services: Vec<_> = self
            .subscriptions
            .get_all_subscriptions()
            .into_iter()
            .filter(|(_, symbols)| !symbols.is_empty())
            .collect();
        services.sort();
        for (service, symbols) in services {
            let fields = self
                .subscriptions
                .get_service_fields(&service)
                .or_else(|| StreamService::from_api_str(&service).map(|s| s.default_fields()))
                .unwrap_or_else(LevelOneEquityFields::default_fields);
            let request = StreamRequest {
                requestid: Uuid::new_v4().to_string(),
                service: service.clone(),
                command: subscribe_command(&service),
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
            let id = request.requestid.clone();
            self.queue(OutgoingMessage::Text(serde_json::to_string(
                &StreamRequests {
                    requests: vec![request],
                },
            )?))?;
            let answer = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match read_half.next().await {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                            let Ok(message) = serde_json::from_str::<StreamMessage>(text.as_ref())
                            else {
                                warn!(
                                    frame_bytes = text.len(),
                                    "dropping unparseable stream frame"
                                );
                                continue;
                            };
                            let code = match &message {
                                StreamMessage::Response(response) => response
                                    .response
                                    .iter()
                                    .find(|item| item.requestid == id)
                                    .map(|item| item.content.code),
                                _ => None,
                            };
                            // The replay's own acknowledgement is not news;
                            // its refusal is.
                            let succeeded = code
                                .and_then(StreamResponseCode::from_code)
                                .is_some_and(|c| c.is_success());
                            if !succeeded {
                                if held.len() < REPLAY_HOLD_LIMIT {
                                    held.push(message);
                                } else {
                                    self.message_tx.record_dropped();
                                }
                            }
                            if let Some(code) = code {
                                return Ok(code);
                            }
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Pong(_))) => {
                            *self.last_pong.write() = Instant::now()
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(_))) => {}
                        _ => return Err(Error::ConnectionClosed),
                    }
                }
            })
            .await
            .map_err(|_| {
                Error::Stream(StreamError::ConnectionFailed(
                    "Subscription replay timeout".into(),
                ))
            })??;
            if !StreamResponseCode::from_code(answer).is_some_and(|c| c.is_success()) {
                warn!(
                    service = %service,
                    code = answer,
                    "Streamer refused a replayed subscription; dropping it"
                );
                self.subscriptions.clear_service(&service);
            }
        }
        Ok(held)
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
                                    // Schwab says why before it closes a
                                    // session normally.
                                    match StreamResponseCode::from_code(item.content.code) {
                                        Some(StreamResponseCode::CloseConnection) => {
                                            *self.closing.lock() = Some(StreamEnd::Replaced)
                                        }
                                        Some(StreamResponseCode::StopStreaming) => {
                                            *self.closing.lock() = Some(StreamEnd::Idle)
                                        }
                                        _ => {}
                                    }
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

        // The clock starts now, not at the handshake: login and replay must
        // not count against the first ping.
        *last_pong.write() = Instant::now();
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
                    if let Err(e) = sender.try_send(OutgoingMessage::Ping) {
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
    socket_url: Option<String>,
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
            socket_url: None,
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

    /// Populate customer/correl IDs, channel, function ID and the socket URL
    /// from a `streamerInfo` entry of the user-preferences response. The URL
    /// is checked when the client is built (see
    /// [`StreamConfig::set_socket_url`](crate::config::StreamConfig::set_socket_url)).
    pub fn streamer_info(mut self, info: &crate::types::accounts::StreamerInfo) -> Self {
        if !info.streamer_socket_url.is_empty() {
            self.socket_url = Some(info.streamer_socket_url.clone());
        }
        let identity = StreamerIdentity::from_streamer_info(info);
        self.customer_id = Some(identity.customer_id);
        self.correl_id = Some(identity.correl_id);
        self.channel = Some(identity.channel);
        self.function_id = Some(identity.function_id);
        self
    }

    pub fn build(self) -> Result<StreamClient> {
        let mut config = self
            .config
            .ok_or_else(|| Error::Config("Stream config required".to_string()))?;
        if let Some(url) = &self.socket_url {
            config.set_socket_url(url)?;
        }
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
            ..Default::default()
        });
        let (tx, mut rx) = mpsc::channel(2);
        let sender = MessageSender::new(tx, health.clone());
        sender.session_ready();
        let beat = || serde_json::from_str::<StreamMessage>(r#"{"notify":[]}"#).unwrap();
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
        assert_eq!(rx.try_recv().unwrap().generation, 1);
        assert_eq!(rx.try_recv().unwrap().generation, 1);
        sender.send(beat()).unwrap();
        let status = health.borrow().clone();
        assert!(status.ready);
        assert_eq!(status.generation, 2);
        assert_eq!(rx.try_recv().unwrap().generation, 2);
        // A dropped receiver is the only error.
        drop(rx);
        assert!(sender.send(beat()).is_err());
    }

    /// An overflow that begins while a new session replays its
    /// subscriptions, and drains after the session turned ready, resumes
    /// ready: readiness follows the session, not the moment the gap began.
    #[test]
    fn an_overflow_during_replay_resumes_ready_once_the_session_is() {
        let (health, _status) = tokio::sync::watch::channel(StreamStatus::default());
        let (tx, mut rx) = mpsc::channel(2);
        let sender = MessageSender::new(tx, health.clone());
        sender.begin_session();
        let beat = || serde_json::from_str::<StreamMessage>(r#"{"notify":[]}"#).unwrap();
        for _ in 0..3 {
            sender.send(beat()).unwrap();
        }
        // Ready while overflowed: reported once the queue drains.
        sender.session_ready();
        assert!(!health.borrow().ready);
        rx.try_recv().unwrap();
        rx.try_recv().unwrap();
        sender.send(beat()).unwrap();
        let status = health.borrow().clone();
        assert!(status.ready);
        let delivered = rx.try_recv().unwrap();
        assert!(delivered.ready);
        assert_eq!(delivered.generation, status.generation);
    }

    /// A stream that ends says why, and its receiver then ends too.
    #[tokio::test]
    async fn an_ended_stream_ends_its_receiver() {
        let (health, status) = tokio::sync::watch::channel(StreamStatus::default());
        let (tx, rx) = mpsc::channel(4);
        let sender = MessageSender::new(tx, health);
        let mut receiver = MessageReceiver::new(rx, status);
        sender.end(StreamEnd::Replaced);
        let Some(StreamMessage::State(state)) = receiver.recv().await else {
            panic!("the end is reported first");
        };
        assert_eq!(state.ended, Some(StreamEnd::Replaced));
        assert!(receiver.recv().await.is_none());
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
        mpsc::Sender<Queued>,
        tokio::sync::watch::Sender<StreamStatus>,
        MessageReceiver,
    ) {
        let (tx, rx) = mpsc::channel(capacity);
        let (status, state) = tokio::sync::watch::channel(initial);
        (tx, status, MessageReceiver::new(rx, state))
    }

    fn queued(generation: u64, ready: bool, message: StreamMessage) -> Queued {
        Queued {
            generation,
            ready,
            message,
        }
    }

    fn heartbeat_id(message: Option<StreamMessage>) -> Option<String> {
        match message {
            Some(StreamMessage::Notify(notify)) => notify.notify[0].heartbeat.clone(),
            _ => None,
        }
    }

    /// A status change while the receiver waits on an empty queue is
    /// reported, not swallowed by the wait that noticed it.
    #[tokio::test]
    async fn a_change_while_waiting_is_reported() {
        let ready = StreamStatus {
            ready: true,
            generation: 1,
            lost_messages: 0,
            ..Default::default()
        };
        let (_tx, status, mut receiver) = tracked_receiver(1, ready);
        let waiting = tokio::spawn(async move { receiver.recv().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        status.send_modify(|s| s.ready = false);
        let reported = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("the change must wake the receiver")
            .unwrap();
        assert!(matches!(reported, Some(StreamMessage::State(s)) if !s.ready));
    }

    /// What was queued before an overflow or a reconnect is delivered, in
    /// order, ahead of the state change that reports the gap; data queued
    /// while the stream was not ready never is.
    #[tokio::test]
    async fn pre_gap_messages_arrive_before_the_gap_is_reported() {
        let ready = StreamStatus {
            ready: true,
            generation: 1,
            lost_messages: 0,
            ..Default::default()
        };
        let (tx, status, mut receiver) = tracked_receiver(4, ready);
        tx.send(queued(1, true, heartbeat("a"))).await.unwrap();
        tx.send(queued(1, true, heartbeat("b"))).await.unwrap();
        // Overflow: the gap is recorded after both were queued.
        status.send_modify(|s| {
            s.ready = false;
            s.lost_messages = 3;
        });
        assert_eq!(heartbeat_id(receiver.recv().await).as_deref(), Some("a"));
        assert_eq!(heartbeat_id(receiver.recv().await).as_deref(), Some("b"));
        assert!(matches!(
            receiver.recv().await,
            Some(StreamMessage::State(s)) if !s.ready && s.lost_messages == 3
        ));
        // A reconnect: frames before it are delivered, frames before its
        // resubscription completes are not, and what follows is.
        status.send_modify(|s| {
            s.ready = true;
            s.generation = 2;
        });
        assert!(matches!(
            receiver.recv().await,
            Some(StreamMessage::State(s)) if s.generation == 2 && s.ready
        ));
        tx.send(queued(2, true, heartbeat("c"))).await.unwrap();
        status.send_modify(|s| {
            s.ready = false;
            s.generation = 3;
        });
        tx.send(queued(3, false, heartbeat("unready")))
            .await
            .unwrap();
        assert_eq!(heartbeat_id(receiver.recv().await).as_deref(), Some("c"));
        assert!(matches!(
            receiver.recv().await,
            Some(StreamMessage::State(s)) if s.generation == 3 && !s.ready
        ));
        status.send_modify(|s| s.ready = true);
        tx.send(queued(3, true, heartbeat("d"))).await.unwrap();
        assert!(matches!(
            receiver.recv().await,
            Some(StreamMessage::State(s)) if s.generation == 3 && s.ready
        ));
        assert_eq!(heartbeat_id(receiver.recv().await).as_deref(), Some("d"));
    }

    fn heartbeat(id: &str) -> StreamMessage {
        serde_json::from_value(serde_json::json!({"notify": [{"heartbeat": id}]})).unwrap()
    }

    #[tokio::test]
    async fn receiver_delivers_current_generation_and_drops_stale_messages() {
        let ready = StreamStatus {
            ready: true,
            generation: 2,
            lost_messages: 0,
            ..Default::default()
        };
        let (tx, _status, mut receiver) = tracked_receiver(10, ready);
        tx.send(queued(1, true, heartbeat("stale"))).await.unwrap();
        tx.send(queued(2, true, heartbeat("current")))
            .await
            .unwrap();

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
                ..Default::default()
            },
        );
        let message = || {
            StreamMessage::State(StreamStatus {
                ready: true,
                generation: 2,
                lost_messages: 0,
                ..Default::default()
            })
        };
        tx.send(queued(1, false, message())).await.unwrap();
        tx.send(queued(2, true, message())).await.unwrap();
        status
            .send(StreamStatus {
                ready: true,
                generation: 2,
                lost_messages: 1,
                ..Default::default()
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
