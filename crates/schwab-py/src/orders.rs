//! Order construction and validation: the SDK's builders, called from Python.
//!
//! Nothing here builds an order by itself. `build_order` expands the same
//! `OrderSpec` JSON the MCP `build_order` tool takes, and the composition
//! functions call `OrderRequest::one_cancels_other`, `triggers` and `bracket`,
//! so an order made in Python is the order Rust code would have made, with the
//! same checks. The templates in `rustyschwab.orders` are Python over these.
//!
//! Every result is run through `OrderRequest::validate`, the check that
//! `place_order` and `preview_order` apply before anything is sent, so a
//! built order is one the client accepts.

use crate::convert::{Json, from_py, from_value, is_instance, to_json};
use crate::errors::{Failure, to_pyerr};
use chrono::NaiveDate;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use schwab_rs::Error;
use schwab_rs::types::Decimal;
use schwab_rs::types::common::{PutCall, exact_decimal};
use schwab_rs::types::trading::{OptionSymbol, OrderRequest, OrderSpec};
use serde_json::{Value, json};

/// The checks `place_order` runs, as a failure the binding can raise.
fn validated(order: OrderRequest) -> Result<OrderRequest, Failure> {
    order.validate().map_err(Error::from)?;
    Ok(order)
}

/// A price or quantity argument as an exact decimal, read the way the SDK
/// reads one in an order: a `str`, `int` or `Decimal` keeps every digit and a
/// `float` is its shortest round-trip spelling.
fn decimal(obj: &Bound<'_, PyAny>, name: &str) -> Result<Decimal, Failure> {
    exact_decimal::deserialize(to_json(obj)?)
        .map_err(|error| Failure::field(name, format!("invalid {name}: {error}")))
}

/// A `datetime.date` or an ISO `YYYY-MM-DD` string as a date. A
/// `datetime.datetime` is refused: an expiration is a calendar day, and which
/// day a timestamp means depends on a timezone.
fn expiration(obj: &Bound<'_, PyAny>) -> Result<NaiveDate, Failure> {
    static DATETIME: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    let invalid = || {
        Failure::field(
            "expiration",
            "expiration must be a date or a YYYY-MM-DD string",
        )
    };
    if is_instance(&DATETIME, obj, "datetime", "datetime")
        .map_err(|error| Failure::input(error.to_string()))?
    {
        return Err(Failure::field(
            "expiration",
            "expiration must be a date, not a datetime; use .date()",
        ));
    }
    match to_json(obj)? {
        Value::String(text) => text.parse().map_err(|_| {
            Failure::field(
                "expiration",
                format!("expiration {text:?} is not a YYYY-MM-DD date"),
            )
        }),
        _ => Err(invalid()),
    }
}

/// An order from an `OrderSpec` dict.
fn build(spec: &Bound<'_, PyAny>) -> Result<OrderRequest, Failure> {
    let spec: OrderSpec = from_py(spec, "order spec")?;
    spec.build()
        .map_err(|message| Failure::input(format!("invalid order spec: {message}")))
}

/// Expand an order spec into the Schwab order JSON that `place_order` takes.
///
/// `spec` is the `OrderSpec` of the SDK and the MCP `build_order` tool: a dict
/// keyed by `kind` (`market`, `limit`, `stop`, `stop_limit`, `trailing_stop`,
/// `net_debit`, `net_credit`, `net_zero`, `oco`, `trigger`, `bracket`).
/// Prices and quantities may be `str`, `int` or `Decimal`. A spec the SDK
/// cannot expand, or an order its validation refuses, raises `ValidationError`.
#[pyfunction]
pub fn build_order(py: Python<'_>, spec: &Bound<'_, PyAny>) -> PyResult<Json<OrderRequest>> {
    build(spec)
        .and_then(validated)
        .map(Json)
        .map_err(|failure| to_pyerr(py, failure))
}

/// Run the SDK's order validation on an order dict, offline.
///
/// This is the check `place_order` and `preview_order` apply before sending:
/// each order type carries exactly its fields, instructions fit the asset,
/// quantities are whole, symbols are canonical. It raises `ValidationError`
/// naming the field when the order fails, and returns `None` when it passes.
/// It does not know Schwab's own rules (buying power, eligibility, tick
/// sizes); `preview_order` does.
#[pyfunction]
pub fn validate_order(py: Python<'_>, order: &Bound<'_, PyAny>) -> PyResult<()> {
    from_py::<OrderRequest>(order, "order")
        .and_then(validated)
        .map(drop)
        .map_err(|failure| to_pyerr(py, failure))
}

/// The order that cancels `second` when `first` fills, and the reverse.
///
/// Both are order dicts. The result is an `OCO` parent with the two as
/// children.
#[pyfunction]
pub fn oco_order(
    py: Python<'_>,
    first: &Bound<'_, PyAny>,
    second: &Bound<'_, PyAny>,
) -> PyResult<Json<OrderRequest>> {
    (|| {
        let first = from_py(first, "first order")?;
        let second = from_py(second, "second order")?;
        validated(OrderRequest::one_cancels_other(first, second))
    })()
    .map(Json)
    .map_err(|failure| to_pyerr(py, failure))
}

/// `entry` with `children` submitted once it fills (a `TRIGGER` parent).
///
/// `entry` is an order dict and `children` a list of them; at least one is
/// required.
#[pyfunction]
pub fn trigger_order(
    py: Python<'_>,
    entry: &Bound<'_, PyAny>,
    children: &Bound<'_, PyAny>,
) -> PyResult<Json<OrderRequest>> {
    (|| {
        let entry: OrderRequest = from_py(entry, "entry order")?;
        let children: Vec<OrderRequest> = from_py(children, "child orders")?;
        if children.is_empty() {
            return Err(Failure::field(
                "children",
                "a trigger needs at least one child order",
            ));
        }
        validated(children.into_iter().fold(entry, OrderRequest::triggers))
    })()
    .map(Json)
    .map_err(|failure| to_pyerr(py, failure))
}

/// `entry` with a good-till-cancel take-profit limit and stop-loss, submitted
/// as an `OCO` pair when it fills (`OrderRequest::bracket`).
///
/// The entry must be a single-leg order that opens a position. The exits use
/// its closing instruction and full quantity, and must sit on the right side
/// of the entry and of each other for the position's direction.
#[pyfunction]
pub fn bracket_order(
    py: Python<'_>,
    entry: &Bound<'_, PyAny>,
    take_profit: &Bound<'_, PyAny>,
    stop_loss: &Bound<'_, PyAny>,
) -> PyResult<Json<OrderRequest>> {
    (|| {
        let entry: OrderRequest = from_py(entry, "entry order")?;
        let bracketed = entry
            .bracket(
                decimal(take_profit, "take_profit")?,
                decimal(stop_loss, "stop_loss")?,
            )
            .map_err(Failure::input)?;
        validated(bracketed)
    })()
    .map(Json)
    .map_err(|failure| to_pyerr(py, failure))
}

fn format_option(
    underlying: &str,
    expiration: &Bound<'_, PyAny>,
    put_call: &Bound<'_, PyAny>,
    strike: &Bound<'_, PyAny>,
) -> Result<String, Failure> {
    let put_call: PutCall = from_value(to_json(put_call)?, "put_call")?;
    let symbol = OptionSymbol::new(
        underlying,
        self::expiration(expiration)?,
        put_call,
        decimal(strike, "strike")?,
    )
    .map_err(Failure::input)?;
    Ok(symbol.to_string())
}

/// The 21-character OSI symbol Schwab uses for an option contract:
/// `option_symbol("AAPL", date(2024, 5, 17), "PUT", "190")` is
/// `"AAPL  240517P00190000"`.
///
/// `expiration` is a `datetime.date` or `YYYY-MM-DD`, `put_call` is `"CALL"`
/// or `"PUT"`, and `strike` a `str`, `int` or `Decimal`. A root over six
/// letters or digits, an expiration outside 2000-2099 and a strike the format
/// cannot carry (not positive, 100000 or more, finer than 0.001) raise
/// `ValidationError`.
#[pyfunction]
pub fn option_symbol(
    py: Python<'_>,
    underlying: &str,
    expiration: &Bound<'_, PyAny>,
    put_call: &Bound<'_, PyAny>,
    strike: &Bound<'_, PyAny>,
) -> PyResult<String> {
    format_option(underlying, expiration, put_call, strike).map_err(|failure| to_pyerr(py, failure))
}

fn parse_option(symbol: &str) -> Result<Value, Failure> {
    let parsed: OptionSymbol = symbol.parse().map_err(Failure::input)?;
    Ok(json!({
        "underlying": parsed.underlying(),
        "expiration": parsed.expiration().to_string(),
        "put_call": parsed.put_call(),
        "strike": parsed.strike().to_string(),
    }))
}

/// Split an OSI option symbol into `underlying`, `expiration` (ISO date),
/// `put_call` (`"CALL"` or `"PUT"`) and `strike` (a decimal string).
///
/// Only the canonical form is accepted: the symbol `option_symbol` writes.
/// Anything else raises `ValidationError`.
#[pyfunction]
pub fn parse_option_symbol(py: Python<'_>, symbol: &str) -> PyResult<Json<Value>> {
    parse_option(symbol)
        .map(Json)
        .map_err(|failure| to_pyerr(py, failure))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::Failure;
    use pyo3::types::PyDict;

    fn run<R>(check: impl FnOnce(Python<'_>) -> PyResult<R>) -> R {
        Python::attach(|py| check(py).expect("python check"))
    }

    fn message(failure: Failure) -> String {
        match failure {
            Failure::Input { message, .. } => message,
            other => panic!("expected an input failure, got {other:?}"),
        }
    }

    #[test]
    fn specs_expand_to_validated_requests() {
        run(|py| {
            let spec = py.eval(
                c"{'kind': 'limit', 'price': __import__('decimal').Decimal('150.25'),\n\
                   'leg': {'instruction': 'BUY', 'quantity': 3,\n\
                           'instrument': {'asset_type': 'EQUITY', 'symbol': 'AAPL'}},\n\
                   'settings': {'tag': 't-1'}}",
                None,
                None,
            )?;
            let order = validated(build(&spec).unwrap()).unwrap();
            let json = serde_json::to_value(&order).unwrap();
            assert_eq!(json["orderType"], "LIMIT");
            assert_eq!(json["price"], "150.25");
            assert_eq!(json["tag"], "t-1");
            assert_eq!(json["orderLegCollection"][0]["quantity"], 3);
            Ok(())
        });
    }

    #[test]
    fn bad_specs_are_input_failures_that_say_why() {
        run(|py| {
            let typo = py.eval(c"{'kind': 'limit', 'prise': '1'}", None, None)?;
            assert!(message(build(&typo).unwrap_err()).starts_with("invalid order spec:"));

            let zero = py.eval(
                c"{'kind': 'limit', 'price': '0',\n\
                   'leg': {'instruction': 'BUY', 'quantity': 1,\n\
                           'instrument': {'asset_type': 'EQUITY', 'symbol': 'AAPL'}}}",
                None,
                None,
            )?;
            assert!(message(build(&zero).unwrap_err()).contains("positive"));
            Ok(())
        });
    }

    #[test]
    fn validation_names_the_field_that_failed() {
        run(|py| {
            let order = py.eval(
                c"{'orderType': 'MARKET', 'session': 'NORMAL', 'duration': 'DAY', 'price': '1',\n\
                   'orderStrategyType': 'SINGLE',\n\
                   'orderLegCollection': [{'instruction': 'BUY', 'quantity': 1,\n\
                   'instrument': {'symbol': 'AAPL', 'assetType': 'EQUITY'}}]}",
                None,
                None,
            )?;
            let request: OrderRequest = from_py(&order, "order").unwrap();
            let Failure::Sdk(Error::Validation(invalid)) = validated(request).unwrap_err() else {
                panic!("expected a validation failure");
            };
            assert_eq!(invalid.field(), Some("price"));
            Ok(())
        });
    }

    #[test]
    fn option_symbols_round_trip() {
        run(|py| {
            let scope = PyDict::new(py);
            py.run(
                c"import datetime, decimal\n\
                  day = datetime.date(2024, 5, 17)\n\
                  moment = datetime.datetime(2024, 5, 17, tzinfo=datetime.timezone.utc)\n\
                  strike = decimal.Decimal('190')",
                Some(&scope),
                Some(&scope),
            )?;
            let get = |name: &str| scope.get_item(name).map(|v| v.expect("name"));
            let call = py.eval(c"'CALL'", None, None)?;
            let put = py.eval(c"'PUT'", None, None)?;

            let symbol = format_option("aapl", &get("day")?, &put, &get("strike")?).unwrap();
            assert_eq!(symbol, "AAPL  240517P00190000");
            let iso = py.eval(c"'2024-05-17'", None, None)?;
            let half = py.eval(c"'192.5'", None, None)?;
            let other = format_option("SPY", &iso, &call, &half).unwrap();
            assert_eq!(other, "SPY   240517C00192500");

            let parsed = parse_option(&symbol).unwrap();
            assert_eq!(
                parsed,
                json!({"underlying": "AAPL", "expiration": "2024-05-17",
                       "put_call": "PUT", "strike": "190"})
            );
            assert_eq!(parse_option(&other).unwrap()["strike"], "192.5");

            let failure = format_option("AAPL", &get("moment")?, &put, &get("strike")?);
            assert!(message(failure.unwrap_err()).contains("not a datetime"));
            Ok(())
        });
    }

    #[test]
    fn malformed_option_inputs_are_refused() {
        run(|py| {
            let day = py.eval(c"'2024-05-17'", None, None)?;
            let put = py.eval(c"'PUT'", None, None)?;
            let strike = py.eval(c"'190'", None, None)?;
            let bad_right = py.eval(c"'put'", None, None)?;
            assert!(format_option("AAPL", &day, &bad_right, &strike).is_err());
            let not_a_day = py.eval(c"'2024-13-45'", None, None)?;
            assert!(format_option("AAPL", &not_a_day, &put, &strike).is_err());
            let fine = py.eval(c"'190.0005'", None, None)?;
            assert!(format_option("AAPL", &day, &put, &fine).is_err());
            assert!(format_option("TOOLONGROOT", &day, &put, &strike).is_err());
            assert!(parse_option("AAPL 240517P00190000").is_err());
            assert!(parse_option("aapl  240517P00190000").is_err());
            Ok(())
        });
    }
}
