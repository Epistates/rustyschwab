//! Reading the state, and carrying out operator commands.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rustyschwab::execution::{ExecutionStore, IntentState, OrderJournal, SubmissionOutcome};
use rustyschwab::paper::PaperBroker;
use rustyschwab::storage::StateDatabase;
use rustyschwab::types::trading::OrdersParams;
use rustyschwab::{SchwabClient, SchwabConfig};
use rustyschwab_mcp::backend::{Backend, Paper};
use rustyschwab_mcp::delegation::{self, Mode};
use rustyschwab_mcp::policy::PrincipalPolicy;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};

use crate::config::ConsoleConfig;
use crate::snapshot::{
    ActivityView, ApprovalView, BudgetView, DeadManView, PrincipalView, QuoteView, Snapshot,
};

/// Tool calls a snapshot carries, across principals.
pub const ACTIVITY_SHOWN: usize = 500;

/// How long an approval from the console lasts.
pub const APPROVAL_MINUTES: i64 = 15;

/// Terminal orders shown per account besides the working ones.
const RECENT_ORDERS: usize = 20;

/// Symbols quoted at once.
const MAX_QUOTED: usize = 40;

/// Something the operator asks the console to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Approve the order `digest` that `principal` is waiting on, for
    /// [`APPROVAL_MINUTES`].
    Approve {
        principal: String,
        digest: String,
    },
    /// Refuse it: its approval is revoked for good.
    Deny {
        principal: String,
        digest: String,
    },
    /// Delegate `mode` to `principal` until `until` (absent, until cancelled).
    Delegate {
        principal: String,
        mode: Mode,
        until: Option<DateTime<Utc>>,
    },
    /// Withdraw `principal`'s delegation: the policy's own mode again.
    EndDelegation {
        principal: String,
    },
    /// Halt every submission, on every server sharing this state.
    Halt {
        reason: String,
    },
    Resume,
    /// Halt, then cancel every working order on the accounts shown.
    Kill {
        reason: String,
    },
}

/// A principal the console follows.
struct Followed {
    policy: PrincipalPolicy,
    journal_path: PathBuf,
    /// Opened once its state exists, so watching never creates it.
    journal: Option<OrderJournal>,
}

impl Followed {
    fn journal(&mut self) -> Option<&OrderJournal> {
        if self.journal.is_none()
            && StateDatabase::new(self.journal_path.clone())
                .path()
                .exists()
        {
            self.journal = Some(OrderJournal::new(self.journal_path.clone()));
        }
        self.journal.as_ref()
    }
    /// The journal, created if need be, for a command.
    fn journal_for_command(&mut self) -> &OrderJournal {
        self.journal
            .get_or_insert_with(|| OrderJournal::new(self.journal_path.clone()))
    }
}

/// The console: what a frontend reads from and commands through.
pub struct Console {
    config: ConsoleConfig,
    backend: Backend,
    /// The venue's own journal (orders, account baselines, the halt).
    state: StateDatabase,
    followed: Vec<Followed>,
    cursors: HashMap<String, i64>,
    activity: VecDeque<ActivityView>,
    quotes: Vec<QuoteView>,
    quoted_at: Option<Instant>,
    /// Whether the live client has logged in (for quotes and kill); the
    /// time of the last failed attempt, so it is not retried every tick.
    live_login: Result<bool, Instant>,
}

impl std::fmt::Debug for Console {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Console")
            .field("mode", &self.mode())
            .field("operator", &self.config.operator)
            .field(
                "principals",
                &self
                    .followed
                    .iter()
                    .map(|p| &p.policy.name)
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl Console {
    /// Open the venue and the principals' journals `config` names. Nothing
    /// is sent to Schwab.
    pub async fn open(config: ConsoleConfig) -> anyhow::Result<Self> {
        let (backend, state) = match &config.paper {
            Some(files) => {
                let paper: rustyschwab::paper::PaperConfig =
                    serde_json::from_slice(&rustyschwab_mcp::read_config(&files.config)?)?;
                let broker = Arc::new(PaperBroker::open(files.state.clone(), paper).await?);
                let state = broker.order_journal().database().clone();
                (
                    Backend::Paper(Paper {
                        broker,
                        market_data: None,
                    }),
                    state,
                )
            }
            None => {
                let client = SchwabClient::new(SchwabConfig::from_env()?)?;
                let state = client.order_journal().database().clone();
                (Backend::Live(Arc::new(client)), state)
            }
        };
        let policies: Vec<PrincipalPolicy> = match &config.policy_file {
            Some(path) => serde_json::from_slice(&rustyschwab_mcp::read_config(path)?)?,
            None => Vec::new(),
        };
        let mut followed = Vec::new();
        for policy in policies {
            policy.validate()?;
            anyhow::ensure!(
                policy.name != config.operator,
                "The operator ({}) must not share a principal's name",
                config.operator
            );
            let digest = rustyschwab_mcp::policy::digest(policy.name.as_bytes());
            let journal_path = backend.trading().principal_journal_path(&digest);
            followed.push(Followed {
                policy,
                journal_path,
                journal: None,
            });
        }
        Ok(Self {
            config,
            backend,
            state,
            followed,
            cursors: HashMap::new(),
            activity: VecDeque::new(),
            quotes: Vec::new(),
            quoted_at: None,
            live_login: Ok(false),
        })
    }

    /// The venue: `live` or `paper`.
    pub fn mode(&self) -> &'static str {
        self.backend.mode()
    }

    pub fn operator(&self) -> &str {
        &self.config.operator
    }

    /// Read everything again.
    pub async fn refresh(&mut self) -> Snapshot {
        let now = Utc::now();
        let mut snapshot = Snapshot::empty(self.mode(), &self.config.operator);
        snapshot.at = now;
        let store = self.backend.trading().execution_store();
        match store.operator_halt().await {
            Ok(halt) => snapshot.halt = halt,
            Err(e) => snapshot.warnings.push(format!("operator halt: {e}")),
        }
        let mut placed: HashMap<(String, String), String> = HashMap::new();
        let mut fresh = Vec::new();
        for index in 0..self.followed.len() {
            let view = self
                .principal(index, now, &mut snapshot, &mut placed, &mut fresh)
                .await;
            snapshot.principals.push(view);
        }
        self.activity.extend(fresh);
        self.activity
            .make_contiguous()
            .sort_by(|a, b| a.at.cmp(&b.at).then(a.sequence.cmp(&b.sequence)));
        while self.activity.len() > ACTIVITY_SHOWN {
            self.activity.pop_front();
        }
        snapshot.activity = self.activity.iter().cloned().collect();
        snapshot.approvals.sort_by_key(|a| a.first_seen);

        let accounts = self.accounts(&mut snapshot.warnings).await;
        for hash in &accounts {
            self.account(hash, now, &placed, &mut snapshot).await;
        }
        if let Err(e) = self.refresh_quotes(&snapshot).await {
            snapshot.warnings.push(e);
        }
        snapshot.quotes = self.quotes.clone();
        snapshot
    }

    async fn principal(
        &mut self,
        index: usize,
        now: DateTime<Utc>,
        snapshot: &mut Snapshot,
        placed: &mut HashMap<(String, String), String>,
        fresh: &mut Vec<ActivityView>,
    ) -> PrincipalView {
        let now_ms = now.timestamp_millis();
        let followed = &mut self.followed[index];
        let policy = followed.policy.clone();
        let name = policy.name.clone();
        let mut view = PrincipalView {
            name: name.clone(),
            approval: policy.approval_mode(),
            delegation: None,
            ceiling: policy.delegation_ceiling(),
            dead_man: None,
            budget: BudgetView {
                max_orders: policy.max_daily_orders.map(|v| v.get()),
                max_notional: policy.max_daily_notional,
                ..BudgetView::default()
            },
            pending_submissions: 0,
            last_call: None,
            active: false,
        };
        let Some(journal) = followed.journal().cloned() else {
            return view;
        };
        view.active = true;
        let mut warn = |what: &str, e: &dyn std::fmt::Display| {
            snapshot.warnings.push(format!("{name}: {what}: {e}"));
        };
        match delegation::stored(&journal).await {
            Ok(grant) => {
                view.approval = delegation::effective(&policy, grant.as_ref(), now_ms);
                view.delegation = grant.filter(|g| g.active(now_ms) && g.mode == view.approval);
            }
            Err(e) => warn("delegation", &e),
        }
        let heartbeat = rustyschwab_mcp::dead_man::heartbeat_path(&journal);
        let heard = std::fs::metadata(&heartbeat)
            .and_then(|m| m.modified())
            .ok()
            .map(DateTime::<Utc>::from);
        view.last_call = heard;
        if let Some(window) = policy.dead_man_secs {
            let window = u64::from(window.get());
            let silent = heard.map_or(window, |at| (now - at).num_seconds().max(0) as u64);
            view.dead_man = Some(DeadManView {
                window_secs: window,
                seconds_left: window.saturating_sub(silent),
            });
        }
        match rustyschwab_mcp::guardrails::day_usage(&journal, &policy, None).await {
            Ok(usage) => {
                view.budget.orders_used = usage.orders;
                view.budget.notional_used = usage.notional;
            }
            Err(e) => warn("daily budget", &e),
        }
        match journal.unresolved_records(&name).await {
            Ok(records) => view.pending_submissions = records.len(),
            Err(e) => warn("pending submissions", &e),
        }
        let cursor = self.cursors.entry(name.clone()).or_insert(0);
        match journal.activity_page(*cursor, 500).await {
            Ok(page) => {
                for entry in page {
                    *cursor = entry.sequence;
                    fresh.push(activity(&name, entry));
                }
            }
            Err(e) => warn("activity", &e),
        }
        let since = now_ms - 24 * 3_600_000;
        match journal.intents(None, since, 200).await {
            Ok(intents) => snapshot.approvals.extend(
                intents
                    .into_iter()
                    .filter(|i| i.principal == name && i.state == IntentState::Awaiting)
                    .map(|i| ApprovalView {
                        description: rustyschwab_mcp::approvals::describe(&i.tool, &i.arguments),
                        rationale: i.arguments["rationale"].as_str().map(str::to_owned),
                        principal: i.principal,
                        digest: i.digest,
                        tool: i.tool,
                        first_seen: DateTime::from_timestamp_millis(i.first_seen).unwrap_or(now),
                        seen: i.seen,
                    }),
            ),
            Err(e) => warn("approvals", &e),
        }
        // Which orders are this principal's: the id its dispatch recorded,
        // or the tag it sent.
        match journal.records_since(&name, now_ms - 7 * 86_400_000).await {
            Ok(records) => {
                for record in records {
                    let arguments = &record.request["arguments"];
                    let Some(account) = arguments["account_hash"].as_str() else {
                        continue;
                    };
                    let id = match &record.outcome {
                        Some(SubmissionOutcome::Accepted { order_id }) => order_id.clone(),
                        Some(SubmissionOutcome::Reconciled { evidence }) => {
                            match rustyschwab_mcp::journal::Dispatch::parse(evidence) {
                                Some(rustyschwab_mcp::journal::Dispatch::Completed {
                                    order_id,
                                }) => order_id,
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    let key = id.or_else(|| {
                        arguments["order"]["tag"]
                            .as_str()
                            .map(|tag| format!("tag:{tag}"))
                    });
                    if let Some(key) = key {
                        placed.insert((account.to_owned(), key), name.clone());
                    }
                }
            }
            Err(e) => warn("orders placed", &e),
        }
        view
    }

    /// The accounts to show: the venue's, and those a principal is granted.
    async fn accounts(&self, warnings: &mut Vec<String>) -> Vec<String> {
        let mut accounts = BTreeSet::new();
        match &self.backend {
            Backend::Paper(_) => match self.backend.account_numbers().await {
                Ok(numbers) => accounts.extend(numbers.into_iter().map(|a| a.hash_value)),
                Err(e) => warnings.push(format!("accounts: {}", e.message)),
            },
            Backend::Live(_) => match self.state.known_accounts().await {
                Ok(known) => accounts.extend(known),
                Err(e) => warnings.push(format!("accounts: {e}")),
            },
        }
        for followed in &self.followed {
            accounts.extend(followed.policy.accounts.iter().cloned());
        }
        accounts.into_iter().collect()
    }

    async fn account(
        &self,
        hash: &str,
        now: DateTime<Utc>,
        placed: &HashMap<(String, String), String>,
        snapshot: &mut Snapshot,
    ) {
        let (account, observed_at, orders) = match &self.backend {
            Backend::Paper(_) => {
                let account = self
                    .backend
                    .account(hash, true)
                    .await
                    .map_err(|e| e.message)
                    .and_then(|a| serde_json::to_value(a).map_err(|e| e.to_string()));
                let params = OrdersParams::new(now - chrono::Duration::days(2), now);
                let orders = self
                    .backend
                    .account_orders(hash, &params)
                    .await
                    .map_err(|e| e.message)
                    .map(|orders| {
                        orders
                            .into_iter()
                            .filter_map(|o| serde_json::to_value(o).ok())
                            .collect::<Vec<_>>()
                    });
                (account, Some(now), orders)
            }
            Backend::Live(_) => {
                let state = self.state.account_state(hash).await;
                let observed = state
                    .as_ref()
                    .ok()
                    .and_then(Option::as_ref)
                    .and_then(|s| DateTime::from_timestamp_millis(s.observed_at));
                let account = match state {
                    Ok(Some(state)) => Ok(state.snapshot),
                    Ok(None) => Err("not reconciled yet".to_owned()),
                    Err(e) => Err(e.to_string()),
                };
                let orders = self
                    .state
                    .known_orders(hash, 200)
                    .await
                    .map_err(|e| e.to_string());
                (account, observed, orders)
            }
        };
        match account {
            Ok(account) => {
                let mut view = crate::read::account(hash, &account);
                view.observed_at = observed_at;
                snapshot.accounts.push(view);
            }
            Err(e) => snapshot.warnings.push(format!("account {hash}: {e}")),
        }
        match orders {
            Ok(orders) => {
                let mut views = Vec::new();
                for order in &orders {
                    crate::read::orders(hash, order, &mut views);
                }
                for view in &mut views {
                    view.principal = placed
                        .get(&(hash.to_owned(), view.order_id.clone()))
                        .cloned();
                }
                // Tags are on the payload, so match them separately.
                for order in &orders {
                    if let Some(tag) = order["tag"].as_str()
                        && let Some(principal) =
                            placed.get(&(hash.to_owned(), format!("tag:{tag}")))
                        && let Some(id) = order["orderId"]
                            .as_i64()
                            .map(|i| i.to_string())
                            .or_else(|| order["orderId"].as_str().map(str::to_owned))
                        && let Some(view) = views.iter_mut().find(|v| v.order_id == id)
                    {
                        view.principal.get_or_insert_with(|| principal.clone());
                    }
                }
                views.sort_by(|a, b| b.working.cmp(&a.working).then(b.entered.cmp(&a.entered)));
                let working = views.iter().filter(|v| v.working).count();
                views.truncate(working + RECENT_ORDERS);
                snapshot.orders.extend(views);
            }
            Err(e) => snapshot.warnings.push(format!("orders {hash}: {e}")),
        }
    }

    async fn refresh_quotes(&mut self, snapshot: &Snapshot) -> Result<(), String> {
        if self
            .quoted_at
            .is_some_and(|at| at.elapsed() < self.config.quote_refresh)
        {
            return Ok(());
        }
        self.quoted_at = Some(Instant::now());
        let mut symbols: BTreeSet<String> = self.config.watchlist.iter().cloned().collect();
        for account in &snapshot.accounts {
            symbols.extend(account.positions.iter().map(|p| p.symbol.clone()));
        }
        for order in snapshot.orders.iter().filter(|o| o.working) {
            if let Some(symbol) = order.legs.split_whitespace().nth(2) {
                symbols.insert(symbol.to_owned());
            }
        }
        if symbols.is_empty() {
            self.quotes.clear();
            return Ok(());
        }
        if matches!(self.backend, Backend::Live(_))
            && (!self.config.live_quotes || !self.ensure_login().await)
        {
            return Ok(());
        }
        let symbols: Vec<&str> = symbols
            .iter()
            .map(String::as_str)
            .take(MAX_QUOTED)
            .collect();
        let quotes = self
            .backend
            .quotes(&symbols, &[], None)
            .await
            .map_err(|e| format!("quotes: {}", e.message))?;
        let quotes = serde_json::to_value(quotes).map_err(|e| e.to_string())?;
        self.quotes = symbols
            .iter()
            .filter_map(|symbol| crate::read::quote(symbol, &quotes))
            .collect();
        Ok(())
    }

    /// Log the live client in, once; a failure is retried after a minute.
    async fn ensure_login(&mut self) -> bool {
        let Backend::Live(client) = &self.backend else {
            return true;
        };
        match self.live_login {
            Ok(true) => true,
            Err(at) if at.elapsed() < std::time::Duration::from_secs(60) => false,
            _ => match client.init().await {
                Ok(()) => {
                    self.live_login = Ok(true);
                    true
                }
                Err(e) => {
                    tracing::warn!(error = %e, "the console could not log in to Schwab");
                    self.live_login = Err(Instant::now());
                    false
                }
            },
        }
    }

    fn followed(&mut self, principal: &str) -> anyhow::Result<&mut Followed> {
        self.followed
            .iter_mut()
            .find(|f| f.policy.name == principal)
            .ok_or_else(|| anyhow::anyhow!("No principal named {principal} in the policy file"))
    }

    /// Carry out `command`; the answer says what was done.
    pub async fn execute(&mut self, command: Command) -> anyhow::Result<String> {
        let operator = self.config.operator.clone();
        match command {
            Command::Approve { principal, digest } => {
                let journal = self.followed(&principal)?.journal_for_command().clone();
                let intent = journal
                    .intents(Some(&digest), 0, 1)
                    .await?
                    .pop()
                    .ok_or_else(|| anyhow::anyhow!("{principal} has not asked for {digest}"))?;
                anyhow::ensure!(
                    intent.principal == principal,
                    "That order was asked for by {}, not {principal}",
                    intent.principal
                );
                anyhow::ensure!(
                    intent.state == IntentState::Awaiting,
                    "That order is no longer waiting ({:?})",
                    intent.state
                );
                let expires = Utc::now().timestamp() + APPROVAL_MINUTES * 60;
                journal
                    .approve(&principal, &digest, &operator, expires)
                    .await?;
                Ok(format!(
                    "Approved for {APPROVAL_MINUTES} minutes: {}",
                    rustyschwab_mcp::approvals::describe(&intent.tool, &intent.arguments)
                ))
            }
            Command::Deny { principal, digest } => {
                let journal = self.followed(&principal)?.journal_for_command().clone();
                journal.revoke_approval(&digest, &operator).await?;
                Ok(format!(
                    "Refused {}'s order {}",
                    principal,
                    &digest[..digest.len().min(12)]
                ))
            }
            Command::Delegate {
                principal,
                mode,
                until,
            } => {
                let followed = self.followed(&principal)?;
                let policy = followed.policy.clone();
                let journal = followed.journal_for_command().clone();
                let grant = delegation::grant(
                    &journal,
                    &policy,
                    mode,
                    until.map(|u| u.timestamp_millis()),
                    &operator,
                )
                .await?;
                Ok(format!(
                    "{principal}: {} {}",
                    describe_mode(&grant.mode),
                    match until {
                        Some(until) => format!("until {}", until.format("%Y-%m-%d %H:%M UTC")),
                        None => "until cancelled".into(),
                    }
                ))
            }
            Command::EndDelegation { principal } => {
                let followed = self.followed(&principal)?;
                let own = describe_mode(&followed.policy.approval_mode());
                let journal = followed.journal_for_command().clone();
                delegation::cancel(&journal, &operator).await?;
                Ok(format!("{principal}: back to its policy ({own})"))
            }
            Command::Halt { reason } => {
                anyhow::ensure!(!reason.trim().is_empty(), "A halt needs a reason");
                self.backend
                    .trading()
                    .execution_store()
                    .set_operator_halt(Some(reason.clone()), &operator)
                    .await?;
                Ok(format!("Trading halted: {reason}"))
            }
            Command::Resume => {
                self.backend
                    .trading()
                    .execution_store()
                    .set_operator_halt(None, &operator)
                    .await?;
                Ok("Trading resumed".into())
            }
            Command::Kill { reason } => {
                anyhow::ensure!(!reason.trim().is_empty(), "A kill needs a reason");
                // Halt first, so nothing new is placed while orders are cancelled.
                self.backend
                    .trading()
                    .halt_submissions(&reason, &operator)
                    .await?;
                anyhow::ensure!(
                    self.ensure_login().await,
                    "Trading is halted, but the console could not log in to Schwab to cancel \
                     orders; run `rustyschwab-admin kill`"
                );
                let mut warnings = Vec::new();
                let accounts = self.accounts(&mut warnings).await;
                let (mut cancelled, mut failed) = (0, 0);
                for account in &accounts {
                    match self.backend.trading().cancel_working_orders(account).await {
                        Ok(outcomes) => {
                            for (_, outcome) in outcomes {
                                if outcome.is_ok() {
                                    cancelled += 1;
                                } else {
                                    failed += 1;
                                }
                            }
                        }
                        Err(e) => {
                            failed += 1;
                            tracing::warn!(account, error = %e, "kill could not list working orders");
                        }
                    }
                }
                Ok(format!(
                    "Halted; {cancelled} orders cancelled{}",
                    if failed > 0 {
                        format!(", {failed} failed (see the log)")
                    } else {
                        String::new()
                    }
                ))
            }
        }
    }

    /// Run the console on its own task: it refreshes every
    /// `config.refresh`, and at once after a command.
    pub fn spawn(mut self) -> Handle {
        let (snapshots, receiver) = watch::channel(Arc::new(Snapshot::empty(
            self.mode(),
            &self.config.operator,
        )));
        let (commands, mut requests) = mpsc::channel::<Request>(16);
        let period = self.config.refresh;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    request = requests.recv() => {
                        let Some(Request { command, reply }) = request else { return };
                        let outcome = self.execute(command).await.map_err(|e| format!("{e:#}"));
                        let _ = reply.send(outcome);
                    }
                }
                let snapshot = Arc::new(self.refresh().await);
                if snapshots.send(snapshot).is_err() {
                    return;
                }
            }
        });
        Handle { receiver, commands }
    }
}

/// A tool call from its activity entry.
fn activity(principal: &str, entry: rustyschwab::execution::Activity) -> ActivityView {
    let payload = &entry.payload;
    let tool = payload["tool"].as_str().unwrap_or("?").to_owned();
    let arguments = &payload["arguments"];
    let summary = if arguments.get("order").is_some() || tool == "cancel_order" {
        rustyschwab_mcp::approvals::describe(&tool, arguments)
    } else {
        brief(arguments)
    };
    ActivityView {
        sequence: entry.sequence,
        at: DateTime::from_timestamp_millis(entry.at).unwrap_or_default(),
        principal: principal.to_owned(),
        outcome: payload["outcome"].as_str().unwrap_or("?").to_owned(),
        duration_ms: payload["durationMs"].as_u64().unwrap_or(0),
        rationale: arguments["rationale"].as_str().map(str::to_owned),
        message: payload["message"].as_str().map(str::to_owned),
        order_id: payload["orderId"].as_str().map(str::to_owned),
        summary,
        tool,
    }
}

/// A call's arguments in a few words: their scalar values, keys first.
fn brief(arguments: &Value) -> String {
    let Some(arguments) = arguments.as_object() else {
        return String::new();
    };
    arguments
        .iter()
        .filter_map(|(key, value)| {
            let value = match value {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Array(items) if items.iter().all(Value::is_string) => items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(","),
                _ => return None,
            };
            Some(format!("{key}={value}"))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A mode in words.
pub fn describe_mode(mode: &Mode) -> String {
    match mode {
        Mode::Supervised => "supervised (every order approved)".into(),
        Mode::Threshold { above } => format!("approve orders over ${above}"),
        Mode::Autonomous => "autonomous (no approvals)".into(),
    }
}

/// The notional a mode lets through unapproved, for display.
pub fn unapproved_reach(mode: &Mode) -> Option<Decimal> {
    match mode {
        Mode::Supervised => Some(Decimal::ZERO),
        Mode::Threshold { above } => Some(*above),
        Mode::Autonomous => None,
    }
}

struct Request {
    command: Command,
    reply: oneshot::Sender<Result<String, String>>,
}

/// A running console, for a frontend: the latest snapshot as it changes,
/// and commands.
#[derive(Clone)]
pub struct Handle {
    receiver: watch::Receiver<Arc<Snapshot>>,
    commands: mpsc::Sender<Request>,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle").finish_non_exhaustive()
    }
}

impl Handle {
    /// The latest snapshot.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.receiver.borrow().clone()
    }
    /// A receiver that wakes on each new snapshot.
    pub fn subscribe(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.receiver.clone()
    }
    /// Carry out `command`: what was done, or why not.
    pub async fn execute(&self, command: Command) -> Result<String, String> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Request { command, reply })
            .await
            .map_err(|_| "The console has stopped".to_owned())?;
        answer
            .await
            .map_err(|_| "The console has stopped".to_owned())?
    }
}
