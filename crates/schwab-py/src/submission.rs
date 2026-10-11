//! What an order submission hands back to Python.
//!
//! The SDK's `OrderSubmitResponse` carries the order ID only. The journal
//! attempt that produced it is what a caller needs to reconcile an order
//! later, and it is what `SubmissionUnknown` names when an acknowledgement is
//! lost, so a success reports it too. The attempt is found by the order's
//! tag: the binding assigns one when the caller gave none, exactly as the
//! SDK would, so it can tell its own journal record from a concurrent one.

use schwab_rs::SchwabClient;
use schwab_rs::types::trading::{OrderRequest, OrderSubmitResponse};
use serde::Serialize;
use serde_json::Value;

/// The outcome of a place or replace that reached an accepting answer.
#[derive(Debug, Clone, Serialize)]
pub struct Submitted {
    /// Schwab's (or the paper venue's) ID for the new order. `None` when
    /// Schwab accepted the order but sent no `Location` header.
    pub order_id: Option<String>,
    /// The `Location` header value, when there was one.
    pub location: Option<String>,
    /// The execution-journal attempt for this submission, when it can be found.
    pub attempt_id: Option<String>,
    /// The order's tag, assigned here if the request had none.
    pub tag: String,
}

/// `order` with a tag: its own, or a fresh `rs-<uuid>` as the SDK assigns.
pub fn tagged(mut order: OrderRequest) -> (OrderRequest, String) {
    let tag = order
        .tag
        .get_or_insert_with(|| format!("rs-{}", uuid::Uuid::new_v4()))
        .clone();
    (order, tag)
}

/// Milliseconds since the epoch, early by a second so a clock step between
/// this call and the journal's own timestamp cannot hide the record.
pub fn journal_window_start() -> i64 {
    chrono::Utc::now().timestamp_millis() - 1_000
}

/// The journal attempt that sent `tag` as `operation` on `account`.
///
/// A failed lookup is `None`: the order was accepted, and that must not turn
/// into an error because a diagnostic read failed.
pub async fn live_attempt(
    sdk: &SchwabClient,
    account: &str,
    operation: &str,
    tag: &str,
    since_ms: i64,
) -> Option<String> {
    let records = sdk
        .order_journal()
        .records_since(account, since_ms)
        .await
        .ok()?;
    records
        .into_iter()
        .rev()
        .find(|record| {
            record.operation == operation
                && record.request.get("tag").and_then(Value::as_str) == Some(tag)
        })
        .map(|record| record.attempt_id)
}

impl Submitted {
    pub fn new(response: OrderSubmitResponse, tag: String, attempt_id: Option<String>) -> Self {
        Self {
            order_id: response.order_id,
            location: response.location,
            attempt_id,
            tag,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_existing_tag_is_kept_and_a_missing_one_assigned() {
        let (order, tag) = tagged(OrderRequest {
            tag: Some("mine".into()),
            ..OrderRequest::default()
        });
        assert_eq!(tag, "mine");
        assert_eq!(order.tag.as_deref(), Some("mine"));

        let (order, tag) = tagged(OrderRequest::default());
        assert!(tag.starts_with("rs-"));
        assert_eq!(order.tag, Some(tag));
    }
}
