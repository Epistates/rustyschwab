//! The queue between a stream's connection and its consumer.
//!
//! The connection task feeds a [`MessageSender`]; the consumer reads a
//! [`MessageReceiver`]. Each message carries the generation and readiness
//! it was produced under, so the receiver reports a gap (a reconnect, an
//! overflow, a frame that could not be parsed) as a state change before
//! anything produced after it, and drops what the gap made stale.

use crate::types::streaming::{StreamEnd, StreamMessage, StreamStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

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

    /// A frame arrived that could not be parsed, so its data is lost. The
    /// connection is sound, so unlike an overflow nothing is waiting to turn
    /// the stream ready again: count the loss and start a new generation, ready
    /// if the session is, which tells the consumer to re-baseline (level-one
    /// updates are deltas, and the lost frame may have carried one).
    pub(crate) fn record_unparsed(&self) {
        let ready =
            self.session_ready.load(Ordering::Acquire) && !self.overflowed.load(Ordering::Acquire);
        self.state.send_modify(|s| {
            s.lost_messages += 1;
            s.generation += 1;
            s.ready = ready;
        });
        metrics::counter!(crate::metrics::STREAM_DROPPED_MESSAGES_TOTAL).increment(1);
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
    /// later [`StreamClient::connect`](crate::StreamClient::connect) starts a new session this receiver
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
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

    /// A frame that cannot be parsed is lost data. The consumer is told (a
    /// count and a new generation) and the stream stays ready, so what
    /// follows is delivered; nothing else would ever turn it back on.
    #[tokio::test]
    async fn an_unparseable_frame_is_reported_as_a_gap_and_delivery_continues() {
        let (tx, rx) = mpsc::channel(8);
        let (status, state) = tokio::sync::watch::channel(StreamStatus::default());
        let sender = MessageSender::new(tx, status);
        let mut receiver = MessageReceiver::new(rx, state);
        sender.begin_session();
        sender.session_ready();
        assert!(matches!(
            receiver.recv().await,
            Some(StreamMessage::State(s)) if s.ready && s.lost_messages == 0
        ));

        sender.send(heartbeat("before")).unwrap();
        sender.record_unparsed();
        sender.send(heartbeat("after")).unwrap();
        // What arrived before the lost frame comes first, then the gap...
        assert_eq!(
            heartbeat_id(receiver.recv().await).as_deref(),
            Some("before")
        );
        let Some(StreamMessage::State(gap)) = receiver.recv().await else {
            panic!("the lost frame must be reported");
        };
        assert!(gap.ready, "the connection is sound; delivery continues");
        assert_eq!(gap.lost_messages, 1);
        assert_eq!(gap.generation, 2);
        // ...and what follows the gap is delivered.
        assert_eq!(
            heartbeat_id(receiver.recv().await).as_deref(),
            Some("after")
        );
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
}
