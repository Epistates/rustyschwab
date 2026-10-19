//! What an operator approves, in words.
//!
//! An approval binds an intent digest. A digest alone tells an operator
//! nothing, and a model that shows the user one order while asking for
//! another's digest would get the wrong order approved. So every intent
//! that needs approval is recorded with its arguments when it is asked for
//! (by `place_order`, `replace_order` or `check_order`), and the operator
//! reviews, approves and audits the order itself: `rustyschwab-admin pending`,
//! `show` and `approve`, or the authority's operator API.
use rustyschwab::execution::{IntentRecord, IntentState};
use rustyschwab_types::trading::OrderRequest;
use serde_json::Value;
use std::fmt::Write;

/// One line describing what intent `tool` with `arguments` would do, for
/// an operator deciding whether to approve it. Never more than the order
/// the digest binds; the model's rationale is shown apart, as its words.
pub fn describe(tool: &str, arguments: &Value) -> String {
    let account = arguments
        .get("account_hash")
        .and_then(Value::as_str)
        .map(rustyschwab::utils::mask_account)
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

/// An intent as `rustyschwab-admin pending` and `show` print it.
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

/// The environment variable naming the approval webhook.
pub const WEBHOOK_ENV: &str = "SCHWAB_MCP_APPROVAL_WEBHOOK";

/// The longest rationale a notification quotes, in characters.
const NOTICE_RATIONALE_CHARS: usize = 500;

/// Tells operators an intent is waiting for their approval: one JSON POST
/// per intent, the first time it is asked for, to a webhook. The body's
/// `text` reads as a chat message (Slack and Mattermost incoming webhooks
/// take it as is); `intent` carries the same facts for a program. Account
/// numbers are masked, as in [`describe`], and nothing the operator could
/// approve with is sent: approving still takes `rustyschwab-admin` or the
/// authority's operator API.
///
/// Delivery is best effort, off the request path, bounded at five seconds
/// and never redirected; a failure is logged and the intent still waits.
#[derive(Clone)]
pub struct Notifier {
    url: reqwest::Url,
    client: reqwest::Client,
}

impl std::fmt::Debug for Notifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The URL of an incoming webhook is its credential.
        f.debug_struct("Notifier")
            .field("host", &self.url.host_str())
            .finish_non_exhaustive()
    }
}

impl Notifier {
    /// A notifier posting to `url`: HTTPS, or HTTP to a loopback address.
    pub fn new(url: &str) -> anyhow::Result<Self> {
        let url = reqwest::Url::parse(url)?;
        let loopback = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        anyhow::ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && loopback),
            "The approval webhook must be https (or http to a loopback address)"
        );
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { url, client })
    }

    /// The notifier [`WEBHOOK_ENV`] configures, if it is set.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        match std::env::var(WEBHOOK_ENV) {
            Ok(url) if !url.trim().is_empty() => Self::new(url.trim()).map(Some),
            _ => Ok(None),
        }
    }

    /// Post the notice for `principal`'s intent `digest` (`tool` with
    /// `arguments`, as it would be dispatched) on its own task.
    pub fn notify(&self, principal: &str, digest: &str, tool: &str, arguments: &Value) {
        let body = notice(principal, digest, tool, arguments);
        let request = self.client.post(self.url.clone()).json(&body);
        let digest = digest.to_owned();
        tokio::spawn(async move {
            match request.send().await.and_then(|r| r.error_for_status()) {
                Ok(_) => tracing::debug!(%digest, "approval notice delivered"),
                Err(e) => {
                    tracing::warn!(%digest, error = %e.without_url(), "approval notice failed")
                }
            }
        });
    }
}

/// The webhook body for an intent awaiting approval.
fn notice(principal: &str, digest: &str, tool: &str, arguments: &Value) -> Value {
    let order = describe(tool, arguments);
    let rationale = arguments
        .get("rationale")
        .and_then(Value::as_str)
        .map(|text| {
            text.chars()
                .take(NOTICE_RATIONALE_CHARS)
                .collect::<String>()
        });
    let mut text = format!(
        "Approval needed: {principal} asks to {order}\nReview: rustyschwab-admin show {digest}"
    );
    if let Some(rationale) = &rationale {
        let _ = write!(text, "\nThe model says: {rationale:?}");
    }
    serde_json::json!({
        "text": text,
        "intent": {
            "principal": principal,
            "digest": digest,
            "tool": tool,
            "order": order,
            "rationale": rationale,
        },
    })
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

    #[test]
    fn a_notice_describes_the_order_and_quotes_the_model_apart() {
        let args = json!({
            "account_hash": "HASHVALUE1234",
            "order": {"orderStrategyType": "SINGLE", "orderType": "LIMIT",
                "session": "NORMAL", "duration": "DAY", "price": "150.25",
                "orderLegCollection": [{"quantity": "10", "instruction": "BUY",
                    "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}}]},
            "rationale": "x".repeat(600),
        });
        let body = notice("desk", "ab12", "place_order", &args);
        let text = body["text"].as_str().unwrap();
        assert!(
            text.starts_with("Approval needed: desk asks to PLACE on account …1234: BUY 10 AAPL"),
            "{text}"
        );
        assert!(text.contains("rustyschwab-admin show ab12"));
        assert!(!text.contains("HASHVALUE"), "the account is masked");
        assert_eq!(body["intent"]["rationale"].as_str().unwrap().len(), 500);
    }

    #[test]
    fn the_webhook_is_https_or_loopback() {
        assert!(Notifier::new("https://hooks.example.com/T/B/x").is_ok());
        assert!(Notifier::new("http://127.0.0.1:9000/hook").is_ok());
        assert!(Notifier::new("http://[::1]:9000/hook").is_ok());
        assert!(Notifier::new("http://localhost/hook").is_ok());
        assert!(Notifier::new("http://hooks.example.com/x").is_err());
        assert!(Notifier::new("ftp://127.0.0.1/x").is_err());
        assert!(
            format!("{:?}", Notifier::new("https://h.example/secret").unwrap())
                .contains("h.example")
        );
    }
}
