//! The most an option strategy can lose, held to expiration.
//!
//! A spread's legs on one underlying and one expiration have a payoff at
//! expiration that is piecewise linear in the underlying's price, with its
//! kinks at the strikes. Its worst case is therefore at a price of zero, at
//! a strike, or, when more calls are sold than bought, without bound as the
//! price rises. The net premium is paid (`NET_DEBIT`) or received
//! (`NET_CREDIT`) once per spread unit: the greatest common divisor of the
//! leg quantities, since a 1×2 ratio of five units is legs of 5 and 10.
//!
//! This judges the order as a position opened at its limit and held to
//! expiration. It does not model early assignment (an American short leg
//! assigned while its long hedge is not exercised, which the hedge still
//! bounds), dividends, or legs at different expirations, which need a
//! pricing model and are refused.
use super::{OptionSymbol, OrderRequest, OrderType};
use crate::common::{AssetType, PutCall};
use rust_decimal::Decimal;

/// What [`defined_risk`] finds an option strategy can lose.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DefinedRisk {
    /// The legs' common underlying.
    pub underlying: String,
    /// Contracts across every leg.
    pub contracts: Decimal,
    /// The most the strategy can lose at expiration, premium included,
    /// in the account's currency; never negative.
    pub max_loss: Decimal,
}

/// The most `order`, a strategy of option legs on one underlying and one
/// expiration priced `NET_DEBIT`, `NET_CREDIT` or `NET_ZERO`, can lose held
/// to expiration. `multiplier` gives each leg's deliverable per contract
/// (100 for a standard contract); every leg must have the same one.
///
/// Refused, with the reason: a leg that is not an option or not in OSI
/// form, mixed underlyings or expirations, fractional or mixed
/// multipliers, a missing or non-positive net price, an instruction that
/// neither buys nor sells, and a strategy whose loss has no bound.
pub fn defined_risk(
    order: &OrderRequest,
    multiplier: impl Fn(&str) -> Option<Decimal>,
) -> Result<DefinedRisk, String> {
    let legs = &order.order_leg_collection;
    if legs.is_empty() {
        return Err("A strategy needs legs".into());
    }
    let mut parsed = Vec::with_capacity(legs.len());
    let mut contract = None;
    for leg in legs {
        if leg.instrument.asset_type != AssetType::Option {
            return Err("Defined risk is computed for option legs only".into());
        }
        let symbol: OptionSymbol = leg.instrument.symbol.parse()?;
        let size = multiplier(&leg.instrument.symbol)
            .filter(|m| *m > Decimal::ZERO)
            .ok_or("Option contract requires an operator-verified multiplier")?;
        if *contract.get_or_insert(size) != size {
            return Err("A strategy's legs must share one contract multiplier".into());
        }
        let sign = match leg.instruction.buys() {
            Some(true) => Decimal::ONE,
            Some(false) => Decimal::NEGATIVE_ONE,
            None => return Err("A leg must buy or sell".into()),
        };
        if leg.quantity <= Decimal::ZERO || !leg.quantity.fract().is_zero() {
            return Err("Leg quantities must be whole contracts".into());
        }
        parsed.push((symbol, sign, leg.quantity));
    }
    let (first, _, _) = &parsed[0];
    if parsed
        .iter()
        .any(|(s, _, _)| s.underlying() != first.underlying())
    {
        return Err("A strategy's legs must share one underlying".into());
    }
    if parsed
        .iter()
        .any(|(s, _, _)| s.expiration() != first.expiration())
    {
        return Err("Legs at different expirations need a pricing model to bound".into());
    }
    let multiplier = contract.unwrap_or(Decimal::ONE);
    let overflow = || "Strategy arithmetic overflow".to_owned();

    // Net premium: positive when paid.
    let units = parsed
        .iter()
        .map(|(_, _, q)| q.trunc())
        .reduce(gcd)
        .unwrap_or(Decimal::ONE);
    let per_unit = match order.order_type {
        Some(OrderType::NetZero) => Decimal::ZERO,
        Some(kind @ (OrderType::NetDebit | OrderType::NetCredit)) => {
            let price = order
                .price
                .filter(|p| *p > Decimal::ZERO)
                .ok_or("A net-priced strategy needs a positive price")?;
            if kind == OrderType::NetDebit {
                price
            } else {
                -price
            }
        }
        _ => return Err("A strategy is priced NET_DEBIT, NET_CREDIT or NET_ZERO".into()),
    };
    let premium = per_unit
        .checked_mul(units)
        .and_then(|v| v.checked_mul(multiplier))
        .ok_or_else(overflow)?;

    // Selling more calls than are bought loses without bound as the price rises.
    let call_slope: Decimal = parsed
        .iter()
        .filter(|(s, _, _)| s.put_call() == PutCall::Call)
        .map(|(_, sign, q)| sign * q)
        .sum();
    if call_slope < Decimal::ZERO {
        return Err("The strategy sells more calls than it buys; its loss has no bound".into());
    }

    // The payoff at expiration, at zero and at every strike.
    let payoff = |price: Decimal| -> Result<Decimal, String> {
        parsed.iter().try_fold(Decimal::ZERO, |sum, (s, sign, q)| {
            let intrinsic = match s.put_call() {
                PutCall::Call => (price - s.strike()).max(Decimal::ZERO),
                PutCall::Put => (s.strike() - price).max(Decimal::ZERO),
            };
            intrinsic
                .checked_mul(*q)
                .and_then(|v| v.checked_mul(multiplier))
                .and_then(|v| sum.checked_add(*sign * v))
                .ok_or_else(overflow)
        })
    };
    let mut worst = Decimal::ZERO;
    for price in std::iter::once(Decimal::ZERO).chain(parsed.iter().map(|(s, _, _)| s.strike())) {
        let loss = premium.checked_sub(payoff(price)?).ok_or_else(overflow)?;
        worst = worst.max(loss);
    }
    Ok(DefinedRisk {
        underlying: first.underlying().to_owned(),
        contracts: parsed.iter().map(|(_, _, q)| *q).sum(),
        max_loss: worst,
    })
}

fn gcd(a: Decimal, b: Decimal) -> Decimal {
    let (mut a, mut b) = (a.abs(), b.abs());
    while !b.is_zero() {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spread(kind: &str, price: Option<&str>, legs: &[(&str, &str, u32)]) -> OrderRequest {
        let legs: Vec<_> = legs
            .iter()
            .map(|(instruction, symbol, quantity)| {
                json!({"instruction": instruction, "quantity": quantity,
                    "instrument": {"symbol": symbol, "assetType": "OPTION"}})
            })
            .collect();
        let mut order = json!({"orderStrategyType": "SINGLE", "orderType": kind,
            "session": "NORMAL", "duration": "DAY", "orderLegCollection": legs});
        if let Some(price) = price {
            order["price"] = json!(price);
        }
        serde_json::from_value(order).unwrap()
    }
    const C100: &str = "XYZ   260116C00100000";
    const C105: &str = "XYZ   260116C00105000";
    const C110: &str = "XYZ   260116C00110000";
    const P90: &str = "XYZ   260116P00090000";
    const P95: &str = "XYZ   260116P00095000";
    fn hundred(_: &str) -> Option<Decimal> {
        Some(Decimal::from(100))
    }
    fn loss(order: &OrderRequest) -> Decimal {
        defined_risk(order, hundred).unwrap().max_loss
    }

    #[test]
    fn a_debit_vertical_risks_its_debit() {
        let bull = spread(
            "NET_DEBIT",
            Some("2"),
            &[("BUY_TO_OPEN", C100, 3), ("SELL_TO_OPEN", C105, 3)],
        );
        let risk = defined_risk(&bull, hundred).unwrap();
        assert_eq!(risk.max_loss, Decimal::from(600));
        assert_eq!(risk.contracts, Decimal::from(6));
        assert_eq!(risk.underlying, "XYZ");
    }

    #[test]
    fn a_credit_vertical_risks_its_width_less_the_credit() {
        let bear = spread(
            "NET_CREDIT",
            Some("1.5"),
            &[("SELL_TO_OPEN", C100, 2), ("BUY_TO_OPEN", C105, 2)],
        );
        // (5 - 1.5) × 100 × 2
        assert_eq!(loss(&bear), Decimal::from(700));
        let puts = spread(
            "NET_CREDIT",
            Some("1"),
            &[("SELL_TO_OPEN", P95, 1), ("BUY_TO_OPEN", P90, 1)],
        );
        assert_eq!(loss(&puts), Decimal::from(400));
    }

    #[test]
    fn an_iron_condor_risks_its_wider_wing_less_the_credit() {
        let condor = spread(
            "NET_CREDIT",
            Some("1.2"),
            &[
                ("BUY_TO_OPEN", P90, 1),
                ("SELL_TO_OPEN", P95, 1),
                ("SELL_TO_OPEN", C105, 1),
                ("BUY_TO_OPEN", C110, 1),
            ],
        );
        assert_eq!(loss(&condor), Decimal::from(380));
    }

    #[test]
    fn a_butterfly_and_a_ratio_count_premium_per_unit() {
        let fly = spread(
            "NET_DEBIT",
            Some("0.8"),
            &[
                ("BUY_TO_OPEN", C100, 1),
                ("SELL_TO_OPEN", C105, 2),
                ("BUY_TO_OPEN", C110, 1),
            ],
        );
        assert_eq!(loss(&fly), Decimal::from(80));
        // Five units of buy one 100 call, sell two 105 calls, buy one 110
        // call: legs of 5, 10, 5.
        let five = spread(
            "NET_DEBIT",
            Some("0.8"),
            &[
                ("BUY_TO_OPEN", C100, 5),
                ("SELL_TO_OPEN", C105, 10),
                ("BUY_TO_OPEN", C110, 5),
            ],
        );
        assert_eq!(loss(&five), Decimal::from(400));
        // A credit received at zero cost to close still never risks less
        // than nothing.
        let free = spread(
            "NET_CREDIT",
            Some("10"),
            &[("SELL_TO_OPEN", C100, 1), ("BUY_TO_OPEN", C105, 1)],
        );
        assert_eq!(loss(&free), Decimal::ZERO);
        // A put ratio's loss is bounded at a price of zero: owning the 95
        // put and short two 90 puts loses 2 × 90 - 95 = 85 a share, less
        // the 0.50 credit.
        let ratio = spread(
            "NET_CREDIT",
            Some("0.5"),
            &[("BUY_TO_OPEN", P95, 1), ("SELL_TO_OPEN", P90, 2)],
        );
        assert_eq!(loss(&ratio), Decimal::from(8450));
    }

    #[test]
    fn unbounded_or_unpriceable_strategies_are_refused() {
        let naked = spread(
            "NET_CREDIT",
            Some("1"),
            &[("SELL_TO_OPEN", C100, 2), ("BUY_TO_OPEN", C105, 1)],
        );
        assert!(
            defined_risk(&naked, hundred)
                .unwrap_err()
                .contains("no bound")
        );
        let calendar = spread(
            "NET_DEBIT",
            Some("1"),
            &[
                ("SELL_TO_OPEN", C100, 1),
                ("BUY_TO_OPEN", "XYZ   260220C00100000", 1),
            ],
        );
        assert!(
            defined_risk(&calendar, hundred)
                .unwrap_err()
                .contains("different expirations")
        );
        let unpriced = spread(
            "NET_DEBIT",
            None,
            &[("BUY_TO_OPEN", C100, 1), ("SELL_TO_OPEN", C105, 1)],
        );
        assert!(defined_risk(&unpriced, hundred).is_err());
        let mixed = |symbol: &str| {
            (symbol == C105)
                .then(|| Decimal::from(10))
                .or(Some(Decimal::from(100)))
        };
        let vertical = spread(
            "NET_DEBIT",
            Some("1"),
            &[("BUY_TO_OPEN", C100, 1), ("SELL_TO_OPEN", C105, 1)],
        );
        assert!(defined_risk(&vertical, mixed).is_err());
        assert!(defined_risk(&vertical, |_| None).is_err());
    }
}
