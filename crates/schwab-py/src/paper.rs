//! `rustyschwab.PaperBroker`: the offline paper venue.
//!
//! The venue owns no network transport and no credentials. Market input is
//! explicit (sessions, quotes, clock events), time is the events' time, and the
//! same inputs reproduce the same fills, which makes it the backtest engine.
//! Events and orders are dicts; amounts the venue reports are decimal strings,
//! so no float ever stands in for money.

use crate::convert::{Json, from_py};
use crate::runtime::awaitable;
use crate::submission::{Submitted, tagged};
use pyo3::prelude::*;
use schwab_rs::broker::TradingBackend;
use schwab_rs::paper::{PaperBroker, PaperConfig, PaperEvent, PaperEventKind};
use schwab_rs::types::trading::OrderRequest;
use serde_json::Value;
use std::path::PathBuf;

/// A durable, deterministic paper venue.
///
/// Open one with `await PaperBroker.open(path, config)`. Reopening the same
/// path with the same config resumes its state; a different config is refused.
#[pyclass(frozen, name = "PaperBroker", module = "rustyschwab._native")]
#[derive(Debug)]
pub struct PyPaperBroker {
    broker: PaperBroker,
}

/// The journal attempt a paper submission is recorded under.
fn paper_attempt(order_id: &Option<String>) -> Option<String> {
    order_id.as_ref().map(|id| format!("paper-{id}"))
}

#[pymethods]
impl PyPaperBroker {
    /// Open or create the venue's state file. `config` is the paper config:
    /// `model_version`, `accounts` (account name to `initial_cash` and
    /// `limits`), the latencies, `slippage_bps`, `market_collar_bps`,
    /// `fee_per_share`, `max_working_orders`, and optionally `clock`
    /// (`"replay"` or `"wall"`) and the settlement calendar.
    #[staticmethod]
    fn open<'py>(
        py: Python<'py>,
        path: PathBuf,
        config: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let config = from_py::<PaperConfig>(config, "paper configuration");
        awaitable(py, async move {
            let config = config?;
            let broker = PaperBroker::open(path, config).await?;
            Ok(PyPaperBroker { broker })
        })
    }

    fn __repr__(&self) -> String {
        let accounts: Vec<_> = self
            .broker
            .config()
            .accounts
            .keys()
            .map(|name| format!("'{name}'"))
            .collect();
        format!("PaperBroker(accounts=[{}])", accounts.join(", "))
    }

    /// The paper config this venue runs under.
    #[getter]
    fn config(&self) -> Json<PaperConfig> {
        Json(self.broker.config().clone())
    }

    /// Apply one sequenced market event: `{"sequence": 1, "at_ms": 0, "kind":
    /// {"kind": "session", "session_id": "s1", "open": true}}`. Sequences are
    /// contiguous from 1 and an exact retry is idempotent; `rustyschwab.paper`
    /// has builders for the common kinds. Returns `{"mode", "duplicate",
    /// "sequence", "fills"}`.
    fn apply<'py>(
        &self,
        py: Python<'py>,
        event: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let event = from_py::<PaperEvent>(event, "paper event");
        let broker = self.broker.clone();
        awaitable(py, async move { Ok(Json(broker.apply(event?).await?)) })
    }

    /// Apply a list of consecutive events in one transaction, all of them or
    /// none, and return their results in order. A backtest applies a bar's
    /// quotes this way. At most 10,000 events.
    fn apply_batch<'py>(
        &self,
        py: Python<'py>,
        events: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let events = from_py::<Vec<PaperEvent>>(events, "paper events");
        let broker = self.broker.clone();
        awaitable(
            py,
            async move { Ok(Json(broker.apply_batch(events?).await?)) },
        )
    }

    /// Apply a live market input (`{"kind": "quote", ...}`) on a venue with a
    /// wall clock, which assigns its sequence and time.
    fn append_live<'py>(
        &self,
        py: Python<'py>,
        kind: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let kind = from_py::<PaperEventKind>(kind, "paper event kind");
        let broker = self.broker.clone();
        awaitable(
            py,
            async move { Ok(Json(broker.append_live(kind?).await?)) },
        )
    }

    /// Place an order (Schwab's JSON shape) on `account`. Returns
    /// `{"order_id", "location", "attempt_id", "tag"}`. Paper orders need a
    /// tag; one is assigned when the order has none.
    fn place_order<'py>(
        &self,
        py: Python<'py>,
        account: String,
        order: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let order = from_py::<OrderRequest>(order, "order");
        let broker = self.broker.clone();
        awaitable(py, async move {
            let (order, tag) = tagged(order?);
            let response = broker.place_order(&account, &order).await?;
            let attempt = paper_attempt(&response.order_id);
            Ok(Json(Submitted::new(response, tag, attempt)))
        })
    }

    /// Replace a working order. The replacement loses queue priority; its
    /// quantity is the new remaining quantity. Result as for `place_order`.
    fn replace_order<'py>(
        &self,
        py: Python<'py>,
        account: String,
        order_id: String,
        order: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let order = from_py::<OrderRequest>(order, "order");
        let broker = self.broker.clone();
        awaitable(py, async move {
            let (order, tag) = tagged(order?);
            let response = broker.replace_order(&account, &order_id, &order).await?;
            let attempt = paper_attempt(&response.order_id);
            Ok(Json(Submitted::new(response, tag, attempt)))
        })
    }

    /// Request cancellation. With cancel latency configured, fills remain
    /// possible until a later event makes the cancel effective.
    fn cancel_order<'py>(
        &self,
        py: Python<'py>,
        account: String,
        order_id: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let broker = self.broker.clone();
        awaitable(py, async move {
            broker.cancel_order(&account, &order_id).await?;
            Ok(Json(Value::Null))
        })
    }

    /// One order's venue record (`status`, `filled_quantity`, ...).
    fn order<'py>(
        &self,
        py: Python<'py>,
        account: String,
        order_id: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let broker = self.broker.clone();
        awaitable(py, async move {
            Ok(Json(broker.get_order(&account, &order_id).await?))
        })
    }

    /// Order history in ascending ID: up to `limit` (1 to 1000) orders after
    /// `after_id`. Pass the last order's ID as the next `after_id`.
    #[pyo3(signature = (account, *, after_id=0, limit=100))]
    fn orders<'py>(
        &self,
        py: Python<'py>,
        account: String,
        after_id: u64,
        limit: u32,
    ) -> PyResult<Bound<'py, PyAny>> {
        let broker = self.broker.clone();
        awaitable(py, async move {
            Ok(Json(broker.orders(&account, after_id, limit).await?))
        })
    }

    /// Every fill of one order, in execution order.
    fn fills<'py>(
        &self,
        py: Python<'py>,
        account: String,
        order_id: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let broker = self.broker.clone();
        awaitable(py, async move {
            Ok(Json(broker.fills(&account, &order_id).await?))
        })
    }

    /// The venue's latest quote for `symbol`, or `None`. `fresh` says whether
    /// an order would fill against it now.
    fn quote<'py>(&self, py: Python<'py>, symbol: String) -> PyResult<Bound<'py, PyAny>> {
        let broker = self.broker.clone();
        awaitable(py, async move { Ok(Json(broker.quote(&symbol).await?)) })
    }

    /// The account's ledger: cash, positions, realized P&L, fees, working
    /// orders, and equity when every position has a fresh quote.
    fn snapshot<'py>(&self, py: Python<'py>, account: String) -> PyResult<Bound<'py, PyAny>> {
        let broker = self.broker.clone();
        awaitable(
            py,
            async move { Ok(Json(broker.snapshot(&account).await?)) },
        )
    }

    /// The venue's clock as epoch milliseconds: the last event's time in
    /// replay mode, the wall clock in wall mode.
    fn venue_time_ms<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let broker = self.broker.clone();
        awaitable(py, async move {
            Ok(Json(broker.venue_time().await?.timestamp_millis()))
        })
    }

    /// The current session as `(session_id, open)`.
    fn market_session<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let broker = self.broker.clone();
        awaitable(py, async move { Ok(Json(broker.market_session().await?)) })
    }

    /// Halt new orders on this venue. Cancellations still work.
    fn halt<'py>(
        &self,
        py: Python<'py>,
        reason: String,
        operator: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let broker = self.broker.clone();
        awaitable(py, async move {
            broker.halt_submissions(&reason, &operator).await?;
            Ok(Json(Value::Null))
        })
    }

    /// Lift the halt.
    fn clear_halt<'py>(&self, py: Python<'py>, operator: String) -> PyResult<Bound<'py, PyAny>> {
        let broker = self.broker.clone();
        awaitable(py, async move {
            broker.clear_operator_halt(&operator).await?;
            Ok(Json(Value::Null))
        })
    }

    /// The reason new orders are halted, or `None`.
    fn halted<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let broker = self.broker.clone();
        awaitable(py, async move { Ok(Json(broker.operator_halt().await?)) })
    }

    /// Check the venue's durable state against its own history: replays the
    /// fills and ledger and reports `{"model_version", "accounts", "orders",
    /// "event_sequence"}`, or raises if they disagree.
    fn verify<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let broker = self.broker.clone();
        awaitable(
            py,
            async move { Ok(Json(broker.verify_integrity().await?)) },
        )
    }

    /// Stop accepting new orders ahead of shutdown.
    fn begin_drain(&self) {
        self.broker.begin_drain();
    }
}
