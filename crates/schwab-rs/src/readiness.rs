//! Readiness is local evidence, not a synthetic live broker probe.
use crate::{SchwabClient, error::Result};
use serde::{Deserialize, Serialize};
/// Serialized in camelCase: this is the body of the `/readyz` health
/// endpoint and of the MCP `get_server_status` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Readiness {
    pub reads_ready: bool,
    pub trading_ready: bool,
    pub tokens_valid: bool,
    pub storage_healthy: bool,
    pub pending_submissions: usize,
    pub unreconciled_accounts: usize,
    pub oldest_baseline_age_ms: Option<i64>,
    pub circuit_open: bool,
}
impl SchwabClient {
    pub async fn readiness(&self, accounts: &[String], max_age_ms: i64) -> Result<Readiness> {
        let tokens_valid = self.auth_manager().has_valid_tokens();
        let circuit_open = self.circuit_state() == crate::CircuitState::Open;
        let has_accounts = !accounts.is_empty();
        let store = self.execution_store();
        store.healthcheck().await?;
        let pending = store.pending_count().await?;
        let now = chrono::Utc::now().timestamp_millis();
        let mut unreconciled = 0;
        let mut oldest = None;
        for account in accounts {
            match store.account_state(account).await? {
                Some(state) => {
                    let age = now - state.observed_at;
                    oldest = Some(oldest.unwrap_or(0).max(age));
                    if !state.reconciled || age < 0 || age > max_age_ms {
                        unreconciled += 1;
                    }
                }
                None => unreconciled += 1,
            }
        }
        let reads_ready = tokens_valid && !circuit_open;
        let trading_ready = reads_ready
            && pending == 0
            && unreconciled == 0
            && has_accounts
            && !self.submissions_suspended()
            && store.operator_halt().await?.is_none()
            && self.risk_provider().is_some()
            && max_age_ms > 0;
        metrics::gauge!(crate::metrics::PENDING_SUBMISSIONS).set(pending as f64);
        metrics::gauge!(crate::metrics::UNRECONCILED_ACCOUNTS).set(unreconciled as f64);
        metrics::gauge!(crate::metrics::TRADING_READY).set(if trading_ready { 1.0 } else { 0.0 });
        if let Some(age) = oldest {
            metrics::gauge!(crate::metrics::BASELINE_AGE_SECONDS).set(age as f64 / 1000.0);
        }
        Ok(Readiness {
            reads_ready,
            trading_ready,
            tokens_valid,
            storage_healthy: true,
            pending_submissions: pending,
            unreconciled_accounts: unreconciled,
            oldest_baseline_age_ms: oldest,
            circuit_open,
        })
    }
}
