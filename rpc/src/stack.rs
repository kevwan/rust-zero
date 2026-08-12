//! Generated-service-independent client and server assembly.

use crate::{BearerToken, RpcMetrics, RpcMetricsLayer};
use futures::FutureExt;
use http::{Request, Response};
use http_body::{Body, Frame};
use pin_project_lite::pin_project;
use rust_zero_core::{
    AdaptiveShedder, AuthFailure, CircuitBreaker, CircuitBreakerConfig, CircuitBreakerPermit,
    CircuitOutcome, LoadShedderConfig, LogContext, LogField, LogLevel, Logger, ShedPermit,
    TraceContext, TraceFlags,
};
use std::{
    collections::HashMap,
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tonic::{body::BoxBody, Code, Status};
use tower::{Layer, Service};

type Validator<T> = dyn Fn(&str) -> Option<T> + Send + Sync;

#[derive(Clone)]
struct RpcSlowLogConfig {
    logger: Logger,
    threshold: Duration,
}

/// Creates the common server layer once and applies it to every generated Tonic service.
///
/// The layer installs authentication, W3C trace extraction, panic recovery, adaptive admission
/// control, result-aware per-method circuit breaking, and complete unary/streaming metrics. Add it
/// to [`tonic::transport::Server`] before adding generated services; those services need no
/// per-service interceptors.
pub struct RpcServerStackBuilder<T = ()> {
    metrics: RpcMetrics,
    auth: Option<Arc<Validator<T>>>,
    shedder: Option<AdaptiveShedder>,
    breaker_config: Option<CircuitBreakerConfig>,
    slow_log: Option<RpcSlowLogConfig>,
}

impl RpcServerStackBuilder<()> {
    pub fn new(metrics: RpcMetrics) -> Self {
        Self {
            metrics,
            auth: None,
            shedder: None,
            breaker_config: Some(CircuitBreakerConfig::rolling(
                rust_zero_core::RollingCircuitBreakerConfig::new(),
            )),
            slow_log: None,
        }
    }
}

impl<T> RpcServerStackBuilder<T>
where
    T: Clone + Send + Sync + 'static,
{
    pub fn with_bearer_auth<U>(
        self,
        validator: impl Fn(&str) -> Option<U> + Send + Sync + 'static,
    ) -> RpcServerStackBuilder<U>
    where
        U: Clone + Send + Sync + 'static,
    {
        RpcServerStackBuilder {
            metrics: self.metrics,
            auth: Some(Arc::new(validator)),
            shedder: self.shedder,
            breaker_config: self.breaker_config,
            slow_log: self.slow_log,
        }
    }

    pub fn with_load_shedder(mut self, config: LoadShedderConfig) -> Self {
        self.shedder = Some(AdaptiveShedder::new(config));
        self
    }

    /// Selects the result-aware circuit-breaker policy installed independently per gRPC method.
    pub fn with_circuit_breaker(mut self, config: CircuitBreakerConfig) -> Self {
        self.breaker_config = Some(config);
        self
    }

    /// Disables the default per-method rolling circuit breaker.
    pub fn without_circuit_breaker(mut self) -> Self {
        self.breaker_config = None;
        self
    }

    /// Logs every completed gRPC call and classifies calls at or above `threshold` as slow.
    /// Streaming calls are finalized from their trailers so the record contains the final status.
    pub fn with_slow_call_logging(mut self, logger: Logger, threshold: Duration) -> Self {
        assert!(
            !threshold.is_zero(),
            "gRPC slow-call threshold must be positive"
        );
        self.slow_log = Some(RpcSlowLogConfig { logger, threshold });
        self
    }

    pub fn build(self) -> RpcServerStack<T> {
        RpcServerStack {
            metrics: RpcMetricsLayer::new(self.metrics),
            auth: self.auth,
            shedder: self.shedder,
            breakers: self.breaker_config.map(|config| {
                Arc::new(ServerCircuitBreakers {
                    config,
                    by_method: Mutex::new(HashMap::new()),
                })
            }),
            slow_log: self.slow_log,
        }
    }
}

pub struct RpcServerStack<T> {
    metrics: RpcMetricsLayer,
    auth: Option<Arc<Validator<T>>>,
    shedder: Option<AdaptiveShedder>,
    breakers: Option<Arc<ServerCircuitBreakers>>,
    slow_log: Option<RpcSlowLogConfig>,
}

impl<T> Clone for RpcServerStack<T> {
    fn clone(&self) -> Self {
        Self {
            metrics: self.metrics.clone(),
            auth: self.auth.clone(),
            shedder: self.shedder.clone(),
            breakers: self.breakers.clone(),
            slow_log: self.slow_log.clone(),
        }
    }
}

impl<S, T> Layer<S> for RpcServerStack<T>
where
    T: Clone + Send + Sync + 'static,
{
    type Service = RpcServerStackService<crate::metrics::RpcMetricsService<S>, T>;

    fn layer(&self, inner: S) -> Self::Service {
        RpcServerStackService {
            inner: self.metrics.layer(inner),
            auth: self.auth.clone(),
            shedder: self.shedder.clone(),
            breakers: self.breakers.clone(),
            slow_log: self.slow_log.clone(),
        }
    }
}

#[derive(Clone)]
pub struct RpcServerStackService<S, T> {
    inner: S,
    auth: Option<Arc<Validator<T>>>,
    shedder: Option<AdaptiveShedder>,
    breakers: Option<Arc<ServerCircuitBreakers>>,
    slow_log: Option<RpcSlowLogConfig>,
}

struct ServerCircuitBreakers {
    config: CircuitBreakerConfig,
    by_method: Mutex<HashMap<String, Arc<CircuitBreaker>>>,
}

impl ServerCircuitBreakers {
    fn acquire(&self, method: &str) -> Option<CircuitBreakerPermit> {
        let breaker = {
            let mut breakers = self
                .by_method
                .lock()
                .expect("gRPC server circuit-breaker map lock poisoned");
            Arc::clone(
                breakers
                    .entry(method.to_owned())
                    .or_insert_with(|| Arc::new(CircuitBreaker::new(self.config))),
            )
        };
        breaker.acquire()
    }
}

impl<S, T, RequestBody, ResponseBody> Service<Request<RequestBody>> for RpcServerStackService<S, T>
where
    S: Service<Request<RequestBody>, Response = Response<ResponseBody>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    RequestBody: Send + 'static,
    ResponseBody: Body<Data = bytes::Bytes> + Send + 'static,
    ResponseBody::Error: Into<tonic::codegen::StdError>,
    T: Clone + Send + Sync + 'static,
{
    type Response = Response<BoxBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, mut request: Request<RequestBody>) -> Self::Future {
        let method = request.uri().path().to_owned();
        let trace = request
            .headers()
            .get("traceparent")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| TraceContext::parse(value).ok())
            .map(|parent| parent.child())
            .unwrap_or_else(|| TraceContext::root(TraceFlags::SAMPLED));
        let mut slow_call = self.slow_log.as_ref().map(|config| {
            RpcSlowCall::new(
                config.clone(),
                method.clone(),
                request
                    .headers()
                    .get("grpc-timeout")
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_grpc_timeout),
                trace.clone(),
            )
        });
        request.extensions_mut().insert(trace);

        if let Some(auth) = &self.auth {
            let identity = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(bearer_token)
                .and_then(|token| auth(token));
            let Some(identity) = identity else {
                if let Some(call) = slow_call.take() {
                    call.finish(Code::Unauthenticated);
                }
                return Box::pin(async {
                    Ok(Status::unauthenticated(format!(
                        "{}: {}",
                        AuthFailure::InvalidCredentials.code(),
                        AuthFailure::InvalidCredentials.message()
                    ))
                    .into_http())
                });
            };
            request.extensions_mut().insert(identity);
        }

        let permit = match &self.shedder {
            Some(shedder) => match shedder.try_acquire() {
                Some(permit) => Some(permit),
                None => {
                    if let Some(call) = slow_call.take() {
                        call.finish(Code::ResourceExhausted);
                    }
                    return Box::pin(async {
                        Ok(Status::resource_exhausted("gRPC server is overloaded").into_http())
                    });
                }
            },
            None => None,
        };

        let breaker_permit = match &self.breakers {
            Some(breakers) => match breakers.acquire(&method) {
                Some(permit) => Some(permit),
                None => {
                    if let Some(call) = slow_call.take() {
                        call.finish(Code::Unavailable);
                    }
                    return Box::pin(async {
                        Ok(Status::unavailable("gRPC method circuit breaker is open").into_http())
                    });
                }
            },
            None => None,
        };

        let future = self.inner.call(request);
        Box::pin(async move {
            let result = AssertUnwindSafe(future).catch_unwind().await;
            match result {
                Ok(Ok(response)) => {
                    let header_code = grpc_status(response.headers());
                    let (parts, body) = response.into_parts();
                    let mut body = RpcServerLogBody {
                        inner: body,
                        slow_call,
                        _shed_permit: permit,
                        breaker_permit,
                    };
                    if let Some(code) = header_code {
                        body.finish(code);
                    } else if !parts.status.is_success() {
                        body.finish_transport_error();
                    }
                    Ok(Response::from_parts(parts, tonic::body::boxed(body)))
                }
                Ok(Err(error)) => {
                    if let Some(permit) = breaker_permit {
                        permit.finish(false);
                    }
                    if let Some(call) = slow_call.take() {
                        call.finish_transport_error();
                    }
                    Err(error)
                }
                Err(_) => {
                    if let Some(permit) = breaker_permit {
                        permit.finish(false);
                    }
                    if let Some(call) = slow_call.take() {
                        call.finish(Code::Internal);
                    }
                    Ok(Status::internal("gRPC handler panicked").into_http())
                }
            }
        })
    }
}

struct RpcSlowCall {
    config: RpcSlowLogConfig,
    method: String,
    deadline: Option<Duration>,
    trace: TraceContext,
    started: Instant,
}

impl RpcSlowCall {
    fn new(
        config: RpcSlowLogConfig,
        method: String,
        deadline: Option<Duration>,
        trace: TraceContext,
    ) -> Self {
        Self {
            config,
            method,
            deadline,
            trace,
            started: Instant::now(),
        }
    }

    fn finish(self, code: Code) {
        self.emit(code_name(code), code == Code::DeadlineExceeded);
    }

    fn finish_transport_error(self) {
        self.emit("transport_error", false);
    }

    fn emit(self, status: &'static str, status_deadline_exceeded: bool) {
        let elapsed = self.started.elapsed();
        let slow = elapsed >= self.config.threshold;
        let deadline_exceeded =
            status_deadline_exceeded || self.deadline.is_some_and(|deadline| elapsed >= deadline);
        let mut fields = vec![
            LogField::new("transport", "grpc"),
            LogField::new("rpc_role", "server"),
            LogField::new("method", self.method),
            LogField::new("status", status),
            LogField::new("elapsed_ms", duration_millis(elapsed)),
            LogField::new("slow_threshold_ms", duration_millis(self.config.threshold)),
            LogField::new("slow", slow),
            LogField::new("deadline_exceeded", deadline_exceeded),
        ];
        if let Some(deadline) = self.deadline {
            fields.push(LogField::new("deadline_ms", duration_millis(deadline)));
        }
        let context = LogContext::new().with_trace(self.trace);
        let _ = self.config.logger.log_with_context(
            if slow { LogLevel::Slow } else { LogLevel::Info },
            "grpc request completed",
            Some(&context),
            fields,
        );
    }
}

pin_project! {
    struct RpcServerLogBody<B> {
        #[pin]
        inner: B,
        slow_call: Option<RpcSlowCall>,
        _shed_permit: Option<ShedPermit>,
        breaker_permit: Option<CircuitBreakerPermit>,
    }

    impl<B> PinnedDrop for RpcServerLogBody<B> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            if let Some(call) = this.slow_call.take() {
                call.finish(Code::Cancelled);
            }
            if let Some(permit) = this.breaker_permit.take() {
                permit.finish_with_outcome(CircuitOutcome::Cancellation);
            }
        }
    }
}

impl<B> RpcServerLogBody<B> {
    fn finish(&mut self, code: Code) {
        if let Some(permit) = self.breaker_permit.take() {
            permit.finish_with_outcome(status_outcome(code));
        }
        if let Some(call) = self.slow_call.take() {
            call.finish(code);
        }
    }

    fn finish_transport_error(&mut self) {
        if let Some(permit) = self.breaker_permit.take() {
            permit.finish(false);
        }
        if let Some(call) = self.slow_call.take() {
            call.finish_transport_error();
        }
    }
}

impl<B> Body for RpcServerLogBody<B>
where
    B: Body,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        match this.inner.as_mut().poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(code) = frame.trailers_ref().and_then(grpc_status) {
                    if let Some(permit) = this.breaker_permit.take() {
                        permit.finish_with_outcome(status_outcome(code));
                    }
                    if let Some(call) = this.slow_call.take() {
                        call.finish(code);
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                if let Some(permit) = this.breaker_permit.take() {
                    permit.finish(false);
                }
                if let Some(call) = this.slow_call.take() {
                    call.finish_transport_error();
                }
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                if let Some(permit) = this.breaker_permit.take() {
                    permit.finish(true);
                }
                if let Some(call) = this.slow_call.take() {
                    call.finish(Code::Ok);
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(char::is_whitespace)?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer")
        && !token.is_empty()
        && !token.contains(char::is_whitespace))
    .then_some(token)
}

/// Builds one transport service that can be passed directly to any generated Tonic client.
///
/// The stack installs bearer credentials, W3C trace propagation, bounded metrics, a default
/// deadline, and protocol-aware circuit breaking. Final streaming statuses are observed from
/// trailers before circuit health is recorded.
pub struct RpcClientStackBuilder {
    metrics: RpcMetrics,
    token: Option<BearerToken>,
    default_timeout: Option<Duration>,
    breaker: Option<Arc<CircuitBreaker>>,
}

impl RpcClientStackBuilder {
    pub fn new(metrics: RpcMetrics) -> Self {
        Self {
            metrics,
            token: None,
            default_timeout: None,
            breaker: None,
        }
    }

    pub fn with_bearer_token(mut self, token: BearerToken) -> Self {
        self.token = Some(token);
        self
    }

    pub fn with_default_timeout(mut self, timeout: Duration) -> Self {
        assert!(!timeout.is_zero(), "RPC timeout must be greater than zero");
        self.default_timeout = Some(timeout);
        self
    }

    pub fn with_circuit_breaker(mut self, config: CircuitBreakerConfig) -> Self {
        self.breaker = Some(Arc::new(CircuitBreaker::new(config)));
        self
    }

    pub fn build(self) -> RpcClientStack {
        RpcClientStack {
            metrics: RpcMetricsLayer::new(self.metrics),
            token: self.token,
            default_timeout: self.default_timeout,
            breaker: self.breaker,
        }
    }
}

#[derive(Clone)]
pub struct RpcClientStack {
    metrics: RpcMetricsLayer,
    token: Option<BearerToken>,
    default_timeout: Option<Duration>,
    breaker: Option<Arc<CircuitBreaker>>,
}

impl<S> Layer<S> for RpcClientStack {
    type Service = RpcClientStackService<crate::metrics::RpcMetricsService<S>>;

    fn layer(&self, inner: S) -> Self::Service {
        RpcClientStackService {
            inner: self.metrics.layer(inner),
            token: self.token.clone(),
            default_timeout: self.default_timeout,
            breaker: self.breaker.clone(),
        }
    }
}

#[derive(Clone)]
pub struct RpcClientStackService<S> {
    inner: S,
    token: Option<BearerToken>,
    default_timeout: Option<Duration>,
    breaker: Option<Arc<CircuitBreaker>>,
}

impl<S, RequestBody, ResponseBody> Service<Request<RequestBody>> for RpcClientStackService<S>
where
    S: Service<Request<RequestBody>, Response = Response<ResponseBody>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    RequestBody: Send + 'static,
    ResponseBody: Body<Data = bytes::Bytes> + Send + 'static,
    ResponseBody::Error: Into<tonic::codegen::StdError>,
{
    type Response = Response<BoxBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, mut request: Request<RequestBody>) -> Self::Future {
        if let Some(token) = &self.token {
            request.headers_mut().insert(
                "authorization",
                http::HeaderValue::from_bytes(token.authorization().as_encoded_bytes())
                    .expect("ASCII gRPC metadata is a valid HTTP header"),
            );
        }

        let parent = request
            .extensions()
            .get::<TraceContext>()
            .cloned()
            .or_else(|| {
                request
                    .headers()
                    .get("traceparent")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| TraceContext::parse(value).ok())
            });
        let trace = parent
            .as_ref()
            .map(TraceContext::child)
            .unwrap_or_else(|| TraceContext::root(TraceFlags::SAMPLED));
        request.headers_mut().insert(
            "traceparent",
            trace
                .traceparent()
                .parse()
                .expect("generated traceparent is a valid HTTP header"),
        );
        request.extensions_mut().insert(trace);

        if !request.headers().contains_key("grpc-timeout") {
            if let Some(timeout) = self.default_timeout {
                request.headers_mut().insert(
                    "grpc-timeout",
                    grpc_timeout(timeout)
                        .parse()
                        .expect("formatted gRPC timeout is a valid HTTP header"),
                );
            }
        }

        let permit = match &self.breaker {
            Some(breaker) => match breaker.acquire() {
                Some(permit) => Some(permit),
                None => {
                    return Box::pin(async {
                        Ok(Status::unavailable("gRPC dependency circuit is open").into_http())
                    });
                }
            },
            None => None,
        };

        let future = self.inner.call(request);
        Box::pin(async move {
            match future.await {
                Ok(response) => {
                    let header_code = grpc_status(response.headers());
                    let (parts, body) = response.into_parts();
                    let mut wrapped = RpcCircuitBody {
                        inner: body,
                        permit,
                    };
                    if let Some(code) = header_code {
                        wrapped.finish(status_outcome(code));
                    } else if !parts.status.is_success() {
                        wrapped.finish(CircuitOutcome::Failure);
                    }
                    Ok(Response::from_parts(parts, tonic::body::boxed(wrapped)))
                }
                Err(error) => {
                    if let Some(permit) = permit {
                        permit.finish(false);
                    }
                    Err(error)
                }
            }
        })
    }
}

pin_project! {
    struct RpcCircuitBody<B> {
        #[pin]
        inner: B,
        permit: Option<CircuitBreakerPermit>,
    }
}

impl<B> RpcCircuitBody<B> {
    fn finish(&mut self, outcome: CircuitOutcome) {
        if let Some(permit) = self.permit.take() {
            permit.finish_with_outcome(outcome);
        }
    }
}

impl<B> Body for RpcCircuitBody<B>
where
    B: Body,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        match this.inner.as_mut().poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(code) = frame.trailers_ref().and_then(grpc_status) {
                    if let Some(permit) = this.permit.take() {
                        permit.finish_with_outcome(status_outcome(code));
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                if let Some(permit) = this.permit.take() {
                    permit.finish(false);
                }
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                if let Some(permit) = this.permit.take() {
                    permit.finish(true);
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

fn grpc_status(headers: &http::HeaderMap) -> Option<Code> {
    headers
        .get("grpc-status")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i32>().ok())
        .map(Code::from_i32)
}

fn parse_grpc_timeout(value: &str) -> Option<Duration> {
    let (amount, unit) = value.split_at(value.len().checked_sub(1)?);
    if amount.is_empty() || amount.len() > 8 {
        return None;
    }
    let amount = amount.parse::<u64>().ok()?;
    match unit {
        "n" => Some(Duration::from_nanos(amount)),
        "u" => Some(Duration::from_micros(amount)),
        "m" => Some(Duration::from_millis(amount)),
        "S" => Some(Duration::from_secs(amount)),
        "M" => Some(Duration::from_secs(amount.saturating_mul(60))),
        "H" => Some(Duration::from_secs(amount.saturating_mul(3_600))),
        _ => None,
    }
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn code_name(code: Code) -> &'static str {
    match code {
        Code::Ok => "ok",
        Code::Cancelled => "cancelled",
        Code::Unknown => "unknown",
        Code::InvalidArgument => "invalid_argument",
        Code::DeadlineExceeded => "deadline_exceeded",
        Code::NotFound => "not_found",
        Code::AlreadyExists => "already_exists",
        Code::PermissionDenied => "permission_denied",
        Code::ResourceExhausted => "resource_exhausted",
        Code::FailedPrecondition => "failed_precondition",
        Code::Aborted => "aborted",
        Code::OutOfRange => "out_of_range",
        Code::Unimplemented => "unimplemented",
        Code::Internal => "internal",
        Code::Unavailable => "unavailable",
        Code::DataLoss => "data_loss",
        Code::Unauthenticated => "unauthenticated",
    }
}

fn status_outcome(code: Code) -> CircuitOutcome {
    if code == Code::Cancelled {
        return CircuitOutcome::Cancellation;
    }
    if matches!(
        code,
        Code::DeadlineExceeded
            | Code::Internal
            | Code::Unavailable
            | Code::DataLoss
            | Code::Unimplemented
            | Code::ResourceExhausted
    ) {
        CircuitOutcome::Failure
    } else {
        CircuitOutcome::Success
    }
}

fn grpc_timeout(timeout: Duration) -> String {
    let nanos = timeout.as_nanos().max(1);
    for (unit, divisor) in [
        ('n', 1_u128),
        ('u', 1_000),
        ('m', 1_000_000),
        ('S', 1_000_000_000),
        ('M', 60_000_000_000),
        ('H', 3_600_000_000_000),
    ] {
        let value = nanos.saturating_add(divisor - 1) / divisor;
        if value <= 99_999_999 {
            return format!("{value}{unit}");
        }
    }
    "99999999H".to_owned()
}

#[cfg(test)]
mod client_tests {
    use super::*;
    use crate::RpcMetricMode;
    use std::{
        convert::Infallible,
        io::{self, Write},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
    };
    use tower::ServiceExt;

    #[derive(Clone)]
    struct FailingTransport {
        calls: Arc<AtomicUsize>,
    }

    impl Service<Request<()>> for FailingTransport {
        type Response = Response<TrailerBody>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: Request<()>) -> Self::Future {
            self.calls.fetch_add(1, Ordering::Relaxed);
            assert_eq!(
                request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok()),
                Some("Bearer secret")
            );
            assert_eq!(
                request
                    .headers()
                    .get("grpc-timeout")
                    .and_then(|value| value.to_str().ok()),
                Some("250000u")
            );
            assert!(request.headers().contains_key("traceparent"));
            std::future::ready(Ok(Response::new(TrailerBody(true))))
        }
    }

    struct TrailerBody(bool);

    impl Body for TrailerBody {
        type Data = bytes::Bytes;
        type Error = Status;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            if !self.0 {
                return Poll::Ready(None);
            }
            self.0 = false;
            let mut trailers = http::HeaderMap::new();
            trailers.insert("grpc-status", "14".parse().unwrap());
            Poll::Ready(Some(Ok(Frame::trailers(trailers))))
        }
    }

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

    #[derive(Clone)]
    struct SlowServerTransport;

    impl Service<Request<()>> for SlowServerTransport {
        type Response = Response<TrailerBody>;
        type Error = Infallible;
        type Future =
            Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: Request<()>) -> Self::Future {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Ok(Response::new(TrailerBody(true)))
            })
        }
    }

    #[derive(Clone)]
    struct FailingServerTransport {
        calls: Arc<AtomicUsize>,
    }

    impl Service<Request<()>> for FailingServerTransport {
        type Response = Response<TrailerBody>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: Request<()>) -> Self::Future {
            self.calls.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Ok(Response::new(TrailerBody(true))))
        }
    }

    #[tokio::test]
    async fn client_stack_composes_headers_metrics_deadline_and_trailer_breaking() {
        let registry = rust_zero_core::Metrics::new();
        let metrics = RpcMetrics::new(
            &registry,
            "stacked",
            RpcMetricMode::Client,
            ["/echo.Echo/Call"],
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut service = RpcClientStackBuilder::new(metrics)
            .with_bearer_token(BearerToken::new("secret").unwrap())
            .with_default_timeout(Duration::from_millis(250))
            .with_circuit_breaker(CircuitBreakerConfig::new(1, Duration::from_secs(60)))
            .build()
            .layer(FailingTransport {
                calls: Arc::clone(&calls),
            });

        let response = service
            .ready()
            .await
            .unwrap()
            .call(Request::builder().uri("/echo.Echo/Call").body(()).unwrap())
            .await
            .unwrap();
        let mut body = Box::pin(response.into_body());
        std::future::poll_fn(|context| body.as_mut().poll_frame(context)).await;

        let rejected = service
            .ready()
            .await
            .unwrap()
            .call(Request::builder().uri("/echo.Echo/Call").body(()).unwrap())
            .await
            .unwrap();
        assert_eq!(rejected.headers().get("grpc-status").unwrap(), "14");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(registry.render().contains(
            "stacked_rpc_client_requests_total{method=\"/echo.Echo/Call\",code=\"14\"} 1"
        ));
    }

    #[tokio::test]
    async fn server_stack_logs_final_status_deadline_and_slow_classification() {
        let registry = rust_zero_core::Metrics::new();
        let metrics = RpcMetrics::new(
            &registry,
            "server_log",
            RpcMetricMode::Server,
            ["/echo.Echo/Call"],
        )
        .unwrap();
        let output = SharedWriter::default();
        let logger =
            Logger::to_writer(rust_zero_core::LogConfig::console("rpc"), output.clone()).unwrap();
        let mut service = RpcServerStackBuilder::new(metrics)
            .with_slow_call_logging(logger, Duration::from_millis(1))
            .build()
            .layer(SlowServerTransport);

        let response = service
            .ready()
            .await
            .unwrap()
            .call(
                Request::builder()
                    .uri("/echo.Echo/Call")
                    .header("grpc-timeout", "1m")
                    .body(())
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut body = Box::pin(response.into_body());
        std::future::poll_fn(|context| body.as_mut().poll_frame(context)).await;

        let bytes = output.0.lock().unwrap().clone();
        let record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record["level"], "slow");
        assert_eq!(record["transport"], "grpc");
        assert_eq!(record["rpc_role"], "server");
        assert_eq!(record["method"], "/echo.Echo/Call");
        assert_eq!(record["status"], "unavailable");
        assert_eq!(record["slow"], true);
        assert_eq!(record["slow_threshold_ms"], 1);
        assert_eq!(record["deadline_ms"], 1);
        assert_eq!(record["deadline_exceeded"], true);
        assert!(record["trace_id"].as_str().is_some());
    }

    #[tokio::test]
    async fn server_shedder_holds_permit_until_stream_completion() {
        let registry = rust_zero_core::Metrics::new();
        let metrics = RpcMetrics::new(
            &registry,
            "server_shed",
            RpcMetricMode::Server,
            ["/echo.Echo/Call"],
        )
        .unwrap();
        let mut service = RpcServerStackBuilder::new(metrics)
            .with_load_shedder(LoadShedderConfig::production(1))
            .build()
            .layer(SlowServerTransport);

        let first = service
            .ready()
            .await
            .unwrap()
            .call(Request::builder().uri("/echo.Echo/Call").body(()).unwrap())
            .await
            .unwrap();
        let rejected = service
            .ready()
            .await
            .unwrap()
            .call(Request::builder().uri("/echo.Echo/Call").body(()).unwrap())
            .await
            .unwrap();
        assert_eq!(rejected.headers().get("grpc-status").unwrap(), "8");

        let mut body = Box::pin(first.into_body());
        std::future::poll_fn(|context| body.as_mut().poll_frame(context)).await;
        drop(body);

        let admitted = service
            .ready()
            .await
            .unwrap()
            .call(Request::builder().uri("/echo.Echo/Call").body(()).unwrap())
            .await
            .unwrap();
        assert_ne!(
            admitted
                .headers()
                .get("grpc-status")
                .map(|value| value.as_bytes()),
            Some(b"8".as_slice())
        );
    }

    #[tokio::test]
    async fn server_breaker_is_result_aware_and_isolated_per_method() {
        let registry = rust_zero_core::Metrics::new();
        let metrics = RpcMetrics::new(
            &registry,
            "server_breaker",
            RpcMetricMode::Server,
            ["/echo.Echo/Call", "/echo.Echo/Other"],
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut service = RpcServerStackBuilder::new(metrics)
            .with_circuit_breaker(CircuitBreakerConfig::new(1, Duration::from_secs(60)))
            .build()
            .layer(FailingServerTransport {
                calls: Arc::clone(&calls),
            });

        let first = service
            .ready()
            .await
            .unwrap()
            .call(Request::builder().uri("/echo.Echo/Call").body(()).unwrap())
            .await
            .unwrap();
        let mut body = Box::pin(first.into_body());
        std::future::poll_fn(|context| body.as_mut().poll_frame(context)).await;

        let rejected = service
            .ready()
            .await
            .unwrap()
            .call(Request::builder().uri("/echo.Echo/Call").body(()).unwrap())
            .await
            .unwrap();
        assert_eq!(rejected.headers().get("grpc-status").unwrap(), "14");
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let other = service
            .ready()
            .await
            .unwrap()
            .call(Request::builder().uri("/echo.Echo/Other").body(()).unwrap())
            .await
            .unwrap();
        let mut body = Box::pin(other.into_body());
        std::future::poll_fn(|context| body.as_mut().poll_frame(context)).await;
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn grpc_deadlines_use_the_smallest_exact_unit() {
        assert_eq!(grpc_timeout(Duration::from_nanos(7)), "7n");
        assert_eq!(grpc_timeout(Duration::from_millis(250)), "250000u");
        assert_eq!(grpc_timeout(Duration::from_secs(100)), "100000m");
    }
}
