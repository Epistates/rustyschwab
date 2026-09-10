//! Immutable per-principal authorization. Network authentication selects a
//! principal-specific handler and session store before MCP dispatch.
use crate::SchwabMcp;
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
    pub fn authorize(&self, name: &str, args: &Value) -> McpResult<()> {
        if !self.tools.contains(name) {
            return Err(McpError::invalid_params("Operation not authorized"));
        }
        if let Some(account) = args.get("account_hash").and_then(Value::as_str)
            && !self.all_accounts
            && !self.accounts.contains(account)
        {
            return Err(McpError::invalid_params("Account not authorized"));
        }
        let aggregate = matches!(
            name,
            "get_accounts" | "get_account_numbers" | "get_user_preferences" | "get_all_orders"
        );
        let activity = args.get("service").and_then(Value::as_str) == Some("ACCT_ACTIVITY");
        if !self.all_accounts && (aggregate || activity) {
            return Err(McpError::invalid_params(
                "Operation requires access to all linked accounts",
            ));
        }
        if matches!(name, "place_order" | "replace_order") {
            let order: schwab_types::trading::OrderRequest =
                serde_json::from_value(args.get("order").cloned().unwrap_or(Value::Null))
                    .map_err(|_| McpError::invalid_params("Invalid order request"))?;
            order.validate().map_err(McpError::invalid_params)?;
            let (quantity, notional) = exposure(&order, &self.option_multipliers)?;
            if quantity > self.max_quantity.unwrap_or(rust_decimal::Decimal::ZERO)
                || notional > self.max_notional.unwrap_or(rust_decimal::Decimal::ZERO)
            {
                return Err(McpError::invalid_params(
                    "Order exceeds configured risk limits",
                ));
            }
            if self.require_approval && !self.approved_intents.contains(&intent_digest(name, args))
            {
                return Err(McpError::invalid_params(format!(
                    "Order requires operator approval of intent {}",
                    intent_digest(name, args)
                )));
            }
        }
        Ok(())
    }
}
// Conservative gross exposure sums all legs and child strategies, including
// mutually exclusive children. Network writes require bounded-price orders;
// market, trailing-market, and naked stop orders have no finite execution cap.
fn exposure(
    order: &schwab_types::trading::OrderRequest,
    multipliers: &std::collections::HashMap<String, rust_decimal::Decimal>,
) -> McpResult<(rust_decimal::Decimal, rust_decimal::Decimal)> {
    use schwab_types::trading::{OrderStrategyType, OrderType};
    let mut quantity = rust_decimal::Decimal::ZERO;
    let mut notional = rust_decimal::Decimal::ZERO;
    if order.order_strategy_type != OrderStrategyType::Oco {
        if !matches!(
            order.order_type,
            Some(
                OrderType::Limit
                    | OrderType::StopLimit
                    | OrderType::NetDebit
                    | OrderType::NetCredit
            )
        ) {
            return Err(McpError::invalid_params(
                "Policy requires an order with a bounded limit price",
            ));
        }
        if order.order_leg_collection.len() != 1 {
            return Err(McpError::invalid_params(
                "Multi-leg exposure requires an external portfolio risk engine",
            ));
        }
        for leg in &order.order_leg_collection {
            use schwab_types::trading::OrderInstruction;
            if matches!(
                leg.instruction,
                OrderInstruction::SellShort
                    | OrderInstruction::SellShortExempt
                    | OrderInstruction::SellToOpen
            ) {
                return Err(McpError::invalid_params(
                    "Opening short exposure requires an external portfolio risk engine",
                ));
            }
            let multiplier = match leg.instrument.asset_type.as_str() {
                "EQUITY" => rust_decimal::Decimal::ONE,
                "OPTION" => *multipliers
                    .get(&leg.instrument.symbol)
                    .filter(|v| **v > rust_decimal::Decimal::ZERO)
                    .ok_or_else(|| {
                        McpError::invalid_params(
                            "Option contract requires an operator-verified multiplier",
                        )
                    })?,
                _ => {
                    return Err(McpError::invalid_params(
                        "Risk policy supports standard equity and option contracts only",
                    ));
                }
            };
            quantity = quantity
                .checked_add(leg.quantity)
                .ok_or_else(|| McpError::invalid_params("Quantity overflow"))?;
            let exposure = leg
                .quantity
                .checked_mul(multiplier)
                .and_then(|v| v.checked_mul(order.price.unwrap_or(rust_decimal::Decimal::MAX)))
                .ok_or_else(|| McpError::invalid_params("Notional overflow"))?;
            notional = notional
                .checked_add(exposure)
                .ok_or_else(|| McpError::invalid_params("Notional overflow"))?;
        }
    }
    for child in &order.child_order_strategies {
        let (q, n) = exposure(child, multipliers)?;
        quantity = quantity
            .checked_add(q)
            .ok_or_else(|| McpError::invalid_params("Quantity overflow"))?;
        notional = notional
            .checked_add(n)
            .ok_or_else(|| McpError::invalid_params("Notional overflow"))?;
    }

    Ok((quantity, notional))
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
    pub audit: schwab_rs::execution::OrderJournal,
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
        let mut effective = (*self.principal).clone();
        if effective.require_approval && matches!(name, "place_order" | "replace_order") {
            let digest = intent_digest(name, &args);
            if self
                .audit
                .is_approved(&effective.name, &digest)
                .await
                .map_err(|_| McpError::internal("Approval store unavailable"))?
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
            Some(self.audit.begin_once(&audit_scope, name, serde_json::json!({"principal":self.principal.name,"intentDigest":digest,"requestId":ctx.request_id(),"arguments":args}), dedup).await
                .map_err(|_| McpError::internal("Audit persistence failed, approval already consumed, or an earlier operation requires reconciliation"))?)
        } else {
            None
        };
        let result = self.inner.call_tool(name, args, ctx).await;
        if let Some(id) = audit_id {
            // The SDK journal owns broker reconciliation. Keep dispatch outcome
            // attribution separately without claiming a tool success is a fill.
            let outcome = schwab_rs::execution::SubmissionOutcome::Reconciled {
                evidence: if result.is_ok() {
                    "MCP dispatch completed; consult SDK submission journal".into()
                } else {
                    "MCP dispatch returned error; consult SDK submission journal".into()
                },
            };
            self.audit.finish(&id, outcome).await.map_err(|_| {
                McpError::internal(
                    "Execution may have completed; audit commit failed; reconcile before retrying",
                )
            })?;
        }
        result
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
    #[test]
    fn account_aggregate_activity_and_tool_permissions_fail_closed() {
        let p = policy();
        assert!(
            p.authorize("get_account", &serde_json::json!({"account_hash":"A"}))
                .is_ok()
        );
        assert!(
            p.authorize("get_account", &serde_json::json!({"account_hash":"B"}))
                .is_err()
        );
        assert!(p.authorize("get_accounts", &serde_json::json!({})).is_err());
        assert!(
            p.authorize(
                "stream_snapshot",
                &serde_json::json!({"service":"ACCT_ACTIVITY"})
            )
            .is_err()
        );
        assert!(
            p.authorize("replace_order", &serde_json::json!({"account_hash":"A"}))
                .is_err()
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
        assert!(p.authorize("place_order", &args).is_err());
        p.approved_intents
            .insert(intent_digest("place_order", &args));
        p.authorize("place_order", &args).unwrap();
        args["order"]["price"] = "50.0000000000000000000000001".into();
        p.approved_intents
            .insert(intent_digest("place_order", &args));
        assert!(p.authorize("place_order", &args).is_err());
    }
}
