mod common;

use rust_decimal::Decimal;
use rustyschwab::{Error, broker::TradingBackend, paper::*, types::trading::OrderRequest};
use serde_json::json;
use std::time::Duration;
fn d(v: &str) -> Decimal {
    v.parse().unwrap()
}
fn config() -> PaperConfig {
    serde_json::from_value(json!({
        "model_version": MODEL_VERSION,
        "accounts": {
            "paper-test": {
                "initial_cash": "1000",
                "limits": {
                    "max_open_quantity": "100",
                    "max_open_notional": "10000",
                    "max_position_quantity": "100",
                    "max_snapshot_age_ms": 5000,
                },
            },
        },
        "execution_latency_ms": 10,
        "cancel_latency_ms": 100,
        "quote_max_age_ms": 5000,
        "slippage_bps": "0",
        "market_collar_bps": "500",
        "fee_per_share": "0.01",
        "max_working_orders": 100,
    }))
    .unwrap()
}
fn order(tag: &str, side: &str, q: &str, price: &str) -> OrderRequest {
    serde_json::from_value(json!({
        "tag": tag,
        "orderStrategyType": "SINGLE",
        "orderType": "LIMIT",
        "session": "NORMAL",
        "duration": "DAY",
        "price": price,
        "orderLegCollection": [{
            "quantity": q,
            "instruction": side,
            "instrument": {"symbol": "ABC", "assetType": "EQUITY"},
        }],
    }))
    .unwrap()
}
fn quote(sequence: u64, at: i64, bid: &str, ask: &str, size: &str) -> PaperEvent {
    PaperEvent {
        sequence,
        at_ms: at,
        kind: PaperEventKind::Quote {
            symbol: "ABC".into(),
            source_at_ms: at,
            bid: d(bid),
            ask: d(ask),
            bid_size: d(size),
            ask_size: d(size),
        },
    }
}
async fn setup() -> (tempfile::TempDir, PaperBroker) {
    let dir = tempfile::tempdir().unwrap();
    let broker = PaperBroker::open(dir.path().join("state/paper.sqlite3"), config())
        .await
        .unwrap();
    broker
        .apply(PaperEvent {
            sequence: 1,
            at_ms: 0,
            kind: PaperEventKind::Session {
                session_id: "session-1".into(),
                open: true,
            },
        })
        .await
        .unwrap();
    broker
        .apply(quote(2, 100, "10", "10.01", "100"))
        .await
        .unwrap();
    (dir, broker)
}
/// Inject a storage fault: a trigger that aborts the next statement matching `when`.
async fn install_fault(broker: &PaperBroker, name: &str, when: &str, message: &str) {
    let ddl = format!("CREATE TRIGGER {name} {when} BEGIN SELECT RAISE(ABORT,'{message}'); END;");
    broker
        .order_journal()
        .database()
        .run(move |c| {
            c.execute_batch(&ddl)
                .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
        .unwrap();
}
#[tokio::test]
async fn notional_and_sell_only_fees_are_charged_and_reserved() {
    let mut value = serde_json::to_value(config()).unwrap();
    value["fee_per_share"] = json!("0");
    value["fee_bps"] = json!("10");
    value["sell_fee_bps"] = json!("0.5");
    value["sell_fee_per_share"] = json!("0.001");
    value["sell_fee_per_share_cap"] = json!("0.005");
    let fees: PaperConfig = serde_json::from_value(value).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let b = PaperBroker::open(dir.path().join("state/paper.sqlite3"), fees.clone())
        .await
        .unwrap();
    b.apply(PaperEvent {
        sequence: 1,
        at_ms: 0,
        kind: PaperEventKind::Session {
            session_id: "session-1".into(),
            open: true,
        },
    })
    .await
    .unwrap();
    b.apply(quote(2, 100, "10", "10", "100")).await.unwrap();
    b.place_order("paper-test", &order("buy", "BUY", "10", "11"))
        .await
        .unwrap();
    // Reserved: 10 shares at the 11 bound plus 10 bps of it.
    assert_eq!(
        b.snapshot("paper-test").await.unwrap().reserved_cash,
        d("110.11")
    );
    let bought = b.apply(quote(3, 200, "10", "10", "100")).await.unwrap();
    // 10 bps of the 100 notional; no sell-only fees on a buy.
    assert_eq!(bought.fills[0].fee, d("0.1"));
    b.place_order("paper-test", &order("sell", "SELL", "10", "10"))
        .await
        .unwrap();
    let sold = b.apply(quote(4, 300, "10", "10", "100")).await.unwrap();
    // 10 bps + 0.5 bps of 100, plus 10 x 0.001 capped at 0.005.
    assert_eq!(sold.fills[0].fee, d("0.110"));
    let snapshot = b.snapshot("paper-test").await.unwrap();
    assert_eq!(snapshot.ledger.fees, d("0.210"));
    assert_eq!(snapshot.ledger.realized_pnl, d("-0.210"));
    // The fee fields round-trip, and a config without them is unchanged.
    assert_eq!(
        serde_json::to_value(&fees).unwrap()["sell_fee_per_share_cap"],
        json!("0.005")
    );
    assert!(
        serde_json::to_value(config())
            .unwrap()
            .get("fee_bps")
            .is_none()
    );
    let mut bad = serde_json::to_value(config()).unwrap();
    bad["sell_fee_bps"] = json!("101");
    let bad: PaperConfig = serde_json::from_value(bad).unwrap();
    assert!(
        PaperBroker::open(dir.path().join("state/bad.sqlite3"), bad)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn a_batch_applies_like_single_events_or_not_at_all() {
    let (_single_dir, single) = setup().await;
    let (_batch_dir, batch) = setup().await;
    for b in [&single, &batch] {
        b.place_order("paper-test", &order("buy", "BUY", "3", "11"))
            .await
            .unwrap();
    }
    let events = vec![
        quote(3, 200, "10", "10.01", "1"),
        quote(4, 300, "10", "10.02", "1"),
        quote(5, 400, "10", "10.03", "5"),
    ];
    let mut one_by_one = Vec::new();
    for event in events.clone() {
        one_by_one.push(single.apply(event).await.unwrap());
    }
    let batched = batch.apply_batch(events.clone()).await.unwrap();
    let fills = |results: &[PaperEventResult]| {
        results
            .iter()
            .flat_map(|r| r.fills.iter().map(|f| (f.quantity, f.price)))
            .collect::<Vec<_>>()
    };
    assert_eq!(fills(&batched), fills(&one_by_one));
    assert_eq!(
        batch.snapshot("paper-test").await.unwrap().ledger.cash,
        single.snapshot("paper-test").await.unwrap().ledger.cash
    );
    assert!(
        batch.apply_batch(events).await.unwrap()[0].duplicate,
        "an exact retry is idempotent"
    );

    // The third event is out of order, so none of the three is applied.
    let broken = vec![
        quote(6, 500, "10", "10.01", "1"),
        quote(7, 600, "10", "10.01", "1"),
        quote(9, 700, "10", "10.01", "1"),
    ];
    assert!(batch.apply_batch(broken).await.is_err());
    let next = batch
        .apply(quote(6, 500, "10", "10.01", "1"))
        .await
        .unwrap();
    assert!(!next.duplicate, "the failed batch left no event behind");
    assert_eq!(batch.verify_integrity().await.unwrap().event_sequence, 6);
}
#[tokio::test]
async fn partial_fills_reservations_cost_basis_and_realized_pnl_survive_restart() {
    let (dir, b) = setup().await;
    let id = b
        .place_order("paper-test", &order("buy", "BUY", "2", "11"))
        .await
        .unwrap()
        .order_id
        .unwrap();
    assert_eq!(
        b.snapshot("paper-test").await.unwrap().reserved_cash,
        d("22.02")
    );
    let event = quote(3, 200, "10", "10.01", "1");
    let fill = b.apply(event.clone()).await.unwrap();
    assert_eq!(fill.fills[0].quantity, d("1"));
    assert!(b.apply(event).await.unwrap().duplicate);
    assert_eq!(
        b.snapshot("paper-test").await.unwrap().ledger.cash,
        d("989.98")
    );
    drop(b);
    let b = PaperBroker::open(dir.path().join("state/paper.sqlite3"), config())
        .await
        .unwrap();
    b.apply(quote(4, 300, "10", "10.01", "1")).await.unwrap();
    assert_eq!(
        b.get_order("paper-test", &id).await.unwrap().status,
        PaperOrderStatus::Filled
    );
    b.place_order("paper-test", &order("sell", "SELL", "1", "11"))
        .await
        .unwrap();
    b.apply(quote(5, 400, "12", "12.01", "1")).await.unwrap();
    let snapshot = b.snapshot("paper-test").await.unwrap();
    assert_eq!(snapshot.ledger.cash, d("991.95"));
    assert_eq!(snapshot.ledger.realized_pnl, d("1.97"));
    assert_eq!(snapshot.ledger.positions["ABC"].cost_basis, d("10.02"));
    assert_eq!(snapshot.unrealized_pnl, Some(d("1.98")));
    assert_eq!(snapshot.equity, Some(d("1003.95")));
    assert_eq!(snapshot.reserved_cash, Decimal::ZERO);
}
#[tokio::test]
async fn fills_and_the_venue_quote_are_readable_per_order_and_symbol() {
    let (_dir, b) = setup().await;
    let seen = b.quote("ABC").await.unwrap().unwrap();
    assert_eq!(
        (seen.bid, seen.ask, seen.source_at_ms, seen.fresh),
        (d("10"), d("10.01"), 100, true)
    );
    assert_eq!(b.quote("XYZ").await.unwrap(), None);
    let id = b
        .place_order("paper-test", &order("buy", "BUY", "2", "11"))
        .await
        .unwrap()
        .order_id
        .unwrap();
    assert!(b.fills("paper-test", &id).await.unwrap().is_empty());
    b.apply(quote(3, 200, "10", "10.01", "1")).await.unwrap();
    b.apply(quote(4, 300, "10.02", "10.03", "1")).await.unwrap();
    let fills: Vec<_> = b
        .fills("paper-test", &id)
        .await
        .unwrap()
        .into_iter()
        .map(|f| (f.event_sequence, f.quantity, f.price))
        .collect();
    assert_eq!(fills, [(3, d("1"), d("10.01")), (4, d("1"), d("10.03"))]);
    // Unknown orders and accounts answer like Schwab's 404, not as bad input.
    for (account, order_id) in [("paper-test", "999"), ("other", id.as_str())] {
        let error = b.fills(account, order_id).await.unwrap_err();
        assert!(
            matches!(&error, Error::Broker { status, details, .. }
                if status.as_u16() == 404
                    && details[0].title.as_deref() == Some(PAPER_NOT_FOUND_TITLE)),
            "{error:?}"
        );
    }
    assert!(matches!(
        b.snapshot("other").await,
        Err(Error::Broker { status, .. }) if status.as_u16() == 404
    ));
    // The clock moving past the quote's age limit keeps it readable but stale.
    b.apply(PaperEvent {
        sequence: 5,
        at_ms: 20_000,
        kind: PaperEventKind::Clock,
    })
    .await
    .unwrap();
    let stale = b.quote("ABC").await.unwrap().unwrap();
    assert_eq!(
        (stale.ask, stale.source_at_ms, stale.fresh),
        (d("10.03"), 300, false)
    );
}
#[tokio::test]
async fn finite_liquidity_is_shared_and_only_later_eligible_events_fill() {
    let (_dir, b) = setup().await;
    let a = b
        .place_order("paper-test", &order("one", "BUY", "2", "11"))
        .await
        .unwrap()
        .order_id
        .unwrap();
    let z = b
        .place_order("paper-test", &order("two", "BUY", "2", "11"))
        .await
        .unwrap()
        .order_id
        .unwrap();
    assert!(
        b.apply(quote(3, 105, "10", "10.01", "10"))
            .await
            .unwrap()
            .fills
            .is_empty()
    );
    let fills = b
        .apply(quote(4, 111, "10", "10.01", "3"))
        .await
        .unwrap()
        .fills;
    assert_eq!(fills.iter().map(|f| f.quantity).sum::<Decimal>(), d("3"));
    assert_eq!(fills[0].order_id, a);
    assert_eq!(
        b.get_order("paper-test", &z)
            .await
            .unwrap()
            .remaining_quantity,
        d("1")
    );
    // Advancing the clock does not replenish liquidity.
    b.apply(PaperEvent {
        sequence: 5,
        at_ms: 500,
        kind: PaperEventKind::Clock,
    })
    .await
    .unwrap();
    assert_eq!(
        b.get_order("paper-test", &z)
            .await
            .unwrap()
            .remaining_quantity,
        d("1")
    );
}
#[tokio::test]
async fn delayed_cancel_allows_fill_race_but_cancels_at_deadline() {
    let (_dir, b) = setup().await;
    let a = b
        .place_order("paper-test", &order("race", "BUY", "2", "11"))
        .await
        .unwrap()
        .order_id
        .unwrap();
    b.cancel_order("paper-test", &a).await.unwrap();
    b.apply(quote(3, 150, "10", "10.01", "1")).await.unwrap();
    let end = b.apply(quote(4, 200, "10", "10.01", "100")).await.unwrap();
    assert!(end.fills.is_empty());
    let order = b.get_order("paper-test", &a).await.unwrap();
    assert_eq!(order.status, PaperOrderStatus::Canceled);
    assert_eq!(order.filled_quantity, d("1"));
    assert_eq!(
        b.snapshot("paper-test").await.unwrap().reserved_cash,
        Decimal::ZERO
    );
}
#[tokio::test]
async fn replacement_failure_rolls_back_old_order_and_success_loses_priority() {
    let (_dir, b) = setup().await;
    let a = b
        .place_order("paper-test", &order("old", "BUY", "2", "11"))
        .await
        .unwrap()
        .order_id
        .unwrap();
    assert!(
        b.replace_order("paper-test", &a, &order("bad", "BUY", "100", "11"))
            .await
            .is_err()
    );
    assert_eq!(
        b.get_order("paper-test", &a).await.unwrap().status,
        PaperOrderStatus::Working
    );
    let z = b
        .replace_order("paper-test", &a, &order("new", "BUY", "1", "11"))
        .await
        .unwrap()
        .order_id
        .unwrap();
    assert_eq!(
        b.get_order("paper-test", &a).await.unwrap().status,
        PaperOrderStatus::Replaced
    );
    assert_eq!(
        b.get_order("paper-test", &z).await.unwrap().replaces,
        Some(a)
    );
    assert_eq!(
        b.snapshot("paper-test").await.unwrap().reserved_cash,
        d("11.01")
    );
}
#[tokio::test]
async fn concurrent_admission_cannot_double_spend_and_tags_are_durable() {
    let (dir, b) = setup().await;
    let one = order("concurrent-1", "BUY", "60", "11");
    let two = order("concurrent-2", "BUY", "60", "11");
    let (a, z) = tokio::join!(
        b.place_order("paper-test", &one),
        b.place_order("paper-test", &two)
    );
    assert_ne!(a.is_ok(), z.is_ok());
    assert!(matches!(
        a.as_ref().and(z.as_ref()),
        Err(Error::RiskDenied(_))
    ));
    let accepted = if a.is_ok() { one } else { two };
    let reopened = PaperBroker::open(dir.path().join("state/paper.sqlite3"), config())
        .await
        .unwrap();
    assert!(matches!(
        reopened.place_order("paper-test", &accepted).await,
        Err(Error::Duplicate(_))
    ));
    assert_eq!(
        reopened.orders("paper-test", 0, 100).await.unwrap().len(),
        1
    );
}
#[tokio::test]
async fn unknown_ack_is_reconciled_by_durable_order_without_resubmission() {
    let (_dir, b) = setup().await;
    b.apply(PaperEvent {
        sequence: 3,
        at_ms: 100,
        kind: PaperEventKind::Fault {
            fault: PaperFault::LoseNextAcknowledgement,
        },
    })
    .await
    .unwrap();
    let request = order("lost", "BUY", "1", "11");
    assert!(matches!(
        b.place_order("paper-test", &request).await,
        Err(Error::SubmissionUnknown { .. })
    ));
    assert_eq!(b.orders("paper-test", 0, 100).await.unwrap().len(), 1);
    assert!(matches!(
        b.place_order("paper-test", &request).await,
        Err(Error::Duplicate(_))
    ));
    b.place_order("paper-test", &order("next", "BUY", "1", "11"))
        .await
        .unwrap();
    b.apply(PaperEvent {
        sequence: 4,
        at_ms: 100,
        kind: PaperEventKind::Fault {
            fault: PaperFault::RejectNext,
        },
    })
    .await
    .unwrap();
    // Venue rejections carry the same shape as a live broker's 400.
    let rejection = b
        .place_order("paper-test", &order("reject", "BUY", "1", "11"))
        .await
        .unwrap_err();
    assert!(matches!(
        &rejection,
        Error::Broker { status, details, .. }
            if *status == reqwest::StatusCode::BAD_REQUEST
                && details[0].title.as_deref() == Some(PAPER_REJECTION_TITLE)
                && details[0].detail.as_deref() == Some("Injected paper rejection")
    ));
    assert!(!rejection.is_retryable());
    assert_eq!(b.orders("paper-test", 0, 100).await.unwrap().len(), 2);
    // A rejection holds nothing at the venue, so the tag carries the retry.
    b.place_order("paper-test", &order("reject", "BUY", "1", "11"))
        .await
        .unwrap();
    assert_eq!(b.orders("paper-test", 0, 100).await.unwrap().len(), 3);
}
#[tokio::test]
async fn halt_and_stale_fencing_deny_submissions_but_not_cancellation() {
    let (_dir, b) = setup().await;
    let lease = b
        .coordinator()
        .acquire("gateway", "owner", Duration::from_secs(30))
        .await
        .unwrap();
    let id = b
        .place_order_fenced("paper-test", &order("held", "BUY", "1", "11"), &lease)
        .await
        .unwrap()
        .order_id
        .unwrap();
    b.coordinator().release(&lease).await.unwrap();
    assert!(matches!(
        b.place_order_fenced("paper-test", &order("stale", "BUY", "1", "11"), &lease)
            .await,
        Err(Error::OwnershipLost)
    ));
    b.halt_submissions("test", "operator").await.unwrap();
    assert!(matches!(
        b.place_order("paper-test", &order("halt", "BUY", "1", "11"))
            .await,
        Err(Error::Halted(_))
    ));
    b.cancel_order("paper-test", &id).await.unwrap();
    b.apply(quote(3, 250, "10", "10.01", "1")).await.unwrap();
    assert!(b.operator_halt().await.unwrap().is_some());
    b.clear_operator_halt("operator").await.unwrap();
    b.place_order("paper-test", &order("resume", "BUY", "1", "11"))
        .await
        .unwrap();
}
#[tokio::test]
async fn event_sequence_and_quote_freshness_fail_closed() {
    let (_dir, b) = setup().await;
    assert!(b.apply(quote(4, 200, "10", "11", "1")).await.is_err());
    assert!(b.apply(quote(3, 99, "10", "11", "1")).await.is_err());
    assert!(b.apply(quote(3, 100, "10", "11", "1")).await.is_err());
    assert!(b.apply(quote(3, 200, "12", "11", "1")).await.is_err());
    assert_eq!(b.snapshot("paper-test").await.unwrap().event_sequence, 2);
    b.apply(PaperEvent {
        sequence: 3,
        at_ms: 10000,
        kind: PaperEventKind::Clock,
    })
    .await
    .unwrap();
    assert!(matches!(
        b.place_order("paper-test", &order("stale", "BUY", "1", "11"))
            .await,
        Err(Error::Broker { .. })
    ));
    b.apply(PaperEvent {
        sequence: 4,
        at_ms: 10000,
        kind: PaperEventKind::Disconnect,
    })
    .await
    .unwrap();
    assert!(matches!(
        b.place_order("paper-test", &order("gap", "BUY", "1", "11"))
            .await,
        Err(Error::Broker { .. })
    ));
}
#[tokio::test]
async fn day_expiry_market_collar_and_unsupported_features_are_explicit() {
    let (_dir, b) = setup().await;
    let mut market = order("market", "BUY", "1", "11");
    market.order_type = Some(rustyschwab::types::trading::OrderType::Market);
    market.price = None;
    let id = b
        .place_order("paper-test", &market)
        .await
        .unwrap()
        .order_id
        .unwrap();
    b.apply(quote(3, 200, "20", "21", "1")).await.unwrap();
    assert_eq!(
        b.get_order("paper-test", &id).await.unwrap().status,
        PaperOrderStatus::Rejected
    );
    let id = b
        .place_order("paper-test", &order("day", "BUY", "1", "11"))
        .await
        .unwrap()
        .order_id
        .unwrap();
    b.apply(PaperEvent {
        sequence: 4,
        at_ms: 300,
        kind: PaperEventKind::Session {
            session_id: "session-1".into(),
            open: false,
        },
    })
    .await
    .unwrap();
    assert_eq!(
        b.get_order("paper-test", &id).await.unwrap().status,
        PaperOrderStatus::Expired
    );
    let mut unsupported = order("unsupported", "BUY", "1", "11");
    unsupported.tax_lot_method = Some(rustyschwab::types::trading::TaxLotMethod::Fifo);
    assert!(b.place_order("paper-test", &unsupported).await.is_err());
    assert!(
        b.place_order("paper-test", &order("short", "SELL", "1", "11"))
            .await
            .is_err()
    );
}
#[tokio::test]
async fn model_configuration_and_state_modes_cannot_be_swapped() {
    let (dir, b) = setup().await;
    let mut changed = config();
    changed.fee_per_share = d("0.02");
    assert!(
        PaperBroker::open(dir.path().join("state/paper.sqlite3"), changed)
            .await
            .is_err()
    );
    let mut live = rustyschwab::SchwabConfig::default();
    live.oauth.app_key = "k".repeat(32);
    live.oauth.app_secret = "s".repeat(16);
    live.oauth.capture_callback = false;
    live.oauth.auto_refresh = false;
    // The live journal derivation resolves to the paper database.
    live.oauth.tokens_file = dir.path().join("tokens.json");
    let client = rustyschwab::SchwabClient::new(live).unwrap();
    b.order_journal()
        .database()
        .backup(client.order_journal().path().to_owned())
        .await
        .unwrap();
    assert!(matches!(
        client.get_account("paper-test", true).await,
        Err(Error::Config(_))
    ));
}
#[tokio::test]
async fn deterministic_replay_matches_across_databases_and_backup_restore() {
    let (dir, a) = setup().await;
    let (_other, b) = setup().await;
    for broker in [&a, &b] {
        broker
            .place_order("paper-test", &order("same", "BUY", "3", "11"))
            .await
            .unwrap();
        for n in 3..10 {
            broker
                .apply(quote(n, n as i64 * 100, "10", "10.01", "1"))
                .await
                .unwrap();
        }
    }
    assert_eq!(
        serde_json::to_value(a.snapshot("paper-test").await.unwrap()).unwrap(),
        serde_json::to_value(b.snapshot("paper-test").await.unwrap()).unwrap()
    );
    let backup = dir.path().join("backup/backup.sqlite3");
    a.order_journal()
        .database()
        .backup(backup.clone())
        .await
        .unwrap();
    let restored = rustyschwab::storage::StateDatabase::restore(
        backup,
        dir.path().join("restored/restored.sqlite3"),
    )
    .await
    .unwrap();
    let restored = PaperBroker::open(restored.path().to_owned(), config())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(a.snapshot("paper-test").await.unwrap()).unwrap(),
        serde_json::to_value(restored.snapshot("paper-test").await.unwrap()).unwrap()
    );
}

#[tokio::test]
async fn failed_event_rolls_back_all_fills_and_remains_replayable() {
    let (_dir, b) = setup().await;
    b.place_order("paper-test", &order("atomic-one", "BUY", "1", "11"))
        .await
        .unwrap();
    b.place_order("paper-test", &order("atomic-two", "BUY", "1", "11"))
        .await
        .unwrap();
    install_fault(
        &b,
        "fail_second_fill",
        "BEFORE UPDATE ON paper_accounts WHEN json_extract(NEW.payload,'$.cash')='979.96'",
        "injected storage fault",
    )
    .await;
    let event = quote(3, 200, "10", "10.01", "2");
    assert!(b.apply(event.clone()).await.is_err());
    let state = b.snapshot("paper-test").await.unwrap();
    assert_eq!(state.ledger.cash, d("1000"));
    assert_eq!(state.event_sequence, 2);
    assert_eq!(state.reserved_cash, d("22.02"));
    b.order_journal()
        .database()
        .run(|c| {
            c.execute_batch("DROP TRIGGER fail_second_fill")
                .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(b.apply(event).await.unwrap().fills.len(), 2);
}

#[tokio::test]
async fn wall_clock_mode_rejects_stopped_feeds_without_a_clock_event() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config();
    config.clock = PaperClockMode::Wall;
    config.quote_max_age_ms = 500;
    let b = PaperBroker::open(dir.path().join("state/paper.sqlite3"), config)
        .await
        .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    b.apply(PaperEvent {
        sequence: 1,
        at_ms: now,
        kind: PaperEventKind::Session {
            session_id: "wall-session".into(),
            open: true,
        },
    })
    .await
    .unwrap();
    b.apply(quote(
        2,
        chrono::Utc::now().timestamp_millis(),
        "10",
        "10.01",
        "1",
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        b.place_order("paper-test", &order("stopped", "BUY", "1", "11"))
            .await
            .is_err()
    );
    assert!(
        !b.readiness(&["paper-test".into()], 500)
            .await
            .unwrap()
            .trading_ready
    );
}
#[tokio::test]
async fn disconnect_does_not_reset_consumed_quote_watermark() {
    let (_dir, b) = setup().await;
    b.apply(PaperEvent {
        sequence: 3,
        at_ms: 100,
        kind: PaperEventKind::Disconnect,
    })
    .await
    .unwrap();
    assert!(b.apply(quote(4, 100, "10", "10.01", "100")).await.is_err());
    b.apply(quote(4, 101, "10", "10.01", "100")).await.unwrap();
    b.place_order("paper-test", &order("fresh", "BUY", "1", "11"))
        .await
        .unwrap();
}

#[tokio::test]
async fn storage_failures_are_not_reported_as_conclusive_submission_rejections() {
    let (_dir, b) = setup().await;
    install_fault(
        &b,
        "fail_accept",
        "BEFORE INSERT ON paper_orders",
        "injected acceptance fault",
    )
    .await;
    assert!(matches!(
        b.place_order("paper-test", &order("storage", "BUY", "1", "11"))
            .await,
        Err(Error::SubmissionUnknown { .. })
    ));
    assert!(b.orders("paper-test", 0, 100).await.unwrap().is_empty());
    assert_eq!(
        b.snapshot("paper-test").await.unwrap().available_cash,
        d("1000")
    );
}

#[tokio::test]
async fn diligence_extreme_quote_timestamps_are_validation_errors_not_worker_panics() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.clock = PaperClockMode::Wall;
    let b = PaperBroker::open(dir.path().join("state/paper.sqlite3"), cfg)
        .await
        .unwrap();
    b.apply(PaperEvent {
        sequence: 1,
        at_ms: chrono::Utc::now().timestamp_millis(),
        kind: PaperEventKind::Session {
            session_id: "wall".into(),
            open: true,
        },
    })
    .await
    .unwrap();
    for source in [i64::MIN, -1, i64::MAX] {
        let mut event = quote(2, chrono::Utc::now().timestamp_millis(), "10", "11", "1");
        if let PaperEventKind::Quote { source_at_ms, .. } = &mut event.kind {
            *source_at_ms = source;
        }
        let error = b.apply(event).await.unwrap_err();
        assert!(matches!(error, Error::InvalidParameter(_)), "{error:?}");
    }
}

#[tokio::test]
async fn diligence_reopen_never_recredits_a_missing_account() {
    let (dir, b) = setup().await;
    b.place_order("paper-test", &order("paid", "BUY", "1", "11"))
        .await
        .unwrap();
    b.apply(quote(3, 200, "10", "10.01", "1")).await.unwrap();
    b.order_journal()
        .database()
        .run(|c| {
            c.execute("DELETE FROM paper_accounts", [])
                .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        PaperBroker::open(dir.path().join("state/paper.sqlite3"), config())
            .await
            .is_err()
    );
    let count = b
        .order_journal()
        .database()
        .run(|c| {
            Ok(c.query_row("SELECT COUNT(*) FROM paper_accounts", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap())
        })
        .await
        .unwrap();
    assert_eq!(count, 0, "Missing state must not be silently recreated");
}

#[tokio::test]
async fn diligence_reopen_rejects_missing_event_history() {
    let (dir, b) = setup().await;
    b.order_journal()
        .database()
        .run(|c| {
            c.execute_batch("DROP TABLE paper_events")
                .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        PaperBroker::open(dir.path().join("state/paper.sqlite3"), config())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn diligence_snapshot_does_not_hide_storage_errors_as_missing_marks() {
    let (_dir, b) = setup().await;
    b.place_order("paper-test", &order("mark", "BUY", "1", "11"))
        .await
        .unwrap();
    b.apply(quote(3, 200, "10", "10.01", "1")).await.unwrap();
    b.order_journal()
        .database()
        .run(|c| {
            c.execute_batch("DROP TABLE paper_quotes")
                .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(matches!(
        b.snapshot("paper-test").await,
        Err(Error::Storage(_))
    ));
}

#[tokio::test]
async fn diligence_reopen_rejects_holes_in_event_history() {
    let (dir, b) = setup().await;
    b.order_journal()
        .database()
        .run(|c| {
            c.execute("DELETE FROM paper_events WHERE sequence=1", [])
                .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        PaperBroker::open(dir.path().join("state/paper.sqlite3"), config())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn diligence_missing_deduplication_tables_cannot_be_recreated() {
    let (dir, b) = setup().await;
    b.place_order("paper-test", &order("dedup", "BUY", "1", "11"))
        .await
        .unwrap();
    b.order_journal()
        .database()
        .run(|c| {
            c.execute_batch("DROP TABLE used_order_tags")
                .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        PaperBroker::open(dir.path().join("state/paper.sqlite3"), config())
            .await
            .is_err()
    );
    assert!(
        b.place_order("paper-test", &order("dedup", "BUY", "1", "11"))
            .await
            .is_err()
    );
}

async fn configured_setup(cfg: PaperConfig) -> (tempfile::TempDir, PaperBroker) {
    let dir = tempfile::tempdir().unwrap();
    let b = PaperBroker::open(dir.path().join("state/paper.sqlite3"), cfg)
        .await
        .unwrap();
    b.apply(PaperEvent {
        sequence: 1,
        at_ms: 0,
        kind: PaperEventKind::Session {
            session_id: "session-1".into(),
            open: true,
        },
    })
    .await
    .unwrap();
    b.apply(quote(2, 100, "10", "10.01", "100")).await.unwrap();
    (dir, b)
}
#[tokio::test]
async fn settlement_proceeds_are_unavailable_until_explicit_calendar_time() {
    let mut cfg = config();
    cfg.settlement_required = true;
    cfg.settlement_calendar.insert("session-1".into(), 1000);
    let (dir, b) = configured_setup(cfg.clone()).await;
    b.place_order("paper-test", &order("buy", "BUY", "2", "11"))
        .await
        .unwrap();
    b.apply(quote(3, 200, "10", "10.01", "2")).await.unwrap();
    b.place_order("paper-test", &order("sell", "SELL", "1", "11"))
        .await
        .unwrap();
    b.apply(quote(4, 300, "12", "12.01", "1")).await.unwrap();
    let state = b.snapshot("paper-test").await.unwrap();
    assert_eq!(state.ledger.cash, d("979.96"));
    assert_eq!(state.unsettled_cash, d("11.99"));
    assert!(
        b.place_order("paper-test", &order("unsettled", "BUY", "90", "11"))
            .await
            .is_err()
    );
    let b = PaperBroker::open(dir.path().join("state/paper.sqlite3"), cfg)
        .await
        .unwrap();
    b.apply(PaperEvent {
        sequence: 5,
        at_ms: 1000,
        kind: PaperEventKind::Clock,
    })
    .await
    .unwrap();
    assert_eq!(
        b.snapshot("paper-test").await.unwrap().unsettled_cash,
        Decimal::ZERO
    );
    b.place_order("paper-test", &order("settled", "BUY", "90", "11"))
        .await
        .unwrap();
}
#[tokio::test]
async fn daily_loss_past_the_cap_trips_the_halt_until_a_new_session() {
    let mut cfg = config();
    cfg.accounts
        .get_mut("paper-test")
        .unwrap()
        .limits
        .max_daily_loss = Some(d("5"));
    let (_dir, b) = configured_setup(cfg).await;
    // Nothing held at the session open, so the open itself is the mark.
    let start = b.snapshot("paper-test").await.unwrap();
    assert_eq!(
        start.ledger.session_mark,
        Some(SessionMark {
            session_id: "session-1".into(),
            equity: d("1000"),
        })
    );
    assert_eq!(start.daily_loss, Some(Decimal::ZERO));
    // Ten shares at 10.01 plus fees, marked at the 10 bid: 0.20 down.
    b.place_order("paper-test", &order("buy", "BUY", "10", "11"))
        .await
        .unwrap();
    b.apply(quote(3, 200, "10", "10.01", "10")).await.unwrap();
    assert_eq!(
        b.snapshot("paper-test").await.unwrap().daily_loss,
        Some(d("0.20"))
    );
    assert!(b.operator_halt().await.unwrap().is_none());
    // The bid falls to 9.40: 6.20 down, past the 5 cap.
    b.apply(quote(4, 300, "9.40", "9.41", "10")).await.unwrap();
    let reason = b.operator_halt().await.unwrap().expect("halt tripped");
    assert!(
        reason.starts_with("Daily loss 6.20 exceeds the 5 limit for account paper-test"),
        "{reason}"
    );
    assert!(matches!(
        b.place_order("paper-test", &order("more", "BUY", "1", "11"))
            .await,
        Err(Error::Halted(_))
    ));
    let audit = b.order_journal().audit_page(0, 100).await.unwrap();
    let trips: Vec<_> = audit
        .iter()
        .filter(|entry| entry.kind == "operator_halt_changed")
        .collect();
    assert_eq!(trips.len(), 1);
    assert_eq!(trips[0].subject, "daily-loss-control");
    assert_eq!(trips[0].payload["reason"], reason);
    // Clearing the halt does not reset the day: the next event trips it again,
    // and a standing operator halt keeps its own reason.
    b.clear_operator_halt("operator").await.unwrap();
    b.apply(quote(5, 400, "9.40", "9.41", "10")).await.unwrap();
    assert_eq!(b.operator_halt().await.unwrap().as_deref(), Some(&*reason));
    b.clear_operator_halt("operator").await.unwrap();
    b.halt_submissions("desk review", "operator").await.unwrap();
    b.apply(quote(6, 500, "9.40", "9.41", "10")).await.unwrap();
    assert_eq!(
        b.operator_halt().await.unwrap().as_deref(),
        Some("desk review")
    );
    // A new session marks afresh at its first complete mark; the halt stays
    // until an operator clears it.
    b.apply(PaperEvent {
        sequence: 7,
        at_ms: 600,
        kind: PaperEventKind::Session {
            session_id: "session-2".into(),
            open: true,
        },
    })
    .await
    .unwrap();
    assert_eq!(b.snapshot("paper-test").await.unwrap().daily_loss, None);
    b.apply(quote(8, 700, "9.40", "9.41", "10")).await.unwrap();
    let next = b.snapshot("paper-test").await.unwrap();
    assert_eq!(
        next.ledger.session_mark,
        Some(SessionMark {
            session_id: "session-2".into(),
            equity: d("993.80"),
        })
    );
    assert_eq!(next.daily_loss, Some(Decimal::ZERO));
    assert!(b.operator_halt().await.unwrap().is_some());
    b.clear_operator_halt("operator").await.unwrap();
    b.place_order("paper-test", &order("again", "BUY", "1", "11"))
        .await
        .unwrap();
}
#[tokio::test]
async fn dividend_entitlement_survives_sale_and_pays_exactly_once() {
    let (_dir, b) = setup().await;
    b.place_order("paper-test", &order("buy", "BUY", "2", "11"))
        .await
        .unwrap();
    b.apply(quote(3, 200, "10", "10.01", "2")).await.unwrap();
    let event = PaperEvent {
        sequence: 4,
        at_ms: 250,
        kind: PaperEventKind::Dividend {
            action_id: "div-1".into(),
            symbol: "ABC".into(),
            cash_per_share: d("0.50"),
            pay_at_ms: 1000,
        },
    };
    b.apply(event.clone()).await.unwrap();
    assert!(b.apply(event.clone()).await.unwrap().duplicate);
    assert_eq!(
        b.snapshot("paper-test").await.unwrap().unsettled_cash,
        d("1")
    );
    b.apply(quote(5, 300, "10", "10.01", "2")).await.unwrap();
    b.place_order("paper-test", &order("sell", "SELL", "2", "10"))
        .await
        .unwrap();
    b.apply(quote(6, 400, "10", "10.01", "2")).await.unwrap();
    b.apply(PaperEvent {
        sequence: 7,
        at_ms: 1000,
        kind: PaperEventKind::Clock,
    })
    .await
    .unwrap();
    let state = b.snapshot("paper-test").await.unwrap();
    assert_eq!(state.ledger.cash, d("1000.94"));
    assert_eq!(state.ledger.realized_pnl, d("0.94"));
    let mut duplicate = event;
    duplicate.sequence = 8;
    duplicate.at_ms = 1000;
    assert!(b.apply(duplicate).await.is_err());
}
#[tokio::test]
async fn reverse_split_preserves_basis_and_settles_fractional_entitlement() {
    let (_dir, b) = setup().await;
    b.place_order("paper-test", &order("buy", "BUY", "3", "11"))
        .await
        .unwrap();
    b.apply(quote(3, 200, "10", "10.01", "3")).await.unwrap();
    let id = b
        .place_order("paper-test", &order("rest", "BUY", "1", "9"))
        .await
        .unwrap()
        .order_id
        .unwrap();
    b.apply(PaperEvent {
        sequence: 4,
        at_ms: 300,
        kind: PaperEventKind::Split {
            action_id: "split-1".into(),
            symbol: "ABC".into(),
            numerator: 1,
            denominator: 2,
            cash_in_lieu_price: d("20"),
            pay_at_ms: 400,
        },
    })
    .await
    .unwrap();
    let state = b.snapshot("paper-test").await.unwrap();
    assert_eq!(state.ledger.positions["ABC"].quantity, d("1"));
    assert_eq!(state.ledger.positions["ABC"].cost_basis, d("20.04"));
    assert_eq!(state.unsettled_cash, d("10"));
    assert_eq!(
        b.get_order("paper-test", &id).await.unwrap().status,
        PaperOrderStatus::Canceled
    );
    assert!(state.equity.is_none());
    b.apply(PaperEvent {
        sequence: 5,
        at_ms: 400,
        kind: PaperEventKind::Clock,
    })
    .await
    .unwrap();
    assert_eq!(
        b.snapshot("paper-test").await.unwrap().ledger.cash,
        d("979.94")
    );
}
#[tokio::test]
async fn stop_limit_trigger_is_persistent_and_never_fills_on_trigger_event() {
    let (dir, b) = setup().await;
    let mut request = order("stop", "BUY", "1", "12");
    request.order_type = Some(rustyschwab::types::trading::OrderType::StopLimit);
    request.stop_price = Some(d("11"));
    let id = b
        .place_order("paper-test", &request)
        .await
        .unwrap()
        .order_id
        .unwrap();
    assert!(
        b.apply(quote(3, 200, "11", "11.01", "1"))
            .await
            .unwrap()
            .fills
            .is_empty()
    );
    assert!(b.get_order("paper-test", &id).await.unwrap().triggered);
    let b = PaperBroker::open(dir.path().join("state/paper.sqlite3"), config())
        .await
        .unwrap();
    assert!(
        b.apply(quote(4, 300, "13", "13.01", "1"))
            .await
            .unwrap()
            .fills
            .is_empty()
    );
    assert_eq!(
        b.apply(quote(5, 400, "11", "11.01", "1"))
            .await
            .unwrap()
            .fills
            .len(),
        1
    );
}
#[test]
fn calendar_respects_offsets_early_closes_and_unknown_dates() {
    let hours=serde_json::from_value(json!({"date":"2026-11-27","isOpen":true,"sessionHours":{"regularMarket":[{"start":"2026-11-27T09:30:00-05:00","end":"2026-11-27T13:00:00-05:00"}]}})).unwrap();
    let at = chrono::DateTime::parse_from_rfc3339("2026-11-27T12:59:59-05:00")
        .unwrap()
        .timestamp_millis();
    assert!(feed::regular_session_open(&hours, "2026-11-27", at).unwrap());
    assert!(!feed::regular_session_open(&hours, "2026-11-27", at + 1000).unwrap());
    assert!(feed::regular_session_open(&hours, "2026-11-28", at).is_err());
}
/// A holiday or weekend is a closed session, not a broken feed: Schwab keys
/// a closed day by market name rather than product code, and reading only
/// `EQ` disconnected the venue every day it was shut.
#[tokio::test]
async fn a_closed_calendar_day_closes_the_session_instead_of_faulting_the_feed() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let (client, _tokens_dir, _) = common::mock_client(&server, false);
    // The feed's day is New York's, which is not UTC's in the evening.
    let today = feed::trading_date(chrono::Utc::now()).to_string();
    Mock::given(method("GET"))
        .and(path("/marketdata/v1/markets/equity"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "equity": {"equity": {
                "date": today, "marketType": "EQUITY", "product": "EQ", "isOpen": false,
            }}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut config = config();
    config.clock = PaperClockMode::Wall;
    let b = PaperBroker::open(dir.path().join("state/paper.sqlite3"), config)
        .await
        .unwrap();
    let feed = feed::SchwabPaperFeed::new(
        std::sync::Arc::new(client),
        b.clone(),
        vec!["ABC".into()],
        d("100"),
    )
    .unwrap();
    feed.tick().await.unwrap();
    assert_eq!(b.market_session().await.unwrap(), (today, false));
    // The feed stops at the closed calendar rather than asking for quotes.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
#[test]
fn feed_requires_realtime_identity_side_timestamps_and_explicit_size_units() {
    let mut item:rustyschwab::types::market_data::QuoteItem=serde_json::from_value(json!({"symbol":"ABC","assetMainType":"EQUITY","realtime":true,"quote":{"bidPrice":10.25,"askPrice":10.26,"bidSize":2,"askSize":3,"bidTime":100,"askTime":101}})).unwrap();
    let event = feed::normalize_quote("ABC", &item, d("100")).unwrap();
    if let PaperEventKind::Quote {
        source_at_ms,
        bid_size,
        ask_size,
        ..
    } = event
    {
        assert_eq!(source_at_ms, 100);
        assert_eq!(bid_size, d("200"));
        assert_eq!(ask_size, d("300"));
    } else {
        panic!("expected quote");
    }
    item.realtime = Some(false);
    assert!(feed::normalize_quote("ABC", &item, d("100")).is_err());
}

#[tokio::test]
async fn integrity_verifies_fills_and_detects_unfunded_ledger_changes() {
    let (_dir, b) = setup().await;
    b.place_order("paper-test", &order("integrity-buy", "BUY", "3", "11"))
        .await
        .unwrap();
    b.apply(quote(3, 200, "10", "10.01", "3")).await.unwrap();
    b.place_order("paper-test", &order("integrity-sell", "SELL", "1", "10"))
        .await
        .unwrap();
    b.apply(quote(4, 300, "10", "10.01", "1")).await.unwrap();
    let report = b.verify_integrity().await.unwrap();
    assert_eq!(report.orders, 2);
    assert_eq!(report.event_sequence, 4);
    b.order_journal()
        .database()
        .run(|c| {
            c.execute(
                "UPDATE paper_accounts SET payload=json_set(payload,'$.cash','999999')",
                [],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(b.verify_integrity().await.is_err());
}

#[tokio::test]
async fn archived_events_preserve_retry_conflict_and_restore_semantics() {
    let (dir, b) = setup().await;
    b.place_order("paper-test", &order("archive-buy", "BUY", "2", "11"))
        .await
        .unwrap();
    let event = quote(3, 200, "10", "10.01", "1");
    let original = b.apply(event.clone()).await.unwrap();
    let backup = dir.path().join("archive/snapshot.sqlite3");
    let report = b.archive_events(backup.clone(), 3).await.unwrap();
    assert_eq!(report.compacted, 3);
    assert_eq!(report.sha256.len(), 64);
    let retried = b.apply(event.clone()).await.unwrap();
    assert!(retried.duplicate);
    assert_eq!(
        serde_json::to_value(retried.fills).unwrap(),
        serde_json::to_value(original.fills).unwrap()
    );
    assert!(b.apply(quote(3, 200, "9", "10", "1")).await.is_err());
    assert!(b.archive_events(backup.clone(), 3).await.is_err());
    b.verify_integrity().await.unwrap();
    let restored = rustyschwab::storage::StateDatabase::restore(
        backup,
        dir.path().join("restore/state.sqlite3"),
    )
    .await
    .unwrap();
    let restored = PaperBroker::open(restored.path().to_owned(), config())
        .await
        .unwrap();
    assert!(restored.apply(event).await.unwrap().duplicate);
    assert_eq!(
        serde_json::to_value(b.snapshot("paper-test").await.unwrap()).unwrap(),
        serde_json::to_value(restored.snapshot("paper-test").await.unwrap()).unwrap()
    );
    b.apply(quote(4, 300, "10", "10.01", "1")).await.unwrap();
    b.verify_integrity().await.unwrap();
}

#[tokio::test]
async fn required_settlement_calendar_denies_missing_session_without_accepting_order() {
    let mut cfg = config();
    cfg.settlement_required = true;
    let (_dir, b) = configured_setup(cfg).await;
    assert!(
        b.place_order("paper-test", &order("missing-calendar", "BUY", "1", "11"))
            .await
            .is_err()
    );
    assert_eq!(b.verify_integrity().await.unwrap().orders, 0);
}

#[tokio::test]
async fn event_attribution_commits_once_and_invalid_subjects_do_not_advance_state() {
    let (_dir, b) = setup().await;
    let event = quote(3, 200, "10", "10.01", "1");
    let source = PaperEventSource::Authority {
        subject: "quotes-desk".into(),
    };
    b.apply_from(event.clone(), source).await.unwrap();
    assert!(
        b.apply_from(
            event,
            PaperEventSource::Authority {
                subject: "retry-desk".into()
            }
        )
        .await
        .unwrap()
        .duplicate
    );
    let audit = b.order_journal().audit_page(0, 100).await.unwrap();
    let events: Vec<_> = audit
        .iter()
        .filter(|entry| entry.kind == "paper_event" && entry.subject == "3")
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload["provenance"]["subject"], "quotes-desk");
    assert!(
        b.apply_from(
            quote(4, 300, "10", "10.01", "1"),
            PaperEventSource::Authority {
                subject: "bad\nsubject".into()
            }
        )
        .await
        .is_err()
    );
    assert_eq!(b.snapshot("paper-test").await.unwrap().event_sequence, 3);
}

#[tokio::test]
async fn command_capture_failure_rolls_back_order_and_tag_acceptance() {
    let (_dir, broker) = setup().await;
    assert_eq!(broker.captured_command_count().await.unwrap(), 2);
    install_fault(
        &broker,
        "fail_capture",
        "BEFORE INSERT ON audit WHEN NEW.kind='paper_command'",
        "capture unavailable",
    )
    .await;
    let request = order("capture-atomic", "BUY", "1", "11");
    assert!(broker.place_order("paper-test", &request).await.is_err());
    assert_eq!(broker.verify_integrity().await.unwrap().orders, 0);
    assert_eq!(broker.captured_command_count().await.unwrap(), 2);
    broker
        .order_journal()
        .database()
        .run(|c| {
            c.execute_batch("DROP TRIGGER fail_capture")
                .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
        .unwrap();
    broker.place_order("paper-test", &request).await.unwrap();
    assert_eq!(broker.captured_command_count().await.unwrap(), 3);
}

#[tokio::test]
async fn synthetic_candle_quotes_drive_a_replay_venue_and_fill_within_the_bar() {
    use rustyschwab::{
        paper::synthetic::{SYNTHETIC_MODEL, SyntheticBbo},
        types::market_data::{Candle, PriceHistoryResponse},
    };
    let dir = tempfile::tempdir().unwrap();
    let b = PaperBroker::open(dir.path().join("state/paper.sqlite3"), config())
        .await
        .unwrap();
    let candle = |datetime: i64, open: f64, high: f64, low: f64, close: f64| -> Candle {
        serde_json::from_value(json!({
            "open": open, "high": high, "low": low, "close": close,
            "volume": 4000, "datetime": datetime,
        }))
        .unwrap()
    };
    // 2024-05-17 09:30 and 09:31 America/New_York.
    let open = 1_715_952_600_000;
    let events = SyntheticBbo {
        candle_ms: 60_000,
        spread_bps: d("10"),
        participation_bps: d("1000"),
        regular_hours_only: true,
    }
    .events(
        &[serde_json::from_value::<PriceHistoryResponse>(json!({
            "symbol": "ABC",
            "empty": false,
            "candles": [
                candle(open, 10.0, 10.2, 9.8, 10.1),
                candle(open + 60_000, 10.1, 10.3, 10.0, 10.25),
            ],
        }))
        .unwrap()],
        1,
    )
    .unwrap();
    let mut events = events.into_iter();
    // The session opens under the model's name, then the open mark prints.
    b.apply(events.next().unwrap()).await.unwrap();
    b.apply(events.next().unwrap()).await.unwrap();
    let session_id = format!("{SYNTHETIC_MODEL}:2024-05-17");
    assert_eq!(
        b.market_session().await.unwrap(),
        (session_id.clone(), true)
    );
    // A limit buy under the open's ask (10.01) rests, then fills at the low
    // mark's synthetic ask (9.81), not at its own limit.
    let id = b
        .place_order("paper-test", &order("synthetic-buy", "BUY", "5", "9.95"))
        .await
        .unwrap()
        .order_id
        .unwrap();
    let fills = b.apply(events.next().unwrap()).await.unwrap().fills;
    assert_eq!(
        fills
            .iter()
            .map(|f| (f.order_id.as_str(), f.quantity, f.price))
            .collect::<Vec<_>>(),
        vec![(id.as_str(), d("5"), d("9.81"))]
    );
    // The rest of the bars replay and the session closes at the last one's end.
    for event in events {
        b.apply(event).await.unwrap();
    }
    assert_eq!(b.market_session().await.unwrap(), (session_id, false));
    let snapshot = b.snapshot("paper-test").await.unwrap();
    assert_eq!(snapshot.at_ms, open + 120_000);
    assert_eq!(snapshot.ledger.positions["ABC"].quantity, d("5"));
    assert_eq!(snapshot.ledger.cash, d("950.90"));
}

/// A gap at the open counts: a session is measured from the previous
/// session's close, as the live venue measures from its start-of-day value.
#[tokio::test]
async fn an_overnight_gap_counts_toward_the_daily_loss() {
    let mut cfg = config();
    cfg.accounts
        .get_mut("paper-test")
        .unwrap()
        .limits
        .max_daily_loss = Some(d("5"));
    let (_dir, b) = configured_setup(cfg).await;
    b.place_order("paper-test", &order("buy", "BUY", "10", "11"))
        .await
        .unwrap();
    b.apply(quote(3, 200, "10", "10.01", "10")).await.unwrap();
    assert!(b.operator_halt().await.unwrap().is_none());
    b.apply(PaperEvent {
        sequence: 4,
        at_ms: 300,
        kind: PaperEventKind::Session {
            session_id: "session-2".into(),
            open: true,
        },
    })
    .await
    .unwrap();
    // The next session opens a dollar lower: ten dollars down from the close.
    b.apply(quote(5, 400, "9", "9.01", "10")).await.unwrap();
    let snapshot = b.snapshot("paper-test").await.unwrap();
    assert_eq!(snapshot.ledger.session_mark.unwrap().equity, d("999.80"));
    assert_eq!(snapshot.daily_loss, Some(d("10.00")));
    assert!(b.operator_halt().await.unwrap().is_some());
}

/// The kill switch's cancel half finds every working order and cancels it.
#[tokio::test]
async fn cancel_working_orders_cancels_every_working_order() {
    use rustyschwab::broker::TradingBackend;
    let (_dir, b) = setup().await;
    for tag in ["one", "two"] {
        b.place_order("paper-test", &order(tag, "BUY", "1", "11"))
            .await
            .unwrap();
    }
    assert_eq!(b.working_orders("paper-test").await.unwrap().len(), 2);
    let outcomes = b.cancel_working_orders("paper-test").await.unwrap();
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.iter().all(|(_, outcome)| outcome.is_ok()));
}
