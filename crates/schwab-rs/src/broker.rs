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
    /// IDs of the orders on `account` that are still working (not filled,
    /// cancelled, rejected, expired or replaced), parents and children.
    async fn working_orders(&self, account: &str) -> Result<Vec<String>>;
    /// The kill switch's second half: cancel every working order on
    /// `account`, one by one, and say how each went. Halting first (so
    /// nothing new is placed meanwhile) is the caller's part.
    async fn cancel_working_orders(&self, account: &str) -> Result<Vec<(String, Result<()>)>> {
        let mut outcomes = Vec::new();
        for id in self.working_orders(account).await? {
            let outcome = self.cancel_order(account, &id).await;
            outcomes.push((id, outcome));
        }
        Ok(outcomes)
    }
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
    /// The account-level risk check a submission of `order` (replacing
    /// `replaces`, if any) would meet, without reserving anything: an
    /// advisory pre-flight. The default (and the paper venue, which judges
    /// its limits at submission) finds nothing to object to.
    async fn check_risk(
        &self,
        _account: &str,
        _order: &OrderRequest,
        _replaces: Option<&str>,
    ) -> Result<()> {
        Ok(())
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
    async fn working_orders(&self, account: &str) -> Result<Vec<String>> {
        let now = chrono::Utc::now();
        let params =
            crate::types::trading::OrdersParams::new(now - chrono::Duration::days(59), now);
        fn collect(order: &crate::types::trading::Order, ids: &mut Vec<String>) {
            if !order.status.is_some_and(|s| s.is_terminal())
                && order.cancelable != Some(false)
                && let Some(id) = &order.order_id
            {
                ids.push(id.clone());
            }
            for child in &order.child_order_strategies {
                collect(child, ids);
            }
        }
        let mut ids = Vec::new();
        for order in self.get_account_orders(account, &params).await? {
            collect(&order, &mut ids);
        }
        Ok(ids)
    }
    async fn submission_admission(&self) -> Result<()> {
        self.submission_admission().await
    }
    async fn check_risk(
        &self,
        account: &str,
        order: &OrderRequest,
        replaces: Option<&str>,
    ) -> Result<()> {
        match self.risk_provider() {
            Some(risk) => risk.check(account, order, replaces).await,
            None => Ok(()),
        }
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
    async fn working_orders(&self, account: &str) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let mut after = 0;
        loop {
            let page = self.orders(account, after, 1000).await?;
            let Some(last) = page.last() else {
                return Ok(ids);
            };
            after = last
                .order_id
                .parse()
                .map_err(|_| crate::Error::Storage("Invalid paper order ID".into()))?;
            ids.extend(
                page.iter()
                    .filter(|order| order.status.is_open())
                    .map(|order| order.order_id.clone()),
            );
        }
    }
}
