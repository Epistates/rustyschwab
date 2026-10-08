//! `get_order_status`: one order reduced to what deciding the next step
//! needs (status, fills, whether Schwab can still change it), optionally
//! waited on, for a bounded time, until it cannot.
//!
//! The raw `get_order` answers with Schwab's full shape; this reads the
//! same endpoint and does the arithmetic a model otherwise does by hand:
//! filled and remaining quantity, the quantity-weighted average fill
//! price over the execution activities, the latest of the order's times,
//! and the same for each child of a trigger or one-cancels-other order.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use schwab_types::trading::{Order, OrderActivityType, OrderLeg, OrderStatus};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::backend::Backend;
use crate::contract::ToolFailure;

/// The longest a call may wait for a terminal status.
pub const MAX_WAIT_MS: u64 = 30_000;
/// How often a waiting call re-reads the order.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The order as `get_order_status` reports it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderStatusReport {
    pub order_id: Option<String>,
    pub status: Option<OrderStatus>,
    /// Schwab's reason, when it gives one (a rejection, typically).
    pub status_description: Option<String>,
    /// Schwab will not change the order again ([`OrderStatus::is_terminal`]).
    pub terminal: bool,
    pub cancelable: Option<bool>,
    pub editable: Option<bool>,
    pub tag: Option<String>,
    pub quantity: Option<f64>,
    pub filled: Option<f64>,
    pub remaining: Option<f64>,
    /// Quantity-weighted mean price of every execution leg, to four
    /// places; null before the first fill. For a multi-leg order this
    /// averages across legs: read the per-leg prices instead.
    pub avg_fill_price: Option<f64>,
    /// Execution activities, whole or partial.
    pub fills: usize,
    pub legs: Vec<Value>,
    pub entered_time: Option<String>,
    pub close_time: Option<String>,
    /// The latest of the order's own times (entered, released, canceled,
    /// closed, last fill), verbatim.
    pub last_update: Option<String>,
    /// Ids of the orders that replaced this one.
    pub replaced_by: Vec<String>,
    /// Children of a trigger or one-cancels-other order, each reported
    /// the same way.
    pub children: Vec<OrderStatusReport>,
    /// Present when the call waited: how long, and how many reads.
    pub waited: Option<Waited>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Waited {
    pub ms: u64,
    pub reads: u32,
}

/// Fill quantity and notional accumulated for one leg.
#[derive(Default)]
struct Fill {
    quantity: f64,
    notional: f64,
}

impl Fill {
    fn add(&mut self, quantity: f64, price: f64) {
        self.quantity += quantity;
        self.notional += quantity * price;
    }

    fn average(&self) -> Option<f64> {
        (self.quantity > 0.0).then(|| (self.notional / self.quantity * 10_000.0).round() / 10_000.0)
    }
}

/// One order leg as the model-facing tools describe it.
pub(crate) fn leg(leg: &OrderLeg) -> Map<String, Value> {
    let Value::Object(map) = json!({
        "legId": leg.leg_id,
        "instruction": leg.instruction,
        "quantity": leg.quantity,
        "symbol": leg.instrument.symbol,
        "assetType": leg.instrument.asset_type,
        "positionEffect": leg.position_effect,
    }) else {
        unreachable!("json! object literal")
    };
    map
}

/// Schwab writes order times as `2025-01-15T14:31:22+0000`; accept the
/// RFC 3339 spellings too.
fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    schwab_types::common::parse_schwab_time(text).ok()
}

impl OrderStatusReport {
    pub fn of(order: Order) -> Self {
        // Fills by leg id; legs Schwab did not number pool under `None`.
        let mut by_leg: BTreeMap<Option<i64>, Fill> = BTreeMap::new();
        let mut total = Fill::default();
        let mut fills = 0;
        let mut times: Vec<&str> = [
            &order.entered_time,
            &order.release_time,
            &order.cancel_time,
            &order.close_time,
        ]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect();
        for activity in &order.order_activity_collection {
            // `executionType` is documented as `FILL` only, yet `CANCELED`
            // arrives with a zero price: count nothing that says otherwise.
            let execution = activity.activity_type == Some(OrderActivityType::Execution)
                && activity
                    .execution_type
                    .as_deref()
                    .is_none_or(|kind| kind == "FILL");
            if !execution {
                continue;
            }
            fills += 1;
            for leg in &activity.execution_legs {
                times.extend(leg.time.as_deref());
                if let (Some(quantity), Some(price)) = (leg.quantity, leg.price) {
                    by_leg.entry(leg.leg_id).or_default().add(quantity, price);
                    total.add(quantity, price);
                }
            }
        }
        let last_update = times
            .iter()
            .filter_map(|text| parse_time(text).map(|at| (at, *text)))
            .max_by_key(|(at, _)| *at)
            .map(|(_, text)| text.to_owned());
        let legs = order
            .order_leg_collection
            .iter()
            .map(|order_leg| {
                let mut map = leg(order_leg);
                let fill = by_leg.get(&order_leg.leg_id);
                map.insert("filled".into(), json!(fill.map(|f| f.quantity)));
                map.insert("avgFillPrice".into(), json!(fill.and_then(Fill::average)));
                Value::Object(map)
            })
            .collect();
        let filled = order
            .filled_quantity
            .or((fills > 0).then_some(total.quantity));
        let remaining = order
            .remaining_quantity
            .or_else(|| Some(order.quantity? - filled?));
        Self {
            terminal: order.status.is_some_and(OrderStatus::is_terminal),
            order_id: order.order_id,
            status: order.status,
            status_description: order.status_description,
            cancelable: order.cancelable,
            editable: order.editable,
            tag: order.tag,
            quantity: order.quantity,
            filled,
            remaining,
            avg_fill_price: total.average(),
            fills,
            legs,
            entered_time: order.entered_time,
            close_time: order.close_time,
            last_update,
            replaced_by: order
                .replacing_order_collection
                .into_iter()
                .filter_map(|replacement| replacement.order_id)
                .collect(),
            children: order
                .child_order_strategies
                .into_iter()
                .map(Self::of)
                .collect(),
            waited: None,
        }
    }
}

/// Read the order, and with `wait_ms` keep reading once a second until
/// the venue will not change it again or the wait elapses, whichever is
/// first; the last read lands at the deadline.
pub async fn report(
    backend: &Backend,
    account_hash: &str,
    order_id: &str,
    wait_ms: Option<u64>,
) -> Result<OrderStatusReport, ToolFailure> {
    let wait = match wait_ms {
        None | Some(0) => None,
        Some(ms) if ms > MAX_WAIT_MS => {
            return Err(ToolFailure::invalid_arguments(format!(
                "wait_ms is at most {MAX_WAIT_MS}; call again to keep waiting"
            )));
        }
        Some(ms) => Some(Duration::from_millis(ms)),
    };
    let started = Instant::now();
    let mut reads = 0;
    loop {
        let mut report = OrderStatusReport::of(backend.order(account_hash, order_id).await?);
        reads += 1;
        let Some(wait) = wait else {
            return Ok(report);
        };
        let remaining = wait.saturating_sub(started.elapsed());
        if report.terminal || remaining.is_zero() {
            report.waited = Some(Waited {
                ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                reads,
            });
            return Ok(report);
        }
        tokio::time::sleep(remaining.min(POLL_INTERVAL)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(value: Value) -> Order {
        serde_json::from_value(value).unwrap()
    }

    /// Partial fills average by quantity, per leg and overall; a canceled
    /// "execution" with its zero price is not a fill; the latest time wins.
    #[test]
    fn fills_average_by_quantity_and_ignore_cancellations() {
        let report = OrderStatusReport::of(order(json!({
            "orderId": 7, "status": "CANCELED", "quantity": 10, "filledQuantity": 6,
            "remainingQuantity": 0, "cancelable": false, "editable": false,
            "enteredTime": "2025-01-15T14:31:22+0000", "closeTime": "2025-01-15T14:40:00+0000",
            "orderLegCollection": [{"legId": 1, "orderLegType": "EQUITY", "instruction": "BUY",
                "quantity": 10, "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}}],
            "orderActivityCollection": [
                {"activityType": "EXECUTION", "executionType": "FILL", "quantity": 4,
                 "executionLegs": [{"legId": 1, "quantity": 4, "price": 150.0,
                                    "time": "2025-01-15T14:32:00+0000"}]},
                {"activityType": "EXECUTION", "executionType": "FILL", "quantity": 2,
                 "executionLegs": [{"legId": 1, "quantity": 2, "price": 150.3,
                                    "time": "2025-01-15T14:33:00+0000"}]},
                {"activityType": "EXECUTION", "executionType": "CANCELED", "quantity": 4,
                 "executionLegs": [{"legId": 1, "quantity": 4, "price": 0.0,
                                    "time": "2025-01-15T14:40:00+0000"}]}
            ]
        })));
        assert_eq!(report.order_id.as_deref(), Some("7"));
        assert!(report.terminal);
        assert_eq!((report.filled, report.remaining), (Some(6.0), Some(0.0)));
        assert_eq!(report.avg_fill_price, Some(150.1));
        assert_eq!(report.fills, 2);
        assert_eq!(report.legs[0]["filled"], 6.0);
        assert_eq!(report.legs[0]["avgFillPrice"], 150.1);
        assert_eq!(report.legs[0]["symbol"], "AAPL");
        assert_eq!(
            report.last_update.as_deref(),
            Some("2025-01-15T14:40:00+0000")
        );
        assert!(report.waited.is_none());
    }

    /// An unfilled working order derives its quantities, and a trigger
    /// order's children come along, each judged on its own status.
    #[test]
    fn working_orders_derive_quantities_and_carry_children() {
        let report = OrderStatusReport::of(order(json!({
            "orderId": 30, "status": "FILLED", "quantity": 100, "filledQuantity": 100,
            "remainingQuantity": 0, "orderStrategyType": "TRIGGER",
            "enteredTime": "2025-01-15T14:30:00+0000",
            "orderLegCollection": [{"legId": 1, "orderLegType": "EQUITY", "instruction": "BUY",
                "quantity": 100, "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}}],
            "childOrderStrategies": [{
                "orderId": 31, "status": "WORKING", "quantity": 100, "price": 160.0,
                "enteredTime": "2025-01-15T14:30:01+0000",
                "orderLegCollection": [{"legId": 1, "orderLegType": "EQUITY",
                    "instruction": "SELL", "quantity": 100, "positionEffect": "CLOSING",
                    "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}}]
            }],
            "replacingOrderCollection": [{"orderId": 40}]
        })));
        assert!(report.terminal);
        assert_eq!(report.replaced_by, ["40"]);
        let child = &report.children[0];
        assert_eq!(child.order_id.as_deref(), Some("31"));
        assert!(!child.terminal);
        assert_eq!((child.filled, child.remaining), (None, None));
        assert_eq!(child.avg_fill_price, None);
        assert_eq!(child.legs[0]["filled"], Value::Null);
        assert_eq!(child.legs[0]["positionEffect"], "CLOSING");
        assert_eq!(
            child.last_update.as_deref(),
            Some("2025-01-15T14:30:01+0000")
        );

        let unknown = OrderStatusReport::of(order(json!({
            "orderId": 1, "status": "SOMETHING_NEW", "quantity": 5, "filledQuantity": 2
        })));
        assert_eq!(unknown.status, Some(OrderStatus::Unknown));
        assert!(!unknown.terminal, "an unknown status may still fill");
        assert_eq!(unknown.remaining, Some(3.0));
    }

    #[test]
    fn schwab_and_rfc3339_times_both_parse() {
        let plain = parse_time("2025-01-15T14:31:22+0000").unwrap();
        let fraction = parse_time("2025-01-15T14:31:22.000+0000").unwrap();
        let rfc = parse_time("2025-01-15T09:31:22-05:00").unwrap();
        assert_eq!(plain, fraction);
        assert_eq!(plain, rfc);
        assert!(parse_time("yesterday").is_none());
    }
}
