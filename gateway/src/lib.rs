//! Health-aware gateway routing and an Actix/Reqwest reverse proxy.

mod transcode;

pub use transcode::{
    grpc_status_to_http, transcode, HttpBinding, HttpVerb, TranscodeError, Transcoder,
    TranscoderBuilder,
};

use actix_web::{
    http::{header, StatusCode},
    web, App, HttpRequest, HttpResponse, HttpServer,
};
use futures::{future::LocalBoxFuture, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    future::Future,
    io,
    net::{SocketAddr, TcpListener},
    path::Path,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

fn default_address() -> SocketAddr {
    "127.0.0.1:8080".parse().expect("static socket address")
}

fn default_workers() -> usize {
    1
}

fn default_timeout_ms() -> u64 {
    30_000
}

fn default_request_body_limit() -> usize {
    10 * 1024 * 1024
}

fn default_response_body_limit() -> usize {
    50 * 1024 * 1024
}

/// File-loadable configuration for the HTTP gateway runtime.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GatewayConfig {
    pub address: SocketAddr,
    pub workers: usize,
    pub request_timeout_ms: u64,
    pub shutdown_timeout_ms: u64,
    pub request_body_limit: usize,
    pub response_body_limit: usize,
    pub routes: Vec<GatewayRoute>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            address: default_address(),
            workers: default_workers(),
            request_timeout_ms: default_timeout_ms(),
            shutdown_timeout_ms: default_timeout_ms(),
            request_body_limit: default_request_body_limit(),
            response_body_limit: default_response_body_limit(),
            routes: Vec::new(),
        }
    }
}

impl GatewayConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, GatewayConfigError> {
        let config: Self = rust_zero_core::load_config(path).map_err(GatewayConfigError::Load)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), GatewayConfigError> {
        if self.workers == 0 {
            return Err(GatewayConfigError::Invalid(
                "workers must be greater than zero",
            ));
        }
        if self.request_timeout_ms == 0 {
            return Err(GatewayConfigError::Invalid(
                "request_timeout_ms must be greater than zero",
            ));
        }
        if self.shutdown_timeout_ms == 0 {
            return Err(GatewayConfigError::Invalid(
                "shutdown_timeout_ms must be greater than zero",
            ));
        }
        if self.request_body_limit == 0 || self.response_body_limit == 0 {
            return Err(GatewayConfigError::Invalid(
                "gateway body limits must be greater than zero",
            ));
        }
        if self.routes.is_empty() {
            return Err(GatewayConfigError::Invalid(
                "at least one gateway route is required",
            ));
        }
        for route in &self.routes {
            let normalized =
                normalize_prefix(route.prefix.clone()).map_err(GatewayConfigError::Route)?;
            if normalized != route.prefix {
                return Err(GatewayConfigError::Invalid(
                    "gateway route prefixes must not have a trailing slash",
                ));
            }
            if route.upstreams.is_empty() {
                return Err(GatewayConfigError::Route(GatewayError::EmptyUpstreams(
                    route.prefix.clone(),
                )));
            }
            validate_middleware_names(&route.middleware).map_err(GatewayConfigError::Middleware)?;
            for upstream in &route.upstreams {
                let url = reqwest::Url::parse(upstream).map_err(|_| {
                    GatewayConfigError::Invalid("gateway upstream must be a valid URL")
                })?;
                if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                    return Err(GatewayConfigError::Invalid(
                        "gateway upstream must use HTTP or HTTPS and include a host",
                    ));
                }
            }
        }
        GatewayRouter::new(self.routes.clone()).map_err(GatewayConfigError::Route)?;
        Ok(())
    }
}

/// Configuration loading or validation failure.
#[derive(Debug)]
pub enum GatewayConfigError {
    Load(rust_zero_core::ConfigError),
    Route(GatewayError),
    Middleware(String),
    Invalid(&'static str),
}

impl std::fmt::Display for GatewayConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Load(error) => write!(formatter, "failed to load gateway configuration: {error}"),
            Self::Route(error) => write!(formatter, "invalid gateway route: {error}"),
            Self::Middleware(error) => write!(formatter, "invalid gateway middleware: {error}"),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for GatewayConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Load(error) => Some(error),
            Self::Route(error) => Some(error),
            Self::Middleware(_) | Self::Invalid(_) => None,
        }
    }
}

/// A configured HTTP path prefix and its upstream endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayRoute {
    pub prefix: String,
    pub upstreams: Vec<String>,
    /// Ordered application middleware applied to requests using this upstream pool.
    #[serde(default)]
    pub middleware: Vec<String>,
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

        Ok(Self {
            prefix,
            upstreams,
            middleware: Vec::new(),
        })
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
        self.select_target_with_middleware(path_and_query)
            .map(|(target, _)| target)
    }

    fn select_target_with_middleware(&self, path_and_query: &str) -> Option<(String, &[String])> {
        let path = path_and_query.split('?').next().unwrap_or(path_and_query);
        let route = self
            .routes
            .iter()
            .find(|route| matches_prefix(path, &route.route.prefix))?;
        select_healthy(route).map(|upstream| {
            let target = format!(
                "{}{}",
                upstream.trim_end_matches('/'),
                if path_and_query.starts_with('/') {
                    path_and_query.to_owned()
                } else {
                    format!("/{path_and_query}")
                }
            );
            (target, route.route.middleware.as_slice())
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

/// An outbound request passed through a configured gateway middleware chain.
pub struct GatewayMiddlewareRequest {
    request: reqwest::Request,
}

impl GatewayMiddlewareRequest {
    pub fn request(&self) -> &reqwest::Request {
        &self.request
    }

    pub fn request_mut(&mut self) -> &mut reqwest::Request {
        &mut self.request
    }

    pub fn into_request(self) -> reqwest::Request {
        self.request
    }
}

/// The boxed future returned by application-defined upstream middleware.
pub type GatewayMiddlewareFuture = LocalBoxFuture<'static, HttpResponse>;

/// The remainder of an upstream middleware chain and its network dispatch.
#[derive(Clone)]
pub struct GatewayMiddlewareNext {
    inner: Rc<dyn Fn(GatewayMiddlewareRequest) -> GatewayMiddlewareFuture>,
}

impl GatewayMiddlewareNext {
    pub fn call(&self, request: GatewayMiddlewareRequest) -> GatewayMiddlewareFuture {
        (self.inner)(request)
    }
}

/// Type-erased application policy registered by name on [`GatewayProxy`] or [`GatewayServer`].
pub trait GatewayMiddleware: Send + Sync + 'static {
    fn call(
        &self,
        request: GatewayMiddlewareRequest,
        next: GatewayMiddlewareNext,
    ) -> GatewayMiddlewareFuture;
}

impl<F, Fut> GatewayMiddleware for F
where
    F: Fn(GatewayMiddlewareRequest, GatewayMiddlewareNext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = HttpResponse> + 'static,
{
    fn call(
        &self,
        request: GatewayMiddlewareRequest,
        next: GatewayMiddlewareNext,
    ) -> GatewayMiddlewareFuture {
        Box::pin((self)(request, next))
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
    middleware: Arc<HashMap<String, Arc<dyn GatewayMiddleware>>>,
}

impl GatewayProxy {
    pub fn new(router: GatewayRouter) -> Self {
        Self {
            router: Arc::new(router),
            client: reqwest::Client::new(),
            request_body_limit: 10 * 1024 * 1024,
            response_body_limit: 50 * 1024 * 1024,
            timeout: Duration::from_secs(30),
            middleware: Arc::new(HashMap::new()),
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

    /// Registers a named policy referenced by one or more configured upstream pools.
    pub fn with_upstream_middleware<M>(
        mut self,
        name: impl Into<String>,
        middleware: M,
    ) -> Result<Self, GatewayConfigError>
    where
        M: GatewayMiddleware,
    {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(GatewayConfigError::Middleware(
                "middleware name must not be empty".to_owned(),
            ));
        }
        let registry = Arc::make_mut(&mut self.middleware);
        if registry
            .insert(name.clone(), Arc::new(middleware))
            .is_some()
        {
            return Err(GatewayConfigError::Middleware(format!(
                "middleware '{name}' is already registered"
            )));
        }
        Ok(self)
    }

    fn validate_middleware(&self) -> Result<(), GatewayConfigError> {
        for route in &self.router.routes {
            for name in &route.route.middleware {
                if !self.middleware.contains_key(name) {
                    return Err(GatewayConfigError::Middleware(format!(
                        "middleware '{name}' is not registered"
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn router(&self) -> &GatewayRouter {
        &self.router
    }

    pub async fn forward(&self, request: HttpRequest, mut payload: web::Payload) -> HttpResponse {
        let path_and_query = request
            .uri()
            .path_and_query()
            .map_or_else(|| request.path(), |value| value.as_str());
        let Some((target, middleware)) = self.router.select_target_with_middleware(path_and_query)
        else {
            return HttpResponse::NotFound().body("no healthy gateway upstream");
        };
        let middleware: Arc<[String]> = middleware.to_vec().into();

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
        let upstream = match upstream.build() {
            Ok(request) => GatewayMiddlewareRequest { request },
            Err(_) => return HttpResponse::BadGateway().body("invalid gateway upstream request"),
        };

        let client = self.client.clone();
        let response_body_limit = self.response_body_limit;
        let terminal = GatewayMiddlewareNext {
            inner: Rc::new(move |request| {
                let client = client.clone();
                Box::pin(async move {
                    dispatch_upstream(client, request.into_request(), response_body_limit).await
                })
            }),
        };
        let registry = Arc::clone(&self.middleware);
        let chain = middleware.iter().rev().fold(terminal, |next, name| {
            let Some(policy) = registry.get(name).cloned() else {
                return GatewayMiddlewareNext {
                    inner: Rc::new(move |_| {
                        Box::pin(async {
                            HttpResponse::InternalServerError()
                                .body("gateway middleware is not registered")
                        })
                    }),
                };
            };
            GatewayMiddlewareNext {
                inner: Rc::new(move |request| policy.call(request, next.clone())),
            }
        });
        chain.call(upstream).await
    }
}

async fn dispatch_upstream(
    client: reqwest::Client,
    request: reqwest::Request,
    response_body_limit: usize,
) -> HttpResponse {
    let response = match client.execute(request).await {
        Ok(response) => response,
        Err(error) if error.is_timeout() => {
            return HttpResponse::GatewayTimeout().body("gateway upstream timed out");
        }
        Err(_) => return HttpResponse::BadGateway().body("gateway upstream unavailable"),
    };
    if response
        .content_length()
        .is_some_and(|length| length > response_body_limit as u64)
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
    let mut downstream = HttpResponse::build(status);
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            header::HeaderName::try_from(name),
            header::HeaderValue::from_bytes(&value),
        ) {
            downstream.insert_header((name, value));
        }
    }
    let limit = response_body_limit;
    let mut received = 0usize;
    let stream = response.bytes_stream().map(move |chunk| match chunk {
        Ok(chunk) => {
            received = received.saturating_add(chunk.len());
            if received > limit {
                Err(actix_web::error::ErrorBadGateway(
                    "gateway response body limit exceeded",
                ))
            } else {
                Ok(chunk)
            }
        }
        Err(_) => Err(actix_web::error::ErrorBadGateway(
            "invalid upstream response",
        )),
    });
    downstream.streaming(stream)
}

/// Configuration-driven Actix gateway with bounded graceful draining.
pub struct GatewayServer {
    config: GatewayConfig,
    proxy: GatewayProxy,
}

impl GatewayServer {
    pub fn new(config: GatewayConfig) -> Result<Self, GatewayConfigError> {
        config.validate()?;
        let router =
            GatewayRouter::new(config.routes.clone()).map_err(GatewayConfigError::Route)?;
        let proxy = GatewayProxy::new(router)
            .with_request_body_limit(config.request_body_limit)
            .with_response_body_limit(config.response_body_limit)
            .with_timeout(Duration::from_millis(config.request_timeout_ms));
        Ok(Self { config, proxy })
    }

    /// Registers a named policy referenced by configured upstream pools.
    pub fn with_upstream_middleware<M>(
        mut self,
        name: impl Into<String>,
        middleware: M,
    ) -> Result<Self, GatewayConfigError>
    where
        M: GatewayMiddleware,
    {
        self.proxy = self.proxy.with_upstream_middleware(name, middleware)?;
        Ok(self)
    }

    pub fn run(&self) -> io::Result<actix_web::dev::Server> {
        self.proxy
            .validate_middleware()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let listener = TcpListener::bind(self.config.address)?;
        self.run_on(listener)
    }

    pub fn run_on(&self, listener: TcpListener) -> io::Result<actix_web::dev::Server> {
        self.proxy
            .validate_middleware()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let proxy = self.proxy.clone();
        let workers = self.config.workers;
        let shutdown_seconds = self.config.shutdown_timeout_ms.div_ceil(1_000);
        HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(proxy.clone()))
                .default_service(web::to(crate::proxy))
        })
        .workers(workers)
        .shutdown_timeout(shutdown_seconds)
        .listen(listener)
        .map(HttpServer::run)
    }

    pub async fn serve_until<F>(&self, shutdown: F) -> io::Result<()>
    where
        F: Future<Output = ()>,
    {
        drain_on_signal(self.run()?, shutdown).await
    }

    pub async fn serve_on_until<F>(&self, listener: TcpListener, shutdown: F) -> io::Result<()>
    where
        F: Future<Output = ()>,
    {
        drain_on_signal(self.run_on(listener)?, shutdown).await
    }
}

async fn drain_on_signal<F>(server: actix_web::dev::Server, shutdown: F) -> io::Result<()>
where
    F: Future<Output = ()>,
{
    use futures::future::{select, Either};

    let handle = server.handle();
    match select(Box::pin(server), Box::pin(shutdown)).await {
        Either::Left((result, _)) => result,
        Either::Right(((), server)) => {
            let (_, result) = futures::future::join(handle.stop(true), server).await;
            result
        }
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

fn validate_middleware_names(names: &[String]) -> Result<(), String> {
    let mut unique = HashSet::new();
    for name in names {
        if name.trim().is_empty() {
            return Err("middleware name must not be empty".to_owned());
        }
        if !unique.insert(name) {
            return Err(format!("duplicate middleware name: {name}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test as actix_test, App, HttpServer};
    use rust_zero_core::{parse_config, ConfigFormat};

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
    fn parses_and_validates_gateway_configuration() {
        let config: GatewayConfig = parse_config(
            r#"
address = "127.0.0.1:9100"
workers = 2
request_timeout_ms = 1500
shutdown_timeout_ms = 4000
request_body_limit = 1024
response_body_limit = 2048

[[routes]]
prefix = "/api"
upstreams = ["http://api-a:8080", "https://api-b:8443"]
middleware = ["sign", "audit"]
"#,
            ConfigFormat::Toml,
        )
        .unwrap();

        config.validate().unwrap();
        assert_eq!(config.workers, 2);
        assert_eq!(config.routes[0].prefix, "/api");
        assert_eq!(config.routes[0].middleware, ["sign", "audit"]);

        let mut invalid = config;
        invalid.routes[0].upstreams[0] = "ftp://api-a".to_owned();
        assert!(invalid.validate().unwrap_err().to_string().contains("HTTP"));
    }

    #[test]
    fn rejects_invalid_or_unregistered_middleware_names() {
        let mut configured = route("/api", &["http://api"]);
        configured.middleware = vec!["audit".to_owned(), "audit".to_owned()];
        let config = GatewayConfig {
            routes: vec![configured],
            ..GatewayConfig::default()
        };
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate"));

        let mut configured = route("/api", &["http://api"]);
        configured.middleware = vec!["audit".to_owned()];
        let proxy = GatewayProxy::new(GatewayRouter::new([configured]).unwrap());
        assert!(proxy
            .validate_middleware()
            .unwrap_err()
            .to_string()
            .contains("not registered"));
    }

    #[actix_web::test]
    async fn applies_named_upstream_middleware_in_order_and_can_short_circuit() {
        let mut configured = route("/api", &["http://unused.invalid"]);
        configured.middleware = vec!["decorate".to_owned(), "authorize".to_owned()];
        let gateway = GatewayProxy::new(GatewayRouter::new([configured]).unwrap())
            .with_upstream_middleware(
                "decorate",
                |mut request: GatewayMiddlewareRequest, next: GatewayMiddlewareNext| async move {
                    request
                        .request_mut()
                        .headers_mut()
                        .insert("x-gateway-policy", "decorated".parse().unwrap());
                    let mut response = next.call(request).await;
                    response.headers_mut().insert(
                        header::HeaderName::from_static("x-policy-response"),
                        header::HeaderValue::from_static("wrapped"),
                    );
                    response
                },
            )
            .unwrap()
            .with_upstream_middleware(
                "authorize",
                |request: GatewayMiddlewareRequest, _next: GatewayMiddlewareNext| async move {
                    assert_eq!(request.request().headers()["x-gateway-policy"], "decorated");
                    HttpResponse::Accepted().body("short-circuited")
                },
            )
            .unwrap();
        gateway.validate_middleware().unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(gateway))
                .default_service(web::to(proxy)),
        )
        .await;
        let response = actix_test::call_service(
            &app,
            actix_test::TestRequest::get()
                .uri("/api/items")
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response.headers().get("x-policy-response").unwrap(),
            "wrapped"
        );
        assert_eq!(actix_test::read_body(response).await, "short-circuited");
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

    #[actix_web::test]
    async fn streams_upstream_response_without_buffering_it() {
        let upstream_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream = HttpServer::new(|| {
            App::new().default_service(web::to(|| async {
                let chunks = futures::stream::unfold(0, |step| async move {
                    match step {
                        0 => Some((
                            Ok::<_, actix_web::Error>(web::Bytes::from_static(b"first")),
                            1,
                        )),
                        1 => {
                            tokio::time::sleep(Duration::from_millis(250)).await;
                            Some((Ok(web::Bytes::from_static(b"second")), 2))
                        }
                        _ => None,
                    }
                });
                HttpResponse::Ok().streaming(chunks)
            }))
        })
        .listen(upstream_listener)
        .unwrap()
        .run();
        let upstream_handle = upstream.handle();
        actix_web::rt::spawn(upstream);

        let gateway_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let gateway_address = gateway_listener.local_addr().unwrap();
        let gateway = GatewayServer::new(GatewayConfig {
            routes: vec![route("/", &[&format!("http://{upstream_address}")])],
            ..GatewayConfig::default()
        })
        .unwrap()
        .run_on(gateway_listener)
        .unwrap();
        let gateway_handle = gateway.handle();
        actix_web::rt::spawn(gateway);

        let started = tokio::time::Instant::now();
        let response = reqwest::get(format!("http://{gateway_address}/events"))
            .await
            .unwrap();
        let mut body = response.bytes_stream();
        assert_eq!(body.next().await.unwrap().unwrap(), "first");
        assert!(started.elapsed() < Duration::from_millis(200));
        assert_eq!(body.next().await.unwrap().unwrap(), "second");

        gateway_handle.stop(true).await;
        upstream_handle.stop(true).await;
    }

    #[actix_web::test]
    async fn shutdown_gracefully_drains_an_inflight_proxy_request() {
        let request_started = Arc::new(tokio::sync::Notify::new());
        let release_request = Arc::new(tokio::sync::Notify::new());
        let upstream_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream = HttpServer::new({
            let request_started = request_started.clone();
            let release_request = release_request.clone();
            move || {
                App::new()
                    .app_data(web::Data::new((
                        request_started.clone(),
                        release_request.clone(),
                    )))
                    .default_service(web::to(
                        |signals: web::Data<(
                            Arc<tokio::sync::Notify>,
                            Arc<tokio::sync::Notify>,
                        )>| async move {
                            signals.0.notify_one();
                            signals.1.notified().await;
                            HttpResponse::Ok().body("drained")
                        },
                    ))
            }
        })
        .listen(upstream_listener)
        .unwrap()
        .run();
        let upstream_handle = upstream.handle();
        actix_web::rt::spawn(upstream);

        let gateway_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let gateway_address = gateway_listener.local_addr().unwrap();
        let server = GatewayServer::new(GatewayConfig {
            routes: vec![route("/", &[&format!("http://{upstream_address}")])],
            shutdown_timeout_ms: 2_000,
            ..GatewayConfig::default()
        })
        .unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let serving = actix_web::rt::spawn(async move {
            server
                .serve_on_until(gateway_listener, async {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        let request = actix_web::rt::spawn(async move {
            reqwest::get(format!("http://{gateway_address}/slow"))
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        });
        request_started.notified().await;
        shutdown_tx.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!serving.is_finished());
        release_request.notify_one();

        assert_eq!(request.await.unwrap(), "drained");
        serving.await.unwrap().unwrap();
        upstream_handle.stop(true).await;
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
