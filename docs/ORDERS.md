# Placing Orders

> [!CAUTION]
> Preview before you place. `preview_order` runs Schwab's own validation and
> returns fees and buying-power effects without executing. Verify automated
> strategies against the paper engine (`docs/PAPER_TRADING.md`) or a test
> account first; live orders carry real financial risk.

Outbound orders are `schwab_types::trading::OrderRequest`: a strict shape
(unknown fields fail deserialization) whose prices and quantities are
`rust_decimal::Decimal`, re-exported as `schwab_types::Decimal`. The
constructors below assemble the same JSON trees Schwab's order examples
show, so you never hand-nest `childOrderStrategies`. `Order` is the tolerant
type Schwab reports back and is never sent.

## Client methods

| Method | Effect |
|---|---|
| `client.preview_order(account_hash, &order)` | Schwab-side validation; returns the raw preview JSON. |
| `client.place_order(account_hash, &order)` | Submits. Schwab answers `201` with the order URL in `Location`; the result is `OrderSubmitResponse { order_id, location }`. |
| `client.replace_order(account_hash, order_id, &order)` | Replaces a working order with a new definition (same response shape). |
| `client.cancel_order(account_hash, order_id)` | Cancels a working order. |

Every constructor produces a `NORMAL`-session, `DAY` order; change either
with `with_session` / `with_duration`. `order.validate()` checks structure
(required prices, leg shape, strategy nesting) before the request leaves the
process; it never rounds a price. `validate_price_increment(tick)` enforces a
tick you supply.

## Single-leg orders

```rust
use schwab_rs::types::Decimal;
use schwab_rs::types::trading::{
    OrderDuration, OrderInstruction, OrderRequest, OrderRequestLeg, PriceLinkBasis,
    PriceLinkType,
};

// Buy 10 AMD at the market.
let market = OrderRequest::market(OrderRequestLeg::equity(OrderInstruction::Buy, "AMD", 10));

// Buy 5 AAPL at 150.00 or better, good till cancelled.
let limit = OrderRequest::limit(
    OrderRequestLeg::equity(OrderInstruction::Buy, "AAPL", 5),
    Decimal::new(15000, 2),
)
.with_duration(OrderDuration::Gtc);

// Sell 5 AAPL if it trades down to 140.00.
let stop = OrderRequest::stop(
    OrderRequestLeg::equity(OrderInstruction::Sell, "AAPL", 5),
    "140".parse()?,
);

// Stop at 37.03 that becomes a 37.00 limit.
let stop_limit = OrderRequest::stop_limit(
    OrderRequestLeg::equity(OrderInstruction::Sell, "ABC", 2),
    "37.03".parse()?,
    "37.00".parse()?,
);

// Sell 10 XYZ with a stop that trails the bid by $10.
let trailing = OrderRequest::trailing_stop(
    OrderRequestLeg::equity(OrderInstruction::Sell, "XYZ", 10),
    PriceLinkBasis::Bid,
    PriceLinkType::Value,
    Decimal::from(10),
);

let response = client.place_order("account_hash", &limit).await?;
println!("order id: {:?}", response.order_id);
```

Quantities accept any integer type or a `Decimal`; prices are `Decimal`, so
`"150.25".parse()?` and `Decimal::new(15025, 2)` both work. Avoid
`Decimal::from_f64`: floating-point input may already have lost precision.

## Options

`OptionSymbol` builds and parses the 21-character OSI form Schwab uses
(`root` padded to six characters, `YYMMDD`, `C`/`P`, strike × 1000 as eight
digits). It rejects roots longer than six alphanumerics and strikes the
format cannot represent.

```rust
use chrono::NaiveDate;
use schwab_rs::types::Decimal;
use schwab_rs::types::trading::{
    ComplexOrderStrategyType, OptionSymbol, OrderInstruction, OrderRequest, OrderRequestLeg,
    PositionEffect,
};

let expiry = NaiveDate::from_ymd_opt(2024, 5, 17).unwrap();
let put = OptionSymbol::put("AAPL", expiry, Decimal::from(190))?;
assert_eq!(put.to_string(), "AAPL  240517P00190000");

// Buy one put to open at 2.50.
let single = OrderRequest::limit(
    OrderRequestLeg::option(OrderInstruction::BuyToOpen, &put, 1)
        .with_position_effect(PositionEffect::Opening),
    "2.50".parse()?,
);

// Vertical call spread for a 1.20 net debit.
let long = OptionSymbol::call("AAPL", expiry, Decimal::from(190))?;
let short = OptionSymbol::call("AAPL", expiry, Decimal::from(195))?;
let vertical = OrderRequest::net_debit(
    vec![
        OrderRequestLeg::option(OrderInstruction::BuyToOpen, &long, 1),
        OrderRequestLeg::option(OrderInstruction::SellToOpen, &short, 1),
    ],
    "1.20".parse()?,
)
.with_complex_strategy(ComplexOrderStrategyType::Vertical);
```

`net_credit` and `net_zero` build the other multi-leg price types. An
already formatted OSI string is also accepted wherever a symbol is.

## Composite orders

```rust
use schwab_rs::types::Decimal;
use schwab_rs::types::trading::{OrderDuration, OrderInstruction, OrderRequest, OrderRequestLeg};

let sell = |symbol: &str| OrderRequestLeg::equity(OrderInstruction::Sell, symbol, 2);

// One-cancels-other: whichever fills first cancels the other.
let oco = OrderRequest::one_cancels_other(
    OrderRequest::limit(sell("XYZ"), "45.97".parse()?),
    OrderRequest::stop_limit(sell("XYZ"), "37.03".parse()?, "37.00".parse()?),
);

// Trigger: the child is submitted once the parent fills.
let trigger = OrderRequest::limit(
    OrderRequestLeg::equity(OrderInstruction::Buy, "XYZ", 5),
    "14.97".parse()?,
)
.triggers(OrderRequest::limit(sell("XYZ"), "15.27".parse()?).with_duration(OrderDuration::Gtc));

// Bracket: entry that triggers a GTC take-profit limit and stop-loss OCO
// pair, using the closing instruction and quantity of the entry leg.
let bracket = OrderRequest::limit(
    OrderRequestLeg::equity(OrderInstruction::Buy, "AAPL", 10),
    "150.25".parse()?,
)
.bracket(Decimal::from(160), Decimal::from(145))?;
```

`bracket` requires a single-leg `SINGLE` entry whose instruction opens a
position (`BUY`, `SELL_SHORT`, `BUY_TO_OPEN`, `SELL_TO_OPEN`) and rejects
exit prices on the wrong side for the direction: long entries need
`take_profit > stop_loss`, short entries the reverse.

## Other request fields

| Setter | Wire field |
|---|---|
| `with_tag(tag)` | `tag` — your idempotency / intent identifier |
| `with_special_instruction(SpecialInstruction)` | `specialInstruction` (`ALL_OR_NONE`, `DO_NOT_REDUCE`, …) |
| `with_destination(RequestedDestination)` | `requestedDestination` (`INET`, `NYSE`, `AUTO`, …) |
| `with_tax_lot_method(TaxLotMethod)` | `taxLotMethod` (`FIFO`, `LIFO`, `SPECIFIC_LOT`, …) |
| `with_stop_type(StopType)` | `stopType` (`STANDARD`, `BID`, `ASK`, `LAST`, `MARK`) |
| `with_complex_strategy(ComplexOrderStrategyType)` | `complexOrderStrategyType` |

Every field is also a public member of `OrderRequest`, so anything without a
constructor (`TRAILING_STOP_LIMIT`, `LIMIT_ON_CLOSE`, `priceLinkBasis`, …)
can be set directly or deserialized from JSON. The paper engine and the MCP
policy reject some of these fields; see their docs.

## Tips

- Symbols: equities `AAPL`; indexes `$SPX`; options via `OptionSymbol`.
- Schwab limits order submission to 120 per minute and 4000 per day.
- Account hashes come from `client.get_account_numbers()`.
