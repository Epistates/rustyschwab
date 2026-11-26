//! What the built-in risk provider lets an account buy and sell, judged by
//! what Schwab says each instrument is.
//!
//! A buy must be of plain common stock. One labelled `EQUITY` is looked up
//! in Schwab's quote (its `assetMainType` and `assetSubType`) before the
//! order is journaled, so a refused buy leaves no trace and never reaches
//! Schwab; one labelled `COLLECTIVE_INVESTMENT` is an ETF by its label. A
//! sell only reduces what the account holds, so it is judged by what is
//! held, whatever the instrument, and needs no lookup. Option strategies are
//! bounded by their legs, and their underlying is not looked up either.
mod common;

use common::mock_client;
use rustyschwab::{
    Error, ErrorKind, SchwabClient,
    broker::TradingBackend,
    execution::SubmissionOutcome,
    risk::{PortfolioLimits, PortfolioRisk, RiskProvider},
    types::trading::OrderRequest,
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const ACCOUNT: &str = "hash";
/// Two calls on one underlying and one expiration, a vertical's legs.
const C100: &str = "XYZ   260116C00100000";
const C105: &str = "XYZ   260116C00105000";

const ETFS: &str = "Built-in portfolio risk covers long equity only; ETFs need a \
    deployment's own risk provider";
const UNCLASSIFIED: &str = "Built-in portfolio risk covers common stock only; a symbol \
    Schwab does not classify as common stock needs a deployment's own risk provider";
const OVERSOLD: &str = "Sell exceeds the position less the symbol's working sells";

/// The refusal for a buy of `kind`, a kind of instrument other than an ETF.
fn not_common_stock(kind: &str) -> String {
    format!(
        "Built-in portfolio risk covers common stock only; {kind} need a deployment's own \
         risk provider"
    )
}

/// A client whose built-in provider judges [`ACCOUNT`] against a fresh,
/// funded baseline holding `positions`, and the mock Schwab it talks to.
async fn trader(positions: Value) -> (MockServer, SchwabClient, tempfile::TempDir) {
    let server = MockServer::start().await;
    let (base, dir, _) = mock_client(&server, false);
    let database = base.order_journal().database().clone();
    database
        .reconcile_snapshot(
            ACCOUNT,
            1,
            chrono::Utc::now().timestamp_millis(),
            json!({"securitiesAccount": {
                "currentBalances": {"availableFunds": "100000"},
                "positions": positions,
            }}),
            vec![],
        )
        .await
        .unwrap();
    let limits = PortfolioLimits {
        max_open_quantity: 1000.into(),
        max_open_notional: 100_000.into(),
        max_position_quantity: 1000.into(),
        max_snapshot_age_ms: 60_000,
        max_daily_loss: None,
        option_multipliers: HashMap::from([
            (C100.to_owned(), 100.into()),
            (C105.to_owned(), 100.into()),
        ]),
    };
    let risk = PortfolioRisk::new(database, HashMap::from([(ACCOUNT.into(), limits)]));
    let client = base.with_risk_provider(Arc::new(risk)).unwrap();
    (server, client, dir)
}

/// A long position of `quantity` in `symbol`, listed as Schwab lists it.
fn holding(symbol: &str, asset: &str, quantity: u32) -> Value {
    json!({"longQuantity": quantity, "shortQuantity": 0,
        "instrument": {"symbol": symbol, "assetType": asset}})
}

/// A one-leg limit order at 50.
fn order(instruction: &str, asset: &str, symbol: &str, quantity: u32) -> OrderRequest {
    serde_json::from_value(json!({
        "orderStrategyType": "SINGLE", "orderType": "LIMIT", "session": "NORMAL",
        "duration": "DAY", "price": "50",
        "orderLegCollection": [{"quantity": quantity, "instruction": instruction,
            "instrument": {"symbol": symbol, "assetType": asset}}]
    }))
    .unwrap()
}

/// Schwab's quote for `symbol` as `GET /marketdata/v1/quotes` returns it,
/// keyed by the symbol asked for, with `assetMainType` and `assetSubType`
/// left out when `None`.
fn quote(symbol: &str, main: Option<&str>, sub: Option<&str>) -> Value {
    let mut item = json!({
        "symbol": symbol, "quoteType": "NBBO", "realtime": true, "ssid": 1_973_757_747,
        "quote": {"bidPrice": 49.99, "askPrice": 50.01, "lastPrice": 50.0,
            "totalVolume": 1_000_000, "securityStatus": "Normal"},
        "reference": {"cusip": "000000000", "description": symbol, "exchange": "Q",
            "exchangeName": "NASDAQ"},
        "fundamental": {"divYield": 0.0, "fundLeverageFactor": 0.0},
    });
    if let Some(main) = main {
        item["assetMainType"] = main.into();
    }
    if let Some(sub) = sub {
        item["assetSubType"] = sub.into();
    }
    json!({ symbol: item })
}

/// Answer every quote request for `symbol` with `body`.
async fn quoted(server: &MockServer, symbol: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path("/marketdata/v1/quotes"))
        .and(query_param("symbols", symbol))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

/// Accept every order placed on [`ACCOUNT`] as broker order 1001.
async fn accepting(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(format!("/trader/v1/accounts/{ACCOUNT}/orders")))
        .respond_with(ResponseTemplate::new(201).insert_header(
            "Location",
            format!("https://api.schwabapi.com/trader/v1/accounts/{ACCOUNT}/orders/1001"),
        ))
        .mount(server)
        .await;
}

async fn requests(server: &MockServer, matching: impl Fn(&Request) -> bool) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| matching(request))
        .count()
}

/// How many times `symbol`'s quote was read.
async fn quote_reads(server: &MockServer, symbol: &str) -> usize {
    requests(server, |request| {
        request.url.path() == "/marketdata/v1/quotes"
            && request
                .url
                .query_pairs()
                .any(|(key, value)| key == "symbols" && value == symbol)
    })
    .await
}

/// How many orders were placed or replaced at Schwab.
async fn orders_sent(server: &MockServer) -> usize {
    requests(server, |request| request.url.path().contains("/orders")).await
}

/// `order` is refused for `reason`, as a submission and as a check alike,
/// and leaves no trace: nothing journaled, nothing sent.
async fn refused_unsent(
    client: &SchwabClient,
    server: &MockServer,
    order: &OrderRequest,
    reason: &str,
) {
    let placed = client.place_order(ACCOUNT, order).await;
    assert!(
        matches!(&placed, Err(Error::RiskDenied(r)) if r == reason),
        "placed: {placed:?}"
    );
    let checked = client.check_risk(ACCOUNT, order, None).await;
    assert!(
        matches!(&checked, Err(Error::RiskDenied(r)) if r == reason),
        "checked: {checked:?}"
    );
    assert_eq!(orders_sent(server).await, 0);
    assert!(client.order_journal().records().await.unwrap().is_empty());
}

/// Date `symbol`'s stored instrument type back by `by_ms`, as if it had
/// been read that long ago.
fn age_instrument_type(client: &SchwabClient, symbol: &str, by_ms: i64) {
    let connection = rusqlite::Connection::open(client.order_journal().database().path()).unwrap();
    let aged = connection
        .execute(
            "UPDATE instrument_types SET observed_at=observed_at-?1 WHERE symbol=?2",
            rusqlite::params![by_ms, symbol],
        )
        .unwrap();
    assert_eq!(aged, 1, "{symbol} has a stored instrument type");
}

/// Plain common stock is what the provider buys: Schwab's `EQUITY` and
/// `COE`. The order is journaled, sent and accepted.
#[tokio::test]
async fn common_stock_is_bought() {
    let (server, client, _dir) = trader(json!([])).await;
    quoted(&server, "AAPL", quote("AAPL", Some("EQUITY"), Some("COE"))).await;
    accepting(&server).await;
    let buy = order("BUY", "EQUITY", "AAPL", 10);
    client.check_risk(ACCOUNT, &buy, None).await.unwrap();
    let placed = client.place_order(ACCOUNT, &buy).await.unwrap();
    assert_eq!(placed.order_id.as_deref(), Some("1001"));
    let records = client.order_journal().records().await.unwrap();
    assert_eq!(records.len(), 1);
    assert!(matches!(
        &records[0].outcome,
        Some(SubmissionOutcome::Accepted { order_id: Some(id) }) if id == "1001"
    ));
    assert_eq!(orders_sent(&server).await, 1);
}

/// An ETF ordered as `EQUITY`, which the order alone cannot tell from a
/// stock, is refused by Schwab's word for it: before anything is journaled
/// or sent, and with the same reason a check gives.
#[tokio::test]
async fn an_etf_ordered_as_equity_is_refused_before_it_is_journaled_or_sent() {
    let (server, client, _dir) = trader(json!([])).await;
    quoted(&server, "SPY", quote("SPY", Some("EQUITY"), Some("ETF"))).await;
    accepting(&server).await;
    refused_unsent(&client, &server, &order("BUY", "EQUITY", "SPY", 1), ETFS).await;
}

/// A leveraged or an inverse fund is an ETF like any other: refused, with
/// no lookup of its leverage needed.
#[tokio::test]
async fn leveraged_and_inverse_etfs_are_refused() {
    let (server, client, _dir) = trader(json!([])).await;
    for (symbol, leverage) in [("TQQQ", 3.0), ("SQQQ", -3.0)] {
        let mut body = quote(symbol, Some("EQUITY"), Some("ETF"));
        body[symbol]["fundamental"]["fundLeverageFactor"] = leverage.into();
        quoted(&server, symbol, body).await;
    }
    accepting(&server).await;
    for symbol in ["TQQQ", "SQQQ"] {
        refused_unsent(&client, &server, &order("BUY", "EQUITY", symbol, 1), ETFS).await;
    }
}

/// Every other kind of instrument Schwab names is refused by that name,
/// never called an ETF. Depositary receipts are among them: Schwab's quote
/// does not say whether one holds common or preferred shares.
#[tokio::test]
async fn other_instruments_are_refused_by_their_kind() {
    let (server, client, _dir) = trader(json!([])).await;
    let cases = [
        ("ETNA", "EQUITY", "ETN", "exchange-traded notes"),
        ("CEFA", "EQUITY", "CEF", "closed-end funds"),
        ("UITA", "EQUITY", "UIT", "unit investment trusts"),
        ("PRFA", "EQUITY", "PRF", "preferred shares"),
        ("WARA", "EQUITY", "WAR", "warrants"),
        ("RGTA", "EQUITY", "RGT", "rights"),
        ("ADRA", "EQUITY", "ADR", "depositary receipts"),
        ("GDRA", "EQUITY", "GDR", "depositary receipts"),
        ("OEFA", "MUTUAL_FUND", "OEF", "mutual funds"),
        ("MMFA", "MUTUAL_FUND", "MMF", "money market funds"),
        ("CEFM", "MUTUAL_FUND", "CEF", "closed-end funds"),
    ];
    for (symbol, main, sub, _) in cases {
        quoted(&server, symbol, quote(symbol, Some(main), Some(sub))).await;
    }
    accepting(&server).await;
    for (symbol, _, _, kind) in cases {
        let reason = not_common_stock(kind);
        refused_unsent(
            &client,
            &server,
            &order("BUY", "EQUITY", symbol, 1),
            &reason,
        )
        .await;
    }
}

/// A fund Schwab's main type alone calls an ETF is one, whatever its
/// subtype says.
#[tokio::test]
async fn an_etf_by_main_type_is_refused() {
    let (server, client, _dir) = trader(json!([])).await;
    quoted(&server, "ETFM", quote("ETFM", Some("ETF"), Some("COE"))).await;
    quoted(&server, "ETFN", quote("ETFN", Some("ETF"), None)).await;
    accepting(&server).await;
    for symbol in ["ETFM", "ETFN"] {
        refused_unsent(&client, &server, &order("BUY", "EQUITY", symbol, 1), ETFS).await;
    }
}

/// An answer that does not say the symbol is plain common stock refuses it:
/// no subtype, a subtype this release does not know, a main type other
/// than `EQUITY`, no quote for the symbol, a quote for another symbol, or
/// one that does not name its symbol. Nothing is assumed to be stock.
#[tokio::test]
async fn a_symbol_schwab_does_not_classify_as_common_stock_is_refused() {
    let (server, client, _dir) = trader(json!([])).await;
    let mut unnamed = quote("NONM", Some("EQUITY"), Some("COE"));
    unnamed["NONM"].as_object_mut().unwrap().remove("symbol");
    let mut other = quote("OTHR", Some("EQUITY"), Some("COE"));
    other["OTHR"]["symbol"] = "AAPL".into();
    let answers = [
        ("NOSB", quote("NOSB", Some("EQUITY"), None)),
        ("UNKN", quote("UNKN", Some("EQUITY"), Some("UNKNOWN"))),
        ("NEWT", quote("NEWT", Some("EQUITY"), Some("NEWTYPE"))),
        ("IDXA", quote("IDXA", Some("INDEX"), Some("COE"))),
        ("NOMN", quote("NOMN", None, Some("COE"))),
        ("EMPT", json!({})),
        ("BADS", json!({"errors": {"invalidSymbols": ["BADS"]}})),
        ("ELSE", quote("AAPL", Some("EQUITY"), Some("COE"))),
        ("NONM", unnamed),
        ("OTHR", other),
    ];
    for (symbol, body) in &answers {
        quoted(&server, symbol, body.clone()).await;
    }
    accepting(&server).await;
    for (symbol, _) in &answers {
        refused_unsent(
            &client,
            &server,
            &order("BUY", "EQUITY", symbol, 1),
            UNCLASSIFIED,
        )
        .await;
    }
    // None of these answers is kept, so each order asks again, except the
    // one that names two known types: it is kept, and judged as kept.
    for (symbol, _) in &answers {
        let reads = if *symbol == "IDXA" { 1 } else { 2 };
        assert_eq!(quote_reads(&server, symbol).await, reads, "{symbol}");
    }
}

/// When the lookup fails the buy is refused with the lookup's own error,
/// retryable when the failure is, and leaves no trace. Nothing is kept, so
/// once Schwab answers the next order is judged on the answer.
#[tokio::test]
async fn a_failed_lookup_refuses_the_buy_and_leaves_no_trace() {
    let (server, client, _dir) = trader(json!([])).await;
    let failing = Mock::given(method("GET"))
        .and(path("/marketdata/v1/quotes"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "errors": [{"status": "503", "title": "Service Unavailable"}]
        })))
        .mount_as_scoped(&server)
        .await;
    accepting(&server).await;
    let buy = order("BUY", "EQUITY", "AAPL", 1);
    let placed = client.place_order(ACCOUNT, &buy).await.unwrap_err();
    assert_eq!(
        placed.http_status().map(|s| s.as_u16()),
        Some(503),
        "{placed:?}"
    );
    assert!(placed.is_retryable(), "{placed:?}");
    assert_ne!(placed.kind(), ErrorKind::RiskDenied);
    let checked = client.check_risk(ACCOUNT, &buy, None).await.unwrap_err();
    assert_eq!(
        checked.http_status().map(|s| s.as_u16()),
        Some(503),
        "{checked:?}"
    );
    assert_eq!(orders_sent(&server).await, 0);
    assert!(client.order_journal().records().await.unwrap().is_empty());
    drop(failing);

    quoted(&server, "AAPL", quote("AAPL", Some("EQUITY"), Some("COE"))).await;
    client.place_order(ACCOUNT, &buy).await.unwrap();
    assert_eq!(orders_sent(&server).await, 1);
}

/// A symbol's type is read once and kept, shared by every order and check
/// on the state file, until it is an hour old; then the next order reads it
/// again. What was kept is what Schwab said, refusals included.
#[tokio::test]
async fn an_instrument_type_is_read_once_an_hour() {
    let (server, client, _dir) = trader(json!([])).await;
    quoted(&server, "AAPL", quote("AAPL", Some("EQUITY"), Some("COE"))).await;
    quoted(&server, "SPY", quote("SPY", Some("EQUITY"), Some("ETF"))).await;
    accepting(&server).await;
    let buy = order("BUY", "EQUITY", "AAPL", 1);
    client.check_risk(ACCOUNT, &buy, None).await.unwrap();
    client.place_order(ACCOUNT, &buy).await.unwrap();
    client.check_risk(ACCOUNT, &buy, None).await.unwrap();
    assert_eq!(quote_reads(&server, "AAPL").await, 1);

    // Just inside the hour it is still current.
    age_instrument_type(&client, "AAPL", 3_590_000);
    client.check_risk(ACCOUNT, &buy, None).await.unwrap();
    assert_eq!(quote_reads(&server, "AAPL").await, 1);
    // Past it, the next check reads it again, and keeps the new reading.
    age_instrument_type(&client, "AAPL", 20_000);
    client.check_risk(ACCOUNT, &buy, None).await.unwrap();
    client.check_risk(ACCOUNT, &buy, None).await.unwrap();
    assert_eq!(quote_reads(&server, "AAPL").await, 2);

    let etf = order("BUY", "EQUITY", "SPY", 1);
    for _ in 0..2 {
        let refused = client.check_risk(ACCOUNT, &etf, None).await;
        assert!(
            matches!(&refused, Err(Error::RiskDenied(r)) if r == ETFS),
            "{refused:?}"
        );
    }
    assert_eq!(quote_reads(&server, "SPY").await, 1);
}

/// A leg labelled `COLLECTIVE_INVESTMENT` is an ETF by its order alone: the
/// buy is refused before it is journaled, and Schwab is not asked.
#[tokio::test]
async fn a_buy_labelled_an_etf_is_refused_without_a_lookup() {
    let (server, client, _dir) = trader(json!([])).await;
    accepting(&server).await;
    refused_unsent(
        &client,
        &server,
        &order("BUY", "COLLECTIVE_INVESTMENT", "SPY", 1),
        ETFS,
    )
    .await;
    assert_eq!(quote_reads(&server, "SPY").await, 0);
}

/// A sell only reduces what the account holds, so a held ETF (even a
/// leveraged one bought before this release, or transferred in) sells down
/// to nothing under either label, and Schwab is not asked what it is. A
/// sell of more than is held, or of what is not held at all, opens a short
/// position, and is refused whatever the instrument.
#[tokio::test]
async fn a_held_etf_sells_down_to_nothing_and_no_further() {
    let (server, client, _dir) = trader(json!([
        holding("TQQQ", "COLLECTIVE_INVESTMENT", 10),
        holding("SPY", "COLLECTIVE_INVESTMENT", 5),
    ]))
    .await;
    accepting(&server).await;
    client
        .check_risk(
            ACCOUNT,
            &order("SELL", "COLLECTIVE_INVESTMENT", "TQQQ", 10),
            None,
        )
        .await
        .unwrap();
    client
        .check_risk(ACCOUNT, &order("SELL", "EQUITY", "TQQQ", 10), None)
        .await
        .unwrap();
    client
        .place_order(ACCOUNT, &order("SELL", "COLLECTIVE_INVESTMENT", "SPY", 5))
        .await
        .unwrap();
    for oversold in [
        order("SELL", "COLLECTIVE_INVESTMENT", "TQQQ", 11),
        order("SELL", "EQUITY", "TQQQ", 11),
        order("SELL", "COLLECTIVE_INVESTMENT", "SPY", 1),
        order("SELL", "EQUITY", "QQQ", 1),
    ] {
        let refused = client.check_risk(ACCOUNT, &oversold, None).await;
        assert!(
            matches!(&refused, Err(Error::RiskDenied(r)) if r == OVERSOLD),
            "{oversold:?}: {refused:?}"
        );
    }
    assert_eq!(orders_sent(&server).await, 1);
    assert_eq!(
        requests(&server, |r| r.url.path() == "/marketdata/v1/quotes").await,
        0
    );
}

/// An option strategy is bounded by its own legs, so its underlying is not
/// looked up: whatever the underlying is, the most the strategy can lose is
/// the same.
#[tokio::test]
async fn an_option_strategy_is_judged_without_a_lookup() {
    let (server, client, _dir) = trader(json!([])).await;
    let vertical: OrderRequest = serde_json::from_value(json!({
        "orderStrategyType": "SINGLE", "orderType": "NET_DEBIT", "session": "NORMAL",
        "duration": "DAY", "price": "2",
        "orderLegCollection": [
            {"instruction": "BUY_TO_OPEN", "quantity": 1,
                "instrument": {"symbol": C100, "assetType": "OPTION"}},
            {"instruction": "SELL_TO_OPEN", "quantity": 1,
                "instrument": {"symbol": C105, "assetType": "OPTION"}}
        ]
    }))
    .unwrap();
    client.check_risk(ACCOUNT, &vertical, None).await.unwrap();
    assert_eq!(
        requests(&server, |r| r.url.path() == "/marketdata/v1/quotes").await,
        0
    );
}

/// A caller that reserves through the provider directly, skipping the
/// screen that reads the instrument type, is refused a buy: the provider
/// never assumes a symbol is stock. Once screened, the same buy is
/// reserved, and it is still sent if its type expires before it leaves.
#[tokio::test]
async fn a_buy_is_reserved_only_once_its_type_is_read() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    let database = client.order_journal().database().clone();
    database
        .reconcile_snapshot(
            ACCOUNT,
            1,
            chrono::Utc::now().timestamp_millis(),
            json!({"securitiesAccount": {"currentBalances": {"availableFunds": "1000"},
                "positions": []}}),
            vec![],
        )
        .await
        .unwrap();
    let limits = PortfolioLimits {
        max_open_quantity: 10.into(),
        max_open_notional: 1000.into(),
        max_position_quantity: 10.into(),
        max_snapshot_age_ms: 60_000,
        max_daily_loss: None,
        option_multipliers: Default::default(),
    };
    let risk = PortfolioRisk::new(database, HashMap::from([(ACCOUNT.into(), limits)]));
    quoted(&server, "AAPL", quote("AAPL", Some("EQUITY"), Some("COE"))).await;
    let buy = order("BUY", "EQUITY", "AAPL", 1);
    let unread = "Built-in portfolio risk has no current instrument type from Schwab for \
        this symbol; screen the order first";
    for refused in [
        risk.reserve(ACCOUNT, "unscreened", &buy).await,
        risk.check(ACCOUNT, &buy, None).await,
    ] {
        assert!(
            matches!(&refused, Err(Error::RiskDenied(r)) if r == unread),
            "{refused:?}"
        );
    }
    risk.screen(ACCOUNT, &buy, &client).await.unwrap();
    risk.check(ACCOUNT, &buy, None).await.unwrap();
    risk.reserve(ACCOUNT, "screened", &buy).await.unwrap();
    // The recheck just before sending judges the baseline and the held
    // reservation, which an expired type does not change.
    age_instrument_type(&client, "AAPL", 3_600_001);
    risk.revalidate(ACCOUNT, "screened", &buy).await.unwrap();
    let expired = risk.reserve(ACCOUNT, "expired", &buy).await;
    assert!(
        matches!(&expired, Err(Error::RiskDenied(r)) if r == unread),
        "{expired:?}"
    );
}
