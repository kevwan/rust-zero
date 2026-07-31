#[cfg(feature = "telemetry")]
use rust_zero_core::{TelemetrySpan, TelemetrySpanKind};
use rust_zero_core::{TraceContext, TraceFlags};
use tonic::{service::Interceptor, Request, Status};

#[cfg(feature = "telemetry")]
use std::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
#[cfg(feature = "telemetry")]
use tower::{Layer, Service};

/// A W3C trace-context interceptor for Tonic clients and servers.
#[derive(Debug, Clone)]
pub struct RpcTrace {
    mode: Mode,
}

#[derive(Debug, Clone)]
enum Mode {
    Client(Option<TraceContext>),
    Server,
}

impl RpcTrace {
    /// Creates a client interceptor. A configured parent is used to create a child span per call.
    pub fn client(parent: Option<TraceContext>) -> Self {
        Self {
            mode: Mode::Client(parent),
        }
    }

    /// Creates a server interceptor that accepts `traceparent` metadata and creates a server span.
    pub fn server() -> Self {
        Self { mode: Mode::Server }
    }

    /// Retrieves the server span installed in a request's extensions.
    pub fn context<T>(request: &Request<T>) -> Option<TraceContext> {
        request.extensions().get::<TraceContext>().cloned()
    }
}

impl Interceptor for RpcTrace {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        match &self.mode {
            Mode::Client(parent) => {
                let context = parent
                    .as_ref()
                    .map(TraceContext::child)
                    .unwrap_or_else(|| TraceContext::root(TraceFlags::SAMPLED));
                request.metadata_mut().insert(
                    "traceparent",
                    context
                        .traceparent()
                        .parse()
                        .expect("generated traceparent values are valid ASCII metadata"),
                );
                request.extensions_mut().insert(context);
            }
            Mode::Server => {
                let context = request
                    .metadata()
                    .get("traceparent")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| TraceContext::parse(value).ok())
                    .map(|parent| parent.child())
                    .unwrap_or_else(|| TraceContext::root(TraceFlags::SAMPLED));
                request.extensions_mut().insert(context);
            }
        }
        Ok(request)
    }
}

/// Whether a gRPC telemetry layer instruments outbound or inbound requests.
#[cfg(feature = "telemetry")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcTelemetryMode {
    Client,
    Server,
}

/// A Tower layer that creates complete OpenTelemetry spans around gRPC calls.
///
/// Apply [`RpcTelemetryLayer::server`] with `tonic::transport::Server::layer`, or wrap a client
/// channel with [`RpcTelemetryLayer::client`] before constructing a generated Tonic client.
#[cfg(feature = "telemetry")]
#[derive(Debug, Clone, Copy)]
pub struct RpcTelemetryLayer {
    mode: RpcTelemetryMode,
}

#[cfg(feature = "telemetry")]
impl RpcTelemetryLayer {
    pub fn client() -> Self {
        Self {
            mode: RpcTelemetryMode::Client,
        }
    }

    pub fn server() -> Self {
        Self {
            mode: RpcTelemetryMode::Server,
        }
    }

    pub fn wrap<S>(&self, inner: S) -> RpcTelemetryService<S> {
        self.layer(inner)
    }
}

#[cfg(feature = "telemetry")]
impl<S> Layer<S> for RpcTelemetryLayer {
    type Service = RpcTelemetryService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RpcTelemetryService {
            inner,
            mode: self.mode,
        }
    }
}

#[cfg(feature = "telemetry")]
#[derive(Debug, Clone)]
pub struct RpcTelemetryService<S> {
    inner: S,
    mode: RpcTelemetryMode,
}

#[cfg(feature = "telemetry")]
impl<S, RequestBody, ResponseBody> Service<http::Request<RequestBody>> for RpcTelemetryService<S>
where
    S: Service<http::Request<RequestBody>, Response = http::Response<ResponseBody>>,
    S::Future: Send + 'static,
    S::Error: fmt::Display + Send + 'static,
    ResponseBody: Send + 'static,
{
    type Response = http::Response<ResponseBody>;
    type Error = S::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, mut request: http::Request<RequestBody>) -> Self::Future {
        let path = request.uri().path().to_owned();
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
        let span = TelemetrySpan::start(
            path.clone(),
            match self.mode {
                RpcTelemetryMode::Client => TelemetrySpanKind::Client,
                RpcTelemetryMode::Server => TelemetrySpanKind::Server,
            },
            parent.as_ref(),
            [("rpc.system", "grpc".to_owned()), ("rpc.method", path)],
        );

        if let Some(context) = span.trace_context().cloned() {
            if self.mode == RpcTelemetryMode::Client {
                if let Ok(value) = context.traceparent().parse() {
                    request.headers_mut().insert("traceparent", value);
                }
            }
            request.extensions_mut().insert(context);
        }
        let future = self.inner.call(request);

        Box::pin(async move {
            match future.await {
                Ok(response) => {
                    span.set_attribute(
                        "http.response.status_code",
                        response.status().as_u16().to_string(),
                    );
                    if let Some(status) = response
                        .headers()
                        .get("grpc-status")
                        .and_then(|value| value.to_str().ok())
                    {
                        span.set_attribute("rpc.grpc.status_code", status.to_owned());
                        if status != "0" {
                            span.set_error(format!("gRPC status {status}"));
                        }
                    }
                    span.end();
                    Ok(response)
                }
                Err(error) => {
                    span.set_error(error.to_string());
                    span.end();
                    Err(error)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn propagates_a_client_trace_to_a_server_span() {
        let parent =
            TraceContext::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        let mut client = RpcTrace::client(Some(parent));
        let outgoing = client.call(Request::new(())).unwrap();
        let mut server = RpcTrace::server();
        let incoming = server.call(outgoing).unwrap();
        let context = RpcTrace::context(&incoming).unwrap();

        assert_eq!(context.trace_id(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert!(context.parent_span_id().is_some());
    }

    #[cfg(feature = "telemetry")]
    #[tokio::test]
    async fn telemetry_layer_injects_a_child_context() {
        use rust_zero_core::Telemetry;
        use std::{convert::Infallible, future::Ready};

        #[derive(Clone)]
        struct Capture;

        impl Service<http::Request<()>> for Capture {
            type Response = http::Response<String>;
            type Error = Infallible;
            type Future = Ready<Result<Self::Response, Self::Error>>;

            fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, request: http::Request<()>) -> Self::Future {
                std::future::ready(Ok(http::Response::new(
                    request
                        .headers()
                        .get("traceparent")
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                )))
            }
        }

        let telemetry = Telemetry::local("rpc-client", 1.0).unwrap();
        let parent =
            TraceContext::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        let mut request = http::Request::builder()
            .uri("/rust_zero.echo.Echo/Echo")
            .body(())
            .unwrap();
        request.extensions_mut().insert(parent);
        let mut service = RpcTelemetryLayer::client().layer(Capture);

        let traceparent = service.call(request).await.unwrap().into_body();
        assert!(traceparent.contains("4bf92f3577b34da6a3ce929d0e0e4736"));
        telemetry.force_flush().unwrap();
    }
}
