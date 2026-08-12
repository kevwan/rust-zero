use crate::{
    route::RoutePolicies, AdaptiveLoadShed, ConcurrencyLimit, ContentEncryption, HttpMetrics,
    LoggingMiddleware, MetricsMiddleware, MultipartConfig, Recover, RequestBodyLimit, RequestId,
    ResponsePolicy, RouteGroupConfig, RouteMiddleware, SecurityHeaders, ServerCircuitBreaker,
    StaticAssets, Timeout, TraceContextMiddleware,
};
use actix_web::{
    body::{self, BoxBody},
    dev::{Service, ServiceResponse},
    http::{header::HeaderMap, Method, Uri},
    middleware::Condition,
    web::{self, ServiceConfig},
    App, Error, HttpServer,
};
use rust_zero_core::{
    AdaptiveShedder, CircuitBreakerConfig, LoadShedderConfig, Metrics, RollingCircuitBreakerConfig,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt,
    future::Future,
    io,
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

macro_rules! standard_app {
    ($config:expr, $http_metrics:expr, $adaptive_shedder:expr, $server_breaker:expr, $route_policies:expr, $response_policy:expr, $content_encryption:expr, $static_assets:expr, $configure:expr) => {{
        let config = $config;
        let http_metrics = $http_metrics;
        let mut multipart_config = MultipartConfig::new(
            config.max_multipart_field_bytes,
            config.max_multipart_file_bytes,
            config.max_multipart_total_bytes,
        );
        if let Some(temp_dir) = &config.multipart_temp_dir {
            multipart_config = multipart_config.with_temp_dir(temp_dir);
        }
        let mut timeout = Timeout::new(Duration::from_millis(config.request_timeout_ms));
        let mut concurrency = ConcurrencyLimit::new(config.max_concurrent_requests)
            .with_priority_reserve(config.priority_concurrency_reserve);
        if config.metrics {
            timeout = timeout.with_metrics(http_metrics.clone());
            concurrency = concurrency.with_metrics(http_metrics.clone());
        }
        let mut adaptive_load = AdaptiveLoadShed::new($adaptive_shedder);
        if config.metrics {
            adaptive_load = adaptive_load.with_metrics(http_metrics.clone());
        }
        let mut server_breaker = $server_breaker;
        if config.metrics {
            server_breaker = server_breaker.with_metrics(http_metrics.clone());
        }
        let app = App::new()
            .app_data(web::JsonConfig::default().limit(config.max_body_bytes))
            .app_data(web::FormConfig::default().limit(config.max_body_bytes))
            .app_data(web::Data::new(multipart_config))
            .app_data(web::Data::new($response_policy))
            .wrap(Condition::new(config.logging, LoggingMiddleware))
            .wrap(Condition::new(config.recovery, Recover::new()))
            .wrap(Condition::new(
                config.security_headers,
                SecurityHeaders::new(),
            ))
            .wrap(Condition::new(config.request_ids, RequestId::new()))
            .wrap(Condition::new(
                config.tracing,
                TraceContextMiddleware::new(),
            ))
            .wrap(Condition::new(
                config.metrics,
                MetricsMiddleware::new(http_metrics),
            ))
            .wrap(timeout)
            .wrap(concurrency)
            .wrap(Condition::new(config.adaptive_load_shedding, adaptive_load))
            .wrap(Condition::new(
                config.server_circuit_breaking,
                server_breaker,
            ))
            .wrap(
                RequestBodyLimit::new(config.max_body_bytes)
                    .decompress_gzip(config.decompress_gzip),
            )
            .wrap(
                $content_encryption
                    .unwrap_or_else(|| ContentEncryption::disabled(config.max_body_bytes)),
            )
            .wrap($route_policies)
            .configure($configure);
        if let Some(static_assets) = $static_assets {
            app.app_data(web::Data::new(static_assets))
                .default_service(web::to(
                    |request: actix_web::HttpRequest, assets: web::Data<StaticAssets>| async move {
                        assets.serve(request).await
                    },
                ))
        } else {
            app.default_service(web::to(actix_web::HttpResponse::NotFound))
        }
    }};
}

/// Socket-free HTTP request accepted by [`ServerlessHandler`].
#[derive(Debug, Clone)]
pub struct ServerlessRequest {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: web::Bytes,
}

impl ServerlessRequest {
    pub fn new(method: Method, uri: Uri, body: impl Into<web::Bytes>) -> Self {
        Self {
            method,
            uri,
            headers: HeaderMap::new(),
            body: body.into(),
        }
    }
}

/// Fully buffered response returned to a serverless platform adapter.
#[derive(Debug)]
pub struct ServerlessResponse {
    pub status: actix_web::http::StatusCode,
    pub headers: HeaderMap,
    pub body: web::Bytes,
}

/// Prebuilt, socket-free instance of the standard REST middleware and routing stack.
#[derive(Clone)]
pub struct ServerlessHandler {
    service: actix_service::boxed::RcService<actix_http::Request, ServiceResponse<BoxBody>, Error>,
}

impl ServerlessHandler {
    /// Dispatches one platform-neutral request through the prebuilt REST stack.
    pub async fn call(&self, request: ServerlessRequest) -> Result<ServerlessResponse, Error> {
        let uri = request.uri.to_string();
        let mut builder = actix_web::test::TestRequest::default()
            .method(request.method)
            .uri(&uri)
            .set_payload(request.body);
        for (name, value) in request.headers.iter() {
            builder = builder.append_header((name.clone(), value.clone()));
        }

        let response = self.service.call(builder.to_request()).await?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = body::to_bytes(response.into_body())
            .await
            .map_err(actix_web::error::ErrorInternalServerError)?;
        Ok(ServerlessResponse {
            status,
            headers,
            body,
        })
    }
}

/// Configuration for the standard rust-zero REST server stack.
///
/// Durations are expressed in milliseconds so the same representation works naturally in
/// JSON, TOML, and YAML configuration files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RestServerConfig {
    pub address: SocketAddr,
    pub workers: usize,
    pub shutdown_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub max_body_bytes: usize,
    pub max_multipart_field_bytes: usize,
    pub max_multipart_file_bytes: usize,
    pub max_multipart_total_bytes: usize,
    pub multipart_temp_dir: Option<PathBuf>,
    pub max_concurrent_requests: usize,
    pub priority_concurrency_reserve: usize,
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
    pub route_groups: Vec<RouteGroupConfig>,
}

impl Default for RestServerConfig {
    fn default() -> Self {
        Self {
            address: "0.0.0.0:8080"
                .parse()
                .expect("default REST address is valid"),
            workers: 1,
            shutdown_timeout_ms: 30_000,
            request_timeout_ms: 10_000,
            max_body_bytes: 4 * 1024 * 1024,
            max_multipart_field_bytes: 64 * 1024,
            max_multipart_file_bytes: 4 * 1024 * 1024,
            max_multipart_total_bytes: 4 * 1024 * 1024,
            multipart_temp_dir: None,
            max_concurrent_requests: 1_024,
            priority_concurrency_reserve: 256,
            adaptive_load_shedding: true,
            server_circuit_breaking: true,
            load_shed_cpu_threshold_percent: 90,
            load_shed_bucket_ms: 1_000,
            load_shed_buckets: 10,
            load_shed_cooldown_ms: 1_000,
            logging: true,
            recovery: true,
            tracing: true,
            metrics: true,
            security_headers: true,
            request_ids: true,
            decompress_gzip: true,
            metrics_namespace: "rust_zero".to_owned(),
            route_groups: Vec::new(),
        }
    }
}

impl RestServerConfig {
    pub fn validate(&self) -> Result<(), RestServerConfigError> {
        if self.workers == 0 {
            return Err(RestServerConfigError::Invalid(
                "workers must be greater than zero",
            ));
        }
        if self.shutdown_timeout_ms == 0 {
            return Err(RestServerConfigError::Invalid(
                "shutdown_timeout_ms must be greater than zero",
            ));
        }
        if self.request_timeout_ms == 0 {
            return Err(RestServerConfigError::Invalid(
                "request_timeout_ms must be greater than zero",
            ));
        }
        if self.max_body_bytes == 0 {
            return Err(RestServerConfigError::Invalid(
                "max_body_bytes must be greater than zero",
            ));
        }
        if self.max_multipart_field_bytes == 0 {
            return Err(RestServerConfigError::Invalid(
                "max_multipart_field_bytes must be greater than zero",
            ));
        }
        if self.max_multipart_file_bytes == 0 {
            return Err(RestServerConfigError::Invalid(
                "max_multipart_file_bytes must be greater than zero",
            ));
        }
        if self.max_multipart_total_bytes == 0 {
            return Err(RestServerConfigError::Invalid(
                "max_multipart_total_bytes must be greater than zero",
            ));
        }
        if self.max_multipart_field_bytes > self.max_multipart_total_bytes {
            return Err(RestServerConfigError::Invalid(
                "max_multipart_field_bytes must not exceed max_multipart_total_bytes",
            ));
        }
        if self.max_multipart_file_bytes > self.max_multipart_total_bytes {
            return Err(RestServerConfigError::Invalid(
                "max_multipart_file_bytes must not exceed max_multipart_total_bytes",
            ));
        }
        if self.max_multipart_total_bytes > self.max_body_bytes {
            return Err(RestServerConfigError::Invalid(
                "max_multipart_total_bytes must not exceed max_body_bytes",
            ));
        }
        if self.max_concurrent_requests == 0 {
            return Err(RestServerConfigError::Invalid(
                "max_concurrent_requests must be greater than zero",
            ));
        }
        if self.priority_concurrency_reserve == 0 {
            return Err(RestServerConfigError::Invalid(
                "priority_concurrency_reserve must be greater than zero",
            ));
        }
        if !(1..=100).contains(&self.load_shed_cpu_threshold_percent) {
            return Err(RestServerConfigError::Invalid(
                "load_shed_cpu_threshold_percent must be between 1 and 100",
            ));
        }
        if self.load_shed_bucket_ms == 0 {
            return Err(RestServerConfigError::Invalid(
                "load_shed_bucket_ms must be greater than zero",
            ));
        }
        if self.load_shed_buckets == 0 {
            return Err(RestServerConfigError::Invalid(
                "load_shed_buckets must be greater than zero",
            ));
        }
        if self.load_shed_cooldown_ms == 0 {
            return Err(RestServerConfigError::Invalid(
                "load_shed_cooldown_ms must be greater than zero",
            ));
        }
        if self.metrics && self.metrics_namespace.trim().is_empty() {
            return Err(RestServerConfigError::Invalid(
                "metrics_namespace must not be empty when metrics are enabled",
            ));
        }
        RoutePolicies::compile(&self.route_groups).map_err(RestServerConfigError::RoutePolicy)?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum RestServerConfigError {
    Invalid(&'static str),
    Metrics(rust_zero_core::MetricsError),
    RoutePolicy(String),
    RouteMiddleware(String),
}

impl fmt::Display for RestServerConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
            Self::Metrics(error) => write!(formatter, "failed to configure REST metrics: {error}"),
            Self::RoutePolicy(error) => write!(formatter, "invalid REST route policy: {error}"),
            Self::RouteMiddleware(error) => {
                write!(formatter, "invalid REST route middleware: {error}")
            }
        }
    }
}

impl std::error::Error for RestServerConfigError {}

/// Assembles and runs an Actix server with rust-zero's standard production middleware.
#[derive(Clone)]
pub struct RestServer {
    config: RestServerConfig,
    metrics: Arc<Metrics>,
    http_metrics: HttpMetrics,
    route_policies: RoutePolicies,
    response_policy: ResponsePolicy,
    content_encryption: Option<ContentEncryption>,
    route_middleware: HashMap<String, Arc<dyn RouteMiddleware>>,
    static_assets: Option<StaticAssets>,
    adaptive_shedder: AdaptiveShedder,
    server_breaker: ServerCircuitBreaker,
}

impl RestServer {
    pub fn new(config: RestServerConfig) -> Result<Self, RestServerConfigError> {
        Self::with_metrics(config, Arc::new(Metrics::new()))
    }

    pub fn with_metrics(
        config: RestServerConfig,
        metrics: Arc<Metrics>,
    ) -> Result<Self, RestServerConfigError> {
        config.validate()?;
        let http_metrics = HttpMetrics::new(&metrics, config.metrics_namespace.clone())
            .map_err(RestServerConfigError::Metrics)?;
        let route_policies = RoutePolicies::compile(&config.route_groups)
            .map_err(RestServerConfigError::RoutePolicy)?;
        let adaptive_shedder = AdaptiveShedder::new(
            LoadShedderConfig::production(config.max_concurrent_requests)
                .with_cpu_threshold(f64::from(config.load_shed_cpu_threshold_percent) / 100.0)
                .with_rolling_window(
                    Duration::from_millis(config.load_shed_bucket_ms),
                    config.load_shed_buckets,
                )
                .with_cooldown(Duration::from_millis(config.load_shed_cooldown_ms)),
        );
        let server_breaker = ServerCircuitBreaker::new(CircuitBreakerConfig::rolling(
            RollingCircuitBreakerConfig::new(),
        ));
        Ok(Self {
            config,
            metrics,
            http_metrics,
            route_policies,
            response_policy: ResponsePolicy::new(),
            content_encryption: None,
            route_middleware: HashMap::new(),
            static_assets: None,
            adaptive_shedder,
            server_breaker,
        })
    }

    /// Installs an opt-in static-directory and/or embedded-asset fallback.
    pub fn with_static_assets(mut self, static_assets: StaticAssets) -> Self {
        self.static_assets = Some(static_assets);
        self
    }

    /// Registers application middleware referenced by declarative route groups.
    pub fn with_route_middleware<M>(
        mut self,
        name: impl Into<String>,
        middleware: M,
    ) -> Result<Self, RestServerConfigError>
    where
        M: RouteMiddleware,
    {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(RestServerConfigError::RouteMiddleware(
                "middleware name must not be empty".to_owned(),
            ));
        }
        if self
            .route_middleware
            .insert(name.clone(), Arc::new(middleware))
            .is_some()
        {
            return Err(RestServerConfigError::RouteMiddleware(format!(
                "middleware '{name}' is already registered"
            )));
        }
        Ok(self)
    }

    /// Installs the response policy as Actix application data for all registered handlers.
    pub fn with_response_policy(mut self, response_policy: ResponsePolicy) -> Self {
        self.response_policy = response_policy;
        self
    }

    /// Enables authenticated request decryption and buffered response encryption.
    ///
    /// Install this only for APIs whose clients implement the versioned content-encryption wire
    /// format. Streaming routes such as SSE must remain on a server without this middleware.
    pub fn with_content_encryption(mut self, content_encryption: ContentEncryption) -> Self {
        self.content_encryption = Some(content_encryption);
        self
    }

    /// Selects the result-aware policy shared by the standard stack's per-route breakers.
    pub fn with_server_circuit_breaker(mut self, config: CircuitBreakerConfig) -> Self {
        self.server_breaker = ServerCircuitBreaker::new(config);
        self.config.server_circuit_breaking = true;
        self
    }

    /// Disables the default per-route rolling circuit breaker.
    pub fn without_server_circuit_breaker(mut self) -> Self {
        self.config.server_circuit_breaking = false;
        self
    }

    pub fn config(&self) -> &RestServerConfig {
        &self.config
    }

    pub fn metrics(&self) -> Arc<Metrics> {
        Arc::clone(&self.metrics)
    }

    pub fn response_policy(&self) -> &ResponsePolicy {
        &self.response_policy
    }

    /// Builds a reusable socket-free handler with the same routes, policies, middleware, and
    /// static fallback as [`RestServer::run`].
    pub async fn serverless_handler<F>(
        &self,
        configure: F,
    ) -> Result<ServerlessHandler, RestServerConfigError>
    where
        F: FnOnce(&mut ServiceConfig) + 'static,
    {
        let route_policies = self
            .route_policies
            .clone()
            .with_middleware(self.route_middleware.clone())
            .map_err(RestServerConfigError::RouteMiddleware)?;
        let app = standard_app!(
            &self.config,
            self.http_metrics.clone(),
            self.adaptive_shedder.clone(),
            self.server_breaker.clone(),
            route_policies,
            self.response_policy.clone(),
            self.content_encryption.clone(),
            self.static_assets.clone(),
            configure
        );
        let service = actix_web::test::init_service(app).await;
        let service = Rc::new(service);
        let service = actix_web::dev::fn_service(move |request| {
            let future = service.call(request);
            async move { future.await.map(ServiceResponse::map_into_boxed_body) }
        });
        Ok(ServerlessHandler {
            service: actix_service::boxed::rc_service(service),
        })
    }

    /// Binds the configured listener and installs the standard stack around application routes.
    ///
    /// The configure callback is cloned per Actix worker and can register ordinary routes,
    /// resources, and scoped route groups.
    pub fn run<F>(&self, configure: F) -> io::Result<actix_web::dev::Server>
    where
        F: Fn(&mut ServiceConfig) + Clone + Send + 'static,
    {
        let listener = TcpListener::bind(self.config.address)?;
        self.run_on(listener, configure)
    }

    /// Runs the configured stack on an existing listener.
    ///
    /// Supplying the listener is useful for socket activation and lets tests reserve an ephemeral
    /// port without a bind race.
    pub fn run_on<F>(
        &self,
        listener: TcpListener,
        configure: F,
    ) -> io::Result<actix_web::dev::Server>
    where
        F: Fn(&mut ServiceConfig) + Clone + Send + 'static,
    {
        let config = self.config.clone();
        let http_metrics = self.http_metrics.clone();
        let route_policies = self
            .route_policies
            .clone()
            .with_middleware(self.route_middleware.clone())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let response_policy = self.response_policy.clone();
        let content_encryption = self.content_encryption.clone();
        let static_assets = self.static_assets.clone();
        let adaptive_shedder = self.adaptive_shedder.clone();
        let server_breaker = self.server_breaker.clone();
        let shutdown_seconds = config.shutdown_timeout_ms.div_ceil(1_000);
        let workers = config.workers;

        HttpServer::new(move || {
            standard_app!(
                &config,
                http_metrics.clone(),
                adaptive_shedder.clone(),
                server_breaker.clone(),
                route_policies.clone(),
                response_policy.clone(),
                content_encryption.clone(),
                static_assets.clone(),
                configure.clone()
            )
        })
        .workers(workers)
        .shutdown_timeout(shutdown_seconds)
        .listen(listener)
        .map(HttpServer::run)
    }

    /// Serves until the supplied shutdown signal resolves, then gracefully drains requests.
    pub async fn serve_until<C, F>(&self, configure: C, shutdown: F) -> io::Result<()>
    where
        C: Fn(&mut ServiceConfig) + Clone + Send + 'static,
        F: Future<Output = ()>,
    {
        let server = self.run(configure)?;
        drain_on_signal(server, shutdown).await
    }

    /// Listener-based variant of [`RestServer::serve_until`].
    pub async fn serve_on_until<C, F>(
        &self,
        listener: TcpListener,
        configure: C,
        shutdown: F,
    ) -> io::Result<()>
    where
        C: Fn(&mut ServiceConfig) + Clone + Send + 'static,
        F: Future<Output = ()>,
    {
        let server = self.run_on(listener, configure)?;
        drain_on_signal(server, shutdown).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{
        dev::ServiceRequest,
        http::{header::HeaderValue, StatusCode},
        test as actix_test, web, HttpResponse,
    };
    use rust_zero_core::{parse_config, ConfigFormat};
    use std::path::Path;
    use tokio::sync::Notify;

    #[test]
    fn parses_and_validates_transport_configuration() {
        let config: RestServerConfig = parse_config(
            r#"
address = "127.0.0.1:9000"
workers = 2
request_timeout_ms = 250
adaptive_load_shedding = true
load_shed_cpu_threshold_percent = 85
load_shed_bucket_ms = 250
load_shed_buckets = 8
load_shed_cooldown_ms = 750
max_body_bytes = 16777216
max_multipart_field_bytes = 32768
max_multipart_file_bytes = 8388608
max_multipart_total_bytes = 12582912
multipart_temp_dir = "/tmp/rust-zero-uploads"

[[route_groups]]
prefix = "/api"
timeout_ms = 100
middleware = ["audit"]

[[route_groups.routes]]
method = "GET"
path = "/users/{id}"
public = true
priority = true
"#,
            ConfigFormat::Toml,
        )
        .unwrap();

        assert_eq!(config.address, "127.0.0.1:9000".parse().unwrap());
        assert_eq!(config.workers, 2);
        assert_eq!(config.request_timeout_ms, 250);
        assert!(config.adaptive_load_shedding);
        assert_eq!(config.load_shed_cpu_threshold_percent, 85);
        assert_eq!(config.load_shed_bucket_ms, 250);
        assert_eq!(config.load_shed_buckets, 8);
        assert_eq!(config.load_shed_cooldown_ms, 750);
        assert_eq!(config.max_multipart_field_bytes, 32 * 1024);
        assert_eq!(config.max_multipart_file_bytes, 8 * 1024 * 1024);
        assert_eq!(config.max_multipart_total_bytes, 12 * 1024 * 1024);
        assert_eq!(
            config.multipart_temp_dir.as_deref(),
            Some(Path::new("/tmp/rust-zero-uploads"))
        );
        assert_eq!(config.route_groups[0].prefix, "/api");
        assert_eq!(config.route_groups[0].middleware, ["audit"]);
        assert_eq!(config.route_groups[0].routes[0].path, "/users/{id}");
        config.validate().unwrap();
    }

    #[test]
    fn rejects_zero_limits_before_binding() {
        let error = RestServer::new(RestServerConfig {
            max_body_bytes: 0,
            ..RestServerConfig::default()
        })
        .err()
        .unwrap();
        assert!(error.to_string().contains("max_body_bytes"));

        let error = RestServer::new(RestServerConfig {
            max_multipart_field_bytes: 3,
            max_multipart_file_bytes: 5,
            max_multipart_total_bytes: 4,
            ..RestServerConfig::default()
        })
        .err()
        .unwrap();
        assert!(error.to_string().contains("max_multipart_file_bytes"));

        let error = RestServer::new(RestServerConfig {
            load_shed_cpu_threshold_percent: 0,
            ..RestServerConfig::default()
        })
        .err()
        .unwrap();
        assert!(error
            .to_string()
            .contains("load_shed_cpu_threshold_percent"));
    }

    #[actix_rt::test]
    async fn configured_stack_wraps_registered_routes() {
        let config = RestServerConfig::default();
        let metrics = Metrics::new();
        let http_metrics = HttpMetrics::new(&metrics, config.metrics_namespace.clone()).unwrap();
        let app = actix_test::init_service(
            App::new()
                .wrap(LoggingMiddleware)
                .wrap(Recover::new())
                .wrap(RequestId::new())
                .wrap(TraceContextMiddleware::new())
                .wrap(MetricsMiddleware::new(http_metrics))
                .wrap(Timeout::new(Duration::from_millis(
                    config.request_timeout_ms,
                )))
                .wrap(ConcurrencyLimit::new(config.max_concurrent_requests))
                .wrap(RequestBodyLimit::new(config.max_body_bytes))
                .route("/healthz", web::get().to(HttpResponse::Ok)),
        )
        .await;

        let response = actix_test::call_service(
            &app,
            actix_test::TestRequest::get().uri("/healthz").to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key("x-request-id"));
        assert!(response.headers().contains_key("traceparent"));
        assert!(metrics.render().contains("http_requests_total"));
    }

    #[actix_rt::test]
    async fn serverless_handler_reuses_routes_middleware_metrics_and_static_fallback() {
        let config = RestServerConfig {
            route_groups: vec![RouteGroupConfig {
                prefix: "/api".to_owned(),
                middleware: vec!["tag".to_owned()],
                routes: vec![crate::RoutePolicyConfig {
                    method: "GET".to_owned(),
                    path: "/value".to_owned(),
                    public: true,
                    jwt: None,
                    timeout_ms: None,
                    max_body_bytes: None,
                    priority: None,
                    sse: None,
                }],
                ..RouteGroupConfig::default()
            }],
            ..RestServerConfig::default()
        };
        let server = RestServer::new(config)
            .unwrap()
            .with_route_middleware(
                "tag",
                |request: ServiceRequest, next: crate::RouteMiddlewareNext| async move {
                    let mut response = next.call(request).await?;
                    response.headers_mut().insert(
                        "x-route-middleware".parse().unwrap(),
                        HeaderValue::from_static("yes"),
                    );
                    Ok(response)
                },
            )
            .unwrap()
            .with_static_assets(
                StaticAssets::embedded([(
                    "index.html",
                    crate::EmbeddedAsset::inferred("serverless home"),
                )])
                .unwrap(),
            );
        let metrics = server.metrics();
        let handler = server
            .serverless_handler(|routes| {
                routes.route("/api/value", web::get().to(|| async { "value" }));
            })
            .await
            .unwrap();

        let api = handler
            .call(ServerlessRequest::new(
                Method::GET,
                Uri::from_static("/api/value"),
                web::Bytes::new(),
            ))
            .await
            .unwrap();
        assert_eq!(api.status, StatusCode::OK);
        assert_eq!(api.body, "value");
        assert_eq!(api.headers.get("x-route-middleware").unwrap(), "yes");
        assert!(api.headers.contains_key("x-request-id"));
        assert!(api.headers.contains_key("traceparent"));

        let static_response = handler
            .call(ServerlessRequest::new(
                Method::GET,
                Uri::from_static("/"),
                web::Bytes::new(),
            ))
            .await
            .unwrap();
        assert_eq!(static_response.status, StatusCode::OK);
        assert_eq!(static_response.body, "serverless home");
        assert!(metrics.render().contains("http_requests_total"));
    }

    #[actix_rt::test]
    async fn standard_serverless_stack_installs_per_route_circuit_breaking() {
        let server = RestServer::new(RestServerConfig::default())
            .unwrap()
            .with_server_circuit_breaker(CircuitBreakerConfig::new(1, Duration::from_secs(60)));
        let metrics = server.metrics();
        let handler = server
            .serverless_handler(|routes| {
                routes
                    .route(
                        "/fail/{id}",
                        web::get()
                            .to(|| async { HttpResponse::InternalServerError().body("fail") }),
                    )
                    .route(
                        "/healthy/{id}",
                        web::get().to(|| async { HttpResponse::Ok().body("ok") }),
                    );
            })
            .await
            .unwrap();

        let first = handler
            .call(ServerlessRequest::new(
                Method::GET,
                Uri::from_static("/fail/1"),
                web::Bytes::new(),
            ))
            .await
            .unwrap();
        assert_eq!(first.status, StatusCode::INTERNAL_SERVER_ERROR);

        let rejected = handler
            .call(ServerlessRequest::new(
                Method::GET,
                Uri::from_static("/fail/2"),
                web::Bytes::new(),
            ))
            .await
            .unwrap();
        assert_eq!(rejected.status, StatusCode::SERVICE_UNAVAILABLE);

        let healthy = handler
            .call(ServerlessRequest::new(
                Method::GET,
                Uri::from_static("/healthy/1"),
                web::Bytes::new(),
            ))
            .await
            .unwrap();
        assert_eq!(healthy.status, StatusCode::OK);
        assert!(metrics.render().contains(
            "rust_zero_http_protection_decisions_total{mechanism=\"circuit_breaker\",decision=\"rejected\"} 1"
        ));
    }

    #[actix_rt::test]
    async fn shutdown_signal_gracefully_drains_an_inflight_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (shutdown_sender, shutdown_receiver) = tokio::sync::oneshot::channel();
        let server = RestServer::new(RestServerConfig {
            address,
            shutdown_timeout_ms: 2_000,
            request_timeout_ms: 2_000,
            ..RestServerConfig::default()
        })
        .unwrap();

        let server_task = actix_rt::spawn({
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            async move {
                server
                    .serve_on_until(
                        listener,
                        move |routes| {
                            routes.route(
                                "/slow",
                                web::get().to({
                                    let started = Arc::clone(&started);
                                    let release = Arc::clone(&release);
                                    move || {
                                        let started = Arc::clone(&started);
                                        let release = Arc::clone(&release);
                                        async move {
                                            started.notify_one();
                                            release.notified().await;
                                            HttpResponse::Ok().body("finished")
                                        }
                                    }
                                }),
                            );
                        },
                        async move {
                            let _ = shutdown_receiver.await;
                        },
                    )
                    .await
            }
        });

        let request_task =
            actix_rt::spawn(async move { reqwest::get(format!("http://{address}/slow")).await });
        actix_rt::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .unwrap();
        shutdown_sender.send(()).unwrap();
        actix_rt::time::sleep(Duration::from_millis(20)).await;
        assert!(!request_task.is_finished());

        release.notify_one();
        let response = request_task.await.unwrap().unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "finished");
        server_task.await.unwrap().unwrap();
    }
}
