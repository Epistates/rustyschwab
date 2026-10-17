//! Live, read-only conformance check against Schwab's real REST API.
//!
//! Skipped unless `SCHWAB_LIVE_CONFORMANCE` is set:
//!
//! - `1` / `check`: fetch every read-only endpoint, prove the typed
//!   bindings still decode the live body, and diff the body's *shape*
//!   (key sets and scalar types, not values) against the recorded
//!   fixtures in `tests/fixtures/conformance/`.
//! - `record`: same, then merge the live shapes into the fixtures.
//!
//! Needs real credentials (`SCHWAB_APP_KEY`, `SCHWAB_APP_SECRET`) and a
//! token file with a valid refresh token; `.env` is loaded if present. Only
//! `GET` requests are made. The run writes a JSON report (default
//! `target/live-conformance.json`, override with
//! `SCHWAB_LIVE_CONFORMANCE_REPORT`) that `qualification.yml` uploads.
//!
//! Failure policy: a typed decode error or a scalar that changed type
//! fails the test. Keys that appear or disappear are reported as warnings,
//! because Schwab omits fields whose value is absent and a live sample is
//! data-dependent; `record` accumulates them into the fixture over time.

mod common;

use chrono::{Duration, Utc};
use rustyschwab::types::accounts::{Account, AccountNumberHash, UserPreferences};
use rustyschwab::types::common::schwab_timestamp as iso;
use rustyschwab::types::market_data::{
    ExpirationChainResponse, InstrumentsResponse, MarketsHoursResponse, MoversResponse,
    OptionChainResponse, PriceHistoryResponse, QuoteItem, QuotesResponse,
};
use rustyschwab::types::trading::{Order, Transaction};
use rustyschwab::{SchwabClient, SchwabConfig};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Check,
    Record,
}

fn mode() -> Option<Mode> {
    match std::env::var("SCHWAB_LIVE_CONFORMANCE").ok()?.as_str() {
        "1" | "check" | "true" => Some(Mode::Check),
        "record" => Some(Mode::Record),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Shape model
// ---------------------------------------------------------------------------

/// Structural summary of a JSON value: which scalar types were seen and, for
/// containers, the merged shape of what they hold. Object keys are schema
/// unless the endpoint marks the object's path as data-keyed (`Map`), in
/// which case only the union of its values is kept.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct Shape {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    types: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fields: Option<BTreeMap<String, Shape>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    values: Option<Box<Shape>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    items: Option<Box<Shape>>,
}

impl Shape {
    fn of(value: &Value, path: &mut Vec<String>, map_paths: &[&str]) -> Shape {
        let mut shape = Shape::default();
        match value {
            Value::Null => {
                shape.types.insert("null".into());
            }
            Value::Bool(_) => {
                shape.types.insert("bool".into());
            }
            Value::Number(_) => {
                shape.types.insert("number".into());
            }
            Value::String(_) => {
                shape.types.insert("string".into());
            }
            Value::Array(items) => {
                shape.types.insert("array".into());
                let mut merged: Option<Shape> = None;
                for item in items {
                    let item_shape = Shape::of(item, path, map_paths);
                    merged = Some(match merged {
                        Some(m) => m.merge(item_shape),
                        None => item_shape,
                    });
                }
                shape.items = merged.map(Box::new);
            }
            Value::Object(map) => {
                if is_map_path(path, map_paths) {
                    shape.types.insert("map".into());
                    let mut merged: Option<Shape> = None;
                    for (key, item) in map {
                        path.push(key.clone());
                        let item_shape = Shape::of(item, path, map_paths);
                        path.pop();
                        merged = Some(match merged {
                            Some(m) => m.merge(item_shape),
                            None => item_shape,
                        });
                    }
                    shape.values = merged.map(Box::new);
                } else {
                    shape.types.insert("object".into());
                    let mut fields = BTreeMap::new();
                    for (key, item) in map {
                        path.push(key.clone());
                        fields.insert(key.clone(), Shape::of(item, path, map_paths));
                        path.pop();
                    }
                    shape.fields = Some(fields);
                }
            }
        }
        shape
    }

    fn merge(mut self, other: Shape) -> Shape {
        self.types.extend(other.types);
        self.fields = match (self.fields.take(), other.fields) {
            (Some(mut a), Some(b)) => {
                for (key, shape) in b {
                    let merged = match a.remove(&key) {
                        Some(existing) => existing.merge(shape),
                        None => shape,
                    };
                    a.insert(key, merged);
                }
                Some(a)
            }
            (a, b) => a.or(b),
        };
        self.values = merge_boxed(self.values.take(), other.values);
        self.items = merge_boxed(self.items.take(), other.items);
        self
    }
}

fn merge_boxed(a: Option<Box<Shape>>, b: Option<Box<Shape>>) -> Option<Box<Shape>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(Box::new((*a).merge(*b))),
        (a, b) => a.or(b),
    }
}

/// `map_paths` entries are JSON-pointer-like key paths (array indices
/// omitted) where `*` matches any single key: `"/callExpDateMap/*"`.
fn is_map_path(path: &[String], map_paths: &[&str]) -> bool {
    map_paths.iter().any(|pattern| {
        let pattern: Vec<&str> = pattern.split('/').skip(1).collect();
        let pattern = if pattern == [""] { vec![] } else { pattern };
        pattern.len() == path.len()
            && pattern
                .iter()
                .zip(path)
                .all(|(p, actual)| *p == "*" || *p == actual)
    })
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Kind {
    /// A key the fixture has that the live sample did not carry.
    Missing,
    /// A key the live sample carries that the fixture does not.
    Added,
    /// A scalar type the fixture never recorded for this key.
    TypeChanged,
    /// The live body no longer deserializes into the typed binding.
    DecodeError,
    /// The request itself failed.
    RequestError,
}

impl Kind {
    fn fails(&self) -> bool {
        matches!(
            self,
            Kind::TypeChanged | Kind::DecodeError | Kind::RequestError
        )
    }
}

#[derive(Debug, Clone, Serialize)]
struct Finding {
    kind: Kind,
    path: String,
    detail: String,
}

fn diff(fixture: &Shape, live: &Shape, path: &str, out: &mut Vec<Finding>) {
    // A field turning nullable is what the `Option` bindings exist for.
    let new_types: Vec<&String> = live
        .types
        .iter()
        .filter(|t| !fixture.types.contains(*t) && t.as_str() != "null")
        .collect();
    if !new_types.is_empty() {
        out.push(Finding {
            kind: Kind::TypeChanged,
            path: path.to_string(),
            detail: format!(
                "fixture types {:?}, live adds {:?}",
                fixture.types, new_types
            ),
        });
    }
    if let (Some(fixture_fields), Some(live_fields)) = (&fixture.fields, &live.fields) {
        for (key, fixture_child) in fixture_fields {
            match live_fields.get(key) {
                Some(live_child) => diff(fixture_child, live_child, &format!("{path}/{key}"), out),
                None => out.push(Finding {
                    kind: Kind::Missing,
                    path: format!("{path}/{key}"),
                    detail: "present in fixture, absent in live sample".into(),
                }),
            }
        }
        for key in live_fields.keys() {
            if !fixture_fields.contains_key(key) {
                out.push(Finding {
                    kind: Kind::Added,
                    path: format!("{path}/{key}"),
                    detail: "live sample carries a key the fixture lacks".into(),
                });
            }
        }
    }
    if let (Some(f), Some(l)) = (&fixture.values, &live.values) {
        diff(f, l, &format!("{path}/*"), out);
    }
    if let (Some(f), Some(l)) = (&fixture.items, &live.items) {
        diff(f, l, &format!("{path}[]"), out);
    }
}

// ---------------------------------------------------------------------------
// Endpoint catalogue
// ---------------------------------------------------------------------------

struct Endpoint {
    name: &'static str,
    path: String,
    query: Vec<(&'static str, String)>,
    map_paths: &'static [&'static str],
    decode: fn(Value) -> Result<(), String>,
}

fn decode_as<T: DeserializeOwned>(value: Value) -> Result<(), String> {
    serde_json::from_value::<T>(value)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn endpoint(
    name: &'static str,
    path: impl Into<String>,
    query: &[(&'static str, &str)],
    map_paths: &'static [&'static str],
    decode: fn(Value) -> Result<(), String>,
) -> Endpoint {
    Endpoint {
        name,
        path: path.into(),
        query: query.iter().map(|(k, v)| (*k, v.to_string())).collect(),
        map_paths,
        decode,
    }
}

/// Endpoints that need no account context.
fn market_data_endpoints() -> Vec<Endpoint> {
    vec![
        endpoint(
            "quotes",
            "/marketdata/v1/quotes",
            &[("symbols", "AAPL,MSFT,SPY,$SPX")],
            &[""],
            decode_as::<QuotesResponse>,
        ),
        endpoint("quote", "/marketdata/v1/AAPL/quotes", &[], &[""], |v| {
            // Symbol-keyed like the batch form; decode the one entry.
            let item = v
                .as_object()
                .and_then(|m| m.get("AAPL"))
                .cloned()
                .ok_or("body is not keyed by the requested symbol")?;
            decode_as::<QuoteItem>(item)
        }),
        endpoint(
            "price_history",
            "/marketdata/v1/pricehistory",
            &[
                ("symbol", "AAPL"),
                ("periodType", "day"),
                ("period", "5"),
                ("frequencyType", "minute"),
                ("frequency", "30"),
            ],
            &[],
            decode_as::<PriceHistoryResponse>,
        ),
        endpoint(
            "option_chain",
            "/marketdata/v1/chains",
            &[
                ("symbol", "AAPL"),
                ("contractType", "ALL"),
                ("strikeCount", "2"),
            ],
            &[
                "/callExpDateMap",
                "/callExpDateMap/*",
                "/putExpDateMap",
                "/putExpDateMap/*",
            ],
            decode_as::<OptionChainResponse>,
        ),
        endpoint(
            "option_expiration_chain",
            "/marketdata/v1/expirationchain",
            &[("symbol", "AAPL")],
            &[],
            decode_as::<ExpirationChainResponse>,
        ),
        endpoint(
            "movers",
            "/marketdata/v1/movers/$DJI",
            &[("sort", "VOLUME"), ("frequency", "0")],
            &[],
            decode_as::<MoversResponse>,
        ),
        endpoint(
            "instruments_fundamental",
            "/marketdata/v1/instruments",
            &[("symbol", "AAPL"), ("projection", "fundamental")],
            &[],
            decode_as::<InstrumentsResponse>,
        ),
        endpoint(
            "instruments_symbol_search",
            "/marketdata/v1/instruments",
            &[("symbol", "AAPL"), ("projection", "symbol-search")],
            &[],
            decode_as::<InstrumentsResponse>,
        ),
        // Same `{"instruments":[…]}` envelope as the search endpoint;
        // `get_instrument` unwraps the first entry.
        endpoint(
            "instrument_by_cusip",
            "/marketdata/v1/instruments/037833100",
            &[],
            &[],
            decode_as::<InstrumentsResponse>,
        ),
        endpoint(
            "markets",
            "/marketdata/v1/markets",
            &[("markets", "equity,option,bond,future,forex")],
            &["/*"],
            decode_as::<MarketsHoursResponse>,
        ),
        endpoint(
            "market_equity",
            "/marketdata/v1/markets/equity",
            &[],
            &["/*"],
            decode_as::<MarketsHoursResponse>,
        ),
    ]
}

/// Endpoints that need an account hash; `orders` / `transactions` also
/// seed the single-item lookups when the account has any.
fn account_endpoints(hash: &str) -> Vec<Endpoint> {
    let now = Utc::now();
    let orders_from = iso(now - Duration::days(60));
    let tx_from = iso(now - Duration::days(30));
    let to = iso(now);
    vec![
        endpoint(
            "accounts",
            "/trader/v1/accounts",
            &[("fields", "positions")],
            &[],
            decode_as::<Vec<Account>>,
        ),
        endpoint(
            "account",
            format!("/trader/v1/accounts/{hash}"),
            &[("fields", "positions")],
            &[],
            decode_as::<Account>,
        ),
        endpoint(
            "account_orders",
            format!("/trader/v1/accounts/{hash}/orders"),
            &[
                ("fromEnteredTime", &orders_from),
                ("toEnteredTime", &to),
                ("maxResults", "50"),
            ],
            &[],
            decode_as::<Vec<Order>>,
        ),
        endpoint(
            "all_orders",
            "/trader/v1/orders",
            &[
                ("fromEnteredTime", &orders_from),
                ("toEnteredTime", &to),
                ("maxResults", "50"),
            ],
            &[],
            decode_as::<Vec<Order>>,
        ),
        endpoint(
            "transactions",
            format!("/trader/v1/accounts/{hash}/transactions"),
            &[
                ("startDate", &tx_from),
                ("endDate", &to),
                ("types", "TRADE"),
            ],
            &[],
            decode_as::<Vec<Transaction>>,
        ),
        endpoint(
            "user_preferences",
            "/trader/v1/userPreference",
            &[],
            &[],
            decode_as::<UserPreferences>,
        ),
    ]
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct EndpointReport {
    name: &'static str,
    path: String,
    status: &'static str,
    fixture: &'static str,
    findings: Vec<Finding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shape: Option<Shape>,
}

#[derive(Debug, Serialize)]
struct Report {
    generated_at: String,
    mode: &'static str,
    endpoints: Vec<EndpointReport>,
}

struct Harness {
    client: SchwabClient,
    mode: Mode,
    fixture_dir: PathBuf,
}

impl Harness {
    fn fixture_path(&self, name: &str) -> PathBuf {
        self.fixture_dir.join(format!("{name}.shape.json"))
    }

    fn load_fixture(&self, name: &str) -> Option<Shape> {
        let bytes = std::fs::read(self.fixture_path(name)).ok()?;
        Some(serde_json::from_slice(&bytes).expect("fixture is valid shape JSON"))
    }

    fn write_fixture(&self, name: &str, shape: &Shape) {
        std::fs::create_dir_all(&self.fixture_dir).expect("create fixture dir");
        let mut json = serde_json::to_string_pretty(shape).unwrap();
        json.push('\n');
        std::fs::write(self.fixture_path(name), json).expect("write fixture");
    }

    /// Fetch one endpoint, decode it, diff its shape and (in record mode)
    /// fold it into the fixture. Returns the body so callers can pick IDs
    /// for follow-up lookups.
    async fn run(&self, endpoint: &Endpoint) -> (EndpointReport, Option<Value>) {
        let query: Vec<(&str, &str)> = endpoint
            .query
            .iter()
            .map(|(k, v)| (*k, v.as_str()))
            .collect();
        let mut findings = Vec::new();
        let body = match self.client.get_json(&endpoint.path, &query).await {
            Ok(body) => body,
            Err(error) => {
                findings.push(Finding {
                    kind: Kind::RequestError,
                    path: String::new(),
                    detail: error.to_string(),
                });
                return (
                    EndpointReport {
                        name: endpoint.name,
                        path: endpoint.path.clone(),
                        status: "fail",
                        fixture: "n/a",
                        findings,
                        shape: None,
                    },
                    None,
                );
            }
        };
        if let Err(error) = (endpoint.decode)(body.clone()) {
            findings.push(Finding {
                kind: Kind::DecodeError,
                path: String::new(),
                detail: error,
            });
        }
        let live = Shape::of(&body, &mut Vec::new(), endpoint.map_paths);
        let fixture = self.load_fixture(endpoint.name);
        let fixture_state = match &fixture {
            Some(fixture) => {
                diff(fixture, &live, "", &mut findings);
                "present"
            }
            None => "missing",
        };
        let mut shape = None;
        match self.mode {
            Mode::Record => {
                let merged = match fixture {
                    Some(fixture) => fixture.merge(live),
                    None => live,
                };
                self.write_fixture(endpoint.name, &merged);
            }
            Mode::Check if fixture_state == "missing" => shape = Some(live),
            Mode::Check => {}
        }
        let status = if findings.iter().any(|f| f.kind.fails()) {
            "fail"
        } else if findings.is_empty() && fixture_state == "present" {
            "ok"
        } else {
            "warn"
        };
        (
            EndpointReport {
                name: endpoint.name,
                path: endpoint.path.clone(),
                status,
                fixture: fixture_state,
                findings,
                shape,
            },
            Some(body),
        )
    }

    /// The full read-only sweep: market data, then the account endpoints
    /// for the first account hash, then single-item lookups when the
    /// account has any orders or transactions.
    async fn sweep(&self) -> Vec<EndpointReport> {
        let mut reports = Vec::new();
        for endpoint in market_data_endpoints() {
            reports.push(self.run(&endpoint).await.0);
        }

        let account_numbers = endpoint(
            "account_numbers",
            "/trader/v1/accounts/accountNumbers",
            &[],
            &[],
            decode_as::<Vec<AccountNumberHash>>,
        );
        let (report, body) = self.run(&account_numbers).await;
        reports.push(report);
        let Some(hash) = first_id(body.as_ref(), "hashValue") else {
            eprintln!("no account hash returned; account endpoints skipped");
            return reports;
        };

        let mut bodies: BTreeMap<&'static str, Value> = BTreeMap::new();
        for endpoint in account_endpoints(&hash) {
            let (report, body) = self.run(&endpoint).await;
            reports.push(report);
            if let Some(body) = body {
                bodies.insert(endpoint.name, body);
            }
        }
        if let Some(order_id) = first_id(bodies.get("account_orders"), "orderId") {
            let single = endpoint(
                "order",
                format!("/trader/v1/accounts/{hash}/orders/{order_id}"),
                &[],
                &[],
                decode_as::<Order>,
            );
            reports.push(self.run(&single).await.0);
        }
        if let Some(activity_id) = first_id(bodies.get("transactions"), "activityId") {
            let single = endpoint(
                "transaction",
                format!("/trader/v1/accounts/{hash}/transactions/{activity_id}"),
                &[],
                &[],
                decode_as::<Transaction>,
            );
            reports.push(self.run(&single).await.0);
        }
        reports
    }
}

fn first_id(body: Option<&Value>, key: &str) -> Option<String> {
    body?.as_array()?.first()?.get(key).map(|v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

#[tokio::test]
async fn live_shapes_match_fixtures() {
    let Some(mode) = mode() else {
        eprintln!("SCHWAB_LIVE_CONFORMANCE not set; skipping live conformance run");
        return;
    };
    dotenvy::dotenv().ok();
    let config = SchwabConfig::from_env().expect("SCHWAB_* environment is complete");
    let client = SchwabClient::new(config).expect("client");
    client.init().await.expect("token file loads");
    let harness = Harness {
        client,
        mode,
        fixture_dir: Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/conformance"),
    };

    let report = Report {
        generated_at: iso(Utc::now()),
        mode: match mode {
            Mode::Check => "check",
            Mode::Record => "record",
        },
        endpoints: harness.sweep().await,
    };
    let report_path = std::env::var("SCHWAB_LIVE_CONFORMANCE_REPORT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/live-conformance.json")
        });
    if let Some(parent) = report_path.parent() {
        std::fs::create_dir_all(parent).expect("create report dir");
    }
    std::fs::write(&report_path, serde_json::to_vec_pretty(&report).unwrap())
        .expect("write report");

    let mut failed = Vec::new();
    for endpoint in &report.endpoints {
        println!(
            "{:<28} {:<5} fixture={:<8} findings={}",
            endpoint.name,
            endpoint.status,
            endpoint.fixture,
            endpoint.findings.len()
        );
        for finding in &endpoint.findings {
            println!(
                "    {:?} {} — {}",
                finding.kind, finding.path, finding.detail
            );
        }
        if endpoint.status == "fail" {
            failed.push(endpoint.name);
        }
    }
    println!("report written to {}", report_path.display());
    assert!(
        failed.is_empty(),
        "live conformance failures: {failed:?} (see {})",
        report_path.display()
    );
}

// ---------------------------------------------------------------------------
// Offline tests for the shape/diff machinery so the harness itself is
// exercised in CI even when no credentials are present.
// ---------------------------------------------------------------------------

#[test]
fn shape_unions_array_items_and_map_values() {
    let body = serde_json::json!({
        "AAPL": {"quote": {"lastPrice": 1.0, "bidSize": 2}},
        "MSFT": {"quote": {"lastPrice": 2.0, "dividendDate": "2026-01-01"}},
        "errors": {"invalidSymbols": ["BAD"]}
    });
    let shape = Shape::of(&body, &mut Vec::new(), &[""]);
    assert!(shape.types.contains("map"));
    let values = shape.values.expect("map values");
    let fields = values.fields.expect("object");
    let quote = fields["quote"].fields.as_ref().unwrap();
    assert!(quote.contains_key("lastPrice"));
    assert!(quote.contains_key("bidSize"));
    assert!(quote.contains_key("dividendDate"));
    assert!(fields.contains_key("invalidSymbols"));
}

#[test]
fn map_path_patterns_match_wildcards_and_root() {
    assert!(is_map_path(&[], &[""]));
    assert!(!is_map_path(&["a".into()], &[""]));
    assert!(is_map_path(
        &["callExpDateMap".into(), "2026-01-16:5".into()],
        &["/callExpDateMap/*"]
    ));
    assert!(!is_map_path(
        &["callExpDateMap".into()],
        &["/callExpDateMap/*"]
    ));
}

#[test]
fn diff_classifies_added_missing_and_type_changes() {
    let fixture = Shape::of(
        &serde_json::json!({"a": 1, "b": "x", "c": [ {"d": true} ]}),
        &mut Vec::new(),
        &[],
    );
    let live = Shape::of(
        &serde_json::json!({"a": "1", "c": [ {"d": true, "e": null} ], "f": 2}),
        &mut Vec::new(),
        &[],
    );
    let mut findings = Vec::new();
    diff(&fixture, &live, "", &mut findings);
    let by_path: BTreeMap<&str, &Kind> = findings
        .iter()
        .map(|f| (f.path.as_str(), &f.kind))
        .collect();
    assert_eq!(by_path["/a"], &Kind::TypeChanged);
    assert_eq!(by_path["/b"], &Kind::Missing);
    assert_eq!(by_path["/c[]/e"], &Kind::Added);
    assert_eq!(by_path["/f"], &Kind::Added);
    assert!(findings.iter().any(|f| f.kind.fails()));
}

#[test]
fn diff_tolerates_fields_becoming_null() {
    let fixture = Shape::of(&serde_json::json!({"a": 1}), &mut Vec::new(), &[]);
    let live = Shape::of(&serde_json::json!({"a": null}), &mut Vec::new(), &[]);
    let mut findings = Vec::new();
    diff(&fixture, &live, "", &mut findings);
    assert!(findings.is_empty(), "{findings:?}");
}

#[test]
fn shape_round_trips_through_json() {
    let shape = Shape::of(
        &serde_json::json!({"x": [1, "two", null], "y": {"z": {}}}),
        &mut Vec::new(),
        &["/y"],
    );
    let json = serde_json::to_string(&shape).unwrap();
    let back: Shape = serde_json::from_str(&json).unwrap();
    assert_eq!(back, shape);
}

/// The whole request → decode → shape → fixture cycle against a mock server:
/// a missing fixture warns and carries the shape, `record` writes it, a
/// later `check` diffs against it, and a typed decode failure fails.
#[tokio::test]
async fn harness_records_then_checks_against_a_mock_server() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn respond(server: &MockServer, body: Value) {
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/marketdata/v1/movers/$DJI"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    let server = MockServer::start().await;
    let (client, _dir, _) = common::mock_client(&server, false);
    let fixtures = tempfile::tempdir().unwrap();
    let movers = || {
        endpoint(
            "movers",
            "/marketdata/v1/movers/$DJI",
            &[("sort", "VOLUME")],
            &[],
            decode_as::<MoversResponse>,
        )
    };
    // `rank` stands in for a field the SDK does not type, so a type change
    // there is exactly the drift the typed decode alone would swallow.
    let v1 = serde_json::json!({"screeners": [{
        "symbol": "AAPL", "lastPrice": 200.0, "netChange": 1.5,
        "totalVolume": 1000, "rank": 0.2
    }]});

    // Check with no fixture: warns and includes the live shape for review.
    respond(&server, v1.clone()).await;
    let check = Harness {
        client,
        mode: Mode::Check,
        fixture_dir: fixtures.path().to_path_buf(),
    };
    let (report, body) = check.run(&movers()).await;
    assert_eq!((report.status, report.fixture), ("warn", "missing"));
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert!(report.shape.is_some());
    assert_eq!(body.unwrap()["screeners"][0]["symbol"], "AAPL");
    assert!(!check.fixture_path("movers").exists());

    // Record writes the fixture; a second identical check is clean.
    let record = Harness {
        mode: Mode::Record,
        ..check
    };
    let (report, _) = record.run(&movers()).await;
    assert_eq!((report.status, report.fixture), ("warn", "missing"));
    assert!(record.fixture_path("movers").is_file());
    let check = Harness {
        mode: Mode::Check,
        ..record
    };
    let (report, _) = check.run(&movers()).await;
    assert_eq!((report.status, report.fixture), ("ok", "present"));
    assert!(
        report.shape.is_none(),
        "shape only reported when unrecorded"
    );

    // A new key warns; a scalar changing type fails even though the typed
    // binding still decodes (the field lands in `extra`).
    let mut v2 = v1.clone();
    v2["screeners"][0]["volume"] = serde_json::json!(5);
    v2["screeners"][0]["rank"] = serde_json::json!("0.2");
    respond(&server, v2).await;
    let (report, _) = check.run(&movers()).await;
    assert_eq!(report.status, "fail");
    let kinds: BTreeMap<&str, &Kind> = report
        .findings
        .iter()
        .map(|f| (f.path.as_str(), &f.kind))
        .collect();
    assert_eq!(kinds["/screeners[]/volume"], &Kind::Added);
    assert_eq!(kinds["/screeners[]/rank"], &Kind::TypeChanged);
    assert!(
        !kinds.values().any(|k| **k == Kind::DecodeError),
        "{:?}",
        report.findings
    );

    // A body the typed binding cannot decode fails even with no other drift.
    respond(&server, serde_json::json!({"screeners": "nope"})).await;
    let (report, _) = check.run(&movers()).await;
    assert_eq!(report.status, "fail");
    assert!(report.findings.iter().any(|f| f.kind == Kind::DecodeError));

    // Transport failures are reported, not panicked.
    server.reset().await;
    let (report, body) = check.run(&movers()).await;
    assert_eq!((report.status, report.fixture), ("fail", "n/a"));
    assert!(report.findings.iter().any(|f| f.kind == Kind::RequestError));
    assert!(body.is_none());
}
