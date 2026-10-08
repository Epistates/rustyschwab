//! Immutable per-principal authorization. Network authentication selects a
//! principal-specific handler and session store before MCP dispatch.
//!
//! Denials are business outcomes, so they are returned as `isError` tool
//! results in the [`crate::contract`] shape rather than JSON-RPC errors: the
//! model reads `not_permitted`, `risk_denied` or `approval_required` and
//! knows whether to change the order, wait for the operator, or stop.
use crate::SchwabMcp;
use crate::backend::Backend;
use crate::contract::{FailureCode, ToolFailure};
use crate::guidance;
use crate::server::Switches;
use schwab_rs::execution::OrderJournal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};
use turbomcp::prelude::*;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrincipalPolicy {
    pub name: String,
    #[serde(default)]
    pub identity_subject: Option<String>,
    #[serde(default)]
    pub token_expires_at: Option<i64>,
    /// SHA-256 of a high-entropy bearer token (never the token itself).
    #[serde(default)]
    pub token_sha256: String,
    pub tools: HashSet<String>,
    #[serde(default)]
    pub accounts: HashSet<String>,
    #[serde(default)]
    pub all_accounts: bool,
    #[serde(default)]
    pub max_quantity: Option<rust_decimal::Decimal>,
    #[serde(default)]
    pub max_notional: Option<rust_decimal::Decimal>,
    #[serde(default)]
    pub option_multipliers: std::collections::HashMap<String, rust_decimal::Decimal>,
    #[serde(default)]
    pub require_approval: bool,
    #[serde(default)]
    pub approved_intents: HashSet<String>,
    /// Tool calls this principal may make per minute (bursting to a tenth
    /// of it); absent, unlimited. Every principal shares one Schwab request
    /// budget, so this keeps one from crowding out the others.
    #[serde(default)]
    pub max_calls_per_minute: Option<std::num::NonZeroU32>,
}
impl PrincipalPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.name.trim().is_empty(), "Principal name is required");
        anyhow::ensure!(
            (self.token_sha256.len() == 64
                && self.token_sha256.bytes().all(|b| b.is_ascii_hexdigit()))
                || (self.token_sha256.is_empty()
                    && self
                        .identity_subject
                        .as_ref()
                        .is_some_and(|s| !s.trim().is_empty())),
            "token_sha256 must be a SHA-256 hex digest"
        );
        if self.tools.contains("place_order") || self.tools.contains("replace_order") {
            anyhow::ensure!(
                self.max_quantity
                    .is_some_and(|v| v > rust_decimal::Decimal::ZERO)
                    && self
                        .max_notional
                        .is_some_and(|v| v > rust_decimal::Decimal::ZERO),
                "Trading principals require positive finite quantity/notional limits"
            );
        }
        Ok(())
    }
    /// Whether this principal may control the stream (any `stream_*` tool).
    pub fn owns_streaming(&self) -> bool {
        self.tools.iter().any(|t| t.starts_with("stream_"))
    }
    /// Whether this principal may act on `account`.
    pub fn grants_account(&self, account: &str) -> bool {
        self.all_accounts || self.accounts.contains(account)
    }
    /// The first reason this principal may not run `name` with `args`.
    pub fn authorize(&self, name: &str, args: &Value) -> Result<(), ToolFailure> {
        self.review(name, args)
            .denials
            .into_iter()
            .next()
            .map_or(Ok(()), Err)
    }
    /// Every reason this principal may not run `name` with `args`, in the
    /// order a dispatch would find them, plus the order's exposure when it
    /// could be computed. Approval is only judged for a valid order, since
    /// fixing an invalid one changes the intent anyway.
    pub(crate) fn review(&self, name: &str, args: &Value) -> Review {
        let mut review = Review::default();
        if !self.tools.contains(name) {
            review.denials.push(ToolFailure::not_permitted(
                "Operation not authorized for this principal",
            ));
        }
        if let Some(account) = args.get("account_hash").and_then(Value::as_str)
            && !self.grants_account(account)
        {
            review.denials.push(ToolFailure::not_permitted(
                "Account not authorized for this principal",
            ));
        }
        let aggregate = matches!(
            name,
            "get_accounts" | "get_account_numbers" | "get_user_preferences" | "get_all_orders"
        );
        let activity = args.get("service").and_then(Value::as_str) == Some("ACCT_ACTIVITY");
        if !self.all_accounts && (aggregate || activity) {
            review.denials.push(ToolFailure::not_permitted(
                "Operation requires access to all linked accounts",
            ));
        }
        if matches!(name, "place_order" | "replace_order" | "cancel_order")
            && let Err(failure) =
                crate::journal::check_rationale(args.get("rationale").and_then(Value::as_str))
        {
            review.denials.push(failure);
        }
        if matches!(name, "place_order" | "replace_order") {
            let order = match order_request(args) {
                Ok(order) => order,
                Err(failure) => {
                    review.denials.push(failure);
                    return review;
                }
            };
            match exposure(&order, &self.option_multipliers) {
                Ok((quantity, notional)) => {
                    review.exposure = Some((quantity, notional));
                    let max_quantity = self.max_quantity.unwrap_or(rust_decimal::Decimal::ZERO);
                    let max_notional = self.max_notional.unwrap_or(rust_decimal::Decimal::ZERO);
                    if quantity > max_quantity || notional > max_notional {
                        review.denials.push(ToolFailure::new(
                            FailureCode::RiskDenied,
                            format!(
                                "Order exposure {quantity} units / {notional} notional exceeds \
                                 the principal's limits of {max_quantity} / {max_notional}"
                            ),
                        ));
                    }
                }
                Err(reason) => review.denials.push(ToolFailure::invalid_arguments(reason)),
            }
            let digest = intent_digest(name, args);
            if self.require_approval && !self.approved_intents.contains(&digest) {
                review.denials.push(
                    ToolFailure::new(
                        FailureCode::ApprovalRequired,
                        format!(
                            "Order requires operator approval of intent {digest}. \
                             `wait_ms` holds the submission for up to \
                             {MAX_APPROVAL_WAIT_MS} ms while an operator grants it; \
                             nothing is submitted if none arrives."
                        ),
                    )
                    .with_intent(digest),
                );
            }
        }
        review
    }
}

/// The first top-level argument `tool`'s input schema does not declare.
fn undeclared_argument(tool: &turbomcp::Tool, args: &Value) -> Option<String> {
    let schema = serde_json::to_value(&tool.input_schema).ok()?;
    let declared = schema.get("properties")?.as_object()?;
    args.as_object()?
        .keys()
        .find(|key| !declared.contains_key(*key))
        .cloned()
}

/// Outcome of [`PrincipalPolicy::review`].
#[derive(Default)]
pub(crate) struct Review {
    pub denials: Vec<ToolFailure>,
    /// Bounded (quantity, notional) of the order, when it was valid and priced.
    pub exposure: Option<(rust_decimal::Decimal, rust_decimal::Decimal)>,
}

/// The strict, validated order in `args`, or the `invalid_arguments` a
/// submission would return.
pub(crate) fn order_request(
    args: &Value,
) -> Result<schwab_types::trading::OrderRequest, ToolFailure> {
    let order: schwab_types::trading::OrderRequest =
        serde_json::from_value(args.get("order").cloned().unwrap_or(Value::Null))
            .map_err(|e| ToolFailure::invalid_arguments(format!("Invalid order request: {e}")))?;
    order.validate().map_err(ToolFailure::from)?;
    Ok(order)
}
/// Validate a principal set as a whole, at startup and on every reload: each
/// policy is valid on its own, names, static token digests and identity
/// subjects are unique, and at most one principal owns streaming for the
/// Schwab login. With `static_credentials` (network transport and no token
/// issuer) every principal must also carry a short-lived static token.
pub fn validate_policies(
    policies: &[PrincipalPolicy],
    static_credentials: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(!policies.is_empty(), "At least one principal is required");
    let mut names = HashSet::new();
    let mut digests = HashSet::new();
    let mut subjects = HashSet::new();
    for policy in policies {
        policy.validate()?;
        anyhow::ensure!(
            names.insert(policy.name.as_str())
                && (policy.token_sha256.is_empty()
                    || digests.insert(policy.token_sha256.to_ascii_lowercase()))
                && policy
                    .identity_subject
                    .as_deref()
                    .is_none_or(|subject| subjects.insert(subject)),
            "Principal names, identity subjects and token digests must be unique"
        );
        anyhow::ensure!(
            !static_credentials
                || (policy.token_expires_at.is_some() && !policy.token_sha256.is_empty()),
            "Static network credentials require token_expires_at; \
             configure short-lived issuer tokens with SCHWAB_MCP_IDENTITY_FILE"
        );
    }
    anyhow::ensure!(
        policies.iter().filter(|p| p.owns_streaming()).count() <= 1,
        "Only one principal may own streaming for a Schwab login; \
         share market data through a separate service"
    );
    Ok(())
}
// Conservative gross exposure sums all legs and child strategies, including
// mutually exclusive children. Network writes require bounded-price orders;
// market, trailing-market, and naked stop orders have no finite execution cap.
// The one exception is a bracket exit: a child that only closes the long
// position its bounded parent opens (same instrument, no more than the
// parent's quantity) cannot fill for more than the parent paid, so an
// unbounded stop there is priced at the parent's limit.
pub(crate) fn exposure(
    order: &schwab_types::trading::OrderRequest,
    multipliers: &std::collections::HashMap<String, rust_decimal::Decimal>,
) -> Result<(rust_decimal::Decimal, rust_decimal::Decimal), String> {
    exposure_under(order, multipliers, None)
}

/// `entry` is the bounded long entry this order hangs under as a triggered
/// child, if any. OCO wrappers carry no legs and pass it through.
fn exposure_under(
    order: &schwab_types::trading::OrderRequest,
    multipliers: &std::collections::HashMap<String, rust_decimal::Decimal>,
    entry: Option<(
        &schwab_types::trading::OrderRequestLeg,
        rust_decimal::Decimal,
    )>,
) -> Result<(rust_decimal::Decimal, rust_decimal::Decimal), String> {
    use schwab_types::trading::{OrderStrategyType, OrderType};
    let mut quantity = rust_decimal::Decimal::ZERO;
    let mut notional = rust_decimal::Decimal::ZERO;
    let mut child_entry = entry;
    if order.order_strategy_type != OrderStrategyType::Oco {
        use schwab_types::{common::AssetType, trading::OrderInstruction};
        let [leg] = order.order_leg_collection.as_slice() else {
            return Err("Multi-leg exposure requires an external portfolio risk engine".into());
        };
        if matches!(
            leg.instruction,
            OrderInstruction::SellShort
                | OrderInstruction::SellShortExempt
                | OrderInstruction::SellToOpen
        ) {
            return Err("Opening short exposure requires an external portfolio risk engine".into());
        }
        let multiplier = match leg.instrument.asset_type {
            AssetType::Equity => rust_decimal::Decimal::ONE,
            AssetType::Option => *multipliers
                .get(&leg.instrument.symbol)
                .filter(|v| **v > rust_decimal::Decimal::ZERO)
                .ok_or("Option contract requires an operator-verified multiplier")?,
            _ => {
                return Err(
                    "Risk policy supports standard equity and option contracts only".into(),
                );
            }
        };
        let bounded = matches!(
            order.order_type,
            Some(
                OrderType::Limit
                    | OrderType::StopLimit
                    | OrderType::LimitOnClose
                    | OrderType::NetDebit
                    | OrderType::NetCredit
            )
        );
        let price = match (bounded, order.price) {
            (true, Some(price)) => price,
            _ => entry
                .filter(|(opened, _)| closes(leg, opened))
                .map(|(_, price)| price)
                .ok_or("Policy requires an order with a bounded limit price")?,
        };
        quantity = quantity
            .checked_add(leg.quantity)
            .ok_or("Quantity overflow")?;
        let exposure = leg
            .quantity
            .checked_mul(multiplier)
            .and_then(|v| v.checked_mul(price))
            .ok_or("Notional overflow")?;
        notional = notional.checked_add(exposure).ok_or("Notional overflow")?;
        child_entry =
            (bounded && leg.instruction.opens_long() == Some(true)).then_some((leg, price));
    }
    for child in &order.child_order_strategies {
        let (q, n) = exposure_under(child, multipliers, child_entry)?;
        quantity = quantity.checked_add(q).ok_or("Quantity overflow")?;
        notional = notional.checked_add(n).ok_or("Notional overflow")?;
    }

    Ok((quantity, notional))
}

/// Whether `leg` can only flatten what `entry` opened: the paired closing
/// instruction, the same instrument, and no more than the entry's quantity.
fn closes(
    leg: &schwab_types::trading::OrderRequestLeg,
    entry: &schwab_types::trading::OrderRequestLeg,
) -> bool {
    entry.instruction.closing() == Some(leg.instruction)
        && leg.instrument.symbol == entry.instrument.symbol
        && leg.instrument.asset_type == entry.instrument.asset_type
        && leg.quantity <= entry.quantity
}
pub fn digest(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
/// SHA-256 over the tool name and canonically sorted arguments. An `order`
/// argument that parses as a strict `OrderRequest` is digested in its wire
/// form, so an approval binds the order Schwab would receive rather than one
/// JSON spelling of it (`"10"` and `10` agree; an order with unknown fields
/// is digested as written and fails validation later). A `rationale` is
/// journaled, not digested: an approval binds the order, not the words,
/// and re-wording an identical order must still be a duplicate intent.
/// `wait_ms` is not digested either: it says how long the caller will hold
/// for the operator, not what the operator is approving, so the digest
/// `check_order` reports is the one a waiting `place_order` submits.
pub fn intent_digest(name: &str, args: &Value) -> String {
    let mut args = args.clone();
    if let Some(args) = args.as_object_mut() {
        args.remove("rationale");
        args.remove("wait_ms");
    }
    if let Some(order) = args.get("order")
        && let Ok(order) =
            serde_json::from_value::<schwab_types::trading::OrderRequest>(order.clone())
        && let Ok(wire) = serde_json::to_value(order)
    {
        args["order"] = wire;
    }
    digest(
        serde_json::to_vec(&(name, schwab_rs::utils::canonical_json(&args)))
            .unwrap()
            .as_slice(),
    )
}

/// The longest a submission may hold for an operator's approval.
///
/// Under the 60-second request timeout common MCP clients default to, so
/// the submission the wait was for still reaches the venue and answers
/// inside the same call. A client that gives up anyway abandons only the
/// answer: once audited, a dispatch runs to its outcome.
pub const MAX_APPROVAL_WAIT_MS: u64 = 45_000;

/// How often a holding submission re-reads the approval store.
const APPROVAL_POLL: Duration = Duration::from_secs(1);

/// The bounded hold `args` asks for, if any.
pub(crate) fn approval_wait(args: &Value) -> Result<Option<Duration>, ToolFailure> {
    match args.get("wait_ms") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(0) => Ok(None),
            Some(ms) if ms <= MAX_APPROVAL_WAIT_MS => Ok(Some(Duration::from_millis(ms))),
            _ => Err(ToolFailure::invalid_arguments(format!(
                "wait_ms is at most {MAX_APPROVAL_WAIT_MS}; call again to keep waiting"
            ))),
        },
    }
}

/// Prefix of the order tag derived from an intent digest.
pub const INTENT_TAG_PREFIX: &str = "mcp-";

/// The order tag a submission of the intent with this digest carries when
/// the model set none. Paper mode accepts at most 128 characters of
/// `[A-Za-z0-9_-]`, which bounds the digest prefix.
pub fn intent_tag(digest: &str) -> String {
    format!("{INTENT_TAG_PREFIX}{}", &digest[..32])
}

/// Default an untagged order's `tag` to its intent digest, so the SDK's
/// per-account tag tombstones turn a resubmitted identical intent into
/// `duplicate_intent` whether or not approvals are required. An explicit
/// tag is the model's deliberate choice to submit again and is kept.
pub fn tag_intent(mut args: Value, digest: &str) -> Value {
    if let Some(order) = args.get_mut("order").and_then(Value::as_object_mut)
        && order.get("tag").is_none_or(Value::is_null)
    {
        order.insert("tag".into(), Value::String(intent_tag(digest)));
    }
    args
}

type CallLimiter = governor::DefaultDirectRateLimiter;

#[derive(Clone)]
pub struct AuthorizedServer {
    pub inner: SchwabMcp,
    pub principal: Arc<PrincipalPolicy>,
    pub audit: OrderJournal,
    calls: Option<Arc<CallLimiter>>,
}
impl AuthorizedServer {
    /// Bind a principal to its own approval and dispatch journal, kept beside
    /// the backend's own state under the principal's name digest.
    pub fn new(backend: Backend, switches: Switches, policy: PrincipalPolicy) -> Self {
        Self::with_stream_hub(backend, switches, policy, crate::server::StreamHub::new())
    }
    /// As [`new`](Self::new), sharing `hub`'s stream with the other
    /// principals' servers (only one principal may control it).
    pub fn with_stream_hub(
        backend: Backend,
        switches: Switches,
        policy: PrincipalPolicy,
        hub: crate::server::StreamHub,
    ) -> Self {
        let audit = OrderJournal::new(
            backend
                .trading()
                .principal_journal_path(&digest(policy.name.as_bytes())),
        );
        let calls = policy.max_calls_per_minute.map(|per_minute| {
            let burst = std::num::NonZeroU32::new(per_minute.get().div_ceil(10))
                .unwrap_or(std::num::NonZeroU32::MIN);
            Arc::new(governor::RateLimiter::direct(
                governor::Quota::per_minute(per_minute).allow_burst(burst),
            ))
        });
        Self {
            inner: SchwabMcp::with_stream_hub(backend, switches, hub),
            principal: Arc::new(policy),
            audit,
            calls,
        }
    }
}
impl McpHandler for AuthorizedServer {
    /// The inner server's identity with the execution mode in its title, so
    /// a client sees `paper` at initialize time, before any tool call.
    fn server_info(&self) -> turbomcp::ServerInfo {
        let mode = self.inner.backend().mode();
        let info = self.inner.server_info();
        let description = info.description.clone().unwrap_or_default();
        info.with_title(format!("Schwab MCP ({mode})"))
            .with_description(format!("Execution mode: {mode}. {description}"))
    }
    fn list_tools(&self) -> Vec<turbomcp::Tool> {
        self.inner
            .list_tools()
            .into_iter()
            .filter(|t| self.principal.tools.contains(t.name.as_str()))
            .collect()
    }
    /// Resources and prompts follow the tools they are about: a principal
    /// without an order tool is shown neither, and `server_capabilities`
    /// then omits them.
    fn list_resources(&self) -> Vec<turbomcp::Resource> {
        let audience = self.audience();
        self.inner
            .list_resources()
            .into_iter()
            .filter(|r| audience.offers_resource(&r.uri))
            .collect()
    }
    fn list_prompts(&self) -> Vec<turbomcp::Prompt> {
        let audience = self.audience();
        self.inner
            .list_prompts()
            .into_iter()
            .filter(|p| audience.offers_prompt(&p.name))
            .collect()
    }
    async fn call_tool<'a>(
        &'a self,
        name: &'a str,
        args: Value,
        ctx: &'a RequestContext,
    ) -> McpResult<ToolResult> {
        // A tool that does not exist is a protocol error, as on the
        // unauthorized transport and per the MCP spec; one that exists but
        // is not granted is a `not_permitted` result the model can read.
        let Some(tool) = self
            .inner
            .list_tools()
            .into_iter()
            .find(|tool| tool.name == name)
        else {
            return Err(McpError::tool_not_found(name));
        };
        if let Some(calls) = &self.calls
            && calls.check().is_err()
        {
            return Ok(ToolFailure::new(
                FailureCode::RateLimited,
                "This principal's tool-call rate limit is reached; call again shortly",
            )
            .into_result());
        }
        // A write's arguments are digested and journaled as sent: one its
        // schema does not declare would change the digest (and defeat the
        // duplicate check) while the tool ignores it.
        if matches!(name, "place_order" | "replace_order" | "cancel_order")
            && let Some(unknown) = undeclared_argument(&tool, &args)
        {
            return Ok(ToolFailure::invalid_arguments(format!(
                "{name} takes no argument `{unknown}`"
            ))
            .into_result());
        }
        match self.dispatch(name, args, ctx).await {
            Ok(result) => result,
            Err(failure) => Ok(failure.into_result()),
        }
    }
    async fn read_resource<'a>(
        &'a self,
        uri: &'a str,
        ctx: &'a RequestContext,
    ) -> McpResult<ResourceResult> {
        if !self.audience().offers_resource(uri) {
            return Err(McpError::resource_not_found(uri));
        }
        self.inner.read_resource(uri, ctx).await
    }
    /// The workflow is rendered for this principal's tools and switches,
    /// and only for an account it holds: a prompt that named another
    /// account would send the model to tools that refuse it.
    async fn get_prompt<'a>(
        &'a self,
        name: &'a str,
        args: Option<Value>,
        _ctx: &'a RequestContext,
    ) -> McpResult<PromptResult> {
        if !self.audience().offers_prompt(name) {
            return Err(McpError::prompt_not_found(name));
        }
        let proposal = guidance::Proposal::parse(args)?;
        if !self.principal.grants_account(&proposal.account_hash) {
            return Err(McpError::permission_denied(
                "Account not authorized for this principal",
            ));
        }
        Ok(guidance::propose_trade(&self.audience(), &proposal))
    }
}
impl AuthorizedServer {
    fn audience(&self) -> guidance::Audience<'_> {
        guidance::Audience::new(
            Some(&self.principal.tools),
            self.inner.switches(),
            self.inner.backend().mode(),
        )
    }
    /// Authorize, audit and dispatch one call. A `ToolFailure` here is a
    /// denial or an audit fault the model must see; the inner result is
    /// whatever the tool itself produced.
    ///
    /// The intent digest, and therefore approval, binds the arguments as the
    /// model sent them; the tag defaulted from it is added only to the
    /// dispatched copy, which is what the audit record keeps.
    async fn dispatch(
        &self,
        name: &str,
        args: Value,
        ctx: &RequestContext,
    ) -> Result<McpResult<ToolResult>, ToolFailure> {
        match name {
            "check_order" => return self.check_order(args).await,
            "get_server_status" => return self.get_server_status(args).await,
            "get_trade_journal" => return self.get_trade_journal(args).await,
            _ => {}
        }
        let submission = matches!(name, "place_order" | "replace_order");
        if submission {
            // An earlier dispatch interrupted by a process exit would block
            // this one (and may hold its approval): settle it first, from
            // the venue's journal, where that decides it.
            self.settle_stranded().await;
        }
        let digest = intent_digest(name, &args);
        let wait = approval_wait(&args)?;
        let effective = self.effective_policy(name, &digest).await?;
        let waited = match effective.authorize(name, &args) {
            Ok(()) => None,
            Err(failure) => {
                let waitable =
                    submission && wait.is_some() && failure.code == FailureCode::ApprovalRequired;
                if !waitable {
                    return Err(failure);
                }
                let (effective, waited) = self.await_approval(name, &digest, wait.unwrap()).await?;
                effective.authorize(name, &args)?;
                Some(waited)
            }
        };
        let args = if submission {
            tag_intent(args, &digest)
        } else {
            args
        };
        let write = submission || name == "cancel_order";
        let audit_id = if write {
            let dedup = if self.principal.require_approval && submission {
                // The audit consumes this one-use approval, so refuse here
                // what the tool would refuse before sending; a refusal
                // after it would leave the operator to approve a new intent.
                self.inner.switches().require(name)?;
                self.inner.backend().submission_admission().await?;
                Some(digest.clone())
            } else {
                None
            };
            // Cancellation must remain available while a submission audit is pending.
            let audit_scope = if name == "cancel_order" {
                format!("{}:cancel:{}", self.principal.name, uuid::Uuid::new_v4())
            } else {
                self.principal.name.clone()
            };
            let mut request = serde_json::json!({
                "principal": self.principal.name,
                "intentDigest": digest,
                "requestId": ctx.request_id(),
                "arguments": args,
            });
            if let Some(ms) = waited {
                request["approvalWaitedMs"] = ms.into();
            }
            // The refusal says which: a consumed approval or reused tag is a
            // duplicate, an unresolved earlier submission needs
            // reconciliation, and a lapsed approval needs a new one.
            let consumes_approval = dedup.is_some();
            let id = self
                .audit
                .begin_once(&audit_scope, name, request, dedup)
                .await
                .map_err(|e| ToolFailure::from(e).with_intent(digest))?;
            Some((id, consumes_approval))
        } else {
            None
        };
        let Some(audit_id) = audit_id else {
            let stream_control = matches!(
                name,
                "stream_connect"
                    | "stream_disconnect"
                    | "stream_subscribe"
                    | "stream_unsubscribe"
                    | "stream_set_service_fields"
            );
            let audited = stream_control.then(|| args.clone());
            let result = self
                .inner
                .call_tool(name, args, ctx)
                .await
                .map(crate::contract::normalize);
            // The stream is shared by the whole server: who changed it, and
            // how, belongs in the audit trail.
            if let Some(arguments) = audited {
                let outcome = crate::journal::Dispatch::of(&result).evidence();
                if let Err(e) = self
                    .audit
                    .record_event(
                        "stream_control",
                        &self.principal.name,
                        serde_json::json!({"tool": name, "arguments": arguments, "outcome": outcome}),
                    )
                    .await
                {
                    tracing::warn!(error = %e, tool = name, "stream control was not audited");
                }
            }
            return Ok(result);
        };
        // From the audit record on, the dispatch runs on its own task: a
        // client that cancels, disconnects or times out abandons only the
        // answer, and the record still reaches its outcome rather than
        // blocking every later submission of this principal.
        let this = self.clone();
        let name = name.to_owned();
        let ctx = ctx.clone();
        tokio::spawn(async move { this.audited(&name, args, &ctx, audit_id).await })
            .await
            .map_err(|e| ToolFailure::internal("Dispatch task failed", e))?
    }

    /// The audited half of [`dispatch`](Self::dispatch): run the tool and
    /// record how it ended against `id`.
    async fn audited(
        &self,
        name: &str,
        args: Value,
        ctx: &RequestContext,
        (id, consumed_approval): (String, bool),
    ) -> Result<McpResult<ToolResult>, ToolFailure> {
        let result = self
            .inner
            .call_tool(name, args, ctx)
            .await
            .map(crate::contract::normalize);
        // The SDK journal owns broker reconciliation. Keep dispatch outcome
        // attribution separately without claiming a tool success is a fill.
        let dispatch = crate::journal::Dispatch::of(&result);
        let outcome = schwab_rs::execution::SubmissionOutcome::Reconciled {
            evidence: dispatch.evidence(),
        };
        self.audit.finish(&id, outcome).await.map_err(|e| {
            ToolFailure::new(
                FailureCode::ReconciliationRequired,
                format!(
                    "Execution may have completed but the audit commit failed ({e}); \
                     reconcile before retrying"
                ),
            )
            .with_attempt(id.clone())
        })?;
        // Refused before anything reached the broker: the approval is
        // not spent, so the same intent may go once the refusal clears.
        if consumed_approval
            && let crate::journal::Dispatch::Failed { code: Some(code) } = dispatch
            && code.keeps_approval()
            && let Err(e) = self.audit.refund_approval(&id).await
        {
            tracing::warn!(attempt = %id, error = %e, "approval refund failed; it stays spent");
        }
        Ok(result)
    }

    /// Settle this principal's stranded submission records; see
    /// [`crate::journal::settle_stranded`]. Failures are logged and leave
    /// the record for an operator.
    async fn settle_stranded(&self) {
        use schwab_rs::execution::ExecutionStore;
        let Ok(pending) = self.audit.unresolved_records(&self.principal.name).await else {
            return;
        };
        let venue = self.inner.backend().trading().execution_store();
        for record in pending {
            if let Err(e) =
                crate::journal::settle_stranded(&self.audit, venue.as_ref(), &record.attempt_id)
                    .await
            {
                tracing::warn!(attempt = %record.attempt_id, error = %e, "could not settle a stranded dispatch");
            }
        }
    }

    /// The principal with a durable operator approval of the submission
    /// intent `digest` merged in, when approvals are required.
    async fn effective_policy(
        &self,
        name: &str,
        digest: &str,
    ) -> Result<PrincipalPolicy, ToolFailure> {
        let mut effective = (*self.principal).clone();
        if effective.require_approval
            && matches!(name, "place_order" | "replace_order")
            && self
                .audit
                .is_approved(&effective.name, digest)
                .await
                .map_err(|e| ToolFailure::internal("Approval store unavailable", e))?
        {
            effective.approved_intents.insert(digest.to_owned());
        }
        Ok(effective)
    }

    /// `check_order` under this principal: the tool and account must be
    /// permitted, then the submission the arguments describe is evaluated
    /// as [`dispatch`](Self::dispatch) would judge it, durable approvals
    /// included. Nothing is audited.
    async fn check_order(&self, args: Value) -> Result<McpResult<ToolResult>, ToolFailure> {
        self.principal.authorize("check_order", &args)?;
        Ok(crate::contract::run("check_order", async {
            let (name, args) = crate::preflight::Request::parse(args)?.intent();
            let digest = intent_digest(name, &args);
            let effective = self.effective_policy(name, &digest).await?;
            let verdict = crate::preflight::evaluate(
                self.inner.backend(),
                Some(&effective),
                self.inner.switches(),
                name,
                &args,
            )
            .await?;
            Ok(crate::contract::local(verdict.into_payload()?))
        })
        .await)
    }

    /// Hold, bounded, for the operator's approval of `digest`, then answer
    /// with the policy as it stands and how long the hold took.
    ///
    /// The approval is still the operator's: this polls the same durable
    /// store [`effective_policy`](Self::effective_policy) reads, written by
    /// `schwab-admin` or by the authority against a verified operator
    /// subject. Nothing here grants anything, so a session cannot approve
    /// its own intent by waiting for it. The caller has judged that the
    /// approval is the only thing missing; a denial that appears while
    /// waiting is caught when the caller judges again.
    async fn await_approval(
        &self,
        name: &str,
        digest: &str,
        wait: Duration,
    ) -> Result<(PrincipalPolicy, u64), ToolFailure> {
        let started = Instant::now();
        loop {
            let remaining = wait.saturating_sub(started.elapsed());
            let effective = self.effective_policy(name, digest).await?;
            if remaining.is_zero() || effective.approved_intents.contains(digest) {
                let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                return Ok((effective, ms));
            }
            tokio::time::sleep(remaining.min(APPROVAL_POLL)).await;
        }
    }

    /// `get_server_status` under this principal: the server's status,
    /// scoped to what this principal may see, with the policy it runs
    /// under.
    async fn get_server_status(&self, args: Value) -> Result<McpResult<ToolResult>, ToolFailure> {
        self.principal.authorize("get_server_status", &args)?;
        Ok(crate::contract::run("get_server_status", async {
            let mut status = self.inner.server_status().await;
            status.scope_to(&self.principal);
            Ok(crate::contract::local(status.into_payload()?))
        })
        .await)
    }

    /// `get_trade_journal` under this principal: its own audit journal for
    /// a permitted account, joined to the venue's orders.
    async fn get_trade_journal(&self, args: Value) -> Result<McpResult<ToolResult>, ToolFailure> {
        self.principal.authorize("get_trade_journal", &args)?;
        Ok(crate::contract::run("get_trade_journal", async {
            let request = crate::journal::Request::parse(args)?;
            let backend = self.inner.backend();
            let page = crate::journal::collect(&self.audit, backend, &self.principal.name, request)
                .await?;
            Ok(crate::contract::envelope(backend.source(), page))
        })
        .await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> PrincipalPolicy {
        serde_json::from_value(serde_json::json!({"name":"desk","token_sha256":"a".repeat(64),"tools":["get_account","get_accounts","check_order","place_order","cancel_order","stream_snapshot"],"accounts":["A"],"max_quantity":"2","max_notional":"100","require_approval":true})).unwrap()
    }
    fn order(quantity: Value) -> Value {
        serde_json::json!({"orderStrategyType":"SINGLE","orderType":"LIMIT","session":"NORMAL","duration":"DAY","price":"50","orderLegCollection":[{"quantity":quantity,"instruction":"BUY","instrument":{"symbol":"AAPL","assetType":"EQUITY"}}]})
    }
    /// A limit-on-close order has a firm price, so it is bounded exposure.
    #[test]
    fn limit_on_close_orders_are_bounded() {
        let mut close = order("2".into());
        close["orderType"] = "LIMIT_ON_CLOSE".into();
        let close: schwab_types::trading::OrderRequest = serde_json::from_value(close).unwrap();
        let (quantity, notional) = exposure(&close, &Default::default()).unwrap();
        assert_eq!((quantity, notional), (2.into(), 100.into()));
    }
    /// An approval binds the order Schwab would receive, not one JSON
    /// spelling of it; an order that is not a strict `OrderRequest` is
    /// digested as written.
    #[test]
    fn intent_digest_normalizes_a_valid_order_to_its_wire_form() {
        let quoted = serde_json::json!({"account_hash":"A","order":order("2".into())});
        let bare = serde_json::json!({"account_hash":"A","order":order(2.into())});
        assert_eq!(
            intent_digest("place_order", &quoted),
            intent_digest("place_order", &bare)
        );
        let mut unknown = quoted.clone();
        unknown["order"]["bogus"] = true.into();
        assert_ne!(
            intent_digest("place_order", &unknown),
            intent_digest("place_order", &quoted)
        );
        assert_ne!(
            intent_digest("place_order", &quoted),
            intent_digest("replace_order", &quoted)
        );
        // The words are journaled, not approved: re-wording is the same intent.
        let mut reasoned = quoted.clone();
        reasoned["rationale"] = "Breakout above the 50-day.".into();
        assert_eq!(
            intent_digest("place_order", &reasoned),
            intent_digest("place_order", &quoted)
        );
        // Nor is how long the caller will hold for the operator: the digest
        // `check_order` reports is the one a waiting submission carries.
        let mut holding = quoted.clone();
        holding["wait_ms"] = 30_000.into();
        assert_eq!(
            intent_digest("place_order", &holding),
            intent_digest("place_order", &quoted)
        );
    }
    /// A hold is bounded, opt-in, and only ever for the approval.
    #[tokio::test]
    async fn a_submission_holds_for_the_operator_and_for_nothing_else() {
        let wait = |value: Value| approval_wait(&serde_json::json!({"wait_ms": value}));
        assert!(approval_wait(&serde_json::json!({})).unwrap().is_none());
        assert!(wait(Value::Null).unwrap().is_none());
        assert!(wait(0.into()).unwrap().is_none());
        assert_eq!(
            wait(MAX_APPROVAL_WAIT_MS.into()).unwrap(),
            Some(Duration::from_millis(MAX_APPROVAL_WAIT_MS))
        );
        for over in [
            (MAX_APPROVAL_WAIT_MS + 1).into(),
            (-1).into(),
            "soon".into(),
        ] {
            assert_eq!(
                wait(over).unwrap_err().code,
                FailureCode::InvalidArguments,
                "wait_ms must be a bounded number of milliseconds"
            );
        }

        let server = authorized_as(
            Switches {
                trading: true,
                cancel: true,
            },
            policy(),
        );
        let ctx = RequestContext::stdio();
        let args = serde_json::json!({"account_hash":"A","order":order("2".into())});
        let digest = intent_digest("place_order", &args);

        // An unapproved intent is still denied when the hold runs out, and
        // the hold is what was asked for, not what the operator takes.
        let (unapproved, ms) = server
            .await_approval("place_order", &digest, Duration::from_millis(0))
            .await
            .unwrap();
        assert!(!unapproved.approved_intents.contains(&digest));
        assert!(ms < 1_000, "{ms}");

        // A denial only the caller can fix never holds, however long it asked.
        let mut oversized = args.clone();
        oversized["order"]["orderLegCollection"][0]["quantity"] = "3".into();
        oversized["wait_ms"] = MAX_APPROVAL_WAIT_MS.into();
        let started = Instant::now();
        let denied = server
            .dispatch("place_order", oversized, &ctx)
            .await
            .unwrap_err();
        assert_eq!(denied.code, FailureCode::RiskDenied);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "held on a denial"
        );
        assert!(server.audit.records().await.unwrap().is_empty());

        // An approval that lands while the call holds is picked up.
        let approving = server.audit.clone();
        let landing = digest.clone();
        tokio::spawn(async move {
            approving
                .approve(
                    "desk",
                    &landing,
                    "operator",
                    chrono::Utc::now().timestamp() + 60,
                )
                .await
                .unwrap();
        });
        let (approved, _) = server
            .await_approval("place_order", &digest, Duration::from_secs(10))
            .await
            .unwrap();
        assert!(approved.approved_intents.contains(&digest));
    }
    /// A rationale is judged before anything is audited or consumed, on
    /// every write tool, and a valid one changes nothing else.
    #[test]
    fn review_judges_the_rationale_of_every_write() {
        let p = policy();
        let mut args = serde_json::json!({"account_hash":"A","order":order("2".into()),
            "rationale":" "});
        let codes = |review: Review| review.denials.iter().map(|f| f.code).collect::<Vec<_>>();
        assert_eq!(
            codes(p.review("place_order", &args)),
            [FailureCode::InvalidArguments, FailureCode::ApprovalRequired]
        );
        args["rationale"] = "Breakout above the 50-day.".into();
        assert_eq!(
            codes(p.review("place_order", &args)),
            [FailureCode::ApprovalRequired]
        );
        let cancel = serde_json::json!({"account_hash":"A","order_id":"1","rationale":""});
        assert_eq!(
            codes(p.review("cancel_order", &cancel)),
            [FailureCode::InvalidArguments]
        );
        let read = serde_json::json!({"account_hash":"A","rationale":""});
        assert!(p.review("get_account", &read).denials.is_empty());
    }
    /// `review` keeps every denial a dispatch would find, in order, with
    /// the exposure it computed; `authorize` is its first denial.
    #[test]
    fn review_collects_every_denial_and_the_exposure() {
        let p = policy();
        let args = serde_json::json!({"account_hash":"B","order":order("3".into())});
        let review = p.review("place_order", &args);
        let codes: Vec<FailureCode> = review.denials.iter().map(|f| f.code).collect();
        assert_eq!(
            codes,
            [
                FailureCode::NotPermitted,
                FailureCode::RiskDenied,
                FailureCode::ApprovalRequired
            ]
        );
        assert_eq!(
            review.exposure,
            Some(("3".parse().unwrap(), "150".parse().unwrap()))
        );
        assert_eq!(
            code(p.authorize("place_order", &args)),
            Some(FailureCode::NotPermitted)
        );
        // Approval is not judged for an order that must change anyway.
        let invalid = serde_json::json!({"account_hash":"A","order":{"orderType":"LIMIT"}});
        let review = p.review("place_order", &invalid);
        assert_eq!(review.denials.len(), 1);
        assert_eq!(review.denials[0].code, FailureCode::InvalidArguments);
        assert_eq!(review.exposure, None);
    }
    /// `check_order` is judged under the principal, durable approvals
    /// included, reports every reason at once, and audits nothing.
    #[tokio::test]
    async fn check_order_reports_the_principal_verdict_without_dispatching() {
        let server = authorized();
        let ctx = RequestContext::stdio();
        let verdict = |result: McpResult<ToolResult>| {
            let result = result.unwrap();
            assert!(!result.is_error(), "{result:?}");
            serde_json::from_str::<Value>(result.first_text().unwrap()).unwrap()
        };
        let args = serde_json::json!({"account_hash":"A","order":order("2".into())});
        let placement = intent_digest("place_order", &args);
        let pending = verdict(server.call_tool("check_order", args.clone(), &ctx).await);
        assert_eq!(pending["allowed"], false);
        let codes = |v: &Value| {
            v["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["code"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        // Approval first, then the trading switch, which tests leave off.
        assert_eq!(codes(&pending), ["approval_required", "not_permitted"]);
        assert_eq!(pending["reasons"][0]["intentDigest"], placement);
        assert_eq!(pending["intentDigest"], placement);
        assert_eq!(pending["tag"], intent_tag(&placement));
        assert_eq!(
            pending["exposure"],
            serde_json::json!({"quantity":"2","notional":"100"})
        );
        assert_eq!(
            pending["limits"],
            serde_json::json!({"maxQuantity":"2","maxNotional":"100"})
        );
        assert_eq!(pending["requiresApproval"], true);
        assert_eq!(pending["approved"], false);
        assert_eq!(pending["tradingEnabled"], false);

        server
            .audit
            .approve(
                "desk",
                &placement,
                "operator",
                chrono::Utc::now().timestamp() + 60,
            )
            .await
            .unwrap();
        let approved = verdict(server.call_tool("check_order", args.clone(), &ctx).await);
        assert_eq!(approved["approved"], true);
        assert_eq!(codes(&approved), ["not_permitted"]);
        assert!(server.audit.records().await.unwrap().is_empty());

        // A submission refused before sending leaves the one-use approval
        // unspent, so it goes through once trading is switched on.
        let refused = server
            .call_tool("place_order", args.clone(), &ctx)
            .await
            .unwrap();
        assert_eq!(
            refused.structured_content.unwrap()["error"]["code"],
            "not_permitted"
        );
        assert!(server.audit.records().await.unwrap().is_empty());
        assert!(server.audit.is_approved("desk", &placement).await.unwrap());

        // An `order_id` makes it a replacement, which this principal lacks.
        let mut replacing = args.clone();
        replacing["order_id"] = "1".into();
        let replacement = verdict(server.call_tool("check_order", replacing, &ctx).await);
        assert_eq!(
            replacement["intentDigest"],
            intent_digest(
                "replace_order",
                &serde_json::json!({"account_hash":"A","order_id":"1","order":order("2".into())})
            )
        );
        assert_eq!(
            codes(&replacement),
            ["not_permitted", "approval_required", "not_permitted"]
        );

        // The tool's own arguments are the caller's problem, not a verdict.
        let denied = server
            .call_tool(
                "check_order",
                serde_json::json!({"account_hash":"B","order":order("2".into())}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(denied.is_error());
        assert_eq!(
            denied.structured_content.unwrap()["error"]["code"],
            "not_permitted"
        );
        let malformed = server
            .call_tool(
                "check_order",
                serde_json::json!({"account_hash":"A","order":{"orderType":"LIMIT"}}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(malformed.is_error());
        assert_eq!(
            malformed.structured_content.unwrap()["error"]["code"],
            "invalid_arguments"
        );
    }
    /// The status a principal sees carries its own policy, and an operator
    /// halt shows up as inadmissible trading whose reason a scoped
    /// principal is not told.
    #[tokio::test]
    async fn server_status_carries_the_principal_and_the_halt() {
        let server = authorized();
        let ctx = RequestContext::stdio();
        let denied = server
            .call_tool("get_server_status", serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(denied.is_error(), "not in the tool allowlist");
        assert_eq!(
            denied.structured_content.unwrap()["error"]["code"],
            "not_permitted"
        );

        let mut policy = policy();
        policy.tools.insert("get_server_status".into());
        let server = AuthorizedServer::new(
            server.inner.backend().clone(),
            Switches::default(),
            policy.clone(),
        );
        server
            .inner
            .backend()
            .trading()
            .halt_submissions("desk review", "operator")
            .await
            .unwrap();
        let result = server
            .call_tool("get_server_status", serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error(), "{result:?}");
        let status: Value = serde_json::from_str(result.first_text().unwrap()).unwrap();
        assert_eq!(status["principal"]["name"], "desk");
        assert_eq!(status["principal"]["accounts"], serde_json::json!(["A"]));
        assert_eq!(status["principal"]["maxQuantity"], "2");
        assert_eq!(status["principal"]["requireApproval"], true);
        let tools = status["principal"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|t| t == "place_order"), "{tools:?}");
        assert!(tools.windows(2).all(|w| w[0].as_str() < w[1].as_str()));
        // The halt is visible; the operator's words, which a control writes
        // with another account's equity, loss and cap, are not.
        assert_eq!(
            status["trading"]["operatorHalt"],
            crate::status::HALT_WITHHELD
        );
        assert_eq!(status["trading"]["admissible"], false);
        assert_eq!(status["readiness"]["tradingReady"], false);

        // A principal granted every account is operator-grade and reads it.
        let mut all = policy.clone();
        all.tools.insert("get_server_status".into());
        all.accounts.clear();
        all.all_accounts = true;
        let server =
            AuthorizedServer::new(server.inner.backend().clone(), Switches::default(), all);
        let result = server
            .call_tool("get_server_status", serde_json::json!({}), &ctx)
            .await
            .unwrap();
        let status: Value = serde_json::from_str(result.first_text().unwrap()).unwrap();
        assert_eq!(status["trading"]["operatorHalt"], "desk review");
    }
    #[test]
    fn approval_digest_is_independent_of_nested_json_object_order() {
        let a: Value =
            serde_json::from_str(r#"{"account":"A","order":{"price":"10","symbol":"AAPL"}}"#)
                .unwrap();
        let b: Value =
            serde_json::from_str(r#"{"order":{"symbol":"AAPL","price":"10"},"account":"A"}"#)
                .unwrap();
        assert_eq!(
            intent_digest("place_order", &a),
            intent_digest("place_order", &b)
        );
    }
    #[test]
    fn issuer_subject_does_not_require_a_dummy_static_secret() {
        let p: PrincipalPolicy = serde_json::from_value(serde_json::json!({"name":"desk","identity_subject":"subject-123","tools":["get_quotes"]})).unwrap();
        assert!(p.validate().is_ok());
        let mut invalid = p;
        invalid.identity_subject = Some(" ".into());
        assert!(invalid.validate().is_err());
    }
    fn code(result: Result<(), ToolFailure>) -> Option<FailureCode> {
        result.err().map(|f| f.code)
    }
    fn authorized() -> AuthorizedServer {
        authorized_as(Switches::default(), policy())
    }
    fn authorized_as(switches: Switches, policy: PrincipalPolicy) -> AuthorizedServer {
        let client = schwab_rs::SchwabClient::new(schwab_rs::SchwabConfig {
            oauth: schwab_rs::OAuthConfig {
                app_key: "k".repeat(32),
                app_secret: "s".repeat(16),
                token_store_kind: schwab_rs::auth::TokenStoreKind::File,
                tokens_file: std::env::temp_dir().join(format!("{}.json", uuid::Uuid::new_v4())),
                ..Default::default()
            },
            streaming: None,
            ..Default::default()
        })
        .unwrap();
        AuthorizedServer::new(Backend::Live(Arc::new(client)), switches, policy)
    }
    /// Denials reach the model as `isError` results it can read, never as
    /// JSON-RPC errors, and carry the code and intent digest it needs next.
    #[tokio::test]
    async fn denials_are_structured_tool_results() {
        let server = authorized();
        let ctx = RequestContext::stdio();
        let error = |result: McpResult<ToolResult>| {
            let result = result.expect("denial is a tool result");
            assert!(result.is_error());
            let text: Value = serde_json::from_str(result.first_text().unwrap()).unwrap();
            let body = result.structured_content.unwrap();
            assert_eq!(text, body);
            body["error"].clone()
        };
        let unlisted = error(
            server
                .call_tool("get_quote", serde_json::json!({"symbol":"AAPL"}), &ctx)
                .await,
        );
        assert_eq!(unlisted["code"], "not_permitted");
        assert_eq!(unlisted["retryable"], false);
        assert_eq!(unlisted["disposition"], "not_permitted");
        let foreign = error(
            server
                .call_tool(
                    "get_account",
                    serde_json::json!({"account_hash":"B","include_positions":false}),
                    &ctx,
                )
                .await,
        );
        assert_eq!(foreign["code"], "not_permitted");
        let args = serde_json::json!({"account_hash":"A","order":{"orderStrategyType":"SINGLE","orderType":"LIMIT","session":"NORMAL","duration":"DAY","price":"50","orderLegCollection":[{"quantity":"2","instruction":"BUY","instrument":{"symbol":"AAPL","assetType":"EQUITY"}}]}});
        let pending = error(server.call_tool("place_order", args.clone(), &ctx).await);
        assert_eq!(pending["code"], "approval_required");
        assert_eq!(pending["disposition"], "ask_operator");
        assert_eq!(pending["intentDigest"], intent_digest("place_order", &args));
        // Arguments the macro cannot extract are normalized into the contract.
        let malformed = error(
            server
                .call_tool(
                    "stream_snapshot",
                    serde_json::json!({"service": "NOT_A_SERVICE"}),
                    &ctx,
                )
                .await,
        );
        assert_eq!(malformed["code"], "invalid_arguments");
        // A write's undeclared argument would change its digest unseen.
        let mut padded = args.clone();
        padded["nonce"] = "x".into();
        let padded = error(server.call_tool("place_order", padded, &ctx).await);
        assert_eq!(padded["code"], "invalid_arguments");
        // A tool that does not exist at all is a protocol error.
        assert!(
            server
                .call_tool("no_such_tool", serde_json::json!({}), &ctx)
                .await
                .is_err()
        );
    }
    /// A principal with a call limit is told to slow down, not refused.
    #[tokio::test]
    async fn a_principal_call_limit_answers_rate_limited() {
        let mut limited = policy();
        limited.max_calls_per_minute = std::num::NonZeroU32::new(1);
        let server = authorized_as(Switches::default(), limited);
        let ctx = RequestContext::stdio();
        let args = serde_json::json!({"symbol": "AAPL"});
        let _ = server.call_tool("get_quote", args.clone(), &ctx).await;
        let second = server.call_tool("get_quote", args, &ctx).await.unwrap();
        assert_eq!(
            second.structured_content.unwrap()["error"]["code"],
            "rate_limited"
        );
    }

    /// An approved submission that fails before reaching the broker (here,
    /// for want of a Schwab session) keeps its approval for the retry.
    #[tokio::test]
    async fn an_unsent_submission_gives_its_approval_back() {
        let server = authorized_as(
            Switches {
                trading: true,
                cancel: false,
            },
            policy(),
        );
        let ctx = RequestContext::stdio();
        let args = serde_json::json!({"account_hash":"A","order":order("2".into())});
        let digest = intent_digest("place_order", &args);
        server
            .audit
            .approve(
                "desk",
                &digest,
                "operator",
                chrono::Utc::now().timestamp() + 60,
            )
            .await
            .unwrap();
        for _ in 0..2 {
            let failed = server
                .call_tool("place_order", args.clone(), &ctx)
                .await
                .unwrap();
            assert_eq!(
                failed.structured_content.unwrap()["error"]["code"],
                "unavailable"
            );
            assert!(server.audit.is_approved("desk", &digest).await.unwrap());
        }
        assert_eq!(server.audit.records().await.unwrap().len(), 2);
    }

    /// A dispatch whose process died after the audit record but before the
    /// venue journaled anything is settled from the venue's journal on the
    /// next submission, its approval given back, instead of blocking the
    /// principal until an operator steps in.
    #[tokio::test]
    async fn a_stranded_dispatch_is_settled_from_the_venue_journal() {
        let server = authorized_as(
            Switches {
                trading: true,
                cancel: false,
            },
            policy(),
        );
        let ctx = RequestContext::stdio();
        let args = serde_json::json!({"account_hash":"A","order":order("2".into())});
        let digest = intent_digest("place_order", &args);
        server
            .audit
            .approve(
                "desk",
                &digest,
                "operator",
                chrono::Utc::now().timestamp() + 600,
            )
            .await
            .unwrap();
        let stranded = server
            .audit
            .begin_once(
                "desk",
                "place_order",
                serde_json::json!({
                    "principal": "desk",
                    "intentDigest": digest,
                    "requestId": "lost",
                    "arguments": tag_intent(args.clone(), &digest),
                }),
                Some(digest.clone()),
            )
            .await
            .unwrap();
        // Too recent to judge: its dispatcher may still be running, and it
        // holds the one-use approval.
        let blocked = server
            .call_tool("place_order", args.clone(), &ctx)
            .await
            .unwrap();
        assert_eq!(
            blocked.structured_content.unwrap()["error"]["code"],
            "approval_required"
        );
        let id = stranded.clone();
        server
            .audit
            .database()
            .run(move |c| {
                c.execute(
                    "UPDATE submissions SET created_at=created_at-180000 WHERE id=?1",
                    [id],
                )
                .map_err(|e| schwab_rs::Error::Storage(e.to_string()))
            })
            .await
            .unwrap();
        let retried = server
            .call_tool("place_order", args.clone(), &ctx)
            .await
            .unwrap();
        // The retry itself reached the tool (and failed there, for want of
        // a Schwab session), so the stranded record no longer blocks it.
        assert_eq!(
            retried.structured_content.unwrap()["error"]["code"],
            "unavailable"
        );
        let settled = server.audit.record(&stranded).await.unwrap().unwrap();
        assert!(matches!(
            settled.outcome,
            Some(schwab_rs::execution::SubmissionOutcome::Reconciled { ref evidence })
                if evidence.contains("nothing reached the venue")
        ));
        assert!(server.audit.is_approved("desk", &digest).await.unwrap());
    }

    /// The digest and approval bind the model's arguments; only the
    /// dispatched copy carries the derived tag, and an explicit tag stays.
    #[tokio::test]
    async fn submissions_are_tagged_with_their_intent_digest() {
        let mut server = authorized();
        let mut policy = policy();
        policy.require_approval = false;
        server.principal = Arc::new(policy);
        let ctx = RequestContext::stdio();
        let args = serde_json::json!({"account_hash":"A","order":{"orderStrategyType":"SINGLE","orderType":"LIMIT","session":"NORMAL","duration":"DAY","price":"50","orderLegCollection":[{"quantity":"2","instruction":"BUY","instrument":{"symbol":"AAPL","assetType":"EQUITY"}}]}});
        let digest = intent_digest("place_order", &args);
        assert!(
            server
                .call_tool("place_order", args.clone(), &ctx)
                .await
                .unwrap()
                .is_error()
        );
        let mut explicit = args.clone();
        explicit["order"]["tag"] = "desk-7".into();
        assert!(
            server
                .call_tool("place_order", explicit.clone(), &ctx)
                .await
                .unwrap()
                .is_error()
        );
        let records = server.audit.records().await.unwrap();
        assert_eq!(records.len(), 2);
        let derived = format!("{INTENT_TAG_PREFIX}{}", &digest[..32]);
        let audited = |tag: &str| {
            &records
                .iter()
                .find(|r| r.request["arguments"]["order"]["tag"] == tag)
                .unwrap_or_else(|| panic!("no audit record tagged {tag}"))
                .request
        };
        assert_eq!(audited(&derived)["intentDigest"], digest);
        assert_eq!(
            audited("desk-7")["intentDigest"],
            intent_digest("place_order", &explicit)
        );
        assert_ne!(audited("desk-7")["intentDigest"], digest);

        let mut null_tag = args.clone();
        null_tag["order"]["tag"] = Value::Null;
        assert_eq!(tag_intent(null_tag, &digest)["order"]["tag"], derived);
        let not_an_order = serde_json::json!({"account_hash":"A","order":"?"});
        assert_eq!(tag_intent(not_an_order.clone(), &digest), not_an_order);
    }
    #[test]
    fn account_aggregate_activity_and_tool_permissions_fail_closed() {
        let p = policy();
        assert!(
            p.authorize("get_account", &serde_json::json!({"account_hash":"A"}))
                .is_ok()
        );
        assert_eq!(
            code(p.authorize("get_account", &serde_json::json!({"account_hash":"B"}))),
            Some(FailureCode::NotPermitted)
        );
        assert_eq!(
            code(p.authorize("get_accounts", &serde_json::json!({}))),
            Some(FailureCode::NotPermitted)
        );
        assert_eq!(
            code(p.authorize(
                "stream_snapshot",
                &serde_json::json!({"service":"ACCT_ACTIVITY"})
            )),
            Some(FailureCode::NotPermitted)
        );
        assert_eq!(
            code(p.authorize("replace_order", &serde_json::json!({"account_hash":"A"}))),
            Some(FailureCode::NotPermitted)
        );
        assert!(
            p.authorize("cancel_order", &serde_json::json!({"account_hash":"A"}))
                .is_ok()
        );
    }
    #[test]
    fn approval_binds_exact_account_order_and_decimal_risk_limits() {
        let mut p = policy();
        let mut args = serde_json::json!({"account_hash":"A","order":{"orderStrategyType":"SINGLE","orderType":"LIMIT","session":"NORMAL","duration":"DAY","price":"50","orderLegCollection":[{"quantity":"2","instruction":"BUY","instrument":{"symbol":"AAPL","assetType":"EQUITY"}}]}});
        let pending = p.authorize("place_order", &args).unwrap_err();
        assert_eq!(pending.code, FailureCode::ApprovalRequired);
        assert_eq!(
            pending.intent_digest.as_deref(),
            Some(intent_digest("place_order", &args).as_str())
        );
        p.approved_intents
            .insert(intent_digest("place_order", &args));
        p.authorize("place_order", &args).unwrap();
        args["order"]["price"] = "50.0000000000000000000000001".into();
        p.approved_intents
            .insert(intent_digest("place_order", &args));
        assert_eq!(
            code(p.authorize("place_order", &args)),
            Some(FailureCode::RiskDenied)
        );
        args["order"]["orderLegCollection"] = serde_json::json!([]);
        assert_eq!(
            code(p.authorize("place_order", &args)),
            Some(FailureCode::InvalidArguments)
        );
    }
    #[test]
    fn bracket_exits_are_priced_at_the_entry_and_everything_else_stays_bounded() {
        use rust_decimal::Decimal;
        use schwab_types::trading::{OrderInstruction, OrderRequest, OrderRequestLeg};
        let d = |s: &str| s.parse::<Decimal>().unwrap();
        let none = Default::default();
        let entry = || OrderRequestLeg::equity(OrderInstruction::Buy, "AAPL", 2);
        // Entry 2 @ 50 + take profit 2 @ 60 + stop 2 priced at the entry's 50.
        let bracket = OrderRequest::limit(entry(), d("50"))
            .bracket(d("60"), d("45"))
            .unwrap();
        assert_eq!(exposure(&bracket, &none).unwrap(), (d("6"), d("320")));

        // A naked stop with nothing to cover it, or a market entry, is refused.
        let stop = || {
            OrderRequest::stop(
                OrderRequestLeg::equity(OrderInstruction::Sell, "AAPL", 2),
                d("45"),
            )
        };
        assert!(exposure(&stop(), &none).is_err());
        assert!(exposure(&OrderRequest::market(entry()), &none).is_err());

        // Only a child that flattens what the entry opened is covered: not a
        // larger quantity, another symbol, another buy, or a market entry.
        let under = |child: OrderRequest| OrderRequest::limit(entry(), d("50")).triggers(child);
        assert!(exposure(&under(stop()), &none).is_ok());
        for uncovered in [
            OrderRequest::stop(
                OrderRequestLeg::equity(OrderInstruction::Sell, "AAPL", 3),
                d("45"),
            ),
            OrderRequest::stop(
                OrderRequestLeg::equity(OrderInstruction::Sell, "MSFT", 2),
                d("45"),
            ),
            OrderRequest::stop(entry(), d("55")),
        ] {
            assert!(exposure(&under(uncovered), &none).is_err());
        }
        assert!(exposure(&OrderRequest::market(entry()).triggers(stop()), &none).is_err());
    }
    /// Prompts and resources follow the principal's tools and accounts: a
    /// principal with `check_order` is offered both, rendered for the tools
    /// and switches it has; one without sees neither, and the capabilities
    /// it is told at initialize agree.
    #[tokio::test]
    async fn guidance_is_gated_by_the_principal() {
        let ctx = RequestContext::stdio();
        let uri = guidance::ORDER_SPEC_URI;
        let prompt = guidance::PROPOSE_TRADE;
        let args = |account: &str| Some(serde_json::json!({"account_hash": account}));

        let desk = authorized();
        assert_eq!(desk.list_resources().len(), 1);
        assert_eq!(desk.list_prompts().len(), 1);
        assert!(desk.server_capabilities().resources.is_some());
        assert!(desk.server_capabilities().prompts.is_some());
        assert!(
            desk.read_resource(uri, &ctx)
                .await
                .unwrap()
                .first_text()
                .unwrap()
                .starts_with("# OrderSpec")
        );
        assert!(desk.read_resource("schwab://other", &ctx).await.is_err());
        // Held `place_order`, but the trading switch is off.
        let text = desk
            .get_prompt(prompt, args("A"), &ctx)
            .await
            .unwrap()
            .messages[0]
            .content
            .as_text()
            .unwrap()
            .to_owned();
        assert!(text.contains("Call `get_account`"), "{text}");
        assert!(!text.contains("`get_quotes`"), "{text}");
        assert!(
            text.contains("does not submit orders (the operator's trading switch is off)"),
            "{text}"
        );
        // Another account, or a missing one, is refused before rendering.
        let denied = desk.get_prompt(prompt, args("B"), &ctx).await.unwrap_err();
        assert!(
            denied.to_string().contains("Account not authorized"),
            "{denied}"
        );
        assert!(desk.get_prompt(prompt, None, &ctx).await.is_err());
        assert!(desk.get_prompt("other", args("A"), &ctx).await.is_err());

        // With the switch on, the same principal is walked through placing it.
        let trading = authorized_as(
            Switches {
                trading: true,
                cancel: true,
            },
            policy(),
        );
        let text = trading
            .get_prompt(prompt, args("A"), &ctx)
            .await
            .unwrap()
            .messages[0]
            .content
            .as_text()
            .unwrap()
            .to_owned();
        assert!(text.contains("call `place_order`"), "{text}");
        assert!(!text.contains("does not submit"), "{text}");

        // A reader holds no order tool: nothing is listed or served.
        let mut reader = policy();
        reader.tools = ["get_account", "get_quotes"].map(String::from).into();
        let reader = authorized_as(Switches::default(), reader);
        assert!(reader.list_resources().is_empty());
        assert!(reader.list_prompts().is_empty());
        assert!(reader.server_capabilities().resources.is_none());
        assert!(reader.server_capabilities().prompts.is_none());
        assert!(reader.read_resource(uri, &ctx).await.is_err());
        assert!(reader.get_prompt(prompt, args("A"), &ctx).await.is_err());
    }
}
