//! `rustyschwab.HistoryStore`: the SDK's local price-history cache, and the
//! candle-to-quote model that turns cached history into paper events.
//!
//! Times are epoch milliseconds, both ends inclusive, as in Schwab's
//! `startDate` and `endDate`. Candles are Schwab's shape
//! (`{"open", "high", "low", "close", "volume", "datetime"}`).

use crate::client::Client;
use crate::convert::{Json, from_py};
use crate::errors::Failure;
use crate::runtime::awaitable;
use pyo3::prelude::*;
use rustyschwab::history::{BarSize, HistoryRequest, HistoryStore, Recorded};
use rustyschwab::paper::synthetic::SyntheticBbo;
use rustyschwab::types::market_data::{Candle, FrequencyType, PriceHistoryResponse};
use std::path::PathBuf;

/// A series request from the keyword arguments every method takes.
fn request(
    symbol: String,
    start_ms: i64,
    end_ms: i64,
    frequency_type: &str,
    frequency: i32,
    extended_hours: bool,
) -> Result<HistoryRequest, Failure> {
    let frequency_type: FrequencyType =
        serde_json::from_value(frequency_type.into()).map_err(|_| {
            rustyschwab::Error::InvalidParameter(format!(
                "frequency_type must be minute, daily, weekly or monthly, not {frequency_type:?}"
            ))
        })?;
    let bars = BarSize::new(frequency_type, frequency)?;
    Ok(HistoryRequest::new(symbol, bars, start_ms, end_ms).with_extended_hours(extended_hours))
}

/// A durable cache of price history and recorded stream payloads.
///
/// Open one with `await HistoryStore.open(path)`. `load` fetches only the
/// spans the file does not hold and serves the rest from disk; `candles`
/// reads without fetching. `record` and `recorded` keep stream payloads
/// (quotes, books) that Schwab has no history API for.
#[pyclass(frozen, name = "HistoryStore", module = "rustyschwab._native")]
#[derive(Debug)]
pub struct PyHistoryStore {
    store: HistoryStore,
}

// Every series method takes the same keyword arguments.
#[allow(clippy::too_many_arguments)]
#[pymethods]
impl PyHistoryStore {
    /// Open or create the cache at `path`, creating its directory.
    #[staticmethod]
    fn open(py: Python<'_>, path: PathBuf) -> PyResult<Bound<'_, PyAny>> {
        awaitable(py, async move {
            Ok(PyHistoryStore {
                store: HistoryStore::open(path).await?,
            })
        })
    }

    fn __repr__(&self) -> String {
        format!("HistoryStore({:?})", self.store.path())
    }

    /// The cache file.
    #[getter]
    fn path(&self) -> PathBuf {
        self.store.path().to_path_buf()
    }

    /// `symbol`'s candles from `start_ms` through `end_ms`, fetching through
    /// `client` the spans the cache lacks. Returns Schwab's price-history
    /// shape, `{"symbol", "empty", "candles"}`. Bars from before today in New
    /// York are kept for good; today's are fetched again on the next call.
    #[pyo3(signature = (client, symbol, start_ms, end_ms, *, frequency_type="minute", frequency=1, extended_hours=false))]
    fn load<'py>(
        &self,
        py: Python<'py>,
        client: &Client,
        symbol: String,
        start_ms: i64,
        end_ms: i64,
        frequency_type: &str,
        frequency: i32,
        extended_hours: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let request = request(
            symbol,
            start_ms,
            end_ms,
            frequency_type,
            frequency,
            extended_hours,
        );
        let (store, shared) = (self.store.clone(), client.shared());
        awaitable(py, async move {
            let request = request?;
            let sdk = shared.ready().await?;
            Ok(Json(store.load(sdk, &request).await?))
        })
    }

    /// The cached candles in the span, without fetching.
    #[pyo3(signature = (symbol, start_ms, end_ms, *, frequency_type="minute", frequency=1, extended_hours=false))]
    fn candles<'py>(
        &self,
        py: Python<'py>,
        symbol: String,
        start_ms: i64,
        end_ms: i64,
        frequency_type: &str,
        frequency: i32,
        extended_hours: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let request = request(
            symbol,
            start_ms,
            end_ms,
            frequency_type,
            frequency,
            extended_hours,
        );
        let store = self.store.clone();
        awaitable(py, async move { Ok(Json(store.candles(&request?).await?)) })
    }

    /// The `(start_ms, end_ms)` parts of the span the cache does not hold in
    /// full.
    #[pyo3(signature = (symbol, start_ms, end_ms, *, frequency_type="minute", frequency=1, extended_hours=false))]
    fn missing<'py>(
        &self,
        py: Python<'py>,
        symbol: String,
        start_ms: i64,
        end_ms: i64,
        frequency_type: &str,
        frequency: i32,
        extended_hours: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let request = request(
            symbol,
            start_ms,
            end_ms,
            frequency_type,
            frequency,
            extended_hours,
        );
        let store = self.store.clone();
        awaitable(py, async move { Ok(store.missing(&request?).await?) })
    }

    /// Store candles for a series, replacing any at the same times. Pass
    /// `complete=(start_ms, end_ms)` only when every bar in that span is
    /// present; `load` will then not fetch it.
    #[pyo3(signature = (symbol, candles, *, frequency_type="minute", frequency=1, extended_hours=false, complete=None))]
    fn put_candles<'py>(
        &self,
        py: Python<'py>,
        symbol: String,
        candles: &Bound<'py, PyAny>,
        frequency_type: &str,
        frequency: i32,
        extended_hours: bool,
        complete: Option<(i64, i64)>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let candles = from_py::<Vec<Candle>>(candles, "candles");
        let request = request(symbol, 0, 0, frequency_type, frequency, extended_hours);
        let store = self.store.clone();
        awaitable(py, async move {
            store.put_candles(&request?, candles?, complete).await?;
            Ok(())
        })
    }

    /// Append recorded stream payloads, each `{"kind", "symbol", "at_ms",
    /// "payload"}`, in one transaction. `kind` names what was recorded, such
    /// as `LEVELONE_EQUITIES`.
    fn record<'py>(
        &self,
        py: Python<'py>,
        entries: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let entries = from_py::<Vec<Recorded>>(entries, "recorded entries");
        let store = self.store.clone();
        awaitable(py, async move {
            store.record_all(entries?).await?;
            Ok(())
        })
    }

    /// Recorded payloads of `kind` for `symbols` in the span, in time order.
    fn recorded<'py>(
        &self,
        py: Python<'py>,
        kind: String,
        symbols: Vec<String>,
        start_ms: i64,
        end_ms: i64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let store = self.store.clone();
        awaitable(py, async move {
            Ok(Json(
                store.recorded(&kind, &symbols, start_ms, end_ms).await?,
            ))
        })
    }
}

/// Paper events for a replay-mode venue from price histories, under the
/// declared `candle-bbo-v1` model: each candle becomes four quotes (open,
/// then low and high, then close) with a synthetic spread and size, and each
/// New York date one session. `model` is `{"candle_ms", "spread_bps",
/// "participation_bps", "regular_hours_only"}`; `histories` are price-history
/// dicts as `HistoryStore.load` returns them. A modeling assumption, not
/// broker conformance: see the paper trading guide.
#[pyfunction]
#[pyo3(signature = (histories, model, *, first_sequence=1))]
pub fn synthesize_quotes(
    py: Python<'_>,
    histories: &Bound<'_, PyAny>,
    model: &Bound<'_, PyAny>,
    first_sequence: u64,
) -> PyResult<Json<Vec<rustyschwab::paper::PaperEvent>>> {
    let built = (|| {
        let histories = from_py::<Vec<PriceHistoryResponse>>(histories, "price histories")?;
        let model = from_py::<SyntheticBbo>(model, "synthetic quote model")?;
        Ok::<_, Failure>(model.events(&histories, first_sequence)?)
    })();
    built.map(Json).map_err(|f| crate::errors::to_pyerr(py, f))
}
