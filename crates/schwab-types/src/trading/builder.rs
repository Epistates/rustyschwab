//! Constructors that assemble [`OrderRequest`] trees the way Schwab's order
//! examples are written; the module-level docs on [`super`] show the flow.

use super::{
    ComplexOrderStrategyType, OrderDuration, OrderInstruction, OrderRequest,
    OrderRequestInstrument, OrderRequestLeg, OrderSession, OrderStrategyType, OrderType,
    PositionEffect, PriceLinkBasis, PriceLinkType, RequestedDestination, SpecialInstruction,
    StopType, TaxLotMethod,
};
use crate::common::{AssetType, PutCall};
use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;
use std::{fmt, str::FromStr};

/// An option contract in the 21-character OSI form Schwab uses for order
/// legs and quotes: the underlying padded to six characters, the expiration
/// as `YYMMDD`, `C` or `P`, and the strike times 1000 as eight digits
/// (`AAPL  240517P00190000`).
///
/// ```
/// use chrono::NaiveDate;
/// use schwab_types::Decimal;
/// use schwab_types::trading::OptionSymbol;
///
/// let expiry = NaiveDate::from_ymd_opt(2024, 5, 17).unwrap();
/// let put = OptionSymbol::put("AAPL", expiry, Decimal::from(190)).unwrap();
/// assert_eq!(put.to_string(), "AAPL  240517P00190000");
/// assert_eq!("AAPL  240517P00190000".parse::<OptionSymbol>().unwrap(), put);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OptionSymbol {
    underlying: String,
    expiration: NaiveDate,
    put_call: PutCall,
    strike: Decimal,
}

impl OptionSymbol {
    /// Build a symbol, rejecting anything the OSI format cannot express: an
    /// underlying longer than six ASCII letters or digits, an expiration
    /// outside 2000–2099 (the symbol keeps two year digits, read as 20YY),
    /// a strike that is not positive, above 99999.999, or finer than a
    /// tenth of a cent.
    pub fn new(
        underlying: impl AsRef<str>,
        expiration: NaiveDate,
        put_call: PutCall,
        strike: Decimal,
    ) -> Result<Self, String> {
        let underlying = underlying.as_ref().trim().to_ascii_uppercase();
        if underlying.is_empty()
            || underlying.len() > 6
            || !underlying.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err(format!(
                "Option underlying must be one to six letters or digits, got {underlying:?}"
            ));
        }
        if !(2000..=2099).contains(&expiration.year()) {
            return Err(format!(
                "Option expiration must be in 2000-2099, got {expiration}"
            ));
        }
        let strike = strike.normalize();
        if strike <= Decimal::ZERO || strike.scale() > 3 || strike >= Decimal::from(100_000) {
            return Err(format!(
                "Option strike must be positive, below 100000, \
                 and a multiple of 0.001, got {strike}"
            ));
        }
        Ok(Self {
            underlying,
            expiration,
            put_call,
            strike,
        })
    }

    /// A call contract; see [`OptionSymbol::new`] for the validation rules.
    pub fn call(
        underlying: impl AsRef<str>,
        expiration: NaiveDate,
        strike: Decimal,
    ) -> Result<Self, String> {
        Self::new(underlying, expiration, PutCall::Call, strike)
    }

    /// A put contract; see [`OptionSymbol::new`] for the validation rules.
    pub fn put(
        underlying: impl AsRef<str>,
        expiration: NaiveDate,
        strike: Decimal,
    ) -> Result<Self, String> {
        Self::new(underlying, expiration, PutCall::Put, strike)
    }

    pub fn underlying(&self) -> &str {
        &self.underlying
    }

    pub fn expiration(&self) -> NaiveDate {
        self.expiration
    }

    pub fn put_call(&self) -> PutCall {
        self.put_call
    }

    pub fn strike(&self) -> Decimal {
        self.strike
    }
}

impl fmt::Display for OptionSymbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let right = match self.put_call {
            PutCall::Call => 'C',
            PutCall::Put => 'P',
        };
        // `new` bounds the strike, so the mantissa fits eight digits.
        let thousandths = (self.strike * Decimal::from(1000)).normalize().mantissa();
        write!(
            f,
            "{:<6}{}{right}{thousandths:08}",
            self.underlying,
            self.expiration.format("%y%m%d")
        )
    }
}

impl FromStr for OptionSymbol {
    type Err = String;

    /// Parses exactly the canonical form [`Display`](fmt::Display) writes:
    /// an upper-case root padded on the right to six characters, so a symbol
    /// round-trips unchanged. The two-digit year is 20YY: listed options do
    /// not expire in the last century, and `%y` would read 70–99 as 19YY.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || format!("Expected a 21-character OSI option symbol, got {s:?}");
        if s.len() != 21 || !s.is_ascii() {
            return Err(invalid());
        }
        let (underlying, rest) = s.split_at(6);
        let (date, rest) = rest.split_at(6);
        let (right, strike) = rest.split_at(1);
        let digits = |t: &str| t.bytes().all(|b| b.is_ascii_digit());
        if !digits(date) {
            return Err(invalid());
        }
        let field = |range: std::ops::Range<usize>| date[range].parse::<u32>().ok();
        let expiration = field(0..2)
            .zip(field(2..4))
            .zip(field(4..6))
            .and_then(|((y, m), d)| NaiveDate::from_ymd_opt(2000 + y as i32, m, d))
            .ok_or_else(invalid)?;
        let put_call = match right {
            "C" => PutCall::Call,
            "P" => PutCall::Put,
            _ => return Err(invalid()),
        };
        let thousandths: i64 = strike
            .parse()
            .ok()
            .filter(|_| digits(strike))
            .ok_or_else(invalid)?;
        let symbol = Self::new(
            underlying,
            expiration,
            put_call,
            Decimal::new(thousandths, 3),
        )?;
        if symbol.to_string() != s {
            return Err(format!(
                "{s:?} is not in canonical OSI form; expected {:?}",
                symbol.to_string()
            ));
        }
        Ok(symbol)
    }
}

impl From<OptionSymbol> for String {
    fn from(symbol: OptionSymbol) -> Self {
        symbol.to_string()
    }
}

impl From<&OptionSymbol> for String {
    fn from(symbol: &OptionSymbol) -> Self {
        symbol.to_string()
    }
}

impl OrderInstruction {
    /// The instruction that flattens a position this one opens: `BUY` →
    /// `SELL`, `SELL_SHORT` and `SELL_SHORT_EXEMPT` → `BUY_TO_COVER`,
    /// `BUY_TO_OPEN` → `SELL_TO_CLOSE`, `SELL_TO_OPEN` → `BUY_TO_CLOSE`.
    /// Closing instructions and `EXCHANGE` have no counterpart.
    pub fn closing(self) -> Option<Self> {
        match self {
            Self::Buy => Some(Self::Sell),
            Self::SellShort | Self::SellShortExempt => Some(Self::BuyToCover),
            Self::BuyToOpen => Some(Self::SellToClose),
            Self::SellToOpen => Some(Self::BuyToClose),
            Self::Sell
            | Self::BuyToCover
            | Self::BuyToClose
            | Self::SellToClose
            | Self::Exchange
            | Self::Unknown => None,
        }
    }

    /// Whether filling this instruction leaves the account long the
    /// instrument (`BUY`, `BUY_TO_OPEN`) rather than short; `None` for
    /// instructions that close or exchange.
    pub fn opens_long(self) -> Option<bool> {
        match self {
            Self::Buy | Self::BuyToOpen => Some(true),
            Self::SellShort | Self::SellShortExempt | Self::SellToOpen => Some(false),
            _ => None,
        }
    }
}

impl OrderRequestInstrument {
    pub fn equity(symbol: impl Into<String>) -> Self {
        Self {
            symbol: symbol.into(),
            asset_type: AssetType::Equity,
        }
    }

    /// An option leg; accepts an [`OptionSymbol`] or an already formatted
    /// OSI string.
    pub fn option(symbol: impl Into<String>) -> Self {
        Self {
            symbol: symbol.into(),
            asset_type: AssetType::Option,
        }
    }
}

impl OrderRequestLeg {
    pub fn new(
        instruction: OrderInstruction,
        instrument: OrderRequestInstrument,
        quantity: impl Into<Decimal>,
    ) -> Self {
        Self {
            instruction,
            quantity: quantity.into(),
            instrument,
            position_effect: None,
            quantity_type: None,
            div_cap_gains: None,
            to_symbol: None,
        }
    }

    pub fn equity(
        instruction: OrderInstruction,
        symbol: impl Into<String>,
        quantity: impl Into<Decimal>,
    ) -> Self {
        Self::new(
            instruction,
            OrderRequestInstrument::equity(symbol),
            quantity,
        )
    }

    /// `quantity` is in contracts.
    pub fn option(
        instruction: OrderInstruction,
        symbol: impl Into<String>,
        quantity: impl Into<Decimal>,
    ) -> Self {
        Self::new(
            instruction,
            OrderRequestInstrument::option(symbol),
            quantity,
        )
    }

    pub fn with_position_effect(mut self, effect: PositionEffect) -> Self {
        self.position_effect = Some(effect);
        self
    }
}

impl OrderRequest {
    fn single(order_type: OrderType, legs: Vec<OrderRequestLeg>) -> Self {
        Self {
            session: Some(OrderSession::Normal),
            duration: Some(OrderDuration::Day),
            order_type: Some(order_type),
            order_strategy_type: OrderStrategyType::Single,
            order_leg_collection: legs,
            ..Self::default()
        }
    }

    pub fn market(leg: OrderRequestLeg) -> Self {
        Self::single(OrderType::Market, vec![leg])
    }

    pub fn limit(leg: OrderRequestLeg, price: Decimal) -> Self {
        Self {
            price: Some(price),
            ..Self::single(OrderType::Limit, vec![leg])
        }
    }

    /// A stop order that becomes a market order at `stop_price`.
    pub fn stop(leg: OrderRequestLeg, stop_price: Decimal) -> Self {
        Self {
            stop_price: Some(stop_price),
            ..Self::single(OrderType::Stop, vec![leg])
        }
    }

    /// A stop order that becomes a limit order at `price` once `stop_price`
    /// trades.
    pub fn stop_limit(leg: OrderRequestLeg, stop_price: Decimal, price: Decimal) -> Self {
        Self {
            price: Some(price),
            stop_price: Some(stop_price),
            ..Self::single(OrderType::StopLimit, vec![leg])
        }
    }

    /// A stop that trails `basis` by `offset`, expressed as `link` (a price
    /// difference, a percentage, or ticks).
    ///
    /// There is no `trailing_stop_limit` constructor on purpose: Schwab's
    /// schema has no `priceOffset` and no published `TRAILING_STOP_LIMIT`
    /// example, so how the limit should be linked is unspecified. To send one,
    /// set `order_type` to [`OrderType::TrailingStopLimit`] and a fixed `price`
    /// on the result (the reading `validate()` enforces) and confirm the shape
    /// with a preview first.
    pub fn trailing_stop(
        leg: OrderRequestLeg,
        basis: PriceLinkBasis,
        link: PriceLinkType,
        offset: Decimal,
    ) -> Self {
        Self {
            stop_price_link_basis: Some(basis),
            stop_price_link_type: Some(link),
            stop_price_offset: Some(offset),
            ..Self::single(OrderType::TrailingStop, vec![leg])
        }
    }

    /// A multi-leg order that pays at most `price` net.
    pub fn net_debit(legs: Vec<OrderRequestLeg>, price: Decimal) -> Self {
        Self {
            price: Some(price),
            ..Self::single(OrderType::NetDebit, legs)
        }
    }

    /// A multi-leg order that collects at least `price` net.
    pub fn net_credit(legs: Vec<OrderRequestLeg>, price: Decimal) -> Self {
        Self {
            price: Some(price),
            ..Self::single(OrderType::NetCredit, legs)
        }
    }

    /// A multi-leg order that must fill for zero net cost.
    pub fn net_zero(legs: Vec<OrderRequestLeg>) -> Self {
        Self::single(OrderType::NetZero, legs)
    }

    pub fn with_session(mut self, session: OrderSession) -> Self {
        self.session = Some(session);
        self
    }

    pub fn with_duration(mut self, duration: OrderDuration) -> Self {
        self.duration = Some(duration);
        self
    }

    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    pub fn with_complex_strategy(mut self, strategy: ComplexOrderStrategyType) -> Self {
        self.complex_order_strategy_type = Some(strategy);
        self
    }

    pub fn with_special_instruction(mut self, instruction: SpecialInstruction) -> Self {
        self.special_instruction = Some(instruction);
        self
    }

    pub fn with_destination(mut self, destination: RequestedDestination) -> Self {
        self.requested_destination = Some(destination);
        self
    }

    pub fn with_tax_lot_method(mut self, method: TaxLotMethod) -> Self {
        self.tax_lot_method = Some(method);
        self
    }

    pub fn with_stop_type(mut self, stop_type: StopType) -> Self {
        self.stop_type = Some(stop_type);
        self
    }

    /// Two orders of which a fill on either cancels the other.
    pub fn one_cancels_other(first: OrderRequest, second: OrderRequest) -> Self {
        Self {
            order_strategy_type: OrderStrategyType::Oco,
            child_order_strategies: vec![first, second],
            ..Self::default()
        }
    }

    /// Make this order a `TRIGGER` parent: `child` is submitted only after
    /// this order fills. Call again to trigger several children.
    pub fn triggers(mut self, child: OrderRequest) -> Self {
        self.order_strategy_type = OrderStrategyType::Trigger;
        self.child_order_strategies.push(child);
        self
    }

    /// Attach a take-profit limit and a stop-loss to a single-leg opening
    /// order: the exits are a good-till-cancel OCO pair that Schwab submits
    /// once the entry fills, using the closing instruction and full quantity
    /// of the entry leg.
    ///
    /// Fails if the entry is not a single-leg `SINGLE` order, if its
    /// instruction does not open a position, or if the exits are on the
    /// wrong sides for the position's direction: of each other always, and
    /// of the entry's price: its limit (`LIMIT`, `STOP_LIMIT`), or for a
    /// `STOP` entry its trigger. A long entry at 150 needs its stop below
    /// 150 and its target above; otherwise an exit is marketable the moment
    /// the entry fills. A market entry has no price to compare, so only the
    /// ordering of the exits is checked.
    pub fn bracket(self, take_profit: Decimal, stop_loss: Decimal) -> Result<Self, String> {
        if self.order_strategy_type != OrderStrategyType::Single
            || !self.child_order_strategies.is_empty()
        {
            return Err("A bracket entry must be a SINGLE order without children".into());
        }
        let [leg] = self.order_leg_collection.as_slice() else {
            return Err("A bracket entry must have exactly one leg".into());
        };
        let (Some(closing), Some(long)) = (leg.instruction.closing(), leg.instruction.opens_long())
        else {
            return Err(format!(
                "{:?} does not open a position, so it cannot be bracketed",
                leg.instruction
            ));
        };
        if (long && take_profit <= stop_loss) || (!long && take_profit >= stop_loss) {
            let side = if long { "long" } else { "short" };
            return Err(format!(
                "Bracket exits are inverted for a {side} entry: \
                 take profit {take_profit}, stop loss {stop_loss}"
            ));
        }
        if let Some(entry) = self.price.or(self.stop_price) {
            let (below, above) = if long {
                (stop_loss, take_profit)
            } else {
                (take_profit, stop_loss)
            };
            if !(below < entry && entry < above) {
                let side = if long { "long" } else { "short" };
                return Err(format!(
                    "Bracket exits must straddle a {side} entry at {entry}: \
                     take profit {take_profit}, stop loss {stop_loss}"
                ));
            }
        }
        let exit = || OrderRequestLeg {
            position_effect: leg.position_effect.map(|_| PositionEffect::Closing),
            ..OrderRequestLeg::new(closing, leg.instrument.clone(), leg.quantity)
        };
        let exits = Self::one_cancels_other(
            Self::limit(exit(), take_profit).with_duration(OrderDuration::Gtc),
            Self::stop(exit(), stop_loss).with_duration(OrderDuration::Gtc),
        );
        Ok(self.triggers(exits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn expiry() -> NaiveDate {
        NaiveDate::from_ymd_opt(2024, 5, 17).unwrap()
    }

    #[test]
    fn option_symbols_round_trip_the_osi_format() {
        let put = OptionSymbol::put("aapl", expiry(), d("190")).unwrap();
        assert_eq!(put.to_string(), "AAPL  240517P00190000");
        assert_eq!(put.underlying(), "AAPL");
        let parsed: OptionSymbol = "AAPL  240517P00190000".parse().unwrap();
        assert_eq!(parsed, put);
        // Fractional strikes and six-character roots.
        let call = OptionSymbol::call("SPXW", expiry(), d("4512.5")).unwrap();
        assert_eq!(call.to_string(), "SPXW  240517C04512500");
        assert_eq!(call.to_string().parse::<OptionSymbol>().unwrap(), call);
        let long = OptionSymbol::call("ABCDEF", expiry(), d("0.5")).unwrap();
        assert_eq!(long.to_string(), "ABCDEF240517C00000500");
        assert_eq!(long.to_string().parse::<OptionSymbol>().unwrap(), long);
        assert_eq!(
            "AAPL  240517P00190000"
                .parse::<OptionSymbol>()
                .unwrap()
                .strike(),
            d("190")
        );
    }

    #[test]
    fn option_symbols_reject_what_osi_cannot_express() {
        assert!(OptionSymbol::call("ABCDEFG", expiry(), d("1")).is_err());
        assert!(OptionSymbol::call("", expiry(), d("1")).is_err());
        assert!(OptionSymbol::call("BRK.B", expiry(), d("1")).is_err());
        assert!(OptionSymbol::call("AAPL", expiry(), d("0")).is_err());
        assert!(OptionSymbol::call("AAPL", expiry(), d("-1")).is_err());
        assert!(OptionSymbol::call("AAPL", expiry(), d("100000")).is_err());
        assert!(OptionSymbol::call("AAPL", expiry(), d("1.0005")).is_err());
        // Two year digits: 2125 and 1999 would print as 2025 and 2099.
        for year in [1999, 2100, 2125] {
            let date = NaiveDate::from_ymd_opt(year, 6, 20).unwrap();
            assert!(OptionSymbol::call("SPY", date, d("550")).is_err(), "{year}");
        }
        for bad in [
            "AAPL 240517P00190000",
            "AAPL  240517X00190000",
            "AAPL  241317P00190000",
            "AAPL  240517P0019000a",
            "AAPL  240517P-0190000",
        ] {
            assert!(bad.parse::<OptionSymbol>().is_err(), "{bad}");
        }
    }

    #[test]
    fn single_orders_match_the_documented_shapes() {
        let market =
            OrderRequest::market(OrderRequestLeg::equity(OrderInstruction::Buy, "AMD", 10));
        market.validate().unwrap();
        assert_eq!(
            serde_json::to_value(&market).unwrap(),
            json!({
                "orderType": "MARKET", "session": "NORMAL", "duration": "DAY",
                "orderStrategyType": "SINGLE",
                "orderLegCollection": [{"instruction": "BUY", "quantity": 10,
                    "instrument": {"symbol": "AMD", "assetType": "EQUITY"}}]
            })
        );
        let put = OptionSymbol::put("AAPL", expiry(), d("190")).unwrap();
        let sell = OrderRequest::limit(
            OrderRequestLeg::option(OrderInstruction::SellToOpen, &put, 3),
            d("1.0"),
        )
        .with_duration(OrderDuration::Gtc)
        .with_complex_strategy(ComplexOrderStrategyType::None);
        sell.validate().unwrap();
        assert_eq!(
            serde_json::to_value(&sell).unwrap(),
            json!({
                "orderType": "LIMIT", "session": "NORMAL", "price": "1.0",
                "duration": "GOOD_TILL_CANCEL", "orderStrategyType": "SINGLE",
                "complexOrderStrategyType": "NONE",
                "orderLegCollection": [{"instruction": "SELL_TO_OPEN", "quantity": 3,
                    "instrument": {"symbol": "AAPL  240517P00190000", "assetType": "OPTION"}}]
            })
        );
        let trailing = OrderRequest::trailing_stop(
            OrderRequestLeg::equity(OrderInstruction::Sell, "XYZ", 10),
            PriceLinkBasis::Bid,
            PriceLinkType::Value,
            d("10"),
        );
        trailing.validate().unwrap();
        assert_eq!(
            serde_json::to_value(&trailing).unwrap(),
            json!({
                "orderType": "TRAILING_STOP", "session": "NORMAL", "stopPriceLinkBasis": "BID",
                "stopPriceLinkType": "VALUE", "stopPriceOffset": "10", "duration": "DAY",
                "orderStrategyType": "SINGLE",
                "orderLegCollection": [{"instruction": "SELL", "quantity": 10,
                    "instrument": {"symbol": "XYZ", "assetType": "EQUITY"}}]
            })
        );
        let spread = OrderRequest::net_debit(
            vec![
                OrderRequestLeg::option(OrderInstruction::BuyToOpen, "XYZ   240315P00045000", 2),
                OrderRequestLeg::option(OrderInstruction::SellToOpen, "XYZ   240315P00043000", 2),
            ],
            d("0.10"),
        );
        spread.validate().unwrap();
        let value = serde_json::to_value(&spread).unwrap();
        assert_eq!(value["orderType"], "NET_DEBIT");
        assert_eq!(value["price"], "0.10");
        assert_eq!(value["orderLegCollection"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn setters_cover_every_typed_request_field() {
        let order = OrderRequest::stop(
            OrderRequestLeg::equity(OrderInstruction::Sell, "XYZ", 1)
                .with_position_effect(PositionEffect::Closing),
            d("9"),
        )
        .with_duration(OrderDuration::Gtc)
        .with_tag("t")
        .with_special_instruction(SpecialInstruction::AllOrNone)
        .with_destination(RequestedDestination::Auto)
        .with_tax_lot_method(TaxLotMethod::Lifo)
        .with_stop_type(StopType::Mark);
        order.validate().unwrap();
        let value = serde_json::to_value(&order).unwrap();
        assert_eq!(value["session"], "NORMAL");
        assert_eq!(value["duration"], "GOOD_TILL_CANCEL");
        // Extended hours take limit orders only.
        assert!(
            order
                .clone()
                .with_session(OrderSession::Seamless)
                .validate()
                .is_err()
        );
        let seamless = OrderRequest::limit(
            OrderRequestLeg::equity(OrderInstruction::Sell, "XYZ", 1),
            d("9"),
        )
        .with_session(OrderSession::Seamless);
        seamless.validate().unwrap();
        assert_eq!(
            serde_json::to_value(&seamless).unwrap()["session"],
            "SEAMLESS"
        );
        assert_eq!(value["tag"], "t");
        assert_eq!(value["specialInstruction"], "ALL_OR_NONE");
        assert_eq!(value["requestedDestination"], "AUTO");
        assert_eq!(value["taxLotMethod"], "LIFO");
        assert_eq!(value["stopType"], "MARK");
        assert_eq!(value["orderLegCollection"][0]["positionEffect"], "CLOSING");
    }

    #[test]
    fn composites_nest_like_the_documented_examples() {
        let sell = |symbol: &str, quantity: u32| {
            OrderRequestLeg::equity(OrderInstruction::Sell, symbol, quantity)
        };
        let oco = OrderRequest::one_cancels_other(
            OrderRequest::limit(sell("XYZ", 2), d("45.97")),
            OrderRequest::stop_limit(sell("ABC", 2), d("37.03"), d("37.00")),
        );
        oco.validate().unwrap();
        let value = serde_json::to_value(&oco).unwrap();
        assert_eq!(
            value,
            json!({
                "orderStrategyType": "OCO",
                "childOrderStrategies": [
                    {"orderType": "LIMIT", "session": "NORMAL", "price": "45.97", "duration": "DAY",
                     "orderStrategyType": "SINGLE",
                     "orderLegCollection": [{"instruction": "SELL", "quantity": 2,
                         "instrument": {"symbol": "XYZ", "assetType": "EQUITY"}}]},
                    {"orderType": "STOP_LIMIT", "session": "NORMAL", "price": "37.00",
                     "stopPrice": "37.03", "duration": "DAY", "orderStrategyType": "SINGLE",
                     "orderLegCollection": [{"instruction": "SELL", "quantity": 2,
                         "instrument": {"symbol": "ABC", "assetType": "EQUITY"}}]}
                ]
            })
        );
        let trigger = OrderRequest::limit(
            OrderRequestLeg::equity(OrderInstruction::Buy, "XYZ", 5),
            d("14.97"),
        )
        .triggers(OrderRequest::one_cancels_other(
            OrderRequest::limit(sell("ABC", 5), d("15.27")).with_duration(OrderDuration::Gtc),
            OrderRequest::stop(sell("IJK", 5), d("11.27")).with_duration(OrderDuration::Gtc),
        ));
        trigger.validate().unwrap();
        let value = serde_json::to_value(&trigger).unwrap();
        assert_eq!(value["orderStrategyType"], "TRIGGER");
        assert_eq!(value["childOrderStrategies"][0]["orderStrategyType"], "OCO");
        assert_eq!(
            value["childOrderStrategies"][0]["childOrderStrategies"][1]["stopPrice"],
            "11.27"
        );
        assert_eq!(
            value["childOrderStrategies"][0]["childOrderStrategies"][1]["duration"],
            "GOOD_TILL_CANCEL"
        );
    }

    #[test]
    fn brackets_derive_exits_from_the_entry() {
        let entry = OrderRequest::limit(
            OrderRequestLeg::equity(OrderInstruction::Buy, "AAPL", 10)
                .with_position_effect(PositionEffect::Opening),
            d("150.25"),
        );
        let bracket = entry.clone().bracket(d("160"), d("145")).unwrap();
        bracket.validate().unwrap();
        let value = serde_json::to_value(&bracket).unwrap();
        assert_eq!(value["orderStrategyType"], "TRIGGER");
        assert_eq!(value["price"], "150.25");
        let exits = &value["childOrderStrategies"][0];
        assert_eq!(exits["orderStrategyType"], "OCO");
        let (profit, loss) = (
            &exits["childOrderStrategies"][0],
            &exits["childOrderStrategies"][1],
        );
        assert_eq!(profit["orderType"], "LIMIT");
        assert_eq!(profit["price"], "160");
        assert_eq!(profit["duration"], "GOOD_TILL_CANCEL");
        assert_eq!(loss["orderType"], "STOP");
        assert_eq!(loss["stopPrice"], "145");
        for exit in [profit, loss] {
            let leg = &exit["orderLegCollection"][0];
            assert_eq!(leg["instruction"], "SELL");
            assert_eq!(leg["quantity"], 10);
            assert_eq!(leg["instrument"]["symbol"], "AAPL");
            assert_eq!(leg["positionEffect"], "CLOSING");
        }
        // Shorts flip the instruction and the price ordering.
        let short = OrderRequest::market(OrderRequestLeg::equity(
            OrderInstruction::SellShort,
            "AAPL",
            10,
        ));
        let covered = short.clone().bracket(d("140"), d("155")).unwrap();
        let exits = &serde_json::to_value(&covered).unwrap()["childOrderStrategies"][0];
        assert_eq!(
            exits["childOrderStrategies"][0]["orderLegCollection"][0]["instruction"],
            "BUY_TO_COVER"
        );
        assert!(short.bracket(d("155"), d("140")).is_err());
        assert!(entry.clone().bracket(d("145"), d("160")).is_err());
        assert!(entry.clone().bracket(d("150"), d("150")).is_err());
        // Only single-leg opening SINGLE orders can be bracketed.
        assert!(
            OrderRequest::market(OrderRequestLeg::equity(OrderInstruction::Sell, "AAPL", 10))
                .bracket(d("160"), d("145"))
                .is_err()
        );
        assert!(
            OrderRequest::net_debit(
                vec![
                    OrderRequestLeg::option(
                        OrderInstruction::BuyToOpen,
                        "XYZ   240315P00045000",
                        1
                    ),
                    OrderRequestLeg::option(
                        OrderInstruction::SellToOpen,
                        "XYZ   240315P00043000",
                        1
                    ),
                ],
                d("0.10"),
            )
            .bracket(d("1"), d("0.05"))
            .is_err()
        );
        assert!(
            entry
                .triggers(OrderRequest::market(OrderRequestLeg::equity(
                    OrderInstruction::Sell,
                    "AAPL",
                    10
                )))
                .bracket(d("160"), d("145"))
                .is_err()
        );
    }

    #[test]
    fn closing_instructions_pair_with_opening_ones() {
        use OrderInstruction::*;
        assert_eq!(Buy.closing(), Some(Sell));
        assert_eq!(SellShort.closing(), Some(BuyToCover));
        assert_eq!(BuyToOpen.closing(), Some(SellToClose));
        assert_eq!(SellToOpen.closing(), Some(BuyToClose));
        assert_eq!(SellShortExempt.closing(), Some(BuyToCover));
        assert_eq!(SellShortExempt.opens_long(), Some(false));
        for closing in [Sell, BuyToCover, BuyToClose, SellToClose, Exchange] {
            assert_eq!(closing.closing(), None);
        }
    }
}
