# schwab-types

Wire types for the Charles Schwab Trader and Market Data APIs: accounts,
orders and transactions, quotes, option chains, price history, market
hours, movers, and the streamer's messages and numbered field tables.

Most users want [`schwab-rs`](https://docs.rs/schwab-rs), the SDK, which
re-exports these as `schwab_rs::types`. Depend on this crate directly to
share the types without the HTTP and streaming stack.

```toml
[dependencies]
schwab-types = "0.2"
```

## Conventions

- **Responses keep what they do not model.** Response structs carry an
  `extra` map for fields Schwab adds, and response enums decode a value
  they do not know as `Unknown` rather than failing the whole payload.
- **Requests are strict.** `OrderRequest` rejects unknown fields, and
  `validate()` refuses what Schwab would reject or misread: `Unknown` enum
  values, fields that do not fit the order type, fractional equity or
  option quantities, extended-hours orders that are not limits, and more.
- **Money is exact.** Request prices and quantities are `Decimal`, read
  from a string such as `"150.25"` or a JSON number. Prices go out as
  strings and quantities as JSON numbers, the forms Schwab's examples use.
- **Orders have constructors.** `OrderRequest::limit`, `stop`, `bracket`,
  `one_cancels_other` and friends build Schwab's documented shapes, and
  `OptionSymbol` reads and writes OSI option symbols.

## Features

- `schemars`: derives `JsonSchema` on the request types, for tool schemas
  (the `schwab-mcp` server uses it).

See the repository [README](https://github.com/epistates/rustyschwab) for
the full documentation set.
