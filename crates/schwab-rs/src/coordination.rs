//! Coordination contract for local workers or a deployment-owned remote service.
use crate::{
    error::{Error, Result},
    storage::{StateDatabase, sql},
};
use async_trait::async_trait;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub resource: String,
    pub owner: String,
    pub epoch: u64,
    pub expires_at: i64,
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
    if ttl < Duration::from_secs(1) || ttl > Duration::from_secs(300) {
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
        self.run(move|c|{
            let tx=c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql)?;
            let now=chrono::Utc::now().timestamp_millis();let expires_at=expiry(now,ttl)?;
            let old:Option<(String,i64,i64)>=tx.query_row("SELECT owner,epoch,expires_at FROM leases WHERE resource=?1",[&resource],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(sql)?;
            if old.as_ref().is_some_and(|(_,_,until)|*until>now){return Err(Error::Storage("Resource already has a live owner".into()));}
            let epoch=old.map_or(1,|(_,epoch,_)|epoch+1);
            tx.execute("INSERT INTO leases VALUES(?1,?2,?3,?4) ON CONFLICT(resource) DO UPDATE SET owner=excluded.owner,epoch=excluded.epoch,expires_at=excluded.expires_at",params![resource,owner,{ epoch },expires_at]).map_err(sql)?;
            tx.commit().map_err(sql)?;Ok(Lease{resource,owner,epoch:epoch as u64,expires_at})
        }).await
    }
    async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<Lease> {
        let mut lease = lease.clone();
        self.run(move|c|{
            let now=chrono::Utc::now().timestamp_millis();let expires_at=expiry(now,ttl)?;
            if c.execute("UPDATE leases SET expires_at=?1 WHERE resource=?2 AND owner=?3 AND epoch=?4 AND expires_at>?5",params![expires_at,lease.resource,lease.owner,lease.epoch as i64,now]).map_err(sql)?!=1{return Err(Error::Storage("Lease expired or fenced".into()));}
            lease.expires_at=expires_at;Ok(lease)
        }).await
    }
    async fn release(&self, lease: &Lease) -> Result<()> {
        let lease = lease.clone();
        self.run(move |c| {
            c.execute(
                "UPDATE leases SET expires_at=0 WHERE resource=?1 AND owner=?2 AND epoch=?3",
                params![lease.resource, lease.owner, lease.epoch as i64],
            )
            .map_err(sql)?;
            Ok(())
        })
        .await
    }
    async fn valid(&self, lease: &Lease) -> Result<bool> {
        let lease = lease.clone();
        self.run(move|c|c.query_row("SELECT EXISTS(SELECT 1 FROM leases WHERE resource=?1 AND owner=?2 AND epoch=?3 AND expires_at>?4)",params![lease.resource,lease.owner,lease.epoch as i64,chrono::Utc::now().timestamp_millis()],|r|r.get(0)).map_err(sql)).await
    }
}

/// Holds and renews ownership while an operation is running. Lease loss cancels
/// the operation; a broker write already in flight still requires reconciliation.
pub struct LeaseSession {
    lease: Lease,
    lost: tokio::sync::watch::Receiver<bool>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    closed: Option<tokio::sync::oneshot::Receiver<()>>,
}
impl LeaseSession {
    pub async fn acquire(backend: std::sync::Arc<dyn Coordinator>, resource: &str) -> Result<Self> {
        let ttl = Duration::from_secs(30);
        let lease = backend
            .acquire(resource, &uuid::Uuid::new_v4().to_string(), ttl)
            .await?;
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let (lost_tx, lost) = tokio::sync::watch::channel(false);
        let (closed_tx, closed) = tokio::sync::oneshot::channel();
        let held = lease.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _=&mut stopped => break,
                    _=tokio::time::sleep(Duration::from_secs(5)) => {
                        let renewal=tokio::time::timeout(Duration::from_secs(5),backend.renew(&held,ttl)).await;
                        if !matches!(renewal,Ok(Ok(_))) { let _=lost_tx.send(true); break; }
                    }
                }
            }
            let _ = backend.release(&held).await;
            let _ = closed_tx.send(());
        });
        Ok(Self {
            lease,
            lost,
            stop: Some(stop),
            closed: Some(closed),
        })
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
        let result = tokio::select! { result=future=>result, _=self.lost.changed()=>Err(Error::OwnershipLost) };
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
