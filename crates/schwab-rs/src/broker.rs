//! Common gateway execution boundary for live and isolated paper backends.
use crate::{
    Result, SchwabClient,
    coordination::{Coordinator, Lease},
    execution::ExecutionStore,
    paper::PaperBroker,
    readiness::Readiness,
    types::trading::{OrderRequest, OrderSubmitResponse},
};
use std::{path::PathBuf, sync::Arc};

/// Trading commands share gateway policy/approval enforcement. Each implementation
/// owns execution and accounting; paper never constructs a live client.
#[async_trait::async_trait]
pub trait TradingBackend: Send + Sync {
    fn mode(&self) -> &'static str;
    fn execution_store(&self) -> Arc<dyn ExecutionStore>;
    fn coordinator(&self) -> Arc<dyn Coordinator>;
    fn principal_journal_path(&self, principal_digest: &str) -> PathBuf;
    fn begin_drain(&self);
    fn paper(&self) -> Option<&PaperBroker> {
        None
    }
    async fn readiness(&self, accounts: &[String], max_age_ms: i64) -> Result<Readiness>;
    async fn place_order_fenced(
        &self,
        account: &str,
        order: &OrderRequest,
        lease: &Lease,
    ) -> Result<OrderSubmitResponse>;
    async fn replace_order_fenced(
        &self,
        account: &str,
        id: &str,
        order: &OrderRequest,
        lease: &Lease,
    ) -> Result<OrderSubmitResponse>;
    async fn cancel_order(&self, account: &str, id: &str) -> Result<()>;
    async fn halt_submissions(&self, reason: &str, operator: &str) -> Result<()> {
        self.execution_store()
            .set_operator_halt(Some(reason.to_owned()), operator)
            .await
    }
    async fn clear_operator_halt(&self, operator: &str) -> Result<()> {
        self.execution_store()
            .set_operator_halt(None, operator)
            .await
    }
    async fn operator_halt(&self) -> Result<Option<String>> {
        self.execution_store().operator_halt().await
    }
    /// Local admission for a new submission, for a gateway to check before
    /// it consumes a one-use approval. Advisory: each submission checks
    /// again inside its own transaction.
    async fn submission_admission(&self) -> Result<()> {
        if self.operator_halt().await?.is_some() {
            return Err(crate::Error::Halted(
                "Operator trading halt is active".into(),
            ));
        }
        Ok(())
    }
}
#[async_trait::async_trait]
impl TradingBackend for SchwabClient {
    fn mode(&self) -> &'static str {
        "live"
    }
    fn execution_store(&self) -> Arc<dyn ExecutionStore> {
        self.execution_store()
    }
    fn coordinator(&self) -> Arc<dyn Coordinator> {
        self.coordinator()
    }
    fn principal_journal_path(&self, digest: &str) -> PathBuf {
        self.auth_manager()
            .config()
            .tokens_file
            .with_extension("orders.json")
            .with_extension(format!("{digest}.audit.json"))
    }
    fn begin_drain(&self) {
        self.begin_drain();
    }
    async fn readiness(&self, accounts: &[String], max_age_ms: i64) -> Result<Readiness> {
        self.readiness(accounts, max_age_ms).await
    }
    async fn place_order_fenced(
        &self,
        account: &str,
        order: &OrderRequest,
        lease: &Lease,
    ) -> Result<OrderSubmitResponse> {
        self.place_order_fenced(account, order, lease).await
    }
    async fn replace_order_fenced(
        &self,
        account: &str,
        id: &str,
        order: &OrderRequest,
        lease: &Lease,
    ) -> Result<OrderSubmitResponse> {
        self.replace_order_fenced(account, id, order, lease).await
    }
    async fn cancel_order(&self, account: &str, id: &str) -> Result<()> {
        self.cancel_order(account, id).await
    }
    async fn submission_admission(&self) -> Result<()> {
        self.submission_admission().await
    }
}
#[async_trait::async_trait]
impl TradingBackend for PaperBroker {
    fn mode(&self) -> &'static str {
        "paper"
    }
    fn execution_store(&self) -> Arc<dyn ExecutionStore> {
        Arc::new(self.order_journal().clone())
    }
    fn coordinator(&self) -> Arc<dyn Coordinator> {
        Arc::new(self.order_journal().database().clone())
    }
    fn principal_journal_path(&self, digest: &str) -> PathBuf {
        self.order_journal()
            .path()
            .with_extension(format!("{digest}.paper-audit.sqlite3"))
    }
    fn begin_drain(&self) {
        self.begin_drain();
    }
    fn paper(&self) -> Option<&PaperBroker> {
        Some(self)
    }
    async fn readiness(&self, accounts: &[String], _max_age_ms: i64) -> Result<Readiness> {
        self.execution_store().healthcheck().await?;
        let mut missing = 0;
        for account in accounts {
            if !self.config().accounts.contains_key(account) {
                missing += 1;
            }
        }
        let report = self.status().await?;
        Ok(Readiness {
            reads_ready: true,
            trading_ready: report
                && missing == 0
                && !accounts.is_empty()
                && !self.is_draining()
                && self.operator_halt().await?.is_none(),
            tokens_valid: false,
            storage_healthy: true,
            pending_submissions: 0,
            unreconciled_accounts: missing,
            oldest_baseline_age_ms: None,
            circuit_open: false,
        })
    }
    async fn place_order_fenced(
        &self,
        account: &str,
        order: &OrderRequest,
        lease: &Lease,
    ) -> Result<OrderSubmitResponse> {
        self.place_order_fenced(account, order, lease).await
    }
    async fn replace_order_fenced(
        &self,
        account: &str,
        id: &str,
        order: &OrderRequest,
        lease: &Lease,
    ) -> Result<OrderSubmitResponse> {
        self.replace_order_fenced(account, id, order, lease).await
    }
    async fn cancel_order(&self, account: &str, id: &str) -> Result<()> {
        self.cancel_order(account, id).await
    }
}
