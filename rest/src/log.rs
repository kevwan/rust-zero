use actix_web::{
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    Error,
};
use futures::future::{ok, LocalBoxFuture, Ready};
use std::{
    task::{Context, Poll},
    time::Instant,
};

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
