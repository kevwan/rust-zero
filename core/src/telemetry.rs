use std::{fmt, time::Duration};

use opentelemetry::{
    global,
    propagation::{Extractor, TextMapPropagator},
    trace::{SpanKind, Status, TraceContextExt, Tracer},
    Context, KeyValue,
};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    propagation::TraceContextPropagator,
    trace::{Sampler, SdkTracerProvider},
    Resource,
};

use crate::TraceContext;

/// OTLP wire transport used to export spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtlpTransport {
    Grpc,
    HttpBinary,
}

/// OpenTelemetry tracer-provider configuration.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    pub service_name: String,
    pub endpoint: String,
    pub transport: OtlpTransport,
    pub sample_ratio: f64,
    pub export_timeout: Duration,
}

impl TelemetryConfig {
    pub fn new(
        service_name: impl Into<String>,
        endpoint: impl Into<String>,
        transport: OtlpTransport,
    ) -> Self {
        Self {
            service_name: service_name.into(),
            endpoint: endpoint.into(),
            transport,
            sample_ratio: 1.0,
            export_timeout: Duration::from_secs(10),
        }
    }

    pub fn with_sample_ratio(mut self, ratio: f64) -> Self {
        self.sample_ratio = ratio;
        self
    }

    pub fn with_export_timeout(mut self, timeout: Duration) -> Self {
        self.export_timeout = timeout;
        self
    }
}

/// Owns the global OpenTelemetry tracer provider and flushes it during shutdown.
#[derive(Debug)]
pub struct Telemetry {
    provider: SdkTracerProvider,
}

impl Telemetry {
    /// Configures a batched OTLP exporter and installs it as the global tracer provider.
    pub fn init(config: TelemetryConfig) -> Result<Self, TelemetryError> {
        validate_config(&config)?;

        let exporter = match config.transport {
            OtlpTransport::Grpc => opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .with_endpoint(config.endpoint)
                .with_timeout(config.export_timeout)
                .build(),
            OtlpTransport::HttpBinary => opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_endpoint(config.endpoint)
                .with_timeout(config.export_timeout)
                .build(),
        }
        .map_err(|error| TelemetryError::Exporter(error.to_string()))?;

        Ok(Self::install(
            &config.service_name,
            config.sample_ratio,
            Some(exporter),
        ))
    }

    /// Installs a recording provider without an exporter.
    ///
    /// This is useful for propagation-only deployments and deterministic middleware tests.
    pub fn local(
        service_name: impl Into<String>,
        sample_ratio: f64,
    ) -> Result<Self, TelemetryError> {
        let service_name = service_name.into();
        validate_service_and_ratio(&service_name, sample_ratio)?;
        Ok(Self::install(&service_name, sample_ratio, None))
    }

    fn install(
        service_name: &str,
        sample_ratio: f64,
        exporter: Option<opentelemetry_otlp::SpanExporter>,
    ) -> Self {
        let builder = SdkTracerProvider::builder()
            .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
                sample_ratio,
            ))))
            .with_resource(
                Resource::builder()
                    .with_service_name(service_name.to_owned())
                    .build(),
            );
        let provider = match exporter {
            Some(exporter) => builder.with_batch_exporter(exporter).build(),
            None => builder.build(),
        };
        global::set_text_map_propagator(TraceContextPropagator::new());
        global::set_tracer_provider(provider.clone());
        Self { provider }
    }

    pub fn force_flush(&self) -> Result<(), TelemetryError> {
        self.provider
            .force_flush()
            .map_err(|error| TelemetryError::Flush(error.to_string()))
    }

    pub fn shutdown(&self) -> Result<(), TelemetryError> {
        self.provider
            .shutdown()
            .map_err(|error| TelemetryError::Shutdown(error.to_string()))
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        let _ = self.provider.shutdown();
    }
}

/// Semantic kind of an exported span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetrySpanKind {
    Client,
    Server,
    Internal,
    Producer,
    Consumer,
}

/// An exportable span that also exposes rust-zero's W3C context representation.
pub struct TelemetrySpan {
    context: Context,
    trace_context: Option<TraceContext>,
    ended: bool,
}

impl fmt::Debug for TelemetrySpan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TelemetrySpan")
            .field("trace_context", &self.trace_context)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

impl TelemetrySpan {
    pub fn start(
        name: impl Into<String>,
        kind: TelemetrySpanKind,
        parent: Option<&TraceContext>,
        attributes: impl IntoIterator<Item = (&'static str, String)>,
    ) -> Self {
        let parent_context = parent.map(parent_otel_context).unwrap_or_default();
        let tracer = global::tracer("rust-zero");
        let builder = tracer
            .span_builder(name.into())
            .with_kind(match kind {
                TelemetrySpanKind::Client => SpanKind::Client,
                TelemetrySpanKind::Server => SpanKind::Server,
                TelemetrySpanKind::Internal => SpanKind::Internal,
                TelemetrySpanKind::Producer => SpanKind::Producer,
                TelemetrySpanKind::Consumer => SpanKind::Consumer,
            })
            .with_attributes(
                attributes
                    .into_iter()
                    .map(|(key, value)| KeyValue::new(key, value)),
            );
        let span = tracer.build_with_context(builder, &parent_context);
        let context = Context::current_with_span(span);
        let trace_context = to_rust_zero_context(&context);
        Self {
            context,
            trace_context,
            ended: false,
        }
    }

    pub fn trace_context(&self) -> Option<&TraceContext> {
        self.trace_context.as_ref()
    }

    pub fn set_attribute(&self, key: &'static str, value: impl Into<String>) {
        self.context
            .span()
            .set_attribute(KeyValue::new(key, value.into()));
    }

    pub fn set_error(&self, description: impl Into<String>) {
        self.context
            .span()
            .set_status(Status::error(description.into()));
    }

    pub fn end(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        if !self.ended {
            self.context.span().end();
            self.ended = true;
        }
    }
}

impl Drop for TelemetrySpan {
    fn drop(&mut self) {
        self.finish();
    }
}

struct TraceParentCarrier<'a>(&'a str);

impl Extractor for TraceParentCarrier<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        key.eq_ignore_ascii_case("traceparent").then_some(self.0)
    }

    fn keys(&self) -> Vec<&str> {
        vec!["traceparent"]
    }
}

fn parent_otel_context(parent: &TraceContext) -> Context {
    TraceContextPropagator::new().extract(&TraceParentCarrier(&parent.traceparent()))
}

fn to_rust_zero_context(context: &Context) -> Option<TraceContext> {
    let span = context.span();
    let span_context = span.span_context();
    if !span_context.is_valid() {
        return None;
    }
    TraceContext::parse(&format!(
        "00-{}-{}-{:02x}",
        span_context.trace_id(),
        span_context.span_id(),
        span_context.trace_flags().to_u8()
    ))
    .ok()
}

fn validate_config(config: &TelemetryConfig) -> Result<(), TelemetryError> {
    validate_service_and_ratio(&config.service_name, config.sample_ratio)?;
    if config.endpoint.trim().is_empty() {
        return Err(TelemetryError::EmptyEndpoint);
    }
    if config.export_timeout.is_zero() {
        return Err(TelemetryError::InvalidTimeout);
    }
    Ok(())
}

fn validate_service_and_ratio(service_name: &str, sample_ratio: f64) -> Result<(), TelemetryError> {
    if service_name.trim().is_empty() {
        return Err(TelemetryError::EmptyServiceName);
    }
    if !(0.0..=1.0).contains(&sample_ratio) || !sample_ratio.is_finite() {
        return Err(TelemetryError::InvalidSampleRatio);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetryError {
    EmptyServiceName,
    EmptyEndpoint,
    InvalidSampleRatio,
    InvalidTimeout,
    Exporter(String),
    Flush(String),
    Shutdown(String),
}

impl fmt::Display for TelemetryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyServiceName => formatter.write_str("telemetry service name cannot be empty"),
            Self::EmptyEndpoint => formatter.write_str("telemetry endpoint cannot be empty"),
            Self::InvalidSampleRatio => {
                formatter.write_str("telemetry sample ratio must be between zero and one")
            }
            Self::InvalidTimeout => {
                formatter.write_str("telemetry export timeout must be greater than zero")
            }
            Self::Exporter(error) => write!(formatter, "telemetry exporter error: {error}"),
            Self::Flush(error) => write!(formatter, "telemetry flush error: {error}"),
            Self::Shutdown(error) => write!(formatter, "telemetry shutdown error: {error}"),
        }
    }
}

impl std::error::Error for TelemetryError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_configuration() {
        assert_eq!(
            Telemetry::local("", 1.0).unwrap_err(),
            TelemetryError::EmptyServiceName
        );
        assert_eq!(
            Telemetry::local("api", 1.1).unwrap_err(),
            TelemetryError::InvalidSampleRatio
        );
    }

    #[test]
    fn creates_exportable_child_contexts() {
        let telemetry = Telemetry::local("users-api", 1.0).unwrap();
        let parent =
            TraceContext::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        let span = TelemetrySpan::start(
            "GET /users",
            TelemetrySpanKind::Server,
            Some(&parent),
            [("http.request.method", "GET".to_owned())],
        );
        let context = span.trace_context().unwrap();

        assert_eq!(context.trace_id(), parent.trace_id());
        assert_ne!(context.span_id(), parent.span_id());
        span.end();
        telemetry.force_flush().unwrap();
    }
}
