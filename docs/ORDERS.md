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
with `with_session` / `with_duration`. `order.validate()` runs before every
submission and preview and never rounds a price. It checks:

- each order type carries exactly its fields (a `MARKET` order has no
  `price`, a `LIMIT` no `stopPrice` or trailing fields, a net-priced order
  two or more legs), and `CABINET`, `EXERCISE` and `NON_MARKETABLE` are refused;
- extended-hours sessions (`AM`, `PM`, `SEAMLESS`) take `LIMIT` orders only,
  and on-close orders need `NORMAL` / `DAY`;
- equity legs use `BUY` / `SELL` / `SELL_SHORT` / `SELL_SHORT_EXEMPT` /
  `BUY_TO_COVER` in whole shares, option legs the `*_TO_OPEN` / `*_TO_CLOSE`
  instructions on a canonical OSI symbol in whole contracts; symbols are
  upper case and unpadded;
- a top-level `quantity`, if present, is the unit count: a single leg trades
  exactly it, and each leg of a multi-leg order a whole multiple of it;
- options trade only in the `NORMAL` session; several legs are priced
  together (`NET_DEBIT`, `NET_CREDIT`, `NET_ZERO` or `MARKET`), no leg
  repeats another, and a named strategy (`VERTICAL`, `IRON_CONDOR`, …) has
  its number of legs on one underlying;
- a trailing offset in ticks is a whole number of ticks;
- trailing stops trail a real basis (not `MANUAL`), by less than 100%;
- an `OCO` parent carries nothing executable, and strategies nest at most
  eight levels;
- no field holds a value Schwab does not document. Request and response
  enums are shared, and responses decode unknown values as `Unknown`; a
  request carrying one is refused.

`validate_price_increment(tick)` enforces a tick you supply on prices, stop
prices and value trailing offsets. Decimals parse exactly: `"1_000"`,
exponent notation, padding and more digits than a decimal holds are errors,
not reinterpretations.

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
`take_profit > stop_loss`, short entries the reverse. When the entry has a
limit price the exits must also straddle it (a long entry at 150 needs its
stop below 150 and its target above), since otherwise an exit is
marketable the moment the entry fills. The MCP risk policy
sums entry, take-profit and stop-loss as gross exposure and prices the
stop-loss at the entry's limit (a child that only flattens what its bounded
parent opens cannot fill for more), so size `max_quantity` / `max_notional`
for three legs; short brackets are refused like any other opening short.

## Other request fields

| Setter | Wire field |
|---|---|
| `with_tag(tag)` | `tag` — your idempotency / intent identifier |
| `with_special_instruction(SpecialInstruction)` | `specialInstruction` (`ALL_OR_NONE`, `DO_NOT_REDUCE`, …) |
| `with_destination(RequestedDestination)` | `requestedDestination` (`INET`, `NYSE`, `AUTO`, …) |
| `with_tax_lot_method(TaxLotMethod)` | `taxLotMethod` (`FIFO`, `LIFO`, `SPECIFIC_LOT`, …) |
| `with_stop_type(StopType)` | `stopType` (`STANDARD`, `BID`, `ASK`, `LAST`, `MARK`) |
| `with_complex_strategy(ComplexOrderStrategyType)` | `complexOrderStrategyType` |

A tag identifies one intent. The SDK assigns a random one when you leave it
out, and reconciliation matches broker orders to intents by it. Once an
intent is accepted (or reconciled) its tag is spent for that account: any
later `place_order` or `replace_order` carrying it, the replacement of the
same order included, fails with `Error::Duplicate`. A rejected intent frees
its tag. Give each replacement its own tag, for example `swing-1-r1`.

Every field is also a public member of `OrderRequest`, so anything without a
constructor (`TRAILING_STOP_LIMIT`, `LIMIT_ON_CLOSE`, `priceLinkBasis`, …)
can be set directly or deserialized from JSON. The paper engine and the MCP
policy reject some of these fields; see their docs.

`TRAILING_STOP_LIMIT` deliberately has no constructor. Schwab's request
schema carries `priceLinkBasis` and `priceLinkType` but no `priceOffset`, and
publishes no example of the type, so whether the limit is a fixed `price` or
trails alongside the stop is unspecified. `validate()` takes the only reading
the schema supports, a fixed `price` plus the trailing-stop trio
(`stopPriceLinkBasis`, `stopPriceLinkType`, `stopPriceOffset`); start from
`trailing_stop(...)`, set `order_type` and `price`, and confirm the shape with
`preview_order` before relying on it.

## JSON specs and the MCP `build_order` tool

`OrderSpec` is the same composition in JSON, keyed by `kind`. The MCP
server's `build_order` tool takes one and returns the expanded, validated
`OrderRequest` JSON to pass to `preview_order` / `place_order`, so the
operator-approved intent and the request Schwab receives stay one shape.
`check_order` takes that JSON and reports, locally, every reason the
server itself would refuse the submission (policy limits, approval, the
trading switch, halts) together with the intent digest to approve.
`OrderSpec::build()` (or `OrderRequest::try_from(spec)`) does the same in
Rust for config-driven callers.

```json
{
  "kind": "bracket",
  "entry": {
    "kind": "limit",
    "leg": {
      "instruction": "BUY_TO_OPEN", "quantity": "1",
      "instrument": {"asset_type": "OPTION", "underlying": "AAPL",
                     "expiration": "2024-05-17", "right": "PUT", "strike": "190"},
      "position_effect": "OPENING"
    },
    "price": "2.50",
    "settings": {"duration": "GOOD_TILL_CANCEL", "tag": "hedge-1"}
  },
  "take_profit": "4.00",
  "stop_loss": "1.50"
}
```

Single kinds (`market`, `limit`, `stop`, `stop_limit`, `trailing_stop`)
take `leg`; the net-priced kinds (`net_debit`, `net_credit`, `net_zero`)
take `legs`; all accept `settings` with `session`, `duration`, `tag`,
`complex_strategy`, `special_instruction`, `destination`,
`tax_lot_method` and `stop_type`. `oco` takes `first` / `second`,
`trigger` takes `entry` / `children`, `bracket` takes `entry` /
`take_profit` / `stop_loss`. Option instruments give either an OSI
`symbol` or all of `underlying`, `expiration`, `right`, `strike`. Unknown
keys are errors.

## Tips

- Symbols: equities `AAPL`; indexes `$SPX`; options via `OptionSymbol`.
- Schwab limits order submission to 120 per minute and 4000 per day.
- Account hashes come from `client.get_account_numbers()`.
