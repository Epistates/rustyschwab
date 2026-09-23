//! A compact JSON description of an order that expands to an
//! [`OrderRequest`] through the constructors, so JSON callers (the MCP
//! `build_order` tool, config files) get the same composition helpers as
//! Rust code without hand-nesting `childOrderStrategies`.

use super::{
    ComplexOrderStrategyType, OptionSymbol, OrderDuration, OrderInstruction, OrderRequest,
    OrderRequestInstrument, OrderRequestLeg, OrderSession, PositionEffect, PriceLinkBasis,
    PriceLinkType, RequestedDestination, SpecialInstruction, StopType, TaxLotMethod,
};
use crate::common::PutCall;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;

/// One order, keyed by `kind`. Single orders take a `leg` (or `legs` for
/// the net-priced multi-leg kinds) and optional `settings`; `oco`,
/// `trigger` and `bracket` nest other specs.
///
/// ```
/// use schwab_types::trading::{OrderSpec, OrderStrategyType};
///
/// let spec: OrderSpec = serde_json::from_value(serde_json::json!({
///     "kind": "bracket",
///     "entry": {
///         "kind": "limit",
///         "leg": {"instruction": "BUY", "quantity": "10",
///                 "instrument": {"asset_type": "EQUITY", "symbol": "AAPL"}},
///         "price": "150.25",
///         "settings": {"tag": "swing-1"}
///     },
///     "take_profit": "160",
///     "stop_loss": "145"
/// }))
/// .unwrap();
/// let order = spec.build().unwrap();
/// assert_eq!(order.order_strategy_type, OrderStrategyType::Trigger);
/// assert_eq!(order.child_order_strategies[0].child_order_strategies.len(), 2);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
// The rustdoc example above is for Rust readers; the schema gets prose.
#[cfg_attr(
    feature = "schemars",
    schemars(
        description = "One order, keyed by `kind`. Single orders take a `leg` (or `legs` \
                            for the net-priced multi-leg kinds) and optional `settings`; `oco`, \
                            `trigger` and `bracket` nest other specs. Quantities and prices are \
                            decimal strings."
    )
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OrderSpec {
    /// Fill at the market.
    Market {
        leg: LegSpec,
        #[serde(default)]
        settings: OrderSettings,
    },
    /// Fill at `price` or better.
    Limit {
        leg: LegSpec,
        #[serde(deserialize_with = "crate::common::exact_decimal::deserialize")]
        #[cfg_attr(feature = "schemars", schemars(with = "crate::common::DecimalString"))]
        price: Decimal,
        #[serde(default)]
        settings: OrderSettings,
    },
    /// Become a market order once `stop_price` trades.
    Stop {
        leg: LegSpec,
        #[serde(deserialize_with = "crate::common::exact_decimal::deserialize")]
        #[cfg_attr(feature = "schemars", schemars(with = "crate::common::DecimalString"))]
        stop_price: Decimal,
        #[serde(default)]
        settings: OrderSettings,
    },
    /// Become a limit order at `price` once `stop_price` trades.
    StopLimit {
        leg: LegSpec,
        #[serde(deserialize_with = "crate::common::exact_decimal::deserialize")]
        #[cfg_attr(feature = "schemars", schemars(with = "crate::common::DecimalString"))]
        stop_price: Decimal,
        #[serde(deserialize_with = "crate::common::exact_decimal::deserialize")]
        #[cfg_attr(feature = "schemars", schemars(with = "crate::common::DecimalString"))]
        price: Decimal,
        #[serde(default)]
        settings: OrderSettings,
    },
    /// A stop that trails `basis` by `offset`, expressed as `link`.
    TrailingStop {
        leg: LegSpec,
        basis: PriceLinkBasis,
        link: PriceLinkType,
        #[serde(deserialize_with = "crate::common::exact_decimal::deserialize")]
        #[cfg_attr(feature = "schemars", schemars(with = "crate::common::DecimalString"))]
        offset: Decimal,
        #[serde(default)]
        settings: OrderSettings,
    },
    /// Multi-leg order paying at most `price` net.
    NetDebit {
        legs: Vec<LegSpec>,
        #[serde(deserialize_with = "crate::common::exact_decimal::deserialize")]
        #[cfg_attr(feature = "schemars", schemars(with = "crate::common::DecimalString"))]
        price: Decimal,
        #[serde(default)]
        settings: OrderSettings,
    },
    /// Multi-leg order collecting at least `price` net.
    NetCredit {
        legs: Vec<LegSpec>,
        #[serde(deserialize_with = "crate::common::exact_decimal::deserialize")]
        #[cfg_attr(feature = "schemars", schemars(with = "crate::common::DecimalString"))]
        price: Decimal,
        #[serde(default)]
        settings: OrderSettings,
    },
    /// Multi-leg order that must fill for zero net cost.
    NetZero {
        legs: Vec<LegSpec>,
        #[serde(default)]
        settings: OrderSettings,
    },
    /// Two orders; a fill on either cancels the other.
    Oco {
        first: Box<OrderSpec>,
        second: Box<OrderSpec>,
    },
    /// `children` are submitted once `entry` fills.
    Trigger {
        entry: Box<OrderSpec>,
        children: Vec<OrderSpec>,
    },
    /// `entry` triggers a good-till-cancel take-profit limit and stop-loss
    /// pair that close the entry leg; see [`OrderRequest::bracket`].
    Bracket {
        entry: Box<OrderSpec>,
        #[serde(deserialize_with = "crate::common::exact_decimal::deserialize")]
        #[cfg_attr(feature = "schemars", schemars(with = "crate::common::DecimalString"))]
        take_profit: Decimal,
        #[serde(deserialize_with = "crate::common::exact_decimal::deserialize")]
        #[cfg_attr(feature = "schemars", schemars(with = "crate::common::DecimalString"))]
        stop_loss: Decimal,
    },
}

/// One leg of an order.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct LegSpec {
    pub instruction: OrderInstruction,
    /// Shares, or contracts for options.
    #[serde(deserialize_with = "crate::common::exact_decimal::deserialize")]
    #[cfg_attr(feature = "schemars", schemars(with = "crate::common::DecimalString"))]
    pub quantity: Decimal,
    pub instrument: InstrumentSpec,
    #[serde(default)]
    pub position_effect: Option<PositionEffect>,
}

/// What a leg trades, keyed by `asset_type`.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(
    tag = "asset_type",
    rename_all = "SCREAMING_SNAKE_CASE",
    deny_unknown_fields
)]
pub enum InstrumentSpec {
    Equity {
        symbol: String,
    },
    /// Either an OSI `symbol` (`AAPL  240517P00190000`) or the contract by
    /// parts: `underlying`, `expiration` (`YYYY-MM-DD`), `right` (`PUT` /
    /// `CALL`) and `strike`.
    Option {
        #[serde(default)]
        symbol: Option<String>,
        #[serde(default)]
        underlying: Option<String>,
        #[serde(default)]
        expiration: Option<NaiveDate>,
        #[serde(default)]
        right: Option<PutCall>,
        #[serde(
            default,
            deserialize_with = "crate::common::exact_decimal::option::deserialize"
        )]
        #[cfg_attr(
            feature = "schemars",
            schemars(with = "Option<crate::common::DecimalString>")
        )]
        strike: Option<Decimal>,
    },
}

/// Fields every single order accepts. `session` defaults to `NORMAL` and
/// `duration` to `DAY`.
#[skip_serializing_none]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case", deny_unknown_fields, default)]
pub struct OrderSettings {
    pub session: Option<OrderSession>,
    pub duration: Option<OrderDuration>,
    /// Caller's own identifier for the order.
    pub tag: Option<String>,
    pub complex_strategy: Option<ComplexOrderStrategyType>,
    pub special_instruction: Option<SpecialInstruction>,
    pub destination: Option<RequestedDestination>,
    pub tax_lot_method: Option<TaxLotMethod>,
    pub stop_type: Option<StopType>,
}

impl OrderSettings {
    fn apply(self, mut order: OrderRequest) -> OrderRequest {
        if let Some(session) = self.session {
            order = order.with_session(session);
        }
        if let Some(duration) = self.duration {
            order = order.with_duration(duration);
        }
        if let Some(tag) = self.tag {
            order = order.with_tag(tag);
        }
        if let Some(strategy) = self.complex_strategy {
            order = order.with_complex_strategy(strategy);
        }
        if let Some(instruction) = self.special_instruction {
            order = order.with_special_instruction(instruction);
        }
        if let Some(destination) = self.destination {
            order = order.with_destination(destination);
        }
        if let Some(method) = self.tax_lot_method {
            order = order.with_tax_lot_method(method);
        }
        if let Some(stop_type) = self.stop_type {
            order = order.with_stop_type(stop_type);
        }
        order
    }
}

impl InstrumentSpec {
    fn build(self) -> Result<OrderRequestInstrument, String> {
        match self {
            Self::Equity { symbol } => Ok(OrderRequestInstrument::equity(symbol)),
            Self::Option {
                symbol: Some(symbol),
                underlying: None,
                expiration: None,
                right: None,
                strike: None,
            } => {
                let parsed: OptionSymbol = symbol.parse()?;
                Ok(OrderRequestInstrument::option(parsed))
            }
            Self::Option {
                symbol: None,
                underlying: Some(underlying),
                expiration: Some(expiration),
                right: Some(right),
                strike: Some(strike),
            } => Ok(OrderRequestInstrument::option(OptionSymbol::new(
                underlying, expiration, right, strike,
            )?)),
            Self::Option { .. } => Err("An option instrument takes either an OSI symbol \
                 or all of underlying, expiration, right and strike"
                .into()),
        }
    }
}

impl LegSpec {
    fn build(self) -> Result<OrderRequestLeg, String> {
        let leg = OrderRequestLeg::new(self.instruction, self.instrument.build()?, self.quantity);
        Ok(match self.position_effect {
            Some(effect) => leg.with_position_effect(effect),
            None => leg,
        })
    }
}

fn build_legs(legs: Vec<LegSpec>) -> Result<Vec<OrderRequestLeg>, String> {
    legs.into_iter().map(LegSpec::build).collect()
}

impl OrderSpec {
    /// Expand into a validated [`OrderRequest`].
    pub fn build(self) -> Result<OrderRequest, String> {
        let order = self.expand()?;
        order.validate()?;
        Ok(order)
    }

    fn expand(self) -> Result<OrderRequest, String> {
        Ok(match self {
            Self::Market { leg, settings } => settings.apply(OrderRequest::market(leg.build()?)),
            Self::Limit {
                leg,
                price,
                settings,
            } => settings.apply(OrderRequest::limit(leg.build()?, price)),
            Self::Stop {
                leg,
                stop_price,
                settings,
            } => settings.apply(OrderRequest::stop(leg.build()?, stop_price)),
            Self::StopLimit {
                leg,
                stop_price,
                price,
                settings,
            } => settings.apply(OrderRequest::stop_limit(leg.build()?, stop_price, price)),
            Self::TrailingStop {
                leg,
                basis,
                link,
                offset,
                settings,
            } => settings.apply(OrderRequest::trailing_stop(
                leg.build()?,
                basis,
                link,
                offset,
            )),
            Self::NetDebit {
                legs,
                price,
                settings,
            } => settings.apply(OrderRequest::net_debit(build_legs(legs)?, price)),
            Self::NetCredit {
                legs,
                price,
                settings,
            } => settings.apply(OrderRequest::net_credit(build_legs(legs)?, price)),
            Self::NetZero { legs, settings } => {
                settings.apply(OrderRequest::net_zero(build_legs(legs)?))
            }
            Self::Oco { first, second } => {
                OrderRequest::one_cancels_other(first.expand()?, second.expand()?)
            }
            Self::Trigger { entry, children } => {
                if children.is_empty() {
                    return Err("A trigger needs at least one child order".into());
                }
                children
                    .into_iter()
                    .try_fold(entry.expand()?, |order, child| {
                        Ok::<_, String>(order.triggers(child.expand()?))
                    })?
            }
            Self::Bracket {
                entry,
                take_profit,
                stop_loss,
            } => entry.expand()?.bracket(take_profit, stop_loss)?,
        })
    }
}

impl TryFrom<OrderSpec> for OrderRequest {
    type Error = String;

    fn try_from(spec: OrderSpec) -> Result<Self, Self::Error> {
        spec.build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn build(spec: serde_json::Value) -> Result<serde_json::Value, String> {
        let spec: OrderSpec = serde_json::from_value(spec).map_err(|e| e.to_string())?;
        Ok(serde_json::to_value(spec.build()?).unwrap())
    }

    #[test]
    fn specs_expand_to_the_documented_request_shapes() {
        let leg = json!({"instruction": "BUY", "quantity": "10",
            "instrument": {"asset_type": "EQUITY", "symbol": "AMD"}});
        assert_eq!(
            build(json!({"kind": "market", "leg": leg})).unwrap(),
            json!({
                "orderType": "MARKET", "session": "NORMAL", "duration": "DAY",
                "orderStrategyType": "SINGLE",
                "orderLegCollection": [{"instruction": "BUY", "quantity": "10",
                    "instrument": {"symbol": "AMD", "assetType": "EQUITY"}}]
            })
        );
        let bracket = build(json!({
            "kind": "bracket",
            "entry": {"kind": "limit", "leg": leg, "price": "150.25",
                      "settings": {"duration": "GOOD_TILL_CANCEL", "tag": "swing-1"}},
            "take_profit": "160",
            "stop_loss": "145"
        }))
        .unwrap();
        assert_eq!(bracket["orderStrategyType"], "TRIGGER");
        assert_eq!(bracket["duration"], "GOOD_TILL_CANCEL");
        assert_eq!(bracket["tag"], "swing-1");
        let exits = &bracket["childOrderStrategies"][0];
        assert_eq!(exits["orderStrategyType"], "OCO");
        assert_eq!(exits["childOrderStrategies"][0]["price"], "160");
        assert_eq!(exits["childOrderStrategies"][1]["stopPrice"], "145");
        assert_eq!(
            exits["childOrderStrategies"][1]["orderLegCollection"][0]["instruction"],
            "SELL"
        );
        let sell = json!({"instruction": "SELL", "quantity": "2",
            "instrument": {"asset_type": "EQUITY", "symbol": "XYZ"}});
        let trigger = build(json!({
            "kind": "trigger",
            "entry": {"kind": "limit", "leg": leg, "price": "14.97"},
            "children": [{
                "kind": "oco",
                "first": {"kind": "limit", "leg": sell, "price": "15.27"},
                "second": {"kind": "stop_limit", "leg": sell, "stop_price": "11.30",
                           "price": "11.27"}
            }]
        }))
        .unwrap();
        assert_eq!(trigger["orderStrategyType"], "TRIGGER");
        assert_eq!(
            trigger["childOrderStrategies"][0]["childOrderStrategies"][1]["orderType"],
            "STOP_LIMIT"
        );
        let trailing = build(json!({
            "kind": "trailing_stop", "leg": sell, "basis": "BID", "link": "VALUE",
            "offset": "10", "settings": {"stop_type": "MARK", "destination": "AUTO"}
        }))
        .unwrap();
        assert_eq!(trailing["stopPriceLinkBasis"], "BID");
        assert_eq!(trailing["stopPriceOffset"], "10");
        assert_eq!(trailing["stopType"], "MARK");
        assert_eq!(trailing["requestedDestination"], "AUTO");
    }

    #[test]
    fn option_instruments_accept_osi_symbols_or_contract_parts() {
        let by_parts = json!({"asset_type": "OPTION", "underlying": "aapl",
            "expiration": "2024-05-17", "right": "PUT", "strike": "190"});
        let by_symbol = json!({"asset_type": "OPTION", "symbol": "AAPL  240517P00190000"});
        for instrument in [by_parts, by_symbol] {
            let order = build(json!({
                "kind": "net_credit",
                "legs": [
                    {"instruction": "SELL_TO_OPEN", "quantity": "1", "instrument": instrument,
                     "position_effect": "OPENING"},
                    {"instruction": "BUY_TO_OPEN", "quantity": "1",
                     "instrument": {"asset_type": "OPTION", "symbol": "AAPL  240517P00185000"}}
                ],
                "price": "1.05",
                "settings": {"complex_strategy": "VERTICAL"}
            }))
            .unwrap();
            assert_eq!(order["orderType"], "NET_CREDIT");
            assert_eq!(order["complexOrderStrategyType"], "VERTICAL");
            let leg = &order["orderLegCollection"][0];
            assert_eq!(leg["instrument"]["symbol"], "AAPL  240517P00190000");
            assert_eq!(leg["instrument"]["assetType"], "OPTION");
            assert_eq!(leg["positionEffect"], "OPENING");
        }
    }

    #[test]
    fn malformed_specs_are_rejected() {
        let leg = json!({"instruction": "BUY", "quantity": "1",
            "instrument": {"asset_type": "EQUITY", "symbol": "AMD"}});
        // Unknown keys and unknown kinds fail deserialization.
        assert!(
            build(json!({"kind": "limit", "leg": leg, "price": "1", "duration": "DAY"})).is_err()
        );
        assert!(
            build(json!({"kind": "limit", "leg": leg, "price": "1",
            "settings": {"durration": "DAY"}}))
            .is_err()
        );
        assert!(build(json!({"kind": "iceberg", "leg": leg})).is_err());
        assert!(build(json!({"kind": "limit", "leg": leg})).is_err());
        // Options need exactly one description.
        for instrument in [
            json!({"asset_type": "OPTION"}),
            json!({"asset_type": "OPTION", "symbol": "AAPL  240517P00190000", "strike": "190"}),
            json!({"asset_type": "OPTION", "underlying": "AAPL", "strike": "190"}),
            json!({"asset_type": "OPTION", "symbol": "AAPL"}),
            json!({"asset_type": "FUTURE", "symbol": "/ES"}),
        ] {
            assert!(
                build(
                    json!({"kind": "market", "leg": {"instruction": "BUY_TO_OPEN",
                    "quantity": "1", "instrument": instrument}})
                )
                .is_err(),
                "{instrument}"
            );
        }
        // Structural rules from the constructors and validate() apply.
        assert!(
            build(
                json!({"kind": "trigger", "entry": {"kind": "market", "leg": leg},
            "children": []})
            )
            .is_err()
        );
        assert!(
            build(
                json!({"kind": "bracket", "entry": {"kind": "market", "leg": leg},
            "take_profit": "1", "stop_loss": "2"})
            )
            .is_err()
        );
        assert!(build(json!({"kind": "limit", "leg": leg, "price": "0"})).is_err());
    }

    #[test]
    fn specs_round_trip_through_serde() {
        let spec = OrderSpec::Bracket {
            entry: Box::new(OrderSpec::Limit {
                leg: LegSpec {
                    instruction: OrderInstruction::Buy,
                    quantity: Decimal::from(10),
                    instrument: InstrumentSpec::Equity {
                        symbol: "AAPL".into(),
                    },
                    position_effect: None,
                },
                price: Decimal::new(15025, 2),
                settings: OrderSettings {
                    duration: Some(OrderDuration::Gtc),
                    ..OrderSettings::default()
                },
            }),
            take_profit: Decimal::from(160),
            stop_loss: Decimal::from(145),
        };
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["kind"], "bracket");
        assert_eq!(json["entry"]["kind"], "limit");
        assert_eq!(json["entry"]["settings"]["duration"], "GOOD_TILL_CANCEL");
        assert_eq!(serde_json::from_value::<OrderSpec>(json).unwrap(), spec);
    }
}
