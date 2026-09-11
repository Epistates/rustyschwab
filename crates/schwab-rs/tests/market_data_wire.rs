//! Market-data requests as they leave the client: query names, casing, path
//! encoding, and the guards that stop bad requests before any bytes are sent.
mod common;

use chrono::NaiveDate;
use common::mock_client;
use schwab_rs::Error;
use schwab_rs::types::market_data::{
    Entitlement, FrequencyType, Market, MoverFrequency, MoverIndex, MoverSort, OptionChainParams,
    OptionType, PeriodType, PriceHistoryParams, QuoteField,
};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// The one request the server saw, or a panic naming how many it got.
async fn only_request(server: &MockServer) -> Request {
    let mut requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "expected exactly one request: {requests:?}"
    );
    requests.pop().unwrap()
}

fn query_pairs(request: &Request) -> Vec<(String, String)> {
    request
        .url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

#[tokio::test]
async fn price_history_sends_lowercase_period_and_frequency_types() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    Mock::given(method("GET"))
        .and(path("/marketdata/v1/pricehistory"))
        .and(query_param("symbol", "AAPL"))
        .and(query_param("periodType", "day"))
        .and(query_param("period", "10"))
        .and(query_param("frequencyType", "minute"))
        .and(query_param("frequency", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "symbol": "AAPL",
            "empty": false,
            "candles": [{
                "open": 1.0, "high": 2.0, "low": 0.5, "close": 1.5,
                "volume": 3_000_000_000_i64, "datetime": 1_700_000_000_000_i64
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let params = PriceHistoryParams {
        period_type: Some(PeriodType::Day),
        period: Some(10),
        frequency_type: Some(FrequencyType::Minute),
        frequency: Some(5),
        ..PriceHistoryParams::new("AAPL")
    };
    let history = client.get_price_history(&params).await.unwrap();
    assert_eq!(history.candles[0].volume, 3_000_000_000);

    let request = only_request(&server).await;
    let pairs = query_pairs(&request);
    // Unset options must be omitted rather than sent as `null`.
    assert!(
        pairs
            .iter()
            .all(|(k, _)| k != "startDate" && k != "endDate"),
        "{pairs:?}"
    );
    assert!(
        !pairs.iter().any(|(_, v)| v == "DAY" || v == "MINUTE"),
        "upper-case wire values regressed: {pairs:?}"
    );
}

#[tokio::test]
async fn invalid_price_history_combinations_never_reach_the_server() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);

    let bad = [
        PriceHistoryParams {
            period_type: Some(PeriodType::Day),
            period: Some(7),
            ..PriceHistoryParams::new("AAPL")
        },
        PriceHistoryParams {
            period_type: Some(PeriodType::Month),
            frequency_type: Some(FrequencyType::Minute),
            ..PriceHistoryParams::new("AAPL")
        },
        PriceHistoryParams {
            start_date: Some(2),
            end_date: Some(1),
            ..PriceHistoryParams::new("AAPL")
        },
        PriceHistoryParams::new(" "),
    ];
    for params in bad {
        assert!(
            matches!(
                client.get_price_history(&params).await,
                Err(Error::InvalidParameter(_))
            ),
            "{params:?}"
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn option_chain_sends_entitlement_option_type_and_dates() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    Mock::given(method("GET"))
        .and(path("/marketdata/v1/chains"))
        .and(query_param("symbol", "AAPL"))
        .and(query_param("entitlement", "PP"))
        .and(query_param("optionType", "S"))
        .and(query_param("fromDate", "2025-01-15"))
        .and(query_param("strikeCount", "4"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "symbol": "AAPL",
            "status": "SUCCESS"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let params = OptionChainParams {
        entitlement: Some(Entitlement::PayingPro),
        option_type: Some(OptionType::Standard),
        from_date: NaiveDate::from_ymd_opt(2025, 1, 15),
        strike_count: Some(4),
        ..OptionChainParams::new("AAPL")
    };
    let chain = client.get_option_chain(&params).await.unwrap();
    assert_eq!(chain.symbol, "AAPL");

    let request = only_request(&server).await;
    let pairs = query_pairs(&request);
    assert!(pairs.iter().all(|(k, _)| k != "toDate"), "{pairs:?}");

    // Out-of-range values are refused locally.
    let bad = OptionChainParams {
        strike_count: Some(0),
        ..OptionChainParams::new("AAPL")
    };
    assert!(matches!(
        client.get_option_chain(&bad).await,
        Err(Error::InvalidParameter(_))
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn quotes_join_fields_with_their_wire_names() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    Mock::given(method("GET"))
        .and(path("/marketdata/v1/quotes"))
        .and(query_param("symbols", "AAPL,MSFT"))
        .and(query_param("fields", "quote,fundamental"))
        .and(query_param("indicative", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "AAPL": {"symbol": "AAPL"},
            "MSFT": {"symbol": "MSFT"}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let quotes = client
        .get_quotes_with_options(
            &["AAPL", "MSFT"],
            &[QuoteField::Quote, QuoteField::Fundamental],
            Some(true),
        )
        .await
        .unwrap();
    assert_eq!(quotes.quotes.len(), 2);

    // Guards: an empty list and a blank symbol are rejected before the wire.
    assert!(matches!(
        client.get_quotes(&[]).await,
        Err(Error::InvalidParameter(_))
    ));
    assert!(matches!(
        client.get_quotes(&["AAPL", ""]).await,
        Err(Error::InvalidParameter(_))
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn single_quote_omits_fields_when_none_requested() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    Mock::given(method("GET"))
        .and(path("/marketdata/v1/AAPL/quotes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "AAPL": {"symbol": "AAPL"}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let quote = client.get_quote("AAPL").await.unwrap();
    assert_eq!(quote.symbol.as_deref(), Some("AAPL"));
    let request = only_request(&server).await;
    assert!(request.url.query().is_none(), "{:?}", request.url);
}

#[tokio::test]
async fn movers_encode_the_index_in_the_path_and_frequency_as_minutes() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    Mock::given(method("GET"))
        .and(path("/marketdata/v1/movers/%24DJI"))
        .and(query_param("sort", "PERCENT_CHANGE_UP"))
        .and(query_param("frequency", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "screeners": [{
                "symbol": "AAPL", "direction": "up", "change": 1.5,
                "last": 200.0, "totalVolume": 1000
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let movers = client
        .get_movers(
            MoverIndex::Dji,
            Some(MoverSort::PercentChangeUp),
            Some(MoverFrequency::FiveMinutes),
        )
        .await
        .unwrap();
    assert_eq!(movers.screeners[0].symbol, "AAPL");
}

/// `/instruments/{cusip}` answers with the same `{"instruments":[…]}` envelope
/// as the search endpoint; `get_instrument` unwraps the first entry and turns
/// an empty list into an API error instead of a decode error.
#[tokio::test]
async fn instrument_by_cusip_unwraps_the_instruments_envelope() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    Mock::given(method("GET"))
        .and(path("/marketdata/v1/instruments/037833100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "instruments": [{
                "cusip": "037833100", "symbol": "AAPL", "description": "APPLE INC",
                "exchange": "NASDAQ", "assetType": "EQUITY"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/marketdata/v1/instruments/000000000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"instruments": []})))
        .expect(1)
        .mount(&server)
        .await;

    let instrument = client.get_instrument("037833100").await.unwrap();
    assert_eq!(instrument.symbol, "AAPL");
    assert_eq!(instrument.cusip.as_deref(), Some("037833100"));

    let err = client.get_instrument("000000000").await.unwrap_err();
    assert!(
        matches!(&err, Error::Api { code, .. } if code == "NOT_FOUND"),
        "{err}"
    );
    assert!(matches!(
        client.get_instrument("  ").await,
        Err(Error::InvalidParameter(_))
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn market_hours_use_lowercase_markets_and_iso_dates() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    let hours = json!({
        "equity": {"EQ": {"date": "2025-01-15", "marketType": "EQUITY", "isOpen": true}},
        "option": {"EQO": {"date": "2025-01-15", "marketType": "OPTION", "isOpen": true}}
    });
    Mock::given(method("GET"))
        .and(path("/marketdata/v1/markets"))
        .and(query_param("markets", "equity,option"))
        .and(query_param("date", "2025-01-15"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hours.clone()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/marketdata/v1/markets/equity"))
        .and(query_param("date", "2025-01-15"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hours))
        .expect(1)
        .mount(&server)
        .await;

    let date = NaiveDate::from_ymd_opt(2025, 1, 15);
    let all = client
        .get_markets(&[Market::Equity, Market::Option], date)
        .await
        .unwrap();
    assert!(all.contains_key(Market::Equity.as_str()));
    let equity = client.get_market(Market::Equity, date).await.unwrap();
    assert_eq!(equity["equity"]["EQ"].is_open, Some(true));

    assert!(matches!(
        client.get_markets(&[], None).await,
        Err(Error::InvalidParameter(_))
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn account_paths_reject_account_numbers_and_separators_before_the_wire() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);

    let err = client.get_account("12345678", false).await.unwrap_err();
    assert!(
        matches!(&err, Error::InvalidParameter(msg) if msg.contains("account number")),
        "{err}"
    );
    assert!(matches!(
        client.get_order("hash", "1/2").await,
        Err(Error::InvalidParameter(_))
    ));
    assert!(matches!(
        client.get_order("ha/sh", "1").await,
        Err(Error::InvalidParameter(_))
    ));
    assert!(matches!(
        client.get_transaction("hash", "../orders").await,
        Err(Error::InvalidParameter(_))
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn get_json_is_a_bearer_get_with_encoded_query_and_raw_body() {
    let server = MockServer::start().await;
    let (client, _dir, _) = mock_client(&server, false);
    Mock::given(method("GET"))
        .and(path("/marketdata/v1/quotes"))
        .and(query_param("symbols", "AAPL,$SPX"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "AAPL": {"quote": {"lastPrice": 1.5}},
            "unknownTopLevelKey": true
        })))
        .mount(&server)
        .await;

    let body = client
        .get_json("/marketdata/v1/quotes", &[("symbols", "AAPL,$SPX")])
        .await
        .unwrap();
    assert_eq!(body["AAPL"]["quote"]["lastPrice"], json!(1.5));
    assert_eq!(body["unknownTopLevelKey"], json!(true));

    let request = only_request(&server).await;
    assert_eq!(
        request.headers.get("authorization").unwrap(),
        "Bearer old_access"
    );
    assert_eq!(
        request.url.query(),
        Some("symbols=AAPL%2C%24SPX"),
        "reqwest form-encodes the pairs"
    );

    assert!(matches!(
        client.get_json("marketdata/v1/quotes", &[]).await,
        Err(Error::InvalidParameter(_))
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
