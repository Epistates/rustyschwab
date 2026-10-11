//! Python bindings for the Schwab SDK, published as `rustyschwab`.
//!
//! This is the native half. The package in `python/rustyschwab` re-exports it
//! next to the exception classes (`errors.py`) and the paper event builders
//! (`paper.py`), which are plain Python.
//!
//! The design in three rules:
//!
//! - **asyncio native.** Every I/O method returns a coroutine that runs on one
//!   shared tokio runtime, never holding the GIL while it waits.
//! - **Data, not wrappers.** Requests and responses are `dict`s and `list`s in
//!   Schwab's JSON shape, converted with serde, so the SDK's own validation is
//!   what decides if a request is acceptable.
//! - **Nothing weaker than the SDK.** Orders use the journaled submission path;
//!   the binding adds no way around risk checks, halts or the request budget.

mod client;
mod config;
mod convert;
mod errors;
mod paper;
mod runtime;
mod stream;
mod submission;

use pyo3::prelude::*;

/// The native extension module, imported as `rustyschwab._native`.
#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    runtime::init();
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    module.add("__sdk_version__", schwab_rs::VERSION)?;
    module.add_class::<client::Client>()?;
    module.add_class::<stream::Stream>()?;
    module.add_class::<paper::PyPaperBroker>()?;
    module.add_function(wrap_pyfunction!(stream::stream_fields, module)?)?;
    Ok(())
}
