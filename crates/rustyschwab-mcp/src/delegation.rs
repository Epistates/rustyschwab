//! How much a principal may do without an operator approving each order,
//! as the operator sets it at run time (from `rustyschwab-dash`, say),
//! within what the policy file allows.
//!
//! - **Modes.** [`Mode::Supervised`]: every order needs approval.
//!   [`Mode::Threshold`]: only an order whose bounded notional is over the
//!   amount does. [`Mode::Autonomous`]: none does. Every other policy and
//!   risk limit applies in every mode; delegation only decides approval.
//! - **The policy's own mode** comes from `require_approval` and
//!   `approval_threshold` ([`PrincipalPolicy::approval_mode`]), and applies
//!   whenever no grant is in force.
//! - **The ceiling.** A grant may be any mode up to the policy's
//!   `max_delegation`, which defaults to the policy's own mode: tightening is
//!   always allowed, widening only as far as the policy file says. A grant
//!   that a reloaded policy no longer allows is ignored.
//! - **Grants** last until a time, or until cancelled (`until` absent). They
//!   are kept in the principal's journal as its `delegation` setting, so
//!   every process serving the principal sees the same one, and each
//!   change is audited as `setting_changed` under the operator who made it.

use rust_decimal::Decimal;
use rustyschwab::execution::OrderJournal;
use serde::{Deserialize, Serialize};

use crate::policy::PrincipalPolicy;

/// The journal setting a grant is kept under.
pub const SETTING: &str = "delegation";

/// When an order needs an operator's approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Mode {
    /// Every order.
    Supervised,
    /// An order whose bounded notional is over `above`.
    Threshold { above: Decimal },
    /// None.
    Autonomous,
}

impl Mode {
    /// How much this mode lets through unapproved: `None` for unbounded.
    fn reach(&self) -> Option<Decimal> {
        match self {
            Self::Supervised => Some(Decimal::ZERO),
            Self::Threshold { above } => Some(*above),
            Self::Autonomous => None,
        }
    }
    /// Whether this mode lets through no more than `ceiling` does.
    pub fn within(&self, ceiling: &Mode) -> bool {
        match (self.reach(), ceiling.reach()) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some(mine), Some(limit)) => mine <= limit,
        }
    }
    /// Whether an order of `notional` (`None` when it could not be bounded)
    /// needs approval under this mode.
    pub fn needs_approval(&self, notional: Option<Decimal>) -> bool {
        match self {
            Self::Supervised => true,
            Self::Threshold { above } => notional.is_none_or(|n| n > *above),
            Self::Autonomous => false,
        }
    }
    fn validate(&self) -> anyhow::Result<()> {
        if let Self::Threshold { above } = self {
            anyhow::ensure!(*above > Decimal::ZERO, "a threshold must be positive");
        }
        Ok(())
    }
}

/// An operator's delegation to a principal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    pub mode: Mode,
    /// Unix milliseconds when it lapses; absent, it holds until cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    /// Who granted it.
    pub operator: String,
    /// Unix milliseconds.
    pub granted_at: i64,
}

impl Grant {
    /// Whether it is still in force at `now` (Unix milliseconds).
    pub fn active(&self, now: i64) -> bool {
        self.until.is_none_or(|until| now < until)
    }
}

/// The grant stored for the principal of `journal`, lapsed or not.
pub async fn stored(journal: &OrderJournal) -> rustyschwab::Result<Option<Grant>> {
    Ok(journal
        .setting(SETTING)
        .await?
        .and_then(|value| serde_json::from_value(value).ok()))
}

/// Grant `principal` `mode` until `until` (Unix milliseconds; `None`, until
/// cancelled), recorded under `operator`. Refused when the mode is wider
/// than the policy's ceiling, or `until` is already past.
pub async fn grant(
    journal: &OrderJournal,
    principal: &PrincipalPolicy,
    mode: Mode,
    until: Option<i64>,
    operator: &str,
) -> anyhow::Result<Grant> {
    mode.validate()?;
    let ceiling = principal.delegation_ceiling();
    anyhow::ensure!(
        mode.within(&ceiling),
        "{} may be granted at most {ceiling:?} (the policy's max_delegation)",
        principal.name
    );
    let now = chrono::Utc::now().timestamp_millis();
    anyhow::ensure!(
        until.is_none_or(|until| until > now),
        "`until` is in the past"
    );
    anyhow::ensure!(!operator.trim().is_empty(), "an operator name is required");
    let grant = Grant {
        mode,
        until,
        operator: operator.to_owned(),
        granted_at: now,
    };
    journal
        .set_setting(SETTING, Some(serde_json::to_value(&grant)?), operator)
        .await?;
    Ok(grant)
}

/// Withdraw any grant, so the policy's own mode applies again.
pub async fn cancel(journal: &OrderJournal, operator: &str) -> rustyschwab::Result<()> {
    journal.set_setting(SETTING, None, operator).await
}

/// The mode in force for `principal` at `now`: an active grant the policy
/// still allows, else the policy's own.
pub fn effective(principal: &PrincipalPolicy, grant: Option<&Grant>, now: i64) -> Mode {
    match grant {
        Some(grant) if grant.active(now) && grant.mode.within(&principal.delegation_ceiling()) => {
            grant.mode.clone()
        }
        Some(grant) if grant.active(now) => {
            tracing::warn!(
                principal = %principal.name,
                "a delegation grant wider than the policy now allows is ignored"
            );
            principal.approval_mode()
        }
        _ => principal.approval_mode(),
    }
}

/// `principal` with the mode in force applied to its approval settings.
pub async fn apply(
    principal: &mut PrincipalPolicy,
    journal: &OrderJournal,
) -> rustyschwab::Result<()> {
    let grant = stored(journal).await?;
    let mode = effective(
        principal,
        grant.as_ref(),
        chrono::Utc::now().timestamp_millis(),
    );
    principal.set_approval_mode(&mode);
    Ok(())
}

pub(crate) fn validate_policy(principal: &PrincipalPolicy) -> anyhow::Result<()> {
    if let Some(above) = principal.approval_threshold {
        anyhow::ensure!(
            principal.require_approval,
            "approval_threshold needs require_approval"
        );
        anyhow::ensure!(above > Decimal::ZERO, "approval_threshold must be positive");
    }
    if let Some(ceiling) = &principal.max_delegation {
        ceiling.validate()?;
        anyhow::ensure!(
            principal.approval_mode().within(ceiling),
            "max_delegation is narrower than the policy's own approval mode"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn above(amount: i64) -> Mode {
        Mode::Threshold {
            above: Decimal::from(amount),
        }
    }

    #[test]
    fn modes_order_by_what_they_let_through() {
        assert!(Mode::Supervised.within(&above(100)));
        assert!(above(100).within(&above(500)));
        assert!(!above(500).within(&above(100)));
        assert!(above(500).within(&Mode::Autonomous));
        assert!(!Mode::Autonomous.within(&above(500)));
        assert!(Mode::Supervised.needs_approval(Some(Decimal::ONE)));
        assert!(!above(100).needs_approval(Some(Decimal::from(100))));
        assert!(above(100).needs_approval(Some(Decimal::from(101))));
        assert!(above(100).needs_approval(None));
        assert!(!Mode::Autonomous.needs_approval(None));
    }

    #[test]
    fn a_policy_cannot_set_a_ceiling_below_its_own_mode() {
        let policy = |extra: serde_json::Value| -> anyhow::Result<PrincipalPolicy> {
            let mut value = serde_json::json!({"name": "desk", "token_sha256": "a".repeat(64),
                "tools": ["place_order"], "accounts": ["A"], "max_quantity": "1",
                "max_notional": "100", "require_approval": true});
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let policy: PrincipalPolicy = serde_json::from_value(value)?;
            policy.validate()?;
            Ok(policy)
        };
        let threshold = policy(serde_json::json!({"approval_threshold": "500"})).unwrap();
        assert_eq!(threshold.approval_mode(), above(500));
        assert_eq!(threshold.delegation_ceiling(), above(500));
        assert!(policy(serde_json::json!({"approval_threshold": "0"})).is_err());
        assert!(
            policy(serde_json::json!({"require_approval": false, "approval_threshold": "5"}))
                .is_err()
        );
        assert!(
            policy(serde_json::json!({"approval_threshold": "500",
                "max_delegation": {"mode": "threshold", "above": "100"}}))
            .is_err()
        );
        let open = policy(serde_json::json!({"max_delegation": {"mode": "autonomous"}})).unwrap();
        assert_eq!(open.approval_mode(), Mode::Supervised);
        assert_eq!(open.delegation_ceiling(), Mode::Autonomous);
    }

    #[test]
    fn modes_read_and_write_as_tagged_objects() {
        assert_eq!(
            serde_json::to_value(above(2500)).unwrap(),
            serde_json::json!({"mode": "threshold", "above": "2500"})
        );
        let read: Mode = serde_json::from_value(serde_json::json!({"mode": "autonomous"})).unwrap();
        assert_eq!(read, Mode::Autonomous);
    }
}
