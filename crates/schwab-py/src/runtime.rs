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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// The process that built the runtime. Its worker threads do not exist in a
/// `fork()`ed child, so a future handed to it would never be polled.
static RUNTIME_PID: AtomicU32 = AtomicU32::new(0);

/// Name the runtime's worker threads, so they are recognizable in a profiler.
pub fn init() {
    RUNTIME_PID.store(std::process::id(), Ordering::Relaxed);
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().thread_name("rustyschwab-rt");
    bridge::init(builder);
}

/// Refuse to hand work to a runtime this process inherited through `fork()`:
/// the call would hang forever. Linux `multiprocessing` forks by default
/// before Python 3.14.
fn ensure_runtime_is_ours(runtime_pid: u32) -> PyResult<()> {
    if runtime_pid == std::process::id() {
        return Ok(());
    }
    Err(pyo3::exceptions::PyRuntimeError::new_err(
        "rustyschwab was used in a process created by fork(); its runtime does not survive \
         a fork. Import and use it only after forking, or start workers with the 'spawn' \
         or 'forkserver' method (multiprocessing.set_start_method)",
    ))
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
        ensure_runtime_is_ours(RUNTIME_PID.load(Ordering::Relaxed))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runtime_built_by_another_process_is_refused() {
        ensure_runtime_is_ours(std::process::id()).unwrap();
        // A fork child has a new pid and inherits the parent's recorded one.
        let error = ensure_runtime_is_ours(std::process::id().wrapping_add(1)).unwrap_err();
        assert!(error.to_string().contains("fork"), "{error}");
    }
}
