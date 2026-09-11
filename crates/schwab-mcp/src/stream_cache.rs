//! Service-aware snapshots with explicit freshness and a bounded activity log.
use crate::redact;
use parking_lot::RwLock;
use schwab_rs::{
    MessageReceiver,
    types::streaming::{StreamMessage, StreamStatus},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

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
    field_times: HashMap<String, i64>,
    received: i64,
    broker: i64,
    generation: u64,
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
                // A snapshot is fresh only when every numbered field was
                // updated within the window on the current generation.
                let broker_recent = entry.broker > 0
                    && entry.broker <= now
                    && now.saturating_sub(entry.broker) <= 2000;
                let fields_recent = entry
                    .field_times
                    .iter()
                    .filter(|(key, _)| key.parse::<u32>().is_ok())
                    .all(|(_, at)| now.saturating_sub(*at) <= 2000);
                let fresh = cache.status.ready
                    && (!cache.baseline_required || baseline.is_some())
                    && age <= 2000
                    && broker_recent
                    && entry.generation == cache.status.generation
                    && fields_recent;
                let snapshot = json!({
                    "content": entry.value,
                    "receivedAt": entry.received,
                    "brokerTimestamp": entry.broker,
                    "generation": entry.generation,
                    "ageMs": age,
                    "fresh": fresh,
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
    pub fn apply_baseline(&self, generation: u64, service: &str, key: &str, quote: Value) -> bool {
        let mut cache = self.inner.write();
        if !cache.status.ready || cache.status.generation != generation {
            return false;
        }
        let mut content = json!({"key":key});
        let fields: &[(u32, &str)] = match service {
            "LEVELONE_EQUITIES" => &[
                (1, "bidPrice"),
                (2, "askPrice"),
                (3, "lastPrice"),
                (4, "bidSize"),
                (5, "askSize"),
                (8, "totalVolume"),
                (9, "lastSize"),
            ],
            "LEVELONE_OPTIONS" => &[
                (2, "bidPrice"),
                (3, "askPrice"),
                (4, "lastPrice"),
                (8, "totalVolume"),
                (16, "bidSize"),
                (17, "askSize"),
                (18, "lastSize"),
            ],
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
        for (field, name) in fields {
            if let Some(v) = prices.get(*name) {
                content[field.to_string()] = v.clone();
            }
        }
        let now = chrono::Utc::now().timestamp_millis();
        let broker = prices.get("quoteTime").and_then(Value::as_i64).unwrap_or(0);
        if broker <= 0 || now < broker || now - broker > 2000 {
            return false;
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
                entry.field_times.insert(field.clone(), broker);
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
                                metrics::histogram!("schwab_stream_recovery_seconds")
                                    .record(started.elapsed().as_secs_f64());
                                metrics::gauge!("schwab_stream_recovery_ready").set(if recovered {
                                    1.0
                                } else {
                                    0.0
                                });
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
                        for content in item.content {
                            if let Ok(mut value) = serde_json::to_value(&content) {
                                redact::apply_stream_content(&item.service, &mut value);
                                cache.put(item.service.clone(), content.key, value, item.timestamp);
                            }
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
            json!({"1":10,"2":11}),
            now - 3000,
        );
        assert_eq!(
            cache.snapshot("LEVELONE_EQUITIES", None)["AAPL"]["fresh"],
            false
        );
        assert!(!cache.apply_baseline(
            1,
            "LEVELONE_EQUITIES",
            "AAPL",
            json!({"quote":{"bidPrice":null,"askPrice":11,"quoteTime":now}})
        ));
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
            json!({"1":12}),
            now,
        );
        assert_eq!(
            cache.snapshot("LEVELONE_EQUITIES", None)["AAPL"]["fresh"],
            false
        );
        let quote = json!({"quote":{"bidPrice":10,"askPrice":11,"quoteTime":now}});
        assert!(!cache.apply_baseline(1, "LEVELONE_EQUITIES", "AAPL", quote.clone()));
        assert!(!cache.apply_baseline(
            2,
            "LEVELONE_EQUITIES",
            "AAPL",
            json!({"quote":{"bidPrice":10,"askPrice":11,"quoteTime":now-3000}})
        ));
        assert!(cache.apply_baseline(2, "LEVELONE_EQUITIES", "AAPL", quote));
        let snapshot = cache.snapshot("LEVELONE_EQUITIES", None);
        assert_eq!(snapshot["AAPL"]["content"]["1"], 12);
        assert_eq!(snapshot["AAPL"]["content"]["2"], 11);
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
            json!({"1":10,"2":11}),
            100,
        );
        cache.put(
            "LEVELONE_EQUITIES".into(),
            "AAPL".into(),
            json!({"1":12}),
            101,
        );
        cache.put(
            "LEVELONE_EQUITIES".into(),
            "AAPL".into(),
            json!({"1":9}),
            99,
        );
        let snapshot = cache.snapshot("LEVELONE_EQUITIES", None);
        assert_eq!(snapshot["AAPL"]["content"], json!({"1":12,"2":11}));
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
