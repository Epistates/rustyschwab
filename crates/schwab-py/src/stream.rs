//! `rustyschwab.Stream`: the Schwab streamer as an async iterator.
//!
//! The SDK's stream client reconnects, replays subscriptions and reports
//! gaps on its own; this class only exposes it. Messages come out of
//! `async for` as dicts, each tagged with a `"type"`:
//!
//! - `"data"`: market data or account activity, as `{"data": [{"service", "timestamp",
//!   "command", "content": [...]}]}`.
//! - `"response"`: the streamer's answer to a login or subscription request.
//! - `"notify"`: a heartbeat or notice.
//! - `"state"`: readiness changed. `ready`, `generation`, `lost_messages` and, when
//!   the stream stopped for good, `ended`. Treat a new `generation` or any loss
//!   as a gap: data from before it is stale, so re-baseline from REST.
//!
//! The iterator finishes after the `"state"` message that carries `ended`.

use crate::convert::Json;
use crate::errors::Failure;
use crate::runtime::awaitable;
use pyo3::prelude::*;
use rustyschwab::types::streaming::{StreamMessage, StreamService};
use rustyschwab::{Error, MessageReceiver, SchwabClient, StreamClient, StreamConfig};
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// A connected streamer session.
///
/// Iterate it with `async for message in stream`, and drive subscriptions with
/// `subscribe` and `unsubscribe`. Close it with `await stream.close()` or an
/// `async with` block; dropping it also stops the connection.
#[pyclass(frozen, module = "rustyschwab._native")]
#[derive(Debug)]
pub struct Stream {
    client: StreamClient,
    receiver: Arc<tokio::sync::Mutex<MessageReceiver>>,
    named_fields: bool,
}

impl Stream {
    /// Log in to the streamer named by the user's preferences.
    pub async fn connect(
        sdk: &SchwabClient,
        config: StreamConfig,
        named_fields: bool,
    ) -> Result<Self, Failure> {
        let preferences = sdk.get_user_preferences().await?;
        let info = preferences
            .streamer_info
            .as_deref()
            .and_then(<[_]>::first)
            .ok_or_else(|| Error::Config("user preferences carry no streamerInfo".into()))?;
        let client = StreamClient::builder()
            .config(config)
            .auth_manager(sdk.auth_manager())
            .streamer_info(info)
            .build()?
            .with_coordinator(sdk.coordinator())?;
        client.connect().await?;
        let receiver = client
            .get_receiver()
            .ok_or_else(|| Error::Config("the stream receiver was already taken".into()))?;
        Ok(Self {
            client,
            receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
            named_fields,
        })
    }
}

fn parse_service(name: &str) -> Result<StreamService, Failure> {
    StreamService::from_api_str(&name.to_ascii_uppercase()).ok_or_else(|| {
        Failure::field(
            "service",
            format!("unknown streamer service {name:?}; expected a name such as LEVELONE_EQUITIES"),
        )
    })
}

/// `fields` (field numbers, or the names `stream_fields` lists) as the CSV the
/// streamer takes.
fn fields_csv(service: StreamService, fields: &[Bound<'_, PyAny>]) -> Result<String, Failure> {
    let table = service.fields();
    let numbers = fields
        .iter()
        .map(|field| {
            if let Ok(number) = field.extract::<u32>() {
                return Ok(number);
            }
            let name: String = field.extract().map_err(|_| {
                Failure::field("fields", "fields must be field numbers or field names")
            })?;
            name.parse::<u32>()
                .ok()
                .or_else(|| table.iter().find(|f| f.name == name).map(|f| f.number))
                .ok_or_else(|| Failure::field("fields", format!("unknown field {name:?}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if numbers.is_empty() {
        return Err(Failure::field("fields", "fields must not be empty"));
    }
    Ok(numbers
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(","))
}

/// A stream message as the dict Python receives.
fn render(message: StreamMessage, named_fields: bool) -> Value {
    let (kind, mut value) = match message {
        StreamMessage::State(status) => ("state", json!(status)),
        StreamMessage::Response(response) => ("response", json!(response)),
        StreamMessage::Notify(notify) => ("notify", json!(notify)),
        StreamMessage::Data(data) if named_fields => {
            let items = data.data.into_iter().map(|item| {
                let table = StreamService::from_api_str(&item.service).map(StreamService::fields);
                let content: Vec<Value> = item
                    .content
                    .into_iter()
                    .map(|content| match table {
                        Some(table) => content.into_named_value(table),
                        None => json!(content),
                    })
                    .collect();
                json!({
                    "service": item.service,
                    "timestamp": item.timestamp,
                    "command": item.command,
                    "content": content,
                })
            });
            ("data", json!({ "data": items.collect::<Vec<_>>() }))
        }
        StreamMessage::Data(data) => ("data", json!(data)),
        other => ("unknown", json!(other)),
    };
    if let Value::Object(map) = &mut value {
        map.insert("type".into(), kind.into());
    } else {
        value = json!({ "type": kind, "value": value });
    }
    value
}

#[pymethods]
impl Stream {
    fn __repr__(&self) -> String {
        format!(
            "Stream(authenticated={}, ready={})",
            self.client.is_authenticated(),
            self.client.status().ready
        )
    }

    fn __aiter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// The next message; raises `StopAsyncIteration` once the stream ended.
    fn __anext__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let receiver = self.receiver.clone();
        let named = self.named_fields;
        awaitable(py, async move {
            match receiver.lock().await.recv().await {
                Some(message) => Ok(Json(render(message, named))),
                None => Err(Failure::EndOfStream),
            }
        })
    }

    fn __aenter__<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        awaitable(py, async move { Ok(slf) })
    }

    #[pyo3(signature = (*_exc))]
    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _exc: &Bound<'py, pyo3::types::PyTuple>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.close(py)
    }

    /// Subscribe to `keys` on `service` (`"LEVELONE_EQUITIES"`, `"CHART_EQUITY"`,
    /// `"ACCT_ACTIVITY"`, ...). Additive: earlier subscriptions stay.
    ///
    /// `fields` picks the fields streamed for the service (numbers or names,
    /// see `stream_fields`), for this and later subscriptions on it.
    /// `ACCT_ACTIVITY` takes the key `"Account Activity"`.
    #[pyo3(signature = (service, keys, *, fields=None))]
    fn subscribe<'py>(
        &self,
        py: Python<'py>,
        service: &str,
        keys: Vec<String>,
        fields: Option<Vec<Bound<'py, PyAny>>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let parsed = parse_service(service).and_then(|service| {
            let csv = fields
                .as_deref()
                .map(|fields| fields_csv(service, fields))
                .transpose()?;
            Ok((service, csv))
        });
        let client = self.client.clone();
        awaitable(py, async move {
            let (service, csv) = parsed?;
            if let Some(csv) = csv {
                client.set_service_fields(service, csv);
            }
            client.subscribe(service, keys).await?;
            Ok(Json(Value::Null))
        })
    }

    /// Stop streaming `keys` on `service`.
    fn unsubscribe<'py>(
        &self,
        py: Python<'py>,
        service: &str,
        keys: Vec<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let parsed = parse_service(service);
        let client = self.client.clone();
        awaitable(py, async move {
            client.unsubscribe(parsed?, keys).await?;
            Ok(Json(Value::Null))
        })
    }

    /// Change the fields streamed for every key already subscribed on
    /// `service`, and for later subscriptions.
    fn view<'py>(
        &self,
        py: Python<'py>,
        service: &str,
        fields: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let parsed = parse_service(service).and_then(|service| {
            let csv = fields_csv(service, &fields)?;
            Ok((service, csv))
        });
        let client = self.client.clone();
        awaitable(py, async move {
            let (service, csv) = parsed?;
            client.view(service, csv).await?;
            Ok(Json(Value::Null))
        })
    }

    /// The keys subscribed per service: `{"LEVELONE_EQUITIES": ["AAPL"]}`.
    fn subscriptions(&self) -> Json<std::collections::BTreeMap<String, Vec<String>>> {
        Json(
            self.client
                .confirmed_subscriptions()
                .into_iter()
                .map(|(service, mut keys)| {
                    keys.sort();
                    (service, keys)
                })
                .collect(),
        )
    }

    /// The stream's current `{"ready", "generation", "lost_messages", "ended"}`.
    fn status(&self) -> Json<rustyschwab::types::streaming::StreamStatus> {
        Json(self.client.status())
    }

    /// Whether the streamer login is live right now.
    #[getter]
    fn is_authenticated(&self) -> bool {
        self.client.is_authenticated()
    }

    /// Log out and close the connection. The iterator then ends.
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.client.clone();
        awaitable(py, async move {
            client.disconnect().await;
            Ok(Json(Value::Null))
        })
    }
}

/// The field table of a streamer service: `[{"number": 1, "name": "bidPrice"}, ...]`.
#[pyfunction]
pub fn stream_fields(service: &str) -> PyResult<Json<Vec<Value>>> {
    let service =
        parse_service(service).map_err(|f| Python::attach(|py| crate::errors::to_pyerr(py, f)))?;
    fn rows(table: &[rustyschwab::types::streaming::Field]) -> Vec<Value> {
        table
            .iter()
            .map(|field| {
                let mut row = Map::new();
                row.insert("number".into(), field.number.into());
                row.insert("name".into(), field.name.into());
                if let Some(nested) = field.nested {
                    row.insert("nested".into(), Value::Array(rows(nested)));
                }
                Value::Object(row)
            })
            .collect()
    }
    Ok(Json(rows(service.fields())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyschwab::types::streaming::StreamStatus;

    /// A data frame as the streamer sends it.
    fn data(service: &str) -> StreamMessage {
        serde_json::from_value(json!({"data": [{
            "service": service, "timestamp": 1, "command": "SUBS",
            "content": [{"key": "AAPL", "1": 189.5, "2": 189.6, "8": 1200}]
        }]}))
        .unwrap()
    }

    #[test]
    fn data_fields_are_named_when_asked() {
        let named = render(data("LEVELONE_EQUITIES"), true);
        assert_eq!(named["type"], "data");
        let content = &named["data"][0]["content"][0];
        assert_eq!(content["key"], "AAPL");
        assert_eq!(content["bidPrice"], 189.5);
        assert_eq!(content["totalVolume"], 1200);
    }

    #[test]
    fn data_fields_stay_numbered_otherwise_or_for_unknown_services() {
        let raw = render(data("LEVELONE_EQUITIES"), false);
        assert_eq!(raw["data"][0]["content"][0]["1"], 189.5);
        let unknown = render(data("SOMETHING_NEW"), true);
        assert_eq!(unknown["data"][0]["content"][0]["1"], 189.5);
    }

    #[test]
    fn state_messages_are_tagged() {
        let state = render(
            StreamMessage::State(StreamStatus {
                ready: true,
                generation: 2,
                lost_messages: 0,
                ended: None,
            }),
            true,
        );
        assert_eq!(state["type"], "state");
        assert_eq!(state["generation"], 2);
    }

    #[test]
    fn fields_resolve_from_numbers_and_names() {
        Python::attach(|py| {
            let fields = vec![
                1u32.into_pyobject(py).unwrap().into_any(),
                "askPrice".into_pyobject(py).unwrap().into_any(),
                "8".into_pyobject(py).unwrap().into_any(),
            ];
            let csv = fields_csv(StreamService::LeveloneEquities, &fields).unwrap();
            assert_eq!(csv, "1,2,8");
            let bad = vec!["nonsense".into_pyobject(py).unwrap().into_any()];
            assert!(fields_csv(StreamService::LeveloneEquities, &bad).is_err());
        });
    }
}
