//! Request-path-aware server deadlines.

use http::{Request, Response};
use http_body::Body;
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tonic::{body::BoxBody, Status};
use tower::{Layer, Service};

/// Applies an exact-method timeout, then a service timeout, then the global fallback.
#[derive(Debug, Clone, Default)]
pub struct RpcServerTimeoutLayer {
    global: Option<Duration>,
    methods: BTreeMap<String, Duration>,
    services: BTreeMap<String, Duration>,
}

impl RpcServerTimeoutLayer {
    pub(crate) fn new(
        global: Option<Duration>,
        methods: BTreeMap<String, Duration>,
        services: BTreeMap<String, Duration>,
    ) -> Self {
        Self {
            global,
            methods,
            services,
        }
    }

    pub(crate) fn timeout(&self, path: &str) -> Option<Duration> {
        self.methods.get(path).copied().or_else(|| {
            path.rsplit_once('/')
                .and_then(|(service, _)| self.services.get(service).copied())
                .or(self.global)
        })
    }
}

impl<S> Layer<S> for RpcServerTimeoutLayer {
    type Service = RpcServerTimeoutService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RpcServerTimeoutService {
            inner,
            policy: self.clone(),
        }
    }
}

/// Service produced by [`RpcServerTimeoutLayer`].
#[derive(Debug, Clone)]
pub struct RpcServerTimeoutService<S> {
    inner: S,
    policy: RpcServerTimeoutLayer,
}

impl<S, B, ResponseBody> Service<Request<B>> for RpcServerTimeoutService<S>
where
    S: Service<Request<B>, Response = Response<ResponseBody>>,
    S::Future: Send + 'static,
    ResponseBody: Body<Data = bytes::Bytes> + Send + 'static,
    ResponseBody::Error: Into<tonic::codegen::StdError>,
{
    type Response = Response<BoxBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, request: Request<B>) -> Self::Future {
        let timeout = self.policy.timeout(request.uri().path());
        let future = self.inner.call(request);
        Box::pin(async move {
            match timeout {
                Some(timeout) => tokio::time::timeout(timeout, future).await.map_or_else(
                    |_| Ok(Status::deadline_exceeded("server request timeout").into_http()),
                    |result| result.map(|response| response.map(tonic::body::boxed)),
                ),
                None => future
                    .await
                    .map(|response| response.map(tonic::body::boxed)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    #[test]
    fn exact_method_precedes_service_and_global_fallbacks() {
        let layer = RpcServerTimeoutLayer::new(
            Some(Duration::from_secs(30)),
            BTreeMap::from([(
                "/rust_zero.echo.Echo/Echo".to_owned(),
                Duration::from_secs(1),
            )]),
            BTreeMap::from([("/rust_zero.echo.Echo".to_owned(), Duration::from_secs(5))]),
        );

        assert_eq!(
            layer.timeout("/rust_zero.echo.Echo/Echo"),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            layer.timeout("/rust_zero.echo.Echo/ServerStream"),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            layer.timeout("/grpc.health.v1.Health/Check"),
            Some(Duration::from_secs(30))
        );
    }

    #[derive(Clone)]
    struct DelayedService;

    impl Service<Request<()>> for DelayedService {
        type Response = Response<tonic::body::BoxBody>;
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

        fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: Request<()>) -> Self::Future {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok(Response::new(tonic::body::empty_body()))
            })
        }
    }

    #[tokio::test]
    async fn expires_the_selected_method_future() {
        let layer = RpcServerTimeoutLayer::new(
            Some(Duration::from_secs(1)),
            BTreeMap::from([(
                "/rust_zero.echo.Echo/Echo".to_owned(),
                Duration::from_millis(5),
            )]),
            BTreeMap::new(),
        );
        let mut service = layer.layer(DelayedService);
        let request = Request::builder()
            .uri("/rust_zero.echo.Echo/Echo")
            .body(())
            .unwrap();

        let response = service.call(request).await.unwrap();
        assert_eq!(response.headers()["grpc-status"], "4");
    }
}
