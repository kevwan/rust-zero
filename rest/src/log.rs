use actix_web::{
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    Error, HttpMessage,
};
use futures::future::{ok, LocalBoxFuture, Ready};
use rust_zero_core::{LogContext, LogField, LogLevel, Logger, TraceContext};
use std::{
    task::{Context, Poll},
    time::{Duration, Instant},
};

use crate::RequestIdValue;

/// Emits a structured event for every completed HTTP request.
pub struct LoggingMiddleware;

impl<S, B> Transform<S, ServiceRequest> for LoggingMiddleware
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: 'static,
    S::Future: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = LoggingMiddlewareService<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(LoggingMiddlewareService { service })
    }
}

pub struct LoggingMiddlewareService<S> {
    service: S,
}

impl<S, B> Service<ServiceRequest> for LoggingMiddlewareService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let method = req.method().clone();
        let path = req.path().to_owned();
        let started_at = Instant::now();
        let future = self.service.call(req);

        Box::pin(async move {
            match future.await {
                Ok(response) => {
                    tracing::info!(
                        method = %method,
                        path = %path,
                        status = response.status().as_u16(),
                        elapsed_ms = started_at.elapsed().as_millis() as u64,
                        "HTTP request completed"
                    );
                    Ok(response)
                }
                Err(error) => {
                    tracing::warn!(
                        method = %method,
                        path = %path,
                        elapsed_ms = started_at.elapsed().as_millis() as u64,
                        error = %error,
                        "HTTP request failed"
                    );
                    Err(error)
                }
            }
        })
    }
}

/// Emits request logs through the standalone `rust-zero-core` structured logger.
#[derive(Debug, Clone)]
pub struct StructuredLogging {
    logger: Logger,
    slow_threshold: Option<Duration>,
}

impl StructuredLogging {
    pub fn new(logger: Logger) -> Self {
        Self {
            logger,
            slow_threshold: None,
        }
    }

    /// Classifies completed calls at or above `threshold` as slow and adds stable transport-aware
    /// fields suitable for log queries and alerts.
    pub fn with_slow_threshold(mut self, threshold: Duration) -> Self {
        assert!(!threshold.is_zero(), "slow-call threshold must be positive");
        self.slow_threshold = Some(threshold);
        self
    }
}

impl<S, B> Transform<S, ServiceRequest> for StructuredLogging
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: 'static,
    S::Future: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = StructuredLoggingService<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(StructuredLoggingService {
            service,
            logger: self.logger.clone(),
            slow_threshold: self.slow_threshold,
        })
    }
}

pub struct StructuredLoggingService<S> {
    service: S,
    logger: Logger,
    slow_threshold: Option<Duration>,
}

impl<S, B> Service<ServiceRequest> for StructuredLoggingService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(context)
    }

    fn call(&self, request: ServiceRequest) -> Self::Future {
        let method = request.method().to_string();
        let path = request.path().to_owned();
        let request_id = request
            .extensions()
            .get::<RequestIdValue>()
            .map(|value| value.as_str().to_owned());
        let trace = request.extensions().get::<TraceContext>().cloned();
        let started_at = Instant::now();
        let future = self.service.call(request);
        let logger = self.logger.clone();
        let slow_threshold = self.slow_threshold;

        Box::pin(async move {
            let context = request_context(request_id, trace);
            match future.await {
                Ok(response) => {
                    let elapsed = started_at.elapsed();
                    let slow = slow_threshold.is_some_and(|threshold| elapsed >= threshold);
                    let mut fields = vec![
                        LogField::new("transport", "http"),
                        LogField::new("method", method),
                        LogField::new("path", path),
                        LogField::new("status", response.status().as_u16()),
                        LogField::new("elapsed_ms", elapsed.as_millis() as u64),
                        LogField::new("slow", slow),
                    ];
                    if let Some(route) = response.request().match_pattern() {
                        fields.push(LogField::new("route", route));
                    }
                    if let Some(threshold) = slow_threshold {
                        fields.push(LogField::new(
                            "slow_threshold_ms",
                            threshold.as_millis() as u64,
                        ));
                    }
                    let _ = logger.log_with_context(
                        if slow { LogLevel::Slow } else { LogLevel::Info },
                        "HTTP request completed",
                        Some(&context),
                        fields,
                    );
                    Ok(response)
                }
                Err(error) => {
                    let elapsed = started_at.elapsed();
                    let slow = slow_threshold.is_some_and(|threshold| elapsed >= threshold);
                    let mut fields = vec![
                        LogField::new("transport", "http"),
                        LogField::new("method", method),
                        LogField::new("path", path),
                        LogField::new("elapsed_ms", elapsed.as_millis() as u64),
                        LogField::new("slow", slow),
                        LogField::new("error", error.to_string()),
                    ];
                    if let Some(threshold) = slow_threshold {
                        fields.push(LogField::new(
                            "slow_threshold_ms",
                            threshold.as_millis() as u64,
                        ));
                    }
                    let _ = logger.log_with_context(
                        LogLevel::Error,
                        "HTTP request failed",
                        Some(&context),
                        fields,
                    );
                    Err(error)
                }
            }
        })
    }
}

fn request_context(request_id: Option<String>, trace: Option<TraceContext>) -> LogContext {
    let mut context = LogContext::new();
    if let Some(request_id) = request_id {
        context = context.with_field(LogField::new("request_id", request_id));
    }
    if let Some(trace) = trace {
        context = context.with_trace(trace);
    }
    context
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RequestId, TraceContextMiddleware};
    use actix_web::{test, web, App, HttpResponse};
    use rust_zero_core::LogConfig;
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[actix_rt::test]
    async fn emits_request_identity_and_trace_context() {
        let output = SharedWriter::default();
        let logger = Logger::to_writer(LogConfig::console("api"), output.clone()).unwrap();
        let app = test::init_service(
            App::new()
                .wrap(StructuredLogging::new(logger))
                .wrap(TraceContextMiddleware::new())
                .wrap(RequestId::new())
                .route(
                    "/",
                    web::get().to(|| async { HttpResponse::NoContent().finish() }),
                ),
        )
        .await;

        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/")
                .insert_header(("x-request-id", "request-42"))
                .insert_header((
                    "traceparent",
                    "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
                ))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 204);

        let bytes = output.0.lock().unwrap().clone();
        let record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record["request_id"], "request-42");
        assert_eq!(record["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(record["method"], "GET");
        assert_eq!(record["status"], 204);
        assert_eq!(record["transport"], "http");
        assert_eq!(record["route"], "/");
        assert_eq!(record["slow"], false);
    }

    #[actix_rt::test]
    async fn classifies_slow_requests_with_queryable_fields() {
        let output = SharedWriter::default();
        let logger = Logger::to_writer(LogConfig::console("api"), output.clone()).unwrap();
        let app = test::init_service(
            App::new()
                .wrap(
                    StructuredLogging::new(logger)
                        .with_slow_threshold(std::time::Duration::from_millis(1)),
                )
                .route(
                    "/users/{id}",
                    web::get().to(|| async {
                        actix_rt::time::sleep(std::time::Duration::from_millis(5)).await;
                        HttpResponse::NoContent().finish()
                    }),
                ),
        )
        .await;

        let response =
            test::call_service(&app, test::TestRequest::get().uri("/users/42").to_request()).await;
        assert_eq!(response.status(), 204);

        let bytes = output.0.lock().unwrap().clone();
        let record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record["level"], "slow");
        assert_eq!(record["route"], "/users/{id}");
        assert_eq!(record["slow"], true);
        assert_eq!(record["slow_threshold_ms"], 1);
    }
}
