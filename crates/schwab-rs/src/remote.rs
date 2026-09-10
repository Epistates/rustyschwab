//! Authenticated remote backends sharing one transactional authority.
//! Use the broker gateway when workers must not possess broker credentials.
use crate::{
    coordination::{Coordinator, Lease},
    error::{Error, Result},
    execution::{AuditEvent, ExecutionStore, SubmissionOutcome, SubmissionRecord},
    reconciliation::{AccountState, ReconciliationReport},
    risk::RiskProvider,
    types::trading::OrderRequest,
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "operation",
    content = "arguments",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum BackendRequest {
    RevalidateFenced {
        account: String,
        attempt_id: String,
        order: OrderRequest,
        lease: Lease,
    },
    Begin {
        account: String,
        operation: String,
        request: Value,
        dedup_key: Option<String>,
    },
    Finish {
        id: String,
        outcome: SubmissionOutcome,
    },
    Records,
    Invalidate {
        account: String,
        generation: u64,
    },
    Account {
        account: String,
    },
    Reconcile {
        account: String,
        generation: u64,
        started_at: i64,
        snapshot: Value,
        orders: Vec<Value>,
    },
    Audit {
        after: i64,
        limit: u32,
    },
    Health,
    Pending,
    Targets {
        account: String,
    },
    Track {
        account: String,
        order_id: String,
    },
    Halt,
    SetHalt {
        reason: Option<String>,
        operator: String,
    },
    Quota {
        key: String,
        rps: u32,
        burst: u32,
    },
    Acquire {
        resource: String,
        owner: String,
        ttl: Duration,
    },
    Renew {
        lease: Lease,
        ttl: Duration,
    },
    ReleaseLease {
        lease: Lease,
    },
    Valid {
        lease: Lease,
    },
    Reserve {
        account: String,
        attempt_id: String,
        order: OrderRequest,
    },
    Revalidate {
        account: String,
        attempt_id: String,
        order: OrderRequest,
    },
    ReleaseRisk {
        attempt_id: String,
    },
}
impl BackendRequest {
    /// Low-level state mutation requires a trusted backend-service identity.
    /// These operations are never exposed to ordinary trading principals.
    pub async fn execute(
        self,
        execution: &dyn ExecutionStore,
        coordinator: &dyn Coordinator,
        risk: &dyn RiskProvider,
    ) -> Result<Value> {
        let value = match self {
            Self::RevalidateFenced {
                account,
                attempt_id,
                order,
                lease,
            } => serde_json::to_value(
                risk.revalidate_fenced(&account, &attempt_id, &order, &lease)
                    .await?,
            )?,
            Self::Begin {
                account,
                operation,
                request,
                dedup_key,
            } => serde_json::to_value(
                execution
                    .begin_once(&account, &operation, request, dedup_key)
                    .await?,
            )?,
            Self::Finish { id, outcome } => {
                serde_json::to_value(execution.finish(&id, outcome).await?)?
            }
            Self::Records => serde_json::to_value(execution.records().await?)?,
            Self::Invalidate {
                account,
                generation,
            } => serde_json::to_value(execution.invalidate_account(&account, generation).await?)?,
            Self::Account { account } => {
                serde_json::to_value(execution.account_state(&account).await?)?
            }
            Self::Reconcile {
                account,
                generation,
                started_at,
                snapshot,
                orders,
            } => serde_json::to_value(
                execution
                    .reconcile_snapshot(&account, generation, started_at, snapshot, orders)
                    .await?,
            )?,
            Self::Audit { after, limit } => {
                serde_json::to_value(execution.audit_page(after, limit).await?)?
            }
            Self::Health => serde_json::to_value(execution.healthcheck().await?)?,
            Self::Pending => serde_json::to_value(execution.pending_count().await?)?,
            Self::Targets { account } => {
                serde_json::to_value(execution.reconciliation_targets(&account).await?)?
            }
            Self::Track { account, order_id } => {
                serde_json::to_value(execution.track_order(&account, &order_id).await?)?
            }
            Self::Halt => serde_json::to_value(execution.operator_halt().await?)?,
            Self::SetHalt { reason, operator } => {
                serde_json::to_value(execution.set_operator_halt(reason, &operator).await?)?
            }
            Self::Quota { key, rps, burst } => {
                serde_json::to_value(coordinator.quota(&key, rps, burst).await?)?
            }
            Self::Acquire {
                resource,
                owner,
                ttl,
            } => serde_json::to_value(coordinator.acquire(&resource, &owner, ttl).await?)?,
            Self::Renew { lease, ttl } => {
                serde_json::to_value(coordinator.renew(&lease, ttl).await?)?
            }
            Self::ReleaseLease { lease } => {
                serde_json::to_value(coordinator.release(&lease).await?)?
            }
            Self::Valid { lease } => serde_json::to_value(coordinator.valid(&lease).await?)?,
            Self::Reserve {
                account,
                attempt_id,
                order,
            } => serde_json::to_value(risk.reserve(&account, &attempt_id, &order).await?)?,
            Self::Revalidate {
                account,
                attempt_id,
                order,
            } => serde_json::to_value(risk.revalidate(&account, &attempt_id, &order).await?)?,
            Self::ReleaseRisk { attempt_id } => {
                serde_json::to_value(risk.release(&attempt_id).await?)?
            }
        };
        Ok(value)
    }
}
#[derive(Clone)]
pub struct RemoteBackend {
    endpoint: reqwest::Url,
    http: reqwest::Client,
    token: Arc<parking_lot::RwLock<SecretString>>,
}
impl std::fmt::Debug for RemoteBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RemoteBackend { credentials: [REDACTED] }")
    }
}
impl RemoteBackend {
    /// Endpoint is the full /v1/backend URL. HTTP is allowed only on loopback.
    pub fn new(endpoint: &str, token: SecretString) -> Result<Self> {
        let endpoint = secure_endpoint(endpoint)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            endpoint,
            http,
            token: Arc::new(parking_lot::RwLock::new(token)),
        })
    }
    /// Rotate short-lived service credentials without rebuilding callers.
    pub fn set_access_token(&self, token: SecretString) {
        *self.token.write() = token;
    }
    pub async fn call<T: DeserializeOwned>(&self, request: &BackendRequest) -> Result<T> {
        self.post(request).await
    }
    pub async fn post<T: DeserializeOwned>(&self, request: &impl Serialize) -> Result<T> {
        let token = self.token.read().clone();
        let mut authorization =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.expose_secret()))
                .map_err(|_| Error::Config("Invalid service credential".into()))?;
        authorization.set_sensitive(true);
        let body = serde_json::to_vec(request)?;
        if body.len() > 8 * 1024 * 1024 {
            return Err(Error::InvalidParameter(
                "Backend request exceeds limit".into(),
            ));
        }
        // Mutations are never retried. A lost reply requires reading durable state.
        let response = self
            .http
            .post(self.endpoint.clone())
            .header(reqwest::header::AUTHORIZATION, authorization)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Error::Storage(format!(
                "Remote authority rejected request (HTTP {})",
                response.status().as_u16()
            )));
        }
        let bytes = bounded_body(response, 8 * 1024 * 1024).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}
pub fn secure_endpoint(endpoint: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(endpoint)
        .map_err(|_| Error::Config("Invalid service endpoint".into()))?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1"));
    if (url.scheme() != "https" && !(url.scheme() == "http" && local))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Config("Service endpoint requires HTTPS, no userinfo/query/fragment; HTTP loopback is allowed for tests".into()));
    }
    Ok(url)
}
pub async fn bounded_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > limit {
            return Err(Error::Storage("Service response exceeds limit".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
#[async_trait::async_trait]
impl ExecutionStore for RemoteBackend {
    async fn begin_once(
        &self,
        account: &str,
        operation: &str,
        request: Value,
        dedup_key: Option<String>,
    ) -> Result<String> {
        self.call(&BackendRequest::Begin {
            account: account.to_owned(),
            operation: operation.to_owned(),
            request,
            dedup_key,
        })
        .await
    }
    async fn finish(&self, id: &str, outcome: SubmissionOutcome) -> Result<()> {
        self.call(&BackendRequest::Finish {
            id: id.to_owned(),
            outcome,
        })
        .await
    }
    async fn records(&self) -> Result<Vec<SubmissionRecord>> {
        self.call(&BackendRequest::Records).await
    }
    async fn invalidate_account(&self, account: &str, generation: u64) -> Result<()> {
        self.call(&BackendRequest::Invalidate {
            account: account.to_owned(),
            generation,
        })
        .await
    }
    async fn account_state(&self, account: &str) -> Result<Option<AccountState>> {
        self.call(&BackendRequest::Account {
            account: account.to_owned(),
        })
        .await
    }
    async fn reconcile_snapshot(
        &self,
        account: &str,
        generation: u64,
        started_at: i64,
        snapshot: Value,
        orders: Vec<Value>,
    ) -> Result<ReconciliationReport> {
        self.call(&BackendRequest::Reconcile {
            account: account.to_owned(),
            generation,
            started_at,
            snapshot,
            orders,
        })
        .await
    }
    async fn audit_page(&self, after: i64, limit: u32) -> Result<Vec<AuditEvent>> {
        self.call(&BackendRequest::Audit { after, limit }).await
    }
    async fn healthcheck(&self) -> Result<()> {
        self.call(&BackendRequest::Health).await
    }
    async fn pending_count(&self) -> Result<usize> {
        self.call(&BackendRequest::Pending).await
    }
    async fn reconciliation_targets(&self, account: &str) -> Result<Vec<String>> {
        self.call(&BackendRequest::Targets {
            account: account.to_owned(),
        })
        .await
    }
    async fn track_order(&self, account: &str, order_id: &str) -> Result<()> {
        self.call(&BackendRequest::Track {
            account: account.to_owned(),
            order_id: order_id.to_owned(),
        })
        .await
    }
    async fn operator_halt(&self) -> Result<Option<String>> {
        self.call(&BackendRequest::Halt).await
    }
    async fn set_operator_halt(&self, reason: Option<String>, operator: &str) -> Result<()> {
        self.call(&BackendRequest::SetHalt {
            reason,
            operator: operator.to_owned(),
        })
        .await
    }
}
#[async_trait::async_trait]
impl Coordinator for RemoteBackend {
    async fn quota(&self, key: &str, rps: u32, burst: u32) -> Result<Duration> {
        self.call(&BackendRequest::Quota {
            key: key.to_owned(),
            rps,
            burst,
        })
        .await
    }
    async fn acquire(&self, resource: &str, owner: &str, ttl: Duration) -> Result<Lease> {
        self.call(&BackendRequest::Acquire {
            resource: resource.to_owned(),
            owner: owner.to_owned(),
            ttl,
        })
        .await
    }
    async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<Lease> {
        self.call(&BackendRequest::Renew {
            lease: lease.clone(),
            ttl,
        })
        .await
    }
    async fn release(&self, lease: &Lease) -> Result<()> {
        self.call(&BackendRequest::ReleaseLease {
            lease: lease.clone(),
        })
        .await
    }
    async fn valid(&self, lease: &Lease) -> Result<bool> {
        self.call(&BackendRequest::Valid {
            lease: lease.clone(),
        })
        .await
    }
}
#[async_trait::async_trait]
impl RiskProvider for RemoteBackend {
    async fn revalidate_fenced(
        &self,
        account: &str,
        attempt_id: &str,
        order: &OrderRequest,
        lease: &Lease,
    ) -> Result<()> {
        self.call(&BackendRequest::RevalidateFenced {
            account: account.into(),
            attempt_id: attempt_id.into(),
            order: order.clone(),
            lease: lease.clone(),
        })
        .await
    }
    async fn reserve(&self, account: &str, attempt_id: &str, order: &OrderRequest) -> Result<()> {
        self.call(&BackendRequest::Reserve {
            account: account.to_owned(),
            attempt_id: attempt_id.to_owned(),
            order: order.clone(),
        })
        .await
    }
    async fn revalidate(
        &self,
        account: &str,
        attempt_id: &str,
        order: &OrderRequest,
    ) -> Result<()> {
        self.call(&BackendRequest::Revalidate {
            account: account.to_owned(),
            attempt_id: attempt_id.to_owned(),
            order: order.clone(),
        })
        .await
    }
    async fn release(&self, attempt_id: &str) -> Result<()> {
        self.call(&BackendRequest::ReleaseRisk {
            attempt_id: attempt_id.to_owned(),
        })
        .await
    }
}
