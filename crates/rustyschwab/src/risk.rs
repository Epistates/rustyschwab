//! Pluggable pre-trade risk with durable reservations for outstanding orders.
use crate::{
    SchwabClient,
    coordination::{Lease, lease_held},
    error::{Error, Result},
    execution::{ensure_not_halted, is_broker_order_id, trip_halt},
    storage::{StateDatabase, sql},
    types::{
        common::{AssetMainType, AssetSubType, AssetType},
        market_data::QuotesResponse,
        trading::{OrderInstruction, OrderRequest, OrderStrategyType, OrderType},
    },
};
use async_trait::async_trait;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::HashMap;

const SELECT_ACCOUNT_BASELINE: &str =
    "SELECT observed_at,valid,snapshot FROM accounts WHERE account=?1";
const SELECT_HELD_RESERVATION: &str = "SELECT quantity,notional,replaces FROM reservations \
     WHERE id=?1 AND account=?2 AND state='held'";
/// Every held reservation for the account except (when revalidating) our
/// own and the one for the order `?4` replaces, with what it would buy or
/// sell. Rows from before reservations kept their scope have no symbol or
/// side and count against every check.
const SELECT_OTHER_HELD_RESERVATIONS: &str = "SELECT r.quantity,r.notional,r.symbol,r.side \
     FROM reservations r WHERE r.account=?1 AND r.state='held' AND (NOT ?2 OR r.id<>?3) \
       AND NOT (?4 IS NOT NULL AND EXISTS(SELECT 1 FROM submissions s WHERE s.id=r.id \
         AND s.account=r.account AND json_extract(s.outcome,'$.order_id')=?4))";
/// Working broker orders for the account, except the one `?2` replaces,
/// that no held local reservation is linked to. Terminal orders are skipped
/// in SQL through the status index, so the check grows with working orders,
/// not with history.
const SELECT_UNRESERVED_BROKER_ORDERS: &str = "SELECT o.payload FROM orders o \
     WHERE o.account=?1 AND (?2 IS NULL OR o.order_id<>?2) \
       AND (json_extract(o.payload,'$.status') IS NULL \
         OR json_extract(o.payload,'$.status') \
           NOT IN ('FILLED','CANCELED','REJECTED','EXPIRED','REPLACED')) \
       AND NOT EXISTS(SELECT 1 FROM reservations r JOIN submissions s ON s.id=r.id \
         WHERE r.state='held' AND s.account=o.account \
           AND json_extract(s.outcome,'$.order_id')=o.order_id)";
const INSERT_HELD_RESERVATION: &str = "INSERT INTO \
     reservations(id,account,quantity,notional,state,symbol,side,replaces) \
     VALUES(?1,?2,?3,?4,'held',?5,?6,?7)";
const SELECT_INSTRUMENT_TYPE: &str = "SELECT asset_main_type,asset_sub_type,observed_at \
     FROM instrument_types WHERE symbol=?1";
const UPSERT_INSTRUMENT_TYPE: &str = "INSERT INTO \
     instrument_types(symbol,asset_main_type,asset_sub_type,observed_at) VALUES(?1,?2,?3,?4) \
     ON CONFLICT(symbol) DO UPDATE SET asset_main_type=excluded.asset_main_type, \
       asset_sub_type=excluded.asset_sub_type, observed_at=excluded.observed_at";
/// How long the built-in provider trusts a symbol's instrument type as it
/// read it from Schwab: an hour. Symbols are reused and reclassified rarely,
/// and at no hour Schwab documents, so a reading is kept for an hour from
/// when it was taken rather than for a trading day: a change reaches orders
/// within the hour, and a symbol bought all day costs one read an hour.
const INSTRUMENT_TYPE_LIFETIME_MS: i64 = 60 * 60 * 1000;
/// Why a buy of an ETF is refused, by its order's label or by Schwab's word.
const ETFS: &str = "Built-in portfolio risk covers long equity only; ETFs need a \
     deployment's own risk provider";
/// Why a buy of what Schwab calls neither common stock nor a depositary
/// receipt, nor names a kind of, is refused.
const UNCLASSIFIED: &str = "Built-in portfolio risk covers common stock and depositary \
     receipts only; a symbol Schwab classifies as neither needs a deployment's own risk \
     provider";
/// Why a buy reserved or checked without being screened is refused.
const UNSCREENED: &str = "Built-in portfolio risk has no current instrument type from \
     Schwab for this symbol; screen the order first";
/// Audit subject of a halt the daily-loss control trips, live or paper.
pub const DAILY_LOSS_CONTROL: &str = "daily-loss-control";

#[async_trait]
pub trait RiskProvider: Send + Sync {
    /// Screen `order` before anything is journaled for it: read what judging
    /// it needs from the market through `market`, the client's own reads
    /// (its auth, request budget, timeouts and circuit breaker), and refuse
    /// it now if what was read settles it. The client calls it, outside
    /// every transaction, ahead of [`reserve`](Self::reserve),
    /// [`reserve_replacement`](Self::reserve_replacement) and
    /// [`check`](Self::check); those still judge the order in full, since a
    /// dispatcher may call them alone. The default reads nothing.
    async fn screen(
        &self,
        _account: &str,
        _order: &OrderRequest,
        _market: &SchwabClient,
    ) -> Result<()> {
        Ok(())
    }
    /// Atomically reserve exposure. Implementations must retain it on unknown outcomes.
    async fn reserve(&self, account: &str, attempt_id: &str, order: &OrderRequest) -> Result<()>;
    /// Reserve exposure for `order` replacing broker order `replaces`.
    /// Schwab cancels the old order and creates the new one atomically, so
    /// a provider that can tell the two apart should not count both; the
    /// default counts both, which only over-reserves.
    async fn reserve_replacement(
        &self,
        account: &str,
        attempt_id: &str,
        order: &OrderRequest,
        replaces: &str,
    ) -> Result<()> {
        let _ = replaces;
        self.reserve(account, attempt_id, order).await
    }
    /// The account's baseline was just reconciled. Providers with a
    /// daily-loss control judge it here, so a breach halts submissions before
    /// the next order rather than at it; the default observes nothing.
    async fn observe_baseline(&self, _account: &str) -> Result<()> {
        Ok(())
    }
    /// Judge `order` (replacing broker order `replaces`, if any) as
    /// [`reserve`](Self::reserve) would, without reserving anything or
    /// tripping any control: an advisory pre-flight. The default finds
    /// nothing to object to.
    async fn check(
        &self,
        _account: &str,
        _order: &OrderRequest,
        _replaces: Option<&str>,
    ) -> Result<()> {
        Ok(())
    }
    /// Whether reconciliation should record the account's net cash
    /// transfers for the day (deposits less withdrawals) with its baseline,
    /// for a daily-loss control that must not read a deposit as a gain or a
    /// withdrawal as a loss. The default asks for none.
    fn needs_day_transfers(&self, _account: &str) -> bool {
        false
    }
    /// Recheck the held reservation and current risk baseline after all admission
    /// waits, immediately before transmission. Fail closed if unsupported.
    async fn revalidate(
        &self,
        _account: &str,
        _attempt_id: &str,
        _order: &OrderRequest,
    ) -> Result<()> {
        Err(Error::Config(
            "Risk provider must implement dispatch revalidation".into(),
        ))
    }
    /// Atomically revalidate risk, operator halt and a gateway dispatch fence.
    /// Implementations unable to check the same authority must fail closed.
    async fn revalidate_fenced(
        &self,
        _account: &str,
        _attempt_id: &str,
        _order: &OrderRequest,
        _lease: &Lease,
    ) -> Result<()> {
        Err(Error::Config(
            "Risk provider must implement atomic fenced revalidation".into(),
        ))
    }
    /// Release only after a definite rejection or authoritative terminal broker state.
    async fn release(&self, attempt_id: &str) -> Result<()>;
}
/// Per-account limits for [`PortfolioRisk`].
///
/// The open-order caps and available funds bound what working buy orders
/// could still add to the account. A sell is bounded by what it could sell:
/// the symbol's long position less the symbol's other working sells. Selling
/// what the account holds only reduces exposure, so sells do not draw on the
/// open-order caps or funds.
///
/// An option strategy (several option legs on one underlying and one
/// expiration, priced net) counts as a buy of its contracts whose notional
/// is the most it can lose at expiration
/// ([`defined_risk`](crate::types::trading::defined_risk)); it needs each
/// leg's contract multiplier in `option_multipliers`, and one whose loss has
/// no bound is refused.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortfolioLimits {
    /// Shares all working buy orders on the account may still buy.
    pub max_open_quantity: Decimal,
    /// Notional (quantity × limit) of all working buy orders on the account.
    pub max_open_notional: Decimal,
    /// The most shares of one symbol the account may hold once its working
    /// buys fill.
    pub max_position_quantity: Decimal,
    /// How old, in milliseconds, the reconciled baseline may be when an
    /// order is judged: 1 to 60000, and 5000 when left out.
    #[serde(default = "max_age")]
    pub max_snapshot_age_ms: i64,
    /// The most the account may lose in a day before submissions halt:
    /// Schwab's start-of-day liquidation value less the current one, so
    /// realized and unrealized alike. Absent, the account has no daily-loss
    /// control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_daily_loss: Option<Decimal>,
    /// Operator-verified deliverable per contract by OSI option symbol (100
    /// for a standard contract); an option strategy needs every leg's.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub option_multipliers: HashMap<String, Decimal>,
}
fn max_age() -> i64 {
    5000
}
/// The built-in [`RiskProvider`]: each account's [`PortfolioLimits`]
/// judged against its reconciled baseline, with durable reservations for
/// working orders. It covers limit and stop-limit orders that buy common
/// stock or depositary receipts or sell what the account holds, and
/// defined-risk option strategies.
///
/// A buy must be of common stock or a depositary receipt, by Schwab's word
/// for the symbol. [`screen`](RiskProvider::screen) reads the symbol's
/// quote before a buy labelled `EQUITY` is journaled, and only an
/// `assetMainType` of `EQUITY` with an `assetSubType` of `COE` (common
/// stock), `ADR` or `GDR` (depositary receipts) passes. ETFs are refused
/// because the limits would understate their leverage, so ETF support is
/// left to a later release on purpose. A depositary receipt carries no
/// leverage, and its ratio is already in its price, so the notional,
/// quantity and funds limits bound it exactly as they bound a stock.
/// Exchange-traded notes, closed-end funds, unit investment trusts,
/// preferred shares, warrants, rights, mutual funds and any symbol Schwab
/// classifies as neither are refused; a leg labelled
/// `COLLECTIVE_INVESTMENT` is an ETF without a lookup. A failed lookup
/// refuses the buy with the lookup's own error. What was read is kept in
/// the state database for an hour, so a symbol costs one read an hour, and
/// a buy reserved or checked without being screened is refused.
///
/// A sell can only reduce what the account holds, so it is bounded by the
/// position whatever the instrument, and nothing is looked up for it;
/// neither is an option strategy's underlying, since the strategy's legs
/// bound what it can lose. A deployment that buys anything else needs its
/// own provider.
#[derive(Debug)]
pub struct PortfolioRisk {
    database: StateDatabase,
    accounts: HashMap<String, PortfolioLimits>,
}
fn decimal(value: Option<&serde_json::Value>) -> Option<Decimal> {
    value
        .and_then(|v| {
            v.as_str()
                .map(str::to_owned)
                .or_else(|| v.is_number().then(|| v.to_string()))
        })
        .and_then(|v| v.parse().ok())
}
/// The `currentBalances` fields that value what an account holds, in a cash
/// or a margin account.
const HOLDINGS_VALUES: &[&str] = &[
    "longMarketValue",
    "shortMarketValue",
    "longOptionMarketValue",
    "shortOptionMarketValue",
    "longStockValue",
    "shortStockValue",
    "longNonMarginableMarketValue",
    "mutualFundValue",
    "bondValue",
    "moneyMarketFund",
];
/// The positions of a baseline's `securitiesAccount`, read with
/// `fields=positions`. An account that holds nothing may come without the
/// key; that is taken as no positions only when the balances agree (a
/// `longMarketValue` and every other holdings value reported are zero), so
/// a response that lost the positions of an account with any still fails
/// closed.
fn baseline_positions(account: &serde_json::Value) -> Option<&[serde_json::Value]> {
    if let Some(positions) = account.get("positions") {
        return positions.as_array().map(Vec::as_slice);
    }
    let balances = account.get("currentBalances")?;
    let zero = |field: &str| decimal(balances.get(field)) == Some(Decimal::ZERO);
    let holds_nothing = zero("longMarketValue")
        && HOLDINGS_VALUES
            .iter()
            .all(|field| balances.get(*field).is_none() || zero(field));
    holds_nothing.then_some(&[])
}
/// Where reconciliation records the day's net cash transfers into the
/// account (deposits less withdrawals) beside Schwab's snapshot.
pub(crate) const DAY_TRANSFERS_KEY: &str = "_dayNetTransfers";

/// The account's loss for the day, read from the reconciled snapshot: its
/// start-of-day liquidation value less the current one, with the day's net
/// transfers taken back out of the current value, so a deposit does not
/// hide a loss and a withdrawal does not fake one. `None` when the venue
/// omitted either value, or reconciliation could not read the transfers:
/// counting them as zero would let a deposit hide a loss.
fn daily_loss(snapshot: &serde_json::Value) -> Option<DailyLoss> {
    let account = &snapshot["securitiesAccount"];
    let start = decimal(account.pointer("/initialBalances/liquidationValue"))?;
    let current = decimal(account.pointer("/currentBalances/liquidationValue"))?;
    let transfers = decimal(snapshot.get(DAY_TRANSFERS_KEY))?;
    let traded = current.checked_sub(transfers)?;
    Some(DailyLoss {
        start,
        current: traded,
        loss: start.checked_sub(traded)?,
    })
}
struct DailyLoss {
    start: Decimal,
    current: Decimal,
    loss: Decimal,
}
/// The halt reason the daily-loss control records: `measure` names what was
/// compared (the venue's liquidation value, or paper equity).
pub(crate) fn daily_loss_reason(
    account: &str,
    measure: &str,
    start: Decimal,
    current: Decimal,
    loss: Decimal,
    cap: Decimal,
) -> String {
    format!(
        "Daily loss {loss} exceeds the {cap} limit for account {account}: \
         {measure} {start} at the open, {current} now"
    )
}
/// What one working order or reservation could still do to the account.
/// `symbol` and `sell` are unknown for reservations from before they were
/// recorded; such an exposure counts against every symbol and both sides.
struct Exposure {
    symbol: Option<String>,
    sell: Option<bool>,
    quantity: Decimal,
    notional: Decimal,
}
impl Exposure {
    fn may_buy(&self) -> bool {
        self.sell != Some(true)
    }
    fn may_sell(&self) -> bool {
        self.sell != Some(false)
    }
    fn touches(&self, symbol: &str) -> bool {
        self.symbol.as_deref().is_none_or(|s| s == symbol)
    }
}
/// Totals over the account's other working orders that the new one is
/// judged against.
#[derive(Default)]
struct Outstanding {
    buy_quantity: Decimal,
    buy_notional: Decimal,
    symbol_buys: Decimal,
    symbol_sells: Decimal,
}
impl Outstanding {
    fn add(&mut self, exposure: &Exposure, symbol: &str) -> Result<()> {
        let overflow = || Error::Storage("Risk total overflow".into());
        if exposure.may_buy() {
            self.buy_quantity = self
                .buy_quantity
                .checked_add(exposure.quantity)
                .ok_or_else(overflow)?;
            self.buy_notional = self
                .buy_notional
                .checked_add(exposure.notional)
                .ok_or_else(overflow)?;
            if exposure.touches(symbol) {
                self.symbol_buys = self
                    .symbol_buys
                    .checked_add(exposure.quantity)
                    .ok_or_else(overflow)?;
            }
        }
        if exposure.may_sell() && exposure.touches(symbol) {
            self.symbol_sells = self
                .symbol_sells
                .checked_add(exposure.quantity)
                .ok_or_else(overflow)?;
        }
        Ok(())
    }
}
/// The cash a buy may draw on: a margin account's `availableFunds`, or a
/// cash account's `cashAvailableForTrading` (it has no `availableFunds`).
fn available_funds(account: &serde_json::Value) -> Option<Decimal> {
    decimal(account.pointer("/currentBalances/availableFunds"))
        .or_else(|| decimal(account.pointer("/currentBalances/cashAvailableForTrading")))
}
/// Whether a reading taken at `at` has expired at `now` (Unix
/// milliseconds) for a use that trusts one for `max_age_ms`: older than
/// that, or dated after now, as it is once the clock steps back.
fn expired(at: i64, now: i64, max_age_ms: i64) -> bool {
    now < at || now - at > max_age_ms
}
impl PortfolioRisk {
    /// The provider for `accounts`, each judged against its reconciled
    /// baseline in `database`: the state database of the client it is
    /// installed in (`client.order_journal().database()`). An account with
    /// no limits here is refused every order.
    pub fn new(database: StateDatabase, accounts: HashMap<String, PortfolioLimits>) -> Self {
        Self { database, accounts }
    }
    /// What Schwab says `symbol` is: the type kept from an earlier read
    /// while it is current, or else the quote's, read through `market` and
    /// kept if it names both types. A read that fails returns its own error
    /// and keeps nothing.
    async fn instrument_type(&self, symbol: &str, market: &SchwabClient) -> Result<InstrumentType> {
        // The cache's one reading of the clock: it judges what is kept and
        // dates what is read.
        let now = chrono::Utc::now().timestamp_millis();
        let key = symbol.to_owned();
        let kept = self
            .database
            .run(move |c| stored_instrument_type(c, &key, now))
            .await?;
        if let Some(kept) = kept {
            return Ok(kept);
        }
        let read = InstrumentType::quoted(&market.risk_quote(symbol).await?, symbol);
        if let Some((main, sub)) = read.named() {
            let row = (symbol.to_owned(), wire(&main)?, wire(&sub)?);
            self.database
                .write(move |c| {
                    c.execute(
                        UPSERT_INSTRUMENT_TYPE,
                        rusqlite::params![row.0, row.1, row.2, now],
                    )
                    .map_err(sql)?;
                    Ok(())
                })
                .await?;
        }
        Ok(read)
    }
    async fn validate_and_reserve(
        &self,
        account: &str,
        attempt_id: &str,
        order: &OrderRequest,
        check: Check,
    ) -> Result<()> {
        order.validate()?;
        if let Check::Reserve {
            replaces: Some(replaces),
        }
        | Check::DryRun {
            replaces: Some(replaces),
        } = &check
            && !is_broker_order_id(replaces)
        {
            return Err(Error::InvalidParameter(
                "A replacement must name the order it replaces by its broker order ID".into(),
            ));
        }
        let limits = self
            .accounts
            .get(account)
            .cloned()
            .ok_or_else(|| Error::RiskDenied("Account has no portfolio risk policy".into()))?;
        if limits.max_open_quantity <= Decimal::ZERO
            || limits.max_open_notional <= Decimal::ZERO
            || limits.max_position_quantity <= Decimal::ZERO
            || !(1..=60000).contains(&limits.max_snapshot_age_ms)
            || limits
                .max_daily_loss
                .is_some_and(|cap| cap <= Decimal::ZERO)
        {
            return Err(Error::Config("Invalid portfolio risk limits".into()));
        }
        let proposed = Proposed::of(order, &limits)?;
        proposed.refuse_labelled_etf()?;
        let stock_buy = proposed.stock_buy().is_some();
        let Proposed {
            symbol,
            sell,
            quantity,
            notional,
            strategy,
            ..
        } = proposed;
        let side = if sell { "SELL" } else { "BUY" };
        let account = account.to_owned();
        let id = attempt_id.to_owned();
        self.database
            .write(move |c| {
                use rusqlite::OptionalExtension;
                let tx = c
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                let state: Option<(i64, bool, String)> = tx
                    .query_row(SELECT_ACCOUNT_BASELINE, [&account], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })
                    .optional()
                    .map_err(sql)?;
                let stale = |reason: &str| Error::BaselineStale {
                    account: account.clone(),
                    reason: reason.into(),
                };
                let (at, valid, body) =
                    state.ok_or_else(|| stale("Account requires reconciliation before trading"))?;
                let now = chrono::Utc::now().timestamp_millis();
                let revalidate = matches!(check, Check::Revalidate { .. });
                let dry_run = matches!(check, Check::DryRun { .. });
                if revalidate {
                    ensure_not_halted(&tx)?;
                    if let Check::Revalidate { fence: Some(lease) } = &check
                        && !lease_held(&tx, lease)?
                    {
                        return Err(Error::OwnershipLost);
                    }
                }

                if !valid || expired(at, now, limits.max_snapshot_age_ms) {
                    return Err(stale("Account baseline is stale or unreconciled"));
                }
                let snapshot: serde_json::Value = serde_json::from_str(&body)?;
                let account_value = &snapshot["securitiesAccount"];
                if let Some(cap) = limits.max_daily_loss {
                    let day = daily_loss(&snapshot).ok_or_else(|| {
                        stale(
                            "The risk baseline lacks the liquidation values or the \
                                 day's cash transfers the daily-loss limit needs",
                        )
                    })?;
                    if day.loss > cap {
                        // The halt outlives this denial: commit it, then fail
                        // closed. A dry run only reports it.
                        let reason = daily_loss_reason(
                            &account,
                            "liquidation value",
                            day.start,
                            day.current,
                            day.loss,
                            cap,
                        );
                        if !dry_run {
                            trip_halt(&tx, &reason, DAILY_LOSS_CONTROL)?;
                            tx.commit().map_err(sql)?;
                        }
                        return Err(Error::Halted(reason));
                    }
                }
                let position = baseline_positions(account_value)
                    .ok_or_else(|| stale("Positions missing from risk baseline"))?
                    .iter()
                    .filter(|p| {
                        p.pointer("/instrument/symbol")
                            .and_then(serde_json::Value::as_str)
                            == Some(&symbol)
                    })
                    .try_fold(Decimal::ZERO, |sum, p| {
                        let quantity = decimal(p.get("longQuantity"))
                            .ok_or_else(|| stale("Position quantity missing or invalid"))?;
                        sum.checked_add(quantity)
                            .ok_or_else(|| stale("Position arithmetic overflow"))
                    })?;
                let replaces = match &check {
                    Check::Reserve { replaces } | Check::DryRun { replaces } => replaces.clone(),
                    Check::Revalidate { .. } => {
                        let held: Option<(String, String, Option<String>)> = tx
                            .query_row(
                                SELECT_HELD_RESERVATION,
                                rusqlite::params![id, account],
                                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                            )
                            .optional()
                            .map_err(sql)?;
                        match held {
                            Some((q, n, replaces))
                                if q == quantity.to_string() && n == notional.to_string() =>
                            {
                                replaces
                            }
                            _ => {
                                return Err(Error::Storage(
                                    "Dispatch reservation missing or changed".into(),
                                ));
                            }
                        }
                    }
                };
                let invalid = || Error::Storage("Invalid risk reservation".into());
                let mut outstanding = Outstanding::default();
                let mut stmt = tx.prepare(SELECT_OTHER_HELD_RESERVATIONS).map_err(sql)?;
                let rows = stmt
                    .query_map(rusqlite::params![account, revalidate, id, replaces], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<String>>(3)?,
                        ))
                    })
                    .map_err(sql)?;
                for row in rows {
                    let (q, n, symbol_held, side_held) = row.map_err(sql)?;
                    let exposure = Exposure {
                        sell: side_held.map(|s| s == "SELL"),
                        symbol: symbol_held,
                        quantity: q.parse().map_err(|_| invalid())?,
                        notional: n.parse().map_err(|_| invalid())?,
                    };
                    outstanding.add(&exposure, &symbol)?;
                }
                drop(stmt);
                // Include broker orders from all writers. Locally held, positively
                // linked orders are already reserved; do not count them twice.
                let mut orders = tx.prepare(SELECT_UNRESERVED_BROKER_ORDERS).map_err(sql)?;
                for row in orders
                    .query_map(rusqlite::params![account, replaces], |r| {
                        r.get::<_, String>(0)
                    })
                    .map_err(sql)?
                {
                    let order: serde_json::Value = serde_json::from_str(&row.map_err(sql)?)?;
                    outstanding.add(
                        &external_exposure(&order, &limits.option_multipliers)?,
                        &symbol,
                    )?;
                }
                drop(orders);
                let denied = |what: &str| {
                    Err(Error::RiskDenied(format!(
                        "{what} limit exceeded for this account"
                    )))
                };
                let overflow = || Error::RiskDenied("Risk arithmetic overflow".into());
                if sell {
                    // Only what the account holds and has not already
                    // offered may be sold.
                    let offered = outstanding
                        .symbol_sells
                        .checked_add(quantity)
                        .ok_or_else(overflow)?;
                    if offered > position {
                        return Err(Error::RiskDenied(
                            "Sell exceeds the position less the symbol's working sells".into(),
                        ));
                    }
                } else {
                    // A buy must be of common stock or a depositary receipt,
                    // by the type Schwab gave the symbol when the order was
                    // screened. The
                    // recheck before sending judges the baseline and the
                    // held reservation, which an expired type changes
                    // neither of.
                    if stock_buy && !revalidate {
                        stored_instrument_type(&tx, &symbol, now)?
                            .ok_or_else(|| Error::RiskDenied(UNSCREENED.into()))?
                            .judge()?;
                    }
                    let funds = available_funds(account_value)
                        .ok_or_else(|| stale("Available funds missing from risk baseline"))?;
                    let buy_quantity = outstanding
                        .buy_quantity
                        .checked_add(quantity)
                        .ok_or_else(overflow)?;
                    let buy_notional = outstanding
                        .buy_notional
                        .checked_add(notional)
                        .ok_or_else(overflow)?;
                    let holding = position
                        .checked_add(outstanding.symbol_buys)
                        .and_then(|v| v.checked_add(quantity))
                        .ok_or_else(overflow)?;
                    if buy_quantity > limits.max_open_quantity {
                        return denied("Open buy quantity");
                    }
                    if buy_notional > limits.max_open_notional {
                        return denied("Open buy notional");
                    }
                    if buy_notional > funds {
                        return denied("Available funds");
                    }
                    // A strategy holds contracts, not the symbol's shares.
                    if !strategy && holding > limits.max_position_quantity {
                        return denied("Position quantity");
                    }
                }
                if dry_run {
                    return Ok(());
                }
                if !revalidate {
                    tx.execute(
                        INSERT_HELD_RESERVATION,
                        rusqlite::params![
                            id,
                            account,
                            quantity.to_string(),
                            notional.to_string(),
                            symbol,
                            side,
                            replaces
                        ],
                    )
                    .map_err(sql)?;
                    crate::storage::audit(
                        &tx,
                        "risk_reserved",
                        &id,
                        &serde_json::json!({
                            "quantity": quantity.to_string(),
                            "notional": notional.to_string(),
                            "symbol": symbol,
                            "side": side,
                            "replaces": replaces,
                        }),
                    )?;
                }
                tx.commit().map_err(sql)?;
                Ok(())
            })
            .await
    }
}
/// What an order would add to the account, as the built-in provider judges
/// it.
struct Proposed {
    /// The equity symbol, or a strategy's first leg's OSI symbol (which no
    /// equity position or order shares).
    symbol: String,
    sell: bool,
    quantity: Decimal,
    notional: Decimal,
    /// An option strategy, bounded by its defined risk.
    strategy: bool,
    /// What a buy of one leg is labelled: `EQUITY` or
    /// `COLLECTIVE_INVESTMENT`. `None` for a sell, which can only reduce
    /// what the account holds, and for a strategy, which its legs bound,
    /// whatever either holds.
    buys: Option<AssetType>,
}
impl Proposed {
    /// Refuse a buy labelled an ETF. ETFs are left out on purpose: judged as
    /// a stock, a leveraged or inverse fund's risk would be understated.
    fn refuse_labelled_etf(&self) -> Result<()> {
        if self.buys == Some(AssetType::CollectiveInvestment) {
            return Err(Error::RiskDenied(ETFS.into()));
        }
        Ok(())
    }
    /// The symbol of a buy labelled `EQUITY`, which only common stock or a
    /// depositary receipt may be, by the instrument type Schwab gives it.
    fn stock_buy(&self) -> Option<&str> {
        (self.buys == Some(AssetType::Equity)).then_some(self.symbol.as_str())
    }
    fn of(order: &OrderRequest, limits: &PortfolioLimits) -> Result<Self> {
        if order.order_strategy_type != OrderStrategyType::Single {
            return Err(Error::RiskDenied(
                "Built-in portfolio risk judges one order at a time; \
                 use an external risk provider for conditional strategies"
                    .into(),
            ));
        }
        if order.order_leg_collection.len() > 1 {
            let risk = crate::types::trading::defined_risk(order, |symbol| {
                limits.option_multipliers.get(symbol).copied()
            })
            .map_err(|refused| Error::RiskDenied(refused.to_string()))?;
            return Ok(Self {
                symbol: order.order_leg_collection[0].instrument.symbol.clone(),
                sell: false,
                quantity: risk.contracts,
                notional: risk.max_loss,
                strategy: true,
                buys: None,
            });
        }
        if order.order_leg_collection.len() != 1
            || !matches!(
                order.order_type,
                Some(OrderType::Limit | OrderType::StopLimit)
            )
        {
            return Err(Error::RiskDenied(
                "Built-in portfolio risk requires a limit order or a defined-risk \
                 option strategy"
                    .into(),
            ));
        }
        let leg = &order.order_leg_collection[0];
        if !matches!(
            leg.instrument.asset_type,
            AssetType::Equity | AssetType::CollectiveInvestment
        ) || !matches!(
            leg.instruction,
            OrderInstruction::Buy | OrderInstruction::Sell
        ) {
            return Err(Error::RiskDenied(
                "Built-in portfolio risk supports long equity exposure; \
                 other assets require an external risk provider"
                    .into(),
            ));
        }
        // A sell can only reduce what the account holds, so the sell rule
        // bounds it whatever it sells, an ETF included; a buy's instrument
        // is judged apart (see `refuse_labelled_etf` and `stock_buy`).
        let sell = leg.instruction == OrderInstruction::Sell;
        let price = order
            .price
            .ok_or_else(|| Error::InvalidParameter("Limit price is required".into()))?;
        Ok(Self {
            symbol: leg.instrument.symbol.clone(),
            sell,
            quantity: leg.quantity,
            notional: leg
                .quantity
                .checked_mul(price)
                .ok_or_else(|| Error::InvalidParameter("Risk arithmetic overflow".into()))?,
            strategy: false,
            buys: (!sell).then_some(leg.instrument.asset_type),
        })
    }
}
/// What Schwab's quote calls a symbol: its `assetMainType` and
/// `assetSubType`, each `None` when the quote names none.
#[derive(Clone, Copy, Debug, Default)]
struct InstrumentType {
    main: Option<AssetMainType>,
    sub: Option<AssetSubType>,
}
impl InstrumentType {
    /// What `quotes`, asked for `symbol` alone, say it is: nothing, unless
    /// they hold a quote that is for exactly that symbol.
    fn quoted(quotes: &QuotesResponse, symbol: &str) -> Self {
        quotes
            .quotes
            .get(symbol)
            .filter(|quote| quote.symbol.as_deref() == Some(symbol))
            .map_or_else(Self::default, |quote| Self {
                main: quote.asset_main_type,
                sub: quote.asset_sub_type,
            })
    }
    /// Both types, when the quote names each with a value this release
    /// knows: an answer worth keeping. Anything less is asked for again.
    fn named(self) -> Option<(AssetMainType, AssetSubType)> {
        match (self.main, self.sub) {
            (Some(main), Some(sub))
                if main != AssetMainType::Unknown && sub != AssetSubType::Unknown =>
            {
                Some((main, sub))
            }
            _ => None,
        }
    }
    /// Refuse a buy of anything but common stock or a depositary receipt,
    /// naming what it is. A receipt, like a share of common stock, counts
    /// only under the main type `EQUITY`: it carries no leverage, and its
    /// ratio is already in its price, so the limits bound it as they bound
    /// a stock.
    fn judge(self) -> Result<()> {
        use AssetMainType as Main;
        use AssetSubType as Sub;
        let kind = match (self.main, self.sub) {
            (Some(Main::Equity), Some(Sub::Coe | Sub::Adr | Sub::Gdr)) => return Ok(()),
            (Some(Main::Etf), _) | (_, Some(Sub::Etf)) => {
                return Err(Error::RiskDenied(ETFS.into()));
            }
            (_, Some(Sub::Etn)) => "exchange-traded notes",
            (_, Some(Sub::Cef)) => "closed-end funds",
            (_, Some(Sub::Uit)) => "unit investment trusts",
            (_, Some(Sub::Prf)) => "preferred shares",
            (_, Some(Sub::War)) => "warrants",
            (_, Some(Sub::Rgt)) => "rights",
            (_, Some(Sub::Oef)) => "mutual funds",
            (_, Some(Sub::Mmf)) => "money market funds",
            _ => return Err(Error::RiskDenied(UNCLASSIFIED.into())),
        };
        Err(Error::RiskDenied(format!(
            "Built-in portfolio risk covers common stock and depositary receipts only; \
             {kind} need a deployment's own risk provider"
        )))
    }
}
/// The text Schwab spells `value` with, as the state database keeps it.
fn wire<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Storage("Instrument type is not text".into()))
}
/// A type kept as [`wire`] spelled it.
fn unwire<T: DeserializeOwned>(text: String) -> Result<T> {
    Ok(serde_json::from_value(serde_json::Value::String(text))?)
}
/// `symbol`'s kept instrument type, if one is current at `now`: read no
/// more than [`INSTRUMENT_TYPE_LIFETIME_MS`] before it.
fn stored_instrument_type(
    connection: &rusqlite::Connection,
    symbol: &str,
    now: i64,
) -> Result<Option<InstrumentType>> {
    use rusqlite::OptionalExtension;
    let kept: Option<(String, String, i64)> = connection
        .query_row(SELECT_INSTRUMENT_TYPE, [symbol], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()
        .map_err(sql)?;
    match kept {
        Some((main, sub, at)) if !expired(at, now, INSTRUMENT_TYPE_LIFETIME_MS) => {
            Ok(Some(InstrumentType {
                main: Some(unwire(main)?),
                sub: Some(unwire(sub)?),
            }))
        }
        _ => Ok(None),
    }
}
/// What [`PortfolioRisk::validate_and_reserve`] is asked to do.
enum Check {
    /// Reserve a new order's exposure, replacing broker order `replaces`
    /// if set.
    Reserve { replaces: Option<String> },
    /// Judge it as `Reserve` would, changing nothing.
    DryRun { replaces: Option<String> },
    /// Recheck the held reservation just before transmission, inside the
    /// dispatch fence if one is given.
    Revalidate { fence: Option<Lease> },
}
#[async_trait]
impl RiskProvider for PortfolioRisk {
    /// Refuses a buy labelled an ETF, and reads a buy labelled `EQUITY`'s
    /// instrument type (see [`PortfolioRisk`]). Whatever else `reserve`
    /// would refuse, it is left to report, as it does for an order it is
    /// given unscreened.
    async fn screen(
        &self,
        account: &str,
        order: &OrderRequest,
        market: &SchwabClient,
    ) -> Result<()> {
        let Some(limits) = self.accounts.get(account) else {
            return Ok(());
        };
        let Ok(proposed) = order
            .validate()
            .map_err(Error::from)
            .and_then(|()| Proposed::of(order, limits))
        else {
            return Ok(());
        };
        proposed.refuse_labelled_etf()?;
        match proposed.stock_buy() {
            Some(symbol) => self.instrument_type(symbol, market).await?.judge(),
            None => Ok(()),
        }
    }
    async fn reserve(&self, account: &str, attempt_id: &str, order: &OrderRequest) -> Result<()> {
        self.validate_and_reserve(
            account,
            attempt_id,
            order,
            Check::Reserve { replaces: None },
        )
        .await
    }
    async fn reserve_replacement(
        &self,
        account: &str,
        attempt_id: &str,
        order: &OrderRequest,
        replaces: &str,
    ) -> Result<()> {
        let replaces = Some(replaces.to_owned());
        self.validate_and_reserve(account, attempt_id, order, Check::Reserve { replaces })
            .await
    }
    /// Trips the halt as soon as a reconciled baseline shows the day's loss
    /// past the cap. A baseline that cannot be measured is left to the next
    /// reservation, which fails closed on it.
    async fn observe_baseline(&self, account: &str) -> Result<()> {
        let Some(cap) = self
            .accounts
            .get(account)
            .and_then(|limits| limits.max_daily_loss)
        else {
            return Ok(());
        };
        let account = account.to_owned();
        self.database
            .write(move |c| {
                use rusqlite::OptionalExtension;
                let tx = c
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                let state: Option<(i64, bool, String)> = tx
                    .query_row(SELECT_ACCOUNT_BASELINE, [&account], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })
                    .optional()
                    .map_err(sql)?;
                let Some((_, true, body)) = state else {
                    return Ok(());
                };
                let snapshot: serde_json::Value = serde_json::from_str(&body)?;
                if let Some(day) = daily_loss(&snapshot)
                    && day.loss > cap
                {
                    let reason = daily_loss_reason(
                        &account,
                        "liquidation value",
                        day.start,
                        day.current,
                        day.loss,
                        cap,
                    );
                    trip_halt(&tx, &reason, DAILY_LOSS_CONTROL)?;
                }
                tx.commit().map_err(sql)
            })
            .await
    }
    async fn revalidate(
        &self,
        account: &str,
        attempt_id: &str,
        order: &OrderRequest,
    ) -> Result<()> {
        self.validate_and_reserve(
            account,
            attempt_id,
            order,
            Check::Revalidate { fence: None },
        )
        .await
    }
    async fn revalidate_fenced(
        &self,
        account: &str,
        attempt_id: &str,
        order: &OrderRequest,
        lease: &Lease,
    ) -> Result<()> {
        let fence = Some(lease.clone());
        self.validate_and_reserve(account, attempt_id, order, Check::Revalidate { fence })
            .await
    }
    async fn check(
        &self,
        account: &str,
        order: &OrderRequest,
        replaces: Option<&str>,
    ) -> Result<()> {
        let replaces = replaces.map(str::to_owned);
        self.validate_and_reserve(account, "dry-run", order, Check::DryRun { replaces })
            .await
    }
    fn needs_day_transfers(&self, account: &str) -> bool {
        self.accounts
            .get(account)
            .is_some_and(|limits| limits.max_daily_loss.is_some())
    }
    async fn release(&self, attempt_id: &str) -> Result<()> {
        let id = attempt_id.to_owned();
        self.database
            .write(move |c| {
                c.execute(
                    "UPDATE reservations SET state='released' WHERE id=?1 AND state='held'",
                    [&id],
                )
                .map_err(sql)?;
                Ok(())
            })
            .await
    }
}

/// Conservative exposure for broker orders not covered by local holds:
/// what their unfilled remainder could still buy or sell. A working option
/// strategy counts the unfilled share of its defined risk. An order for no
/// shares or no units cannot be bounded: no order the SDK sends is one
/// (`validate` refuses it), so its figures are not to be trusted.
fn external_exposure(
    order: &serde_json::Value,
    multipliers: &HashMap<String, Decimal>,
) -> Result<Exposure> {
    let deny =
        || Error::RiskDenied("External open order needs a supported, bounded risk model".into());
    if matches!(
        order.get("status").and_then(|v| v.as_str()),
        Some("FILLED" | "CANCELED" | "REJECTED" | "EXPIRED" | "REPLACED")
    ) {
        return Ok(Exposure {
            symbol: None,
            sell: None,
            quantity: Decimal::ZERO,
            notional: Decimal::ZERO,
        });
    }
    if order
        .get("orderLegCollection")
        .and_then(|v| v.as_array())
        .is_some_and(|legs| legs.len() > 1)
    {
        return external_strategy_exposure(order, multipliers).ok_or_else(deny);
    }
    if order.get("status").and_then(|v| v.as_str()).is_none()
        || order.get("orderStrategyType").and_then(|v| v.as_str()) != Some("SINGLE")
        || !matches!(
            order.get("orderType").and_then(|v| v.as_str()),
            Some("LIMIT" | "STOP_LIMIT")
        )
    {
        return Err(deny());
    }
    let legs = order
        .get("orderLegCollection")
        .and_then(|v| v.as_array())
        .ok_or_else(deny)?;
    if legs.len() != 1
        || legs[0]
            .pointer("/instrument/assetType")
            .and_then(|v| v.as_str())
            != Some("EQUITY")
        || !matches!(
            legs[0].get("instruction").and_then(|v| v.as_str()),
            Some("BUY" | "SELL")
        )
    {
        return Err(deny());
    }
    let total = decimal(order.get("quantity"))
        .or_else(|| decimal(legs[0].get("quantity")))
        .ok_or_else(deny)?;
    let filled = decimal(order.get("filledQuantity")).ok_or_else(deny)?;
    let remaining = decimal(order.get("remainingQuantity"))
        .or_else(|| total.checked_sub(filled))
        .ok_or_else(deny)?;
    let price = decimal(order.get("price")).ok_or_else(deny)?;
    if total <= Decimal::ZERO
        || filled < Decimal::ZERO
        || filled > total
        || remaining < Decimal::ZERO
        || remaining > total
        || filled.checked_add(remaining) != Some(total)
        || price <= Decimal::ZERO
    {
        return Err(deny());
    }
    Ok(Exposure {
        symbol: Some(
            legs[0]
                .pointer("/instrument/symbol")
                .and_then(|v| v.as_str())
                .ok_or_else(deny)?
                .to_owned(),
        ),
        sell: Some(legs[0].get("instruction").and_then(|v| v.as_str()) == Some("SELL")),
        quantity: remaining,
        notional: remaining.checked_mul(price).ok_or_else(deny)?,
    })
}

/// [`external_exposure`] of a working option strategy: its defined risk
/// scaled by the share of its units still unfilled; `None` if it cannot be
/// bounded.
fn external_strategy_exposure(
    order: &serde_json::Value,
    multipliers: &HashMap<String, Decimal>,
) -> Option<Exposure> {
    use crate::types::trading::Order;
    if order.get("status").and_then(|v| v.as_str()).is_none()
        || order.get("orderStrategyType").and_then(|v| v.as_str()) != Some("SINGLE")
    {
        return None;
    }
    let parsed: Order = serde_json::from_value(order.clone()).ok()?;
    let request = OrderRequest::try_from(&parsed).ok()?;
    let risk =
        crate::types::trading::defined_risk(&request, |symbol| multipliers.get(symbol).copied())
            .ok()?;
    let total = decimal(order.get("quantity"))?;
    let remaining = decimal(order.get("remainingQuantity")).or_else(|| {
        decimal(order.get("filledQuantity")).and_then(|filled| total.checked_sub(filled))
    })?;
    if total <= Decimal::ZERO || remaining < Decimal::ZERO || remaining > total {
        return None;
    }
    let share = remaining.checked_div(total)?;
    Some(Exposure {
        symbol: Some(request.order_leg_collection[0].instrument.symbol.clone()),
        sell: Some(false),
        quantity: risk.contracts.checked_mul(share)?,
        notional: risk.max_loss.checked_mul(share)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A baseline is trusted from the millisecond it is dated through
    /// exactly the policy's age: stale a millisecond later, and stale if it
    /// is dated even a millisecond after now.
    #[test]
    fn a_baseline_is_trusted_from_its_date_through_exactly_the_policys_age() {
        let at = 1_764_000_000_000;
        assert!(!expired(at, at, 5000));
        assert!(!expired(at, at + 5000, 5000));
        assert!(expired(at, at + 5001, 5000));
        assert!(expired(at, at - 1, 5000));
    }

    /// A symbol's kept instrument type is current from the millisecond it
    /// was read through exactly an hour, and not once the clock is back
    /// before it; a symbol never read has none.
    #[tokio::test]
    async fn an_instrument_type_is_kept_from_its_reading_through_exactly_an_hour() {
        let dir = tempfile::tempdir().unwrap();
        let database = StateDatabase::new(dir.path().join("state.json"));
        let at: i64 = 1_764_000_000_000;
        let kept = |now: i64| {
            let database = database.clone();
            async move {
                database
                    .run(move |c| stored_instrument_type(c, "AAPL", now))
                    .await
                    .unwrap()
                    .map(|kept| (kept.main, kept.sub))
            }
        };
        assert_eq!(kept(at).await, None);
        database
            .write(move |c| {
                c.execute(
                    UPSERT_INSTRUMENT_TYPE,
                    rusqlite::params!["AAPL", "EQUITY", "COE", at],
                )
                .map_err(sql)?;
                Ok(())
            })
            .await
            .unwrap();
        let common = Some((Some(AssetMainType::Equity), Some(AssetSubType::Coe)));
        assert_eq!(kept(at).await, common);
        assert_eq!(kept(at + 3_600_000).await, common);
        assert_eq!(kept(at + 3_600_001).await, None);
        assert_eq!(kept(at - 1).await, None);
    }
}
