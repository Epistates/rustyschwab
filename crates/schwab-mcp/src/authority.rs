//! Shared transactional authority and broker gateway. Bind behind a TLS proxy;
//! backend-service identities are privileged and separate from trading identities.
use crate::{
    identity::JwtVerifier,
    policy::{PrincipalPolicy, digest, intent_digest},
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use schwab_rs::{
    ErrorEnvelope, ErrorKind, SchwabClient,
    broker::TradingBackend,
    coordination::Lease,
    execution::{OrderJournal, SubmissionOutcome},
    paper::{PaperBroker, PaperEvent, PaperEventKind, PaperEventSource},
    remote::BackendRequest,
    risk::RiskProvider,
    types::trading::OrderRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};

#[derive(Clone)]
pub struct ServiceIdentity {
    pub subject: String,
    pub expires_at: u64,
}
#[async_trait::async_trait]
pub trait IdentityVerifier: Send + Sync {
    async fn verify(&self, token: &str) -> anyhow::Result<ServiceIdentity>;
}
#[async_trait::async_trait]
impl IdentityVerifier for JwtVerifier {
    async fn verify(&self, token: &str) -> anyhow::Result<ServiceIdentity> {
        let c = JwtVerifier::verify(self, token).await?;
        Ok(ServiceIdentity {
            subject: c.sub,
            expires_at: c.exp,
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Backend,
    Operator,
    Trader,
    PaperMarketData,
    PaperCalendar,
    PaperCorporateActions,
    PaperFault,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub subject: String,
    pub role: Role,
    #[serde(default)]
    pub policy: Option<PrincipalPolicy>,
}
/// Re-read the policy for each request so removal applies without process restart.
pub struct Authority {
    pub client: Arc<dyn TradingBackend>,
    pub risk: Option<Arc<dyn RiskProvider>>,
    pub identity: Arc<dyn IdentityVerifier>,
    pub policy_file: PathBuf,
    capacity: Arc<tokio::sync::Semaphore>,
    pub required_accounts: Vec<String>,
}
impl Authority {
    pub fn new(
        client: Arc<SchwabClient>,
        risk: Arc<dyn RiskProvider>,
        identity: Arc<dyn IdentityVerifier>,
        policy_file: PathBuf,
        required_accounts: Vec<String>,
    ) -> Self {
        Self {
            client,
            risk: Some(risk),
            identity,
            policy_file,
            required_accounts,
            capacity: Arc::new(tokio::sync::Semaphore::new(16)),
        }
    }
    /// Paper execution uses the same verified identity, policy, approval and lease routes.
    pub fn new_paper(
        client: Arc<PaperBroker>,
        identity: Arc<dyn IdentityVerifier>,
        policy_file: PathBuf,
    ) -> Self {
        let required_accounts = client.config().accounts.keys().cloned().collect();
        Self {
            client,
            risk: None,
            identity,
            policy_file,
            required_accounts,
            capacity: Arc::new(tokio::sync::Semaphore::new(16)),
        }
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        self.grants().map(|_| ())
    }
    fn grants(&self) -> anyhow::Result<Vec<Grant>> {
        let file = std::fs::File::open(&self.policy_file)?;
        use std::io::Read;
        let mut bytes = Vec::new();
        file.take(1048577).read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() <= 1048576, "Policy exceeds limit");
        let grants: Vec<Grant> = serde_json::from_slice(&bytes)?;
        let mut subjects = std::collections::HashSet::new();
        let mut names = std::collections::HashSet::new();
        for grant in &grants {
            anyhow::ensure!(
                !grant.subject.trim().is_empty() && subjects.insert(&grant.subject),
                "Duplicate/empty subject"
            );
            if grant.role == Role::Trader {
                let policy = grant
                    .policy
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Trader policy required"))?;
                policy.validate()?;
                anyhow::ensure!(
                    policy.identity_subject.as_deref() == Some(&grant.subject)
                        && names.insert(&policy.name),
                    "Trader subject/name mismatch"
                );
            } else {
                anyhow::ensure!(grant.policy.is_none(), "Only traders have trading policies");
            }
        }
        Ok(grants)
    }
    async fn authenticate(
        &self,
        headers: &HeaderMap,
        role: Role,
    ) -> Result<(ServiceIdentity, Vec<Grant>), StatusCode> {
        let token = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .filter(|s| (32..=16384).contains(&s.len()))
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let identity = self
            .identity
            .verify(token)
            .await
            .map_err(|_| StatusCode::UNAUTHORIZED)?;
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        if identity.expires_at <= now {
            return Err(StatusCode::UNAUTHORIZED);
        }
        let grants = self.grants().map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        if !grants
            .iter()
            .any(|g| g.subject == identity.subject && g.role == role)
        {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok((identity, grants))
    }
    fn journal(&self, principal: &str) -> OrderJournal {
        OrderJournal::new(
            self.client
                .principal_journal_path(&digest(principal.as_bytes())),
        )
    }
}
pub fn router(state: Arc<Authority>) -> Router {
    Router::new()
        .route("/livez", get(|| async { Json(json!({"live":true})) }))
        .route("/readyz", get(ready))
        .route("/v1/backend", post(backend))
        .route("/v1/operator", post(operator))
        .route("/v1/lease", post(lease))
        .route("/v1/orders", post(order))
        .route("/v1/paper/events", post(paper_event))
        .route("/v1/paper/account", post(paper_account))
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .layer(axum::middleware::from_fn_with_state(state.clone(), admit))
        .with_state(state)
}
async fn admit(
    State(state): State<Arc<Authority>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Ok(_permit) = state.capacity.clone().try_acquire_owned() else {
        return response(Err(StatusCode::SERVICE_UNAVAILABLE.into()));
    };
    tokio::time::timeout(Duration::from_secs(35), next.run(request))
        .await
        .unwrap_or_else(|_| response(Err(StatusCode::GATEWAY_TIMEOUT.into())))
}
async fn ready(State(state): State<Arc<Authority>>) -> Response {
    match tokio::time::timeout(
        Duration::from_secs(2),
        state.client.readiness(&state.required_accounts, 5000),
    )
    .await
    {
        Ok(Ok(report)) => (
            if report.trading_ready {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            },
            Json({
                let mut value = json!(report);
                value["mode"] = json!(state.client.mode());
                value
            }),
        )
            .into_response(),
        _ => response(Err(StatusCode::SERVICE_UNAVAILABLE.into())),
    }
}
/// A refused request. Protocol refusals (authentication, capacity, limits)
/// carry only a status; SDK failures cross the wire as an [`ErrorEnvelope`]
/// so a `RemoteBackend` recovers the typed error.
enum Denial {
    Status(StatusCode),
    Error(schwab_rs::Error),
}
impl From<StatusCode> for Denial {
    fn from(code: StatusCode) -> Self {
        Self::Status(code)
    }
}
impl From<schwab_rs::Error> for Denial {
    fn from(error: schwab_rs::Error) -> Self {
        Self::Error(error)
    }
}
const OPAQUE_DENIAL: &str = "authority request denied or incomplete";
const OPAQUE_FAILURE: &str = "authority-side failure; consult the authority log";
fn status_for(kind: ErrorKind) -> StatusCode {
    match kind {
        ErrorKind::InvalidParameter => StatusCode::BAD_REQUEST,
        ErrorKind::ApprovalInvalid => StatusCode::FORBIDDEN,
        ErrorKind::Duplicate
        | ErrorKind::ReconciliationRequired
        | ErrorKind::OwnershipLost
        | ErrorKind::SubmissionUnknown => StatusCode::CONFLICT,
        ErrorKind::RiskDenied | ErrorKind::BaselineStale => StatusCode::UNPROCESSABLE_ENTITY,
        ErrorKind::Halted => StatusCode::LOCKED,
        ErrorKind::RateLimit => StatusCode::TOO_MANY_REQUESTS,
        ErrorKind::Storage | ErrorKind::Remote | ErrorKind::CircuitOpen => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        ErrorKind::Timeout => StatusCode::GATEWAY_TIMEOUT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}
/// Messages the SDK authors for the caller cross the wire verbatim; anything
/// describing the authority's own internals is logged here and replaced.
fn envelope(error: &schwab_rs::Error) -> ErrorEnvelope {
    let mut envelope = ErrorEnvelope::from(error);
    let relays_message = matches!(
        envelope.kind,
        ErrorKind::InvalidParameter
            | ErrorKind::Halted
            | ErrorKind::RiskDenied
            | ErrorKind::BaselineStale
            | ErrorKind::Duplicate
            | ErrorKind::ApprovalInvalid
            | ErrorKind::ReconciliationRequired
            | ErrorKind::SubmissionUnknown
            | ErrorKind::OwnershipLost
            | ErrorKind::CircuitOpen
            | ErrorKind::RateLimit
            | ErrorKind::Timeout
            | ErrorKind::Broker
    );
    if !relays_message {
        tracing::warn!(kind = %envelope.kind, %error, "authority request failed");
        envelope.message = OPAQUE_FAILURE.into();
    }
    envelope
}
fn response(result: Result<Value, Denial>) -> Response {
    match result {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(Denial::Status(code)) => {
            (code, Json(json!({ "error": OPAQUE_DENIAL }))).into_response()
        }
        Err(Denial::Error(error)) => {
            let envelope = envelope(&error);
            (status_for(envelope.kind), Json(envelope)).into_response()
        }
    }
}
fn allowance(identity: &ServiceIdentity) -> Duration {
    Duration::from_secs(
        identity
            .expires_at
            .saturating_sub(chrono::Utc::now().timestamp().max(0) as u64)
            .min(30),
    )
}
async fn backend(
    State(state): State<Arc<Authority>>,
    headers: HeaderMap,
    Json(mut request): Json<BackendRequest>,
) -> Response {
    response(
        async {
            let (identity, _) = state.authenticate(&headers, Role::Backend).await?;
            if let BackendRequest::SetHalt { reason, operator } = &mut request {
                if reason.is_none() {
                    return Err(StatusCode::FORBIDDEN.into());
                }
                *operator = identity.subject.clone();
            }
            let value = tokio::time::timeout(
                allowance(&identity),
                request.execute(
                    state.client.execution_store().as_ref(),
                    state.client.coordinator().as_ref(),
                    state.risk.as_deref().ok_or(StatusCode::FORBIDDEN)?,
                ),
            )
            .await
            .map_err(|_| StatusCode::GATEWAY_TIMEOUT)??;
            if serde_json::to_vec(&value)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .len()
                > 8 * 1024 * 1024
            {
                return Err(StatusCode::PAYLOAD_TOO_LARGE.into());
            }
            Ok(value)
        }
        .await,
    )
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperatorRequest {
    Approve {
        principal: String,
        digest: String,
        expires_at: i64,
    },
    Revoke {
        principal: String,
        digest: String,
    },
    Halt {
        reason: String,
    },
    Resume,
    Resolve {
        principal: String,
        attempt: String,
        evidence: String,
    },
    Status,
}
async fn operator(
    State(state): State<Arc<Authority>>,
    headers: HeaderMap,
    Json(request): Json<OperatorRequest>,
) -> Response {
    response(
        async {
            let _permit = state
                .capacity
                .clone()
                .try_acquire_owned()
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            let (identity, grants) = state.authenticate(&headers, Role::Operator).await?;
            let operation = async {
                // Operators may only act on principals they do not trade as.
                let journal = |name: &str| -> Result<OrderJournal, StatusCode> {
                    let delegated = grants.iter().any(|g| {
                        g.role == Role::Trader
                            && g.subject != identity.subject
                            && g.policy.as_ref().is_some_and(|p| p.name == name)
                    });
                    if !delegated {
                        return Err(StatusCode::FORBIDDEN);
                    }
                    Ok(state.journal(name))
                };
                match request {
                    OperatorRequest::Approve {
                        principal,
                        digest,
                        expires_at,
                    } => {
                        journal(&principal)?
                            .approve(&principal, &digest, &identity.subject, expires_at)
                            .await?
                    }
                    OperatorRequest::Revoke { principal, digest } => {
                        journal(&principal)?
                            .revoke_approval(&digest, &identity.subject)
                            .await?
                    }
                    OperatorRequest::Resolve {
                        principal,
                        attempt,
                        evidence,
                    } => {
                        let outcome = SubmissionOutcome::Reconciled {
                            evidence: format!("Verified operator {}: {evidence}", identity.subject),
                        };
                        journal(&principal)?.finish(&attempt, outcome).await?
                    }
                    OperatorRequest::Halt { reason } => {
                        state
                            .client
                            .halt_submissions(&reason, &identity.subject)
                            .await?
                    }
                    OperatorRequest::Resume => {
                        state.client.clear_operator_halt(&identity.subject).await?
                    }
                    OperatorRequest::Status => {
                        let halted = state.client.operator_halt().await?;
                        let pending = state.client.execution_store().pending_count().await?;
                        return Ok(json!({
                            "operator_halt": halted,
                            "pending_submissions": pending,
                        }));
                    }
                }
                Ok(json!({"ok":true}))
            };
            tokio::time::timeout(allowance(&identity), operation)
                .await
                .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
        }
        .await,
    )
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum LeaseRequest {
    Acquire,
    Renew { lease: Lease },
    Release { lease: Lease },
}
async fn lease(
    State(state): State<Arc<Authority>>,
    headers: HeaderMap,
    Json(request): Json<LeaseRequest>,
) -> Response {
    response(
        async {
            let (identity, _) = state.authenticate(&headers, Role::Trader).await?;
            let coordinator = state.client.coordinator();
            let operation = async {
                let value = match request {
                    LeaseRequest::Acquire => serde_json::to_value(
                        coordinator
                            .acquire("gateway", &identity.subject, Duration::from_secs(30))
                            .await?,
                    ),
                    LeaseRequest::Renew { lease } => {
                        check_owner(&lease, &identity)?;
                        serde_json::to_value(
                            coordinator.renew(&lease, Duration::from_secs(30)).await?,
                        )
                    }
                    LeaseRequest::Release { lease } => {
                        check_owner(&lease, &identity)?;
                        coordinator.release(&lease).await?;
                        Ok(json!({"ok":true}))
                    }
                };
                value.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into())
            };
            tokio::time::timeout(allowance(&identity), operation)
                .await
                .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
        }
        .await,
    )
}
fn check_owner(lease: &Lease, identity: &ServiceIdentity) -> Result<(), StatusCode> {
    if lease.resource != "gateway" || lease.owner != identity.subject {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayRequest {
    pub request_id: String,
    pub account_hash: String,
    pub action: GatewayAction,
    pub lease: Option<Lease>,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayAction {
    Place {
        order: OrderRequest,
    },
    Replace {
        order_id: String,
        order: OrderRequest,
    },
    Cancel {
        order_id: String,
    },
}
async fn order(
    State(state): State<Arc<Authority>>,
    headers: HeaderMap,
    Json(request): Json<GatewayRequest>,
) -> Response {
    response(
        async {
            let (identity, grants) = state.authenticate(&headers, Role::Trader).await?;
            if uuid::Uuid::parse_str(&request.request_id).is_err()
                || request.account_hash.is_empty()
                || !request
                    .account_hash
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(StatusCode::BAD_REQUEST.into());
            }
            let mut principal = grants
                .iter()
                .find(|g| g.subject == identity.subject)
                .and_then(|g| g.policy.clone())
                .ok_or(StatusCode::FORBIDDEN)?;
            let cancel = matches!(request.action, GatewayAction::Cancel { .. });
            let (tool, args) = match &request.action {
                GatewayAction::Place { order } => (
                    "place_order",
                    json!({"account_hash":request.account_hash,"order":order}),
                ),
                GatewayAction::Replace { order_id, order } => (
                    "replace_order",
                    json!({"account_hash":request.account_hash,"order_id":order_id,"order":order}),
                ),
                GatewayAction::Cancel { order_id } => (
                    "cancel_order",
                    json!({"account_hash":request.account_hash,"order_id":order_id}),
                ),
            };
            if let Some(id) = args.get("order_id").and_then(Value::as_str)
                && (id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()))
            {
                return Err(StatusCode::BAD_REQUEST.into());
            }
            if !cancel {
                let held = request.lease.as_ref().ok_or(StatusCode::FORBIDDEN)?;
                check_owner(held, &identity)?;
                if !state.client.coordinator().valid(held).await? {
                    return Err(schwab_rs::Error::OwnershipLost.into());
                }
            }
            let audit = state.journal(&principal.name);
            let digest = intent_digest(tool, &args);
            principal.approved_intents.clear();
            if audit.is_approved(&principal.name, &digest).await? {
                let operator = audit.approval_operator(&digest).await?;
                if grants
                    .iter()
                    .any(|g| g.role == Role::Operator && Some(&g.subject) == operator.as_ref())
                {
                    principal.approved_intents.insert(digest.clone());
                }
            }
            principal
                .authorize(tool, &args)
                .map_err(|_| StatusCode::FORBIDDEN)?;
            let key = if !cancel && principal.require_approval {
                digest
            } else {
                format!("gateway:{}:{}", principal.name, request.request_id)
            };
            let scope = if cancel {
                format!("{}:cancel:{}", principal.name, request.request_id)
            } else {
                principal.name.clone()
            };
            let attempt = audit
                .begin_once(
                    &scope,
                    tool,
                    json!({"request_id":request.request_id,"arguments":args}),
                    Some(key),
                )
                .await?;
            // After durable dispatch intent, all interruption paths remain pending.
            let execute = async {
                match request.action {
                    GatewayAction::Place { order } => state
                        .client
                        .place_order_fenced(
                            &request.account_hash,
                            &order,
                            request.lease.as_ref().unwrap(),
                        )
                        .await
                        .map(Some),
                    GatewayAction::Replace { order_id, order } => state
                        .client
                        .replace_order_fenced(
                            &request.account_hash,
                            &order_id,
                            &order,
                            request.lease.as_ref().unwrap(),
                        )
                        .await
                        .map(Some),
                    GatewayAction::Cancel { order_id } => state
                        .client
                        .cancel_order(&request.account_hash, &order_id)
                        .await
                        .map(|_| None),
                }
            };
            match tokio::time::timeout(allowance(&identity), execute).await {
                Ok(Ok(result)) => {
                    let outcome = SubmissionOutcome::Accepted {
                        order_id: result.as_ref().and_then(|r| r.order_id.clone()),
                    };
                    if audit.finish(&attempt, outcome).await.is_err() {
                        return Ok(json!({"mode":state.client.mode(),"status":"unknown","attempt_id":attempt}));
                    }
                    Ok(json!({"mode":state.client.mode(),"status":"accepted","attempt_id":attempt,"result":result}))
                }
                Ok(Err(error))
                    if !matches!(error, schwab_rs::Error::SubmissionUnknown { .. })
                        && !(cancel && error.is_retryable()) =>
                {
                    audit.finish(&attempt, SubmissionOutcome::Rejected).await?;
                    Ok(json!({"mode":state.client.mode(),"status":"rejected","attempt_id":attempt,"error":envelope(&error)}))
                }
                _ => Ok(json!({"mode":state.client.mode(),"status":"unknown","attempt_id":attempt})),
            }
        }
        .await,
    )
}

// Market ingestion is a privileged operation; trading users cannot invent fills.
async fn paper_event(
    State(state): State<Arc<Authority>>,
    headers: HeaderMap,
    Json(event): Json<PaperEvent>,
) -> Response {
    response(
        async {
            let role = match &event.kind {
                PaperEventKind::Quote { .. }
                | PaperEventKind::Clock
                | PaperEventKind::Disconnect => Role::PaperMarketData,
                PaperEventKind::Session { .. } => Role::PaperCalendar,
                PaperEventKind::Dividend { .. } | PaperEventKind::Split { .. } => {
                    Role::PaperCorporateActions
                }
                PaperEventKind::Fault { .. } => Role::PaperFault,
            };
            let (identity, _) = state.authenticate(&headers, role).await?;
            let paper = state.client.paper().ok_or(StatusCode::NOT_FOUND)?;
            let result = paper
                .apply_from(
                    event,
                    PaperEventSource::Authority {
                        subject: identity.subject,
                    },
                )
                .await?;
            serde_json::to_value(result).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into())
        }
        .await,
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PaperAccountQuery {
    account: String,
    #[serde(default)]
    after_id: u64,
}
async fn paper_account(
    State(state): State<Arc<Authority>>,
    headers: HeaderMap,
    Json(query): Json<PaperAccountQuery>,
) -> Response {
    response(
        async {
            let (identity, grants) = state.authenticate(&headers, Role::Trader).await?;
            let policy = grants
                .iter()
                .find(|g| g.subject == identity.subject)
                .and_then(|g| g.policy.as_ref())
                .ok_or(StatusCode::FORBIDDEN)?;
            if !policy.accounts.contains(&query.account) {
                return Err(StatusCode::FORBIDDEN.into());
            }
            let paper = state.client.paper().ok_or(StatusCode::NOT_FOUND)?;
            let snapshot = paper.snapshot(&query.account).await?;
            let orders = paper.orders(&query.account, query.after_id, 100).await?;
            Ok(json!({"mode":"paper","snapshot":snapshot,"orders":orders}))
        }
        .await,
    )
}
