use actix_web::{
    body::{EitherBody, MessageBody},
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    http::{header, StatusCode},
    web::BytesMut,
    Error, HttpMessage, HttpResponse,
};
use futures::future::{ok, LocalBoxFuture, Ready};
use futures::{Stream, StreamExt};
use std::{
    io::Read,
    pin::Pin,
    rc::Rc,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

/// Rejects requests that exceed the configured maximum execution time.
pub struct Timeout {
    duration: Duration,
}

impl Clone for Timeout {
    fn clone(&self) -> Self {
        Self {
            duration: self.duration,
        }
    }
}

impl Timeout {
    pub fn new(duration: Duration) -> Self {
        assert!(
            !duration.is_zero(),
            "timeout duration must be greater than zero"
        );
        Self { duration }
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
        })
    }
}

pub struct TimeoutMiddleware<S> {
    service: S,
    duration: Duration,
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
        let future = self.service.call(request);
        let duration = self.duration;

        Box::pin(async move {
            match actix_rt::time::timeout(duration, future).await {
                Ok(response) => response,
                Err(_) => Err(actix_web::error::ErrorGatewayTimeout("request timed out")),
            }
        })
    }
}

/// Sheds excess load instead of queueing requests when all execution slots are busy.
pub struct ConcurrencyLimit {
    semaphore: Arc<Semaphore>,
}

impl Clone for ConcurrencyLimit {
    fn clone(&self) -> Self {
        Self {
            semaphore: Arc::clone(&self.semaphore),
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
        }
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
        })
    }
}

pub struct ConcurrencyLimitMiddleware<S> {
    service: S,
    semaphore: Arc<Semaphore>,
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
        let permit = match Arc::clone(&self.semaphore).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
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
}

impl Clone for RateLimit {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            permits_per_second: self.permits_per_second,
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
        }
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
        })
    }
}

pub struct RateLimitMiddleware<S> {
    service: S,
    state: Arc<Mutex<TokenBucket>>,
    permits_per_second: f64,
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
        let max_bytes = self.max_bytes;
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
    use std::{
        future::{poll_fn, Future},
        sync::Arc,
        task::Poll,
    };
    use tokio::sync::Notify;

    #[actix_rt::test]
    async fn timeout_returns_gateway_timeout() {
        let app = test::init_service(
            App::new()
                .wrap(Timeout::new(Duration::from_millis(5)))
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
    }

    #[actix_rt::test]
    async fn concurrency_limit_sheds_busy_requests() {
        let release = Arc::new(Notify::new());
        let app = test::init_service(
            App::new()
                .app_data(Data::from(Arc::clone(&release)))
                .wrap(ConcurrencyLimit::new(1))
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

        release.notify_waiters();
        assert_eq!(first.await.status(), StatusCode::OK);
    }

    #[actix_rt::test]
    async fn rate_limit_returns_retry_after() {
        let app = test::init_service(
            App::new()
                .wrap(RateLimit::new(1, 1))
                .route("/", web::get().to(|| async { HttpResponse::Ok().finish() })),
        )
        .await;

        let first = test::call_service(&app, test::TestRequest::get().uri("/").to_request()).await;
        let second = test::call_service(&app, test::TestRequest::get().uri("/").to_request()).await;

        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(second.headers().get(header::RETRY_AFTER).unwrap(), "1");
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
