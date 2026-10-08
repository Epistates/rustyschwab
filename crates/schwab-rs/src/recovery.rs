//! Restartable account recovery worker. Consumers can subscribe to readiness.
use crate::{SchwabClient, error::Result, reconciliation::ReconciliationOptions};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};

/// How often an incremental worker still scans each account's full order
/// lookback, as a backstop for anything an incremental window could miss.
pub const FULL_RESCAN_EVERY: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecoveryStatus {
    pub generation: u64,
    pub ready: bool,
    pub pending_accounts: Vec<String>,
    pub last_success: Option<i64>,
}
#[must_use = "dropping the worker stops recovery"]
pub struct RecoveryWorker {
    status: tokio::sync::watch::Receiver<RecoveryStatus>,
    task: tokio::task::JoinHandle<()>,
}
impl std::fmt::Debug for RecoveryWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryWorker")
            .field("status", &*self.status.borrow())
            .finish_non_exhaustive()
    }
}
impl RecoveryWorker {
    /// Reconcile `accounts` every `interval` (at least a second), or sooner
    /// when [`SchwabClient::request_reconciliation`] is called, though never
    /// less than half an interval apart.
    ///
    /// Each pass costs two requests per account (the order list and the
    /// account) plus one per working order entered before the pass's order
    /// window; after the first complete pass the window starts at the
    /// previous pass (less [`SINCE_OVERLAP`](crate::reconciliation::SINCE_OVERLAP))
    /// rather than the full lookback,
    /// with a full scan every [`FULL_RESCAN_EVERY`]. The interval trades that
    /// request cost against baseline freshness: a risk policy refuses orders
    /// against a baseline older than its `max_snapshot_age_ms`.
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
            // Start of each account's last complete scan, and of its last full one.
            let mut since: HashMap<String, DateTime<Utc>> = HashMap::new();
            let mut full: HashMap<String, DateTime<Utc>> = HashMap::new();
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
                    let rescan = full.get(account).is_none_or(|at| {
                        (Utc::now() - *at).to_std().unwrap_or_default() >= FULL_RESCAN_EVERY
                    });
                    let options = ReconciliationOptions {
                        since: if rescan {
                            None
                        } else {
                            since.get(account).copied()
                        },
                        ..Default::default()
                    };
                    match client
                        .reconcile_account_with_options(account, current, options)
                        .await
                    {
                        // A newer pass (another worker's) committed first:
                        // its verdict is the account's.
                        Ok(report) if report.superseded => {
                            let reconciled = client
                                .execution_store()
                                .account_state(account)
                                .await
                                .ok()
                                .flatten()
                                .is_some_and(|s| s.reconciled);
                            if !reconciled {
                                pending.push(account.clone());
                            }
                        }
                        // The scan completed, so the window may advance even
                        // if intents remain unresolved: they widen it anyway.
                        Ok(report) => {
                            let started = DateTime::from_timestamp_millis(report.started_at)
                                .unwrap_or_else(Utc::now);
                            since.insert(account.clone(), started);
                            if rescan {
                                full.insert(account.clone(), started);
                            }
                            if !report.reconciled {
                                tracing::warn!(
                                    account = %crate::utils::mask_account(account),
                                    unresolved = report.unresolved_attempts.len(),
                                    "Account has unresolved submissions; trading waits for reconciliation"
                                );
                                pending.push(account.clone());
                            }
                        }
                        // An incomplete scan leaves the window where it was.
                        Err(error) => {
                            metrics::counter!(crate::metrics::RECOVERY_FAILURES_TOTAL).increment(1);
                            tracing::warn!(
                                account = %crate::utils::mask_account(account),
                                %error,
                                "Reconciliation pass failed; the account is not ready"
                            );
                            pending.push(account.clone());
                        }
                    }
                }
                let ready = pending.is_empty();
                tx.send_modify(|s| {
                    s.ready = ready;
                    s.pending_accounts = pending;
                    if ready {
                        s.last_success = Some(Utc::now().timestamp_millis());
                    }
                });
                metrics::gauge!(crate::metrics::RECOVERY_READY).set(if ready { 1.0 } else { 0.0 });
                // Activity comes in bursts (a fill, its order update, the
                // balance change): it pulls the next pass forward, but never
                // to less than half an interval after this one.
                let finished = tokio::time::Instant::now();
                let interval = interval.max(Duration::from_secs(1));
                tokio::select! {
                    _ = tokio::time::sleep_until(finished + interval) => {}
                    _ = client.reconciliation_requested() => {
                        tokio::time::sleep_until(finished + interval / 2).await;
                    }
                }
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

/// Rough share of the request budget a worker reconciling `accounts` every
/// `interval` consumes, counting two requests per account per pass (working
/// orders older than the window add one each). Deployments should keep it
/// well below one: sustained account activity can pull passes to half an
/// interval apart, doubling it.
pub fn budget_share(accounts: usize, interval: Duration, requests_per_second: u32) -> f64 {
    let per_second = (2 * accounts) as f64 / interval.as_secs_f64().max(1.0);
    per_second / f64::from(requests_per_second.max(1))
}
