use opentelemetry::trace::TraceId;
use tracing_subscriber::{prelude::*, EnvFilter, Registry};

/// Fetch an opentelemetry::trace::TraceId as hex through the full tracing stack
pub fn get_trace_id() -> Option<TraceId> {
    use opentelemetry::trace::TraceContextExt as _; // opentelemetry::Context -> opentelemetry::trace::Span
    use tracing_opentelemetry::OpenTelemetrySpanExt as _; // tracing::Span to opentelemetry::Context

    match tracing::Span::current()
        .context()
        .span()
        .span_context()
        .trace_id()
    {
        TraceId::INVALID => None,
        valid => Some(valid),
    }
}

#[cfg(feature = "telemetry")]
fn init_tracer_provider() -> opentelemetry_sdk::trace::SdkTracerProvider {
    use opentelemetry_otlp::{SpanExporter, WithExportConfig};
    use opentelemetry_sdk::{Resource, trace::SdkTracerProvider};

    let otlp_endpoint = std::env::var("OPENTELEMETRY_ENDPOINT_URL")
        .expect("Need a otel tracing collector configured via OPENTELEMETRY_ENDPOINT_URL");

    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(otlp_endpoint)
        .build()
        .expect("Failed to create OTLP span exporter");

    SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_service_name("restate-operator")
                .build(),
        )
        .build()
}

/// Initialize tracing
pub fn init() {
    // Setup tracing layers
    #[cfg(feature = "telemetry")]
    let telemetry = {
        use opentelemetry::trace::TracerProvider;
        let provider = init_tracer_provider();
        let tracer = provider.tracer("restate-operator");
        // Set the global tracer provider so it stays alive
        opentelemetry::global::set_tracer_provider(provider);
        tracing_opentelemetry::layer().with_tracer(tracer)
    };

    let logger = tracing_subscriber::fmt::layer().compact();
    let env_filter = EnvFilter::try_from_default_env()
        .or(EnvFilter::try_new("info"))
        .unwrap();

    // Decide on layers
    #[cfg(feature = "telemetry")]
    let collector = Registry::default()
        .with(telemetry)
        .with(logger)
        .with(env_filter);
    #[cfg(not(feature = "telemetry"))]
    let collector = Registry::default().with(logger).with(env_filter);

    // Initialize tracing
    tracing::subscriber::set_global_default(collector).unwrap();
}

#[cfg(test)]
mod test {
    // This test only works when telemetry is initialized fully
    // and requires OPENTELEMETRY_ENDPOINT_URL pointing to a valid server
    #[cfg(feature = "telemetry")]
    #[test]
    #[ignore = "requires a trace exporter"]
    fn get_trace_id_returns_valid_traces() {
        use super::*;
        super::init();
        #[tracing::instrument(name = "test_span")] // need to be in an instrumented fn
        fn test_trace_id() -> Option<TraceId> {
            get_trace_id()
        }
        assert_ne!(test_trace_id(), None, "valid trace");
        assert_ne!(test_trace_id(), Some(TraceId::INVALID), "valid trace");
    }
}
