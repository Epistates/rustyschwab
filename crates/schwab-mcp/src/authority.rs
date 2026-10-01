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
    /// Reserved for `/v1/operator`, so a halt still gets through when trading
    /// traffic (or unauthenticated noise) holds every general slot.
    operator_capacity: Arc<tokio::sync::Semaphore>,
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
            operator_capacity: Arc::new(tokio::sync::Semaphore::new(4)),
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
            operator_capacity: Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        self.grants().map(|_| ())
    }
    fn grants(&self) -> anyhow::Result<Vec<Grant>> {
        load_grants(&self.policy_file)
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
        // Read per request, so a revoked grant takes effect at once; off
        // the async workers, since the file may be up to a megabyte.
        let path = self.policy_file.clone();
        let grants = tokio::task::spawn_blocking(move || load_grants(&path))
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
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
/// Read and check the grants file: bounded, one grant per subject, and a
/// validated trading policy on every trader and on no one else.
fn load_grants(path: &std::path::Path) -> anyhow::Result<Vec<Grant>> {
    let file = std::fs::File::open(path)?;
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
    let lane = if request.uri().path() == "/v1/operator" {
        // Only a verified operator may hold a reserved permit: anyone else
        // could fill the lane with slow bodies and lock out a halt. The
        // handler authenticates again, against the grants as they then are.
        if let Err(code) = state.authenticate(request.headers(), Role::Operator).await {
            return response(Err(code.into()));
        }
        &state.operator_capacity
    } else {
        &state.capacity
    };
    let Ok(_permit) = lane.clone().try_acquire_owned() else {
        return response(Err(StatusCode::SERVICE_UNAVAILABLE.into()));
    };
    tokio::time::timeout(Duration::from_secs(35), next.run(request))
        .await
        .unwrap_or_else(|_| response(Err(StatusCode::GATEWAY_TIMEOUT.into())))
}
async fn ready(State(state): State<Arc<Authority>>) -> Response {
    match tokio::time::timeout(
        Duration::from_secs(2),
        state.client.readiness(
            &state.required_accounts,
            crate::status::baseline_max_age_ms(),
        ),
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
fn status_for(envelope: &ErrorEnvelope) -> StatusCode {
    match envelope.kind {
        ErrorKind::InvalidParameter => StatusCode::BAD_REQUEST,
        // The venue's own answer to a caller mistake (a paper 404 for an
        // unknown account, a Schwab 400 for an untradable symbol) crosses the
        // gateway as is; its session and server failures are ours to report.
        ErrorKind::Broker => match envelope.status.and_then(|s| StatusCode::from_u16(s).ok()) {
            Some(status)
                if status.is_client_error()
                    && status != StatusCode::UNAUTHORIZED
                    && status != StatusCode::FORBIDDEN =>
            {
                status
            }
            _ => StatusCode::BAD_GATEWAY,
        },
        ErrorKind::ApprovalInvalid => StatusCode::FORBIDDEN,
        ErrorKind::Duplicate
        | ErrorKind::ReconciliationRequired
        | ErrorKind::OwnershipLost
        | ErrorKind::SubmissionUnknown => StatusCode::CONFLICT,
        ErrorKind::RiskDenied | ErrorKind::BaselineStale => StatusCode::UNPROCESSABLE_ENTITY,
        ErrorKind::Halted => StatusCode::LOCKED,
        ErrorKind::RateLimit => StatusCode::TOO_MANY_REQUESTS,
        ErrorKind::Storage
        | ErrorKind::Remote
        | ErrorKind::CircuitOpen
        | ErrorKind::LeaseBusy
        | ErrorKind::Suspended => StatusCode::SERVICE_UNAVAILABLE,
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
            | ErrorKind::Suspended
            | ErrorKind::RiskDenied
            | ErrorKind::BaselineStale
            | ErrorKind::Duplicate
            | ErrorKind::ApprovalInvalid
            | ErrorKind::ReconciliationRequired
            | ErrorKind::SubmissionUnknown
            | ErrorKind::OwnershipLost
            | ErrorKind::LeaseBusy
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
            (status_for(&envelope), Json(envelope)).into_response()
        }
    }
}
/// The least credential lifetime a gateway submission must have left: room
/// for the SDK's own deadline and an answer.
const MIN_DISPATCH_ALLOWANCE: Duration = Duration::from_secs(15);
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
    Json(mut request): Json<GatewayRequest>,
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
            if !cancel {
                use schwab_rs::execution::ExecutionStore;
                // An earlier dispatch interrupted by a process exit would
                // block this one (and may hold its approval): settle it
                // first, from the venue's journal, where that decides it.
                let venue = state.client.execution_store();
                for record in audit
                    .unresolved_records(&principal.name)
                    .await
                    .unwrap_or_default()
                {
                    if let Err(e) =
                        crate::journal::settle_stranded(&audit, venue.as_ref(), &record.attempt_id)
                            .await
                    {
                        tracing::warn!(attempt = %record.attempt_id, error = %e, "could not settle a stranded dispatch");
                    }
                }
            }
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
            // As on the MCP path: an untagged order is tagged with its intent
            // digest, so resubmitting the identical order under a fresh
            // request_id is a duplicate at the SDK's tag check, not a second
            // order. The digest (what operators approve) is of the untagged form.
            let args = if cancel {
                args
            } else {
                crate::policy::tag_intent(args, &digest)
            };
            if let GatewayAction::Place { order } | GatewayAction::Replace { order, .. } =
                &mut request.action
                && order.tag.is_none()
            {
                order.tag = Some(crate::policy::intent_tag(&digest));
            }
            let consumes_approval = !cancel && principal.require_approval;
            let key = if consumes_approval {
                // As on the MCP path: refuse a halted or suspended submission
                // before the audit consumes its one-use approval.
                state.client.submission_admission().await?;
                digest
            } else {
                format!("gateway:{}:{}", principal.name, request.request_id)
            };
            let scope = if cancel {
                format!("{}:cancel:{}", principal.name, request.request_id)
            } else {
                principal.name.clone()
            };
            // A submission that cannot finish inside the caller's
            // credential is refused before it is audited, not abandoned
            // after.
            if allowance(&identity) < MIN_DISPATCH_ALLOWANCE {
                return Err(StatusCode::UNAUTHORIZED.into());
            }
            let attempt = audit
                .begin_once(
                    &scope,
                    tool,
                    json!({"request_id":request.request_id,"arguments":args}),
                    Some(key),
                )
                .await?;
            // From the audit record on, the dispatch runs on its own task:
            // a caller that disconnects or times out abandons only the
            // answer, and the record still reaches its outcome.
            let client = state.client.clone();
            let account = request.account_hash.clone();
            let lease = request.lease.clone();
            let execute = async move {
                match request.action {
                    GatewayAction::Place { order } => client
                        .place_order_fenced(&account, &order, lease.as_ref().unwrap())
                        .await
                        .map(Some),
                    GatewayAction::Replace { order_id, order } => client
                        .replace_order_fenced(&account, &order_id, &order, lease.as_ref().unwrap())
                        .await
                        .map(Some),
                    GatewayAction::Cancel { order_id } => client
                        .cancel_order(&account, &order_id)
                        .await
                        .map(|_| None),
                }
            };
            let mode = state.client.mode();
            let finish = {
                let audit = audit.clone();
                let attempt = attempt.clone();
                async move {
                    let result = execute.await;
                    match &result {
                        Ok(result) => {
                            let outcome = SubmissionOutcome::Accepted {
                                order_id: result.as_ref().and_then(|r| r.order_id.clone()),
                            };
                            let recorded = audit.finish(&attempt, outcome).await.is_ok();
                            (Ok(result.clone()), recorded)
                        }
                        Err(error)
                            if !matches!(error, schwab_rs::Error::SubmissionUnknown { .. })
                                && !(cancel && error.is_retryable()) =>
                        {
                            let recorded = audit
                                .finish(&attempt, SubmissionOutcome::Rejected)
                                .await
                                .is_ok();
                            // Refused before anything reached the broker: as
                            // on the MCP path, the approval is not spent.
                            if recorded
                                && consumes_approval
                                && crate::contract::refusal_keeps_approval(error.kind())
                                && let Err(e) = audit.refund_approval(&attempt).await
                            {
                                tracing::warn!(%attempt, error = %e, "approval refund failed; it stays spent");
                            }
                            (result, recorded)
                        }
                        Err(_) => (result, false),
                    }
                }
            };
            let task = tokio::spawn(finish);
            match tokio::time::timeout(allowance(&identity), task).await {
                Ok(Ok((Ok(result), true))) => {
                    Ok(json!({"mode":mode,"status":"accepted","attempt_id":attempt,"result":result}))
                }
                Ok(Ok((Err(error), true))) => {
                    Ok(json!({"mode":mode,"status":"rejected","attempt_id":attempt,"error":envelope(&error)}))
                }
                // Ambiguous, unrecorded, or still running past the caller's
                // credential: the record settles on its own task.
                _ => Ok(json!({"mode":mode,"status":"unknown","attempt_id":attempt})),
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
            if !policy.grants_account(&query.account) {
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
