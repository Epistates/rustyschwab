//! Service-aware snapshots with explicit freshness and a bounded activity log.
//!
//! Content is stored by field name (`bidPrice`, not `"1"`), decoded through
//! the service's field table, so snapshots read like REST quotes and REST
//! baselines merge in by name.
//!
//! A snapshot is `fresh` when the cache is authoritative for the key *and*
//! recent: the stream is ready on the generation that produced the entry,
//! the REST baseline has landed when one is required, and the last delta
//! both reached the cache within [`FRESH_WINDOW_MS`] of the broker stamping
//! it and arrived within [`FRESH_WINDOW_MS`] of now. Level-one deltas carry
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
    baseline_required: bool,
    baselines: HashMap<(String, String), Value>,
    reconciled_generation: Option<u64>,
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
    fn stale_reason(&self, entry: &Entry, has_baseline: bool, now: i64) -> Option<&'static str> {
        if !self.status.ready {
            Some("stream not ready")
        } else if entry.generation != self.status.generation {
            Some("stream restarted since this update")
        } else if self.baseline_required && !has_baseline {
            Some("REST baseline not applied yet")
        } else if entry.broker <= 0 {
            Some("no broker timestamp")
        } else if entry.received.saturating_sub(entry.broker) > FRESH_WINDOW_MS {
            Some("delivered late relative to the broker timestamp")
        } else if now.saturating_sub(entry.received) > FRESH_WINDOW_MS {
            Some("no update within the freshness window")
        } else {
            None
        }
    }
}
#[derive(Clone, Default)]
pub struct StreamCache {
    inner: Arc<RwLock<CacheState>>,
}
impl StreamCache {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn update_status(&self, status: StreamStatus) {
        let mut cache = self.inner.write();
        if !status.ready || status.generation != cache.status.generation {
            cache.snapshots.clear();
            cache.baselines.clear();
            cache.reconciled_generation = None;
        }
        cache.status = status;
    }
    fn put(&self, service: String, key: String, value: Value, broker: i64) {
        let mut cache = self.inner.write();
        let now = chrono::Utc::now().timestamp_millis();
        let generation = cache.status.generation;
        if service == "ACCT_ACTIVITY" {
            if cache.events.len() == 1024 {
                cache.events.pop_front();
                cache.events_evicted += 1;
            }
            cache.events.push_back(json!({
                "receivedAt": now,
                "brokerTimestamp": broker,
                "generation": generation,
                "content": value,
            }));
            return;
        }
        if cache.snapshots.len() >= 10000
            && !cache
                .snapshots
                .contains_key(&(service.clone(), key.clone()))
            && let Some(oldest) = cache
                .snapshots
                .iter()
                .min_by_key(|(_, e)| e.received)
                .map(|(k, _)| k.clone())
        {
            cache.snapshots.remove(&oldest);
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
                let stale_reason = cache.stale_reason(entry, baseline.is_some(), now);
                let snapshot = json!({
                    "content": entry.value,
                    "receivedAt": entry.received,
                    "brokerTimestamp": entry.broker,
                    "generation": entry.generation,
                    "ageMs": age,
                    "lagMs": entry.received.saturating_sub(entry.broker),
                    "fresh": stale_reason.is_none(),
                    "staleReason": stale_reason,
                    "fieldUpdatedAt": entry.field_times,
                    "restBaseline": baseline,
                });
                (key.clone(), snapshot)
            })
            .collect()
    }
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
        let now = chrono::Utc::now().timestamp_millis();
        let broker = prices.get("quoteTime").and_then(Value::as_i64).unwrap_or(0);
        if broker <= 0 || now < broker || now - broker > FRESH_WINDOW_MS {
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
    pub fn mark_accounts_reconciled(&self, generation: u64) {
        let mut cache = self.inner.write();
        if cache.status.ready && cache.status.generation == generation {
            cache.reconciled_generation = Some(generation);
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
        cache.reconciled_generation = None;
    }
}
/// Feeds deltas continuously while REST recovery runs independently. A new
/// generation cancels its predecessor; late baselines cannot mark new data ready.
#[derive(Clone, Copy)]
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
                    client.suspend_submissions("Required stream is recovering");
                    cache.update_status(status.clone());
                    if status.ready {
                        let cache = cache.clone();
                        let client = client.clone();
                        let stream = stream.clone();
                        recovery.0 = Some(tokio::spawn(async move {
                            loop {
                                let started = std::time::Instant::now();
                                let recover = async {
                                    let mut recovered = true;
                                    let subscriptions = stream.confirmed_subscriptions();
                                    for (service, keys) in &subscriptions {
                                        if !matches!(
                                            service.as_str(),
                                            "LEVELONE_EQUITIES" | "LEVELONE_OPTIONS"
                                        ) {
                                            continue;
                                        }
                                        for chunk in keys.chunks(100) {
                                            let refs: Vec<_> =
                                                chunk.iter().map(String::as_str).collect();
                                            if let Ok(quotes) = client.get_quotes(&refs).await {
                                                if quotes.quotes.len() != chunk.len() {
                                                    recovered = false;
                                                }
                                                for (key, quote) in quotes.quotes {
                                                    if let Ok(value) = serde_json::to_value(quote) {
                                                        recovered &= cache.apply_baseline(
                                                            status.generation,
                                                            service,
                                                            &key,
                                                            value,
                                                        );
                                                    } else {
                                                        recovered = false;
                                                    }
                                                }
                                            } else {
                                                recovered = false;
                                            }
                                        }
                                    }
                                    if subscriptions.contains_key("ACCT_ACTIVITY") {
                                        let mut ready = true;
                                        match client.get_account_numbers().await {
                                            Ok(accounts) => {
                                                for account in accounts {
                                                    if !client
                                                        .reconcile_account(
                                                            &account.hash_value,
                                                            status.generation,
                                                        )
                                                        .await
                                                        .is_ok_and(|r| r.reconciled)
                                                    {
                                                        ready = false;
                                                    }
                                                }
                                            }
                                            Err(_) => ready = false,
                                        }
                                        recovered &= ready;
                                        if ready {
                                            cache.mark_accounts_reconciled(status.generation);
                                        }
                                    }
                                    recovered
                                };
                                let recovered = tokio::time::timeout(
                                    policy.deadline.max(std::time::Duration::from_millis(1)),
                                    recover,
                                )
                                .await
                                .unwrap_or(false);
                                metrics::histogram!(crate::metrics::STREAM_RECOVERY_SECONDS)
                                    .record(started.elapsed().as_secs_f64());
                                metrics::gauge!(crate::metrics::STREAM_RECOVERY_READY)
                                    .set(if recovered { 1.0 } else { 0.0 });
                                let current = stream.status();
                                if recovered
                                    && current.ready
                                    && current.generation == status.generation
                                {
                                    client.resume_submissions();
                                } else {
                                    client.suspend_submissions("REST baseline recovery incomplete");
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
                    for item in data.data {
                        // An unknown service keeps its numbered fields.
                        let table = item.service().map_or(&[][..], StreamService::fields);
                        for content in item.content {
                            let mut value = content.to_named_value(table);
                            if !value.is_object() {
                                continue;
                            }
                            redact::apply_stream_content(&item.service, &mut value);
                            cache.put(item.service.clone(), content.key, value, item.timestamp);
                        }
                    }
                }
                _ => {}
            }
        }
        client.suspend_submissions("Stream delivery stopped");
        cache.clear();
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recent_delivery_does_not_make_old_broker_data_fresh() {
        let cache = StreamCache::new();
        cache.update_status(StreamStatus {
            ready: true,
            generation: 1,
            lost_messages: 0,
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
    fn recovery_rejects_stale_generations_and_old_broker_quotes() {
        let cache = StreamCache::new();
        cache.require_baseline();
        cache.update_status(StreamStatus {
            ready: true,
            generation: 2,
            lost_messages: 1,
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
        assert!(!cache.apply_baseline(
            2,
            "LEVELONE_EQUITIES",
            "AAPL",
            json!({"quote":{"bidPrice":10,"askPrice":11,"quoteTime":now-3000}})
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
        });
        assert!(cache.snapshot("LEVELONE_EQUITIES", None).is_empty());
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
