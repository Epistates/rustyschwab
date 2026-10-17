//! OTLP export of spans and metrics, behind the `otel` feature.
//!
//! [`init_otel`] reads the standard `OTEL_*` environment, and only when it
//! names an endpoint does it build an exporter. Without one it installs the
//! same fmt subscriber as the other `init_*` helpers, so an application can
//! ship one binary and turn export on by configuration alone.
//!
//! Only spans leave the process. Log events (`warn!`, `debug!`, ...) carry
//! free-form fields such as error text, file paths and symbols, so the
//! OpenTelemetry layer is filtered to spans: what is exported is exactly
//! what a span declares as a field, and the spans this workspace creates
//! declare nothing but static operation labels.

use std::fmt;

use metrics_opentelemetry::OpenTelemetryMetrics;
use opentelemetry::Key;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{ExporterBuildError, MetricExporter, SpanExporter};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::resource::{EnvResourceDetector, TelemetryResourceDetector};
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanExporter as SpanExport};
use tracing::Subscriber;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{Layer, Registry};

pub use metrics_opentelemetry::OpenTelemetryRecorder;

/// Instrumentation scope name of the spans and metrics this crate exports.
const SCOPE: &str = "rustyschwab";

const SDK_DISABLED: &str = "OTEL_SDK_DISABLED";
const ENDPOINT: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
const TRACES_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT";
const METRICS_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT";
const PROTOCOL: &str = "OTEL_EXPORTER_OTLP_PROTOCOL";
const TRACES_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL";
const METRICS_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL";
const TRACES_EXPORTER: &str = "OTEL_TRACES_EXPORTER";
const METRICS_EXPORTER: &str = "OTEL_METRICS_EXPORTER";

/// Why OTLP export could not be set up. Both variants are configuration
/// errors: the environment asked for something this build cannot do.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OtelError {
    /// The environment selects an OTLP wire protocol other than
    /// `http/protobuf`, the only one compiled in. Sending it anyway would
    /// post HTTP to a port that speaks gRPC.
    #[error("{variable}={value:?} is not supported; only http/protobuf OTLP is built in")]
    UnsupportedProtocol {
        /// The environment variable that carried the value.
        variable: &'static str,
        /// The value it carried.
        value: String,
    },
    /// The OTLP exporter rejected its configuration (a malformed endpoint
    /// or header, for instance).
    #[error("could not build the OTLP {signal} exporter: {source}")]
    Exporter {
        /// `traces` or `metrics`.
        signal: &'static str,
        /// The exporter's own error.
        #[source]
        source: ExporterBuildError,
    },
}

/// Which signals the environment asks to export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Signals {
    traces: bool,
    metrics: bool,
}

/// Decide what to export from `get`, an environment lookup.
///
/// A signal is exported when an endpoint is set for it (the shared
/// `OTEL_EXPORTER_OTLP_ENDPOINT` or its own), unless `OTEL_SDK_DISABLED` is
/// true or its `OTEL_*_EXPORTER` is `none`. An endpoint with a protocol this
/// build cannot speak is an error rather than a silent fallback.
fn signals(get: &dyn Fn(&str) -> Option<String>) -> Result<Signals, OtelError> {
    let set = |name: &str| get(name).is_some_and(|v| !v.trim().is_empty());
    let is =
        |name: &str, want: &str| get(name).is_some_and(|v| v.trim().eq_ignore_ascii_case(want));
    if is(SDK_DISABLED, "true") {
        return Ok(Signals {
            traces: false,
            metrics: false,
        });
    }
    let wanted =
        |endpoint: &str, exporter: &str| (set(ENDPOINT) || set(endpoint)) && !is(exporter, "none");
    let traces = wanted(TRACES_ENDPOINT, TRACES_EXPORTER);
    let metrics = wanted(METRICS_ENDPOINT, METRICS_EXPORTER);
    for (on, own) in [(traces, TRACES_PROTOCOL), (metrics, METRICS_PROTOCOL)] {
        if !on {
            continue;
        }
        for variable in [own, PROTOCOL] {
            if let Some(value) = get(variable).filter(|v| !v.trim().is_empty())
                && value.trim() != "http/protobuf"
            {
                return Err(OtelError::UnsupportedProtocol { variable, value });
            }
        }
    }
    Ok(Signals { traces, metrics })
}

/// The resource describing this process: telemetry SDK attributes, then
/// `OTEL_RESOURCE_ATTRIBUTES`, then `OTEL_SERVICE_NAME`. `default_service`
/// names the service only when neither of those did, in place of the SDK's
/// `unknown_service:<executable>`.
fn resource(default_service: &str) -> Resource {
    let sdk = Resource::builder().build();
    let named = sdk
        .get(&Key::from_static_str("service.name"))
        .is_some_and(|name| !name.as_str().starts_with("unknown_service"));
    if named {
        sdk
    } else {
        // Same detectors minus the one that invents the service name, so
        // the default can take its place.
        Resource::builder_empty()
            .with_detector(Box::new(TelemetryResourceDetector))
            .with_detector(Box::new(EnvResourceDetector::new()))
            .with_service_name(default_service.to_owned())
            .build()
    }
}

/// A tracer provider batching spans to `exporter`.
fn tracer_provider(resource: Resource, exporter: impl SpanExport + 'static) -> SdkTracerProvider {
    SdkTracerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build()
}

/// A meter provider pushing to `exporter` on the periodic reader's interval.
fn meter_provider(resource: Resource, exporter: impl PushMetricExporter) -> SdkMeterProvider {
    SdkMeterProvider::builder()
        .with_resource(resource)
        .with_reader(PeriodicReader::builder(exporter).build())
        .build()
}

/// The `tracing` layer that turns spans into `provider`'s spans.
///
/// It sees spans only: events are filtered out so log fields never become
/// span events (see the module docs).
fn span_layer<S>(provider: &SdkTracerProvider) -> impl Layer<S> + use<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    tracing_opentelemetry::layer()
        .with_tracer(provider.tracer(SCOPE))
        .with_filter(filter_fn(|metadata| metadata.is_span()))
}

/// The fmt layer every `init_*` helper installs.
fn fmt_layer<S, W>(writer: W) -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
}

/// Owns the OTLP providers and flushes them when dropped.
///
/// Keep it alive until the program is done tracing, typically a `let
/// _telemetry = ...` at the top of `main`: dropping it exports the spans and
/// metrics still buffered and shuts the providers down, which blocks for at
/// most the exporter timeout. A guard that exports nothing (no endpoint was
/// configured, or a subscriber was already installed) drops for free.
#[must_use = "dropping the guard immediately shuts OTLP export down"]
pub struct OtelGuard {
    tracer: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
    resource: Resource,
    metrics: bool,
}

impl OtelGuard {
    /// Whether spans are being exported.
    pub fn exports_traces(&self) -> bool {
        self.tracer.is_some()
    }

    /// A `metrics` recorder that exports over OTLP, when the environment
    /// configures a metrics endpoint.
    ///
    /// The meter provider and its export thread are created on the first
    /// call, so an application that never installs the recorder pays
    /// nothing. Install the result with `metrics::set_global_recorder`, or
    /// fan it out beside another recorder.
    pub fn metrics_recorder(&mut self) -> Result<Option<OpenTelemetryRecorder>, OtelError> {
        if !self.metrics {
            return Ok(None);
        }
        if self.meter.is_none() {
            let exporter = MetricExporter::builder()
                .with_http()
                .build()
                .map_err(|source| OtelError::Exporter {
                    signal: "metrics",
                    source,
                })?;
            self.meter = Some(meter_provider(self.resource.clone(), exporter));
        }
        Ok(self.meter.as_ref().map(recorder))
    }
}

impl fmt::Debug for OtelGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtelGuard")
            .field("traces", &self.tracer.is_some())
            .field("metrics", &self.metrics)
            .finish_non_exhaustive()
    }
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.tracer.take()
            && let Err(error) = provider.shutdown()
        {
            tracing::warn!(%error, "OTLP trace export did not shut down cleanly");
        }
        if let Some(provider) = self.meter.take()
            && let Err(error) = provider.shutdown()
        {
            tracing::warn!(%error, "OTLP metrics export did not shut down cleanly");
        }
    }
}

/// A `metrics` recorder that records into `provider`'s meter.
fn recorder(provider: &SdkMeterProvider) -> OpenTelemetryRecorder {
    OpenTelemetryRecorder::new(OpenTelemetryMetrics::new(provider.meter(SCOPE)))
}

/// Install the global subscriber, exporting spans over OTLP when the
/// standard environment asks for it.
///
/// With no endpoint configured this is the same subscriber as
/// [`init_tracing_with_level`](super::init_tracing_with_level), writing to
/// `writer`: fmt output filtered by `RUST_LOG`, `default_level` otherwise.
/// With one, the same fmt layer runs beside a layer exporting spans through
/// the OTLP HTTP/protobuf exporter, which reads its endpoint, headers,
/// timeout and compression from `OTEL_EXPORTER_OTLP_*`. `RUST_LOG` filters
/// both, so a span below the active level is neither printed nor exported.
///
/// `default_service` names the service when `OTEL_SERVICE_NAME` and
/// `OTEL_RESOURCE_ATTRIBUTES` do not. Set `OTEL_SDK_DISABLED=true` or
/// `OTEL_TRACES_EXPORTER=none` to switch export off while keeping the
/// endpoint configured.
///
/// Like the other helpers it keeps a subscriber the application already
/// installed; in that case nothing is exported and a warning says so. Hold
/// the returned guard until exit so buffered spans are flushed.
///
/// # Errors
///
/// [`OtelError`] when the environment selects a protocol other than
/// `http/protobuf` or the exporter rejects its configuration.
///
/// # Example
///
/// ```no_run
/// # fn main() -> Result<(), rustyschwab::telemetry::OtelError> {
/// let _telemetry = rustyschwab::telemetry::init_otel("my-service", "info", std::io::stderr)?;
/// // ... run the application; spans are flushed when `_telemetry` drops.
/// # Ok(())
/// # }
/// ```
pub fn init_otel<W>(
    default_service: &str,
    default_level: &str,
    writer: W,
) -> Result<OtelGuard, OtelError>
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let signals = signals(&|name| std::env::var(name).ok())?;
    let resource = resource(default_service);
    let filter = super::filter(default_level);

    let tracer = if signals.traces {
        let exporter = SpanExporter::builder()
            .with_http()
            .build()
            .map_err(|source| OtelError::Exporter {
                signal: "traces",
                source,
            })?;
        Some(tracer_provider(resource.clone(), exporter))
    } else {
        None
    };
    let installed = Registry::default()
        .with(filter)
        .with(fmt_layer(writer))
        .with(tracer.as_ref().map(span_layer))
        .try_init();
    let tracer = match installed {
        Ok(()) => tracer,
        Err(_) => {
            // The application's subscriber stays, and OTLP export is off for
            // it: shut down the provider built for the one that lost.
            if let Some(provider) = tracer {
                let _ = provider.shutdown();
                tracing::warn!(
                    "a tracing subscriber is already installed; OTLP span export is off"
                );
            }
            None
        }
    };
    Ok(OtelGuard {
        tracer,
        meter: None,
        resource,
        metrics: signals.metrics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use opentelemetry::KeyValue;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::trace::InMemorySpanExporter;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    /// An in-memory span exporter that also keeps the resource the provider
    /// hands it, which the stock one does not expose.
    #[derive(Debug, Clone, Default)]
    struct Capture {
        spans: InMemorySpanExporter,
        resource: std::sync::Arc<std::sync::Mutex<Option<Resource>>>,
    }

    impl Capture {
        fn resource(&self) -> Option<Resource> {
            self.resource.lock().unwrap().clone()
        }
    }

    impl SpanExport for Capture {
        async fn export(
            &self,
            batch: Vec<opentelemetry_sdk::trace::SpanData>,
        ) -> opentelemetry_sdk::error::OTelSdkResult {
            self.spans.export(batch).await
        }

        fn set_resource(&mut self, resource: &Resource) {
            *self.resource.lock().unwrap() = Some(resource.clone());
        }
    }

    fn attr(resource: &Resource, key: &'static str) -> Option<String> {
        resource
            .get(&opentelemetry::Key::from_static_str(key))
            .map(|v| v.as_str().into_owned())
    }

    #[test]
    fn nothing_is_exported_without_an_endpoint() {
        let none = Signals {
            traces: false,
            metrics: false,
        };
        assert_eq!(signals(&env(&[])).unwrap(), none);
        // Headers, service name and an empty endpoint are not an endpoint.
        let configured = env(&[
            (ENDPOINT, "  "),
            ("OTEL_EXPORTER_OTLP_HEADERS", "authorization=Bearer x"),
            ("OTEL_SERVICE_NAME", "svc"),
        ]);
        assert_eq!(signals(&configured).unwrap(), none);
    }

    #[test]
    fn an_endpoint_enables_its_own_signals() {
        let shared = signals(&env(&[(ENDPOINT, "http://collector:4318")])).unwrap();
        assert!(shared.traces && shared.metrics);
        let traces_only = signals(&env(&[(
            TRACES_ENDPOINT,
            "http://collector:4318/v1/traces",
        )]));
        assert_eq!(
            traces_only.unwrap(),
            Signals {
                traces: true,
                metrics: false
            }
        );
        let metrics_only = signals(&env(&[(
            METRICS_ENDPOINT,
            "http://collector:4318/v1/metrics",
        )]));
        assert_eq!(
            metrics_only.unwrap(),
            Signals {
                traces: false,
                metrics: true
            }
        );
    }

    #[test]
    fn export_can_be_switched_off_with_the_endpoint_left_in_place() {
        let disabled = env(&[(ENDPOINT, "http://c:4318"), (SDK_DISABLED, "TRUE")]);
        assert!(!signals(&disabled).unwrap().traces);
        let no_traces = env(&[(ENDPOINT, "http://c:4318"), (TRACES_EXPORTER, "none")]);
        let signals = signals(&no_traces).unwrap();
        assert!(!signals.traces && signals.metrics);
    }

    #[test]
    fn a_protocol_this_build_cannot_speak_is_refused() {
        let grpc = env(&[(ENDPOINT, "http://c:4317"), (PROTOCOL, "grpc")]);
        let error = signals(&grpc).unwrap_err();
        assert!(matches!(
            error,
            OtelError::UnsupportedProtocol {
                variable: PROTOCOL,
                ..
            }
        ));
        // The per-signal variable wins the diagnosis, and a disabled signal
        // is not checked.
        let json = env(&[
            (TRACES_ENDPOINT, "http://c"),
            (TRACES_PROTOCOL, "http/json"),
        ]);
        assert!(signals(&json).is_err());
        let metrics_only = env(&[(METRICS_ENDPOINT, "http://c"), (TRACES_PROTOCOL, "grpc")]);
        assert!(signals(&metrics_only).is_ok());
        let ok = env(&[(ENDPOINT, "http://c"), (PROTOCOL, "http/protobuf")]);
        assert!(signals(&ok).is_ok());
    }

    #[test]
    fn the_default_service_name_applies_only_when_the_environment_names_none() {
        // The test environment sets neither OTEL_SERVICE_NAME nor
        // OTEL_RESOURCE_ATTRIBUTES; the SDK's own env detectors are covered
        // by its tests.
        if std::env::var_os("OTEL_SERVICE_NAME").is_some()
            || std::env::var_os("OTEL_RESOURCE_ATTRIBUTES").is_some()
        {
            return;
        }
        let resource = resource("schwab-test");
        assert_eq!(
            attr(&resource, "service.name").as_deref(),
            Some("schwab-test")
        );
        assert_eq!(
            attr(&resource, "telemetry.sdk.language").as_deref(),
            Some("rust")
        );
    }

    #[test]
    fn a_span_is_exported_with_the_resource_and_its_fields_but_not_events() {
        let exporter = Capture::default();
        let resource = Resource::builder_empty()
            .with_service_name("schwab-test")
            .with_attribute(KeyValue::new("deployment.environment", "ci"))
            .build();
        let provider = tracer_provider(resource, exporter.clone());
        let subscriber = Registry::default().with(span_layer(&provider));

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("schwab.request", op = "get_quotes");
            let _entered = span.enter();
            // A log event inside the span, carrying the kind of detail that
            // must stay in the process.
            tracing::warn!(account = "1234567890", "a free-form log line");
        });
        provider.force_flush().unwrap();

        // The provider hands its resource to the exporter, which is what an
        // OTLP exporter sends beside the spans.
        let exported = exporter.resource().expect("the provider set a resource");
        assert_eq!(
            attr(&exported, "service.name").as_deref(),
            Some("schwab-test")
        );
        assert_eq!(
            attr(&exported, "deployment.environment").as_deref(),
            Some("ci")
        );

        let spans = exporter.spans.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1, "{spans:?}");
        let span = &spans[0];
        assert_eq!(span.name, "schwab.request");
        assert!(
            span.attributes
                .iter()
                .any(|kv| kv.key.as_str() == "op" && kv.value.as_str() == "get_quotes")
        );
        assert_eq!(
            span.events.len(),
            0,
            "log events must not become span events"
        );
        assert!(
            !format!("{span:?}").contains("1234567890"),
            "an event field reached the exported span"
        );
        assert_eq!(span.instrumentation_scope.name(), SCOPE);
    }

    #[test]
    fn shutting_the_provider_down_flushes_the_batch() {
        let exporter = Capture::default();
        let provider = tracer_provider(Resource::builder_empty().build(), exporter.clone());
        tracing::subscriber::with_default(Registry::default().with(span_layer(&provider)), || {
            let _entered = tracing::info_span!("mcp.tool", tool = "get_quotes").entered();
        });
        // No `force_flush`: the span is still batched until shutdown, as it
        // is for the spans of a process that is exiting.
        provider.shutdown().unwrap();
        assert_eq!(exporter.spans.get_finished_spans().unwrap().len(), 1);
    }

    #[test]
    fn a_metric_recorded_through_the_facade_is_exported_with_the_resource() {
        let exporter = InMemoryMetricExporter::default();
        let resource = Resource::builder_empty()
            .with_service_name("schwab-test")
            .build();
        let provider = meter_provider(resource, exporter.clone());
        let recorder = recorder(&provider);

        metrics::with_local_recorder(&recorder, || {
            metrics::counter!(crate::metrics::HTTP_REQUESTS_TOTAL, "op" => "get_quotes")
                .increment(3);
        });
        provider.force_flush().unwrap();

        let finished = exporter.get_finished_metrics().unwrap();
        let resource_metrics = finished.last().expect("one export");
        assert_eq!(
            attr(resource_metrics.resource(), "service.name").as_deref(),
            Some("schwab-test")
        );
        let metric = resource_metrics
            .scope_metrics()
            .flat_map(|scope| scope.metrics())
            .find(|metric| metric.name() == crate::metrics::HTTP_REQUESTS_TOTAL)
            .expect("the counter was exported");
        let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
            panic!("a counter exports as a u64 sum");
        };
        assert_eq!(sum.data_points().map(|p| p.value()).sum::<u64>(), 3);
    }

    #[test]
    fn a_guard_without_an_exporter_drops_quietly_and_offers_no_recorder() {
        let mut guard = OtelGuard {
            tracer: None,
            meter: None,
            resource: resource("schwab-test"),
            metrics: false,
        };
        assert!(!guard.exports_traces());
        assert!(guard.metrics_recorder().unwrap().is_none());
    }
}
