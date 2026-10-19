//! The engine's otel wire, ported from the controller's
//! src/otel_logging.rs with one adaptation: robots keeps its
//! stderr tier. The controller replaced its logger with the bridge
//! entirely; the engine's windmill job logs (stderr) are the
//! fallback diagnosis path when the collector is down — so one
//! fan-out logger serves both sinks: env_logger to stderr (the
//! two-tier #273 behavior unchanged) and the OpenTelemetry bridge
//! to OTLP/HTTP (the loki wire the estate reads). Endpoint and
//! service name come from the same two env vars the controller
//! reads, same defaults; the env var carries the collector BASE
//! url and the signal path (/v1/logs) is appended here. Export
//! failures drop in the background — the door never waits on
//! telemetry. The exit path (emit_result) flushes the provider:
//! the batch timer thread dies with the process, and python's
//! atexit hook is this half.

use std::sync::OnceLock;

use log::{Log, Metadata, Record};
use opentelemetry::KeyValue;
use opentelemetry_appender_log::OpenTelemetryLogBridge;
use opentelemetry_otlp::LogExporter;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::logs::{SdkLogger, SdkLoggerProvider};
use opentelemetry_sdk::Resource;

/// The provider stays reachable for the whole process so the exit
/// path can flush it (the bridge only holds an internal handle).
static LOGGER_PROVIDER: OnceLock<SdkLoggerProvider> = OnceLock::new();

/// One `log` facade, two sinks: stderr keeps the windmill job log
/// alive, the bridge carries the same records to loki via the
/// collector. Neither sink sees the other fail.
struct FanOutLogger {
    stderr: env_logger::Logger,
    otel: OpenTelemetryLogBridge<SdkLoggerProvider, SdkLogger>,
}

impl Log for FanOutLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.stderr.enabled(metadata) || self.otel.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        self.stderr.log(record);
        self.otel.log(record);
    }

    fn flush(&self) {
        self.stderr.flush();
        self.otel.flush();
    }
}

pub fn init_telemetry() {
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").unwrap_or_else(|_| {
        "http://godon-observability-opentelemetry-collector.godon-observability.svc.cluster.local:4318"
            .to_string()
    });
    let service_name =
        std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "godon-robots".to_string());

    let exporter = LogExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/logs"))
        .build()
        .expect("build the OTLP log exporter");

    let resource = Resource::builder()
        .with_attributes([KeyValue::new("service.name", service_name)])
        .build();

    let provider = SdkLoggerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();
    let provider = LOGGER_PROVIDER.get_or_init(|| provider);

    let otel = OpenTelemetryLogBridge::new(provider);
    let stderr =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).build();
    log::set_boxed_logger(Box::new(FanOutLogger { stderr, otel }))
        .expect("install the fan-out logger");
    log::set_max_level(log::LevelFilter::Debug);
}

/// Flush pending records to the collector before the process exits.
/// Called from emit_result — every component's terminal beat — so a
/// one-shot job hands its logs over on the way out instead of
/// racing the batch timer. Never blocks on retries.
pub fn shutdown_telemetry() {
    if let Some(provider) = LOGGER_PROVIDER.get() {
        let _ = provider.shutdown();
    }
}
