//! Coordination contract for local workers or a deployment-owned remote service.
use crate::{
    error::{Error, Result},
    storage::{StateDatabase, sql},
};
use async_trait::async_trait;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const SELECT_LEASE: &str = "SELECT owner,epoch,expires_at FROM leases WHERE resource=?1";
const UPSERT_LEASE: &str = "INSERT INTO leases VALUES(?1,?2,?3,?4) \
     ON CONFLICT(resource) DO UPDATE SET owner=excluded.owner,epoch=excluded.epoch,\
     expires_at=excluded.expires_at";
/// Extends a lease only while the caller still holds the current epoch, and
/// the clock has not moved back past it (see [`MAX_TTL`]).
const RENEW_LEASE: &str = "UPDATE leases SET expires_at=?1 \
     WHERE resource=?2 AND owner=?3 AND epoch=?4 AND expires_at>?5 AND expires_at<=?6";
const RELEASE_LEASE: &str =
    "UPDATE leases SET expires_at=0 WHERE resource=?1 AND owner=?2 AND epoch=?3";
const SELECT_LEASE_HELD: &str = "SELECT EXISTS(SELECT 1 FROM leases \
     WHERE resource=?1 AND owner=?2 AND epoch=?3 AND expires_at>?4 AND expires_at<=?5)";

/// The longest lease a store grants. An expiry further ahead than this was
/// written before the clock moved back, so the lease is treated as neither
/// held nor free: fail closed, as [`Coordinator::quota`] does.
const MAX_TTL: Duration = Duration::from_secs(300);

fn clock_moved_back() -> Error {
    Error::Storage("Clock moved backwards; lease state is not trustworthy".into())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub resource: String,
    pub owner: String,
    pub epoch: u64,
    pub expires_at: i64,
}
/// Whether `lease` is still the live owner of its resource in this state file.
/// Dispatch fences call this inside the same transaction as the write they
/// guard so ownership cannot lapse between the check and the commit.
pub(crate) fn lease_held(c: &rusqlite::Connection, lease: &Lease) -> Result<bool> {
    // Epochs are allocated from an SQLite integer, so one that does not fit
    // was never issued by this store.
    let Ok(epoch) = i64::try_from(lease.epoch) else {
        return Ok(false);
    };
    let now = chrono::Utc::now().timestamp_millis();
    c.query_row(
        SELECT_LEASE_HELD,
        params![
            lease.resource,
            lease.owner,
            epoch,
            now,
            now + MAX_TTL.as_millis() as i64
        ],
        |r| r.get(0),
    )
    .map_err(sql)
}
#[async_trait]
pub trait Coordinator: std::fmt::Debug + Send + Sync {
    /// Atomically reserve quota or return the minimum wait before retrying.
    async fn quota(&self, key: &str, rps: u32, burst: u32) -> Result<Duration>;
    async fn acquire(&self, resource: &str, owner: &str, ttl: Duration) -> Result<Lease>;
    async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<Lease>;
    async fn release(&self, lease: &Lease) -> Result<()>;
    async fn valid(&self, lease: &Lease) -> Result<bool>;
}
fn expiry(now: i64, ttl: Duration) -> Result<i64> {
    if ttl < Duration::from_secs(1) || ttl > MAX_TTL {
        return Err(Error::InvalidParameter(
            "Lease TTL must be between 1 and 300 seconds".into(),
        ));
    }
    Ok(now + ttl.as_millis() as i64)
}
#[async_trait]
impl Coordinator for StateDatabase {
    async fn quota(&self, key: &str, rps: u32, burst: u32) -> Result<Duration> {
        if rps == 0 || burst == 0 {
            return Err(Error::Config("Quota must be positive".into()));
        }
        let key = key.to_owned();
        self.run(move |c| {
            let tx = c
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(sql)?;
            tx.execute(
                "INSERT OR IGNORE INTO quota_config VALUES(?1,?2,?3)",
                params![key, rps, burst],
            )
            .map_err(sql)?;
            let config: (u32, u32) = tx
                .query_row(
                    "SELECT per_second,burst FROM quota_config WHERE key=?1",
                    [&key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(sql)?;
            if config != (rps, burst) {
                return Err(Error::Config(
                    "Credential quota configuration conflicts with durable configuration".into(),
                ));
            }
            let now = chrono::Utc::now().timestamp_millis();
            let last: Option<i64> = tx
                .query_row("SELECT MAX(at) FROM quota WHERE key=?1", [&key], |r| {
                    r.get(0)
                })
                .map_err(sql)?;
            if last.is_some_and(|last| last > now) {
                return Err(Error::Storage("Clock moved backwards; quota denied".into()));
            }
            tx.execute(
                "DELETE FROM quota WHERE key=?1 AND at<=?2",
                params![key, now - 60000],
            )
            .map_err(sql)?;
            let (count, oldest): (i64, Option<i64>) = tx
                .query_row(
                    "SELECT COUNT(*),MIN(at) FROM quota WHERE key=?1",
                    [&key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(sql)?;
            let (short, shortest): (i64, Option<i64>) = tx
                .query_row(
                    "SELECT COUNT(*),MIN(at) FROM quota WHERE key=?1 AND at>?2",
                    params![key, now - 1000],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(sql)?;
            // A rolling second cap and rolling minute cap count actual attempts.
            let wait = if count >= i64::from(rps) * 60 {
                oldest.unwrap_or(now) + 60000 - now
            } else if short >= i64::from(burst) {
                shortest.unwrap_or(now) + 1000 - now
            } else {
                0
            };
            if wait == 0 {
                tx.execute("INSERT INTO quota VALUES(?1,?2)", params![key, now])
                    .map_err(sql)?;
            }
            tx.commit().map_err(sql)?;
            Ok(Duration::from_millis(wait.max(0) as u64))
        })
        .await
    }
    async fn acquire(&self, resource: &str, owner: &str, ttl: Duration) -> Result<Lease> {
        let resource = resource.to_owned();
        let owner = owner.to_owned();
        self.run(move |c| {
            let tx = c
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(sql)?;
            let now = chrono::Utc::now().timestamp_millis();
            let expires_at = expiry(now, ttl)?;
            let old: Option<(String, i64, i64)> = tx
                .query_row(SELECT_LEASE, [&resource], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()
                .map_err(sql)?;
            if old
                .as_ref()
                .is_some_and(|(_, _, until)| *until > now + MAX_TTL.as_millis() as i64)
            {
                return Err(clock_moved_back());
            }
            if old.as_ref().is_some_and(|(_, _, until)| *until > now) {
                return Err(Error::LeaseBusy);
            }
            let epoch = old.map_or(1, |(_, epoch, _)| epoch + 1);
            tx.execute(UPSERT_LEASE, params![resource, owner, epoch, expires_at])
                .map_err(sql)?;
            tx.commit().map_err(sql)?;
            Ok(Lease {
                resource,
                owner,
                epoch: epoch as u64,
                expires_at,
            })
        })
        .await
    }
    async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<Lease> {
        let mut lease = lease.clone();
        self.run(move |c| {
            let now = chrono::Utc::now().timestamp_millis();
            let expires_at = expiry(now, ttl)?;
            let renewed = c
                .execute(
                    RENEW_LEASE,
                    params![
                        expires_at,
                        lease.resource,
                        lease.owner,
                        lease.epoch as i64,
                        now,
                        now + MAX_TTL.as_millis() as i64
                    ],
                )
                .map_err(sql)?;
            if renewed != 1 {
                return Err(Error::Storage("Lease expired or fenced".into()));
            }
            lease.expires_at = expires_at;
            Ok(lease)
        })
        .await
    }
    async fn release(&self, lease: &Lease) -> Result<()> {
        let lease = lease.clone();
        self.run(move |c| {
            c.execute(
                RELEASE_LEASE,
                params![lease.resource, lease.owner, lease.epoch as i64],
            )
            .map_err(sql)?;
            Ok(())
        })
        .await
    }
    async fn valid(&self, lease: &Lease) -> Result<bool> {
        let lease = lease.clone();
        self.run(move |c| lease_held(c, &lease)).await
    }
}

const LEASE_TTL: Duration = Duration::from_secs(30);
const RENEW_EVERY: Duration = Duration::from_secs(5);
/// Longer than SQLite's five-second busy timeout, so lock contention at that
/// boundary is not mistaken for a store that stopped answering.
const RENEW_WITHIN: Duration = Duration::from_secs(8);

/// Holds and renews ownership while an operation is running. Lease loss cancels
/// the operation; a broker write already in flight still requires reconciliation.
pub struct LeaseSession {
    lease: Lease,
    lost: tokio::sync::watch::Receiver<bool>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    closed: Option<tokio::sync::oneshot::Receiver<()>>,
}
impl LeaseSession {
    /// Take the lease now or fail with [`Error::LeaseBusy`]: for a standby
    /// that should not wait on a live owner (the stream).
    pub async fn acquire(backend: std::sync::Arc<dyn Coordinator>, resource: &str) -> Result<Self> {
        let lease = backend
            .acquire(resource, &uuid::Uuid::new_v4().to_string(), LEASE_TTL)
            .await?;
        Ok(Self::hold(backend, lease))
    }

    /// Take the lease, waiting while another operation holds it (in this
    /// process or another sharing the state file) for at most `wait`.
    /// Submissions serialize on the credential's lease this way instead of
    /// failing because a concurrent one, possibly for another account, was
    /// mid-flight. Only [`Error::LeaseBusy`] is waited out.
    pub async fn acquire_waiting(
        backend: std::sync::Arc<dyn Coordinator>,
        resource: &str,
        wait: Duration,
    ) -> Result<Self> {
        let owner = uuid::Uuid::new_v4().to_string();
        let deadline = tokio::time::Instant::now() + wait;
        // A holder keeps the lease for a broker round trip, and each attempt
        // is a write transaction on the state file the holder is also
        // using: back off from 20–40 ms to 100–200 ms between attempts.
        let mut base = 20u64;
        loop {
            match backend.acquire(resource, &owner, LEASE_TTL).await {
                Ok(lease) => return Ok(Self::hold(backend, lease)),
                Err(Error::LeaseBusy) if tokio::time::Instant::now() < deadline => {
                    // Jitter keeps waiters from retrying in lockstep.
                    let pause = Duration::from_millis(base + rand::random::<u64>() % base);
                    base = (base * 2).min(100);
                    tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + pause))
                        .await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn hold(backend: std::sync::Arc<dyn Coordinator>, lease: Lease) -> Self {
        let ttl = LEASE_TTL;
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let (lost_tx, lost) = tokio::sync::watch::channel(false);
        let (closed_tx, closed) = tokio::sync::oneshot::channel();
        let held = lease.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    _ = tokio::time::sleep(RENEW_EVERY) => {
                        let renewal = tokio::time::timeout(RENEW_WITHIN, backend.renew(&held, ttl))
                            .await;
                        match renewal {
                            Ok(Ok(_)) => {}
                            // The store answered: the lease is gone.
                            Ok(Err(_)) => {
                                let _ = lost_tx.send(true);
                                break;
                            }
                            // The store did not answer in time. The renewal may
                            // still commit (blocking SQLite work outlives a
                            // dropped future), so ask before giving up: an
                            // unconfirmed lease is treated as lost, and never
                            // released, since it may belong to a successor.
                            Err(_) => {
                                let held_now = tokio::time::timeout(RENEW_WITHIN, backend.valid(&held))
                                    .await;
                                if !matches!(held_now, Ok(Ok(true))) {
                                    let _ = lost_tx.send(true);
                                    let _ = closed_tx.send(());
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            let _ = backend.release(&held).await;
            let _ = closed_tx.send(());
        });
        Self {
            lease,
            lost,
            stop: Some(stop),
            closed: Some(closed),
        }
    }
    pub fn lease(&self) -> &Lease {
        &self.lease
    }
    pub async fn run<T>(
        &mut self,
        future: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        if *self.lost.borrow() {
            return Err(Error::OwnershipLost);
        }
        let result = tokio::select! {
            result = future => result,
            _ = self.lost.changed() => Err(Error::OwnershipLost),
        };
        self.stop.take();
        if let Some(closed) = self.closed.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), closed).await;
        }
        result
    }
}
impl Drop for LeaseSession {
    fn drop(&mut self) {
        self.stop.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lease written before the clock moved back expires further ahead
    /// than any TTL allows: it is neither held nor free, as with quota.
    #[tokio::test]
    async fn a_lease_from_before_a_backward_clock_jump_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let db = StateDatabase::new(dir.path().join("state.json"));
        let lease = db
            .acquire("stream", "a", Duration::from_secs(10))
            .await
            .unwrap();
        // As if the clock had just moved back an hour.
        let resource = lease.resource.clone();
        db.run(move |c| {
            c.execute(
                "UPDATE leases SET expires_at=expires_at+3600000 WHERE resource=?1",
                [resource],
            )
            .map_err(sql)?;
            Ok(())
        })
        .await
        .unwrap();
        assert!(!db.valid(&lease).await.unwrap());
        assert!(db.renew(&lease, Duration::from_secs(10)).await.is_err());
        assert!(matches!(
            db.acquire("stream", "b", Duration::from_secs(10)).await,
            Err(Error::Storage(_))
        ));
    }
}
