//! `init_otel` with no endpoint installs the plain fmt subscriber and no
//! exporter, so one binary can ship with `otel` compiled in and export only
//! where the environment asks for it.
//!
//! Sets process environment and the global subscriber, so it is the only
//! test in this binary.
#![cfg(feature = "otel")]
#![allow(unsafe_code)] // `remove_var` is unsafe in edition 2024; this runs before any other thread exists.

use schwab_rs::telemetry::init_otel;

#[test]
fn no_endpoint_means_no_exporter() {
    // SAFETY: the only test in this binary, and no thread has been spawned.
    unsafe {
        for name in [
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
            "OTEL_SDK_DISABLED",
        ] {
            std::env::remove_var(name);
        }
        // Everything else a deployment sets is not an endpoint.
        std::env::set_var("OTEL_SERVICE_NAME", "configured-but-silent");
        std::env::set_var("OTEL_EXPORTER_OTLP_HEADERS", "x-key=unused");
    }

    let mut guard = init_otel("schwab-test", "info", std::io::sink).expect("plain init succeeds");
    assert!(!guard.exports_traces());
    assert!(
        guard
            .metrics_recorder()
            .expect("no exporter to build")
            .is_none()
    );

    // The subscriber is installed and working: spans and events are fine.
    let _entered = tracing::info_span!("schwab.request", op = "get_quotes").entered();
    tracing::info!("still logs");
    // The global subscriber is ours, so a second init keeps it and reports
    // no export either.
    let second = init_otel("schwab-test", "info", std::io::sink).expect("second init");
    assert!(!second.exports_traces());
}
