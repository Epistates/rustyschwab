//! Restartable account recovery worker. Consumers can subscribe to readiness.
use crate::{SchwabClient, error::Result};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecoveryStatus {
    pub generation: u64,
    pub ready: bool,
    pub pending_accounts: Vec<String>,
    pub last_success: Option<i64>,
}
pub struct RecoveryWorker {
    status: tokio::sync::watch::Receiver<RecoveryStatus>,
    task: tokio::task::JoinHandle<()>,
}
impl RecoveryWorker {
    pub fn start(
        client: Arc<SchwabClient>,
        accounts: Vec<String>,
        generation: u64,
        interval: Duration,
    ) -> Self {
        let (tx, status) = tokio::sync::watch::channel(RecoveryStatus {
            generation,
            pending_accounts: accounts.clone(),
            ..Default::default()
        });
        let task = tokio::spawn(async move {
            loop {
                let mut pending = Vec::new();
                for account in &accounts {
                    let current = client
                        .execution_store()
                        .account_state(account)
                        .await
                        .ok()
                        .flatten()
                        .map_or(generation, |s| s.generation.max(generation));
                    if !client
                        .reconcile_account(account, current)
                        .await
                        .is_ok_and(|r| r.reconciled)
                    {
                        pending.push(account.clone());
                    }
                }
                let ready = pending.is_empty();
                tx.send_modify(|s| {
                    s.ready = ready;
                    s.pending_accounts = pending;
                    if ready {
                        s.last_success = Some(chrono::Utc::now().timestamp_millis());
                    }
                });
                metrics::gauge!("schwab_recovery_ready").set(if ready { 1.0 } else { 0.0 });
                tokio::time::sleep(interval.max(Duration::from_secs(1))).await;
            }
        });
        Self { status, task }
    }
    pub fn status(&self) -> RecoveryStatus {
        self.status.borrow().clone()
    }
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<RecoveryStatus> {
        self.status.clone()
    }
    pub async fn wait_ready(&mut self) -> Result<()> {
        while !self.status.borrow().ready {
            self.status
                .changed()
                .await
                .map_err(|_| crate::Error::Storage("Recovery worker stopped".into()))?;
        }
        Ok(())
    }
}
impl Drop for RecoveryWorker {
    fn drop(&mut self) {
        self.task.abort();
    }
}
