//! Python objects to SDK request types and back.
//!
//! Requests arrive as plain Python data in Schwab's JSON shape. They are first
//! read into a `serde_json::Value`, which is where Python-only types get their
//! JSON spelling, then deserialized into the SDK type, so the SDK's own serde
//! rules and `validate()` decide what is acceptable. Responses go the other
//! way through `pythonize`, so a Schwab object is a `dict`, a list a `list`.

use crate::errors::Failure;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Number, Value};

/// A response value that reaches Python as `dict`, `list`, `str`, ... by way
/// of its `Serialize` implementation.
#[derive(Debug)]
pub struct Json<T>(pub T);

impl<'py, T: Serialize> IntoPyObject<'py> for Json<T> {
    type Target = PyAny;
    type Output = Bound<'py, PyAny>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
        Ok(pythonize::pythonize(py, &self.0)?)
    }
}

/// A Python class looked up once: `decimal.Decimal`, `datetime.datetime`, ...
fn class<'py>(
    cell: &'static PyOnceLock<Py<PyAny>>,
    py: Python<'py>,
    module: &str,
    name: &str,
) -> PyResult<&'py Bound<'py, PyAny>> {
    cell.get_or_try_init(py, || Ok(py.import(module)?.getattr(name)?.unbind()))
        .map(|class| class.bind(py))
}

pub(crate) fn is_instance(
    cell: &'static PyOnceLock<Py<PyAny>>,
    obj: &Bound<'_, PyAny>,
    module: &str,
    name: &str,
) -> PyResult<bool> {
    obj.is_instance(class(cell, obj.py(), module, name)?)
}

/// What `obj` is, for an error message.
fn type_name(obj: &Bound<'_, PyAny>) -> String {
    obj.get_type()
        .name()
        .map(|name| name.to_string())
        .unwrap_or_else(|_| "object".into())
}

fn at(path: &str) -> String {
    if path.is_empty() {
        "argument".into()
    } else {
        path.into()
    }
}

/// `obj` as the JSON value Schwab's API would carry.
///
/// Beyond `None`, `bool`, `int`, `float`, `str`, `list`/`tuple` and `dict`:
/// `decimal.Decimal` becomes its exact decimal string, a timezone-aware
/// `datetime.datetime` its RFC 3339 string, a `datetime.date` its ISO date,
/// an `enum.Enum` its value and an `os.PathLike` its path. A naive datetime
/// is refused rather than guessed to be UTC.
pub fn to_json(obj: &Bound<'_, PyAny>) -> Result<Value, Failure> {
    json_at(obj, "")
}

fn json_at(obj: &Bound<'_, PyAny>, path: &str) -> Result<Value, Failure> {
    let fail = |message: String| Failure::field(path, message);
    let python = |error: PyErr| Failure::field(path, error.to_string());
    if obj.is_none() {
        return Ok(Value::Null);
    }
    if obj.is_instance_of::<PyBool>() {
        return Ok(Value::Bool(obj.extract().map_err(python)?));
    }
    if obj.is_instance_of::<PyInt>() {
        return if let Ok(n) = obj.extract::<i64>() {
            Ok(Value::from(n))
        } else if let Ok(n) = obj.extract::<u64>() {
            Ok(Value::from(n))
        } else {
            Err(fail(format!("{} is out of range", at(path))))
        };
    }
    if obj.is_instance_of::<PyFloat>() {
        let n: f64 = obj.extract().map_err(python)?;
        return Number::from_f64(n)
            .map(Value::Number)
            .ok_or_else(|| fail(format!("{} must be a finite number", at(path))));
    }
    if obj.is_instance_of::<PyString>() {
        return Ok(Value::String(obj.extract().map_err(python)?));
    }
    if let Ok(dict) = obj.cast::<PyDict>() {
        let mut map = Map::with_capacity(dict.len());
        for (key, value) in dict.iter() {
            let key: String = key.extract().map_err(|_| {
                fail(format!(
                    "{} has a {} key; keys must be strings",
                    at(path),
                    type_name(&key)
                ))
            })?;
            let child = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            map.insert(key, json_at(&value, &child)?);
        }
        return Ok(Value::Object(map));
    }
    if obj.is_instance_of::<PyList>() || obj.is_instance_of::<PyTuple>() {
        let items = obj
            .try_iter()
            .map_err(python)?
            .enumerate()
            .map(|(index, item)| json_at(&item.map_err(python)?, &format!("{path}[{index}]")))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Value::Array(items));
    }
    scalar_object(obj, path)
}

/// The standard-library scalar types that have a JSON spelling.
fn scalar_object(obj: &Bound<'_, PyAny>, path: &str) -> Result<Value, Failure> {
    static DECIMAL: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    static DATETIME: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    static DATE: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    static ENUM: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    static PATHLIKE: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    let python = |error: PyErr| Failure::field(path, error.to_string());

    if is_instance(&DECIMAL, obj, "decimal", "Decimal").map_err(python)? {
        let text: String = obj.str().and_then(|s| s.extract()).map_err(python)?;
        return Ok(Value::String(text));
    }
    if is_instance(&DATETIME, obj, "datetime", "datetime").map_err(python)? {
        if obj.getattr("tzinfo").map_err(python)?.is_none() {
            return Err(Failure::field(
                path,
                format!("{} is a naive datetime; attach a timezone", at(path)),
            ));
        }
        let text: String = obj
            .call_method0("isoformat")
            .and_then(|s| s.extract())
            .map_err(python)?;
        return Ok(Value::String(text));
    }
    if is_instance(&DATE, obj, "datetime", "date").map_err(python)? {
        let text: String = obj
            .call_method0("isoformat")
            .and_then(|s| s.extract())
            .map_err(python)?;
        return Ok(Value::String(text));
    }
    if is_instance(&ENUM, obj, "enum", "Enum").map_err(python)? {
        return json_at(&obj.getattr("value").map_err(python)?, path);
    }
    if is_instance(&PATHLIKE, obj, "os", "PathLike").map_err(python)? {
        let text: String = obj
            .py()
            .import("os")
            .and_then(|os| os.call_method1("fspath", (obj,)))
            .and_then(|s| s.extract())
            .map_err(python)?;
        return Ok(Value::String(text));
    }
    if obj.is_instance_of::<PyBytes>() {
        return Err(Failure::field(
            path,
            format!("{} is bytes; pass a str", at(path)),
        ));
    }
    Err(Failure::field(
        path,
        format!("{} has unsupported type {}", at(path), type_name(obj)),
    ))
}

/// Read `obj` as the SDK request type `T`; `what` names it in the message.
pub fn from_py<T: DeserializeOwned>(obj: &Bound<'_, PyAny>, what: &str) -> Result<T, Failure> {
    from_value(to_json(obj)?, what)
}

/// Deserialize `value` as `T`, reporting what the SDK's serde rules refused.
pub fn from_value<T: DeserializeOwned>(value: Value, what: &str) -> Result<T, Failure> {
    serde_json::from_value(value)
        .map_err(|error| Failure::input(format!("invalid {what}: {error}")))
}

/// The enum `T` spelled `name` on the wire, for arguments that are one of a
/// few strings (a quote field, a movers index).
pub fn name_of<T: DeserializeOwned>(name: &str, what: &str) -> Result<T, Failure> {
    from_value(Value::String(name.to_owned()), what)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::types::PyDict;
    use rustyschwab::types::trading::OrderRequest;
    use serde_json::json;

    fn run<R>(check: impl FnOnce(Python<'_>) -> PyResult<R>) -> R {
        Python::attach(|py| check(py).expect("python check"))
    }

    #[test]
    fn plain_data_keeps_its_json_shape() {
        run(|py| {
            let obj = py.eval(
                c"{'a': [1, 2.5, None, True, 'x'], 'b': {'c': (1, 2)}}",
                None,
                None,
            )?;
            assert_eq!(
                to_json(&obj).unwrap(),
                json!({"a": [1, 2.5, null, true, "x"], "b": {"c": [1, 2]}})
            );
            Ok(())
        });
    }

    #[test]
    fn python_scalars_get_a_json_spelling() {
        run(|py| {
            let scope = PyDict::new(py);
            py.run(
                c"import decimal, datetime, enum, pathlib\n\
                  class Side(enum.Enum):\n    BUY = 'BUY'\n\
                  value = {'d': decimal.Decimal('150.25'),\n\
                           'when': datetime.datetime(2026, 1, 2, 3, 4, 5, tzinfo=datetime.timezone.utc),\n\
                           'day': datetime.date(2026, 1, 2), 'side': Side.BUY,\n\
                           'p': pathlib.PurePosixPath('/tmp/t.json')}",
                Some(&scope),
                Some(&scope),
            )?;
            let value = scope.get_item("value")?.expect("value");
            assert_eq!(
                to_json(&value).unwrap(),
                json!({"d": "150.25", "when": "2026-01-02T03:04:05+00:00", "day": "2026-01-02",
                       "side": "BUY", "p": "/tmp/t.json"})
            );
            Ok(())
        });
    }

    #[test]
    fn naive_datetimes_and_odd_types_are_refused_with_their_path() {
        run(|py| {
            let naive = py.eval(
                c"{'when': __import__('datetime').datetime(2026, 1, 1)}",
                None,
                None,
            )?;
            let Failure::Input { field, message } = to_json(&naive).unwrap_err() else {
                panic!("expected an input failure");
            };
            assert_eq!(field.as_deref(), Some("when"));
            assert!(message.contains("naive datetime"));

            let odd = py.eval(c"{'legs': [{'q': object()}]}", None, None)?;
            let Failure::Input { field, .. } = to_json(&odd).unwrap_err() else {
                panic!("expected an input failure");
            };
            assert_eq!(field.as_deref(), Some("legs[0].q"));

            let nan = py.eval(c"float('nan')", None, None)?;
            assert!(to_json(&nan).is_err());
            Ok(())
        });
    }

    #[test]
    fn orders_deserialize_through_the_sdk_rules() {
        run(|py| {
            let order = py.eval(
                c"{'orderType': 'LIMIT', 'session': 'NORMAL', 'duration': 'DAY', 'price': 150.25,\n\
                   'orderStrategyType': 'SINGLE',\n\
                   'orderLegCollection': [{'instruction': 'BUY', 'quantity': 3,\n\
                   'instrument': {'symbol': 'AAPL', 'assetType': 'EQUITY'}}]}",
                None,
                None,
            )?;
            let request: OrderRequest = from_py(&order, "order").unwrap();
            assert_eq!(
                request.price.map(|p| p.to_string()).as_deref(),
                Some("150.25")
            );

            let typo = py.eval(c"{'ordertype': 'LIMIT'}", None, None)?;
            let Failure::Input { message, .. } =
                from_py::<OrderRequest>(&typo, "order").unwrap_err()
            else {
                panic!("expected an input failure");
            };
            assert!(message.starts_with("invalid order:"), "{message}");
            Ok(())
        });
    }

    #[test]
    fn responses_become_python_containers() {
        run(|py| {
            let out =
                Json(json!({"symbol": "AAPL", "bid": 1.5, "tags": [1, null]})).into_pyobject(py)?;
            let dict = out.cast::<PyDict>()?;
            assert_eq!(dict.len(), 3);
            assert_eq!(out.get_item("symbol")?.extract::<String>()?, "AAPL");
            Ok(())
        });
    }
}
