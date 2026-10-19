# OrderSpec

The compact order description `build_order` expands, locally and without
contacting Schwab, into the exact `order` JSON that `check_order`,
`preview_order`, `place_order` and `replace_order` accept. Every spec is
an object keyed by `kind`; unknown keys are rejected. Quantities and prices
are decimal strings (`"10"`, `"150.25"`), never binary floats, and must be
positive.

## Kinds

| `kind` | Fields | Expands to |
| --- | --- | --- |
| `market` | `leg`, `settings?` | `MARKET`: fill at the market |
| `limit` | `leg`, `price`, `settings?` | `LIMIT`: fill at `price` or better |
| `stop` | `leg`, `stop_price`, `settings?` | `STOP`: a market order once `stop_price` trades |
| `stop_limit` | `leg`, `stop_price`, `price`, `settings?` | `STOP_LIMIT`: a limit at `price` once `stop_price` trades |
| `trailing_stop` | `leg`, `basis`, `link`, `offset`, `settings?` | `TRAILING_STOP`: a stop trailing `basis` (`LAST`, `BID`, `ASK`, `MARK`, …) by `offset`, expressed as `link` (`VALUE`, `PERCENT`, `TICK`) |
| `net_debit` | `legs`, `price`, `settings?` | multi-leg order paying at most `price` net |
| `net_credit` | `legs`, `price`, `settings?` | multi-leg order collecting at least `price` net |
| `net_zero` | `legs`, `settings?` | multi-leg order filling for zero net cost |
| `oco` | `first`, `second` | two specs; a fill on either cancels the other |
| `trigger` | `entry`, `children` | `children` (one or more specs) are submitted once `entry` fills |
| `bracket` | `entry`, `take_profit`, `stop_loss` | `entry` triggers a good-till-cancel take-profit limit and stop-loss pair that close the entry leg |

## Leg

```json
{"instruction": "BUY", "quantity": "10",
 "instrument": {"asset_type": "EQUITY", "symbol": "AAPL"},
 "position_effect": "OPENING"}
```

`instruction` is one of `BUY`, `SELL`, `BUY_TO_COVER`, `SELL_SHORT`,
`BUY_TO_OPEN`, `BUY_TO_CLOSE`, `SELL_TO_OPEN`, `SELL_TO_CLOSE`; use the
`_TO_OPEN` / `_TO_CLOSE` forms for options. `quantity` is shares, or
contracts for options. `position_effect` (`OPENING`, `CLOSING`,
`AUTOMATIC`) is optional.

An instrument is keyed by `asset_type`:

- `{"asset_type": "EQUITY", "symbol": "AAPL"}`
- `{"asset_type": "OPTION", "symbol": "AAPL  240517P00190000"}`: an OSI
  symbol, as `get_option_contracts` and `get_option_chain` report it
- `{"asset_type": "OPTION", "underlying": "AAPL", "expiration": "2024-05-17",
  "right": "PUT", "strike": "190"}`: the contract by parts

An option takes the OSI symbol or all four parts, never a mix.

## Settings

Every single-order kind accepts `settings`, all of whose fields are
optional:

```json
{"session": "NORMAL", "duration": "DAY", "tag": "swing-1",
 "complex_strategy": "VERTICAL", "special_instruction": "ALL_OR_NONE",
 "destination": "AUTO", "tax_lot_method": "FIFO", "stop_type": "MARK"}
```

`session` defaults to `NORMAL` (`AM`, `PM`, `SEAMLESS`); `duration` to `DAY`
(`GOOD_TILL_CANCEL`, `FILL_OR_KILL`, `IMMEDIATE_OR_CANCEL`, `END_OF_WEEK`,
`END_OF_MONTH`, `NEXT_END_OF_MONTH`). Equity orders take only `DAY`,
`GOOD_TILL_CANCEL` or `FILL_OR_KILL`; Schwab refuses the others. Leave `tag` unset: `place_order` and
`replace_order` then tag the order with its intent digest, so submitting
the identical order again is refused as `duplicate_intent`. Set a `tag`
only to place the same order twice on purpose.

## Rules the schema cannot express

- Prices, stop prices, offsets and quantities are positive.
- `trigger` needs at least one child.
- `bracket` needs a single-leg `entry` whose instruction opens a position
  (`BUY`, `BUY_TO_OPEN`, `SELL_SHORT`, `SELL_TO_OPEN`). For a long entry
  `take_profit` must be above `stop_loss`; for a short entry, below. When
  the entry is a `limit` or `stop_limit`, the exits must also straddle its
  price. The exits use the closing instruction and the entry's full
  quantity, and are `GOOD_TILL_CANCEL` regardless of the entry's duration.
- Equity legs trade whole shares with `BUY`, `SELL`, `SELL_SHORT`,
  `SELL_SHORT_EXEMPT` or `BUY_TO_COVER`; option legs trade whole contracts
  with `BUY_TO_OPEN`, `BUY_TO_CLOSE`, `SELL_TO_OPEN` or `SELL_TO_CLOSE`.
  Symbols are upper case.
- A `session` other than `NORMAL` takes `limit` orders only.
- An equity order's `duration` is `DAY`, `GOOD_TILL_CANCEL` or
  `FILL_OR_KILL`.
- Trailing stops cannot trail `MANUAL`, and a `PERCENT` offset is below 100.
- Strategies nest at most eight levels.

## What policy accepts

A principal's risk limits are judged on the order's bounded exposure, so
under a principal policy an order must have a finite worst case:

- One leg that is not an opening short: buys, and sells that close, are
  accepted; a lone `SELL_SHORT` or `SELL_TO_OPEN` is refused for want of a
  portfolio risk engine.
- Or a defined-risk option strategy: option legs on one underlying and one
  expiration, priced `net_debit`, `net_credit` or `net_zero` (verticals,
  iron condors, butterflies, put ratios). Its exposure is its contracts
  and the most it can lose at expiration, premium included; a strategy that
  sells more calls than it buys has no bound and is refused, as are legs at
  different expirations. Every leg needs a multiplier the operator verified.
- A bounded price: `limit`, `stop_limit`, or a limit-on-close order. A
  `market`, `stop` or `trailing_stop` on its own has no finite cost and is
  refused; a `bracket` with a `limit` entry is accepted, its exits being
  priced at the entry. (`net_debit` and `net_credit` need two or more legs,
  so the rule above refuses them.)
- An option contract needs an operator-configured multiplier for its
  symbol.

`check_order` reports the exposure (`quantity`, `notional`) against the
limits before anything is submitted. The paper venue models whole-share
equity `BUY` and `SELL` legs only.

## Examples

A limit buy:

```json
{"kind": "limit", "price": "150.25",
 "leg": {"instruction": "BUY", "quantity": "10",
         "instrument": {"asset_type": "EQUITY", "symbol": "AAPL"}}}
```

expands to

```json
{"orderType": "LIMIT", "session": "NORMAL", "duration": "DAY", "price": "150.25",
 "orderStrategyType": "SINGLE",
 "orderLegCollection": [{"instruction": "BUY", "quantity": "10",
                         "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}}]}
```

A bracket around that entry (take profit at 160, stop at 145):

```json
{"kind": "bracket", "take_profit": "160", "stop_loss": "145",
 "entry": {"kind": "limit", "price": "150.25",
           "leg": {"instruction": "BUY", "quantity": "10",
                   "instrument": {"asset_type": "EQUITY", "symbol": "AAPL"}}}}
```

A put credit spread by contract parts:

```json
{"kind": "net_credit", "price": "1.05", "settings": {"complex_strategy": "VERTICAL"},
 "legs": [
   {"instruction": "SELL_TO_OPEN", "quantity": "1",
    "instrument": {"asset_type": "OPTION", "underlying": "AAPL",
                   "expiration": "2024-05-17", "right": "PUT", "strike": "190"}},
   {"instruction": "BUY_TO_OPEN", "quantity": "1",
    "instrument": {"asset_type": "OPTION", "underlying": "AAPL",
                   "expiration": "2024-05-17", "right": "PUT", "strike": "185"}}]}
```

A sell to close with a trailing stop 3% under the last trade:

```json
{"kind": "trailing_stop", "basis": "LAST", "link": "PERCENT", "offset": "3",
 "leg": {"instruction": "SELL", "quantity": "10",
         "instrument": {"asset_type": "EQUITY", "symbol": "AAPL"}}}
```
