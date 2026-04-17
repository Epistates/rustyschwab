//! In-memory cache of the latest streaming message per (service, key).
//!
//! MCP is request/response, but `StreamClient` yields a push-driven message
//! stream. We bridge the gap: a background task drains the receiver and
//! writes the most recent content for each subscribed key into this cache,
//! which tools can then query.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use schwab_rs::MessageReceiver;
use schwab_rs::types::streaming::StreamMessage;

/// Arc-shareable cache of streaming snapshots.
///
/// Keyed by `(service_name, key)` where `service_name` is the SCREAMING
/// case API string (e.g. `LEVELONE_EQUITIES`) and `key` is the symbol /
/// contract / book key carried in the message.
#[derive(Clone, Default)]
pub struct StreamCache {
    inner: Arc<RwLock<HashMap<(String, String), serde_json::Value>>>,
}

impl StreamCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert/overwrite the latest value for a given (service, key).
    fn put(&self, service: String, key: String, value: serde_json::Value) {
        self.inner.write().insert((service, key), value);
    }

    /// Read a snapshot of all entries matching `service`. If `keys` is
    /// `Some`, only those keys are returned; otherwise every cached key
    /// for that service is returned.
    pub fn snapshot(
        &self,
        service: &str,
        keys: Option<&[String]>,
    ) -> HashMap<String, serde_json::Value> {
        let guard = self.inner.read();
        guard
            .iter()
            .filter(|((svc, _), _)| svc == service)
            .filter(|((_, k), _)| match keys {
                Some(list) => list.iter().any(|w| w == k),
                None => true,
            })
            .map(|((_, k), v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Drain the underlying cache (used on disconnect).
    pub fn clear(&self) {
        self.inner.write().clear();
    }
}

/// Spawn a background task that reads from `rx` until it closes, writing
/// each `StreamData` content item into the cache. `Response` and `Notify`
/// frames are ignored (they carry no per-key payload).
pub fn spawn_cache_feeder(mut rx: MessageReceiver, cache: StreamCache) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let StreamMessage::Data(data) = msg {
                for item in data.data {
                    for content in item.content {
                        // `content` is `DataContent { key, fields }`. Serialize
                        // the whole struct so consumers get the raw shape.
                        let value = match serde_json::to_value(&content) {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(error = %e, "failed to serialize stream data content");
                                continue;
                            }
                        };
                        cache.put(item.service.clone(), content.key, value);
                    }
                }
            }
        }
        tracing::info!("stream cache feeder exiting — receiver closed");
    })
}
