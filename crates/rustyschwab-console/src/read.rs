//! Views built from the JSON Schwab (or the paper venue) reports, read by
//! field so a missing or added field never hides the rest.

use serde_json::Value;

use crate::snapshot::{AccountView, OrderView, PositionView, QuoteView};

fn number(value: &Value, pointer: &str) -> Option<f64> {
    let field = value.pointer(pointer)?;
    field
        .as_f64()
        .or_else(|| field.as_str().and_then(|s| s.parse().ok()))
}

fn text(value: &Value, pointer: &str) -> Option<String> {
    let field = value.pointer(pointer)?;
    field
        .as_str()
        .map(str::to_owned)
        .or_else(|| field.as_i64().map(|n| n.to_string()))
}

/// The sum of `values`, rounded to cents; `None` when none is present.
fn sum(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    values
        .flatten()
        .fold(None, |total: Option<f64>, v| Some(total.unwrap_or(0.0) + v))
        .map(|total| (total * 100.0).round() / 100.0)
}

/// An account from Schwab's account JSON (`{"securitiesAccount": ...}`).
pub(crate) fn account(hash: &str, account: &Value) -> AccountView {
    let securities = &account["securitiesAccount"];
    let positions: Vec<PositionView> = securities["positions"]
        .as_array()
        .map(|positions| positions.iter().map(position).collect())
        .unwrap_or_default();
    let balance = |field: &str| number(securities, &format!("/currentBalances/{field}"));
    let number = text(securities, "/accountNumber").unwrap_or_default();
    let last4: String = number
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    AccountView {
        hash: hash.to_owned(),
        number: if last4.is_empty() {
            hash.chars().take(8).collect()
        } else {
            format!("…{last4}")
        },
        liquidation_value: balance("liquidationValue"),
        cash: balance("cashBalance"),
        buying_power: balance("buyingPower").or_else(|| balance("cashAvailableForTrading")),
        day_pnl: sum(positions.iter().map(|p| p.day_pnl)),
        open_pnl: sum(positions.iter().map(|p| p.open_pnl)),
        positions,
        observed_at: None,
    }
}

fn position(position: &Value) -> PositionView {
    let long = number(position, "/longQuantity").unwrap_or(0.0);
    let short = number(position, "/shortQuantity").unwrap_or(0.0);
    let open = sum([
        number(position, "/longOpenProfitLoss"),
        number(position, "/shortOpenProfitLoss"),
    ]
    .into_iter());
    PositionView {
        symbol: text(position, "/instrument/symbol").unwrap_or_else(|| "?".into()),
        quantity: long - short,
        average_price: number(position, "/averagePrice"),
        market_value: number(position, "/marketValue"),
        day_pnl: number(position, "/currentDayProfitLoss"),
        open_pnl: open,
    }
}

/// Whether Schwab may still change an order in `status`.
pub(crate) fn working(status: &str) -> bool {
    !matches!(
        status,
        "FILLED" | "CANCELED" | "REJECTED" | "EXPIRED" | "REPLACED" | "UNKNOWN"
    )
}

/// `order` and the children of its strategy, flattened, in that order.
pub(crate) fn orders(account: &str, order: &Value, into: &mut Vec<OrderView>) {
    if let Some(id) = text(order, "/orderId") {
        let legs = order["orderLegCollection"]
            .as_array()
            .map(|legs| {
                legs.iter()
                    .map(|leg| {
                        let quantity = number(leg, "/quantity")
                            .map(|q| format!(" {q}"))
                            .unwrap_or_default();
                        format!(
                            "{}{quantity} {}",
                            text(leg, "/instruction").unwrap_or_default(),
                            text(leg, "/instrument/symbol").unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" / ")
            })
            .unwrap_or_default();
        let status = text(order, "/status").unwrap_or_else(|| "?".into());
        into.push(OrderView {
            account: account.to_owned(),
            order_id: id,
            principal: None,
            working: working(&status),
            status,
            legs,
            order_type: text(order, "/orderType").unwrap_or_default(),
            price: number(order, "/price"),
            stop_price: number(order, "/stopPrice"),
            quantity: number(order, "/quantity"),
            filled: number(order, "/filledQuantity"),
            entered: text(order, "/enteredTime"),
        });
    }
    if let Some(children) = order["childOrderStrategies"].as_array() {
        for child in children {
            orders(account, child, into);
        }
    }
}

/// The symbol's quote from a quotes response (`{"AAPL": {"quote": ...}}`).
pub(crate) fn quote(symbol: &str, quotes: &Value) -> Option<QuoteView> {
    let entry = quotes.get(symbol)?;
    let quote = entry.get("quote").unwrap_or(entry);
    Some(QuoteView {
        symbol: symbol.to_owned(),
        last: number(quote, "/lastPrice").or_else(|| number(quote, "/mark")),
        bid: number(quote, "/bidPrice"),
        ask: number(quote, "/askPrice"),
        change: number(quote, "/netChange"),
        change_percent: number(quote, "/netPercentChange"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accounts_sum_positions_and_mask_the_number() {
        let view = account(
            "HASH",
            &json!({"securitiesAccount": {"accountNumber": "12345678",
            "currentBalances": {"liquidationValue": 1000.5, "cashAvailableForTrading": 200},
            "positions": [
                {"instrument": {"symbol": "AAPL"}, "longQuantity": 10, "shortQuantity": 0,
                 "currentDayProfitLoss": 5.004, "longOpenProfitLoss": 20},
                {"instrument": {"symbol": "MSFT"}, "longQuantity": 0, "shortQuantity": 2,
                 "currentDayProfitLoss": -1, "shortOpenProfitLoss": -3}
            ]}}),
        );
        assert_eq!(view.number, "…5678");
        assert_eq!(view.buying_power, Some(200.0));
        assert_eq!(view.day_pnl, Some(4.0));
        assert_eq!(view.open_pnl, Some(17.0));
        assert_eq!(view.positions[1].quantity, -2.0);
    }

    #[test]
    fn strategies_flatten_with_their_children() {
        let mut views = Vec::new();
        orders(
            "A",
            &json!({"orderId": 1, "status": "FILLED", "orderType": "LIMIT",
                "orderLegCollection": [{"instruction": "BUY", "quantity": 10,
                    "instrument": {"symbol": "AAPL"}}],
                "childOrderStrategies": [{"orderId": 2, "status": "WORKING",
                    "orderType": "STOP", "stopPrice": 150}]}),
            &mut views,
        );
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].legs, "BUY 10 AAPL");
        assert!(!views[0].working);
        assert!(views[1].working);
        assert_eq!(views[1].stop_price, Some(150.0));
    }
}
