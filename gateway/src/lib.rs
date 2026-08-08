//! Health-aware gateway routing and an Actix/Reqwest reverse proxy.

mod transcode;

pub use transcode::{
    grpc_status_to_http, transcode, HttpBinding, HttpVerb, TranscodeError, Transcoder,
    TranscoderBuilder,
};

use actix_web::{
    http::{header, StatusCode},
    web, HttpRequest, HttpResponse,
};
use futures::StreamExt;
use std::{
    cmp::Reverse,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

/// A configured HTTP path prefix and its upstream endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayRoute {
    pub prefix: String,
    pub upstreams: Vec<String>,
}

impl GatewayRoute {
    pub fn new(prefix: impl Into<String>, upstreams: Vec<String>) -> Result<Self, GatewayError> {
        let prefix = normalize_prefix(prefix.into())?;
        if upstreams.is_empty() {
            return Err(GatewayError::EmptyUpstreams(prefix));
        }
        if upstreams.iter().any(|upstream| upstream.is_empty()) {
            return Err(GatewayError::EmptyUpstream);
        }

        Ok(Self { prefix, upstreams })
    }
}

struct RoutePool {
    route: GatewayRoute,
    next_upstream: AtomicUsize,
    healthy: Vec<AtomicBool>,
}

/// Selects the most specific route and distributes requests across its upstreams.
pub struct GatewayRouter {
    routes: Vec<RoutePool>,
}

impl GatewayRouter {
    pub fn new(routes: impl IntoIterator<Item = GatewayRoute>) -> Result<Self, GatewayError> {
        let mut routes: Vec<_> = routes
            .into_iter()
            .map(|route| RoutePool {
                healthy: route
                    .upstreams
                    .iter()
                    .map(|_| AtomicBool::new(true))
                    .collect(),
                route,
                next_upstream: AtomicUsize::new(0),
            })
            .collect();

        routes.sort_unstable_by_key(|route| Reverse(route.route.prefix.len()));
        for routes_with_same_prefix in routes.windows(2) {
            if routes_with_same_prefix[0].route.prefix == routes_with_same_prefix[1].route.prefix {
                return Err(GatewayError::DuplicatePrefix(
                    routes_with_same_prefix[0].route.prefix.clone(),
                ));
            }
        }

        Ok(Self { routes })
    }

    /// Selects an upstream for a request path, using round robin within its matched route.
    pub fn select(&self, path: &str) -> Option<&str> {
        self.routes
            .iter()
            .find(|route| matches_prefix(path, &route.route.prefix))
            .and_then(select_healthy)
    }

    /// Builds the full upstream URL for a path and optional query string.
    pub fn select_target(&self, path_and_query: &str) -> Option<String> {
        let path = path_and_query.split('?').next().unwrap_or(path_and_query);
        self.select(path).map(|upstream| {
            format!(
                "{}{}",
                upstream.trim_end_matches('/'),
                if path_and_query.starts_with('/') {
                    path_and_query.to_owned()
                } else {
                    format!("/{path_and_query}")
                }
            )
        })
    }

    /// Includes or excludes an upstream from selection, for use by active health checks.
    pub fn set_upstream_health(
        &self,
        prefix: &str,
        upstream: &str,
        healthy: bool,
    ) -> Result<(), GatewayError> {
        let route = self
            .routes
            .iter()
            .find(|route| route.route.prefix == prefix)
            .ok_or_else(|| GatewayError::UnknownPrefix(prefix.to_owned()))?;
        let index = route
            .route
            .upstreams
            .iter()
            .position(|candidate| candidate == upstream)
            .ok_or_else(|| GatewayError::UnknownUpstream(upstream.to_owned()))?;
        route.healthy[index].store(healthy, Ordering::Release);
        Ok(())
    }
}

fn select_healthy(route: &RoutePool) -> Option<&str> {
    let len = route.route.upstreams.len();
    let start = route.next_upstream.fetch_add(1, Ordering::Relaxed) % len;
    (0..len).find_map(|offset| {
        let index = (start + offset) % len;
        route.healthy[index]
            .load(Ordering::Acquire)
            .then(|| route.route.upstreams[index].as_str())
    })
}

/// An HTTP reverse proxy backed by a [`GatewayRouter`].
#[derive(Clone)]
pub struct GatewayProxy {
    router: Arc<GatewayRouter>,
    client: reqwest::Client,
    request_body_limit: usize,
    response_body_limit: usize,
    timeout: Duration,
}

impl GatewayProxy {
    pub fn new(router: GatewayRouter) -> Self {
        Self {
            router: Arc::new(router),
            client: reqwest::Client::new(),
            request_body_limit: 10 * 1024 * 1024,
            response_body_limit: 50 * 1024 * 1024,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    pub fn with_request_body_limit(mut self, bytes: usize) -> Self {
        assert!(bytes > 0, "request body limit must be greater than zero");
        self.request_body_limit = bytes;
        self
    }

    pub fn with_response_body_limit(mut self, bytes: usize) -> Self {
        assert!(bytes > 0, "response body limit must be greater than zero");
        self.response_body_limit = bytes;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        assert!(
            !timeout.is_zero(),
            "gateway timeout must be greater than zero"
        );
        self.timeout = timeout;
        self
    }

    pub fn router(&self) -> &GatewayRouter {
        &self.router
    }

    pub async fn forward(&self, request: HttpRequest, mut payload: web::Payload) -> HttpResponse {
        let path_and_query = request
            .uri()
            .path_and_query()
            .map_or_else(|| request.path(), |value| value.as_str());
        let Some(target) = self.router.select_target(path_and_query) else {
            return HttpResponse::NotFound().body("no healthy gateway upstream");
        };

        let mut request_body = web::BytesMut::new();
        while let Some(chunk) = payload.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => return HttpResponse::BadRequest().body("invalid request body"),
            };
            if request_body.len().saturating_add(chunk.len()) > self.request_body_limit {
                return HttpResponse::PayloadTooLarge().body("gateway request body limit exceeded");
            }
            request_body.extend_from_slice(&chunk);
        }

        let method = match reqwest::Method::from_bytes(request.method().as_str().as_bytes()) {
            Ok(method) => method,
            Err(_) => return HttpResponse::BadRequest().body("invalid request method"),
        };
        let mut upstream = self
            .client
            .request(method, target)
            .timeout(self.timeout)
            .body(request_body.freeze());
        for (name, value) in request.headers() {
            if is_hop_by_hop(name.as_str())
                || name == header::HOST
                || name == header::CONTENT_LENGTH
            {
                continue;
            }
            upstream = upstream.header(name.as_str(), value.as_bytes());
        }
        if let Some(peer) = request.peer_addr() {
            upstream = upstream.header("x-forwarded-for", peer.ip().to_string());
        }
        upstream = upstream
            .header("x-forwarded-proto", request.connection_info().scheme())
            .header("x-forwarded-host", request.connection_info().host());

        let response = match upstream.send().await {
            Ok(response) => response,
            Err(error) if error.is_timeout() => {
                return HttpResponse::GatewayTimeout().body("gateway upstream timed out");
            }
            Err(_) => return HttpResponse::BadGateway().body("gateway upstream unavailable"),
        };
        if response
            .content_length()
            .is_some_and(|length| length > self.response_body_limit as u64)
        {
            return HttpResponse::BadGateway().body("gateway response body limit exceeded");
        }

        let status =
            StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let headers: Vec<_> = response
            .headers()
            .iter()
            .filter(|(name, _)| {
                !is_hop_by_hop(name.as_str()) && *name != reqwest::header::CONTENT_LENGTH
            })
            .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
            .collect();
        let mut response_body = web::BytesMut::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => return HttpResponse::BadGateway().body("invalid upstream response"),
            };
            if response_body.len().saturating_add(chunk.len()) > self.response_body_limit {
                return HttpResponse::BadGateway().body("gateway response body limit exceeded");
            }
            response_body.extend_from_slice(&chunk);
        }

        let mut downstream = HttpResponse::build(status);
        for (name, value) in headers {
            if let (Ok(name), Ok(value)) = (
                header::HeaderName::try_from(name),
                header::HeaderValue::from_bytes(&value),
            ) {
                downstream.insert_header((name, value));
            }
        }
        downstream.body(response_body.freeze())
    }
}

/// Actix handler for mounting a [`GatewayProxy`] stored in `web::Data`.
pub async fn proxy(
    gateway: web::Data<GatewayProxy>,
    request: HttpRequest,
    payload: web::Payload,
) -> HttpResponse {
    gateway.forward(request, payload).await
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// Errors produced by invalid gateway routing configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayError {
    InvalidPrefix(String),
    EmptyUpstreams(String),
    EmptyUpstream,
    DuplicatePrefix(String),
    UnknownPrefix(String),
    UnknownUpstream(String),
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPrefix(prefix) => {
                write!(formatter, "gateway prefix must start with '/': {prefix}")
            }
            Self::EmptyUpstreams(prefix) => {
                write!(formatter, "gateway route {prefix} has no upstreams")
            }
            Self::EmptyUpstream => formatter.write_str("gateway upstream cannot be empty"),
            Self::DuplicatePrefix(prefix) => {
                write!(formatter, "duplicate gateway prefix: {prefix}")
            }
            Self::UnknownPrefix(prefix) => write!(formatter, "unknown gateway prefix: {prefix}"),
            Self::UnknownUpstream(upstream) => {
                write!(formatter, "unknown gateway upstream: {upstream}")
            }
        }
    }
}

impl std::error::Error for GatewayError {}

fn normalize_prefix(mut prefix: String) -> Result<String, GatewayError> {
    if !prefix.starts_with('/') {
        return Err(GatewayError::InvalidPrefix(prefix));
    }
    while prefix.len() > 1 && prefix.ends_with('/') {
        prefix.pop();
    }
    Ok(prefix)
}

fn matches_prefix(path: &str, prefix: &str) -> bool {
    prefix == "/"
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test as actix_test, App, HttpServer};

    fn route(prefix: &str, upstreams: &[&str]) -> GatewayRoute {
        GatewayRoute::new(
            prefix,
            upstreams
                .iter()
                .map(|upstream| (*upstream).to_owned())
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn selects_the_most_specific_matching_prefix() {
        let router = GatewayRouter::new([
            route("/", &["http://home"]),
            route("/api", &["http://api"]),
            route("/api/admin", &["http://admin"]),
        ])
        .unwrap();

        assert_eq!(router.select("/api/admin/users"), Some("http://admin"));
        assert_eq!(router.select("/api/users"), Some("http://api"));
        assert_eq!(router.select("/apix"), Some("http://home"));
    }

    #[test]
    fn rotates_through_matched_route_upstreams() {
        let router = GatewayRouter::new([route("/api", &["http://one", "http://two"])]).unwrap();

        assert_eq!(router.select("/api/items"), Some("http://one"));
        assert_eq!(router.select("/api/items"), Some("http://two"));
        assert_eq!(router.select("/api/items"), Some("http://one"));
    }

    #[test]
    fn skips_unhealthy_upstreams_and_builds_targets() {
        let router = GatewayRouter::new([route("/api", &["http://one/", "http://two"])]).unwrap();
        router
            .set_upstream_health("/api", "http://one/", false)
            .unwrap();

        assert_eq!(router.select("/api/items"), Some("http://two"));
        assert_eq!(
            router.select_target("/api/items?page=2"),
            Some("http://two/api/items?page=2".to_owned())
        );
        router
            .set_upstream_health("/api", "http://two", false)
            .unwrap();
        assert_eq!(router.select("/api/items"), None);
    }

    #[test]
    fn identifies_hop_by_hop_headers() {
        assert!(is_hop_by_hop("Connection"));
        assert!(is_hop_by_hop("transfer-encoding"));
        assert!(!is_hop_by_hop("content-type"));
    }

    #[actix_web::test]
    async fn forwards_requests_and_upstream_responses() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = HttpServer::new(|| {
            App::new().default_service(web::to(
                |request: HttpRequest, body: web::Bytes| async move {
                    HttpResponse::Created()
                        .insert_header(("x-upstream", "yes"))
                        .body(format!(
                            "{} {} {}",
                            request.method(),
                            request.uri(),
                            String::from_utf8_lossy(&body)
                        ))
                },
            ))
        })
        .listen(listener)
        .unwrap()
        .run();
        let server_handle = server.handle();
        actix_web::rt::spawn(server);

        let gateway = GatewayProxy::new(
            GatewayRouter::new([route("/api", &[&format!("http://{address}")])]).unwrap(),
        );
        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(gateway))
                .default_service(web::to(proxy)),
        )
        .await;
        let response = actix_test::call_service(
            &app,
            actix_test::TestRequest::post()
                .uri("/api/hello?x=1")
                .set_payload("world")
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers().get("x-upstream").unwrap(), "yes");
        assert_eq!(
            actix_test::read_body(response).await,
            "POST /api/hello?x=1 world"
        );
        server_handle.stop(true).await;
    }

    #[test]
    fn rejects_invalid_routes() {
        assert_eq!(
            GatewayRoute::new("api", vec!["http://api".to_owned()]).unwrap_err(),
            GatewayError::InvalidPrefix("api".to_owned())
        );
        assert_eq!(
            GatewayRoute::new("/api", Vec::new()).unwrap_err(),
            GatewayError::EmptyUpstreams("/api".to_owned())
        );
    }
}
