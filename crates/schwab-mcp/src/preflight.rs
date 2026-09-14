//! Local pre-flight for an order intent: every check a `place_order` or
//! `replace_order` call would apply before contacting the venue, reported
//! together instead of as the first failure. Nothing is dispatched, audited
//! or approved. A duplicate intent is only detected by the submission
//! journal at dispatch, and every check is repeated then, so a verdict is
//! advisory.
use crate::backend::Backend;
use crate::contract::ToolFailure;
use crate::policy::{self, PrincipalPolicy};
use crate::server::Switches;
use rust_decimal::Decimal;
use schwab_types::trading::OrderRequest;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// What a submission of the intent would meet, as `check_order` reports it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Verdict {
    /// The submission would be dispatched to the venue.
    pub allowed: bool,
    /// Every failure the submission would return, in the order a dispatch
    /// would find them.
    pub reasons: Vec<ToolFailure>,
    /// Bounded exposure of the order, when it was valid and priced.
    pub exposure: Option<Exposure>,
    /// The principal's risk limits; absent without a principal policy.
    pub limits: Option<Limits>,
    pub requires_approval: bool,
    /// The intent digest is approved, whether by policy or durably.
    pub approved: bool,
    /// What an operator approves.
    pub intent_digest: String,
    /// The order tag the submission would carry.
    pub tag: String,
    pub trading_enabled: bool,
}

impl Verdict {
    /// The verdict as a tool's local payload.
    pub fn into_payload(self) -> Result<Value, ToolFailure> {
        serde_json::to_value(self).map_err(|e| ToolFailure::internal("Verdict serialization", e))
    }
}

#[derive(Debug, Serialize)]
pub struct Exposure {
    pub quantity: Decimal,
    pub notional: Decimal,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub max_quantity: Decimal,
    pub max_notional: Decimal,
}

/// `check_order` arguments, the shape of the tool's signature, so the
/// authorized transport extracts them from raw arguments exactly as the
/// `#[tool]` macro does: a malformed order is the caller's error, not a
/// verdict.
#[derive(Debug, Deserialize)]
pub struct Request {
    pub account_hash: String,
    pub order: OrderRequest,
    #[serde(default)]
    pub order_id: Option<String>,
}

impl Request {
    pub fn parse(args: Value) -> Result<Self, ToolFailure> {
        serde_json::from_value(args).map_err(|e| {
            ToolFailure::invalid_arguments(format!("Invalid check_order arguments: {e}"))
        })
    }

    /// The submission these arguments stand for, as its tool would receive
    /// it: `replace_order` of `order_id` when given, else `place_order`.
    pub fn intent(self) -> (&'static str, Value) {
        match self.order_id {
            Some(order_id) => (
                "replace_order",
                json!({
                    "account_hash": self.account_hash,
                    "order_id": order_id,
                    "order": self.order,
                }),
            ),
            None => (
                "place_order",
                json!({ "account_hash": self.account_hash, "order": self.order }),
            ),
        }
    }
}

/// Evaluate the intent `name` (`place_order` or `replace_order`) with
/// `args` under `policy`, or without one on the unauthorized transport.
/// `policy` must already carry any durable approvals.
pub async fn evaluate(
    backend: &Backend,
    policy: Option<&PrincipalPolicy>,
    switches: Switches,
    name: &str,
    args: &Value,
) -> Result<Verdict, ToolFailure> {
    let intent_digest = policy::intent_digest(name, args);
    let tag = args["order"]["tag"]
        .as_str()
        .map_or_else(|| policy::intent_tag(&intent_digest), str::to_owned);
    let mut reasons = Vec::new();
    let (exposure, limits, requires_approval, approved) = match policy {
        Some(policy) => {
            let review = policy.review(name, args);
            reasons.extend(review.denials);
            let limits = Limits {
                max_quantity: policy.max_quantity.unwrap_or_default(),
                max_notional: policy.max_notional.unwrap_or_default(),
            };
            let approved = policy.approved_intents.contains(&intent_digest);
            (
                review.exposure,
                Some(limits),
                policy.require_approval,
                approved,
            )
        }
        None => {
            let exposure = match policy::order_request(args) {
                Ok(order) => policy::exposure(&order, &Default::default()).ok(),
                Err(failure) => {
                    reasons.push(failure);
                    None
                }
            };
            (exposure, None, false, false)
        }
    };
    if let Err(failure) = switches.require(name) {
        reasons.push(failure);
    }
    if let Err(halted) = backend.submission_admission().await {
        reasons.push(halted);
    }
    Ok(Verdict {
        allowed: reasons.is_empty(),
        reasons,
        exposure: exposure.map(|(quantity, notional)| Exposure { quantity, notional }),
        limits,
        requires_approval,
        approved,
        intent_digest,
        tag,
        trading_enabled: switches.allows(name),
    })
}
