use crate::{metrics::HttpMetrics, route::RequestPolicy};
use actix_web::{
    body::{EitherBody, MessageBody},
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    http::{header, StatusCode},
    web::BytesMut,
    Error, HttpMessage, HttpResponse,
};
use futures::future::{ok, LocalBoxFuture, Ready};
use futures::{Stream, StreamExt};
use rust_zero_core::{
    AdaptiveShedder, CircuitBreaker, CircuitBreakerConfig, CircuitBreakerPermit, CircuitOutcome,
    ShedPermit,
};
use std::{
    collections::HashMap,
    io::Read,
    pin::Pin,
    rc::Rc,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

/// Applies independent, result-aware circuit breakers to stable HTTP route patterns.
///
/// Breaker permits live until the response body completes. Dropping a streaming body before its
/// final frame records cancellation rather than poisoning route health.
#[derive(Clone)]
pub struct ServerCircuitBreaker {
    shared: Arc<ServerCircuitBreakers>,
    metrics: Option<HttpMetrics>,
}

struct ServerCircuitBreakers {
    config: CircuitBreakerConfig,
    by_route: Mutex<HashMap<String, Arc<CircuitBreaker>>>,
}

impl ServerCircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            shared: Arc::new(ServerCircuitBreakers {
                config,
                by_route: Mutex::new(HashMap::new()),
            }),
            metrics: None,
        }
    }

    pub fn with_metrics(mut self, metrics: HttpMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }
}

impl<S, B> Transform<S, ServiceRequest> for ServerCircuitBreaker
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<ServerCircuitBody<B>>>;
    type Error = Error;
    type Transform = ServerCircuitBreakerMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(ServerCircuitBreakerMiddleware {
            service,
            shared: Arc::clone(&self.shared),
            metrics: self.metrics.clone(),
        })
    }
}

pub struct ServerCircuitBreakerMiddleware<S> {
    service: S,
    shared: Arc<ServerCircuitBreakers>,
    metrics: Option<HttpMetrics>,
}

impl<S, B> Service<ServiceRequest> for ServerCircuitBreakerMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<ServerCircuitBody<B>>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(context)
    }

    fn call(&self, request: ServiceRequest) -> Self::Future {
        let pattern = request
            .match_pattern()
            .unwrap_or_else(|| "<unmatched>".to_owned());
        let route = format!("{} {pattern}", request.method());
        let breaker = {
            let mut breakers = self
                .shared
                .by_route
                .lock()
                .expect("REST server circuit-breaker map lock poisoned");
            Arc::clone(
                breakers
                    .entry(route)
                    .or_insert_with(|| Arc::new(CircuitBreaker::new(self.shared.config))),
            )
        };
        let Some(permit) = breaker.acquire() else {
            if let Some(metrics) = &self.metrics {
                metrics.record_protection("circuit_breaker", "rejected");
            }
            return Box::pin(async move {
                Ok(request.into_response(
                    HttpResponse::ServiceUnavailable()
                        .body("HTTP route circuit breaker is open")
                        .map_into_right_body(),
                ))
            });
        };

        let future = self.service.call(request);
        Box::pin(async move {
            match future.await {
                Ok(response) => {
                    let outcome = http_status_outcome(response.status());
                    Ok(response
                        .map_body(move |_, body| ServerCircuitBody::new(body, permit, outcome))
                        .map_into_left_body())
                }
                Err(error) => {
                    permit.finish(false);
                    Err(error)
                }
            }
        })
    }
}

fn http_status_outcome(status: StatusCode) -> CircuitOutcome {
    if status.is_server_error() {
        CircuitOutcome::Failure
    } else {
        CircuitOutcome::Success
    }
}

pub struct ServerCircuitBody<B> {
    inner: Pin<Box<B>>,
    permit: Option<CircuitBreakerPermit>,
    outcome: CircuitOutcome,
}

impl<B> ServerCircuitBody<B> {
    fn new(body: B, permit: CircuitBreakerPermit, outcome: CircuitOutcome) -> Self {
        Self {
            inner: Box::pin(body),
            permit: Some(permit),
            outcome,
        }
    }

    fn finish(&mut self, outcome: CircuitOutcome) {
        if let Some(permit) = self.permit.take() {
            permit.finish_with_outcome(outcome);
        }
    }
}

impl<B: MessageBody> MessageBody for ServerCircuitBody<B> {
    type Error = B::Error;

    fn size(&self) -> actix_web::body::BodySize {
        self.inner.as_ref().get_ref().size()
    }

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<actix_web::web::Bytes, Self::Error>>> {
        match self.inner.as_mut().poll_next(context) {
            Poll::Ready(None) => {
                let outcome = self.outcome;
                self.finish(outcome);
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(error))) => {
                self.finish(CircuitOutcome::Failure);
                Poll::Ready(Some(Err(error)))
            }
            other => other,
        }
    }
}

/// Rejects requests that exceed the configured maximum execution time.
pub struct Timeout {
    duration: Duration,
    metrics: Option<HttpMetrics>,
}

impl Clone for Timeout {
    fn clone(&self) -> Self {
        Self {
            duration: self.duration,
            metrics: self.metrics.clone(),
        }
    }
}

impl Timeout {
    pub fn new(duration: Duration) -> Self {
        assert!(
            !duration.is_zero(),
            "timeout duration must be greater than zero"
        );
        Self {
            duration,
            metrics: None,
        }
    }

    pub fn with_metrics(mut self, metrics: HttpMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }
}

impl<S, B> Transform<S, ServiceRequest> for Timeout
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = TimeoutMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(TimeoutMiddleware {
            service,
            duration: self.duration,
            metrics: self.metrics.clone(),
        })
    }
}

pub struct TimeoutMiddleware<S> {
    service: S,
    duration: Duration,
    metrics: Option<HttpMetrics>,
}

impl<S, B> Service<ServiceRequest> for TimeoutMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(context)
    }

    fn call(&self, request: ServiceRequest) -> Self::Future {
        let (duration, sse) = request
            .extensions()
            .get::<RequestPolicy>()
            .map(|policy| (policy.timeout.unwrap_or(self.duration), policy.sse))
            .unwrap_or((self.duration, false));
        let future = self.service.call(request);
        let metrics = self.metrics.clone();

        Box::pin(async move {
            if sse {
                return future.await;
            }
            match actix_rt::time::timeout(duration, future).await {
                Ok(response) => response,
                Err(_) => {
                    if let Some(metrics) = metrics {
                        metrics.record_protection("timeout", "rejected");
                    }
                    Err(actix_web::error::ErrorGatewayTimeout("request timed out"))
                }
            }
        })
    }
}

/// Sheds excess load instead of queueing requests when all execution slots are busy.
pub struct ConcurrencyLimit {
    semaphore: Arc<Semaphore>,
    priority_reserve: Arc<Semaphore>,
    metrics: Option<HttpMetrics>,
}

impl Clone for ConcurrencyLimit {
    fn clone(&self) -> Self {
        Self {
            semaphore: Arc::clone(&self.semaphore),
            priority_reserve: Arc::clone(&self.priority_reserve),
            metrics: self.metrics.clone(),
        }
    }
}

impl ConcurrencyLimit {
    pub fn new(max_concurrent_requests: usize) -> Self {
        assert!(
            max_concurrent_requests > 0,
            "maximum concurrent requests must be greater than zero"
        );
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent_requests)),
            priority_reserve: Arc::new(Semaphore::new(max_concurrent_requests.div_ceil(4))),
            metrics: None,
        }
    }

    /// Sets the additional capacity reserved exclusively for priority routes.
    pub fn with_priority_reserve(mut self, capacity: usize) -> Self {
        assert!(capacity > 0, "priority reserve must be greater than zero");
        self.priority_reserve = Arc::new(Semaphore::new(capacity));
        self
    }

    pub fn with_metrics(mut self, metrics: HttpMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }
}

impl<S, B> Transform<S, ServiceRequest> for ConcurrencyLimit
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Transform = ConcurrencyLimitMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(ConcurrencyLimitMiddleware {
            service,
            semaphore: Arc::clone(&self.semaphore),
            priority_reserve: Arc::clone(&self.priority_reserve),
            metrics: self.metrics.clone(),
        })
    }
}

pub struct ConcurrencyLimitMiddleware<S> {
    service: S,
    semaphore: Arc<Semaphore>,
    priority_reserve: Arc<Semaphore>,
    metrics: Option<HttpMetrics>,
}

/// Applies rust-zero's CPU- and throughput-aware admission control to HTTP requests.
#[derive(Clone)]
pub struct AdaptiveLoadShed {
    shedder: AdaptiveShedder,
    metrics: Option<HttpMetrics>,
}

impl AdaptiveLoadShed {
    pub fn new(shedder: AdaptiveShedder) -> Self {
        Self {
            shedder,
            metrics: None,
        }
    }

    pub fn with_metrics(mut self, metrics: HttpMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }
}

impl<S, B> Transform<S, ServiceRequest> for AdaptiveLoadShed
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<PermitBody<B>>>;
    type Error = Error;
    type Transform = AdaptiveLoadShedMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(AdaptiveLoadShedMiddleware {
            service,
            shedder: self.shedder.clone(),
            metrics: self.metrics.clone(),
        })
    }
}

pub struct AdaptiveLoadShedMiddleware<S> {
    service: S,
    shedder: AdaptiveShedder,
    metrics: Option<HttpMetrics>,
}

impl<S, B> Service<ServiceRequest> for AdaptiveLoadShedMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<PermitBody<B>>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(context)
    }

    fn call(&self, request: ServiceRequest) -> Self::Future {
        let Some(permit) = self.shedder.try_acquire() else {
            if let Some(metrics) = &self.metrics {
                metrics.record_protection("load_shedder", "rejected");
            }
            return Box::pin(async move {
                Ok(request.into_response(
                    HttpResponse::build(StatusCode::SERVICE_UNAVAILABLE)
                        .body("server is overloaded")
                        .map_into_right_body(),
                ))
            });
        };
        let future = self.service.call(request);
        Box::pin(async move {
            let response = future
                .await?
                .map_body(move |_, body| PermitBody::new(body, permit))
                .map_into_left_body();
            Ok(response)
        })
    }
}

pub struct PermitBody<B> {
    inner: Pin<Box<B>>,
    _permit: ShedPermit,
}

impl<B> PermitBody<B> {
    fn new(body: B, permit: ShedPermit) -> Self {
        Self {
            inner: Box::pin(body),
            _permit: permit,
        }
    }
}

impl<B: MessageBody> MessageBody for PermitBody<B> {
    type Error = B::Error;

    fn size(&self) -> actix_web::body::BodySize {
        self.inner.as_ref().get_ref().size()
    }

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<actix_web::web::Bytes, Self::Error>>> {
        self.inner.as_mut().poll_next(context)
    }
}

impl<S, B> Service<ServiceRequest> for ConcurrencyLimitMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(context)
    }

    fn call(&self, request: ServiceRequest) -> Self::Future {
        let priority = request
            .extensions()
            .get::<RequestPolicy>()
            .is_some_and(|policy| policy.priority);
        let permit = match Arc::clone(&self.semaphore)
            .try_acquire_owned()
            .or_else(|error| {
                if priority {
                    Arc::clone(&self.priority_reserve).try_acquire_owned()
                } else {
                    Err(error)
                }
            }) {
            Ok(permit) => permit,
            Err(_) => {
                if let Some(metrics) = &self.metrics {
                    metrics.record_protection("concurrency", "rejected");
                }
                return Box::pin(async move {
                    Ok(request.into_response(
                        HttpResponse::build(StatusCode::SERVICE_UNAVAILABLE)
                            .body("server is overloaded")
                            .map_into_right_body(),
                    ))
                });
            }
        };

        let future = self.service.call(request);
        Box::pin(async move {
            let response = future.await?.map_into_left_body();
            drop(permit);
            Ok(response)
        })
    }
}

/// A token-bucket limiter that can be shared by cloning it into Actix workers.
pub struct RateLimit {
    state: Arc<Mutex<TokenBucket>>,
    permits_per_second: f64,
    metrics: Option<HttpMetrics>,
}

impl Clone for RateLimit {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            permits_per_second: self.permits_per_second,
            metrics: self.metrics.clone(),
        }
    }
}

impl RateLimit {
    pub fn new(permits_per_second: u32, burst: u32) -> Self {
        assert!(
            permits_per_second > 0,
            "permits per second must be greater than zero"
        );
        assert!(burst > 0, "burst capacity must be greater than zero");

        Self {
            state: Arc::new(Mutex::new(TokenBucket {
                available: f64::from(burst),
                capacity: f64::from(burst),
                last_refill: Instant::now(),
            })),
            permits_per_second: f64::from(permits_per_second),
            metrics: None,
        }
    }

    pub fn with_metrics(mut self, metrics: HttpMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }
}

impl<S, B> Transform<S, ServiceRequest> for RateLimit
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Transform = RateLimitMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(RateLimitMiddleware {
            service,
            state: Arc::clone(&self.state),
            permits_per_second: self.permits_per_second,
            metrics: self.metrics.clone(),
        })
    }
}

pub struct RateLimitMiddleware<S> {
    service: S,
    state: Arc<Mutex<TokenBucket>>,
    permits_per_second: f64,
    metrics: Option<HttpMetrics>,
}

impl<S, B> Service<ServiceRequest> for RateLimitMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(context)
    }

    fn call(&self, request: ServiceRequest) -> Self::Future {
        let retry_after = self
            .state
            .lock()
            .expect("rate limiter state lock poisoned")
            .try_acquire(self.permits_per_second);

        if let Some(retry_after) = retry_after {
            if let Some(metrics) = &self.metrics {
                metrics.record_protection("rate_limit", "rejected");
            }
            let retry_after_seconds = retry_after.as_secs_f64().ceil().max(1.0) as u64;
            return Box::pin(async move {
                Ok(request.into_response(
                    HttpResponse::build(StatusCode::TOO_MANY_REQUESTS)
                        .insert_header((header::RETRY_AFTER, retry_after_seconds.to_string()))
                        .body("rate limit exceeded")
                        .map_into_right_body(),
                ))
            });
        }

        let future = self.service.call(request);
        Box::pin(async move { Ok(future.await?.map_into_left_body()) })
    }
}

struct TokenBucket {
    available: f64,
    capacity: f64,
    last_refill: Instant,
}

/// Buffers request bodies, optionally expands gzip input, and rejects oversized payloads.
///
/// Both compressed and expanded data are checked, preventing a small gzip payload from expanding
/// beyond the configured application limit.
#[derive(Debug, Clone)]
pub struct RequestBodyLimit {
    max_bytes: usize,
    decompress_gzip: bool,
}

impl RequestBodyLimit {
    pub fn new(max_bytes: usize) -> Self {
        assert!(
            max_bytes > 0,
            "request body limit must be greater than zero"
        );
        Self {
            max_bytes,
            decompress_gzip: true,
        }
    }

    pub fn decompress_gzip(mut self, enabled: bool) -> Self {
        self.decompress_gzip = enabled;
        self
    }
}

impl<S, B> Transform<S, ServiceRequest> for RequestBodyLimit
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Transform = RequestBodyLimitMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(RequestBodyLimitMiddleware {
            service: Rc::new(service),
            max_bytes: self.max_bytes,
            decompress_gzip: self.decompress_gzip,
        })
    }
}

pub struct RequestBodyLimitMiddleware<S> {
    service: Rc<S>,
    max_bytes: usize,
    decompress_gzip: bool,
}

impl<S, B> Service<ServiceRequest> for RequestBodyLimitMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(context)
    }

    fn call(&self, mut request: ServiceRequest) -> Self::Future {
        let max_bytes = request
            .extensions()
            .get::<RequestPolicy>()
            .and_then(|policy| policy.max_body_bytes)
            .unwrap_or(self.max_bytes);
        let decompress_gzip = self.decompress_gzip;
        let service = Rc::clone(&self.service);

        Box::pin(async move {
            if request
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<usize>().ok())
                .is_some_and(|length| length > max_bytes)
            {
                return Ok(payload_too_large(request, max_bytes));
            }

            let gzip = decompress_gzip
                && request
                    .headers()
                    .get(header::CONTENT_ENCODING)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| {
                        value
                            .split(',')
                            .any(|encoding| encoding.trim().eq_ignore_ascii_case("gzip"))
                    });

            let mut payload = request.take_payload();
            let mut body = BytesMut::new();
            while let Some(chunk) = payload.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(_) => {
                        return Ok(request.into_response(
                            HttpResponse::BadRequest()
                                .body("invalid request body")
                                .map_into_right_body(),
                        ));
                    }
                };
                if body.len().saturating_add(chunk.len()) > max_bytes {
                    return Ok(payload_too_large(request, max_bytes));
                }
                body.extend_from_slice(&chunk);
            }

            if gzip {
                let decoder = flate2::read::GzDecoder::new(body.as_ref());
                let mut expanded = Vec::new();
                if decoder
                    .take(max_bytes as u64 + 1)
                    .read_to_end(&mut expanded)
                    .is_err()
                {
                    return Ok(request.into_response(
                        HttpResponse::BadRequest()
                            .body("invalid gzip request body")
                            .map_into_right_body(),
                    ));
                }
                if expanded.len() > max_bytes {
                    return Ok(payload_too_large(request, max_bytes));
                }
                body = BytesMut::from(expanded.as_slice());
                request.headers_mut().remove(header::CONTENT_ENCODING);
                request.headers_mut().remove(header::CONTENT_LENGTH);
            }

            let body = body.freeze();
            let payload =
                futures::stream::once(async move { Ok::<_, actix_web::error::PayloadError>(body) });
            let payload: Pin<
                Box<
                    dyn Stream<
                        Item = Result<actix_web::web::Bytes, actix_web::error::PayloadError>,
                    >,
                >,
            > = Box::pin(payload);
            request.set_payload(payload.into());
            Ok(service.call(request).await?.map_into_left_body())
        })
    }
}

fn payload_too_large<B>(
    request: ServiceRequest,
    max_bytes: usize,
) -> ServiceResponse<EitherBody<B>> {
    request.into_response(
        HttpResponse::build(StatusCode::PAYLOAD_TOO_LARGE)
            .body(format!("request body exceeds {max_bytes} bytes"))
            .map_into_right_body(),
    )
}

impl TokenBucket {
    /// Returns the time until the next permit when the bucket is empty.
    fn try_acquire(&mut self, permits_per_second: f64) -> Option<Duration> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.available = (self.available + elapsed * permits_per_second).min(self.capacity);
        self.last_refill = now;

        if self.available >= 1.0 {
            self.available -= 1.0;
            None
        } else {
            Some(Duration::from_secs_f64(
                (1.0 - self.available) / permits_per_second,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{
        test,
        web::{self, Data},
        App, HttpResponse,
    };
    use rust_zero_core::Metrics;
    use std::{
        future::{poll_fn, Future},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        task::Poll,
    };
    use tokio::sync::Notify;

    #[actix_rt::test]
    async fn timeout_returns_gateway_timeout() {
        let metrics = Metrics::new();
        let http_metrics = HttpMetrics::new(&metrics, "test").unwrap();
        let app = test::init_service(
            App::new()
                .wrap(Timeout::new(Duration::from_millis(5)).with_metrics(http_metrics))
                .route(
                    "/",
                    web::get().to(|| async {
                        actix_rt::time::sleep(Duration::from_millis(50)).await;
                        HttpResponse::Ok().finish()
                    }),
                ),
        )
        .await;

        let error = test::try_call_service(&app, test::TestRequest::get().uri("/").to_request())
            .await
            .expect_err("slow request should time out");

        assert_eq!(
            actix_web::error::ResponseError::status_code(error.as_response_error()),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert!(metrics.render().contains(
            "test_http_protection_decisions_total{mechanism=\"timeout\",decision=\"rejected\"} 1"
        ));
    }

    #[actix_rt::test]
    async fn server_breaker_is_result_aware_and_isolated_per_route() {
        let calls = Arc::new(AtomicUsize::new(0));
        let metrics = Metrics::new();
        let http_metrics = HttpMetrics::new(&metrics, "server_breaker").unwrap();
        let app = test::init_service(
            App::new()
                .app_data(Data::from(Arc::clone(&calls)))
                .wrap(
                    ServerCircuitBreaker::new(CircuitBreakerConfig::new(
                        1,
                        Duration::from_secs(60),
                    ))
                    .with_metrics(http_metrics),
                )
                .route(
                    "/fail/{id}",
                    web::get().to(|calls: Data<AtomicUsize>| async move {
                        calls.fetch_add(1, Ordering::Relaxed);
                        HttpResponse::InternalServerError().body("failed")
                    }),
                )
                .route(
                    "/other/{id}",
                    web::get().to(|calls: Data<AtomicUsize>| async move {
                        calls.fetch_add(1, Ordering::Relaxed);
                        HttpResponse::InternalServerError().body("failed")
                    }),
                ),
        )
        .await;

        let first =
            test::call_service(&app, test::TestRequest::get().uri("/fail/1").to_request()).await;
        assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);
        test::read_body(first).await;

        let rejected =
            test::call_service(&app, test::TestRequest::get().uri("/fail/2").to_request()).await;
        assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let other =
            test::call_service(&app, test::TestRequest::get().uri("/other/1").to_request()).await;
        assert_eq!(other.status(), StatusCode::INTERNAL_SERVER_ERROR);
        test::read_body(other).await;
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert!(metrics.render().contains(
            "server_breaker_http_protection_decisions_total{mechanism=\"circuit_breaker\",decision=\"rejected\"} 1"
        ));
    }

    #[actix_rt::test]
    async fn server_breaker_body_records_early_stream_drop_as_cancellation() {
        let breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig::new(
            1,
            Duration::from_secs(60),
        )));
        let body = ServerCircuitBody::new((), breaker.acquire().unwrap(), CircuitOutcome::Success);
        drop(body);

        assert_eq!(breaker.snapshot().cancellations, 1);
        assert_eq!(breaker.snapshot().failures, 0);
    }

    #[actix_rt::test]
    async fn concurrency_limit_sheds_busy_requests() {
        let release = Arc::new(Notify::new());
        let metrics = Metrics::new();
        let http_metrics = HttpMetrics::new(&metrics, "test").unwrap();
        let app = test::init_service(
            App::new()
                .app_data(Data::from(Arc::clone(&release)))
                .wrap(ConcurrencyLimit::new(1).with_metrics(http_metrics))
                .route(
                    "/",
                    web::get().to(|release: Data<Notify>| async move {
                        release.notified().await;
                        HttpResponse::Ok().finish()
                    }),
                ),
        )
        .await;

        let first = test::call_service(&app, test::TestRequest::get().uri("/").to_request());
        futures::pin_mut!(first);
        poll_fn(|context| {
            assert!(first.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;

        let second = test::call_service(&app, test::TestRequest::get().uri("/").to_request()).await;
        assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(metrics.render().contains(
            "test_http_protection_decisions_total{mechanism=\"concurrency\",decision=\"rejected\"} 1"
        ));

        release.notify_waiters();
        assert_eq!(first.await.status(), StatusCode::OK);
    }

    #[actix_rt::test]
    async fn adaptive_load_shed_is_installed_as_http_middleware() {
        let release = Arc::new(Notify::new());
        let metrics = Metrics::new();
        let http_metrics = HttpMetrics::new(&metrics, "adaptive").unwrap();
        let shedder = AdaptiveShedder::new(rust_zero_core::LoadShedderConfig::new(
            1,
            Duration::from_secs(1),
        ));
        let app = test::init_service(
            App::new()
                .app_data(Data::from(Arc::clone(&release)))
                .wrap(AdaptiveLoadShed::new(shedder).with_metrics(http_metrics))
                .route(
                    "/",
                    web::get().to(|release: Data<Notify>| async move {
                        release.notified().await;
                        HttpResponse::Ok().finish()
                    }),
                ),
        )
        .await;

        let first = test::call_service(&app, test::TestRequest::get().uri("/").to_request());
        futures::pin_mut!(first);
        poll_fn(|context| {
            assert!(first.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;

        let rejected =
            test::call_service(&app, test::TestRequest::get().uri("/").to_request()).await;
        assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(metrics.render().contains(
            "adaptive_http_protection_decisions_total{mechanism=\"load_shedder\",decision=\"rejected\"} 1"
        ));

        release.notify_waiters();
        assert_eq!(first.await.status(), StatusCode::OK);
    }

    #[actix_rt::test]
    async fn adaptive_permit_lives_until_the_response_body_is_dropped() {
        let shedder = AdaptiveShedder::new(rust_zero_core::LoadShedderConfig::new(
            1,
            Duration::from_secs(1),
        ));
        let body = PermitBody::new((), shedder.try_acquire().unwrap());
        assert!(shedder.try_acquire().is_none());
        drop(body);
        assert!(shedder.try_acquire().is_some());
    }

    #[actix_rt::test]
    async fn rate_limit_returns_retry_after() {
        let metrics = Metrics::new();
        let http_metrics = HttpMetrics::new(&metrics, "test").unwrap();
        let app = test::init_service(
            App::new()
                .wrap(RateLimit::new(1, 1).with_metrics(http_metrics))
                .route("/", web::get().to(|| async { HttpResponse::Ok().finish() })),
        )
        .await;

        let first = test::call_service(&app, test::TestRequest::get().uri("/").to_request()).await;
        let second = test::call_service(&app, test::TestRequest::get().uri("/").to_request()).await;

        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(second.headers().get(header::RETRY_AFTER).unwrap(), "1");
        assert!(metrics.render().contains(
            "test_http_protection_decisions_total{mechanism=\"rate_limit\",decision=\"rejected\"} 1"
        ));
    }

    #[actix_rt::test]
    async fn request_body_limit_rejects_oversized_streams() {
        let app = test::init_service(
            App::new()
                .wrap(RequestBodyLimit::new(4))
                .route("/", web::post().to(|body: String| async move { body })),
        )
        .await;

        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/")
                .set_payload("12345")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[actix_rt::test]
    async fn request_body_limit_expands_gzip_input() {
        use flate2::{write::GzEncoder, Compression};
        use std::io::Write;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"hello").unwrap();
        let compressed = encoder.finish().unwrap();
        let app = test::init_service(
            App::new()
                .wrap(RequestBodyLimit::new(64))
                .route("/", web::post().to(|body: String| async move { body })),
        )
        .await;

        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/")
                .insert_header((header::CONTENT_ENCODING, "gzip"))
                .set_payload(compressed)
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(test::read_body(response).await, "hello");
    }
}
