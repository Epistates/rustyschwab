//! A principal's dead-man switch: when it has made no tool call for its
//! window, the server cancels the working orders it placed.
//!
//! An agent that crashes, hangs or loses its connection leaves its orders
//! working with nobody watching them. `dead_man_secs` is the operator's own
//! safety control for that case, so it is not one of the principal's tools
//! and does not wait on `SCHWAB_MCP_ALLOW_CANCEL`: the cancels go straight
//! to the backend, which the SDK exempts from the circuit breaker.
//!
//! - **Liveness.** Every tool call by the principal, of any kind, resets
//!   its timer. While a call is in flight the principal is not silent, so a
//!   submission held for an operator's approval cannot expire it.
//! - **One watcher per principal.** A [`Beacon`] is shared by every clone
//!   of the principal's [`AuthorizedServer`](crate::policy::AuthorizedServer),
//!   whatever the sessions or transport, and one task watches it. The task
//!   ends when the last clone is dropped. A [`Liveness`] registry, shared
//!   by the servers a policy reload rebuilds, retires the previous
//!   generation's beacon as it arms the next, which inherits the timer, so a
//!   reload neither doubles the watcher nor grants a fresh window.
//! - **Across processes.** Calls also touch the principal's heartbeat file
//!   ([`heartbeat_path`]), and a watcher whose window ran out waits out any
//!   call another process heard since. A stdio server whose client has gone
//!   stays for the switch ([`Beacon::outlive_client`]).
//! - **Once per silence.** After it fires the switch stays quiet until the
//!   principal's next call. A pass that fails (the venue could not be read,
//!   a cancel was refused) is retried a few times within the same silence.
//! - **Attribution.** Only orders this principal placed are cancelled,
//!   found from its audit records over [`ORDER_LOOKBACK_DAYS`]: the venue
//!   order id a completed dispatch recorded, or, for a dispatch that
//!   recorded none (a lost acknowledgement, a process that died
//!   mid-dispatch), the order tag in the arguments it dispatched. Those are
//!   matched against the working orders the venue lists on the accounts the
//!   principal traded. Another principal's order on the same account is
//!   never touched.
//!
//! Each firing that found something working is recorded in the principal's
//! audit journal as a `dead_man_cancel` event with every order id and
//! outcome, logged at warn, and counted in
//! [`DEAD_MAN_FIRINGS_TOTAL`](crate::metrics::DEAD_MAN_FIRINGS_TOTAL).

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::Duration;

use chrono::Utc;
use parking_lot::Mutex;
use rustyschwab::execution::{OrderJournal, SubmissionOutcome, SubmissionRecord};
use rustyschwab_types::trading::{Order, OrderStatus, OrdersParams};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::backend::Backend;
use crate::contract::ToolFailure;
use crate::journal::Dispatch;
use crate::policy::PrincipalPolicy;
use crate::portfolio::ORDER_LOOKBACK_DAYS;

/// The shortest window `dead_man_secs` may set. Shorter than an agent takes
/// to think between calls, the switch would cancel orders of a principal
/// that is working normally.
pub const MIN_WINDOW_SECS: u32 = 30;

/// The longest window `dead_man_secs` may set: a day, the life of a DAY
/// order. A longer silence is not one the switch is for.
pub const MAX_WINDOW_SECS: u32 = 86_400;

/// How many passes a firing makes at the same silence before giving up.
const MAX_ATTEMPTS: u32 = 4;

/// How long a failed pass waits before the next.
const RETRY_AFTER: Duration = Duration::from_secs(10);

/// The kind of audit event a firing records.
pub const EVENT_KIND: &str = "dead_man_cancel";

/// What a principal's `get_server_status` says of its switch, so an agent
/// knows to keep calling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeadManStatus {
    /// Seconds of silence after which the principal's working orders are
    /// cancelled.
    pub window_secs: u64,
    /// Seconds until then if the principal makes no further call. The status
    /// call is itself a call and resets the timer, so this reads the whole
    /// window; it falls only between calls.
    pub seconds_left: u64,
}

/// What the watcher observes: when the principal last acted, how many of
/// its calls are running, and whether this beacon has been superseded.
#[derive(Debug, Clone, Copy)]
struct Pulse {
    at: Instant,
    in_flight: u32,
    retired: bool,
}

/// A principal's liveness, shared by every clone of its server.
#[derive(Debug)]
pub struct Beacon {
    window: Duration,
    pulse: watch::Sender<Pulse>,
    /// The principal's heartbeat file, shared with the other processes that
    /// serve it; see [`heartbeat_path`].
    heartbeat: std::path::PathBuf,
    /// When this process last touched `heartbeat`.
    touched: Mutex<Option<std::time::Instant>>,
    /// Counts the watcher's firings, so a server whose client has gone can
    /// wait for the next one ([`Beacon::outlive_client`]).
    firings: watch::Receiver<u64>,
}

impl Beacon {
    /// Note that the principal is calling a tool, until the guard drops;
    /// the principal's clock restarts then.
    pub fn call(self: &Arc<Self>) -> Call {
        self.pulse.send_modify(|pulse| {
            pulse.at = Instant::now();
            pulse.in_flight += 1;
        });
        self.touch();
        Call(self.clone())
    }

    /// Tell the other processes serving the principal that it is alive, at
    /// most once a second (or a tenth of the window, if less).
    fn touch_every(&self) -> Duration {
        HEARTBEAT_EVERY.min(self.window / 10)
    }

    fn touch(&self) {
        let mut touched = self.touched.lock();
        if touched.is_some_and(|at| at.elapsed() < self.touch_every()) {
            return;
        }
        *touched = Some(std::time::Instant::now());
        if let Err(e) = touch(&self.heartbeat) {
            tracing::debug!(error = %e, "the dead-man heartbeat was not written");
        }
    }

    /// Keep the switch after the client has gone (a stdio client that
    /// disconnected, or crashed), until it fires, or until another process
    /// shows the principal is back and takes over its watch. The silence
    /// that began with the principal's last call runs out as it would have
    /// with the client connected.
    pub async fn outlive_client(&self) {
        let left = self.status().seconds_left;
        let gone = std::time::SystemTime::now();
        let mut firings = self.firings.clone();
        firings.borrow_and_update();
        tracing::info!(
            seconds_left = left,
            "the client disconnected; the dead-man switch keeps the server until it fires"
        );
        // Later than this process's own last touch could be.
        let margin = self.touch_every() * 2;
        loop {
            tokio::select! {
                _ = firings.changed() => return,
                () = tokio::time::sleep(self.touch_every() * 5) => {
                    if heard_at(&self.heartbeat).is_some_and(|at| at > gone + margin) {
                        tracing::info!(
                            "the principal is back through another server; its switch is that \
                             server's now"
                        );
                        return;
                    }
                }
            }
        }
    }

    /// The window and what is left of it.
    pub fn status(&self) -> DeadManStatus {
        let pulse = *self.pulse.borrow();
        DeadManStatus {
            window_secs: self.window.as_secs(),
            seconds_left: self
                .window
                .saturating_sub(pulse.at.elapsed())
                .as_secs_f64()
                .ceil() as u64,
        }
    }

    /// Stop this beacon's watcher: a newer one has taken over.
    pub(crate) fn retire(&self) {
        self.pulse.send_modify(|pulse| pulse.retired = true);
    }
}

/// A tool call in flight; see [`Beacon::call`].
#[derive(Debug)]
pub struct Call(Arc<Beacon>);

impl Drop for Call {
    fn drop(&mut self) {
        self.0.pulse.send_modify(|pulse| {
            pulse.at = Instant::now();
            pulse.in_flight = pulse.in_flight.saturating_sub(1);
        });
        self.0.touch();
    }
}

/// How often a busy principal's heartbeat file is touched.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(1);

/// The principal's heartbeat: a file beside its audit journal (which is the
/// principal's own) whose modification time is its last call through any
/// process sharing that state. A watcher whose window has run out checks
/// it, so one principal served by two processes (or a client that came back
/// through a new one) is not cancelled by the process it stopped using.
pub fn heartbeat_path(audit: &OrderJournal) -> std::path::PathBuf {
    let mut path = audit.path().as_os_str().to_owned();
    path.push(".alive");
    path.into()
}

fn touch(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .set_modified(std::time::SystemTime::now())
}

fn heard_at(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// What remains of the window since the principal was last heard from by
/// any process, if anything does.
fn heard_within(path: &std::path::Path, window: Duration) -> Option<Duration> {
    let since = heard_at(path)?.elapsed().unwrap_or_default();
    window.checked_sub(since).filter(|left| !left.is_zero())
}

/// The beacons of one process's principals, by name. Servers built over the
/// same backend share one, so that a policy reload finds the generation it
/// replaces.
#[derive(Debug, Clone, Default)]
pub struct Liveness {
    beacons: Arc<Mutex<BTreeMap<String, Weak<Beacon>>>>,
}

impl Liveness {
    /// Arm the principal's switch for `window`, or disarm it when there is
    /// none. The generation already armed under the name is retired, and its
    /// timer carried over.
    pub(crate) fn arm(&self, reaper: Reaper, window: Option<Duration>) -> Option<Arc<Beacon>> {
        let mut beacons = self.beacons.lock();
        beacons.retain(|_, beacon| beacon.strong_count() > 0);
        let previous = beacons
            .remove(&reaper.principal.name)
            .and_then(|beacon| beacon.upgrade());
        if let Some(previous) = &previous {
            previous.retire();
        }
        let window = window?;
        let at = previous.map_or_else(Instant::now, |previous| previous.pulse.borrow().at);
        let (pulse, watching) = watch::channel(Pulse {
            at,
            in_flight: 0,
            retired: false,
        });
        let (fired, firings) = watch::channel(0);
        let heartbeat = heartbeat_path(&reaper.audit);
        let beacon = Arc::new(Beacon {
            window,
            pulse,
            heartbeat: heartbeat.clone(),
            touched: Mutex::new(None),
            firings,
        });
        beacon.touch();
        beacons.insert(reaper.principal.name.clone(), Arc::downgrade(&beacon));
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(watch_silence(watching, window, reaper, heartbeat, fired));
            }
            Err(_) => tracing::error!(
                principal = %reaper.principal.name,
                "the dead-man switch needs a Tokio runtime and is not armed"
            ),
        }
        Some(beacon)
    }
}

/// Wait out the principal's silence and cancel its orders, until its beacon
/// is retired or dropped. Each firing is counted in `fired`.
async fn watch_silence(
    mut pulse: watch::Receiver<Pulse>,
    window: Duration,
    reaper: Reaper,
    heartbeat: std::path::PathBuf,
    fired: watch::Sender<u64>,
) {
    loop {
        let seen = *pulse.borrow_and_update();
        if seen.retired {
            return;
        }
        // With calls in flight the principal is not silent: only the end of
        // one can move the deadline.
        let expired = if seen.in_flight == 0 {
            tokio::select! {
                changed = pulse.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    false
                }
                () = tokio::time::sleep_until(seen.at + window) => true,
            }
        } else {
            if pulse.changed().await.is_err() {
                return;
            }
            false
        };
        // A call that landed as the window ran out is not silence.
        if !expired || pulse.has_changed().unwrap_or(true) {
            continue;
        }
        // Nor is a call through another process serving the principal:
        // wait out the window from that one instead.
        if let Some(left) = heard_within(&heartbeat, window) {
            tokio::select! {
                changed = pulse.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                () = tokio::time::sleep(left) => {}
            }
            continue;
        }
        let acted = cancel_in_silence(&mut pulse, &reaper, window).await;
        fired.send_modify(|count| *count += 1);
        if acted {
            continue;
        }
        // Fired: quiet until the principal next acts.
        if pulse.changed().await.is_err() {
            return;
        }
    }
}

/// Cancel the principal's working orders, retrying a failed pass; returns
/// whether the principal acted meanwhile, ending the silence.
async fn cancel_in_silence(
    pulse: &mut watch::Receiver<Pulse>,
    reaper: &Reaper,
    window: Duration,
) -> bool {
    for attempt in 1..=MAX_ATTEMPTS {
        if !reaper.fire(window).await || attempt == MAX_ATTEMPTS {
            return false;
        }
        tokio::select! {
            _ = pulse.changed() => return true,
            () = tokio::time::sleep(RETRY_AFTER) => {}
        }
    }
    false
}

/// What the switch needs to cancel a principal's orders, apart from the
/// server: holding the server itself would keep the switch alive forever.
#[derive(Debug, Clone)]
pub(crate) struct Reaper {
    pub(crate) principal: Arc<PrincipalPolicy>,
    pub(crate) audit: OrderJournal,
    pub(crate) backend: Backend,
}

/// What a principal's audit records say it placed on one account.
#[derive(Debug, Default, PartialEq, Eq)]
struct Placed {
    /// Venue order ids that completed dispatches recorded.
    ids: HashSet<String>,
    /// Order tags of dispatches that recorded no id.
    tags: HashSet<String>,
}

/// The orders `records` show the principal placing, by account it may act
/// on. A submission that provably produced no order is left out.
fn placed(records: &[SubmissionRecord], policy: &PrincipalPolicy) -> BTreeMap<String, Placed> {
    let mut accounts = BTreeMap::<String, Placed>::new();
    for record in records
        .iter()
        .filter(|record| crate::guardrails::counts(record))
    {
        let arguments = &record.request["arguments"];
        let Some(account) = arguments["account_hash"]
            .as_str()
            .filter(|account| policy.grants_account(account))
        else {
            continue;
        };
        let id = match &record.outcome {
            Some(SubmissionOutcome::Accepted { order_id }) => order_id.clone(),
            Some(SubmissionOutcome::Reconciled { evidence }) => match Dispatch::parse(evidence) {
                Some(Dispatch::Completed { order_id }) => order_id,
                _ => None,
            },
            _ => None,
        };
        let placed = accounts.entry(account.to_owned()).or_default();
        match (id, arguments["order"]["tag"].as_str()) {
            (Some(id), _) => {
                placed.ids.insert(id);
            }
            (None, Some(tag)) => {
                placed.tags.insert(tag.to_owned());
            }
            (None, None) => {}
        }
    }
    accounts
}

/// The ids to cancel among `orders` for what the principal `placed`: an
/// order of it that is still working, or, under one of its orders that is
/// not (a filled entry), the working orders beneath. Cancelling an order
/// takes its children with it, and one already being cancelled is left.
fn working(orders: Vec<Order>, placed: &Placed) -> Vec<String> {
    fn walk(order: Order, owned: bool, placed: &Placed, found: &mut Vec<String>) {
        let owned = owned
            || order
                .order_id
                .as_ref()
                .is_some_and(|id| placed.ids.contains(id))
            || order
                .tag
                .as_ref()
                .is_some_and(|tag| placed.tags.contains(tag));
        if owned && order.status.is_none_or(|status| !status.is_terminal()) {
            if order.status != Some(OrderStatus::PendingCancel)
                && let Some(id) = order.order_id
                && !found.contains(&id)
            {
                found.push(id);
            }
            return;
        }
        for child in order.child_order_strategies {
            walk(child, owned, placed, found);
        }
    }
    let mut found = Vec::new();
    for order in orders {
        walk(order, false, placed, &mut found);
    }
    found
}

impl Reaper {
    /// Cancel every working order the principal placed and record it.
    /// Returns whether another pass is worth making: something could not be
    /// read or cancelled.
    async fn fire(&self, window: Duration) -> bool {
        let name = &self.principal.name;
        let mut unread = Vec::new();
        let mut cancels = Vec::new();
        match self.working_orders(&mut unread).await {
            Ok(orders) => {
                for (account, order_id) in orders {
                    let outcome = self.backend.cancel_order(&account, &order_id).await;
                    cancels.push((account, order_id, outcome));
                }
            }
            Err(e) => unread.push(e),
        }
        if cancels.is_empty() && unread.is_empty() {
            tracing::debug!(principal = %name, "the dead-man switch found no working order");
            return false;
        }
        let failed = !unread.is_empty() || cancels.iter().any(|(_, _, outcome)| outcome.is_err());
        let orders: Vec<Value> = cancels
            .iter()
            .map(|(account, order_id, outcome)| match outcome {
                Ok(()) => json!({"accountHash": account, "orderId": order_id,
                    "outcome": "canceled"}),
                Err(e) => json!({"accountHash": account, "orderId": order_id,
                    "outcome": "failed", "code": e.code, "message": e.message}),
            })
            .collect();
        let unread: Vec<String> = unread.iter().map(|e| e.message.clone()).collect();
        tracing::warn!(
            principal = %name,
            window_secs = window.as_secs(),
            canceled = cancels.iter().filter(|(_, _, o)| o.is_ok()).count(),
            failed = cancels.iter().filter(|(_, _, o)| o.is_err()).count(),
            unread = unread.len(),
            "the dead-man switch fired: the principal went silent"
        );
        metrics::counter!(
            crate::metrics::DEAD_MAN_FIRINGS_TOTAL,
            "outcome" => if failed { "failed" } else { "canceled" },
        )
        .increment(1);
        let payload = json!({"windowSecs": window.as_secs(), "orders": orders, "unread": unread});
        if let Err(e) = self.audit.record_event(EVENT_KIND, name, payload).await {
            tracing::error!(principal = %name, error = %e, "the dead-man firing was not audited");
        }
        failed
    }

    /// The `(account, order id)` pairs to cancel. A venue read that fails is
    /// added to `unread` and the other accounts are still searched.
    async fn working_orders(
        &self,
        unread: &mut Vec<ToolFailure>,
    ) -> Result<Vec<(String, String)>, ToolFailure> {
        let lookback = chrono::Duration::days(ORDER_LOOKBACK_DAYS);
        let records = self
            .audit
            .records_since(
                &self.principal.name,
                (Utc::now() - lookback).timestamp_millis(),
            )
            .await?;
        let accounts = placed(&records, &self.principal);
        if accounts.is_empty() {
            return Ok(Vec::new());
        }
        let now = self.backend.now().await?;
        let params = OrdersParams::new(now - lookback, now);
        let mut found = Vec::new();
        for (account, placed) in accounts {
            match self.backend.account_orders(&account, &params).await {
                Ok(orders) => found.extend(
                    working(orders, &placed)
                        .into_iter()
                        .map(|id| (account.clone(), id)),
                ),
                Err(e) => unread.push(e),
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::FailureCode;

    fn policy(accounts: &[&str]) -> PrincipalPolicy {
        serde_json::from_value(json!({"name": "desk", "token_sha256": "a".repeat(64),
            "tools": ["place_order"], "accounts": accounts, "max_quantity": "10",
            "max_notional": "1000", "dead_man_secs": 60}))
        .unwrap()
    }

    fn record(
        operation: &str,
        account: &str,
        tag: Option<&str>,
        outcome: Option<SubmissionOutcome>,
    ) -> SubmissionRecord {
        SubmissionRecord {
            attempt_id: uuid::Uuid::new_v4().to_string(),
            account: "desk".into(),
            operation: operation.into(),
            request: json!({"arguments": {"account_hash": account, "order": {"tag": tag}}}),
            created_at: 0,
            outcome,
            dedup_key: None,
            armed: true,
        }
    }

    fn completed(order_id: Option<&str>) -> Option<SubmissionOutcome> {
        Some(SubmissionOutcome::Reconciled {
            evidence: Dispatch::Completed {
                order_id: order_id.map(str::to_owned),
            }
            .evidence(),
        })
    }

    fn order(value: Value) -> Order {
        serde_json::from_value(value).unwrap()
    }

    /// An order is the principal's by the id its dispatch recorded, or, when
    /// none was recorded, by its tag; a submission that produced nothing,
    /// another operation and an account the principal lacks are not.
    #[test]
    fn orders_are_attributed_by_recorded_id_or_by_tag() {
        let failed = Some(SubmissionOutcome::Reconciled {
            evidence: Dispatch::Failed {
                code: Some(FailureCode::RiskDenied),
            }
            .evidence(),
        });
        let lost = Some(SubmissionOutcome::Reconciled {
            evidence: Dispatch::Failed {
                code: Some(FailureCode::ReconciliationRequired),
            }
            .evidence(),
        });
        let records = [
            record("place_order", "A", Some("t1"), completed(Some("101"))),
            record("replace_order", "A", Some("t2"), completed(Some("102"))),
            record("place_order", "A", Some("t3"), completed(None)),
            record("place_order", "A", Some("t4"), None),
            record("place_order", "A", Some("t5"), lost),
            record("place_order", "A", Some("t6"), failed),
            record(
                "place_order",
                "A",
                Some("t7"),
                Some(SubmissionOutcome::Rejected),
            ),
            record("cancel_order", "A", None, completed(Some("103"))),
            record("place_order", "B", Some("t8"), completed(Some("104"))),
            record("place_order", "C", None, None),
        ];
        let accounts = placed(&records, &policy(&["A", "C"]));
        let strings = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(
            accounts["A"],
            Placed {
                ids: strings(&["101", "102"]),
                tags: strings(&["t3", "t4", "t5"]),
            }
        );
        assert_eq!(accounts["C"], Placed::default());
        assert!(!accounts.contains_key("B"), "an account it is not granted");
    }

    /// Only working orders of the principal are chosen: others', finished
    /// ones and ones already being cancelled are not, and a working child
    /// under a finished entry of the principal is.
    #[test]
    fn only_the_principals_working_orders_are_chosen() {
        let placed = Placed {
            ids: [
                "1".to_owned(),
                "2".to_owned(),
                "3".to_owned(),
                "5".to_owned(),
            ]
            .into(),
            tags: ["mine".to_owned()].into(),
        };
        let orders = vec![
            order(json!({"orderId": "1", "status": "WORKING"})),
            order(json!({"orderId": "2", "status": "FILLED"})),
            order(json!({"orderId": "3", "status": "PENDING_CANCEL"})),
            order(json!({"orderId": "4", "status": "WORKING"})),
            order(json!({"orderId": "6", "status": "QUEUED", "tag": "mine"})),
            order(json!({"orderId": "7", "status": "WORKING", "tag": "theirs"})),
            order(
                json!({"orderId": "5", "status": "FILLED", "childOrderStrategies": [
                {"orderId": "8", "status": "WORKING"},
                {"orderId": "9", "status": "CANCELED"}]}),
            ),
            order(
                json!({"orderId": "10", "status": "WORKING", "childOrderStrategies": [
                {"orderId": "11", "status": "WORKING"}]}),
            ),
        ];
        assert_eq!(working(orders, &placed), ["1", "6", "8"]);
    }

    /// The status reports the window and what is left of it.
    #[test]
    fn the_beacon_counts_down_and_resets_on_a_call() {
        let dir = tempfile::tempdir().unwrap();
        let heartbeat = dir.path().join("audit.alive");
        let beacon = |silent: u64| {
            let (pulse, _) = watch::channel(Pulse {
                at: Instant::now() - Duration::from_secs(silent),
                in_flight: 0,
                retired: false,
            });
            Arc::new(Beacon {
                window: Duration::from_secs(60),
                pulse,
                heartbeat: heartbeat.clone(),
                touched: Mutex::new(None),
                firings: watch::channel(0).1,
            })
        };
        assert_eq!(
            beacon(0).status(),
            DeadManStatus {
                window_secs: 60,
                seconds_left: 60
            }
        );
        let silent = beacon(25);
        assert_eq!(silent.status().seconds_left, 35);
        drop(silent.call());
        assert_eq!(silent.status().seconds_left, 60);
        assert_eq!(beacon(90).status().seconds_left, 0);
        // The call was heard by every process sharing the heartbeat.
        assert!(heard_within(&heartbeat, Duration::from_secs(60)).is_some());
        assert!(heard_within(&heartbeat, Duration::ZERO).is_none());
    }
}
