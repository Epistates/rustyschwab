//! `rustyschwab.analyze_performance`: performance analytics over fills.
//!
//! The SDK's `performance` module does the work; this converts its inputs and
//! output. Fills and equity points are lists of dicts, amounts may be `str`,
//! `int`, `float` or `decimal.Decimal` (a `Decimal` keeps every digit), and
//! the report comes back as a dict in which money is a decimal string, as it
//! is on the paper venue, and ratios are floats. The call is CPU-only, so it
//! is a plain function and releases the GIL while it computes.

use crate::convert::{Json, from_py, from_value, to_json};
use crate::errors::{Failure, to_pyerr};
use pyo3::prelude::*;
use rustyschwab::performance::{self, EquityPoint, Fill, PerformanceOptions, PerformanceReport};
use serde_json::{Map, Number, Value};

/// `value` as JSON, with an error that names the argument it came from.
fn named(value: &Bound<'_, PyAny>, name: &str) -> Result<Value, Failure> {
    to_json(value).map_err(|failure| match failure {
        Failure::Input {
            field: None,
            message,
        } => Failure::field(name, message),
        other => other,
    })
}

/// Analyze `fills` and the `equity` curve they ran through.
///
/// `fills` are dicts with `symbol`, `side` (`"BUY"`, `"SELL"` or a Schwab
/// instruction such as `"SELL_SHORT"`), `quantity`, `price`, `at_ms` and,
/// optionally, `fee` and `multiplier` (100 for equity options), in time
/// order. A paper venue fill works as is once it has its order's `symbol` and
/// `side`. `equity` is a list of `{"at_ms", "equity"}` points in time order;
/// `None` or an empty list builds the curve from the fills.
///
/// `starting_equity` is the account value before the first fill.
/// `risk_free_rate` is annual, as a fraction. `benchmark` maps a symbol to
/// its `(first_price, last_price)` for an equal-weight buy-and-hold return.
/// `marks` maps a symbol to the price that values its open lots, which are
/// otherwise valued at the symbol's last fill price.
///
/// Returns the report as a dict. Raises `InvalidParameter` for fills or
/// points out of order, a non-positive quantity or price, a negative fee and
/// the like.
#[pyfunction]
#[pyo3(signature = (fills, equity=None, *, starting_equity, risk_free_rate=0.0, periods_per_year=performance::DEFAULT_PERIODS_PER_YEAR, benchmark=None, marks=None))]
#[allow(clippy::too_many_arguments)]
pub fn analyze_performance<'py>(
    py: Python<'py>,
    fills: &Bound<'py, PyAny>,
    equity: Option<&Bound<'py, PyAny>>,
    starting_equity: &Bound<'py, PyAny>,
    risk_free_rate: f64,
    periods_per_year: u32,
    benchmark: Option<&Bound<'py, PyAny>>,
    marks: Option<&Bound<'py, PyAny>>,
) -> PyResult<Json<PerformanceReport>> {
    let read = || -> Result<(Vec<Fill>, Vec<EquityPoint>, PerformanceOptions), Failure> {
        let fills: Vec<Fill> = from_py(fills, "fills")?;
        let equity: Vec<EquityPoint> = match equity {
            Some(equity) if !equity.is_none() => from_py(equity, "equity")?,
            _ => Vec::new(),
        };
        let mut options = Map::new();
        options.insert(
            "starting_equity".into(),
            named(starting_equity, "starting_equity")?,
        );
        options.insert(
            "risk_free_rate".into(),
            Number::from_f64(risk_free_rate)
                .map(Value::Number)
                .ok_or_else(|| Failure::field("risk_free_rate", "risk_free_rate must be finite"))?,
        );
        options.insert("periods_per_year".into(), periods_per_year.into());
        for (key, value) in [("benchmark", benchmark), ("marks", marks)] {
            if let Some(value) = value.filter(|value| !value.is_none()) {
                options.insert(key.into(), named(value, key)?);
            }
        }
        let options = from_value(Value::Object(options), "performance options")?;
        Ok((fills, equity, options))
    };
    let (fills, equity, options) = read().map_err(|failure| to_pyerr(py, failure))?;
    py.detach(|| performance::analyze(&fills, &equity, &options))
        .map(Json)
        .map_err(|error| to_pyerr(py, error.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::types::PyDict;

    fn run<R>(check: impl FnOnce(Python<'_>) -> PyResult<R>) -> R {
        Python::attach(|py| check(py).expect("python check"))
    }

    fn analyze<'py>(
        py: Python<'py>,
        fills: &str,
        equity: Option<&str>,
        extra: &str,
    ) -> PyResult<Bound<'py, PyAny>> {
        let scope = PyDict::new(py);
        py.run(
            std::ffi::CString::new(format!(
                "import decimal\nD = decimal.Decimal\nfills = {fills}\nequity = {}\nextra = {extra}",
                equity.unwrap_or("None")
            ))
            .expect("source")
            .as_c_str(),
            Some(&scope),
            Some(&scope),
        )?;
        let get = |name: &str| scope.get_item(name).map(|v| v.expect(name));
        let report = analyze_performance(
            py,
            &get("fills")?,
            Some(&get("equity")?),
            &get("extra")?.get_item("starting_equity")?,
            0.0,
            252,
            None,
            None,
        )?;
        Ok(pythonize::pythonize(py, &report.0)?)
    }

    #[test]
    fn decimal_amounts_give_a_dict_with_decimal_strings() {
        run(|py| {
            let report = analyze(
                py,
                "[{'symbol': 'ABC', 'side': 'BUY', 'quantity': D('10'), 'price': D('100.5'), 'at_ms': 1},\n\
                  {'symbol': 'ABC', 'side': 'SELL', 'quantity': 10, 'price': '101.25', 'fee': '0.5', 'at_ms': 2}]",
                None,
                "{'starting_equity': D('1000')}",
            )?;
            assert_eq!(report.get_item("net_pnl")?.extract::<String>()?, "7.00");
            assert_eq!(
                report.get_item("equity_source")?.extract::<String>()?,
                "fills"
            );
            assert!(report.get_item("sharpe")?.is_none());
            assert!(report.get_item("net_return")?.extract::<f64>()? > 0.0);
            let trips = report.get_item("round_trips")?;
            assert_eq!(trips.len()?, 1);
            assert_eq!(
                trips
                    .get_item(0)?
                    .get_item("direction")?
                    .extract::<String>()?,
                "long"
            );
            Ok(())
        });
    }

    #[test]
    fn bad_input_is_an_error() {
        run(|py| {
            let unordered = analyze(
                py,
                "[{'symbol': 'A', 'side': 'BUY', 'quantity': 1, 'price': 1, 'at_ms': 2},\n\
                  {'symbol': 'A', 'side': 'SELL', 'quantity': 1, 'price': 1, 'at_ms': 1}]",
                None,
                "{'starting_equity': 1}",
            );
            assert!(unordered.is_err());
            let typo = analyze(
                py,
                "[{'symbol': 'A', 'side': 'HOLD', 'quantity': 1, 'price': 1, 'at_ms': 2}]",
                None,
                "{'starting_equity': 1}",
            );
            assert!(typo.is_err());
            Ok(())
        });
    }
}
