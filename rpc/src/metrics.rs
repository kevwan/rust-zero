use http::{Request, Response};
use http_body::{Body, Frame};
use pin_project_lite::pin_project;
use rust_zero_core::{
    CounterVec, GaugeVec, HistogramOptions, HistogramVec, Metrics, MetricsError, VectorOptions,
};
use std::{
    collections::BTreeSet,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Instant,
};
use tower::{Layer, Service};

const UNKNOWN_METHOD: &str = "unknown";

/// The side of a gRPC transport observed by [`RpcMetrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcMetricMode {
    Client,
    Server,
}

impl RpcMetricMode {
    fn name(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Server => "server",
        }
    }
}

/// Registered gRPC request, latency, and in-flight metrics.
///
/// Callers provide the generated RPC method paths up front (for example
/// `/users.Users/Get`). Requests for any other path share the `unknown` label, preventing an
/// attacker or a misconfigured client from creating an unbounded number of metric series.
#[derive(Clone)]
pub struct RpcMetrics {
    requests: CounterVec,
    duration: HistogramVec,
    in_flight: GaugeVec,
    methods: Arc<BTreeSet<String>>,
}

impl RpcMetrics {
    pub fn new<I, S>(
        metrics: &Metrics,
        namespace: impl Into<String>,
        mode: RpcMetricMode,
        methods: I,
    ) -> Result<Self, MetricsError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let namespace = namespace.into();
        let subsystem = format!("rpc_{}", mode.name());
        let options = |name, help| {
            VectorOptions::new(name, help)
                .with_namespace(namespace.clone())
                .with_subsystem(subsystem.clone())
        };

        Ok(Self {
            requests: metrics.counter_vec(
                options("requests_total", "Completed gRPC requests")
                    .with_labels(["method", "code"]),
            )?,
            duration: metrics.histogram_vec(
                HistogramOptions::new("", "").with_vector_options(
                    options("request_duration_seconds", "gRPC request duration")
                        .with_labels(["method", "code"]),
                ),
            )?,
            in_flight: metrics.gauge_vec(
                options("requests_in_flight", "In-flight gRPC requests").with_labels(["method"]),
            )?,
            methods: Arc::new(methods.into_iter().map(Into::into).collect()),
        })
    }

    fn method(&self, path: &str) -> String {
        if self.methods.contains(path) {
            path.to_owned()
        } else {
            UNKNOWN_METHOD.to_owned()
        }
    }

    fn start(&self, path: &str) -> RpcObservation {
        let method = self.method(path);
        self.in_flight
            .inc(&[&method])
            .expect("gRPC in-flight metric labels are fixed");
        RpcObservation {
            metrics: self.clone(),
            method,
            started_at: Instant::now(),
            finished: false,
        }
    }
}

struct RpcObservation {
    metrics: RpcMetrics,
    method: String,
    started_at: Instant,
    finished: bool,
}

impl RpcObservation {
    fn finish(&mut self, code: &str) {
        if self.finished {
            return;
        }
        self.finished = true;
        let labels = [self.method.as_str(), code];
        self.metrics
            .requests
            .inc(&labels)
            .expect("gRPC request metric labels are fixed");
        self.metrics
            .duration
            .observe(self.started_at.elapsed().as_secs_f64(), &labels)
            .expect("gRPC duration metric labels and observation are valid");
        self.metrics
            .in_flight
            .dec(&[&self.method])
            .expect("gRPC in-flight metric labels are fixed");
    }
}

impl Drop for RpcObservation {
    fn drop(&mut self) {
        self.finish("cancelled");
    }
}

/// Tower layer that records complete unary and streaming gRPC calls.
#[derive(Clone)]
pub struct RpcMetricsLayer {
    metrics: RpcMetrics,
}

impl RpcMetricsLayer {
    pub fn new(metrics: RpcMetrics) -> Self {
        Self { metrics }
    }
}

impl<S> Layer<S> for RpcMetricsLayer {
    type Service = RpcMetricsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RpcMetricsService {
            inner,
            metrics: self.metrics.clone(),
        }
    }
}

#[derive(Clone)]
pub struct RpcMetricsService<S> {
    inner: S,
    metrics: RpcMetrics,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for RpcMetricsService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ReqBody: Send + 'static,
    ResBody: Body + Send + 'static,
{
    type Response = Response<RpcMetricsBody<ResBody>>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        let mut observation = self.metrics.start(request.uri().path());
        let future = self.inner.call(request);
        Box::pin(async move {
            match future.await {
                Ok(response) => {
                    let header_code = grpc_code(response.headers())
                        .map(str::to_owned)
                        .or_else(|| (!response.status().is_success()).then(|| "http_error".into()));
                    let (parts, body) = response.into_parts();
                    let mut wrapped = RpcMetricsBody {
                        inner: body,
                        observation: Some(observation),
                    };
                    if let Some(code) = header_code {
                        wrapped.finish(&code);
                    }
                    Ok(Response::from_parts(parts, wrapped))
                }
                Err(error) => {
                    observation.finish("transport_error");
                    Err(error)
                }
            }
        })
    }
}

pin_project! {
    pub struct RpcMetricsBody<B> {
        #[pin]
        inner: B,
        observation: Option<RpcObservation>,
    }

    impl<B> PinnedDrop for RpcMetricsBody<B> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            // Dropping an unfinished response body is a cancelled RPC.
            this.observation.take();
        }
    }
}

impl<B> RpcMetricsBody<B> {
    fn finish(&mut self, code: &str) {
        if let Some(mut observation) = self.observation.take() {
            observation.finish(code);
        }
    }
}

impl<B> Body for RpcMetricsBody<B>
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
                if let Some(code) = frame.trailers_ref().and_then(grpc_code) {
                    if let Some(mut observation) = this.observation.take() {
                        observation.finish(code);
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                if let Some(mut observation) = this.observation.take() {
                    observation.finish("body_error");
                }
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                if let Some(mut observation) = this.observation.take() {
                    observation.finish("0");
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

fn grpc_code(headers: &http::HeaderMap) -> Option<&str> {
    headers
        .get("grpc-status")
        .and_then(|value| value.to_str().ok())
        .map(|code| match code {
            "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "10" | "11" | "12"
            | "13" | "14" | "15" | "16" => code,
            _ => "invalid",
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body::Frame;
    use std::{convert::Infallible, future::Ready};

    #[derive(Clone)]
    struct Reply {
        code: &'static str,
    }

    impl Service<Request<()>> for Reply {
        type Response = Response<OneFrame>;
        type Error = Infallible;
        type Future = Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: Request<()>) -> Self::Future {
            std::future::ready(Ok(Response::new(OneFrame(Some(self.code)))))
        }
    }

    struct OneFrame(Option<&'static str>);

    impl Body for OneFrame {
        type Data = &'static [u8];
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            Poll::Ready(self.0.take().map(|code| {
                let mut trailers = http::HeaderMap::new();
                trailers.insert("grpc-status", code.parse().unwrap());
                Ok(Frame::trailers(trailers))
            }))
        }
    }

    #[tokio::test]
    async fn records_trailer_status_and_bounds_unknown_methods() {
        use tower::ServiceExt;

        let registry = Metrics::new();
        let metrics = RpcMetrics::new(
            &registry,
            "users",
            RpcMetricMode::Server,
            ["/users.Users/Get"],
        )
        .unwrap();
        let mut service = RpcMetricsLayer::new(metrics).layer(Reply { code: "5" });
        let response = service
            .ready()
            .await
            .unwrap()
            .call(Request::builder().uri("/attacker/value").body(()).unwrap())
            .await
            .unwrap();
        let mut body = Box::pin(response.into_body());
        std::future::poll_fn(|context| body.as_mut().poll_frame(context)).await;

        let rendered = registry.render();
        assert!(
            rendered.contains("users_rpc_server_requests_total{method=\"unknown\",code=\"5\"} 1")
        );
        assert!(rendered.contains("users_rpc_server_requests_in_flight{method=\"unknown\"} 0"));
        assert!(!rendered.contains("attacker"));
    }
}
