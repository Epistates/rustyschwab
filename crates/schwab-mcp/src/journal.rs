//! `get_trade_journal`: what this principal decided, why, and what the
//! venue did about it, newest first. Each row is one write dispatch from
//! the principal's audit journal (the arguments as dispatched, the
//! model's `rationale` among them, and how the dispatch ended) joined to
//! the venue's current state of the order it produced.
//!
//! The rationale is the model's own words for the audit trail. It is
//! never sent to the venue and is not part of the intent digest: an
//! approval binds the order, not the words, and re-wording an identical
//! order must still be a duplicate intent.
//!
//! One venue read, of the account's orders entered since the window
//! start (or since the venue's list horizon, when the window reaches
//! further back), joins the rows; it degrades to a warning, in which
//! case the rows carry the audit's side alone. An order is joined by the id the
//! dispatch recorded, or by its tag when the dispatch recorded none (a
//! lost acknowledgement, or a process that died mid-dispatch), which is
//! how a working order the model was never told about surfaces here.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::{self, Display};
use std::str::FromStr;

use chrono::{DateTime, Duration, Utc};
use schwab_rs::execution::{OrderJournal, SubmissionOutcome, SubmissionRecord};
use schwab_types::trading::{Order, OrderRequest, OrdersParams};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use turbomcp::prelude::*;

use crate::backend::Backend;
use crate::contract::{FailureCode, Reported, ToolFailure};
use crate::order_status::OrderStatusReport;
use crate::portfolio::ORDER_LOOKBACK_DAYS;
use crate::projection::{self, Page, Row};
use crate::query::{Bound, DEFAULT_WINDOW_DAYS, FlexibleTime};

/// The longest `rationale` a write tool accepts, in characters.
pub const MAX_RATIONALE_CHARS: usize = 2000;

/// Validate a write tool's `rationale`: absent, or non-blank text of at
/// most [`MAX_RATIONALE_CHARS`] characters. Judged by the policy review
/// before any approval is consumed, and by the tool itself.
pub fn check_rationale(rationale: Option<&str>) -> Result<(), ToolFailure> {
    let Some(text) = rationale else {
        return Ok(());
    };
    if text.trim().is_empty() {
        return Err(ToolFailure::invalid_arguments(
            "rationale must say why, or be omitted",
        ));
    }
    let chars = text.chars().count();
    if chars > MAX_RATIONALE_CHARS {
        return Err(ToolFailure::invalid_arguments(format!(
            "rationale is {chars} characters; at most {MAX_RATIONALE_CHARS} are journaled"
        )));
    }
    Ok(())
}

const COMPLETED: &str = "MCP dispatch completed";
const FAILED: &str = "MCP dispatch returned error";
const RECONCILE: &str = "consult SDK submission journal";

/// How a write dispatch ended, as the audit outcome's evidence records
/// it and the journal reads it back. The SDK journal owns broker
/// reconciliation; this attributes the dispatch without claiming a tool
/// success is a fill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatch {
    /// The tool answered a success; `order_id` is the venue's id for the
    /// order placed, replaced or canceled, when the answer carried one.
    Completed { order_id: Option<String> },
    /// The tool answered a contract failure with `code`, or a protocol
    /// error (no code).
    Failed { code: Option<FailureCode> },
}

impl Dispatch {
    pub fn of(result: &McpResult<ToolResult>) -> Self {
        match result {
            Ok(result) if !result.is_error() => Self::Completed {
                order_id: Reported::of(result).order_id().map(str::to_owned),
            },
            Ok(result) => Self::Failed {
                code: Reported::of(result).failure.map(|(code, _)| code),
            },
            Err(_) => Self::Failed { code: None },
        }
    }

    /// The audit outcome's evidence.
    pub fn evidence(&self) -> String {
        match self {
            Self::Completed { order_id: Some(id) } => format!("{COMPLETED}; venue order {id}"),
            Self::Completed { order_id: None } => format!("{COMPLETED}; {RECONCILE}"),
            Self::Failed { code: Some(code) } => format!("{FAILED} ({code}); {RECONCILE}"),
            Self::Failed { code: None } => format!("{FAILED}; {RECONCILE}"),
        }
    }

    /// The dispatch an evidence string records; `None` for evidence this
    /// server did not write.
    pub fn parse(evidence: &str) -> Option<Self> {
        if let Some(rest) = evidence.strip_prefix(COMPLETED) {
            let order_id = rest
                .strip_prefix("; venue order ")
                .map(|id| id.split(';').next().unwrap_or(id).trim())
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
            Some(Self::Completed { order_id })
        } else if let Some(rest) = evidence.strip_prefix(FAILED) {
            let code = rest
                .strip_prefix(" (")
                .and_then(|rest| rest.split(')').next())
                .and_then(|name| name.parse().ok());
            Some(Self::Failed { code })
        } else {
            None
        }
    }
}

/// How long a principal's audit record may stay unresolved before it is
/// judged stranded. A dispatch runs on its own task, so only a process that
/// died mid-dispatch leaves one; its venue journal has an intent within
/// seconds of the audit record, so this is ample.
pub const STRANDED_AFTER: Duration = Duration::minutes(2);

const STRANDED: &str = "Dispatch interrupted; settled from the venue journal";

/// Settle `attempt_id`, a principal audit record its dispatch never
/// finished (the process died mid-dispatch), from what the venue's own
/// journal knows of the order tag it carried:
///
/// - the venue resolved its intent: the record is settled with that outcome;
/// - the venue's intent is still unresolved: nothing is settled, since the
///   order may exist (venue reconciliation resolves it, and a later call
///   settles this record);
/// - the tag is spent with no intent (the venue holds an order under it):
///   settled as such;
/// - the tag was never spent: nothing reached the venue. Settled, and a
///   one-use approval it consumed is given back.
///
/// Returns whether the record was settled; a record that is not stranded,
/// is younger than [`STRANDED_AFTER`], or carries no order tag is left
/// alone.
pub async fn settle_stranded(
    audit: &OrderJournal,
    venue: &dyn schwab_rs::execution::ExecutionStore,
    attempt_id: &str,
) -> schwab_rs::Result<bool> {
    let Some(record) = audit.record(attempt_id).await? else {
        return Ok(false);
    };
    let age = Utc::now().timestamp_millis() - record.created_at;
    if record.outcome.is_some() || age < STRANDED_AFTER.num_milliseconds() {
        return Ok(false);
    }
    let arguments = &record.request["arguments"];
    let (Some(account), Some(tag)) = (
        arguments.get("account_hash").and_then(Value::as_str),
        arguments.pointer("/order/tag").and_then(Value::as_str),
    ) else {
        return Ok(false);
    };
    let (evidence, refund) = match venue.submission_by_tag(account, tag).await? {
        Some(intent) => match intent.outcome {
            None => return Ok(false),
            Some(outcome) => (
                format!(
                    "{STRANDED}: venue attempt {} {}",
                    intent.attempt_id,
                    serde_json::to_string(&outcome)?
                ),
                false,
            ),
        },
        None if venue.tag_spent(account, tag).await? => (
            format!("{STRANDED}: the venue holds an order tagged {tag}"),
            false,
        ),
        None => (format!("{STRANDED}: nothing reached the venue"), true),
    };
    audit
        .finish(attempt_id, SubmissionOutcome::Reconciled { evidence })
        .await?;
    // Only an approval digest is given back; a gateway idempotency key
    // stays spent.
    let approval = record
        .dedup_key
        .as_deref()
        .is_some_and(|k| k.len() == 64 && k.bytes().all(|b| b.is_ascii_hexdigit()));
    if refund
        && approval
        && let Err(e) = audit.refund_approval(attempt_id).await
    {
        tracing::warn!(attempt = %attempt_id, error = %e, "approval refund failed; it stays spent");
    }
    tracing::warn!(attempt = %attempt_id, "settled an interrupted dispatch from the venue journal");
    Ok(true)
}

/// `get_trade_journal` arguments, the shape of the tool's signature, so
/// the authorized transport extracts them from raw arguments exactly as
/// the `#[tool]` macro does.
#[derive(Debug, Deserialize)]
pub struct Request {
    pub account_hash: String,
    #[serde(default)]
    pub since: Option<FlexibleTime>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub cursor: Option<String>,
}

impl Request {
    pub fn parse(args: Value) -> Result<Self, ToolFailure> {
        serde_json::from_value(args).map_err(|e| {
            ToolFailure::invalid_arguments(format!("Invalid get_trade_journal arguments: {e}"))
        })
    }
}

pub const COLUMNS: &[&str] = &[
    "at",
    "tool",
    "dispatch",
    "failure",
    "orderId",
    "replaces",
    "status",
    "filled",
    "avgFillPrice",
    "orderType",
    "price",
    "stopPrice",
    "duration",
    "legs",
    "tag",
    "rationale",
    "intentDigest",
    "attemptId",
];

/// Orders rows newest first: the later dispatch sorts lower, so the page
/// starts at the most recent and the cursor continues into the past.
#[derive(Debug, Clone, PartialEq, Eq)]
struct JournalKey {
    at: i64,
    id: String,
}

impl Ord for JournalKey {
    fn cmp(&self, other: &Self) -> Ordering {
        other.at.cmp(&self.at).then_with(|| other.id.cmp(&self.id))
    }
}

impl PartialOrd for JournalKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Display for JournalKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}|{}", self.at, self.id)
    }
}

impl FromStr for JournalKey {
    type Err = ();
    fn from_str(text: &str) -> Result<Self, ()> {
        let (at, id) = text.split_once('|').ok_or(())?;
        Ok(Self {
            at: at.parse().map_err(|_| ())?,
            id: id.to_owned(),
        })
    }
}

/// The venue's orders by id and by tag, for the join.
#[derive(Default)]
struct VenueOrders {
    by_id: HashMap<String, Order>,
    by_tag: HashMap<String, String>,
}

impl VenueOrders {
    fn index(orders: Vec<Order>) -> Self {
        let mut venue = Self::default();
        for order in orders {
            let Some(id) = order.order_id.clone() else {
                continue;
            };
            if let Some(tag) = order.tag.clone() {
                venue.by_tag.insert(tag, id.clone());
            }
            venue.by_id.insert(id, order);
        }
        venue
    }

    fn get(&self, id: &str) -> Option<&Order> {
        self.by_id.get(id)
    }

    fn by_tag(&self, tag: &str) -> Option<&Order> {
        self.by_tag.get(tag).and_then(|id| self.by_id.get(id))
    }
}

/// One journal row from an audit record and the venue's view of it.
fn row(record: SubmissionRecord, venue: &VenueOrders) -> Row<JournalKey> {
    let args = &record.request["arguments"];
    let (dispatch, failure, reported_id) = match &record.outcome {
        None => ("pending", None, None),
        Some(SubmissionOutcome::Reconciled { evidence }) => match Dispatch::parse(evidence) {
            Some(Dispatch::Completed { order_id }) => ("completed", None, order_id),
            Some(Dispatch::Failed { code }) => ("error", code, None),
            None => ("completed", None, None),
        },
        // Only the SDK journal writes these; the principal audit never does.
        Some(SubmissionOutcome::Accepted { order_id }) => ("completed", None, order_id.clone()),
        Some(SubmissionOutcome::Rejected) => ("error", None, None),
    };
    let argument = |name: &str| args.get(name).and_then(Value::as_str).map(str::to_owned);
    let request: Option<OrderRequest> = args
        .get("order")
        .cloned()
        .and_then(|order| serde_json::from_value(order).ok());
    let tag = request.as_ref().and_then(|order| order.tag.clone());
    let (order_id, replaces) = match record.operation.as_str() {
        "cancel_order" => (argument("order_id"), None),
        "replace_order" => (reported_id, argument("order_id")),
        _ => (reported_id, None),
    };
    let order = order_id
        .as_deref()
        .and_then(|id| venue.get(id))
        .or_else(|| tag.as_deref().and_then(|tag| venue.by_tag(tag)));
    let order_id = order_id.or_else(|| order.and_then(|o| o.order_id.clone()));
    let status = order.map(|order| OrderStatusReport::of(order.clone()));
    // What the model asked for; for a cancellation, what the venue holds.
    let (order_type, price, stop_price, duration, legs) = match (&request, order) {
        (Some(request), _) => (
            json!(request.order_type),
            json!(request.price),
            json!(request.stop_price),
            json!(request.duration),
            request
                .order_leg_collection
                .iter()
                .map(|leg| {
                    json!({
                        "instruction": leg.instruction,
                        "quantity": leg.quantity,
                        "symbol": leg.instrument.symbol,
                        "assetType": leg.instrument.asset_type,
                    })
                })
                .collect(),
        ),
        (None, Some(order)) => (
            json!(order.order_type),
            json!(order.price),
            json!(order.stop_price),
            json!(order.duration),
            order
                .order_leg_collection
                .iter()
                .map(|leg| Value::Object(crate::order_status::leg(leg)))
                .collect(),
        ),
        (None, None) => (Value::Null, Value::Null, Value::Null, Value::Null, vec![]),
    };
    let tag = tag.or_else(|| status.as_ref().and_then(|s| s.tag.clone()));
    Row {
        key: JournalKey {
            at: record.created_at,
            id: record.attempt_id.clone(),
        },
        value: json!([
            DateTime::from_timestamp_millis(record.created_at)
                .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            record.operation,
            dispatch,
            failure,
            order_id,
            replaces,
            status.as_ref().and_then(|s| s.status),
            status.as_ref().and_then(|s| s.filled),
            status.as_ref().and_then(|s| s.avg_fill_price),
            order_type,
            price,
            stop_price,
            duration,
            legs,
            tag,
            argument("rationale"),
            record.request["intentDigest"].as_str(),
            record.attempt_id,
        ]),
    }
}

/// The principal's journal for one account, newest first, within the
/// response budget.
pub async fn collect(
    audit: &OrderJournal,
    backend: &Backend,
    principal: &str,
    request: Request,
) -> Result<Page, ToolFailure> {
    let now = Utc::now();
    let since = request
        .since
        .as_ref()
        .map_or(Ok(now - Duration::days(DEFAULT_WINDOW_DAYS)), |t| {
            t.resolve(Bound::Start)
        })
        .map_err(ToolFailure::invalid_arguments)?;
    if since > now {
        return Err(ToolFailure::invalid_arguments(
            "since must not be in the future",
        ));
    }
    let mut warnings: Vec<ToolFailure> = Vec::new();
    let records: Vec<SubmissionRecord> = audit
        .records()
        .await
        .map_err(|e| ToolFailure::internal("Audit journal unavailable", e))?
        .into_iter()
        .filter(|record| {
            record.created_at >= since.timestamp_millis()
                && record.request["arguments"]["account_hash"] == request.account_hash
        })
        .collect();
    // The audit window may reach further back than the venue lists.
    let venue_since = since.max(now - Duration::days(ORDER_LOOKBACK_DAYS));
    let venue = if records.is_empty() {
        VenueOrders::default()
    } else {
        match backend
            .account_orders(&request.account_hash, &OrdersParams::new(venue_since, now))
            .await
        {
            Ok(orders) => VenueOrders::index(orders),
            Err(e) => {
                warnings.push(e);
                VenueOrders::default()
            }
        }
    };
    let rows = records
        .into_iter()
        .map(|record| row(record, &venue))
        .collect();
    let mut header = Map::new();
    header.insert("principal".into(), json!(principal));
    header.insert(
        "since".into(),
        json!(since.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
    );
    header.insert("warnings".into(), json!(warnings));
    projection::page(
        rows,
        COLUMNS,
        header,
        request.cursor.as_deref(),
        request.limit,
        projection::budget(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rationale_must_be_present_text_within_the_limit() {
        assert!(check_rationale(None).is_ok());
        assert!(check_rationale(Some("Momentum entry on the breakout.")).is_ok());
        let blank = check_rationale(Some("  \n")).unwrap_err();
        assert_eq!(blank.code, FailureCode::InvalidArguments);
        let long = "é".repeat(MAX_RATIONALE_CHARS + 1);
        let over = check_rationale(Some(&long)).unwrap_err();
        assert_eq!(over.code, FailureCode::InvalidArguments);
        assert!(over.message.contains("2001 characters"), "{}", over.message);
        assert!(check_rationale(Some(&"é".repeat(MAX_RATIONALE_CHARS))).is_ok());
    }

    #[test]
    fn dispatch_evidence_round_trips_and_keeps_the_legacy_wording() {
        for dispatch in [
            Dispatch::Completed {
                order_id: Some("1001".into()),
            },
            Dispatch::Completed { order_id: None },
            Dispatch::Failed {
                code: Some(FailureCode::RiskDenied),
            },
            Dispatch::Failed { code: None },
        ] {
            let evidence = dispatch.evidence();
            assert_eq!(Dispatch::parse(&evidence), Some(dispatch), "{evidence}");
        }
        assert_eq!(
            Dispatch::Failed {
                code: Some(FailureCode::RiskDenied)
            }
            .evidence(),
            "MCP dispatch returned error (risk_denied); consult SDK submission journal"
        );
        // Evidence written before dispatches recorded ids or codes.
        assert_eq!(
            Dispatch::parse("MCP dispatch completed; consult SDK submission journal"),
            Some(Dispatch::Completed { order_id: None })
        );
        assert_eq!(
            Dispatch::parse("MCP dispatch returned error; consult SDK submission journal"),
            Some(Dispatch::Failed { code: None })
        );
        assert_eq!(Dispatch::parse("reconciled by operator"), None);
    }

    #[test]
    fn dispatch_reads_the_result_through_the_contract() {
        let placed =
            ToolResult::text(json!({"source": "paper", "data": {"orderId": "7"}}).to_string());
        assert_eq!(
            Dispatch::of(&Ok(placed)),
            Dispatch::Completed {
                order_id: Some("7".into())
            }
        );
        let canceled =
            ToolResult::text(json!({"source": "paper", "data": {"order_id": "7"}}).to_string());
        assert_eq!(
            Dispatch::of(&Ok(canceled)),
            Dispatch::Completed {
                order_id: Some("7".into())
            }
        );
        let denied = ToolFailure::new(FailureCode::Halted, "halted").into_result();
        assert_eq!(
            Dispatch::of(&Ok(denied)),
            Dispatch::Failed {
                code: Some(FailureCode::Halted)
            }
        );
        assert_eq!(
            Dispatch::of(&Err(McpError::internal("boom"))),
            Dispatch::Failed { code: None }
        );
    }

    #[test]
    fn keys_sort_newest_first_and_survive_the_cursor() {
        let older = JournalKey {
            at: 1,
            id: "b".into(),
        };
        let newer = JournalKey {
            at: 2,
            id: "a".into(),
        };
        assert!(newer < older, "the newer dispatch pages first");
        let text = newer.to_string();
        assert_eq!(text, "2|a");
        assert_eq!(text.parse::<JournalKey>().unwrap(), newer);
        assert!("2".parse::<JournalKey>().is_err());
        assert!("x|a".parse::<JournalKey>().is_err());
    }

    fn record(
        operation: &str,
        at: i64,
        args: Value,
        outcome: Option<SubmissionOutcome>,
    ) -> SubmissionRecord {
        SubmissionRecord {
            attempt_id: format!("attempt-{at}"),
            account: "desk".into(),
            operation: operation.into(),
            request: json!({
                "principal": "desk",
                "intentDigest": "d".repeat(64),
                "requestId": "r",
                "arguments": args,
            }),
            created_at: at,
            outcome,
            dedup_key: None,
            armed: true,
        }
    }

    fn venue_order(id: &str, tag: &str, status: &str) -> Order {
        serde_json::from_value(json!({
            "orderId": id, "status": status, "tag": tag, "orderType": "LIMIT", "price": 9.5,
            "duration": "DAY", "quantity": 2.0, "filledQuantity": 1.0, "remainingQuantity": 1.0,
            "orderLegCollection": [{"orderLegType": "EQUITY", "instruction": "BUY",
                "quantity": 2.0, "instrument": {"assetType": "EQUITY", "symbol": "ABC"}}]
        }))
        .unwrap()
    }

    /// A completed placement joins by id, a dispatch with no recorded id
    /// joins by tag, a cancellation names the order it canceled, and a
    /// failed dispatch keeps its code with no venue side.
    #[test]
    fn rows_join_the_audit_to_the_venue() {
        let order = |tag: &str| {
            json!({
                "orderStrategyType": "SINGLE", "orderType": "LIMIT", "session": "NORMAL",
                "duration": "DAY", "price": "9.50", "tag": tag,
                "orderLegCollection": [{"quantity": "2", "instruction": "BUY",
                    "instrument": {"symbol": "ABC", "assetType": "EQUITY"}}],
            })
        };
        let venue = VenueOrders::index(vec![
            venue_order("1", "mcp-first", "WORKING"),
            venue_order("2", "mcp-lost", "FILLED"),
        ]);
        let placed = row(
            record(
                "place_order",
                10,
                json!({"account_hash": "acct", "order": order("mcp-first"),
                       "rationale": "Breakout above 9.40 on volume."}),
                Some(SubmissionOutcome::Reconciled {
                    evidence: Dispatch::Completed {
                        order_id: Some("1".into()),
                    }
                    .evidence(),
                }),
            ),
            &venue,
        );
        assert_eq!(placed.key.at, 10);
        let v = &placed.value;
        assert_eq!(v[0], "1970-01-01T00:00:00.010Z");
        assert_eq!(v[1], "place_order");
        assert_eq!(v[2], "completed");
        assert_eq!(v[3], Value::Null);
        assert_eq!(v[4], "1");
        assert_eq!(v[6], "WORKING");
        assert_eq!(v[7], 1.0);
        assert_eq!(v[9], "LIMIT");
        assert_eq!(v[10], "9.50");
        assert_eq!(v[13][0]["symbol"], "ABC");
        assert_eq!(v[14], "mcp-first");
        assert_eq!(v[15], "Breakout above 9.40 on volume.");
        assert_eq!(v[16], "d".repeat(64));
        assert_eq!(v[17], "attempt-10");

        let lost = row(
            record(
                "place_order",
                20,
                json!({"account_hash": "acct", "order": order("mcp-lost")}),
                None,
            ),
            &venue,
        );
        assert_eq!(lost.value[2], "pending");
        assert_eq!(lost.value[4], "2", "the venue's order surfaces by tag");
        assert_eq!(lost.value[6], "FILLED");
        assert_eq!(lost.value[15], Value::Null);

        let canceled = row(
            record(
                "cancel_order",
                30,
                json!({"account_hash": "acct", "order_id": "1", "rationale": "Thesis broke."}),
                Some(SubmissionOutcome::Reconciled {
                    evidence: Dispatch::Completed {
                        order_id: Some("1".into()),
                    }
                    .evidence(),
                }),
            ),
            &venue,
        );
        assert_eq!(canceled.value[1], "cancel_order");
        assert_eq!(canceled.value[4], "1");
        assert_eq!(canceled.value[5], Value::Null);
        assert_eq!(canceled.value[9], "LIMIT", "what the venue holds");
        assert_eq!(canceled.value[13][0]["instruction"], "BUY");
        assert_eq!(canceled.value[14], "mcp-first");
        assert_eq!(canceled.value[15], "Thesis broke.");

        let replaced = row(
            record(
                "replace_order",
                40,
                json!({"account_hash": "acct", "order_id": "1", "order": order("mcp-next")}),
                Some(SubmissionOutcome::Reconciled {
                    evidence: Dispatch::Failed {
                        code: Some(FailureCode::RiskDenied),
                    }
                    .evidence(),
                }),
            ),
            &venue,
        );
        assert_eq!(replaced.value[2], "error");
        assert_eq!(replaced.value[3], "risk_denied");
        assert_eq!(replaced.value[4], Value::Null);
        assert_eq!(replaced.value[5], "1");
        assert_eq!(replaced.value[6], Value::Null);
    }
}
