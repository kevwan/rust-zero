use rust_zero_core::{TraceContext, TraceFlags};
use tonic::{service::Interceptor, Request, Status};

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
}
