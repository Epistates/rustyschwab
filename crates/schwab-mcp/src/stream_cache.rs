//! Service-aware snapshots with explicit freshness and a bounded activity log.
//!
//! Content is stored by field name (`bidPrice`, not `"1"`), decoded through
//! the service's field table, so snapshots read like REST quotes and REST
//! baselines merge in by name.
//!
//! A snapshot is `fresh` when the cache is authoritative for the key *and*
//! recent: the stream is ready on the generation that produced the entry,
//! the REST baseline has landed for a service that has one (level-one
//! equities and options), and the last delta both reached the cache within
//! [`FRESH_WINDOW_MS`] of the broker stamping it and arrived within
//! [`FRESH_WINDOW_MS`] of now.
//!
//! The generation is the feeder's: the cache moves to a new one only when
//! the feeder delivers the `State` that reports it, after every frame of
//! the old one. A reader that sees the live stream ahead of the feeder only
//! marks the cache not ready, so frames from before a gap are never filed
//! under the generation after it. Level-one deltas carry
//! only changed fields, so freshness is judged per key, never per field;
//! `fieldUpdatedAt` reports the per-field times for callers that care.
//! `staleReason` names the first failed condition so a consumer can tell
//! "reconnecting" from "quiet symbol".
use crate::redact;
use parking_lot::RwLock;
use schwab_rs::{
    MessageReceiver,
    types::streaming::{StreamMessage, StreamService, StreamStatus},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

/// Bound on both the delivery lag of the last delta (`received - broker`)
/// and its age (`now - received`) for a snapshot to be `fresh`.
pub const FRESH_WINDOW_MS: i64 = 2000;

#[derive(Default)]
struct CacheState {
    status: StreamStatus,
    snapshots: HashMap<(String, String), Entry>,
    events: VecDeque<Value>,
    events_evicted: u64,
    /// Account-activity messages already logged, by their content digest,
    /// so one Schwab resends after a reconnect is not logged twice.
    event_digests: HashMap<String, ()>,
    baseline_required: bool,
    baselines: HashMap<(String, String), Value>,
    /// Keys Schwab's REST quotes did not return in this generation (an
    /// unknown or expired symbol): no baseline will ever land for them.
    baseline_missing: std::collections::HashSet<(String, String)>,
    reconciled_generation: Option<u64>,
    /// The least `received - broker` seen this generation. Negative means
    /// this host's clock is behind Schwab's by at least that much, which
    /// would understate every lag; lags are judged with it added back.
    least_lag: Option<i64>,
}
impl CacheState {
    /// Delivery lag of `entry`'s last delta, corrected for this host's clock
    /// running behind Schwab's.
    fn lag(&self, entry: &Entry) -> i64 {
        let behind = self.least_lag.unwrap_or(0).min(0);
        entry
            .received
            .saturating_sub(entry.broker)
            .saturating_sub(behind)
    }
}
/// Services whose snapshots start from a REST quote baseline.
fn has_rest_baseline(service: &str) -> bool {
    matches!(service, "LEVELONE_EQUITIES" | "LEVELONE_OPTIONS")
}
struct Entry {
    value: Value,
    /// Receipt time of each field's current value, whether it came from a
    /// stream delta or the REST baseline.
    field_times: HashMap<String, i64>,
    received: i64,
    broker: i64,
    generation: u64,
}
impl CacheState {
    /// The first freshness condition an entry fails, or `None` when fresh.
    /// Ordered from "the stream is unusable" to "the key is merely quiet".
    fn stale_reason(
        &self,
        service: &str,
        key: &str,
        entry: &Entry,
        has_baseline: bool,
        now: i64,
    ) -> Option<&'static str> {
        if !self.status.ready {
            Some("stream not ready")
        } else if entry.generation != self.status.generation {
            Some("stream restarted since this update")
        } else if self.baseline_required && has_rest_baseline(service) && !has_baseline {
            if self
                .baseline_missing
                .contains(&(service.to_owned(), key.to_owned()))
            {
                Some("Schwab returned no REST quote for this key")
            } else {
                Some("REST baseline not applied yet")
            }
        } else if entry.broker <= 0 {
            Some("no broker timestamp")
        } else if self.lag(entry) > FRESH_WINDOW_MS {
            Some("delivered late relative to the broker timestamp")
        } else if now.saturating_sub(entry.received) > FRESH_WINDOW_MS {
            Some("no update within the freshness window")
        } else {
            None
        }
    }
}
/// A stable digest of an activity message's content.
fn schwab_mcp_digest(value: &Value) -> String {
    crate::policy::digest(
        serde_json::to_string(&schwab_rs::utils::canonical_json(value))
            .unwrap_or_default()
            .as_bytes(),
    )
}
/// Keys kept across all non-activity services before the oldest are evicted.
const MAX_SNAPSHOTS: usize = 10_000;

#[derive(Clone, Default)]
pub struct StreamCache {
    inner: Arc<RwLock<CacheState>>,
}
impl std::fmt::Debug for StreamCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cache = self.inner.read();
        f.debug_struct("StreamCache")
            .field("status", &cache.status)
            .field("snapshots", &cache.snapshots.len())
            .field("events", &cache.events.len())
            .finish_non_exhaustive()
    }
}
impl StreamCache {
    pub fn new() -> Self {
        Self::default()
    }
    /// Adopt the stream status the feeder just received: the only way the
    /// cache moves to a new generation.
    pub fn update_status(&self, status: StreamStatus) {
        let mut cache = self.inner.write();
        if !status.ready || status.generation != cache.status.generation {
            cache.snapshots.clear();
            cache.baselines.clear();
            cache.baseline_missing.clear();
            cache.reconciled_generation = None;
            cache.least_lag = None;
        }
        cache.status = status;
    }
    /// A reader saw the live stream: if it is not ready, or already past
    /// the feeder's generation, nothing cached is current. The cache stays
    /// on the feeder's generation; the feeder's next `State` moves it.
    pub fn observe_live(&self, live: &StreamStatus) {
        let mut cache = self.inner.write();
        if !live.ready || live.generation != cache.status.generation {
            cache.status.ready = false;
        }
    }
    fn put(&self, service: String, key: String, value: Value, broker: i64) {
        let mut cache = self.inner.write();
        let now = chrono::Utc::now().timestamp_millis();
        let generation = cache.status.generation;
        if service == "ACCT_ACTIVITY" {
            // Schwab may resend activity after a reconnect; the same
            // message is logged once.
            let digest = schwab_mcp_digest(&value);
            if cache.event_digests.insert(digest.clone(), ()).is_some() {
                return;
            }
            if cache.events.len() == 1024 {
                if let Some(evicted) = cache.events.pop_front()
                    && let Some(old) = evicted.get("digest").and_then(Value::as_str)
                {
                    cache.event_digests.remove(old);
                }
                cache.events_evicted += 1;
            }
            cache.events.push_back(json!({
                "digest": digest,
                "receivedAt": now,
                "brokerTimestamp": broker,
                "generation": generation,
                "content": value,
            }));
            return;
        }
        if cache.snapshots.len() >= MAX_SNAPSHOTS
            && !cache
                .snapshots
                .contains_key(&(service.clone(), key.clone()))
        {
            // Evict the least recently updated tenth in one pass, so a
            // churning key set costs one scan per thousand new keys rather
            // than one per key.
            let mut by_age: Vec<_> = cache
                .snapshots
                .iter()
                .map(|(k, e)| (e.received, k.clone()))
                .collect();
            let evict = MAX_SNAPSHOTS / 10;
            by_age.select_nth_unstable_by_key(evict - 1, |(received, _)| *received);
            for (_, stale) in by_age.into_iter().take(evict) {
                cache.snapshots.remove(&stale);
            }
        }
        if broker > 0 {
            let lag = now.saturating_sub(broker);
            cache.least_lag = Some(cache.least_lag.map_or(lag, |least| least.min(lag)));
        }
        let merge = service.starts_with("LEVELONE_");
        let entry = cache
            .snapshots
            .entry((service, key))
            .or_insert_with(|| Entry {
                value: json!({}),
                field_times: HashMap::new(),
                received: now,
                broker,
                generation,
            });
        if broker < entry.broker {
            return;
        }
        if merge {
            if let (Some(old), Some(new)) = (entry.value.as_object_mut(), value.as_object()) {
                for (k, v) in new {
                    old.insert(k.clone(), v.clone());
                    entry.field_times.insert(k.clone(), now);
                }
            }
        } else {
            entry.value = value;
            entry.field_times.clear();
        }
        entry.received = now;
        entry.broker = broker;
        entry.generation = generation;
    }
    pub fn snapshot(&self, service: &str, keys: Option<&[String]>) -> HashMap<String, Value> {
        let cache = self.inner.read();
        let now = chrono::Utc::now().timestamp_millis();
        cache
            .snapshots
            .iter()
            .filter(|((svc, k), _)| svc == service && keys.is_none_or(|ks| ks.contains(k)))
            .map(|((_, key), entry)| {
                let age = now.saturating_sub(entry.received).max(0);
                let baseline = cache.baselines.get(&(service.to_owned(), key.clone()));
                let stale_reason = cache.stale_reason(service, key, entry, baseline.is_some(), now);
                let snapshot = json!({
                    "content": entry.value,
                    "receivedAt": entry.received,
                    "brokerTimestamp": entry.broker,
                    "generation": entry.generation,
                    "ageMs": age,
                    "lagMs": cache.lag(entry),
                    "fresh": stale_reason.is_none(),
                    "staleReason": stale_reason,
                    "fieldUpdatedAt": entry.field_times,
                    "restBaseline": baseline,
                });
                (key.clone(), snapshot)
            })
            .collect()
    }
    /// The bounded account-activity log. `reconciliationRequired` is false
    /// only once REST reconciliation has vouched for the current stream
    /// generation; with account activity never subscribed nothing vouches
    /// for an empty log, so it stays true.
    pub fn events(&self) -> Value {
        let cache = self.inner.read();
        json!({
            "events": cache.events,
            "evicted": cache.events_evicted,
            "lostMessages": cache.status.lost_messages,
            "reconciliationRequired": cache.reconciled_generation != Some(cache.status.generation),
        })
    }
    pub fn require_baseline(&self) {
        self.inner.write().baseline_required = true;
    }
    /// Seed a level-one snapshot from a REST quote item. Every REST field
    /// whose name the service's stream table also uses is copied, so the
    /// baseline and later deltas describe the same fields.
    pub fn apply_baseline(&self, generation: u64, service: &str, key: &str, quote: Value) -> bool {
        let mut cache = self.inner.write();
        if !cache.status.ready || cache.status.generation != generation {
            return false;
        }
        let table = match StreamService::from_api_str(service) {
            Some(service @ (StreamService::LeveloneEquities | StreamService::LeveloneOptions)) => {
                service.fields()
            }
            _ => return false,
        };
        let Some(prices) = quote.get("quote").and_then(Value::as_object) else {
            return false;
        };
        if !["bidPrice", "askPrice"].iter().all(|field| {
            prices
                .get(*field)
                .and_then(Value::as_f64)
                .is_some_and(|value| value.is_finite() && value >= 0.0)
        }) {
            return false;
        }
        // An old quote time is a quiet key, not a bad baseline: the entry's
        // age and lag report it. A time ahead of this host is not trusted.
        let now = chrono::Utc::now().timestamp_millis();
        let broker = prices.get("quoteTime").and_then(Value::as_i64).unwrap_or(0);
        if broker <= 0 || broker > now + FRESH_WINDOW_MS {
            return false;
        }
        // `extended` is left out: it reuses `quoteTime`/`totalVolume` for the
        // extended session.
        let mut content = json!({ "key": key });
        for section in ["quote", "reference", "fundamental", "regular"] {
            let Some(section) = quote.get(section).and_then(Value::as_object) else {
                continue;
            };
            for (name, value) in section {
                if table.iter().any(|field| field.name == name) {
                    content[name] = value.clone();
                }
            }
        }
        let entry = cache
            .snapshots
            .entry((service.into(), key.into()))
            .or_insert_with(|| Entry {
                value: json!({}),
                field_times: HashMap::new(),
                received: now,
                broker,
                generation,
            });
        // REST has no stream sequence barrier. Preserve deltas received during
        // recovery; fill only missing fields and expose both source timestamps.
        for (field, value) in content.as_object().unwrap() {
            if entry.value.get(field).is_none() {
                entry.value[field] = value.clone();
                entry.field_times.insert(field.clone(), now);
            }
        }
        cache.baselines.insert((service.into(), key.into()), quote);
        true
    }
    /// Record that Schwab's REST quotes have no entry for `key`.
    pub fn baseline_unavailable(&self, generation: u64, service: &str, key: &str) {
        let mut cache = self.inner.write();
        if cache.status.generation == generation {
            cache
                .baseline_missing
                .insert((service.to_owned(), key.to_owned()));
        }
    }
    /// Whether `key` still needs a REST baseline in `generation`.
    fn needs_baseline(&self, generation: u64, service: &str, key: &str) -> bool {
        let cache = self.inner.read();
        let id = (service.to_owned(), key.to_owned());
        cache.status.generation == generation
            && !cache.baselines.contains_key(&id)
            && !cache.baseline_missing.contains(&id)
    }
    pub fn mark_accounts_reconciled(&self, generation: u64) {
        let mut cache = self.inner.write();
        if cache.status.ready && cache.status.generation == generation {
            cache.reconciled_generation = Some(generation);
        }
    }
    /// After `service`'s field list changes to `fields_csv`, drop the cached
    /// fields it no longer streams: level-one entries merge deltas, so their
    /// last values would ride along with fresh fields and read as current.
    /// Fields outside the service's numbered table (`key`, `delayed`, …)
    /// are kept.
    pub fn retain_fields(&self, service: &str, fields_csv: &str) {
        let Some(table) = StreamService::from_api_str(service).map(|s| s.fields()) else {
            return;
        };
        let numbers: std::collections::HashSet<u32> = fields_csv
            .split(',')
            .filter_map(|f| f.parse().ok())
            .collect();
        let dropped = |name: &str| {
            table
                .iter()
                .any(|f| f.name == name && !numbers.contains(&f.number))
        };
        let mut cache = self.inner.write();
        for ((_, _), entry) in cache
            .snapshots
            .iter_mut()
            .filter(|((svc, _), _)| svc == service)
        {
            if let Some(fields) = entry.value.as_object_mut() {
                fields.retain(|name, _| !dropped(name));
            }
            entry.field_times.retain(|name, _| !dropped(name));
        }
    }
    pub fn remove(&self, service: &str, keys: &[String]) {
        self.inner
            .write()
            .snapshots
            .retain(|(svc, key), _| svc != service || !keys.contains(key));
    }
    pub fn clear(&self) {
        let mut cache = self.inner.write();
        cache.snapshots.clear();
        cache.status.ready = false;
        cache.baselines.clear();
        cache.baseline_missing.clear();
        cache.reconciled_generation = None;
    }
}
/// Feeds deltas continuously while REST recovery runs independently. A new
/// generation cancels its predecessor; late baselines cannot mark new data ready.
///
/// Recovery seeds each level-one key's REST baseline once per stream
/// generation and, when account activity is subscribed, reconciles the
/// linked accounts so the activity log can vouch for the generation. It
/// retries every `interval` until both are done, each attempt bounded by
/// `deadline`. It never gates order submission: the risk check reads the
/// reconciled account baselines, which the recovery worker keeps, not the
/// stream, and a snapshot's `fresh` flag tells a reader whether its quote
/// can be relied on.
#[derive(Debug, Clone, Copy)]
pub struct RecoveryPolicy {
    pub deadline: std::time::Duration,
    pub interval: std::time::Duration,
}
impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            deadline: std::time::Duration::from_secs(30),
            interval: std::time::Duration::from_secs(10),
        }
    }
}
pub fn spawn_recovering_cache_feeder(
    rx: MessageReceiver,
    cache: StreamCache,
    client: Arc<schwab_rs::SchwabClient>,
    stream: schwab_rs::StreamClient,
) -> tokio::task::JoinHandle<()> {
    spawn_recovering_cache_feeder_with_policy(rx, cache, client, stream, RecoveryPolicy::default())
}
pub fn spawn_recovering_cache_feeder_with_policy(
    mut rx: MessageReceiver,
    cache: StreamCache,
    client: Arc<schwab_rs::SchwabClient>,
    stream: schwab_rs::StreamClient,
    policy: RecoveryPolicy,
) -> tokio::task::JoinHandle<()> {
    cache.require_baseline();
    tokio::spawn(async move {
        struct AbortOnDrop(Option<tokio::task::JoinHandle<()>>);
        impl Drop for AbortOnDrop {
            fn drop(&mut self) {
                if let Some(task) = self.0.take() {
                    task.abort();
                }
            }
        }
        let mut recovery = AbortOnDrop(None);
        while let Some(message) = rx.recv().await {
            match message {
                StreamMessage::State(status) => {
                    if let Some(task) = recovery.0.take() {
                        task.abort();
                    }
                    cache.update_status(status.clone());
                    if status.ready {
                        let cache = cache.clone();
                        let client = client.clone();
                        let stream = stream.clone();
                        recovery.0 = Some(tokio::spawn(async move {
                            loop {
                                let started = std::time::Instant::now();
                                let recovered = tokio::time::timeout(
                                    policy.deadline.max(std::time::Duration::from_millis(1)),
                                    recover(&cache, &client, &stream, status.generation),
                                )
                                .await
                                .unwrap_or(false);
                                metrics::histogram!(crate::metrics::STREAM_RECOVERY_SECONDS)
                                    .record(started.elapsed().as_secs_f64());
                                metrics::gauge!(crate::metrics::STREAM_RECOVERY_READY)
                                    .set(if recovered { 1.0 } else { 0.0 });
                                if recovered {
                                    break;
                                }
                                tokio::time::sleep(
                                    policy.interval.max(std::time::Duration::from_millis(10)),
                                )
                                .await;
                            }
                        }));
                    }
                }
                StreamMessage::Data(data) => {
                    // Activity means orders or balances moved: have the
                    // recovery worker, if any, re-read the account now
                    // rather than at its next interval.
                    if data.data.iter().any(|item| item.service == "ACCT_ACTIVITY") {
                        client.request_reconciliation();
                    }
                    for item in data.data {
                        // An unknown service keeps its numbered fields.
                        let table = item.service().map_or(&[][..], StreamService::fields);
                        for content in item.content {
                            let key = content.key.clone();
                            let mut value = content.into_named_value(table);
                            redact::apply_stream_content(&item.service, &mut value);
                            cache.put(item.service.clone(), key, value, item.timestamp);
                        }
                    }
                }
                _ => {}
            }
        }
        // The stream ended (its last `State` says why) or its client is
        // gone: nothing cached is current any more.
        cache.clear();
    })
}

/// One recovery attempt for `generation`: seed the level-one keys still
/// missing a REST baseline, and with account activity subscribed reconcile
/// every linked account. True once nothing is left to do.
async fn recover(
    cache: &StreamCache,
    client: &schwab_rs::SchwabClient,
    stream: &schwab_rs::StreamClient,
    generation: u64,
) -> bool {
    let mut recovered = true;
    let subscriptions = stream.confirmed_subscriptions();
    for (service, keys) in &subscriptions {
        if !has_rest_baseline(service) {
            continue;
        }
        let wanted: Vec<&str> = keys
            .iter()
            .filter(|key| cache.needs_baseline(generation, service, key))
            .map(String::as_str)
            .collect();
        for chunk in wanted.chunks(100) {
            let Ok(quotes) = client.get_quotes(chunk).await else {
                recovered = false;
                continue;
            };
            for key in chunk {
                let applied = quotes
                    .quotes
                    .get(*key)
                    .and_then(|quote| serde_json::to_value(quote).ok())
                    .is_some_and(|quote| cache.apply_baseline(generation, service, key, quote));
                // A key Schwab returns nothing usable for will not get a
                // baseline by asking again.
                if !applied {
                    cache.baseline_unavailable(generation, service, key);
                }
            }
        }
    }
    if subscriptions.contains_key("ACCT_ACTIVITY") {
        let reconciled = match client.get_account_numbers().await {
            Ok(accounts) => {
                let store = client.execution_store();
                let mut all = true;
                for account in accounts {
                    // The account's own durable generation: a stream gap
                    // changes nothing about which baseline is current, and
                    // reconciling under the stream's generation would
                    // invalidate the recovery worker's.
                    let durable = store
                        .account_state(&account.hash_value)
                        .await
                        .ok()
                        .flatten()
                        .map_or(0, |state| state.generation);
                    let current = match client.reconcile_account(&account.hash_value, durable).await
                    {
                        Ok(report) if report.superseded => store
                            .account_state(&account.hash_value)
                            .await
                            .ok()
                            .flatten()
                            .is_some_and(|state| state.reconciled),
                        Ok(report) => report.reconciled,
                        Err(_) => false,
                    };
                    all &= current;
                }
                all
            }
            Err(_) => false,
        };
        if reconciled {
            cache.mark_accounts_reconciled(generation);
        }
        recovered &= reconciled;
    }
    recovered
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_full_cache_evicts_a_tenth_and_keeps_the_newest_key() {
        let cache = StreamCache::new();
        cache.update_status(StreamStatus {
            ready: true,
            generation: 1,
            lost_messages: 0,
            ..Default::default()
        });
        let now = chrono::Utc::now().timestamp_millis();
        for i in 0..MAX_SNAPSHOTS {
            cache.put("LEVELONE_EQUITIES".into(), format!("K{i}"), json!({}), now);
        }
        assert_eq!(cache.inner.read().snapshots.len(), MAX_SNAPSHOTS);
        cache.put("LEVELONE_EQUITIES".into(), "NEW".into(), json!({}), now);
        let snapshots = &cache.inner.read().snapshots;
        assert_eq!(snapshots.len(), MAX_SNAPSHOTS - MAX_SNAPSHOTS / 10 + 1);
        assert!(snapshots.contains_key(&("LEVELONE_EQUITIES".into(), "NEW".into())));
    }

    /// A narrower field list drops the fields it no longer streams, so a
    /// merged entry never pairs a fresh last price with an old bid.
    #[test]
    fn a_new_field_list_drops_the_fields_it_stops_streaming() {
        let cache = StreamCache::new();
        cache.update_status(StreamStatus {
            ready: true,
            generation: 1,
            lost_messages: 0,
            ..Default::default()
        });
        let now = chrono::Utc::now().timestamp_millis();
        cache.put(
            "LEVELONE_EQUITIES".into(),
            "AAPL".into(),
            json!({"key":"AAPL","delayed":false,"bidPrice":10,"askPrice":11,"lastPrice":10.5}),
            now,
        );
        cache.retain_fields("LEVELONE_EQUITIES", "0,3");
        let snapshot = cache.snapshot("LEVELONE_EQUITIES", None);
        assert_eq!(
            snapshot["AAPL"]["content"],
            json!({"key":"AAPL","delayed":false,"lastPrice":10.5})
        );
        let mut timed: Vec<_> = snapshot["AAPL"]["fieldUpdatedAt"]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        timed.sort();
        assert_eq!(timed, ["delayed", "key", "lastPrice"]);
    }

    #[test]
    fn recent_delivery_does_not_make_old_broker_data_fresh() {
        let cache = StreamCache::new();
        cache.update_status(StreamStatus {
            ready: true,
            generation: 1,
            lost_messages: 0,
            ..Default::default()
        });
        let now = chrono::Utc::now().timestamp_millis();
        cache.put(
            "LEVELONE_EQUITIES".into(),
            "AAPL".into(),
            json!({"bidPrice":10,"askPrice":11}),
            now - 3000,
        );
        let snapshot = cache.snapshot("LEVELONE_EQUITIES", None);
        assert_eq!(snapshot["AAPL"]["fresh"], false);
        assert_eq!(
            snapshot["AAPL"]["staleReason"],
            "delivered late relative to the broker timestamp"
        );
        assert!(snapshot["AAPL"]["lagMs"].as_i64().unwrap() >= 3000);
        assert!(!cache.apply_baseline(
            1,
            "LEVELONE_EQUITIES",
            "AAPL",
            json!({"quote":{"bidPrice":null,"askPrice":11,"quoteTime":now}})
        ));
    }
    /// Level-one deltas only carry the fields that changed, so a field that
    /// has not ticked for a while must not make an actively updating key
    /// stale; only the entry's own age does.
    #[test]
    fn freshness_is_judged_per_key_not_per_field() {
        let cache = StreamCache::new();
        cache.update_status(StreamStatus {
            ready: true,
            generation: 1,
            lost_messages: 0,
            ..Default::default()
        });
        let now = chrono::Utc::now().timestamp_millis();
        cache.put(
            "LEVELONE_EQUITIES".into(),
            "AAPL".into(),
            json!({"bidPrice":10,"askPrice":11,"description":"Apple Inc"}),
            now,
        );
        {
            let mut state = cache.inner.write();
            let entry = state
                .snapshots
                .get_mut(&("LEVELONE_EQUITIES".into(), "AAPL".into()))
                .unwrap();
            entry.field_times.insert("description".into(), now - 60_000);
            entry.field_times.insert("askPrice".into(), now - 5_000);
        }
        let snapshot = cache.snapshot("LEVELONE_EQUITIES", None);
        assert_eq!(snapshot["AAPL"]["fresh"], true, "{snapshot:?}");
        assert_eq!(snapshot["AAPL"]["staleReason"], Value::Null);
        assert_eq!(
            snapshot["AAPL"]["fieldUpdatedAt"]["description"],
            now - 60_000
        );

        // A quiet key is stale for the recency reason, not a lag reason.
        cache
            .inner
            .write()
            .snapshots
            .get_mut(&("LEVELONE_EQUITIES".into(), "AAPL".into()))
            .unwrap()
            .received = now - 3_000;
        let snapshot = cache.snapshot("LEVELONE_EQUITIES", None);
        assert_eq!(snapshot["AAPL"]["fresh"], false);
        assert_eq!(
            snapshot["AAPL"]["staleReason"],
            "no update within the freshness window"
        );
        assert!(snapshot["AAPL"]["ageMs"].as_i64().unwrap() >= 3_000);
    }
    /// Reasons are reported from the most to the least severe condition.
    #[test]
    fn stale_reasons_name_the_first_failed_condition() {
        let cache = StreamCache::new();
        cache.require_baseline();
        cache.update_status(StreamStatus {
            ready: true,
            generation: 1,
            lost_messages: 0,
            ..Default::default()
        });
        let now = chrono::Utc::now().timestamp_millis();
        cache.put(
            "LEVELONE_EQUITIES".into(),
            "AAPL".into(),
            json!({"bidPrice":10}),
            now,
        );
        let reason = |cache: &StreamCache| {
            cache.snapshot("LEVELONE_EQUITIES", None)["AAPL"]["staleReason"].clone()
        };
        assert_eq!(reason(&cache), "REST baseline not applied yet");
        cache.inner.write().status.generation = 2;
        assert_eq!(reason(&cache), "stream restarted since this update");
        // `update_status` would drop the entries; poke the flag directly to
        // see the reason a not-ready stream reports.
        cache.inner.write().status.ready = false;
        assert_eq!(reason(&cache), "stream not ready");
    }
    #[test]
    fn recovery_rejects_stale_generations_and_future_broker_quotes() {
        let cache = StreamCache::new();
        cache.require_baseline();
        cache.update_status(StreamStatus {
            ready: true,
            generation: 2,
            lost_messages: 1,
            ..Default::default()
        });
        let now = chrono::Utc::now().timestamp_millis();
        cache.put(
            "LEVELONE_EQUITIES".into(),
            "AAPL".into(),
            json!({"bidPrice":12}),
            now,
        );
        assert_eq!(
            cache.snapshot("LEVELONE_EQUITIES", None)["AAPL"]["fresh"],
            false
        );
        let quote = json!({
            "quote": {"bidPrice":10,"askPrice":11,"quoteTime":now,"shortInterest":3},
            "reference": {"description":"Apple Inc","cusip":"037833100"},
            "fundamental": {"peRatio":30.5,"beta":1.2},
            "regular": {"regularMarketLastPrice":10.5},
            "extended": {"quoteTime":1}
        });
        assert!(!cache.apply_baseline(1, "LEVELONE_EQUITIES", "AAPL", quote.clone()));
        // A quote time ahead of this host is not trusted; an old one is a
        // quiet key and seeds the baseline (its age is reported).
        assert!(!cache.apply_baseline(
            2,
            "LEVELONE_EQUITIES",
            "AAPL",
            json!({"quote":{"bidPrice":10,"askPrice":11,"quoteTime":now+60_000}})
        ));
        assert!(cache.apply_baseline(2, "LEVELONE_EQUITIES", "AAPL", quote));
        let snapshot = cache.snapshot("LEVELONE_EQUITIES", None);
        let content = &snapshot["AAPL"]["content"];
        // The stream delta wins; REST fills the rest by table name only.
        assert_eq!(content["bidPrice"], 12);
        assert_eq!(content["askPrice"], 11);
        assert_eq!(content["quoteTime"], now);
        assert_eq!(content["description"], "Apple Inc");
        assert_eq!(content["peRatio"], 30.5);
        assert_eq!(content["regularMarketLastPrice"], 10.5);
        for absent in ["shortInterest", "cusip", "beta"] {
            assert!(
                content.get(absent).is_none(),
                "{absent} is not a stream field"
            );
        }
        assert_eq!(snapshot["AAPL"]["fresh"], true);
        cache.update_status(StreamStatus {
            ready: false,
            generation: 2,
            lost_messages: 2,
            ..Default::default()
        });
        assert!(cache.snapshot("LEVELONE_EQUITIES", None).is_empty());
    }
    #[test]
    fn deltas_merge_and_old_packets_do_not_overwrite_newer_quotes() {
        let cache = StreamCache::new();
        cache.update_status(StreamStatus {
            ready: true,
            generation: 1,
            lost_messages: 0,
            ..Default::default()
        });
        cache.put(
            "LEVELONE_EQUITIES".into(),
            "AAPL".into(),
            json!({"bidPrice":10,"askPrice":11}),
            100,
        );
        cache.put(
            "LEVELONE_EQUITIES".into(),
            "AAPL".into(),
            json!({"bidPrice":12}),
            101,
        );
        cache.put(
            "LEVELONE_EQUITIES".into(),
            "AAPL".into(),
            json!({"bidPrice":9}),
            99,
        );
        let snapshot = cache.snapshot("LEVELONE_EQUITIES", None);
        assert_eq!(
            snapshot["AAPL"]["content"],
            json!({"bidPrice":12,"askPrice":11})
        );
        cache.update_status(StreamStatus {
            ready: false,
            generation: 1,
            lost_messages: 1,
            ..Default::default()
        });
        assert!(cache.snapshot("LEVELONE_EQUITIES", None).is_empty());
    }
    /// A reader that sees the live stream past the feeder's generation only
    /// marks the cache not ready; frames the feeder still delivers from
    /// before the gap are never filed as the new generation's.
    #[test]
    fn a_reader_ahead_of_the_feeder_cannot_make_pre_gap_frames_fresh() {
        let cache = StreamCache::new();
        let ready = |generation| StreamStatus {
            ready: true,
            generation,
            ..Default::default()
        };
        cache.update_status(ready(1));
        cache.observe_live(&ready(2));
        let now = chrono::Utc::now().timestamp_millis();
        cache.put(
            "LEVELONE_EQUITIES".into(),
            "AAPL".into(),
            json!({"bidPrice":10}),
            now,
        );
        let snapshot = cache.snapshot("LEVELONE_EQUITIES", None);
        assert_eq!(snapshot["AAPL"]["generation"], 1);
        assert_eq!(snapshot["AAPL"]["staleReason"], "stream not ready");
        // The feeder's own State moves the cache on and drops the old entry.
        cache.update_status(ready(2));
        assert!(cache.snapshot("LEVELONE_EQUITIES", None).is_empty());
    }

    /// Only services that have a REST baseline wait for one; a key Schwab
    /// has no quote for says so instead of waiting forever.
    #[test]
    fn baselines_are_required_per_service() {
        let cache = StreamCache::new();
        cache.require_baseline();
        cache.update_status(StreamStatus {
            ready: true,
            generation: 1,
            ..Default::default()
        });
        let now = chrono::Utc::now().timestamp_millis();
        cache.put(
            "LEVELONE_FUTURES".into(),
            "/ES".into(),
            json!({"bidPrice":1}),
            now,
        );
        assert_eq!(
            cache.snapshot("LEVELONE_FUTURES", None)["/ES"]["fresh"],
            true
        );
        cache.put(
            "LEVELONE_EQUITIES".into(),
            "ZZZZ".into(),
            json!({"bidPrice":1}),
            now,
        );
        cache.baseline_unavailable(1, "LEVELONE_EQUITIES", "ZZZZ");
        assert_eq!(
            cache.snapshot("LEVELONE_EQUITIES", None)["ZZZZ"]["staleReason"],
            "Schwab returned no REST quote for this key"
        );
        assert!(!cache.needs_baseline(1, "LEVELONE_EQUITIES", "ZZZZ"));
    }

    /// Activity Schwab resends after a reconnect is logged once.
    #[test]
    fn resent_activity_is_logged_once() {
        let cache = StreamCache::new();
        for _ in 0..2 {
            cache.put(
                "ACCT_ACTIVITY".into(),
                "Account Activity".into(),
                json!({"seq":7,"messageType":"OrderFill"}),
                1,
            );
        }
        cache.put(
            "ACCT_ACTIVITY".into(),
            "Account Activity".into(),
            json!({"seq":8,"messageType":"OrderFill"}),
            2,
        );
        assert_eq!(cache.events()["events"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn activity_is_a_bounded_event_history_with_explicit_eviction() {
        let cache = StreamCache::new();
        for i in 0..1025 {
            cache.put(
                "ACCT_ACTIVITY".into(),
                "Account Activity".into(),
                json!({"id":i}),
                i,
            );
        }
        let events = cache.events();
        assert_eq!(events["events"].as_array().unwrap().len(), 1024);
        assert_eq!(events["evicted"], 1);
        assert_eq!(events["reconciliationRequired"], true);
    }
}
