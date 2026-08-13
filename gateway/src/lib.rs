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
use futures::{future::LocalBoxFuture, StreamExt};
use rest::{
    RestCorsConfig, RestServer, RestServerConfig, RestServerConfigError, RestTlsConfig,
    RouteGroupConfig,
};
use rpc::{RpcClient, RpcClientConfig, RpcClientTlsConfig};
use rust_zero_core::{
    EndpointChangeFuture, EndpointSubscription, EtcdClient, EtcdConfig, EtcdTlsConfig,
};
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
    pub max_concurrent_requests: usize,
    pub priority_concurrency_reserve: usize,
    pub rate_limit_requests_per_second: Option<u32>,
    pub rate_limit_burst: Option<u32>,
    pub adaptive_load_shedding: bool,
    pub server_circuit_breaking: bool,
    pub load_shed_cpu_threshold_percent: u8,
    pub load_shed_bucket_ms: u64,
    pub load_shed_buckets: usize,
    pub load_shed_cooldown_ms: u64,
    pub logging: bool,
    pub recovery: bool,
    pub tracing: bool,
    pub metrics: bool,
    pub security_headers: bool,
    pub request_ids: bool,
    pub decompress_gzip: bool,
    pub metrics_namespace: String,
    pub tls: Option<RestTlsConfig>,
    pub cors: Option<RestCorsConfig>,
    /// Declarative authentication and per-route policy for proxy/transcoding route patterns.
    pub route_groups: Vec<RouteGroupConfig>,
    pub routes: Vec<GatewayRoute>,
    /// Descriptor-driven JSON/HTTP to gRPC upstreams mounted by path prefix.
    pub grpc: Vec<GatewayGrpcUpstream>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        let rest = RestServerConfig::default();
        Self {
            address: default_address(),
            workers: default_workers(),
            request_timeout_ms: default_timeout_ms(),
            shutdown_timeout_ms: default_timeout_ms(),
            request_body_limit: default_request_body_limit(),
            response_body_limit: default_response_body_limit(),
            max_concurrent_requests: rest.max_concurrent_requests,
            priority_concurrency_reserve: rest.priority_concurrency_reserve,
            rate_limit_requests_per_second: rest.rate_limit_requests_per_second,
            rate_limit_burst: rest.rate_limit_burst,
            adaptive_load_shedding: rest.adaptive_load_shedding,
            server_circuit_breaking: rest.server_circuit_breaking,
            load_shed_cpu_threshold_percent: rest.load_shed_cpu_threshold_percent,
            load_shed_bucket_ms: rest.load_shed_bucket_ms,
            load_shed_buckets: rest.load_shed_buckets,
            load_shed_cooldown_ms: rest.load_shed_cooldown_ms,
            logging: rest.logging,
            recovery: rest.recovery,
            tracing: rest.tracing,
            metrics: rest.metrics,
            security_headers: rest.security_headers,
            request_ids: rest.request_ids,
            decompress_gzip: rest.decompress_gzip,
            metrics_namespace: rest.metrics_namespace,
            tls: rest.tls,
            cors: rest.cors,
            route_groups: rest.route_groups,
            routes: Vec::new(),
            grpc: Vec::new(),
        }
    }
}

impl GatewayConfig {
    fn rest_config(&self) -> RestServerConfig {
        let mut config = RestServerConfig::default();
        config.address = self.address;
        config.workers = self.workers;
        config.shutdown_timeout_ms = self.shutdown_timeout_ms;
        config.request_timeout_ms = self.request_timeout_ms;
        config.max_body_bytes = self.request_body_limit;
        config.max_multipart_field_bytes = config
            .max_multipart_field_bytes
            .min(self.request_body_limit);
        config.max_multipart_file_bytes =
            config.max_multipart_file_bytes.min(self.request_body_limit);
        config.max_multipart_total_bytes = self.request_body_limit;
        config.max_concurrent_requests = self.max_concurrent_requests;
        config.priority_concurrency_reserve = self.priority_concurrency_reserve;
        config.rate_limit_requests_per_second = self.rate_limit_requests_per_second;
        config.rate_limit_burst = self.rate_limit_burst;
        config.adaptive_load_shedding = self.adaptive_load_shedding;
        config.server_circuit_breaking = self.server_circuit_breaking;
        config.load_shed_cpu_threshold_percent = self.load_shed_cpu_threshold_percent;
        config.load_shed_bucket_ms = self.load_shed_bucket_ms;
        config.load_shed_buckets = self.load_shed_buckets;
        config.load_shed_cooldown_ms = self.load_shed_cooldown_ms;
        config.logging = self.logging;
        config.recovery = self.recovery;
        config.tracing = self.tracing;
        config.metrics = self.metrics;
        config.security_headers = self.security_headers;
        config.request_ids = self.request_ids;
        config.decompress_gzip = self.decompress_gzip;
        config.metrics_namespace = self.metrics_namespace.clone();
        config.tls = self.tls.clone();
        config.cors = self.cors.clone();
        config.route_groups = self.route_groups.clone();
        config
    }

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
        self.rest_config()
            .validate()
            .map_err(GatewayConfigError::Rest)?;
        if self.routes.is_empty() && self.grpc.is_empty() {
            return Err(GatewayConfigError::Invalid(
                "at least one HTTP or gRPC gateway route is required",
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
        let mut prefixes: HashSet<&str> = self
            .routes
            .iter()
            .map(|route| route.prefix.as_str())
            .collect();
        for grpc in &self.grpc {
            grpc.validate()?;
            if !prefixes.insert(&grpc.prefix) {
                return Err(GatewayConfigError::Invalid(
                    "HTTP and gRPC gateway route prefixes must be unique",
                ));
            }
        }
        GatewayRouter::new(self.routes.clone()).map_err(GatewayConfigError::Route)?;
        Ok(())
    }
}

/// A configured gRPC upstream and its public HTTP bindings.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayGrpcUpstream {
    pub prefix: String,
    /// One or more direct/discovered endpoints. Multiple endpoints use Tonic balancing.
    #[serde(default)]
    pub endpoints: Vec<String>,
    /// Optional live etcd service discovery. Mutually exclusive with `endpoints`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<GatewayGrpcEtcdDiscovery>,
    /// A compiled protobuf `FileDescriptorSet`. Omit when `reflection` is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor_set: Option<std::path::PathBuf>,
    #[serde(default)]
    pub reflection: bool,
    #[serde(default)]
    pub annotated_bindings: bool,
    #[serde(default)]
    pub bindings: Vec<HttpBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<RpcClientTlsConfig>,
    /// Fixed bearer token used after any downstream authorization header is stripped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
}

impl std::fmt::Debug for GatewayGrpcUpstream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GatewayGrpcUpstream")
            .field("prefix", &self.prefix)
            .field("endpoints", &self.endpoints)
            .field("discovery", &self.discovery)
            .field("descriptor_set", &self.descriptor_set)
            .field("reflection", &self.reflection)
            .field("annotated_bindings", &self.annotated_bindings)
            .field("bindings", &self.bindings)
            .field("tls", &self.tls)
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

impl GatewayGrpcUpstream {
    fn validate(&self) -> Result<(), GatewayConfigError> {
        let normalized =
            normalize_prefix(self.prefix.clone()).map_err(GatewayConfigError::Route)?;
        if normalized != self.prefix {
            return Err(GatewayConfigError::Invalid(
                "gRPC gateway prefixes must not have a trailing slash",
            ));
        }
        if self.endpoints.is_empty() == self.discovery.is_none() {
            return Err(GatewayConfigError::Invalid(
                "configure exactly one of endpoints or discovery for a gRPC upstream",
            ));
        }
        for endpoint in &self.endpoints {
            let uri: http::Uri = endpoint.parse().map_err(|_| {
                GatewayConfigError::Invalid("gRPC endpoint must be a valid absolute URI")
            })?;
            if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.authority().is_none() {
                return Err(GatewayConfigError::Invalid(
                    "gRPC endpoint must use HTTP or HTTPS and include an authority",
                ));
            }
        }
        if let Some(discovery) = &self.discovery {
            discovery.validate()?;
        }
        if self.descriptor_set.is_some() == self.reflection {
            return Err(GatewayConfigError::Invalid(
                "configure exactly one of descriptor_set or reflection for a gRPC upstream",
            ));
        }
        if !self.annotated_bindings && self.bindings.is_empty() {
            return Err(GatewayConfigError::Invalid(
                "gRPC gateway requires explicit or annotated HTTP bindings",
            ));
        }
        if self
            .bearer_token
            .as_ref()
            .is_some_and(|token| token.trim().is_empty())
        {
            return Err(GatewayConfigError::Invalid(
                "gRPC gateway bearer token must not be empty",
            ));
        }
        Ok(())
    }
}

/// Etcd-backed live endpoint discovery for one gRPC service.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GatewayGrpcEtcdDiscovery {
    pub endpoints: Vec<String>,
    pub namespace: String,
    pub service: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub connect_timeout_ms: u64,
    pub tls: Option<EtcdTlsConfig>,
}

impl Default for GatewayGrpcEtcdDiscovery {
    fn default() -> Self {
        Self {
            endpoints: Vec::new(),
            namespace: "/rust-zero".to_owned(),
            service: String::new(),
            username: None,
            password: None,
            connect_timeout_ms: 10_000,
            tls: None,
        }
    }
}

impl std::fmt::Debug for GatewayGrpcEtcdDiscovery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GatewayGrpcEtcdDiscovery")
            .field("endpoints", &self.endpoints)
            .field("namespace", &self.namespace)
            .field("service", &self.service)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("connect_timeout_ms", &self.connect_timeout_ms)
            .field("tls", &self.tls)
            .finish()
    }
}

impl GatewayGrpcEtcdDiscovery {
    fn validate(&self) -> Result<(), GatewayConfigError> {
        if self.endpoints.is_empty()
            || self
                .endpoints
                .iter()
                .any(|endpoint| endpoint.trim().is_empty())
        {
            return Err(GatewayConfigError::Invalid(
                "gRPC etcd discovery requires non-empty etcd endpoints",
            ));
        }
        if self.service.trim().is_empty() || self.service.contains('/') {
            return Err(GatewayConfigError::Invalid(
                "gRPC etcd discovery service must be a non-empty name",
            ));
        }
        let namespace = self.namespace.trim_matches('/');
        if namespace.is_empty() || namespace.contains('/') {
            return Err(GatewayConfigError::Invalid(
                "gRPC etcd discovery namespace must be a non-empty single path segment",
            ));
        }
        if self.username.is_some() != self.password.is_some() {
            return Err(GatewayConfigError::Invalid(
                "gRPC etcd discovery username and password must be configured together",
            ));
        }
        if self.connect_timeout_ms == 0 {
            return Err(GatewayConfigError::Invalid(
                "gRPC etcd discovery connect timeout must be greater than zero",
            ));
        }
        Ok(())
    }

    fn etcd_config(&self) -> EtcdConfig {
        let mut config = EtcdConfig::new(self.endpoints.clone())
            .with_namespace(&self.namespace)
            .with_timeout(Duration::from_millis(self.connect_timeout_ms));
        if let (Some(username), Some(password)) = (&self.username, &self.password) {
            config = config.with_credentials(username, password);
        }
        if let Some(tls) = &self.tls {
            config = config.with_tls(tls.clone());
        }
        config
    }
}

/// Configuration loading or validation failure.
#[derive(Debug)]
pub enum GatewayConfigError {
    Load(rust_zero_core::ConfigError),
    Route(GatewayError),
    Middleware(String),
    Invalid(&'static str),
    DescriptorIo(io::Error),
    Transcode(TranscodeError),
    Rpc(rpc::RpcClientError),
    Etcd(rust_zero_core::EtcdError),
    Rest(RestServerConfigError),
}

impl std::fmt::Display for GatewayConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Load(error) => write!(formatter, "failed to load gateway configuration: {error}"),
            Self::Route(error) => write!(formatter, "invalid gateway route: {error}"),
            Self::Middleware(error) => write!(formatter, "invalid gateway middleware: {error}"),
            Self::Invalid(message) => formatter.write_str(message),
            Self::DescriptorIo(error) => {
                write!(formatter, "failed to read descriptor set: {error}")
            }
            Self::Transcode(error) => {
                write!(formatter, "failed to configure gRPC transcoding: {error}")
            }
            Self::Rpc(error) => write!(formatter, "failed to configure gRPC upstream: {error}"),
            Self::Etcd(error) => write!(formatter, "failed to configure gRPC discovery: {error}"),
            Self::Rest(error) => write!(formatter, "failed to configure gateway server: {error}"),
        }
    }
}

impl std::error::Error for GatewayConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Load(error) => Some(error),
            Self::Route(error) => Some(error),
            Self::DescriptorIo(error) => Some(error),
            Self::Transcode(error) => Some(error),
            Self::Rpc(error) => Some(error),
            Self::Etcd(error) => Some(error),
            Self::Rest(error) => Some(error),
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
    rest: RestServer,
    proxy: GatewayProxy,
    grpc: Vec<(String, Transcoder)>,
}

impl GatewayServer {
    /// Builds an HTTP-only gateway. Use [`GatewayServer::from_config`] when `config.grpc` is set.
    pub fn new(config: GatewayConfig) -> Result<Self, GatewayConfigError> {
        config.validate()?;
        if !config.grpc.is_empty() {
            return Err(GatewayConfigError::Invalid(
                "gRPC gateway routes require the asynchronous from_config constructor",
            ));
        }
        let router =
            GatewayRouter::new(config.routes.clone()).map_err(GatewayConfigError::Route)?;
        let proxy = GatewayProxy::new(router)
            .with_request_body_limit(config.request_body_limit)
            .with_response_body_limit(config.response_body_limit)
            .with_timeout(Duration::from_millis(config.request_timeout_ms));
        let rest = RestServer::new(config.rest_config()).map_err(GatewayConfigError::Rest)?;
        Ok(Self {
            config,
            rest,
            proxy,
            grpc: Vec::new(),
        })
    }

    /// Builds the complete mixed-protocol gateway, including descriptor or reflection loading.
    pub async fn from_config(config: GatewayConfig) -> Result<Self, GatewayConfigError> {
        config.validate()?;
        let router =
            GatewayRouter::new(config.routes.clone()).map_err(GatewayConfigError::Route)?;
        let proxy = GatewayProxy::new(router)
            .with_request_body_limit(config.request_body_limit)
            .with_response_body_limit(config.response_body_limit)
            .with_timeout(Duration::from_millis(config.request_timeout_ms));
        let mut grpc = Vec::with_capacity(config.grpc.len());
        for upstream in &config.grpc {
            let representative_uri = upstream.endpoints.first().cloned().unwrap_or_else(|| {
                if upstream.tls.is_some() {
                    "https://discovery.invalid".to_owned()
                } else {
                    "http://discovery.invalid".to_owned()
                }
            });
            let mut client_config = RpcClientConfig::new(representative_uri)
                .with_request_timeout(Duration::from_millis(config.request_timeout_ms));
            if let Some(tls) = &upstream.tls {
                client_config = client_config.with_tls(tls.clone());
            }
            let client = RpcClient::try_new(client_config).map_err(|error| {
                GatewayConfigError::Rpc(rpc::RpcClientError::Configuration(error))
            })?;
            let channel = if let Some(discovery) = &upstream.discovery {
                let etcd = EtcdClient::connect(discovery.etcd_config())
                    .await
                    .map_err(GatewayConfigError::Etcd)?;
                let subscription = etcd
                    .subscribe(&discovery.service)
                    .await
                    .map_err(GatewayConfigError::Etcd)?;
                client.connect_discovered(subscription)
            } else if upstream.endpoints.len() == 1 {
                client.connect().await.map_err(GatewayConfigError::Rpc)?
            } else {
                client.connect_discovered(StaticEndpoints(upstream.endpoints.clone()))
            };
            let mut builder = if let Some(path) = &upstream.descriptor_set {
                let bytes = std::fs::read(path).map_err(GatewayConfigError::DescriptorIo)?;
                TranscoderBuilder::from_descriptor_set(bytes, channel)
                    .map_err(GatewayConfigError::Transcode)?
            } else {
                TranscoderBuilder::from_reflection(channel)
                    .await
                    .map_err(GatewayConfigError::Transcode)?
            };
            if upstream.annotated_bindings {
                builder = builder.load_annotated_bindings();
            }
            for binding in &upstream.bindings {
                builder = builder.add_binding(binding.clone());
            }
            if let Some(token) = &upstream.bearer_token {
                builder = builder
                    .with_authorization(format!("Bearer {token}"))
                    .map_err(GatewayConfigError::Transcode)?;
            }
            let transcoder = builder.build().map_err(GatewayConfigError::Transcode)?;
            grpc.push((upstream.prefix.clone(), transcoder));
        }
        grpc.sort_unstable_by_key(|(prefix, _)| Reverse(prefix.len()));
        let rest = RestServer::new(config.rest_config()).map_err(GatewayConfigError::Rest)?;
        Ok(Self {
            config,
            rest,
            proxy,
            grpc,
        })
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

    /// Returns the registry populated by the standard gateway HTTP metrics middleware.
    pub fn metrics(&self) -> Arc<rust_zero_core::Metrics> {
        self.rest.metrics()
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
        let grpc = self.grpc.clone();
        let http_prefixes: Vec<_> = self
            .config
            .routes
            .iter()
            .map(|route| route.prefix.clone())
            .collect();
        self.rest.run_on(listener, move |services| {
            services.app_data(web::Data::new(proxy.clone()));
            for (prefix, transcoder) in &grpc {
                services.service(
                    web::scope(prefix)
                        .app_data(web::Data::new(transcoder.clone()))
                        .service(web::resource("").route(web::route().to(crate::transcode)))
                        .service(
                            web::resource("/{tail:.*}").route(web::route().to(crate::transcode)),
                        ),
                );
            }
            for prefix in &http_prefixes {
                if prefix == "/" {
                    services
                        .service(web::resource("/{tail:.*}").route(web::route().to(crate::proxy)));
                } else {
                    services.service(
                        web::resource(prefix.as_str()).route(web::route().to(crate::proxy)),
                    );
                    services.service(
                        web::resource(format!("{prefix}/{{tail:.*}}"))
                            .route(web::route().to(crate::proxy)),
                    );
                }
            }
        })
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

struct StaticEndpoints(Vec<String>);

impl EndpointSubscription for StaticEndpoints {
    type Error = std::convert::Infallible;

    fn endpoints(&self) -> Vec<String> {
        self.0.clone()
    }

    fn changed(&mut self) -> EndpointChangeFuture<'_, Self::Error> {
        Box::pin(std::future::pending())
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
    use tonic::{transport::Server as TonicServer, Request, Response, Status};

    mod fixture {
        tonic::include_proto!("rust_zero.gateway_test");
    }

    use fixture::{
        greeter_server::{Greeter, GreeterServer},
        GetRequest, GetResponse,
    };

    #[derive(Default)]
    struct GreeterService;

    #[tonic::async_trait]
    impl Greeter for GreeterService {
        type WatchStream = tokio_stream::wrappers::ReceiverStream<Result<GetResponse, Status>>;

        async fn get(&self, request: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
            let authenticated = request
                .metadata()
                .get("authorization")
                .is_some_and(|value| value == "Bearer upstream-secret");
            let request = request.into_inner();
            Ok(Response::new(GetResponse {
                id: request.id,
                message: format!("grpc:{}:{authenticated}", request.view),
            }))
        }

        async fn watch(
            &self,
            _: Request<GetRequest>,
        ) -> Result<Response<Self::WatchStream>, Status> {
            let (_, receiver) = tokio::sync::mpsc::channel(1);
            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                receiver,
            )))
        }

        async fn fail(&self, _: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
            Err(Status::not_found("missing"))
        }
    }

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
max_concurrent_requests = 64
priority_concurrency_reserve = 8
rate_limit_requests_per_second = 100
rate_limit_burst = 25

[cors]
allowed_origins = ["https://console.example"]
allowed_methods = ["GET"]

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
        assert_eq!(config.max_concurrent_requests, 64);
        assert_eq!(config.rate_limit_requests_per_second, Some(100));
        assert_eq!(config.rate_limit_burst, Some(25));
        assert_eq!(
            config.cors.as_ref().unwrap().allowed_origins,
            ["https://console.example"]
        );
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

    #[test]
    fn parses_grpc_configuration_and_redacts_credentials() {
        let config: GatewayConfig = parse_config(
            r#"
[[grpc]]
prefix = "/grpc"
endpoints = ["https://greeter-a:50051", "https://greeter-b:50051"]
reflection = true
bearer_token = "upstream-secret"

[[grpc.bindings]]
verb = "get"
path = "/grpc/greeters/{id}"
rpc = "acme.greeter.v1.Greeter.Get"
"#,
            ConfigFormat::Toml,
        )
        .unwrap();

        config.validate().unwrap();
        assert_eq!(config.grpc[0].bindings[0].verb, HttpVerb::Get);
        let debug = format!("{:?}", config.grpc[0]);
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("upstream-secret"));

        let mut invalid = config.grpc[0].clone();
        invalid.discovery = Some(GatewayGrpcEtcdDiscovery::default());
        assert!(invalid
            .validate()
            .unwrap_err()
            .to_string()
            .contains("exactly one"));
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

    #[actix_web::test]
    async fn configured_server_serves_http_proxy_and_grpc_transcoding_together() {
        let http_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let http_address = http_listener.local_addr().unwrap();
        let http_server = HttpServer::new(|| {
            App::new().default_service(web::to(|| async {
                HttpResponse::Ok().json(serde_json::json!({"protocol": "http"}))
            }))
        })
        .listen(http_listener)
        .unwrap()
        .run();
        let http_handle = http_server.handle();
        actix_web::rt::spawn(http_server);

        let grpc_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let grpc_address = grpc_listener.local_addr().unwrap();
        let grpc_server = tokio::spawn(async move {
            TonicServer::builder()
                .add_service(GreeterServer::new(GreeterService))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(
                    grpc_listener,
                ))
                .await
                .unwrap();
        });

        let descriptor_path = std::env::temp_dir().join(format!(
            "rust-zero-gateway-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &descriptor_path,
            include_bytes!(concat!(env!("OUT_DIR"), "/gateway.bin")),
        )
        .unwrap();

        let gateway_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let gateway_address = gateway_listener.local_addr().unwrap();
        let protected_routes = |prefix: &str| rest::RouteGroupConfig {
            prefix: prefix.to_owned(),
            jwt: Some(rest::RouteJwtConfig {
                secret: "downstream-secret".to_owned(),
                previous_secret: None,
                leeway_seconds: 0,
                claim_projection: Default::default(),
            }),
            routes: ["", "/{tail:.*}"]
                .into_iter()
                .map(|path| rest::RoutePolicyConfig {
                    method: "GET".to_owned(),
                    path: path.to_owned(),
                    public: false,
                    jwt: None,
                    timeout_ms: None,
                    max_body_bytes: None,
                    priority: None,
                    sse: None,
                })
                .collect(),
            ..Default::default()
        };
        let config = GatewayConfig {
            routes: vec![route("/http", &[&format!("http://{http_address}")])],
            grpc: vec![GatewayGrpcUpstream {
                prefix: "/grpc".to_owned(),
                endpoints: vec![format!("http://{grpc_address}")],
                discovery: None,
                descriptor_set: Some(descriptor_path.clone()),
                reflection: false,
                annotated_bindings: false,
                bindings: vec![HttpBinding::new(
                    HttpVerb::Get,
                    "/grpc/greeters/{id}",
                    "rust_zero.gateway_test.Greeter.Get",
                )],
                tls: None,
                bearer_token: Some("upstream-secret".to_owned()),
            }],
            cors: Some(RestCorsConfig::new(["https://console.example"])),
            route_groups: vec![protected_routes("/http"), protected_routes("/grpc")],
            ..GatewayConfig::default()
        };
        let gateway = GatewayServer::from_config(config).await.unwrap();
        let metrics = gateway.metrics();
        let gateway = gateway.run_on(gateway_listener).unwrap();
        let gateway_handle = gateway.handle();
        actix_web::rt::spawn(gateway);

        let client = reqwest::Client::new();
        for path in [
            "/http",
            "/http/orders",
            "/grpc",
            "/grpc/greeters/7?view=configured",
        ] {
            let unauthorized = client
                .get(format!("http://{gateway_address}{path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
        }
        let token = rest::encode_hs256(
            &serde_json::json!({"sub": "gateway-client"}),
            b"downstream-secret",
        )
        .unwrap();
        let http_response = client
            .get(format!("http://{gateway_address}/http/orders"))
            .bearer_auth(&token)
            .header("origin", "https://console.example")
            .header("x-request-id", "gateway-http")
            .send()
            .await
            .unwrap();
        assert_eq!(http_response.headers()["x-request-id"], "gateway-http");
        assert_eq!(http_response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            http_response.headers()["access-control-allow-origin"],
            "https://console.example"
        );
        let http = http_response.bytes().await.unwrap();
        let http: serde_json::Value = serde_json::from_slice(&http).unwrap();
        assert_eq!(http, serde_json::json!({"protocol": "http"}));
        let grpc_response = client
            .get(format!(
                "http://{gateway_address}/grpc/greeters/7?view=configured"
            ))
            .bearer_auth(&token)
            .header("origin", "https://console.example")
            .header("x-request-id", "gateway-grpc")
            .send()
            .await
            .unwrap();
        assert_eq!(grpc_response.headers()["x-request-id"], "gateway-grpc");
        assert_eq!(grpc_response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            grpc_response.headers()["access-control-allow-origin"],
            "https://console.example"
        );
        let grpc = grpc_response.bytes().await.unwrap();
        let grpc: serde_json::Value = serde_json::from_slice(&grpc).unwrap();
        assert_eq!(
            grpc,
            serde_json::json!({"id": 7, "message": "grpc:configured:true"})
        );
        let rendered_metrics = metrics.render();
        assert!(rendered_metrics.contains("path=\"/http/{tail:.*}\""));
        assert!(rendered_metrics.contains("path=\"/grpc/{tail:.*}\""));

        gateway_handle.stop(true).await;
        http_handle.stop(true).await;
        grpc_server.abort();
        std::fs::remove_file(descriptor_path).unwrap();
    }

    #[actix_web::test]
    async fn configured_https_gateway_keeps_observability_middleware_enabled() {
        use rcgen::{
            BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair,
            KeyUsagePurpose,
        };

        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
        let ca_key = KeyPair::generate().unwrap();
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let server_key = KeyPair::generate().unwrap();
        let mut server_params = CertificateParams::new(["localhost".to_owned()]).unwrap();
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_certificate = server_params.signed_by(&server_key, &ca, &ca_key).unwrap();

        let upstream_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream = HttpServer::new(|| {
            App::new().default_service(web::to(|| async { HttpResponse::Ok().body("secure") }))
        })
        .listen(upstream_listener)
        .unwrap()
        .run();
        let upstream_handle = upstream.handle();
        actix_web::rt::spawn(upstream);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let gateway = GatewayServer::new(GatewayConfig {
            routes: vec![route("/api", &[&format!("http://{upstream_address}")])],
            tls: Some(RestTlsConfig::new(
                server_certificate.pem(),
                server_key.serialize_pem(),
            )),
            tracing: true,
            metrics: true,
            request_ids: true,
            ..GatewayConfig::default()
        })
        .unwrap();
        let metrics = gateway.metrics();
        let server = gateway.run_on(listener).unwrap();
        let server_handle = server.handle();
        actix_web::rt::spawn(server);

        let root = reqwest::Certificate::from_pem(ca.pem().as_bytes()).unwrap();
        let client = reqwest::Client::builder()
            .add_root_certificate(root)
            .build()
            .unwrap();
        let trace_id = "4bf92f3577b34da6a3ce929d0e0e4736";
        let response = client
            .get(format!("https://localhost:{}/api/health", address.port()))
            .header("x-request-id", "gateway-tls")
            .header("traceparent", format!("00-{trace_id}-00f067aa0ba902b7-01"))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.headers()["x-request-id"], "gateway-tls");
        assert!(response.headers()["traceparent"]
            .to_str()
            .unwrap()
            .starts_with(&format!("00-{trace_id}-")));
        assert_eq!(response.text().await.unwrap(), "secure");
        assert!(metrics.render().contains("path=\"/api/{tail:.*}\""));

        server_handle.stop(true).await;
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
