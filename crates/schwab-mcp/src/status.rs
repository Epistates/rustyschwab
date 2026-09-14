//! `get_server_status`: what this server is, what it will let a model do
//! right now, and whether what it depends on is ready. Everything is local
//! evidence except the equity session calendar, which is Schwab's and is
//! cached until it can no longer answer.
use crate::backend::Backend;
use crate::contract::{FailureCode, ToolFailure};
use crate::policy::PrincipalPolicy;
use crate::server::Switches;
use chrono::{DateTime, Days, FixedOffset, NaiveDate, TimeDelta, Utc};
use parking_lot::Mutex;
use rust_decimal::Decimal;
use schwab_rs::paper::PaperClockMode;
use schwab_rs::{CircuitState, RequestBudgetUsage, SchwabClient, readiness::Readiness};
use schwab_types::market_data::{self, Market, MarketHours, SessionPhase, SessionTransition};
use schwab_types::streaming::StreamStatus;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Readiness treats an account baseline older than this as stale, as the
/// `/readyz` endpoint does.
pub const BASELINE_MAX_AGE_MS: i64 = 5_000;

/// Bound on the readiness probe and on each calendar request, so a status
/// call answers promptly even when a dependency does not.
const PROBE_DEADLINE: Duration = Duration::from_secs(3);

/// Days after today searched for the next opening; enough for a long
/// weekend.
const LOOKAHEAD_DAYS: u64 = 4;

/// Re-read a calendar that names nothing ahead at this cadence.
const RECHECK: TimeDelta = TimeDelta::minutes(15);

/// Trust no calendar longer than this, whatever it says.
const MAX_AGE: TimeDelta = TimeDelta::hours(6);

/// The status as `get_server_status` reports it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    /// `live`: every order tool reaches Schwab. `paper`: orders, fills and
    /// accounts are the paper venue's simulation.
    pub mode: &'static str,
    pub version: &'static str,
    pub server_time: ServerTime,
    /// The principal this session runs as; null on the local stdio
    /// transport, where no policy applies.
    pub principal: Option<Principal>,
    pub trading: Trading,
    /// Null when the execution store could not be probed (see `warnings`).
    pub readiness: Option<Readiness>,
    /// Null when rate limiting is disabled or there is no Schwab client.
    pub request_budget: Option<RequestBudgetUsage>,
    pub stream: Stream,
    /// Null when Schwab's calendar could not be read (see `warnings`).
    pub equity_session: Option<EquitySession>,
    /// The paper venue; null in live mode.
    pub paper: Option<PaperVenue>,
    /// Sections that could not be determined, with why.
    pub warnings: Vec<ToolFailure>,
}

impl ServerStatus {
    /// Name the principal this session runs as and answer only what it may
    /// see. The halt stays visible either way — it is why submissions are
    /// refused — but a principal scoped to particular accounts is not told
    /// its reason, which is operator text and, when a control wrote it,
    /// carries another account's equity, loss and cap. The full reason
    /// stays in the audit journal and on the operator's own surfaces.
    pub fn scope_to(&mut self, principal: &crate::policy::PrincipalPolicy) {
        if !principal.all_accounts
            && let Some(reason) = self.trading.operator_halt.as_mut()
        {
            *reason = HALT_WITHHELD.to_owned();
        }
        self.principal = Some(Principal::from(principal));
    }

    /// The status as a tool's local payload.
    pub fn into_payload(self) -> Result<Value, ToolFailure> {
        serde_json::to_value(self)
            .map_err(|e| ToolFailure::internal("Server status serialization", e))
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerTime {
    pub rfc3339: String,
    pub epoch_ms: i64,
}

impl From<DateTime<Utc>> for ServerTime {
    fn from(at: DateTime<Utc>) -> Self {
        Self {
            rfc3339: at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            epoch_ms: at.timestamp_millis(),
        }
    }
}

/// What the principal's policy grants, so a model can plan within it
/// instead of discovering denials one call at a time.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Principal {
    pub name: String,
    pub tools: Vec<String>,
    pub accounts: Vec<String>,
    pub all_accounts: bool,
    pub max_quantity: Option<Decimal>,
    pub max_notional: Option<Decimal>,
    pub require_approval: bool,
}

impl From<&PrincipalPolicy> for Principal {
    fn from(policy: &PrincipalPolicy) -> Self {
        let sorted = |set: &std::collections::HashSet<String>| {
            let mut items: Vec<String> = set.iter().cloned().collect();
            items.sort();
            items
        };
        Self {
            name: policy.name.clone(),
            tools: sorted(&policy.tools),
            accounts: sorted(&policy.accounts),
            all_accounts: policy.all_accounts,
            max_quantity: policy.max_quantity,
            max_notional: policy.max_notional,
            require_approval: policy.require_approval,
        }
    }
}

/// What a principal is told about an active halt. The reason is operator
/// text, and when an automatic control wrote it, it names an account and
/// its equity, loss and cap: a session scoped to particular accounts has
/// no claim on either.
pub const HALT_WITHHELD: &str = "An operator halt is active. Its reason is recorded for the operator, \
     who is the only one who can clear it.";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Trading {
    /// The server's trading switch (`SCHWAB_MCP_ALLOW_TRADING`) is on.
    pub enabled: bool,
    pub cancel_enabled: bool,
    /// The reason for a persisted operator halt, else null. Withheld as
    /// [`HALT_WITHHELD`] from a principal that is not granted every
    /// account; see [`ServerStatus::scope_to`].
    pub operator_halt: Option<String>,
    /// Submissions are suspended while a required stream recovers or the
    /// process drains; cancellations and reads continue.
    pub submissions_suspended: bool,
    pub circuit: CircuitState,
    /// A submission would pass the trading switch and local admission
    /// right now. Validity, risk limits, approval and duplicates are judged
    /// per order by `check_order` and again on submission.
    pub admissible: bool,
}

/// The stream section, also the whole `stream_status` answer.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stream {
    pub connected: bool,
    pub generation: u64,
    pub lost_messages: u64,
    /// Cached snapshots may have missed updates: the stream reconnected or
    /// dropped messages since it was first connected.
    pub reconciliation_required: bool,
}

impl From<StreamStatus> for Stream {
    fn from(status: StreamStatus) -> Self {
        Self {
            connected: status.ready,
            generation: status.generation,
            lost_messages: status.lost_messages,
            reconciliation_required: status.lost_messages > 0 || status.generation > 1,
        }
    }
}

/// The paper venue as it stands: its clock, the session its fills are
/// judged in, and the accounts it simulates.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaperVenue {
    /// `replay`: time advances only with events. `wall`: real time, and
    /// quotes age against it.
    pub clock: PaperClockMode,
    /// The venue's session, set by its feed or replay; orders only fill
    /// while it is open. Not Schwab's calendar.
    pub session: PaperSession,
    pub accounts: Vec<String>,
    pub model_version: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaperSession {
    pub id: String,
    pub open: bool,
}

/// Where the current instant falls in Schwab's equity calendar.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EquitySession {
    pub phase: SessionPhase,
    /// Schwab's calendar date (US Eastern) the phase is read from.
    pub date: Option<String>,
    /// The calendar lists sessions for that date.
    pub trading_day: bool,
    /// The next session boundary, looked up to a few days ahead once
    /// today's are past; null when none is known.
    pub next_transition: Option<SessionTransition>,
    /// When the calendar was read from Schwab.
    pub fetched_at: DateTime<Utc>,
}

/// Collect every section. Probes that fail become `warnings` and leave
/// their section null, so the answer is always complete about the rest.
pub async fn collect(
    backend: &Backend,
    switches: Switches,
    stream: Stream,
    calendar: &SessionCalendar,
) -> ServerStatus {
    let now = Utc::now();
    let mut warnings = Vec::new();
    let (operator_halt, halt_known) = match backend.trading().operator_halt().await {
        Ok(halt) => (halt, true),
        Err(e) => {
            warnings.push(ToolFailure::from(e));
            (None, false)
        }
    };
    let enabled = switches.trading;
    let submissions_suspended = backend.submissions_suspended();
    let trading = Trading {
        enabled,
        cancel_enabled: switches.cancel,
        admissible: enabled && halt_known && operator_halt.is_none() && !submissions_suspended,
        operator_halt,
        submissions_suspended,
        circuit: backend.circuit_state(),
    };
    let readiness =
        match tokio::time::timeout(PROBE_DEADLINE, backend.readiness(BASELINE_MAX_AGE_MS)).await {
            Ok(Ok(readiness)) => Some(readiness),
            Ok(Err(e)) => {
                warnings.push(e);
                None
            }
            Err(_) => {
                warnings.push(timeout("Readiness probe"));
                None
            }
        };
    let equity_session = match backend.market_data("The equity calendar") {
        Ok(schwab) if schwab.auth_manager().has_valid_tokens() => {
            match calendar.session(schwab, now).await {
                Ok(session) => Some(session),
                Err(e) => {
                    warnings.push(e);
                    None
                }
            }
        }
        Ok(_) => {
            warnings.push(ToolFailure::unauthenticated(
                "Schwab session is not authenticated, so the equity calendar was not read; \
                 the operator must run the OAuth flow",
            ));
            None
        }
        Err(e) => {
            warnings.push(e);
            None
        }
    };
    let paper = match backend.trading().paper() {
        Some(broker) => match broker.market_session().await {
            Ok((id, open)) => Some(PaperVenue {
                clock: broker.config().clock,
                session: PaperSession { id, open },
                accounts: broker.config().accounts.keys().cloned().collect(),
                model_version: broker.config().model_version.clone(),
            }),
            Err(e) => {
                warnings.push(ToolFailure::from(e));
                None
            }
        },
        None => None,
    };
    ServerStatus {
        mode: backend.mode(),
        version: env!("CARGO_PKG_VERSION"),
        server_time: now.into(),
        principal: None,
        trading,
        readiness,
        request_budget: backend.request_budget(),
        stream,
        equity_session,
        paper,
        warnings,
    }
}

fn timeout(what: &str) -> ToolFailure {
    ToolFailure::new(
        FailureCode::Timeout,
        format!(
            "{what} did not answer within {} seconds",
            PROBE_DEADLINE.as_secs()
        ),
    )
}

/// Schwab's equity calendar for the current day, with the next opening
/// looked up once the day's sessions are over, kept while it can still
/// answer: the day's schedule does not change, so only a date roll, a
/// predicted opening arriving, or having nothing ahead re-reads it.
#[derive(Clone, Default)]
pub struct SessionCalendar(Arc<Mutex<Option<CalendarEntry>>>);

#[derive(Clone)]
struct CalendarEntry {
    today: MarketHours,
    /// The first boundary on a later day, found by looking ahead when
    /// today's were already past at fetch time.
    next_open: Option<SessionTransition>,
    fetched_at: DateTime<Utc>,
}

impl SessionCalendar {
    /// The session at `now`, from cache when it can still answer.
    pub async fn session(
        &self,
        client: &SchwabClient,
        now: DateTime<Utc>,
    ) -> Result<EquitySession, ToolFailure> {
        let cached = self.0.lock().clone();
        let entry = match cached.filter(|entry| entry.answers(now)) {
            Some(entry) => entry,
            None => {
                let entry = CalendarEntry::fetch(client, now).await?;
                *self.0.lock() = Some(entry.clone());
                entry
            }
        };
        Ok(entry.session(now))
    }
}

impl CalendarEntry {
    async fn fetch(client: &SchwabClient, now: DateTime<Utc>) -> Result<Self, ToolFailure> {
        let today = equity_hours(client, None).await?;
        let mut next_open = None;
        if today.phase_at(now).1.is_none()
            && let Some(date) = today
                .date
                .as_deref()
                .and_then(|d| d.parse::<NaiveDate>().ok())
        {
            for ahead in 1..=LOOKAHEAD_DAYS {
                let Some(day) = date.checked_add_days(Days::new(ahead)) else {
                    break;
                };
                if let (_, Some(next)) = equity_hours(client, Some(day)).await?.phase_at(now) {
                    next_open = Some(next);
                    break;
                }
            }
        }
        Ok(Self {
            today,
            next_open,
            fetched_at: now,
        })
    }

    fn answers(&self, now: DateTime<Utc>) -> bool {
        let age = now - self.fetched_at;
        if age > MAX_AGE {
            return false;
        }
        // Schwab's local date, by the calendar's own offset, must still be
        // the one fetched.
        if let (Some(date), Some(offset)) = (self.today.date.as_deref(), self.offset())
            && now.with_timezone(&offset).format("%Y-%m-%d").to_string() != date
        {
            return false;
        }
        if self.today.phase_at(now).1.is_some() {
            return true;
        }
        match &self.next_open {
            Some(next) => now < next.at,
            None => age <= RECHECK,
        }
    }

    /// The offset the calendar's own timestamps carry.
    fn offset(&self) -> Option<FixedOffset> {
        self.today
            .session_hours
            .as_ref()
            .and_then(|hours| {
                [
                    hours.pre_market.as_deref(),
                    hours.regular_market.as_deref(),
                    hours.post_market.as_deref(),
                ]
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|window| window.start.as_deref())
                .filter_map(|start| DateTime::parse_from_rfc3339(start).ok())
                .map(|start| *start.offset())
                .next()
            })
            .or_else(|| self.next_open.as_ref().map(|next| *next.at.offset()))
    }

    fn session(&self, now: DateTime<Utc>) -> EquitySession {
        let (phase, next) = self.today.phase_at(now);
        EquitySession {
            phase,
            date: self.today.date.clone(),
            trading_day: self.today.is_open == Some(true),
            next_transition: next.or_else(|| self.next_open.clone().filter(|next| now < next.at)),
            fetched_at: self.fetched_at,
        }
    }
}

async fn equity_hours(
    client: &SchwabClient,
    date: Option<NaiveDate>,
) -> Result<MarketHours, ToolFailure> {
    let response = tokio::time::timeout(PROBE_DEADLINE, client.get_market(Market::Equity, date))
        .await
        .map_err(|_| timeout("Schwab market hours"))??;
    market_data::equity_hours(&response)
        .cloned()
        .ok_or_else(|| {
            ToolFailure::internal(
                "Equity calendar absent from Schwab market hours",
                format!("markets present: {:?}", response.keys().collect::<Vec<_>>()),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hours(value: Value) -> MarketHours {
        serde_json::from_value(value).unwrap()
    }
    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }
    fn trading_day(date: &str) -> MarketHours {
        hours(json!({
            "date": date, "isOpen": true,
            "sessionHours": {
                "preMarket": [{"start": format!("{date}T07:00:00-05:00"), "end": format!("{date}T09:30:00-05:00")}],
                "regularMarket": [{"start": format!("{date}T09:30:00-05:00"), "end": format!("{date}T16:00:00-05:00")}],
                "postMarket": [{"start": format!("{date}T16:00:00-05:00"), "end": format!("{date}T20:00:00-05:00")}]
            }
        }))
    }

    /// A day's calendar answers until Schwab's date rolls; after the last
    /// session only a looked-up opening keeps it, else it is re-read every
    /// quarter hour.
    #[test]
    fn calendar_entries_answer_until_they_cannot() {
        let fetched = at("2025-01-15T08:00:00-05:00");
        let entry = CalendarEntry {
            today: trading_day("2025-01-15"),
            next_open: None,
            fetched_at: fetched,
        };
        assert!(entry.answers(at("2025-01-15T09:30:00-05:00")));
        assert!(entry.answers(at("2025-01-15T13:59:00-05:00")));
        assert!(!entry.answers(at("2025-01-15T14:01:00-05:00")), "six hours");
        let evening = CalendarEntry {
            fetched_at: at("2025-01-15T20:30:00-05:00"),
            ..entry.clone()
        };
        assert!(evening.answers(at("2025-01-15T20:44:00-05:00")));
        assert!(
            !evening.answers(at("2025-01-15T20:46:00-05:00")),
            "nothing ahead"
        );
        let next_open = SessionTransition {
            phase: SessionPhase::PreMarket,
            at: DateTime::parse_from_rfc3339("2025-01-16T07:00:00-05:00").unwrap(),
        };
        let looked_ahead = CalendarEntry {
            next_open: Some(next_open.clone()),
            ..evening.clone()
        };
        assert!(looked_ahead.answers(at("2025-01-15T23:59:00-05:00")));
        assert!(
            !looked_ahead.answers(at("2025-01-16T00:01:00-05:00")),
            "date rolled"
        );
        let session = looked_ahead.session(at("2025-01-15T23:00:00-05:00"));
        assert_eq!(session.phase, SessionPhase::Closed);
        assert!(session.trading_day);
        assert_eq!(session.next_transition, Some(next_open));

        let closed = CalendarEntry {
            today: hours(json!({"date": "2025-01-18", "isOpen": false})),
            next_open: None,
            fetched_at: at("2025-01-18T12:00:00-05:00"),
        };
        assert!(closed.answers(at("2025-01-18T12:10:00-05:00")));
        assert!(!closed.answers(at("2025-01-18T12:20:00-05:00")));
        let session = closed.session(at("2025-01-18T12:10:00-05:00"));
        assert!(!session.trading_day);
        assert_eq!(session.next_transition, None);
    }

    /// The stream section says the same thing `stream_status` always has.
    #[test]
    fn stream_section_flags_reconciliation() {
        let fresh = Stream::from(StreamStatus {
            ready: true,
            generation: 1,
            lost_messages: 0,
        });
        assert!(fresh.connected && !fresh.reconciliation_required);
        let reconnected = Stream::from(StreamStatus {
            ready: true,
            generation: 2,
            lost_messages: 0,
        });
        assert!(reconnected.reconciliation_required);
    }
}
