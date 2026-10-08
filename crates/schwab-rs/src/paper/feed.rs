//! Read-only Schwab REST feed and broker-supplied regular-session calendar.
//! Polling does not model every exchange tick. Each side must carry a fresh
//! timestamp; old snapshots cannot replenish simulated liquidity.
use super::{PaperBroker, PaperClockMode, PaperEventKind, PaperEventSource};
use crate::{
    Error, Result, SchwabClient,
    types::{
        common::AssetMainType,
        market_data::{Market, MarketHours, QuoteItem, equity_hours},
    },
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::{collections::BTreeSet, sync::Arc, time::Duration};

/// Upper bound on one calendar or quote fetch; a slow broker must not stall
/// the clock indefinitely.
const FEED_DEADLINE: Duration = Duration::from_secs(10);

/// The US equity trading day `now` falls in: New York's calendar date. The
/// UTC date turns over at 8 pm Eastern (7 pm in winter), which would open
/// the next day's session hours early.
pub fn trading_date(now: DateTime<Utc>) -> chrono::NaiveDate {
    now.with_timezone(&chrono_tz::America::New_York)
        .date_naive()
}

fn deadline_exceeded() -> Error {
    Error::Timeout {
        duration: FEED_DEADLINE.as_secs(),
    }
}

pub fn regular_session_open(hours: &MarketHours, date: &str, at_ms: i64) -> Result<bool> {
    let deny = || Error::InvalidParameter("Missing or invalid broker calendar".into());
    if hours.date.as_deref() != Some(date) {
        return Err(deny());
    }
    if hours.is_open == Some(false) {
        return Ok(false);
    }
    if hours.is_open != Some(true) {
        return Err(deny());
    }
    let sessions = hours
        .session_hours
        .as_ref()
        .and_then(|h| h.regular_market.as_ref())
        .ok_or_else(deny)?;
    if sessions.is_empty() || sessions.len() > 8 {
        return Err(deny());
    }
    let mut open = false;
    let mut last_end = None;
    for session in sessions {
        let start = DateTime::parse_from_rfc3339(session.start.as_deref().ok_or_else(deny)?)
            .map_err(|_| deny())?
            .timestamp_millis();
        let end = DateTime::parse_from_rfc3339(session.end.as_deref().ok_or_else(deny)?)
            .map_err(|_| deny())?
            .timestamp_millis();
        if start >= end || last_end.is_some_and(|last| start < last) {
            return Err(deny());
        }
        last_end = Some(end);
        open |= start <= at_ms && at_ms < end;
    }
    Ok(open)
}
/// The multiplier must match the chosen feed's documented size units.
pub fn normalize_quote(
    symbol: &str,
    item: &QuoteItem,
    size_multiplier: Decimal,
) -> Result<PaperEventKind> {
    let deny = || {
        Error::InvalidParameter(
            "Paper feed requires identified real-time equity BBO, side timestamps and sizes".into(),
        )
    };
    if item.realtime != Some(true)
        || item.asset_main_type != Some(AssetMainType::Equity)
        || item.symbol.as_deref() != Some(symbol)
        || size_multiplier <= Decimal::ZERO
        || size_multiplier > Decimal::from(1000)
        || !size_multiplier.fract().is_zero()
    {
        return Err(deny());
    }
    let quote = item.quote.as_ref().ok_or_else(deny)?;
    let decimal = |v: Option<f64>| -> Result<Decimal> {
        let value = v.filter(|v| v.is_finite() && *v > 0.0).ok_or_else(deny)?;
        value.to_string().parse().map_err(|_| deny())
    };
    let size = |v: Option<i32>| -> Result<Decimal> {
        Decimal::from(v.filter(|v| *v >= 0).ok_or_else(deny)?)
            .checked_mul(size_multiplier)
            .ok_or_else(deny)
    };
    Ok(PaperEventKind::Quote {
        symbol: symbol.into(),
        source_at_ms: quote
            .bid_time
            .ok_or_else(deny)?
            .min(quote.ask_time.ok_or_else(deny)?),
        bid: decimal(quote.bid_price)?,
        ask: decimal(quote.ask_price)?,
        bid_size: size(quote.bid_size)?,
        ask_size: size(quote.ask_size)?,
    })
}

/// Owns a separate market-data client. Only read endpoints are called.
/// Use one feed driver per paper run; readiness fails closed when polling stops.
pub struct SchwabPaperFeed {
    market_data: Arc<SchwabClient>,
    paper: PaperBroker,
    symbols: Vec<String>,
    size_multiplier: Decimal,
    poll_lock: tokio::sync::Mutex<()>,
}
impl SchwabPaperFeed {
    pub fn new(
        market_data: Arc<SchwabClient>,
        paper: PaperBroker,
        symbols: Vec<String>,
        size_multiplier: Decimal,
    ) -> Result<Self> {
        if paper.config().clock != PaperClockMode::Wall
            || symbols.is_empty()
            || symbols.len() > 100
            || symbols.iter().collect::<BTreeSet<_>>().len() != symbols.len()
            || symbols.iter().any(|s| s.is_empty() || s.len() > 32)
            || size_multiplier <= Decimal::ZERO
            || size_multiplier > Decimal::from(1000)
            || !size_multiplier.fract().is_zero()
        {
            return Err(Error::Config("Invalid paper feed configuration".into()));
        }
        Ok(Self {
            market_data,
            paper,
            symbols,
            size_multiplier,
            poll_lock: tokio::sync::Mutex::new(()),
        })
    }
    /// Bound each network read. Failure durably invalidates quotes; no simulated fill
    /// is synthesized to cover a missed or invalid upstream observation.
    pub async fn tick(&self) -> Result<()> {
        let _guard = self.poll_lock.lock().await;
        match self.poll().await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.publish(PaperEventKind::Disconnect).await?;
                Err(error)
            }
        }
    }
    async fn publish(&self, kind: PaperEventKind) -> Result<super::PaperEventResult> {
        self.paper
            .append_live_from(kind, PaperEventSource::SchwabRest)
            .await
    }
    async fn poll(&self) -> Result<()> {
        let today = trading_date(Utc::now());
        let date = today.format("%Y-%m-%d").to_string();
        // Do not cancel database mutations: blocking SQLite work may outlive a
        // dropped future and otherwise commit after the disconnect event.
        let hours = tokio::time::timeout(
            FEED_DEADLINE,
            self.market_data.get_market(Market::Equity, Some(today)),
        )
        .await
        .map_err(|_| deadline_exceeded())??;
        // Schwab keys a trading day by product code and a closed day by market
        // name, so the shared lookup decides; reading only `EQ` would turn every
        // holiday and weekend into a feed error and a disconnect.
        let hours = equity_hours(&hours)
            .ok_or_else(|| Error::InvalidParameter("Equity calendar absent".into()))?;
        let open = regular_session_open(hours, &date, Utc::now().timestamp_millis())?;
        if self.paper.market_session().await? != (date.clone(), open) {
            self.publish(PaperEventKind::Session {
                session_id: date.clone(),
                open,
            })
            .await?;
        } else {
            self.publish(PaperEventKind::Clock).await?;
        }
        if !open {
            return Ok(());
        }
        let symbols: Vec<_> = self.symbols.iter().map(String::as_str).collect();
        let quotes = tokio::time::timeout(FEED_DEADLINE, self.market_data.get_quotes(&symbols))
            .await
            .map_err(|_| deadline_exceeded())??;
        if !regular_session_open(hours, &date, Utc::now().timestamp_millis())? {
            self.publish(PaperEventKind::Session {
                session_id: date,
                open: false,
            })
            .await?;
            return Ok(());
        }
        // One symbol's bad quote (absent, crossed, stale, under a dollar)
        // costs that symbol its update, not every symbol its quotes: the
        // venue keeps judging the rest, and the skipped one ages out.
        for symbol in &self.symbols {
            let Some(item) = quotes.quotes.get(symbol) else {
                tracing::warn!(%symbol, "paper feed: Schwab returned no quote; skipped");
                continue;
            };
            let event = match normalize_quote(symbol, item, self.size_multiplier) {
                Ok(event) => event,
                Err(error) => {
                    tracing::warn!(%symbol, %error, "paper feed: unusable quote; skipped");
                    continue;
                }
            };
            if let PaperEventKind::Quote { source_at_ms, .. } = &event
                && self
                    .paper
                    .quote_watermark(symbol)
                    .await?
                    .is_some_and(|last| *source_at_ms <= last)
            {
                continue;
            }
            match self.publish(event).await {
                Ok(_) => {}
                // The venue's own state failing is the feed failing.
                Err(error @ (Error::Storage(_) | Error::Io(_))) => return Err(error),
                Err(error) => {
                    tracing::warn!(%symbol, %error, "paper feed: the venue refused a quote; skipped");
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::trading_date;
    use chrono::{NaiveDate, TimeZone, Utc};

    #[test]
    fn the_trading_day_is_new_yorks_date() {
        let day = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).unwrap();
        // 21:30 EDT on the 16th is already the 17th in UTC.
        let evening = Utc.with_ymd_and_hms(2024, 5, 17, 1, 30, 0).unwrap();
        assert_eq!(trading_date(evening), day(2024, 5, 16));
        // 19:30 EST on the 16th, winter offset.
        let winter = Utc.with_ymd_and_hms(2024, 1, 17, 0, 30, 0).unwrap();
        assert_eq!(trading_date(winter), day(2024, 1, 16));
        let open = Utc.with_ymd_and_hms(2024, 5, 17, 13, 30, 0).unwrap();
        assert_eq!(trading_date(open), day(2024, 5, 17));
    }
}
