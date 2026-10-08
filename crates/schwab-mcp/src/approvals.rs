//! What an operator approves, in words.
//!
//! An approval binds an intent digest. A digest alone tells an operator
//! nothing, and a model that shows the user one order while asking for
//! another's digest would get the wrong order approved. So every intent
//! that needs approval is recorded with its arguments when it is asked for
//! (by `place_order`, `replace_order` or `check_order`), and the operator
//! reviews, approves and audits the order itself: `schwab-admin pending`,
//! `show` and `approve`, or the authority's operator API.
use schwab_rs::execution::{IntentRecord, IntentState};
use schwab_types::trading::OrderRequest;
use serde_json::Value;
use std::fmt::Write;

/// One line describing what intent `tool` with `arguments` would do, for
/// an operator deciding whether to approve it. Never more than the order
/// the digest binds; the model's rationale is shown apart, as its words.
pub fn describe(tool: &str, arguments: &Value) -> String {
    let account = arguments
        .get("account_hash")
        .and_then(Value::as_str)
        .map(schwab_rs::utils::mask_account)
        .unwrap_or_else(|| "?".into());
    let mut line = match tool {
        "replace_order" => format!(
            "REPLACE order {} on account {account}:",
            arguments
                .get("order_id")
                .and_then(Value::as_str)
                .unwrap_or("?")
        ),
        _ => format!("PLACE on account {account}:"),
    };
    match serde_json::from_value::<OrderRequest>(
        arguments.get("order").cloned().unwrap_or(Value::Null),
    ) {
        Ok(order) => describe_order(&mut line, &order, 0),
        Err(e) => {
            let _ = write!(line, " (unreadable order: {e})");
        }
    }
    line
}

fn describe_order(out: &mut String, order: &OrderRequest, depth: usize) {
    let wire = |v: &dyn erased::Wire| v.wire();
    if !order.order_leg_collection.is_empty() {
        for (i, leg) in order.order_leg_collection.iter().enumerate() {
            let _ = write!(
                out,
                "{} {} {} {}",
                if i == 0 { "" } else { " +" },
                wire(&leg.instruction),
                leg.quantity,
                leg.instrument.symbol
            );
        }
        if let Some(kind) = &order.order_type {
            let _ = write!(out, " {}", wire(kind));
        }
        if let Some(price) = order.price {
            let _ = write!(out, " @ {price}");
        }
        if let Some(stop) = order.stop_price {
            let _ = write!(out, " stop {stop}");
        }
        if let Some(duration) = &order.duration {
            let _ = write!(out, " {}", wire(duration));
        }
        if let Some(session) = &order.session {
            let _ = write!(out, " {}", wire(session));
        }
    } else {
        let _ = write!(out, " {}", wire(&order.order_strategy_type));
    }
    for (i, child) in order.child_order_strategies.iter().enumerate() {
        let _ = write!(
            out,
            "{}{} [",
            if i == 0 { ";" } else { "," },
            if depth == 0 { " then" } else { "" }
        );
        describe_order(out, child, depth + 1);
        out.push_str(" ]");
    }
    if depth == 0
        && let Some(tag) = &order.tag
    {
        let _ = write!(out, " (tag {tag})");
    }
}

/// Serialize an enum to its wire spelling for display.
mod erased {
    pub trait Wire {
        fn wire(&self) -> String;
    }
    impl<T: serde::Serialize> Wire for T {
        fn wire(&self) -> String {
            serde_json::to_value(self)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_else(|| "?".into())
        }
    }
}

/// An intent as `schwab-admin pending` and `show` print it.
pub fn render(intent: &IntentRecord) -> String {
    let state = match &intent.state {
        IntentState::Awaiting => "awaiting approval".to_owned(),
        IntentState::Approved {
            operator,
            expires_at,
        } => format!("approved by {operator} until {expires_at}"),
        IntentState::Spent => "spent on an order".to_owned(),
        IntentState::Revoked => "revoked".to_owned(),
        IntentState::Lapsed => "approval lapsed; may be approved again".to_owned(),
        _ => "unknown".to_owned(),
    };
    let mut out = format!(
        "{}  {}\n  principal {}, asked {} time(s), last at {} ({state})\n  {}",
        intent.digest,
        intent.tool,
        intent.principal,
        intent.seen,
        intent.last_seen,
        describe(&intent.tool, &intent.arguments)
    );
    if let Some(rationale) = intent.arguments.get("rationale").and_then(Value::as_str) {
        let _ = write!(out, "\n  the model says: {rationale:?}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_intent_reads_as_the_order_it_binds() {
        let args = json!({
            "account_hash": "HASHVALUE1234",
            "order": {"orderStrategyType": "SINGLE", "orderType": "LIMIT",
                "session": "NORMAL", "duration": "DAY", "price": "150.25",
                "orderLegCollection": [{"quantity": "10", "instruction": "BUY",
                    "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}}],
                "tag": "mcp-abc"},
            "rationale": "Breakout"
        });
        assert_eq!(
            describe("place_order", &args),
            "PLACE on account …1234: BUY 10 AAPL LIMIT @ 150.25 DAY NORMAL (tag mcp-abc)"
        );
        let replace = json!({"account_hash": "A", "order_id": "77", "order": args["order"]});
        assert!(describe("replace_order", &replace).starts_with("REPLACE order 77"));
    }
}
