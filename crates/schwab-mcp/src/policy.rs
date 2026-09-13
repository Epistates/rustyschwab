//! Immutable per-principal authorization. Network authentication selects a
//! principal-specific handler and session store before MCP dispatch.
//!
//! Denials are business outcomes, so they are returned as `isError` tool
//! results in the [`crate::contract`] shape rather than JSON-RPC errors: the
//! model reads `not_permitted`, `risk_denied` or `approval_required` and
//! knows whether to change the order, wait for the operator, or stop.
use crate::SchwabMcp;
use crate::contract::{FailureCode, ToolFailure};
use schwab_rs::{SchwabClient, broker::TradingBackend, execution::OrderJournal};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, sync::Arc};
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
    fn owns_streaming(&self) -> bool {
        self.tools.iter().any(|t| t.starts_with("stream_"))
    }
    /// The first reason this principal may not run `name` with `args`.
    pub fn authorize(&self, name: &str, args: &Value) -> Result<(), ToolFailure> {
        if !self.tools.contains(name) {
            return Err(ToolFailure::not_permitted(
                "Operation not authorized for this principal",
            ));
        }
        if let Some(account) = args.get("account_hash").and_then(Value::as_str)
            && !self.all_accounts
            && !self.accounts.contains(account)
        {
            return Err(ToolFailure::not_permitted(
                "Account not authorized for this principal",
            ));
        }
        let aggregate = matches!(
            name,
            "get_accounts" | "get_account_numbers" | "get_user_preferences" | "get_all_orders"
        );
        let activity = args.get("service").and_then(Value::as_str) == Some("ACCT_ACTIVITY");
        if !self.all_accounts && (aggregate || activity) {
            return Err(ToolFailure::not_permitted(
                "Operation requires access to all linked accounts",
            ));
        }
        if matches!(name, "place_order" | "replace_order") {
            let order: schwab_types::trading::OrderRequest = serde_json::from_value(
                args.get("order").cloned().unwrap_or(Value::Null),
            )
            .map_err(|e| ToolFailure::invalid_arguments(format!("Invalid order request: {e}")))?;
            order.validate().map_err(ToolFailure::invalid_arguments)?;
            let (quantity, notional) = exposure(&order, &self.option_multipliers)
                .map_err(ToolFailure::invalid_arguments)?;
            let max_quantity = self.max_quantity.unwrap_or(rust_decimal::Decimal::ZERO);
            let max_notional = self.max_notional.unwrap_or(rust_decimal::Decimal::ZERO);
            if quantity > max_quantity || notional > max_notional {
                return Err(ToolFailure::new(
                    FailureCode::RiskDenied,
                    format!(
                        "Order exposure {quantity} units / {notional} notional exceeds the \
                         principal's limits of {max_quantity} / {max_notional}"
                    ),
                ));
            }
            let digest = intent_digest(name, args);
            if self.require_approval && !self.approved_intents.contains(&digest) {
                return Err(ToolFailure::new(
                    FailureCode::ApprovalRequired,
                    format!("Order requires operator approval of intent {digest}"),
                )
                .with_intent(digest));
            }
        }
        Ok(())
    }
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
pub fn intent_digest(name: &str, args: &Value) -> String {
    digest(
        serde_json::to_vec(&(name, schwab_rs::utils::canonical_json(args)))
            .unwrap()
            .as_slice(),
    )
}

#[derive(Clone)]
pub struct AuthorizedServer {
    pub inner: SchwabMcp,
    pub principal: Arc<PrincipalPolicy>,
    pub audit: OrderJournal,
}
impl AuthorizedServer {
    /// Bind a principal to its own approval and dispatch journal, kept beside
    /// the client's token file under the principal's name digest.
    pub fn new(client: Arc<SchwabClient>, policy: PrincipalPolicy) -> Self {
        let audit =
            OrderJournal::new(client.principal_journal_path(&digest(policy.name.as_bytes())));
        Self {
            inner: SchwabMcp::new(client),
            principal: Arc::new(policy),
            audit,
        }
    }
}
impl McpHandler for AuthorizedServer {
    fn server_info(&self) -> turbomcp::ServerInfo {
        self.inner.server_info()
    }
    fn list_tools(&self) -> Vec<turbomcp::Tool> {
        self.inner
            .list_tools()
            .into_iter()
            .filter(|t| self.principal.tools.contains(t.name.as_str()))
            .collect()
    }
    fn list_resources(&self) -> Vec<turbomcp::Resource> {
        vec![]
    }
    fn list_prompts(&self) -> Vec<turbomcp::Prompt> {
        vec![]
    }
    async fn call_tool<'a>(
        &'a self,
        name: &'a str,
        args: Value,
        ctx: &'a RequestContext,
    ) -> McpResult<ToolResult> {
        match self.dispatch(name, args, ctx).await {
            Ok(result) => result,
            Err(failure) => Ok(failure.into_result()),
        }
    }
    async fn read_resource<'a>(
        &'a self,
        uri: &'a str,
        _ctx: &'a RequestContext,
    ) -> McpResult<ResourceResult> {
        Err(McpError::resource_not_found(uri))
    }
    async fn get_prompt<'a>(
        &'a self,
        name: &'a str,
        _args: Option<Value>,
        _ctx: &'a RequestContext,
    ) -> McpResult<PromptResult> {
        Err(McpError::prompt_not_found(name))
    }
}
impl AuthorizedServer {
    /// Authorize, audit and dispatch one call. A `ToolFailure` here is a
    /// denial or an audit fault the model must see; the inner result is
    /// whatever the tool itself produced.
    async fn dispatch(
        &self,
        name: &str,
        args: Value,
        ctx: &RequestContext,
    ) -> Result<McpResult<ToolResult>, ToolFailure> {
        let mut effective = (*self.principal).clone();
        if effective.require_approval && matches!(name, "place_order" | "replace_order") {
            let digest = intent_digest(name, &args);
            if self
                .audit
                .is_approved(&effective.name, &digest)
                .await
                .map_err(|e| ToolFailure::internal("Approval store unavailable", e))?
            {
                effective.approved_intents.insert(digest);
            }
        }
        effective.authorize(name, &args)?;
        let write = matches!(name, "place_order" | "replace_order" | "cancel_order");
        let audit_id = if write {
            let digest = intent_digest(name, &args);
            let dedup = if self.principal.require_approval && name != "cancel_order" {
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
            let request = serde_json::json!({
                "principal": self.principal.name,
                "intentDigest": digest,
                "requestId": ctx.request_id(),
                "arguments": args,
            });
            let id = self
                .audit
                .begin_once(&audit_scope, name, request, dedup)
                .await
                .map_err(|e| {
                    ToolFailure::new(
                        FailureCode::ReconciliationRequired,
                        format!(
                            "Audit refused the dispatch ({e}): the approval was already \
                             consumed or an earlier operation still requires reconciliation. \
                             Read the order state; do not resubmit."
                        ),
                    )
                    .with_intent(digest)
                })?;
            Some(id)
        } else {
            None
        };
        let result = self
            .inner
            .call_tool(name, args, ctx)
            .await
            .map(crate::contract::normalize);
        if let Some(id) = audit_id {
            // The SDK journal owns broker reconciliation. Keep dispatch outcome
            // attribution separately without claiming a tool success is a fill.
            let succeeded = result.as_ref().is_ok_and(|r| !r.is_error());
            let outcome = schwab_rs::execution::SubmissionOutcome::Reconciled {
                evidence: if succeeded {
                    "MCP dispatch completed; consult SDK submission journal".into()
                } else {
                    "MCP dispatch returned error; consult SDK submission journal".into()
                },
            };
            self.audit.finish(&id, outcome).await.map_err(|e| {
                ToolFailure::new(
                    FailureCode::ReconciliationRequired,
                    format!(
                        "Execution may have completed but the audit commit failed ({e}); \
                         reconcile before retrying"
                    ),
                )
                .with_attempt(id)
            })?;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> PrincipalPolicy {
        serde_json::from_value(serde_json::json!({"name":"desk","token_sha256":"a".repeat(64),"tools":["get_account","get_accounts","place_order","cancel_order","stream_snapshot"],"accounts":["A"],"max_quantity":"2","max_notional":"100","require_approval":true})).unwrap()
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
        AuthorizedServer::new(Arc::new(client), policy())
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
}
