use rust_zero_core::{OtlpTransport, Telemetry, TelemetryConfig, TelemetrySpan, TelemetrySpanKind};
use std::env;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:4317".to_owned());
    let telemetry = Telemetry::init(TelemetryConfig::new(
        "rust-zero-example",
        endpoint,
        OtlpTransport::Grpc,
    ))?;
    let span = TelemetrySpan::start(
        "example.startup",
        TelemetrySpanKind::Internal,
        None,
        [("deployment.environment", "development".to_owned())],
    );
    span.end();
    telemetry.force_flush()?;
    Ok(())
}
