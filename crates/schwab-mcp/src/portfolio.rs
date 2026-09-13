//! `get_portfolio_summary`: one account's balances, positions, working
//! orders and profit and loss in one bounded answer, so a model reads a
//! portfolio without three calls and the arithmetic in between.
//!
//! Two Schwab reads (the orders entered within Schwab's list horizon,
//! then the account with its positions, in that order so an order that
//! fills in between shows as both working and held rather than as
//! neither) plus this server's own record of submissions Schwab has not
//! answered. The reads are not an atomic snapshot. The account read is
//! essential; the others degrade to a null section and a warning.

use std::fmt::{self, Display};
use std::str::FromStr;

use chrono::{Duration, Utc};
use schwab_rs::SchwabClient;
use schwab_types::accounts::{Balance, Position, SecuritiesAccount};
use schwab_types::trading::{Order, OrdersParams};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::contract::ToolFailure;
use crate::order_status;
use crate::projection::{self, Page, Row};

/// Schwab lists orders entered at most this many days ago, the horizon
/// reconciliation scans too; an older working order is reachable by
/// `get_order` only.
pub const ORDER_LOOKBACK_DAYS: i64 = 59;

/// The summary as `get_portfolio_summary` reports it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortfolioSummary {
    /// Masked like every account number.
    pub account_number: String,
    #[serde(rename = "type")]
    pub account_type: Option<String>,
    pub is_day_trader: Option<bool>,
    pub round_trips: Option<i32>,
    pub is_closing_only_restricted: Option<bool>,
    pub balances: Balances,
    pub pnl: ProfitLoss,
    /// The first page of `get_positions`, which continues from
    /// `nextCursor`.
    pub positions: Page,
    /// Orders Schwab may still change, entered since `ordersSince`, oldest
    /// first, with the children of trigger and one-cancels-other orders
    /// flattened beside their parent. `truncated` means more exist:
    /// `get_account_orders` lists them. Null when the read failed (see
    /// `warnings`).
    pub working_orders: Option<Page>,
    pub orders_since: String,
    /// Submissions this server made for the account whose outcome Schwab
    /// has not answered; an order may exist that `workingOrders` does not
    /// show. Null when the journal could not be read.
    pub pending_submissions: Option<usize>,
    pub warnings: Vec<ToolFailure>,
}

/// What a trading decision needs from Schwab's current balances, plus the
/// projected block's call flag. A cash account has no buying power or
/// available funds of its own: both are its `cashAvailableForTrading`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Balances {
    pub liquidation_value: Option<f64>,
    pub equity: Option<f64>,
    pub cash_balance: Option<f64>,
    pub available_funds: Option<f64>,
    pub buying_power: Option<f64>,
    pub day_trading_buying_power: Option<f64>,
    pub long_market_value: Option<f64>,
    pub short_market_value: Option<f64>,
    pub maintenance_requirement: Option<f64>,
    pub maintenance_call: Option<f64>,
    pub reg_t_call: Option<f64>,
    pub cash_call: Option<f64>,
    pub unsettled_cash: Option<f64>,
    pub in_call: Option<bool>,
}

impl Balances {
    fn of(account: &SecuritiesAccount) -> Self {
        let current = account.current_balances.as_ref();
        let cur = |field: fn(&Balance) -> Option<f64>| current.and_then(field);
        Self {
            liquidation_value: cur(|b| b.liquidation_value),
            equity: cur(|b| b.equity),
            cash_balance: cur(|b| b.cash_balance),
            available_funds: cur(|b| b.available_funds.or(b.cash_available_for_trading)),
            buying_power: cur(|b| b.buying_power.or(b.cash_available_for_trading)),
            day_trading_buying_power: cur(|b| b.day_trading_buying_power),
            long_market_value: cur(|b| b.long_market_value),
            short_market_value: cur(|b| b.short_market_value),
            maintenance_requirement: cur(|b| b.maintenance_requirement),
            maintenance_call: cur(|b| b.maintenance_call),
            reg_t_call: cur(|b| b.reg_t_call),
            cash_call: cur(|b| b.cash_call),
            unsettled_cash: cur(|b| b.unsettled_cash),
            in_call: account
                .projected_balances
                .as_ref()
                .and_then(|b| b.is_in_call),
        }
    }
}

/// Sums of Schwab's per-position figures over the positions held now,
/// rounded to cents; null when no position reports one. Realized profit
/// and loss is not in the account read: `get_transactions` has the
/// trades.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfitLoss {
    /// Since the previous close.
    pub day: Option<f64>,
    /// Unrealized, long and short together.
    pub open: Option<f64>,
}

impl ProfitLoss {
    fn of(positions: &[Position]) -> Self {
        fn sum(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
            values
                .flatten()
                .fold(None, |acc: Option<f64>, v| Some(acc.unwrap_or(0.0) + v))
                .map(|total| (total * 100.0).round() / 100.0)
        }
        Self {
            day: sum(positions.iter().map(|p| p.current_day_profit_loss)),
            open: sum(positions
                .iter()
                .flat_map(|p| [p.long_open_profit_loss, p.short_open_profit_loss])),
        }
    }
}

pub const WORKING_ORDER_COLUMNS: &[&str] = &[
    "orderId",
    "parentOrderId",
    "status",
    "strategy",
    "orderType",
    "duration",
    "session",
    "price",
    "stopPrice",
    "quantity",
    "filledQuantity",
    "remainingQuantity",
    "enteredTime",
    "legs",
];

/// Orders working orders by Schwab's entry time, then id. The summary
/// issues no cursor for them, so the text form is never parsed back.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct OrderKey {
    entered: String,
    id: String,
}

impl Display for OrderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}|{}", self.entered, self.id)
    }
}

impl FromStr for OrderKey {
    type Err = ();
    fn from_str(text: &str) -> Result<Self, ()> {
        let (entered, id) = text.rsplit_once('|').ok_or(())?;
        Ok(Self {
            entered: entered.to_owned(),
            id: id.to_owned(),
        })
    }
}

/// One row per order Schwab may still change, children included.
fn working_rows(orders: Vec<Order>) -> Vec<Row<OrderKey>> {
    fn collect(mut order: Order, parent: Option<&str>, rows: &mut Vec<Row<OrderKey>>) {
        let children = std::mem::take(&mut order.child_order_strategies);
        if order.status.is_none_or(|status| !status.is_terminal()) {
            rows.push(Row {
                key: OrderKey {
                    entered: order.entered_time.clone().unwrap_or_default(),
                    id: order.order_id.clone().unwrap_or_default(),
                },
                value: json!([
                    order.order_id,
                    parent,
                    order.status,
                    order.order_strategy_type,
                    order.order_type,
                    order.duration,
                    order.session,
                    order.price,
                    order.stop_price,
                    order.quantity,
                    order.filled_quantity,
                    order.remaining_quantity,
                    order.entered_time,
                    order
                        .order_leg_collection
                        .iter()
                        .map(|leg| Value::Object(order_status::leg(leg)))
                        .collect::<Vec<_>>(),
                ]),
            });
        }
        for child in children {
            collect(child, order.order_id.as_deref(), rows);
        }
    }
    let mut rows = Vec::new();
    for order in orders {
        collect(order, None, &mut rows);
    }
    rows
}

fn order_page(orders: Vec<Order>, budget: usize) -> Result<Page, ToolFailure> {
    let mut page = projection::page(
        working_rows(orders),
        WORKING_ORDER_COLUMNS,
        Map::new(),
        None,
        None,
        budget,
    )?;
    // There is no tool to continue from; `truncated` alone says to list
    // the rest with `get_account_orders`.
    page.next_cursor = None;
    Ok(page)
}

/// Assemble the summary, spending half the response budget on positions
/// and half on working orders.
pub async fn collect(
    client: &SchwabClient,
    account_hash: &str,
) -> Result<PortfolioSummary, ToolFailure> {
    let now = Utc::now();
    let since = now - Duration::days(ORDER_LOOKBACK_DAYS);
    let budget = projection::budget() / 2;
    let mut warnings = Vec::new();
    let working_orders = match client
        .get_account_orders(account_hash, &OrdersParams::new(since, now))
        .await
    {
        Ok(orders) => Some(order_page(orders, budget)?),
        Err(e) => {
            warnings.push(ToolFailure::from(e));
            None
        }
    };
    let account = client
        .get_account(account_hash, true)
        .await?
        .securities_account;
    let pending_submissions = match client.execution_store().records().await {
        Ok(records) => Some(
            records
                .iter()
                .filter(|r| r.account == account_hash && r.outcome.is_none())
                .count(),
        ),
        Err(e) => {
            warnings.push(ToolFailure::from(e));
            None
        }
    };
    let balances = Balances::of(&account);
    let positions = account.positions.unwrap_or_default();
    let pnl = ProfitLoss::of(&positions);
    let positions = projection::page(
        projection::position_rows(positions),
        projection::POSITION_COLUMNS,
        Map::new(),
        None,
        None,
        budget,
    )?;
    Ok(PortfolioSummary {
        account_number: account.account_number,
        account_type: account.account_type,
        is_day_trader: account.is_day_trader,
        round_trips: account.round_trips,
        is_closing_only_restricted: account.is_closing_only_restricted,
        balances,
        pnl,
        positions,
        working_orders,
        orders_since: since.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        pending_submissions,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(value: Value) -> Order {
        serde_json::from_value(value).unwrap()
    }

    /// Terminal orders drop out, children flatten beside their parent, and
    /// the rows are in entry order.
    #[test]
    fn working_orders_flatten_children_and_drop_terminal_orders() {
        let orders = vec![
            order(json!({
                "orderId": 30, "status": "FILLED", "enteredTime": "2025-01-15T14:30:00+0000",
                "orderStrategyType": "TRIGGER",
                "orderLegCollection": [{"orderLegType": "EQUITY", "instruction": "BUY",
                    "quantity": 100, "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}}],
                "childOrderStrategies": [
                    {"orderId": 31, "status": "WORKING", "orderType": "LIMIT", "price": 160.0,
                     "duration": "GOOD_TILL_CANCEL", "enteredTime": "2025-01-15T14:30:01+0000",
                     "orderLegCollection": [{"orderLegType": "EQUITY", "instruction": "SELL",
                        "quantity": 100, "positionEffect": "CLOSING",
                        "instrument": {"assetType": "EQUITY", "symbol": "AAPL"}}]},
                    {"orderId": 32, "status": "CANCELED", "enteredTime": "2025-01-15T14:30:01+0000"}
                ]
            })),
            order(json!({
                "orderId": 20, "status": "QUEUED", "orderType": "MARKET",
                "enteredTime": "2025-01-14T09:00:00+0000", "quantity": 5, "filledQuantity": 0,
                "remainingQuantity": 5,
                "orderLegCollection": [{"orderLegType": "OPTION", "instruction": "BUY_TO_OPEN",
                    "quantity": 5,
                    "instrument": {"assetType": "OPTION", "symbol": "AAPL  250117C00150000"}}]
            })),
        ];
        let page = order_page(orders, 1 << 20).unwrap();
        assert_eq!(page.columns, WORKING_ORDER_COLUMNS);
        assert_eq!((page.count, page.total, page.truncated), (2, 2, false));
        assert_eq!(page.rows[0][0], "20");
        assert_eq!(page.rows[0][2], "QUEUED");
        assert_eq!(page.rows[0][13][0]["symbol"], "AAPL  250117C00150000");
        assert_eq!(page.rows[1][0], "31");
        assert_eq!(page.rows[1][1], "30");
        assert_eq!(page.rows[1][5], "GOOD_TILL_CANCEL");
        assert_eq!(page.rows[1][13][0]["positionEffect"], "CLOSING");

        let tight = order_page(
            vec![order(json!({"orderId": 1, "status": "WORKING"})); 3],
            1,
        )
        .unwrap();
        assert_eq!((tight.count, tight.total, tight.truncated), (1, 3, true));
        assert!(
            tight.next_cursor.is_none(),
            "nothing accepts an order cursor"
        );
    }

    #[test]
    fn balances_fall_back_to_cash_available_and_pnl_sums_to_cents() {
        let cash: SecuritiesAccount = serde_json::from_value(json!({
            "accountNumber": "1", "type": "CASH",
            "currentBalances": {"liquidationValue": 1000.5, "cashBalance": 400.0,
                "cashAvailableForTrading": 350.25, "unsettledCash": 49.75, "cashCall": 0.0},
            "projectedBalances": {"cashAvailableForTrading": 350.25, "isInCall": false},
            "positions": [
                {"longQuantity": 1.0, "currentDayProfitLoss": 0.1, "longOpenProfitLoss": 0.2,
                 "instrument": {"assetType": "EQUITY", "symbol": "A"}},
                {"shortQuantity": 1.0, "currentDayProfitLoss": 0.2, "shortOpenProfitLoss": -0.5,
                 "instrument": {"assetType": "EQUITY", "symbol": "B"}}
            ]
        }))
        .unwrap();
        let balances = Balances::of(&cash);
        assert_eq!(balances.buying_power, Some(350.25));
        assert_eq!(balances.available_funds, Some(350.25));
        assert_eq!(balances.unsettled_cash, Some(49.75));
        assert_eq!(balances.in_call, Some(false));
        assert_eq!(balances.maintenance_call, None);
        let pnl = ProfitLoss::of(cash.positions.as_deref().unwrap_or_default());
        assert_eq!(
            pnl,
            ProfitLoss {
                day: Some(0.3),
                open: Some(-0.3)
            }
        );

        let margin: SecuritiesAccount = serde_json::from_value(json!({
            "accountNumber": "2", "type": "MARGIN",
            "currentBalances": {"buyingPower": 8000.0, "availableFunds": 4000.0,
                "maintenanceCall": 0.0, "regTCall": 0.0}
        }))
        .unwrap();
        let balances = Balances::of(&margin);
        assert_eq!(balances.buying_power, Some(8000.0));
        assert_eq!(balances.available_funds, Some(4000.0));
        assert_eq!(balances.in_call, None);
        assert_eq!(
            ProfitLoss::of(&[]),
            ProfitLoss {
                day: None,
                open: None
            }
        );
    }
}
