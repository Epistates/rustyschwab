//! Process-wide quota by credential and API origin, including every wire attempt.
use crate::{
    config::RateLimitConfig,
    coordination::Coordinator,
    error::{Error, Result},
};
use governor::{Quota, RateLimiter};
use parking_lot::Mutex;
use std::{
    collections::{HashMap, VecDeque},
    num::NonZeroU32,
    sync::{Arc, LazyLock, Weak},
    time::Duration,
};
use tokio::time::Instant;

type Limiter = RateLimiter<
    governor::state::NotKeyed,
    governor::state::InMemoryState,
    governor::clock::DefaultClock,
>;
pub(crate) struct RequestBudget {
    key: String,
    limiter: Limiter,
    recent: Mutex<VecDeque<Instant>>,
    per_minute: usize,
    configuration: (u32, u32),
}
static BUDGETS: LazyLock<Mutex<HashMap<String, Weak<RequestBudget>>>> =
    LazyLock::new(Default::default);
impl RequestBudget {
    pub fn shared(key: String, config: &RateLimitConfig) -> Result<Arc<Self>> {
        let mut budgets = BUDGETS.lock();
        budgets.retain(|_, budget| budget.strong_count() > 0);
        if let Some(existing) = budgets.get(&key).and_then(Weak::upgrade) {
            if existing.configuration != (config.requests_per_second, config.burst_size) {
                return Err(Error::Config(
                    "Clients sharing credentials must use the same rate limits".into(),
                ));
            }
            return Ok(existing);
        }
        let rps = NonZeroU32::new(config.requests_per_second)
            .ok_or_else(|| Error::Config("requests_per_second must be positive".into()))?;
        let burst = NonZeroU32::new(config.burst_size)
            .ok_or_else(|| Error::Config("burst_size must be positive".into()))?;
        let budget = Arc::new(Self {
            key: key.clone(),
            limiter: RateLimiter::direct(Quota::per_second(rps).allow_burst(burst)),
            recent: Mutex::new(VecDeque::new()),
            per_minute: rps.get().saturating_mul(60) as usize,
            configuration: (rps.get(), burst.get()),
        });
        budgets.insert(key, Arc::downgrade(&budget));
        Ok(budget)
    }
    /// Wait for the in-process budget, then for the durable quota the
    /// coordinator shares with every other process on this credential. Both
    /// count the attempt only once it is admitted.
    pub async fn admit(&self, coordinator: &dyn Coordinator) -> Result<()> {
        self.acquire().await;
        let (rps, burst) = self.configuration;
        loop {
            let wait = coordinator.quota(&self.key, rps, burst).await?;
            if wait.is_zero() {
                return Ok(());
            }
            tokio::time::sleep(wait).await;
        }
    }
    async fn acquire(&self) {
        self.limiter.until_ready().await;
        loop {
            let wake = {
                let now = Instant::now();
                let mut recent = self.recent.lock();
                while recent
                    .front()
                    .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
                {
                    recent.pop_front();
                }
                if recent.len() < self.per_minute {
                    recent.push_back(now);
                    return;
                }
                *recent.front().unwrap() + Duration::from_secs(60)
            };
            tokio::time::sleep_until(wake).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn credential_budget_is_shared_and_waits_can_be_cancelled() {
        let key = uuid::Uuid::new_v4().to_string();
        let config = RateLimitConfig {
            requests_per_second: 1,
            burst_size: 1,
            ..Default::default()
        };
        let a = RequestBudget::shared(key.clone(), &config).unwrap();
        let b = RequestBudget::shared(key.clone(), &config).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        a.acquire().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), b.acquire())
                .await
                .is_err()
        );
        let changed = RateLimitConfig {
            requests_per_second: 2,
            ..config
        };
        assert!(RequestBudget::shared(key, &changed).is_err());
    }
}
