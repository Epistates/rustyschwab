use super::model::*;
use crate::{
    Error, Result,
    coordination::{Lease, lease_held},
    execution::{OrderJournal, SubmissionOutcome, ensure_not_halted, trip_halt},
    risk::{DAILY_LOSS_CONTROL, daily_loss_reason},
    storage::{audit, meta, sql},
    types::{
        common::AssetType,
        trading::{
            OrderDuration, OrderInstruction, OrderRequest, OrderSession, OrderStrategyType,
            OrderSubmitResponse, OrderType,
        },
    },
    utils::canonical_json,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use rust_decimal::Decimal;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const PAPER_SCHEMA: &str = "\
    CREATE TABLE paper_accounts(account TEXT PRIMARY KEY,payload TEXT NOT NULL);\
    CREATE TABLE paper_orders(id INTEGER PRIMARY KEY AUTOINCREMENT,account TEXT NOT NULL,\
      active INTEGER NOT NULL,payload TEXT NOT NULL);\
    CREATE INDEX paper_active ON paper_orders(active,id);\
    CREATE INDEX paper_account_orders ON paper_orders(account,id);\
    CREATE TABLE paper_quotes(symbol TEXT PRIMARY KEY,payload TEXT NOT NULL);\
    CREATE TABLE paper_quote_watermarks(symbol TEXT PRIMARY KEY,source_at INTEGER NOT NULL);\
    CREATE TABLE paper_events(sequence INTEGER PRIMARY KEY,payload TEXT NOT NULL,\
      result TEXT NOT NULL);";
const COUNT_PAPER_TABLES: &str = "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' \
     AND name IN ('paper_accounts','paper_orders','paper_quotes','paper_quote_watermarks',\
     'paper_events')";
/// Live execution rows that would make a database unsafe to reuse for paper.
const COUNT_LIVE_STATE_ROWS: &str = "SELECT (SELECT COUNT(*) FROM submissions)\
     +(SELECT COUNT(*) FROM accounts)+(SELECT COUNT(*) FROM audit)";
const COUNT_CAPTURE_METADATA: &str =
    "SELECT COUNT(*) FROM meta WHERE key IN ('paper_capture_version','paper_capture_count')";
/// Whether this database ever claimed complete command capture.
const SELECT_CAPTURE_EXPECTED: &str = "SELECT \
     EXISTS(SELECT 1 FROM meta WHERE key='paper_capture_version') \
     OR EXISTS(SELECT 1 FROM audit WHERE kind='paper_command')";
const SELECT_ORDER_PAGE: &str =
    "SELECT payload FROM paper_orders WHERE account=?1 AND id>?2 ORDER BY id LIMIT ?3";
/// Fills are audit rows keyed by order ID; the index keeps per-order reads
/// from scanning the whole audit trail.
const INDEX_FILLS: &str = "CREATE INDEX IF NOT EXISTS paper_fills ON audit(kind,subject,sequence)";
const SELECT_FILLS: &str =
    "SELECT payload FROM audit WHERE kind='paper_fill' AND subject=?1 ORDER BY sequence";
const UPSERT_QUOTE_WATERMARK: &str = "INSERT INTO paper_quote_watermarks VALUES(?1,?2) \
     ON CONFLICT(symbol) DO UPDATE SET source_at=excluded.source_at";
const UPSERT_QUOTE: &str = "INSERT INTO paper_quotes VALUES(?1,?2) \
     ON CONFLICT(symbol) DO UPDATE SET payload=excluded.payload";
const SELECT_EVENT_PAGE: &str = "SELECT sequence, payload FROM paper_events \
     WHERE sequence>?1 AND sequence<=?2 ORDER BY sequence LIMIT 1000";
const SELECT_EVENT_PAYLOAD: &str = "SELECT payload FROM paper_events WHERE sequence=?1";
const REPLACE_EVENT_PAYLOAD: &str = "UPDATE paper_events SET payload=?2 WHERE sequence=?1";

/// Independent execution engine. Durable configuration is immutable for a run.
#[derive(Clone, Debug)]
pub struct PaperBroker {
    journal: OrderJournal,
    config: Arc<PaperConfig>,
    draining: Arc<AtomicBool>,
}
/// Title carried by every paper venue rejection, so callers can tell a
/// simulated rejection from a Schwab one without parsing the detail text.
pub const PAPER_REJECTION_TITLE: &str = "Paper broker rejected the order";
/// Title carried when the venue has no such account or order.
pub const PAPER_NOT_FOUND_TITLE: &str = "Paper broker has no such account or order";

/// Caller supplied an unusable argument, event or command.
fn invalid(message: &str) -> Error {
    Error::InvalidParameter(message.into())
}
/// Durable state or the engine's own arithmetic broke an invariant. Never a
/// caller error; in a mutation this becomes `SubmissionUnknown`.
fn corrupt(message: &str) -> Error {
    Error::Storage(message.into())
}
/// A venue answer shaped like a Schwab HTTP error so code written against
/// live trading handles paper identically; the reason travels as the
/// `ApiError` detail.
fn venue_error(status: reqwest::StatusCode, title: &str, detail: &str) -> Error {
    Error::Broker {
        status,
        details: vec![crate::error::ApiError::new(
            Some(status.as_u16().to_string()),
            title,
            Some(detail.to_owned()),
        )],
        request_id: None,
    }
}
/// The venue declined the command, as a 400.
fn rejected(reason: &str) -> Error {
    venue_error(
        reqwest::StatusCode::BAD_REQUEST,
        PAPER_REJECTION_TITLE,
        reason,
    )
}
/// The account or order does not exist at the venue, as a 404.
fn missing(what: &str) -> Error {
    venue_error(
        reqwest::StatusCode::NOT_FOUND,
        PAPER_NOT_FOUND_TITLE,
        &format!("Unknown paper {what}"),
    )
}
// Storage/worker failures can obscure a commit outcome. Never report a
// conclusive broker rejection merely because persistence returned an error.
fn mutation_error(error: Error, scope: String) -> Error {
    match error {
        Error::Storage(_) | Error::Io(_) => Error::SubmissionUnknown {
            attempt_id: scope,
            source: Box::new(error),
        },
        other => other,
    }
}
pub(super) fn add(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_add(b)
        .ok_or_else(|| corrupt("Decimal addition overflow"))
}
pub(super) fn sub(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_sub(b)
        .ok_or_else(|| corrupt("Decimal subtraction overflow"))
}
pub(super) fn mul(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_mul(b)
        .ok_or_else(|| corrupt("Decimal multiplication overflow"))
}
pub(super) fn div(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_div(b)
        .ok_or_else(|| corrupt("Decimal division overflow"))
}
fn encode(value: &impl Serialize) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}
/// Paper run state that `initialize` wrote; a missing key is corruption.
fn get<T: DeserializeOwned>(c: &Connection, key: &str) -> Result<T> {
    meta::get_json(c, key)?.ok_or_else(|| corrupt("Paper run metadata is missing"))
}
fn put(c: &Connection, key: &str, value: &impl Serialize) -> Result<()> {
    meta::put_json(c, key, value)
}
fn load_ledger(c: &Connection, account: &str) -> Result<Option<PaperAccount>> {
    let value: Option<String> = c
        .query_row(
            "SELECT payload FROM paper_accounts WHERE account=?1",
            [account],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql)?;
    value.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
}
/// A caller-named account; absence is the caller's error.
fn ledger(c: &Connection, account: &str) -> Result<PaperAccount> {
    load_ledger(c, account)?.ok_or_else(|| missing("account"))
}
/// A configured account; absence means the durable state is incomplete.
fn configured_ledger(c: &Connection, account: &str) -> Result<PaperAccount> {
    load_ledger(c, account)?.ok_or_else(|| corrupt("Paper account state is incomplete"))
}
fn save_ledger(c: &Connection, account: &str, value: &PaperAccount) -> Result<()> {
    c.execute(
        "UPDATE paper_accounts SET payload=?2 WHERE account=?1",
        params![account, encode(value)?],
    )
    .map_err(sql)?;
    Ok(())
}
fn find_order(c: &Connection, account: &str, id: &str) -> Result<PaperOrder> {
    let value: Option<String> = c
        .query_row(
            "SELECT payload FROM paper_orders WHERE account=?1 AND id=?2",
            params![account, id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql)?;
    Ok(serde_json::from_str(
        &value.ok_or_else(|| missing("order"))?,
    )?)
}
fn open_orders(c: &Connection) -> Result<Vec<PaperOrder>> {
    let mut statement = c
        .prepare("SELECT payload FROM paper_orders WHERE active=1 ORDER BY id")
        .map_err(sql)?;
    statement
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(sql)?
        .map(|v| Ok(serde_json::from_str(&v.map_err(sql)?)?))
        .collect()
}
fn save_order(c: &Connection, order: &PaperOrder) -> Result<()> {
    c.execute(
        "UPDATE paper_orders SET active=?2,payload=?3 WHERE id=?1",
        params![order.order_id, order.status.is_open(), encode(order)?],
    )
    .map_err(sql)?;
    c.execute(
        "UPDATE reservations SET quantity=?2,notional=?3,state=?4 WHERE id=?1",
        params![
            format!("paper-{}", order.order_id),
            order.remaining_quantity.to_string(),
            order.reserved_cash.to_string(),
            if order.status.is_open() {
                "held"
            } else {
                "released"
            }
        ],
    )
    .map_err(sql)?;
    Ok(())
}
fn effective_clock(mut clock: Clock, config: &PaperConfig) -> Result<Clock> {
    if config.clock == PaperClockMode::Wall {
        let now = chrono::Utc::now().timestamp_millis();
        if now < clock.at_ms {
            return Err(corrupt("Wall clock moved backwards"));
        }
        clock.at_ms = now;
    }
    Ok(clock)
}
/// The latest quote the venue has seen for `symbol`, fresh or not.
fn load_quote(c: &Connection, symbol: &str) -> Result<Option<Quote>> {
    let value: Option<String> = c
        .query_row(
            "SELECT payload FROM paper_quotes WHERE symbol=?1",
            [symbol],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql)?;
    value.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
}
/// Whether orders are judged against `quote` now: the session is open and
/// the quote is within `quote_max_age_ms`.
fn is_fresh(quote: &Quote, clock: &Clock, config: &PaperConfig) -> bool {
    clock.open
        && quote.source_at_ms <= clock.at_ms
        && clock.at_ms - quote.source_at_ms <= config.quote_max_age_ms
}
/// The latest quote for `symbol` if it is fresh; `None` when there is no
/// usable quote.
fn fresh_quote(
    c: &Connection,
    symbol: &str,
    clock: &Clock,
    config: &PaperConfig,
) -> Result<Option<Quote>> {
    Ok(load_quote(c, symbol)?.filter(|quote| is_fresh(quote, clock, config)))
}
/// Conservative marks of the account's holdings: their value at fresh bids
/// (`None` while any held position lacks one) and their remaining cost basis.
fn mark_positions(
    c: &Connection,
    ledger: &PaperAccount,
    clock: &Clock,
    config: &PaperConfig,
) -> Result<(Option<Decimal>, Decimal)> {
    let mut market = Some(Decimal::ZERO);
    let mut basis = Decimal::ZERO;
    for (symbol, position) in &ledger.positions {
        basis = add(basis, position.cost_basis)?;
        if position.quantity.is_zero() {
            continue;
        }
        let quote = fresh_quote(c, symbol, clock, config)?;
        market = match (market, quote) {
            (Some(value), Some(q)) => Some(add(value, mul(position.quantity, q.bid)?)?),
            _ => None,
        };
    }
    Ok((market, basis))
}
fn unsettled_cash(ledger: &PaperAccount) -> Result<Decimal> {
    ledger
        .pending_cash
        .iter()
        .try_fold(Decimal::ZERO, |sum, p| add(sum, p.amount))
}
/// Cash, unsettled proceeds and the marked holdings.
fn equity(ledger: &PaperAccount, market: Option<Decimal>) -> Result<Option<Decimal>> {
    market
        .map(|v| add(add(ledger.cash, unsettled_cash(ledger)?)?, v))
        .transpose()
}
/// How far `equity` has fallen from this session's mark; `None` without both.
fn session_loss(
    ledger: &PaperAccount,
    clock: &Clock,
    equity: Option<Decimal>,
) -> Result<Option<Decimal>> {
    ledger
        .session_mark
        .as_ref()
        .filter(|mark| mark.session_id == clock.session_id)
        .zip(equity)
        .map(|(mark, equity)| sub(mark.equity, equity))
        .transpose()
}
/// The daily-loss control. Each capped account is marked at every event. A
/// session is measured from the previous session's last complete mark (its
/// close), so a gap at the open counts as the live venue's start-of-day
/// value would; the account's first session starts from its first mark.
/// Once equity has fallen further below that than the cap, the halt trips,
/// in this transaction. Clearing the halt does not reset the day; the next
/// event trips it again until a new session sets a new mark.
fn enforce_daily_loss(tx: &Connection, config: &PaperConfig, clock: &Clock) -> Result<()> {
    for (name, settings) in &config.accounts {
        let Some(cap) = settings.limits.max_daily_loss else {
            continue;
        };
        let mut ledger = configured_ledger(tx, name)?;
        let (market, _) = mark_positions(tx, &ledger, clock, config)?;
        let Some(equity) = equity(&ledger, market)? else {
            continue;
        };
        let opened = match ledger
            .session_mark
            .as_ref()
            .filter(|mark| mark.session_id == clock.session_id)
        {
            Some(mark) => mark.equity,
            None => {
                let opening = ledger
                    .last_mark
                    .as_ref()
                    .filter(|mark| mark.session_id != clock.session_id)
                    .map_or(equity, |mark| mark.equity);
                ledger.session_mark = Some(SessionMark {
                    session_id: clock.session_id.clone(),
                    equity: opening,
                });
                opening
            }
        };
        let current = SessionMark {
            session_id: clock.session_id.clone(),
            equity,
        };
        if ledger.last_mark.as_ref() != Some(&current) {
            ledger.last_mark = Some(current);
            save_ledger(tx, name, &ledger)?;
        }
        let loss = sub(opened, equity)?;
        if loss > cap {
            let reason = daily_loss_reason(name, "equity", opened, equity, loss, cap);
            trip_halt(tx, &reason, DAILY_LOSS_CONTROL)?;
        }
    }
    Ok(())
}
/// Latest instant the venue clocks: 9999-12-31T23:59:59Z.
pub(super) const MAX_AT_MS: i64 = 253402300799000;
/// Largest per-side size a quote may offer.
pub(super) const MAX_QUOTE_SIZE: u64 = 1_000_000_000;
pub(super) fn symbol_valid(symbol: &str) -> bool {
    !symbol.is_empty()
        && symbol.len() <= 32
        && symbol
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
}
pub(super) fn positive_bounded(v: Decimal) -> bool {
    v > Decimal::ZERO && v <= Decimal::from(1_000_000_000_000_i64)
}
fn validate_config(config: &PaperConfig) -> Result<()> {
    if config.model_version != MODEL_VERSION
        || config.accounts.is_empty()
        || config.accounts.len() > 100
        || !(0..=60000).contains(&config.execution_latency_ms)
        || !(0..=60000).contains(&config.cancel_latency_ms)
        || !(1..=60000).contains(&config.quote_max_age_ms)
        || !(1..=10000).contains(&config.max_working_orders)
        || config.slippage_bps < Decimal::ZERO
        || config.slippage_bps > Decimal::from(100)
        || config.market_collar_bps < config.slippage_bps
        || config.market_collar_bps > Decimal::from(1000)
        || config.fee_per_share < Decimal::ZERO
        || config.fee_per_share > Decimal::ONE
        || !(Decimal::ZERO..=Decimal::from(1000)).contains(&config.fee_bps)
        || !(Decimal::ZERO..=Decimal::from(100)).contains(&config.sell_fee_bps)
        || !(Decimal::ZERO..=Decimal::ONE).contains(&config.sell_fee_per_share)
        || config
            .sell_fee_per_share_cap
            .is_some_and(|cap| !(Decimal::ZERO..=Decimal::from(1_000_000)).contains(&cap))
        || config.settlement_calendar.len() > 10000
        || config
            .settlement_calendar
            .iter()
            .any(|(id, at)| id.is_empty() || id.len() > 64 || !(0..=MAX_AT_MS).contains(at))
    {
        return Err(Error::Config(
            "Invalid paper configuration or model version".into(),
        ));
    }
    for (name, account) in &config.accounts {
        if !name.starts_with("paper-")
            || name.len() > 64
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || !positive_bounded(account.initial_cash)
            || !positive_bounded(account.limits.max_open_quantity)
            || !(1..=60000).contains(&account.limits.max_snapshot_age_ms)
            || !positive_bounded(account.limits.max_open_notional)
            || !positive_bounded(account.limits.max_position_quantity)
            || account
                .limits
                .max_daily_loss
                .is_some_and(|cap| !positive_bounded(cap))
        {
            return Err(Error::Config(
                "Paper account names must start paper- and limits must be positive and bounded"
                    .into(),
            ));
        }
    }
    Ok(())
}
fn validate_order(order: &OrderRequest) -> Result<()> {
    order.validate()?;
    if order.order_strategy_type != OrderStrategyType::Single
        || order.order_leg_collection.len() != 1
        || !order.child_order_strategies.is_empty()
        || order.complex_order_strategy_type.is_some()
        || order.session != Some(OrderSession::Normal)
        || !matches!(
            order.duration,
            Some(OrderDuration::Day | OrderDuration::Gtc)
        )
        || !matches!(
            order.order_type,
            Some(OrderType::Market | OrderType::Limit | OrderType::Stop | OrderType::StopLimit)
        )
        || order.stop_type.is_some()
        || order.price_link_basis.is_some()
        || order.price_link_type.is_some()
        || order.tax_lot_method.is_some()
        || order.special_instruction.is_some()
        || order.requested_destination.is_some()
        || order.stop_price_offset.is_some()
        || order.stop_price_link_type.is_some()
        || order.stop_price_link_basis.is_some()
    {
        return Err(invalid(
            "Paper v2 supports single-leg NORMAL DAY/GTC equity \
             MARKET/LIMIT/STOP/STOP_LIMIT orders only",
        ));
    }
    let leg = &order.order_leg_collection[0];
    if leg.instrument.asset_type != AssetType::Equity
        || !symbol_valid(&leg.instrument.symbol)
        || leg.position_effect.is_some()
        || !matches!(
            leg.instruction,
            OrderInstruction::Buy | OrderInstruction::Sell
        )
        || !positive_bounded(leg.quantity)
        || !leg.quantity.fract().is_zero()
        || order.quantity.is_some_and(|q| q != leg.quantity)
        || order.tag.as_ref().is_none_or(|v| {
            v.is_empty()
                || v.len() > 128
                || !v
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
    {
        return Err(invalid(
            "Paper orders require whole long-equity shares and a unique nonempty tag",
        ));
    }
    if matches!(order.order_type, Some(OrderType::Market | OrderType::Stop))
        && order.price.is_some()
    {
        return Err(invalid("Market order must not specify a limit"));
    }
    if !matches!(
        order.order_type,
        Some(OrderType::Stop | OrderType::StopLimit)
    ) && order.stop_price.is_some()
    {
        return Err(invalid("Only stop orders accept stopPrice"));
    }
    if let Some(stop) = order.stop_price
        && (stop < Decimal::ONE || !positive_bounded(stop) || stop.scale() > 2)
    {
        return Err(invalid("Invalid paper stop price"));
    }
    if let Some(price) = order.price
        && (price < Decimal::ONE || !positive_bounded(price) || price.scale() > 2)
    {
        return Err(invalid(
            "Paper prices must be >= 1 with at most two decimal places",
        ));
    }
    Ok(())
}
fn buy(order: &PaperOrder) -> bool {
    order.request.order_leg_collection[0].instruction == OrderInstruction::Buy
}
fn symbol(order: &PaperOrder) -> &str {
    &order.request.order_leg_collection[0].instrument.symbol
}
fn reserve_cash(order: &PaperOrder, config: &PaperConfig) -> Result<Decimal> {
    if buy(order) && order.status.is_open() {
        // The most one share can cost: the price bound and every buy fee.
        let per_share = add(
            mul(order.price_bound, add(Decimal::ONE, bps(config.fee_bps)?)?)?,
            config.fee_per_share,
        )?;
        mul(order.remaining_quantity, per_share)
    } else {
        Ok(Decimal::ZERO)
    }
}
fn bps(rate: Decimal) -> Result<Decimal> {
    div(rate, Decimal::from(10_000))
}
/// The fee on one fill: the per-share and notional fees on every fill, and
/// on a sell the sell-only fees, the per-share part capped per fill.
fn fill_fee(
    config: &PaperConfig,
    is_buy: bool,
    quantity: Decimal,
    notional: Decimal,
) -> Result<Decimal> {
    let mut fee = add(
        mul(quantity, config.fee_per_share)?,
        mul(notional, bps(config.fee_bps)?)?,
    )?;
    if !is_buy {
        let mut per_share = mul(quantity, config.sell_fee_per_share)?;
        if let Some(cap) = config.sell_fee_per_share_cap {
            per_share = per_share.min(cap);
        }
        fee = add(
            add(fee, mul(notional, bps(config.sell_fee_bps)?)?)?,
            per_share,
        )?;
    }
    Ok(fee)
}
fn end_order(
    c: &Connection,
    order: &mut PaperOrder,
    status: PaperOrderStatus,
    at: i64,
) -> Result<()> {
    order.status = status;
    order.reserved_cash = Decimal::ZERO;
    order.cancel_at_ms = None;
    save_order(c, order)?;
    audit(
        c,
        "paper_order_terminal",
        &order.order_id,
        &json!({
        "mode":"paper","at_ms":at,"status":status}
        ),
    )
}
fn check_admission(c: &Connection, fence: Option<&Lease>, draining: bool) -> Result<()> {
    if draining {
        return Err(Error::Suspended("Paper broker is draining".into()));
    }
    ensure_not_halted(c)?;
    if let Some(fence) = fence
        && !lease_held(c, fence)?
    {
        return Err(Error::OwnershipLost);
    }
    Ok(())
}
impl PaperBroker {
    /// Open or create a dedicated paper database. Existing live execution state is rejected.
    pub async fn open(path: PathBuf, config: PaperConfig) -> Result<Self> {
        validate_config(&config)?;
        let journal = OrderJournal::new(path);
        let saved = config.clone();
        journal
            .database()
            .run(move |c| initialize(c, &saved))
            .await?;
        Ok(Self {
            journal,
            config: Arc::new(config),
            draining: Arc::new(AtomicBool::new(false)),
        })
    }
    pub(crate) async fn status(&self) -> Result<bool> {
        let config = self.config.clone();
        self.journal
            .database()
            .run(move |c| {
                let clock = effective_clock(get(c, "paper_clock")?, &config)?;
                let mut statement = c.prepare("SELECT symbol FROM paper_quotes").map_err(sql)?;
                let symbols = statement
                    .query_map([], |r| r.get::<_, String>(0))
                    .map_err(sql)?;
                for symbol in symbols {
                    if fresh_quote(c, &symbol.map_err(sql)?, &clock, &config)?.is_some() {
                        return Ok(true);
                    }
                }
                Ok(false)
            })
            .await
    }
    pub fn order_journal(&self) -> &OrderJournal {
        &self.journal
    }
    pub fn config(&self) -> &PaperConfig {
        &self.config
    }
    pub fn begin_drain(&self) {
        self.draining.store(true, Ordering::Release);
    }
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }
    pub async fn place_order(
        &self,
        account: &str,
        order: &OrderRequest,
    ) -> Result<OrderSubmitResponse> {
        self.submit(account, None, order, None).await
    }
    pub async fn place_order_fenced(
        &self,
        account: &str,
        order: &OrderRequest,
        fence: &Lease,
    ) -> Result<OrderSubmitResponse> {
        self.submit(account, None, order, Some(fence.clone())).await
    }
    /// Replacement is atomic at command acceptance; it loses priority. Quantity is
    /// the new remaining quantity, not the original order's cumulative quantity.
    pub async fn replace_order(
        &self,
        account: &str,
        id: &str,
        order: &OrderRequest,
    ) -> Result<OrderSubmitResponse> {
        self.submit(account, Some(id.to_owned()), order, None).await
    }
    pub async fn replace_order_fenced(
        &self,
        account: &str,
        id: &str,
        order: &OrderRequest,
        fence: &Lease,
    ) -> Result<OrderSubmitResponse> {
        self.submit(account, Some(id.to_owned()), order, Some(fence.clone()))
            .await
    }
    async fn submit(
        &self,
        account: &str,
        replaces: Option<String>,
        request: &OrderRequest,
        fence: Option<Lease>,
    ) -> Result<OrderSubmitResponse> {
        validate_order(request)?;
        let scope = format!(
            "paper-tag:{account}:{}",
            request.tag.as_deref().unwrap_or_default()
        );
        let account = account.to_owned();
        let request = request.clone();
        let this = self.clone();
        let (response, lost) = self
            .journal
            .database()
            .run(move |c| {
                let tx = c
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(sql)?;
                check_admission(&tx, fence.as_ref(), this.is_draining())?;
                let mut clock = effective_clock(get(&tx, "paper_clock")?, &this.config)?;
                let tag = request
                    .tag
                    .as_ref()
                    .ok_or_else(|| invalid("Paper tag required"))?;
                let used: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM used_order_tags WHERE account=?1 AND tag=?2)",
                        params![account, tag],
                        |r| r.get(0),
                    )
                    .map_err(sql)?;
                if used {
                    return Err(Error::Duplicate(
                        "Paper order tag already consumed; inspect order state".into(),
                    ));
                }
                let account_state = ledger(&tx, &account)?;
                let limits = &this
                    .config
                    .accounts
                    .get(&account)
                    .ok_or_else(|| missing("account"))?
                    .limits;
                settlement_at(&this.config, &clock)?;
                let quote = fresh_quote(
                    &tx,
                    &request.order_leg_collection[0].instrument.symbol,
                    &clock,
                    &this.config,
                )?
                .ok_or_else(|| rejected("No fresh quote; session closed or quote stale"))?;
                if clock.at_ms - quote.source_at_ms > limits.max_snapshot_age_ms {
                    return Err(Error::BaselineStale {
                        account: account.clone(),
                        reason: "Paper account quote freshness limit exceeded".into(),
                    });
                }
                // Like a live broker rejection, this leaves the tag free for a
                // retry; only accepted orders consume tags.
                if clock.fault == Some(PaperFault::RejectNext) {
                    clock.fault = None;
                    put(&tx, "paper_clock", &clock)?;
                    audit(
                        &tx,
                        "paper_injected_rejection",
                        tag,
                        &json!({
                        "mode":"paper","account":account,"request":request,"at_ms":clock.at_ms}
                        ),
                    )?;
                    let command = match &replaces {
                        Some(id) => PaperCommand::Replace {
                            account: account.clone(),
                            order_id: id.clone(),
                            order: request.clone(),
                        },
                        None => PaperCommand::Place {
                            account: account.clone(),
                            order: request.clone(),
                        },
                    };
                    capture(&tx, command, PaperCapturedOutcome::InjectedRejection)?;
                    tx.commit().map_err(sql)?;
                    return Ok((None, false));
                }
                if let Some(id) = &replaces {
                    let mut old = find_order(&tx, &account, id)?;
                    if old.status != PaperOrderStatus::Working {
                        return Err(rejected("Only working paper orders may be replaced"));
                    }
                    if old.request.order_leg_collection[0].instrument.symbol
                        != request.order_leg_collection[0].instrument.symbol
                        || old.request.order_leg_collection[0].instruction
                            != request.order_leg_collection[0].instruction
                    {
                        return Err(rejected("Replacement must retain symbol and side"));
                    }
                    end_order(&tx, &mut old, PaperOrderStatus::Replaced, clock.at_ms)?;
                }
                let is_buy = request.order_leg_collection[0].instruction == OrderInstruction::Buy;
                let collar = div(this.config.market_collar_bps, Decimal::from(10000))?;
                let price_bound = if let Some(price) = request.price {
                    price
                } else if is_buy {
                    mul(
                        quote.ask.max(request.stop_price.unwrap_or(quote.ask)),
                        add(Decimal::ONE, collar)?,
                    )?
                } else {
                    mul(
                        quote.bid.min(request.stop_price.unwrap_or(quote.bid)),
                        sub(Decimal::ONE, collar)?,
                    )?
                };
                if !is_buy {
                    // A share sold at the bound must at least pay its fees.
                    let config = &this.config;
                    let kept = mul(
                        price_bound,
                        sub(
                            Decimal::ONE,
                            bps(add(config.fee_bps, config.sell_fee_bps)?)?,
                        )?,
                    )?;
                    if kept < add(config.fee_per_share, config.sell_fee_per_share)? {
                        return Err(rejected("Paper sell price protection cannot cover fees"));
                    }
                }
                let mut order = PaperOrder {
                    order_id: String::new(),
                    account: account.clone(),
                    request,
                    status: PaperOrderStatus::Working,
                    filled_quantity: Decimal::ZERO,
                    remaining_quantity: Decimal::ZERO,
                    accepted_at_ms: clock.at_ms,
                    accepted_sequence: clock.sequence,
                    eligible_at_ms: clock
                        .at_ms
                        .checked_add(this.config.execution_latency_ms)
                        .ok_or_else(|| corrupt("Clock overflow"))?,
                    session_id: clock.session_id.clone(),
                    cancel_at_ms: None,
                    price_bound,
                    reserved_cash: Decimal::ZERO,
                    replaces,
                    triggered: false,
                };
                order.remaining_quantity = order.request.order_leg_collection[0].quantity;
                order.reserved_cash = reserve_cash(&order, &this.config)?;
                let open = open_orders(&tx)?;
                if open.len() >= this.config.max_working_orders {
                    return Err(Error::RiskDenied(
                        "Paper working-order capacity reached".into(),
                    ));
                }
                // The live risk provider's rules (`PortfolioLimits`): the
                // open caps bound what working buys could still add, the
                // position cap one symbol, and a sell only what the account
                // holds less its other working sells of the symbol.
                let mut held_cash = order.reserved_cash;
                let (mut open_q, mut open_n, mut symbol_buys, mut sells) = if is_buy {
                    let n = mul(order.remaining_quantity, price_bound)?;
                    (
                        order.remaining_quantity,
                        n,
                        order.remaining_quantity,
                        Decimal::ZERO,
                    )
                } else {
                    (
                        Decimal::ZERO,
                        Decimal::ZERO,
                        Decimal::ZERO,
                        order.remaining_quantity,
                    )
                };
                for other in open.iter().filter(|o| o.account == account) {
                    held_cash = add(held_cash, other.reserved_cash)?;
                    let same = symbol(other) == symbol(&order);
                    if buy(other) {
                        open_q = add(open_q, other.remaining_quantity)?;
                        open_n = add(open_n, mul(other.remaining_quantity, other.price_bound)?)?;
                        if same {
                            symbol_buys = add(symbol_buys, other.remaining_quantity)?;
                        }
                    } else if same {
                        sells = add(sells, other.remaining_quantity)?;
                    }
                }
                let position = account_state
                    .positions
                    .get(symbol(&order))
                    .map_or(Decimal::ZERO, |p| p.quantity);
                let buy_exceeds = is_buy
                    && (add(position, symbol_buys)? > limits.max_position_quantity
                        || open_q > limits.max_open_quantity
                        || open_n > limits.max_open_notional);
                if held_cash > account_state.cash || sells > position || buy_exceeds {
                    return Err(Error::RiskDenied(
                        "Paper cash, inventory or portfolio limit exceeded".into(),
                    ));
                }
                tx.execute(
                    "INSERT INTO paper_orders(account,active,payload) VALUES(?1,1,'')",
                    [&account],
                )
                .map_err(sql)?;
                order.order_id = tx.last_insert_rowid().to_string();
                let attempt = format!("paper-{}", order.order_id);
                let outcome = SubmissionOutcome::Accepted {
                    order_id: Some(order.order_id.clone()),
                };
                tx.execute(
                    "INSERT INTO used_order_tags VALUES(?1,?2)",
                    params![account, order.request.tag],
                )
                .map_err(sql)?;
                tx.execute(
                    "INSERT INTO submissions(id,account,operation,created_at,request,outcome) \
                     VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        attempt,
                        account,
                        if order.replaces.is_some() {
                            "replace_order"
                        } else {
                            "place_order"
                        },
                        clock.at_ms,
                        encode(&order.request)?,
                        encode(&outcome)?
                    ],
                )
                .map_err(sql)?;
                tx.execute(
                    "INSERT INTO reservations(id,account,quantity,notional,state,symbol,side) \
                     VALUES(?1,?2,?3,?4,'held',?5,?6)",
                    params![
                        attempt,
                        account,
                        order.remaining_quantity.to_string(),
                        order.reserved_cash.to_string(),
                        symbol(&order),
                        if buy(&order) { "BUY" } else { "SELL" }
                    ],
                )
                .map_err(sql)?;
                save_order(&tx, &order)?;
                audit(
                    &tx,
                    "paper_order_accepted",
                    &order.order_id,
                    &serde_json::to_value(&order)?,
                )?;
                let lost = clock.fault.take() == Some(PaperFault::LoseNextAcknowledgement);
                put(&tx, "paper_clock", &clock)?;
                let response = OrderSubmitResponse {
                    order_id: Some(order.order_id.clone()),
                    location: Some(format!("paper://{account}/orders/{}", order.order_id)),
                };
                let command = match &order.replaces {
                    Some(id) => PaperCommand::Replace {
                        account: account.clone(),
                        order_id: id.clone(),
                        order: order.request.clone(),
                    },
                    None => PaperCommand::Place {
                        account: account.clone(),
                        order: order.request.clone(),
                    },
                };
                let outcome = if lost {
                    PaperCapturedOutcome::LostAcknowledgement {
                        order_id: order.order_id.clone(),
                    }
                } else {
                    PaperCapturedOutcome::Success {
                        result: serde_json::to_value(&response)?,
                    }
                };
                capture(&tx, command, outcome)?;
                tx.commit().map_err(sql)?;
                Ok((Some(response), lost))
            })
            .await
            .map_err(|error| mutation_error(error, scope))?;
        let response = response.ok_or_else(|| rejected("Injected paper rejection"))?;
        if lost {
            return Err(Error::SubmissionUnknown {
                attempt_id: format!("paper-{}", response.order_id.as_deref().unwrap_or_default()),
                source: Box::new(Error::ConnectionClosed),
            });
        }
        Ok(response)
    }
    /// Acknowledges the cancel request. With latency, fills remain possible until
    /// the cancel becomes effective on a later event; inspect the final status.
    pub async fn cancel_order(&self, account: &str, id: &str) -> Result<()> {
        let scope = format!("paper-cancel:{account}:{id}");
        let account = account.to_owned();
        let id = id.to_owned();
        let delay = self.config.cancel_latency_ms;
        let config = self.config.clone();
        self.journal
            .database()
            .run(move |c| {
                let tx = c
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(sql)?;
                let clock = effective_clock(get(&tx, "paper_clock")?, &config)?;
                let mut order = find_order(&tx, &account, &id)?;
                if order.status == PaperOrderStatus::Canceled
                    || order.status == PaperOrderStatus::PendingCancel
                {
                    return Ok(());
                }
                if !order.status.is_open() {
                    return Err(rejected("Paper order is already terminal"));
                }
                if delay == 0 {
                    end_order(&tx, &mut order, PaperOrderStatus::Canceled, clock.at_ms)?;
                } else {
                    order.status = PaperOrderStatus::PendingCancel;
                    order.cancel_at_ms = Some(
                        clock
                            .at_ms
                            .checked_add(delay)
                            .ok_or_else(|| corrupt("Clock overflow"))?,
                    );
                    save_order(&tx, &order)?;
                    audit(
                        &tx,
                        "paper_cancel_requested",
                        &id,
                        &json!({
                        "mode":"paper","at_ms":clock.at_ms,"effective_at_ms":order.cancel_at_ms}
                        ),
                    )?;
                }
                capture(
                    &tx,
                    PaperCommand::Cancel {
                        account,
                        order_id: id,
                    },
                    PaperCapturedOutcome::Success {
                        result: json!({"cancel_requested":true}),
                    },
                )?;
                tx.commit().map_err(sql)
            })
            .await
            .map_err(|error| mutation_error(error, scope))
    }
    pub async fn get_order(&self, account: &str, id: &str) -> Result<PaperOrder> {
        let account = account.to_owned();
        let id = id.to_owned();
        self.journal
            .database()
            .run(move |c| find_order(c, &account, &id))
            .await
    }
    /// Every fill recorded for the account's order `id`, in execution order.
    pub async fn fills(&self, account: &str, id: &str) -> Result<Vec<PaperFill>> {
        let account = account.to_owned();
        let id = id.to_owned();
        self.journal
            .database()
            .run(move |c| {
                find_order(c, &account, &id)?;
                let mut statement = c.prepare(SELECT_FILLS).map_err(sql)?;
                statement
                    .query_map([id], |r| r.get::<_, String>(0))
                    .map_err(sql)?
                    .map(|v| Ok(serde_json::from_str(&v.map_err(sql)?)?))
                    .collect()
            })
            .await
    }
    /// The venue's latest quote for `symbol`, or `None` if it has never seen
    /// one. `fresh` tells whether an order would fill against it now.
    pub async fn quote(&self, symbol: &str) -> Result<Option<PaperQuote>> {
        let symbol = symbol.to_owned();
        let config = self.config.clone();
        self.journal
            .database()
            .run(move |c| {
                let Some(quote) = load_quote(c, &symbol)? else {
                    return Ok(None);
                };
                let clock = effective_clock(get(c, "paper_clock")?, &config)?;
                Ok(Some(PaperQuote {
                    fresh: is_fresh(&quote, &clock, &config),
                    symbol,
                    source_at_ms: quote.source_at_ms,
                    bid: quote.bid,
                    ask: quote.ask,
                }))
            })
            .await
    }
    /// Bounded order history, ascending ID. Persist the last ID as the next cursor.
    pub async fn orders(
        &self,
        account: &str,
        after_id: u64,
        limit: u32,
    ) -> Result<Vec<PaperOrder>> {
        if !(1..=1000).contains(&limit) || after_id > i64::MAX as u64 {
            return Err(invalid("Invalid paper history cursor/limit"));
        }
        let account = account.to_owned();
        self.journal
            .database()
            .run(move |c| {
                ledger(c, &account)?;
                let mut statement = c.prepare(SELECT_ORDER_PAGE).map_err(sql)?;
                statement
                    .query_map(params![account, after_id as i64, limit], |r| {
                        r.get::<_, String>(0)
                    })
                    .map_err(sql)?
                    .map(|v| Ok(serde_json::from_str(&v.map_err(sql)?)?))
                    .collect()
            })
            .await
    }
    pub async fn snapshot(&self, account: &str) -> Result<PaperSnapshot> {
        let account = account.to_owned();
        let config = self.config.clone();
        self.journal
            .database()
            .run(move |c| {
                let tx = c.transaction().map_err(sql)?;
                let clock = effective_clock(get(&tx, "paper_clock")?, &config)?;
                let ledger = ledger(&tx, &account)?;
                let working_orders: Vec<_> = open_orders(&tx)?
                    .into_iter()
                    .filter(|o| o.account == account)
                    .collect();
                let reserved_cash = working_orders
                    .iter()
                    .try_fold(Decimal::ZERO, |sum, o| add(sum, o.reserved_cash))?;
                let (market, basis) = mark_positions(&tx, &ledger, &clock, &config)?;
                let equity = equity(&ledger, market)?;
                let snapshot = PaperSnapshot {
                    unsettled_cash: unsettled_cash(&ledger)?,
                    clock: config.clock,
                    mode: "paper".into(),
                    model_version: MODEL_VERSION.into(),
                    account,
                    at_ms: clock.at_ms,
                    event_sequence: clock.sequence,
                    reserved_cash,
                    available_cash: sub(ledger.cash, reserved_cash)?,
                    equity,
                    unrealized_pnl: market.map(|v| sub(v, basis)).transpose()?,
                    daily_loss: session_loss(&ledger, &clock, equity)?,
                    ledger,
                    working_orders,
                };
                tx.commit().map_err(sql)?;
                Ok(snapshot)
            })
            .await
    }
    /// Atomically assign a durable sequence to a live input. Never available in replay mode.
    pub async fn append_live(&self, kind: PaperEventKind) -> Result<PaperEventResult> {
        self.append_live_from(kind, PaperEventSource::Local).await
    }
    /// Attribute live input using trusted producer metadata.
    pub async fn append_live_from(
        &self,
        kind: PaperEventKind,
        source: PaperEventSource,
    ) -> Result<PaperEventResult> {
        validate_source(&source)?;
        if self.config.clock != PaperClockMode::Wall {
            return Err(Error::Config(
                "Live ingestion requires wall clock mode".into(),
            ));
        }
        let config = self.config.clone();
        self.journal
            .database()
            .run(move |c| {
                apply_event(
                    c,
                    &config,
                    PaperEvent {
                        sequence: 0,
                        at_ms: 0,
                        kind,
                    },
                    source,
                )
            })
            .await
    }
    /// The venue's time: the wall clock in wall mode, the replayed clock in
    /// replay mode (a backtest lives in its own past).
    pub async fn venue_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
        let config = self.config.clone();
        let at_ms = self
            .journal
            .database()
            .run(move |c| Ok(effective_clock(get(c, "paper_clock")?, &config)?.at_ms))
            .await?;
        chrono::DateTime::from_timestamp_millis(at_ms)
            .ok_or_else(|| corrupt("Venue clock out of range"))
    }
    pub async fn market_session(&self) -> Result<(String, bool)> {
        self.journal
            .database()
            .run(|c| {
                let clock: Clock = get(c, "paper_clock")?;
                Ok((clock.session_id, clock.open))
            })
            .await
    }
    pub async fn quote_watermark(&self, symbol: &str) -> Result<Option<i64>> {
        let symbol = symbol.to_owned();
        self.journal
            .database()
            .run(move |c| {
                c.query_row(
                    "SELECT source_at FROM paper_quote_watermarks WHERE symbol=?1",
                    [symbol],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)
            })
            .await
    }
    /// Apply exactly one sequenced market event atomically with fills and ledger
    /// changes. An exact event retry returns its original fills without reapplying.
    pub async fn apply(&self, event: PaperEvent) -> Result<PaperEventResult> {
        self.apply_from(event, PaperEventSource::Local).await
    }
    /// Attribution is trusted SDK input. Remote callers must authenticate before
    /// calling this method; exact retries preserve the original event's source.
    pub async fn apply_from(
        &self,
        event: PaperEvent,
        source: PaperEventSource,
    ) -> Result<PaperEventResult> {
        validate_source(&source)?;
        check_sequenced(&event)?;
        let config = self.config.clone();
        self.journal
            .database()
            .run(move |c| apply_event(c, &config, event, source))
            .await
    }
    /// Apply consecutive sequenced events in one transaction: all of them, or
    /// none if any fails. Each event is recorded, audited and captured exactly
    /// as [`PaperBroker::apply`] records it, so the transcript is the same; a
    /// backtest uses this to apply a bar's quotes for one commit instead of
    /// one per quote. At most 10,000 events per batch.
    pub async fn apply_batch(&self, events: Vec<PaperEvent>) -> Result<Vec<PaperEventResult>> {
        if events.len() > MAX_BATCH_EVENTS {
            return Err(invalid("Too many paper events in one batch"));
        }
        events.iter().try_for_each(check_sequenced)?;
        let config = self.config.clone();
        self.journal
            .database()
            .run(move |c| {
                let tx = c
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(sql)?;
                let results = events
                    .into_iter()
                    .map(|event| apply_in(&tx, &config, event, PaperEventSource::Local))
                    .collect::<Result<Vec<_>>>()?;
                tx.commit().map_err(sql)?;
                Ok(results)
            })
            .await
    }
}

const MAX_BATCH_EVENTS: usize = 10_000;

fn check_sequenced(event: &PaperEvent) -> Result<()> {
    if event.sequence == 0
        || event.sequence > i64::MAX as u64
        || !(0..=MAX_AT_MS).contains(&event.at_ms)
    {
        return Err(invalid("Invalid paper event sequence or time"));
    }
    Ok(())
}

fn capture(c: &Connection, command: PaperCommand, outcome: PaperCapturedOutcome) -> Result<()> {
    let count: Option<u64> = meta::get_json(c, "paper_capture_count")?;
    // Historical databases remain usable, but cannot claim complete capture.
    let Some(count) = count else {
        let expected: bool = c
            .query_row(SELECT_CAPTURE_EXPECTED, [], |row| row.get(0))
            .map_err(sql)?;
        if expected {
            return Err(Error::Storage("Command capture metadata is missing".into()));
        }
        return Ok(());
    };
    let index = count
        .checked_add(1)
        .ok_or_else(|| corrupt("Capture sequence exhausted"))?;
    let record = PaperCapturedCommand {
        index,
        command,
        outcome,
    };
    audit(
        c,
        "paper_command",
        &index.to_string(),
        &serde_json::to_value(record)?,
    )?;
    put(c, "paper_capture_count", &index)
}

fn validate_source(source: &PaperEventSource) -> Result<()> {
    if let PaperEventSource::Authority { subject } = source
        && (subject.trim().is_empty()
            || subject.len() > 1024
            || subject.chars().any(char::is_control))
    {
        return Err(invalid("Invalid paper event subject"));
    }
    Ok(())
}

fn apply_event(
    c: &mut Connection,
    config: &PaperConfig,
    event: PaperEvent,
    source: PaperEventSource,
) -> Result<PaperEventResult> {
    let tx = c
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    let result = apply_in(&tx, config, event, source)?;
    tx.commit().map_err(sql)?;
    Ok(result)
}

/// Apply one event inside the caller's write transaction.
fn apply_in(
    tx: &Connection,
    config: &PaperConfig,
    event: PaperEvent,
    source: PaperEventSource,
) -> Result<PaperEventResult> {
    let mut event = event;
    if event.sequence == 0 {
        let clock: Clock = get(tx, "paper_clock")?;
        event.sequence = clock
            .sequence
            .checked_add(1)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or_else(|| corrupt("Event sequence exhausted"))?;
        event.at_ms = chrono::Utc::now().timestamp_millis();
    }
    let payload = encode(&canonical_json(&serde_json::to_value(&event)?))?;
    let prior: Option<(String, String)> = tx
        .query_row(
            "SELECT payload,result FROM paper_events WHERE sequence=?1",
            [event.sequence as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(sql)?;
    if let Some((old, result)) = prior {
        if old != payload && old != payload_receipt(&payload) {
            return Err(Error::Duplicate(
                "Event sequence already recorded with different content".into(),
            ));
        }
        let mut result: PaperEventResult = serde_json::from_str(&result)?;
        result.duplicate = true;
        return Ok(result);
    }
    if config.clock == PaperClockMode::Wall {
        let now = chrono::Utc::now().timestamp_millis();
        if event.at_ms > now || now - event.at_ms > config.quote_max_age_ms {
            return Err(invalid("Live paper event is stale or future dated"));
        }
    }
    let mut clock: Clock = get(tx, "paper_clock")?;
    if event.sequence != clock.sequence + 1 || event.at_ms < clock.at_ms {
        return Err(invalid("Paper events must be contiguous and time ordered"));
    }
    clock.sequence = event.sequence;
    clock.at_ms = event.at_ms;
    let mut quotes: Option<(String, Quote, Decimal, Decimal)> = None;
    match &event.kind {
        PaperEventKind::Session { session_id, open } => {
            if session_id.is_empty() || session_id.len() > 64 {
                return Err(invalid("Session ID required"));
            }
            // A session transition invalidates all prior liquidity/baselines.
            clock.session_id = session_id.clone();
            clock.open = *open;
            tx.execute("DELETE FROM paper_quotes", []).map_err(sql)?;
        }
        PaperEventKind::Disconnect => {
            tx.execute("DELETE FROM paper_quotes", []).map_err(sql)?;
        }
        PaperEventKind::Fault { fault } => {
            if clock.fault.is_some() {
                return Err(invalid("A paper fault is already armed"));
            }
            clock.fault = Some(*fault);
        }
        PaperEventKind::Clock => {}
        PaperEventKind::Dividend { .. } | PaperEventKind::Split { .. } => {
            corporate_action(tx, config, &clock, &event.kind)?;
        }
        PaperEventKind::Quote {
            symbol,
            source_at_ms,
            bid,
            ask,
            bid_size,
            ask_size,
        } => {
            if *source_at_ms < 0
                || *source_at_ms > clock.at_ms
                || (config.clock == PaperClockMode::Wall
                    && chrono::Utc::now()
                        .timestamp_millis()
                        .checked_sub(*source_at_ms)
                        .is_none_or(|age| age < 0 || age > config.quote_max_age_ms))
                || !clock.open
                || !symbol_valid(symbol)
                || clock.at_ms - source_at_ms > config.quote_max_age_ms
                || *bid < Decimal::ONE
                || !positive_bounded(*bid)
                || !positive_bounded(*ask)
                || bid > ask
                || *bid_size < Decimal::ZERO
                || *ask_size < Decimal::ZERO
                || *bid_size > Decimal::from(MAX_QUOTE_SIZE)
                || *ask_size > Decimal::from(MAX_QUOTE_SIZE)
                || !bid_size.fract().is_zero()
                || !ask_size.fract().is_zero()
            {
                return Err(invalid("Invalid, stale or crossed paper quote"));
            }
            let previous: Option<i64> = tx
                .query_row(
                    "SELECT source_at FROM paper_quote_watermarks WHERE symbol=?1",
                    [symbol],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?;
            if previous.is_some_and(|previous| *source_at_ms <= previous) {
                return Err(invalid(
                    "Quote source time must advance; do not reuse snapshot liquidity",
                ));
            }
            tx.execute(UPSERT_QUOTE_WATERMARK, params![symbol, source_at_ms])
                .map_err(sql)?;
            let quote = Quote {
                source_at_ms: *source_at_ms,
                bid: *bid,
                ask: *ask,
            };
            tx.execute(UPSERT_QUOTE, params![symbol, encode(&quote)?])
                .map_err(sql)?;
            quotes = Some((symbol.clone(), quote, *bid_size, *ask_size));
        }
    }
    settle_cash(tx, config, clock.at_ms)?;
    let mut orders = open_orders(tx)?;
    // A cancel effective exactly at this event wins over a fill at this event.
    for order in &mut orders {
        if order.request.duration == Some(OrderDuration::Day)
            && (!clock.open || order.session_id != clock.session_id)
        {
            end_order(tx, order, PaperOrderStatus::Expired, clock.at_ms)?;
        } else if order.cancel_at_ms.is_some_and(|at| at <= clock.at_ms) {
            end_order(tx, order, PaperOrderStatus::Canceled, clock.at_ms)?;
        }
    }
    let mut fills = Vec::new();
    if let Some((ticker, quote, mut bid_left, mut ask_left)) = quotes {
        // Stable acceptance-ID priority, sharing one liquidity budget across accounts.
        for order in &mut orders {
            if !order.status.is_open()
                || symbol(order) != ticker
                || clock.sequence <= order.accepted_sequence
                || clock.at_ms < order.eligible_at_ms
                || quote.source_at_ms <= order.accepted_at_ms
            {
                continue;
            }
            let is_buy = buy(order);
            if let Some(stop) = order.request.stop_price
                && !order.triggered
            {
                if (is_buy && quote.ask < stop) || (!is_buy && quote.bid > stop) {
                    continue;
                }
                order.triggered = true;
                save_order(tx, order)?;
                audit(
                    tx,
                    "paper_stop_triggered",
                    &order.order_id,
                    &json!({"at_ms":clock.at_ms,"sequence":clock.sequence}),
                )?;
                // Triggering is not filling; execution starts on a later event.
                order.eligible_at_ms = clock
                    .at_ms
                    .checked_add(config.execution_latency_ms)
                    .ok_or_else(|| corrupt("Clock overflow"))?;
                save_order(tx, order)?;
                continue;
            }
            let slip = div(config.slippage_bps, Decimal::from(10000))?;
            let price = if is_buy {
                mul(quote.ask, add(Decimal::ONE, slip)?)?
            } else {
                mul(quote.bid, sub(Decimal::ONE, slip)?)?
            };
            let crosses = if is_buy {
                price <= order.price_bound
            } else {
                price >= order.price_bound
            };
            if !crosses {
                if matches!(
                    order.request.order_type,
                    Some(OrderType::Market | OrderType::Stop)
                ) {
                    end_order(tx, order, PaperOrderStatus::Rejected, clock.at_ms)?;
                }
                continue;
            }
            let liquidity = if is_buy { &mut ask_left } else { &mut bid_left };
            let quantity = order.remaining_quantity.min(*liquidity);
            if quantity.is_zero() {
                continue;
            }
            let notional = mul(quantity, price)?;
            let fee = fill_fee(config, is_buy, quantity, notional)?;
            let mut account = ledger(tx, &order.account)?;
            let position = account.positions.entry(ticker.clone()).or_default();
            if is_buy {
                let cost = add(notional, fee)?;
                if cost > account.cash {
                    return Err(Error::Storage("Paper cash invariant violated".into()));
                }
                account.cash = sub(account.cash, cost)?;
                position.quantity = add(position.quantity, quantity)?;
                position.cost_basis = add(position.cost_basis, cost)?;
            } else {
                if quantity > position.quantity {
                    return Err(Error::Storage("Paper inventory invariant violated".into()));
                }
                let basis = if quantity == position.quantity {
                    position.cost_basis
                } else {
                    div(mul(position.cost_basis, quantity)?, position.quantity)?
                };
                let proceeds = sub(notional, fee)?;
                account.realized_pnl = add(account.realized_pnl, sub(proceeds, basis)?)?;
                position.quantity = sub(position.quantity, quantity)?;
                position.cost_basis = sub(position.cost_basis, basis)?;
                credit_cash(
                    &mut account,
                    proceeds,
                    settlement_at(config, &clock)?,
                    clock.at_ms,
                    format!("fill:{}:{}", clock.sequence, order.order_id),
                )?;
            }
            account.fees = add(account.fees, fee)?;
            save_ledger(tx, &order.account, &account)?;
            order.filled_quantity = add(order.filled_quantity, quantity)?;
            order.remaining_quantity = sub(order.remaining_quantity, quantity)?;
            if order.remaining_quantity.is_zero() {
                order.status = PaperOrderStatus::Filled;
                order.cancel_at_ms = None;
            }
            order.reserved_cash = reserve_cash(order, config)?;
            save_order(tx, order)?;
            *liquidity = sub(*liquidity, quantity)?;
            let fill = PaperFill {
                order_id: order.order_id.clone(),
                event_sequence: clock.sequence,
                at_ms: clock.at_ms,
                quantity,
                price,
                fee,
            };
            audit(
                tx,
                "paper_fill",
                &order.order_id,
                &serde_json::to_value(&fill)?,
            )?;
            fills.push(fill);
        }
    }
    enforce_daily_loss(tx, config, &clock)?;
    put(tx, "paper_clock", &clock)?;
    let result = PaperEventResult {
        mode: "paper".into(),
        duplicate: false,
        sequence: clock.sequence,
        fills,
    };
    tx.execute(
        "INSERT INTO paper_events VALUES(?1,?2,?3)",
        params![event.sequence as i64, payload, encode(&result)?],
    )
    .map_err(sql)?;
    audit(
        tx,
        "paper_event",
        &event.sequence.to_string(),
        &json!({
        "mode":"paper","event":event,"provenance":source}
        ),
    )?;
    capture(
        tx,
        PaperCommand::Event { event },
        PaperCapturedOutcome::Success {
            result: serde_json::to_value(&result)?,
        },
    )?;
    Ok(result)
}

// Reopening validates existing state; only first creation may initialize accounts
// or history. Reconstructing missing tables would erase deduplication evidence.
fn initialize(c: &mut Connection, config: &PaperConfig) -> Result<()> {
    let tx = c
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    let mode = meta::get(&tx, "execution_mode")?;
    if mode.as_deref().is_some_and(|m| m != "paper") {
        return Err(Error::Config(
            "Paper mode cannot use live execution state".into(),
        ));
    }
    let prior: Option<serde_json::Value> = meta::get_json(&tx, "paper_config")?;
    if let Some(prior) = prior {
        if canonical_json(&prior) != canonical_json(&serde_json::to_value(config)?) {
            return Err(Error::Config(
                "Paper configuration differs from durable run".into(),
            ));
        }
        let tables: i64 = tx
            .query_row(COUNT_PAPER_TABLES, [], |r| r.get(0))
            .map_err(sql)?;
        if tables != 5 {
            return Err(Error::Storage(
                "Paper database is missing required tables; restore a complete backup".into(),
            ));
        }
        let capture_metadata: i64 = tx
            .query_row(COUNT_CAPTURE_METADATA, [], |row| row.get(0))
            .map_err(sql)?;
        let captured: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM audit WHERE kind='paper_command'",
                [],
                |row| row.get(0),
            )
            .map_err(sql)?;
        if capture_metadata == 2 {
            let version: u32 = get(&tx, "paper_capture_version")?;
            let expected: u64 = get(&tx, "paper_capture_count")?;
            if version != 1 || u64::try_from(captured).ok() != Some(expected) {
                return Err(Error::Storage(
                    "Command capture history is incomplete or unsupported".into(),
                ));
            }
        } else if capture_metadata != 0 || captured != 0 {
            return Err(Error::Storage(
                "Command capture metadata is incomplete".into(),
            ));
        }
        let clock: Clock = get(&tx, "paper_clock")?;
        let (count, latest): (i64, i64) = tx
            .query_row(
                "SELECT COUNT(*), COALESCE(MAX(sequence),0) FROM paper_events",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(sql)?;
        if u64::try_from(count).ok() != Some(clock.sequence)
            || u64::try_from(latest).ok() != Some(clock.sequence)
        {
            return Err(Error::Storage(
                "Paper event history does not match its cursor".into(),
            ));
        }
        let count: i64 = tx
            .query_row("SELECT COUNT(*) FROM paper_accounts", [], |r| r.get(0))
            .map_err(sql)?;
        if usize::try_from(count).ok() != Some(config.accounts.len()) {
            return Err(Error::Storage("Paper account state is incomplete".into()));
        }
        for name in config.accounts.keys() {
            configured_ledger(&tx, name)?;
        }
    } else {
        let paper_tables: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name GLOB 'paper_*'",
                [],
                |r| r.get(0),
            )
            .map_err(sql)?;
        let count: i64 = tx
            .query_row(COUNT_LIVE_STATE_ROWS, [], |r| r.get(0))
            .map_err(sql)?;
        if mode.is_some() || paper_tables != 0 || count != 0 {
            return Err(Error::Config(
                "Paper mode requires a new dedicated database".into(),
            ));
        }
        tx.execute_batch(PAPER_SCHEMA).map_err(sql)?;
        for (name, account) in &config.accounts {
            let state = PaperAccount {
                cash: account.initial_cash,
                realized_pnl: Decimal::ZERO,
                fees: Decimal::ZERO,
                positions: BTreeMap::new(),
                pending_cash: Vec::new(),
                session_mark: None,
                last_mark: None,
            };
            tx.execute(
                "INSERT INTO paper_accounts VALUES(?1,?2)",
                params![name, encode(&state)?],
            )
            .map_err(sql)?;
        }
        meta::put(&tx, "execution_mode", "paper")?;
        put(&tx, "paper_config", config)?;
        put(&tx, "paper_clock", &Clock::default())?;
        put(&tx, "paper_capture_version", &1_u32)?;
        put(&tx, "paper_capture_count", &0_u64)?;
    }
    tx.execute_batch(INDEX_FILLS).map_err(sql)?;
    tx.commit().map_err(sql)
}

fn settlement_at(config: &PaperConfig, clock: &Clock) -> Result<i64> {
    match config.settlement_calendar.get(&clock.session_id) {
        Some(at) if *at >= clock.at_ms => Ok(*at),
        Some(_) => Err(Error::Config(
            "Settlement timestamp precedes trade time".into(),
        )),
        None if config.settlement_required => Err(Error::Config(
            "Trading session has no settlement calendar entry".into(),
        )),
        None => Ok(clock.at_ms),
    }
}
fn credit_cash(
    account: &mut PaperAccount,
    amount: Decimal,
    available_at_ms: i64,
    now: i64,
    source: String,
) -> Result<()> {
    if amount < Decimal::ZERO {
        return Err(corrupt("Negative cash credit"));
    }
    if amount.is_zero() {
        return Ok(());
    }
    if available_at_ms <= now {
        account.cash = add(account.cash, amount)?;
    } else {
        if account.pending_cash.len() >= 10000 {
            return Err(corrupt("Pending cash capacity reached"));
        }
        account.pending_cash.push(PendingCash {
            source,
            amount,
            available_at_ms,
        });
    }
    Ok(())
}
fn settle_cash(c: &Connection, config: &PaperConfig, at: i64) -> Result<()> {
    for name in config.accounts.keys() {
        let mut account = configured_ledger(c, name)?;
        let due: Vec<_> = account
            .pending_cash
            .iter()
            .filter(|p| p.available_at_ms <= at)
            .cloned()
            .collect();
        if due.is_empty() {
            continue;
        }
        for pending in &due {
            account.cash = add(account.cash, pending.amount)?;
            audit(
                c,
                "paper_cash_settled",
                name,
                &json!({"at_ms":at,"cash":pending}),
            )?;
        }
        account.pending_cash.retain(|p| p.available_at_ms > at);
        save_ledger(c, name, &account)?;
    }
    Ok(())
}
fn corporate_action(
    c: &Connection,
    config: &PaperConfig,
    clock: &Clock,
    event: &PaperEventKind,
) -> Result<()> {
    let (id, symbol, pay_at) = match event {
        PaperEventKind::Dividend {
            action_id,
            symbol,
            pay_at_ms,
            ..
        }
        | PaperEventKind::Split {
            action_id,
            symbol,
            pay_at_ms,
            ..
        } => (action_id, symbol, *pay_at_ms),
        _ => return Err(invalid("Expected corporate action")),
    };
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        || !symbol_valid(symbol)
        || pay_at < clock.at_ms
        || pay_at > MAX_AT_MS
    {
        return Err(invalid("Invalid corporate action identity/time"));
    }
    let key = format!("paper_action:{id}");
    if meta::exists(c, &key)? {
        return Err(Error::Duplicate(
            "Corporate action already applied; retry the original event sequence".into(),
        ));
    }
    match event {
        PaperEventKind::Dividend { cash_per_share, .. } if !positive_bounded(*cash_per_share) => {
            return Err(invalid("Invalid dividend amount"));
        }
        PaperEventKind::Split {
            numerator,
            denominator,
            cash_in_lieu_price,
            ..
        } if *numerator == 0
            || *denominator == 0
            || *numerator > 10000
            || *denominator > 10000
            || !positive_bounded(*cash_in_lieu_price) =>
        {
            return Err(invalid("Invalid split terms"));
        }
        _ => {}
    }
    if matches!(event, PaperEventKind::Split { .. }) {
        for mut order in open_orders(c)?
            .into_iter()
            .filter(|o| self::symbol(o) == symbol)
        {
            end_order(c, &mut order, PaperOrderStatus::Canceled, clock.at_ms)?;
        }
    }
    for name in config.accounts.keys() {
        let mut account = configured_ledger(c, name)?;
        let Some(position) = account.positions.get_mut(symbol) else {
            continue;
        };
        if position.quantity.is_zero() {
            continue;
        }
        let amount = match event {
            PaperEventKind::Dividend { cash_per_share, .. } => {
                let cash = mul(position.quantity, *cash_per_share)?;
                account.realized_pnl = add(account.realized_pnl, cash)?;
                cash
            }
            PaperEventKind::Split {
                numerator,
                denominator,
                cash_in_lieu_price,
                ..
            } => {
                let exact = div(
                    mul(position.quantity, Decimal::from(*numerator))?,
                    Decimal::from(*denominator),
                )?;
                let fractional = exact.fract();
                let removed_basis = if exact < Decimal::ONE {
                    position.cost_basis
                } else {
                    div(mul(position.cost_basis, fractional)?, exact)?
                };
                let cash = mul(fractional, *cash_in_lieu_price)?;
                position.quantity = exact.trunc();
                position.cost_basis = sub(position.cost_basis, removed_basis)?;
                account.realized_pnl = add(account.realized_pnl, sub(cash, removed_basis)?)?;
                cash
            }
            _ => unreachable!(),
        };
        credit_cash(&mut account, amount, pay_at, clock.at_ms, id.clone())?;
        save_ledger(c, name, &account)?;
    }
    c.execute("DELETE FROM paper_quotes WHERE symbol=?1", [symbol])
        .map_err(sql)?;
    put(c, &key, &clock.sequence)?;
    audit(
        c,
        "paper_corporate_action",
        id,
        &serde_json::to_value(event)?,
    )
}

impl PaperBroker {
    /// Verify ledger, order and reservation identities in a consistent snapshot.
    /// This detects inconsistencies, not coordinated hostile rewrites.
    pub async fn verify_integrity(&self) -> Result<PaperIntegrityReport> {
        let config = self.config.clone();
        self.journal
            .database()
            .run(move |connection| {
                let tx = connection.transaction().map_err(sql)?;
                let sqlite: String = tx
                    .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                    .map_err(sql)?;
                if sqlite != "ok" {
                    return Err(Error::Storage("SQLite integrity check failed".into()));
                }
                let clock: Clock = get(&tx, "paper_clock")?;
                let (events, latest): (i64, i64) = tx
                    .query_row(
                        "SELECT COUNT(*), COALESCE(MAX(sequence), 0) FROM paper_events",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(sql)?;
                if u64::try_from(events).ok() != Some(clock.sequence)
                    || u64::try_from(latest).ok() != Some(clock.sequence)
                {
                    return Err(Error::Storage("Event continuity invariant failed".into()));
                }
                let open = open_orders(&tx)?;
                for (name, settings) in &config.accounts {
                    let account = configured_ledger(&tx, name)?;
                    let basis =
                        account
                            .positions
                            .values()
                            .try_fold(Decimal::ZERO, |sum, position| {
                                if position.quantity < Decimal::ZERO
                                    || !position.quantity.fract().is_zero()
                                    || position.cost_basis < Decimal::ZERO
                                    || (position.quantity.is_zero()
                                        && !position.cost_basis.is_zero())
                                {
                                    return Err(Error::Storage("Position invariant failed".into()));
                                }
                                add(sum, position.cost_basis)
                            })?;
                    let pending =
                        account
                            .pending_cash
                            .iter()
                            .try_fold(Decimal::ZERO, |sum, credit| {
                                if credit.amount < Decimal::ZERO {
                                    return Err(Error::Storage(
                                        "Cash credit invariant failed".into(),
                                    ));
                                }
                                add(sum, credit.amount)
                            })?;
                    if account.cash < Decimal::ZERO
                        || account.fees < Decimal::ZERO
                        || add(add(account.cash, pending)?, basis)?
                            != add(settings.initial_cash, account.realized_pnl)?
                    {
                        return Err(Error::Storage(
                            "Ledger conservation invariant failed".into(),
                        ));
                    }
                    let mut held = Decimal::ZERO;
                    let mut sells: BTreeMap<String, Decimal> = BTreeMap::new();
                    for order in open.iter().filter(|order| &order.account == name) {
                        held = add(held, order.reserved_cash)?;
                        if !buy(order) {
                            let value = sells.entry(symbol(order).into()).or_default();
                            *value = add(*value, order.remaining_quantity)?;
                        }
                    }
                    if held > account.cash
                        || sells.iter().any(|(symbol, quantity)| {
                            *quantity
                                > account
                                    .positions
                                    .get(symbol)
                                    .map_or(Decimal::ZERO, |position| position.quantity)
                        })
                    {
                        return Err(Error::Storage(
                            "Cash/inventory reservation invariant failed".into(),
                        ));
                    }
                }
                let mut count = 0;
                let mut statement = tx
                    .prepare("SELECT payload FROM paper_orders ORDER BY id")
                    .map_err(sql)?;
                let rows = statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(sql)?;
                for row in rows {
                    let order: PaperOrder = serde_json::from_str(&row.map_err(sql)?)?;
                    validate_order(&order.request)?;
                    if order.filled_quantity < Decimal::ZERO
                        || order.remaining_quantity < Decimal::ZERO
                        || add(order.filled_quantity, order.remaining_quantity)?
                            != order.request.order_leg_collection[0].quantity
                        || order.reserved_cash != reserve_cash(&order, &config)?
                    {
                        return Err(Error::Storage(
                            "Order quantity/reservation invariant failed".into(),
                        ));
                    }
                    let tagged: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM used_order_tags WHERE account=?1 AND tag=?2)",
                    params![order.account, order.request.tag], |row| row.get(0),
                ).map_err(sql)?;
                    let (quantity, notional, state): (String, String, String) = tx
                        .query_row(
                            "SELECT quantity, notional, state FROM reservations WHERE id=?1",
                            [format!("paper-{}", order.order_id)],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        )
                        .map_err(sql)?;
                    if !tagged
                        || quantity.parse::<Decimal>().ok() != Some(order.remaining_quantity)
                        || notional.parse::<Decimal>().ok() != Some(order.reserved_cash)
                        || (state == "held") != order.status.is_open()
                    {
                        return Err(Error::Storage(
                            "Durable order reservation/tag mismatch".into(),
                        ));
                    }
                    count += 1;
                }
                Ok(PaperIntegrityReport {
                    model_version: MODEL_VERSION.into(),
                    accounts: config.accounts.len(),
                    orders: count,
                    event_sequence: clock.sequence,
                })
            })
            .await
    }
}

fn payload_receipt(payload: &str) -> String {
    let hash = ring::digest::digest(&ring::digest::SHA256, payload.as_bytes());
    let hex = hash
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{hex}")
}

impl PaperBroker {
    /// Back up state before replacing event payloads with SHA-256 receipts.
    /// Results and audit history remain available for exact retry semantics.
    /// This does not delete receipts or provide immutable external retention.
    pub async fn archive_events(
        &self,
        destination: PathBuf,
        through_sequence: u64,
    ) -> Result<PaperArchiveReport> {
        if through_sequence == 0 || through_sequence > i64::MAX as u64 {
            return Err(invalid("Invalid archive sequence"));
        }
        self.journal.database().backup(destination.clone()).await?;
        self.journal
            .database()
            .run(move |connection| {
                use std::io::Read;
                let archive = rusqlite::Connection::open_with_flags(
                    &destination,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
                .map_err(sql)?;
                let saved: Clock = get(&archive, "paper_clock")?;
                if through_sequence > saved.sequence {
                    return Err(invalid("Archive does not cover requested frontier"));
                }
                let mut file = std::fs::File::open(&destination)?;
                let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
                let mut buffer = [0_u8; 65536];
                loop {
                    let bytes = file.read(&mut buffer)?;
                    if bytes == 0 {
                        break;
                    }
                    digest.update(&buffer[..bytes]);
                }
                let hash = digest
                    .finish()
                    .as_ref()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                let tx = connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(sql)?;
                let mut cursor = 0_i64;
                let mut compacted = 0;
                loop {
                    let batch = {
                        let mut statement = tx.prepare(SELECT_EVENT_PAGE).map_err(sql)?;
                        statement
                            .query_map(params![cursor, through_sequence as i64], |row| {
                                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                            })
                            .map_err(sql)?
                            .collect::<std::result::Result<Vec<_>, _>>()
                            .map_err(sql)?
                    };
                    if batch.is_empty() {
                        break;
                    }
                    for (sequence, payload) in batch {
                        cursor = sequence;
                        if payload.starts_with("sha256:") {
                            continue;
                        }
                        let saved: String = archive
                            .query_row(SELECT_EVENT_PAYLOAD, [sequence], |row| row.get(0))
                            .map_err(sql)?;
                        if payload != saved {
                            return Err(Error::Storage("Archived event payload mismatch".into()));
                        }
                        tx.execute(
                            REPLACE_EVENT_PAYLOAD,
                            params![sequence, payload_receipt(&payload)],
                        )
                        .map_err(sql)?;
                        compacted += 1;
                    }
                }
                let report = PaperArchiveReport {
                    through_sequence,
                    compacted,
                    archive_path: destination,
                    sha256: hash,
                };
                put(&tx, &format!("paper_archive:{through_sequence}"), &report)?;
                audit(
                    &tx,
                    "paper_events_archived",
                    &through_sequence.to_string(),
                    &serde_json::to_value(&report)?,
                )?;
                tx.commit().map_err(sql)?;
                Ok(report)
            })
            .await
    }
}

impl PaperBroker {
    /// Number of atomically captured mutations. Missing on historical runs that
    /// predate capture; never infer complete history from partial audit records.
    pub async fn captured_command_count(&self) -> Result<u64> {
        self.journal
            .database()
            .run(|connection| {
                meta::get_json(connection, "paper_capture_count")?
                    .ok_or_else(|| Error::Storage("Run lacks complete command capture".into()))
            })
            .await
    }
}
