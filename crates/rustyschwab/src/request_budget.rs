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

/// The in-process request budget of one credential at one instant.
///
/// `used_last_minute` counts every attempt this process admitted in the
/// trailing 60 seconds, so `remaining` is what more it may send before the
/// budget starts waiting; the coordinator's shared quota may still wait
/// first when other processes use the same credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RequestBudgetUsage {
    pub per_minute: usize,
    pub used_last_minute: usize,
    pub remaining: usize,
}

/// Who a request is for, so ordinary reads cannot use up the budget that
/// protective writes and reconciliation depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Priority {
    /// Reads and everything else: up to [`NORMAL_SHARE`] of the minute.
    Normal,
    /// Reconciliation, which keeps the risk baseline fresh: up to
    /// [`RECOVERY_SHARE`].
    Recovery,
    /// Order submissions and cancels: the whole budget.
    Critical,
}
/// Share of each trailing minute's budget reads may use.
const NORMAL_SHARE: f64 = 0.75;
/// Share reconciliation may use; the rest is for submissions and cancels.
const RECOVERY_SHARE: f64 = 0.9;

impl Priority {
    /// The admissions in the trailing minute below which a request of this
    /// priority may go, out of `per_minute`.
    fn ceiling(self, per_minute: usize) -> usize {
        let share = match self {
            Self::Normal => NORMAL_SHARE,
            Self::Recovery => RECOVERY_SHARE,
            Self::Critical => 1.0,
        };
        ((per_minute as f64 * share).floor() as usize).max(1)
    }
}

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
    pub async fn admit(&self, coordinator: &dyn Coordinator, priority: Priority) -> Result<()> {
        self.acquire(priority).await;
        let (rps, burst) = self.configuration;
        loop {
            let wait = coordinator.quota(&self.key, rps, burst).await?;
            if wait.is_zero() {
                return Ok(());
            }
            tokio::time::sleep(wait).await;
        }
    }
    /// This process's use of the budget over the trailing minute.
    pub fn usage(&self) -> RequestBudgetUsage {
        let mut recent = self.recent.lock();
        prune(&mut recent, Instant::now());
        RequestBudgetUsage {
            per_minute: self.per_minute,
            used_last_minute: recent.len(),
            remaining: self.per_minute.saturating_sub(recent.len()),
        }
    }
    async fn acquire(&self, priority: Priority) {
        let ceiling = priority.ceiling(self.per_minute);
        loop {
            let wake = {
                let now = Instant::now();
                let mut recent = self.recent.lock();
                prune(&mut recent, now);
                if recent.len() < ceiling {
                    None
                } else {
                    // The admission whose expiry brings the count under
                    // this priority's ceiling.
                    Some(recent[recent.len() - ceiling] + Duration::from_secs(60))
                }
            };
            match wake {
                Some(wake) => tokio::time::sleep_until(wake).await,
                None => {
                    self.limiter.until_ready().await;
                    let now = Instant::now();
                    let mut recent = self.recent.lock();
                    prune(&mut recent, now);
                    // Re-checked: others may have been admitted while this
                    // one waited on the per-second limiter.
                    if recent.len() < ceiling {
                        recent.push_back(now);
                        return;
                    }
                }
            }
        }
    }
}

/// Drop admissions that have left the trailing minute.
fn prune(recent: &mut VecDeque<Instant>, now: Instant) {
    while recent
        .front()
        .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
    {
        recent.pop_front();
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
        assert_eq!(
            a.usage(),
            RequestBudgetUsage {
                per_minute: 60,
                used_last_minute: 0,
                remaining: 60
            }
        );
        a.acquire(Priority::Critical).await;
        assert_eq!(b.usage().used_last_minute, 1);
        assert_eq!(b.usage().remaining, 59);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), b.acquire(Priority::Critical))
                .await
                .is_err()
        );
        let changed = RateLimitConfig {
            requests_per_second: 2,
            ..config
        };
        assert!(RequestBudget::shared(key, &changed).is_err());
    }

    /// Reads stop at their share of the minute; reconciliation and then
    /// writes keep the rest.
    #[tokio::test]
    async fn reads_leave_room_for_reconciliation_and_writes() {
        // 60 a minute, with a burst large enough that only the minute binds:
        // reads get 45, reconciliation up to 54, writes all 60.
        let config = RateLimitConfig {
            requests_per_second: 1,
            burst_size: 100,
            ..Default::default()
        };
        let budget = RequestBudget::shared(uuid::Uuid::new_v4().to_string(), &config).unwrap();
        for _ in 0..Priority::Normal.ceiling(60) {
            budget.acquire(Priority::Normal).await;
        }
        let short = Duration::from_millis(50);
        assert!(
            tokio::time::timeout(short, budget.acquire(Priority::Normal))
                .await
                .is_err(),
            "reads are at their share"
        );
        tokio::time::timeout(short, budget.acquire(Priority::Recovery))
            .await
            .expect("reconciliation still goes");
        tokio::time::timeout(short, budget.acquire(Priority::Critical))
            .await
            .expect("writes still go");
    }
}
