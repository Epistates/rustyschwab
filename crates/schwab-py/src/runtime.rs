//! The shared tokio runtime and the bridge to asyncio.
//!
//! Every SDK future runs on one multi-threaded runtime that `pyo3-async-runtimes`
//! owns for the process; Python's event loop only ever waits on the result. A
//! call hands back a real coroutine, not a bare `asyncio.Future`, so it works
//! with `asyncio.create_task`, `gather` and `wait_for` alike, and nothing
//! starts until it is awaited.

use crate::errors::{Failure, to_pyerr};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyCFunction;
use pyo3_async_runtimes::tokio as bridge;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

/// Name the runtime's worker threads, so they are recognizable in a profiler.
pub fn init() {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().thread_name("rustyschwab-rt");
    bridge::init(builder);
}

/// Run synchronous SDK constructors inside the runtime's context: some of
/// them spawn background work and need to find it.
pub fn enter() -> tokio::runtime::EnterGuard<'static> {
    bridge::get_runtime().enter()
}

/// `async def run(start): return await start()` from `rustyschwab._aio`.
fn runner(py: Python<'_>) -> PyResult<&Bound<'_, PyAny>> {
    static RUNNER: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    RUNNER
        .get_or_try_init(py, || {
            Ok(py.import("rustyschwab._aio")?.getattr("run")?.unbind())
        })
        .map(|runner| runner.bind(py))
}

/// A coroutine that awaits `fut` on the shared runtime.
///
/// `fut` is not polled until the coroutine first runs, inside a running event
/// loop, which is what `asyncio` needs to deliver the result. A failure is
/// raised as the matching `rustyschwab` exception. Cancelling the awaiting
/// task drops the future.
pub fn awaitable<'py, T, F>(py: Python<'py>, fut: F) -> PyResult<Bound<'py, PyAny>>
where
    F: Future<Output = Result<T, Failure>> + Send + 'static,
    T: for<'a> IntoPyObject<'a> + Send + 'static,
{
    let pending = Arc::new(Mutex::new(Some(fut)));
    let start = PyCFunction::new_closure(py, None, None, move |args, _kwargs| {
        let fut = pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("already awaited"))?;
        bridge::future_into_py(args.py(), async move {
            fut.await
                .map_err(|failure| Python::attach(|py| to_pyerr(py, failure)))
        })
        .map(Bound::unbind)
    })?;
    runner(py)?.call1((start,))
}
