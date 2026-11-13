//! Immutable per-principal authorization. Network authentication selects a
//! principal-specific handler and session store before MCP dispatch.
//!
//! Denials are business outcomes, so they are returned as `isError` tool
//! results in the [`crate::contract`] shape rather than JSON-RPC errors: the
//! model reads `not_permitted`, `risk_denied` or `approval_required` and
//! knows whether to change the order, wait for the operator, or stop.
use crate::contract::{FailureCode, ToolFailure};
use crate::dead_man;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, time::Duration};

#[derive(Debug, Clone, Deserialize, Serialize)]
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
    /// With `require_approval`, ask the MCP client's user to approve each
    /// order (an MCP elicitation) before falling back to an operator. The
    /// operator delegates approval to whoever answers the client's prompts:
    /// right for a person at a desktop client, wrong for an unattended agent.
    #[serde(default)]
    pub client_approval: bool,
    /// Tool calls this principal may make per minute (bursting to a tenth
    /// of it); absent, unlimited. Every principal shares one Schwab request
    /// budget, so this keeps one from crowding out the others.
    #[serde(default)]
    pub max_calls_per_minute: Option<std::num::NonZeroU32>,
    /// Symbols this principal may trade; empty, any. An option matches by
    /// its underlying or its own symbol (see [`crate::guardrails`]).
    #[serde(default)]
    pub allowed_symbols: HashSet<String>,
    /// Symbols this principal may not trade, over any allowance.
    #[serde(default)]
    pub denied_symbols: HashSet<String>,
    /// Refuse a limit order priced more than this many basis points through
    /// the far side of the market (see [`crate::guardrails`]). Required for
    /// a principal that can place or replace orders: the notional caps value
    /// an order at its limit, which only the collar keeps near the market.
    #[serde(default)]
    pub price_collar_bps: Option<std::num::NonZeroU32>,
    /// Submissions this principal may start in a New York trading day.
    #[serde(default)]
    pub max_daily_orders: Option<std::num::NonZeroU32>,
    /// Bounded notional this principal may submit in a trading day.
    #[serde(default)]
    pub max_daily_notional: Option<rust_decimal::Decimal>,
    /// Dead-man switch: after this many seconds with no tool call by the
    /// principal, the server cancels the working orders it placed (see
    /// [`crate::dead_man`]). Only for a principal that can place or replace
    /// orders.
    #[serde(default)]
    pub dead_man_secs: Option<std::num::NonZeroU32>,
    /// With `require_approval`, only an order whose bounded notional is
    /// over this amount needs approval (see [`crate::delegation`]).
    #[serde(default)]
    pub approval_threshold: Option<rust_decimal::Decimal>,
    /// The widest approval mode an operator may delegate to this principal
    /// at run time; absent, the policy's own mode, so the operator can only
    /// tighten it (see [`crate::delegation`]).
    #[serde(default)]
    pub max_delegation: Option<crate::delegation::Mode>,
}
impl PrincipalPolicy {
    /// The approval mode this policy sets on its own, before any grant.
    pub fn approval_mode(&self) -> crate::delegation::Mode {
        use crate::delegation::Mode;
        match (self.require_approval, self.approval_threshold) {
            (false, _) => Mode::Autonomous,
            (true, None) => Mode::Supervised,
            (true, Some(above)) => Mode::Threshold { above },
        }
    }
    /// The widest mode an operator may grant: `max_delegation`, or the
    /// policy's own mode.
    pub fn delegation_ceiling(&self) -> crate::delegation::Mode {
        self.max_delegation
            .clone()
            .unwrap_or_else(|| self.approval_mode())
    }
    /// Set the approval settings to `mode`, for a grant in force.
    pub fn set_approval_mode(&mut self, mode: &crate::delegation::Mode) {
        use crate::delegation::Mode;
        (self.require_approval, self.approval_threshold) = match mode {
            Mode::Supervised => (true, None),
            Mode::Threshold { above } => (true, Some(*above)),
            Mode::Autonomous => (false, None),
        };
    }
    /// Whether submitting `name` with `args` needs an operator's approval
    /// under this policy (granted or not): what decides whether the
    /// submission consumes one.
    pub fn approval_applies(&self, name: &str, args: &Value) -> bool {
        matches!(name, "place_order" | "replace_order")
            && self.require_approval
            && self
                .approval_mode()
                .needs_approval(self.bounded_notional(args))
    }
    /// The order's bounded notional, when it can be computed.
    fn bounded_notional(&self, args: &Value) -> Option<rust_decimal::Decimal> {
        let order = serde_json::from_value(args.get("order")?.clone()).ok()?;
        exposure(&order, &self.option_multipliers)
            .ok()
            .map(|(_, notional)| notional)
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        crate::delegation::validate_policy(self)?;
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
            // A sell's limit only floors its proceeds, so without a collar a
            // sell of the whole position at a penny passes every notional cap.
            anyhow::ensure!(
                self.price_collar_bps.is_some(),
                "Trading principals require price_collar_bps, the most a limit order \
                 may be priced through the market, in basis points (500 is 5%)"
            );
        }
        anyhow::ensure!(
            !self.client_approval || self.require_approval,
            "client_approval needs require_approval"
        );
        crate::guardrails::validate_symbols("allowed_symbols", &self.allowed_symbols)?;
        crate::guardrails::validate_symbols("denied_symbols", &self.denied_symbols)?;
        anyhow::ensure!(
            self.price_collar_bps.is_none_or(|bps| bps.get() < 10_000),
            "price_collar_bps must be under 10000 (100%)"
        );
        anyhow::ensure!(
            self.max_daily_notional
                .is_none_or(|v| v > rust_decimal::Decimal::ZERO),
            "max_daily_notional must be positive"
        );
        if let Some(secs) = self.dead_man_secs {
            anyhow::ensure!(
                self.tools.contains("place_order") || self.tools.contains("replace_order"),
                "dead_man_secs needs place_order or replace_order"
            );
            anyhow::ensure!(
                (dead_man::MIN_WINDOW_SECS..=dead_man::MAX_WINDOW_SECS).contains(&secs.get()),
                "dead_man_secs must be {}-{} seconds, got {secs}",
                dead_man::MIN_WINDOW_SECS,
                dead_man::MAX_WINDOW_SECS
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
            if let Some(denial) = crate::guardrails::symbol_denial(self, &order) {
                review.denials.push(denial);
            }
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
            let notional = review.exposure.map(|(_, notional)| notional);
            if self.require_approval
                && self.approval_mode().needs_approval(notional)
                && !self.approved_intents.contains(&digest)
            {
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
) -> Result<rustyschwab_types::trading::OrderRequest, ToolFailure> {
    let order: rustyschwab_types::trading::OrderRequest =
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
// A multi-leg option strategy on one underlying and expiration counts its
// contracts and the most it can lose at expiration (`defined_risk`); one
// whose loss has no bound is refused.
// The one exception is a bracket exit: a child that only closes the long
// position its bounded parent opens (same instrument, no more than the
// parent's quantity) cannot fill for more than the parent paid, so an
// unbounded stop there is priced at the parent's limit.
pub(crate) fn exposure(
    order: &rustyschwab_types::trading::OrderRequest,
    multipliers: &std::collections::HashMap<String, rust_decimal::Decimal>,
) -> Result<(rust_decimal::Decimal, rust_decimal::Decimal), String> {
    exposure_under(order, multipliers, None)
}

/// `entry` is the bounded long entry this order hangs under as a triggered
/// child, if any. OCO wrappers carry no legs and pass it through.
fn exposure_under(
    order: &rustyschwab_types::trading::OrderRequest,
    multipliers: &std::collections::HashMap<String, rust_decimal::Decimal>,
    entry: Option<(
        &rustyschwab_types::trading::OrderRequestLeg,
        rust_decimal::Decimal,
    )>,
) -> Result<(rust_decimal::Decimal, rust_decimal::Decimal), String> {
    use rustyschwab_types::trading::{OrderStrategyType, OrderType};
    let mut quantity = rust_decimal::Decimal::ZERO;
    let mut notional = rust_decimal::Decimal::ZERO;
    let mut child_entry = entry;
    if order.order_strategy_type != OrderStrategyType::Oco && order.order_leg_collection.len() > 1 {
        // An option strategy is bounded by what it can lose at expiration.
        let risk = rustyschwab_types::trading::defined_risk(order, |symbol| {
            multipliers.get(symbol).copied()
        })
        .map_err(|refused| refused.to_string())?;
        quantity = quantity
            .checked_add(risk.contracts)
            .ok_or("Quantity overflow")?;
        notional = notional
            .checked_add(risk.max_loss)
            .ok_or("Notional overflow")?;
        child_entry = None;
    } else if order.order_strategy_type != OrderStrategyType::Oco {
        use rustyschwab_types::{common::AssetType, trading::OrderInstruction};
        let [leg] = order.order_leg_collection.as_slice() else {
            return Err("An order needs a leg".into());
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
            // A stop-limit counts at the higher of its prices: a sell's
            // limit only floors what it fills at, and it triggers near its
            // stop.
            (true, Some(price)) => order.stop_price.map_or(price, |stop| price.max(stop)),
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
    leg: &rustyschwab_types::trading::OrderRequestLeg,
    entry: &rustyschwab_types::trading::OrderRequestLeg,
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
            serde_json::from_value::<rustyschwab_types::trading::OrderRequest>(order.clone())
        && let Ok(wire) = serde_json::to_value(order)
    {
        args["order"] = wire;
    }
    digest(
        serde_json::to_vec(&(name, rustyschwab::utils::canonical_json(&args)))
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

/// Who the approval store records as granting an approval the client's
/// user gave (see [`PrincipalPolicy::client_approval`]), before the
/// principal's name. The authority gateway honors approvals by configured
/// operators only, so it never honors one of these.
pub const CLIENT_APPROVER_PREFIX: &str = "client:";

/// How long an approval the client's user gave lasts: long enough for the
/// submission it was asked for, which follows at once.
pub const CLIENT_APPROVAL_SECS: i64 = 60;

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

/// The MCP handler that holds a principal to its policy.
pub use crate::authorized::AuthorizedServer;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn policy() -> PrincipalPolicy {
        serde_json::from_value(serde_json::json!({"name":"desk","token_sha256":"a".repeat(64),"tools":["get_account","get_accounts","check_order","place_order","cancel_order","stream_snapshot"],"accounts":["A"],"max_quantity":"2","max_notional":"100","require_approval":true})).unwrap()
    }
    pub(crate) fn order(quantity: Value) -> Value {
        serde_json::json!({"orderStrategyType":"SINGLE","orderType":"LIMIT","session":"NORMAL","duration":"DAY","price":"50","orderLegCollection":[{"quantity":quantity,"instruction":"BUY","instrument":{"symbol":"AAPL","assetType":"EQUITY"}}]})
    }
    /// A limit-on-close order has a firm price, so it is bounded exposure.
    #[test]
    fn limit_on_close_orders_are_bounded() {
        let mut close = order("2".into());
        close["orderType"] = "LIMIT_ON_CLOSE".into();
        let close: rustyschwab_types::trading::OrderRequest =
            serde_json::from_value(close).unwrap();
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
        use rustyschwab_types::trading::{OrderInstruction, OrderRequest, OrderRequestLeg};
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
    /// A stop-limit counts at the higher of its prices, so a sell stop at 45
    /// with a penny limit counts what it sells near 45, not a penny; a buy's
    /// limit is already its ceiling.
    #[test]
    fn a_stop_limit_counts_at_its_higher_price() {
        use rust_decimal::Decimal;
        use rustyschwab_types::trading::{OrderInstruction, OrderRequest, OrderRequestLeg};
        let d = |s: &str| s.parse::<Decimal>().unwrap();
        let none = Default::default();
        let leg = |instruction| OrderRequestLeg::equity(instruction, "AAPL", 10);
        let sell = OrderRequest::stop_limit(leg(OrderInstruction::Sell), d("45"), d("0.01"));
        assert_eq!(exposure(&sell, &none).unwrap(), (d("10"), d("450")));
        let buy = OrderRequest::stop_limit(leg(OrderInstruction::Buy), d("55"), d("56"));
        assert_eq!(exposure(&buy, &none).unwrap(), (d("10"), d("560")));
    }
    /// A defined-risk option strategy counts its contracts and the most it
    /// can lose; one with no bound is refused.
    #[test]
    fn option_strategies_are_bounded_by_their_defined_risk() {
        const C100: &str = "XYZ   260116C00100000";
        const C105: &str = "XYZ   260116C00105000";
        let multipliers = std::collections::HashMap::from([
            (C100.to_owned(), rust_decimal::Decimal::from(100)),
            (C105.to_owned(), rust_decimal::Decimal::from(100)),
        ]);
        let strategy = |kind: &str, short: u32| -> rustyschwab_types::trading::OrderRequest {
            serde_json::from_value(serde_json::json!({"orderStrategyType": "SINGLE",
                "orderType": kind, "session": "NORMAL", "duration": "DAY", "price": "1.5",
                "orderLegCollection": [
                    {"quantity": 2, "instruction": "SELL_TO_OPEN",
                     "instrument": {"symbol": C100, "assetType": "OPTION"}},
                    {"quantity": short, "instruction": "BUY_TO_OPEN",
                     "instrument": {"symbol": C105, "assetType": "OPTION"}}]}))
            .unwrap()
        };
        assert_eq!(
            exposure(&strategy("NET_CREDIT", 2), &multipliers).unwrap(),
            (
                rust_decimal::Decimal::from(4),
                rust_decimal::Decimal::from(700)
            )
        );
        let naked = exposure(&strategy("NET_CREDIT", 1), &multipliers).unwrap_err();
        assert!(naked.contains("no bound"), "{naked}");
        assert!(exposure(&strategy("NET_CREDIT", 2), &Default::default()).is_err());
    }

    /// The dead-man window is bounded, and only a principal that can place
    /// or replace orders has orders to cancel.
    #[test]
    fn the_dead_man_window_is_validated() {
        let with = |tools: Value, secs: Value| {
            let mut policy = serde_json::to_value(policy()).unwrap();
            policy["price_collar_bps"] = 500.into();
            policy["tools"] = tools;
            policy["dead_man_secs"] = secs;
            serde_json::from_value::<PrincipalPolicy>(policy)
                .map_err(anyhow::Error::from)
                .and_then(|policy| policy.validate())
        };
        let trader = serde_json::json!(["place_order"]);
        assert!(with(trader.clone(), serde_json::json!(null)).is_ok());
        for secs in [dead_man::MIN_WINDOW_SECS, 600, dead_man::MAX_WINDOW_SECS] {
            assert!(with(trader.clone(), secs.into()).is_ok(), "{secs}");
        }
        for secs in [
            1,
            dead_man::MIN_WINDOW_SECS - 1,
            dead_man::MAX_WINDOW_SECS + 1,
        ] {
            let refused = with(trader.clone(), secs.into()).unwrap_err().to_string();
            assert!(refused.contains("dead_man_secs must be"), "{refused}");
        }
        assert!(
            with(trader.clone(), 0.into()).is_err(),
            "zero is not a window"
        );
        assert!(with(trader, (-5).into()).is_err());
        assert!(with(serde_json::json!(["replace_order"]), 60.into()).is_ok());
        let refused = with(serde_json::json!(["get_account"]), 60.into())
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("needs place_order or replace_order"),
            "{refused}"
        );
    }

    #[test]
    fn guardrail_settings_are_validated() {
        let with = |field: &str, value: Value| {
            let mut policy = serde_json::to_value(policy()).unwrap();
            policy["price_collar_bps"] = 500.into();
            policy[field] = value;
            serde_json::from_value::<PrincipalPolicy>(policy)
                .unwrap()
                .validate()
        };
        assert!(with("allowed_symbols", serde_json::json!(["AAPL"])).is_ok());
        assert!(with("denied_symbols", serde_json::json!(["aapl"])).is_err());
        assert!(with("price_collar_bps", serde_json::json!(500)).is_ok());
        assert!(with("price_collar_bps", serde_json::json!(10_000)).is_err());
        assert!(with("max_daily_notional", serde_json::json!("0")).is_err());
        assert!(with("max_daily_notional", serde_json::json!("5000")).is_ok());
        assert!(with("client_approval", serde_json::json!(true)).is_ok());
        // A trader must set a collar; a principal that only reads need not.
        let uncollared = with("price_collar_bps", Value::Null)
            .unwrap_err()
            .to_string();
        assert!(uncollared.contains("price_collar_bps"), "{uncollared}");
        let mut reader = serde_json::to_value(policy()).unwrap();
        reader["tools"] = serde_json::json!(["get_account", "cancel_order"]);
        let reader: PrincipalPolicy = serde_json::from_value(reader).unwrap();
        assert!(reader.validate().is_ok(), "{:?}", reader.validate());
        let mut unapproved = serde_json::to_value(policy()).unwrap();
        unapproved["price_collar_bps"] = 500.into();
        unapproved["require_approval"] = false.into();
        unapproved["client_approval"] = true.into();
        let unapproved: PrincipalPolicy = serde_json::from_value(unapproved).unwrap();
        assert!(
            unapproved.validate().is_err(),
            "client approval needs approvals"
        );
    }
}
