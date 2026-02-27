# Placing Orders

> [!CAUTION]
> **SAFETY FIRST**: Always use the `preview_order` method to validate your logic before executing a live trade. It is highly recommended to verify all automated strategies using a test account or Schwab's Paper Money environment. Placing live orders via this SDK involves significant financial risk.

This guide provides detailed examples for placing various types of orders using the Schwab Rust SDK.

## Order Methods

### `client.place_order(account_hash, &order)`
Places an order for the account specified by account_hash.

### `client.preview_order(account_hash, &order)`
Returns a preview of the order (fees, validation, balances, etc.) without actually placing it. Highly recommended for testing.

### `client.replace_order(account_hash, order_id, &order)`
Replaces an existing order with a new order definition.

### `client.cancel_order(account_hash, order_id)`
Cancels a specific order.

---

## Order Examples

### 1. Buy Equities (Market Order)

```rust
use schwab_rs::types::trading::*;

let order = Order {
    order_type: OrderType::Market,
    session: OrderSession::Normal,
    duration: OrderDuration::Day,
    order_strategy_type: Some(OrderStrategyType::Single),
    order_leg_collection: vec![OrderLeg {
        order_leg_type: OrderLegType::Equity,
        instrument: OrderInstrument {
            symbol: "AMD".to_string(),
            asset_type: "EQUITY".to_string(),
            cusip: None,
            description: None,
        },
        instruction: OrderInstruction::Buy,
        quantity: 10.0,
        position_effect: Some(PositionEffect::Automatic),
        leg_id: None,
    }],
    ..Default::default()
};

let response = client.place_order("your_account_hash", &order).await?;
```

### 2. Buy Equities (Limit Order)

```rust
let order = Order {
    order_type: OrderType::Limit,
    price: Some(150.00),
    session: OrderSession::Normal,
    duration: OrderDuration::Day,
    order_strategy_type: Some(OrderStrategyType::Single),
    order_leg_collection: vec![OrderLeg {
        order_leg_type: OrderLegType::Equity,
        instrument: OrderInstrument {
            symbol: "AAPL".to_string(),
            asset_type: "EQUITY".to_string(),
            cusip: None,
            description: None,
        },
        instruction: OrderInstruction::Buy,
        quantity: 5.0,
        position_effect: Some(PositionEffect::Automatic),
        leg_id: None,
    }],
    ..Default::default()
};
```

### 3. Sell Equities (Stop Order)

```rust
let order = Order {
    order_type: OrderType::Stop,
    stop_price: Some(140.00), // Note: Ensure your Order struct has stop_price or use price field based on Schwab docs
    session: OrderSession::Normal,
    duration: OrderDuration::Gtc,
    order_strategy_type: Some(OrderStrategyType::Single),
    order_leg_collection: vec![OrderLeg {
        order_leg_type: OrderLegType::Equity,
        instrument: OrderInstrument {
            symbol: "AAPL".to_string(),
            asset_type: "EQUITY".to_string(),
            cusip: None,
            description: None,
        },
        instruction: OrderInstruction::Sell,
        quantity: 5.0,
        position_effect: Some(PositionEffect::Automatic),
        leg_id: None,
    }],
    ..Default::default()
};
```

### 4. Buy Single Option (Buy to Open)

Option symbols follow a specific format: `Symbol (6 chars) + YYMMDD + C/P + Strike (8 digits)`.
Example: `AAPL  240517P00190000`

```rust
let order = Order {
    order_type: OrderType::Limit,
    price: Some(2.50),
    session: OrderSession::Normal,
    duration: OrderDuration::Day,
    order_strategy_type: Some(OrderStrategyType::Single),
    order_leg_collection: vec![OrderLeg {
        order_leg_type: OrderLegType::Option,
        instrument: OrderInstrument {
            symbol: "AAPL  240517P00190000".to_string(),
            asset_type: "OPTION".to_string(),
            cusip: None,
            description: None,
        },
        instruction: OrderInstruction::BuyToOpen,
        quantity: 1.0,
        position_effect: Some(PositionEffect::Opening),
        leg_id: None,
    }],
    ..Default::default()
};
```

### 5. Vertical Spread (Complex Order)

```rust
let order = Order {
    order_type: OrderType::NetDebit,
    price: Some(1.20),
    complex_order_strategy_type: Some(ComplexOrderStrategyType::Vertical),
    session: OrderSession::Normal,
    duration: OrderDuration::Day,
    order_strategy_type: Some(OrderStrategyType::Single),
    order_leg_collection: vec![
        OrderLeg {
            order_leg_type: OrderLegType::Option,
            instrument: OrderInstrument {
                symbol: "AAPL  240517C00190000".to_string(),
                asset_type: "OPTION".to_string(),
                cusip: None,
                description: None,
            },
            instruction: OrderInstruction::BuyToOpen,
            quantity: 1.0,
            position_effect: Some(PositionEffect::Opening),
            leg_id: None,
        },
        OrderLeg {
            order_leg_type: OrderLegType::Option,
            instrument: OrderInstrument {
                symbol: "AAPL  240517C00195000".to_string(),
                asset_type: "OPTION".to_string(),
                cusip: None,
                description: None,
            },
            instruction: OrderInstruction::SellToOpen,
            quantity: 1.0,
            position_effect: Some(PositionEffect::Opening),
            leg_id: None,
        },
    ],
    ..Default::default()
};
```

## Tips for Success

*   **Use `preview_order`**: Always preview your order before placing it to see calculated fees and validation errors.
*   **Symbol Formatting**: 
    *   Equities: `AAPL`, `MSFT`
    *   Indexes: `$SPX`, `$DJI`
    *   Options: `AAPL  240517P00190000` (Note the spaces)
*   **Limits**: 120 orders per minute, 4000 per day.
*   **Account Hash**: You can find your account hash by calling `client.get_accounts()`.
