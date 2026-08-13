use crate::{
    route::RoutePolicies, AdaptiveLoadShed, ConcurrencyLimit, ContentEncryption, HttpMetrics,
    LoggingMiddleware, MetricsMiddleware, MultipartConfig, RateLimit, Recover, RequestBodyLimit,
    RequestId, ResponsePolicy, RouteGroupConfig, RouteMiddleware, SecurityHeaders,
    ServerCircuitBreaker, StaticAssets, Timeout, TraceContextMiddleware,
};
use actix_cors::Cors;
use actix_web::{
    body::{self, BoxBody},
    dev::{Service, ServiceResponse},
    http::{header::HeaderMap, header::HeaderName, Method, Uri},
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
    io::{self, BufReader, Cursor},
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

/// PEM identity and optional client CA used by the standard HTTPS listener.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestTlsConfig {
    pub certificate_pem: String,
    pub private_key_pem: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ca_pem: Option<String>,
}

impl fmt::Debug for RestTlsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RestTlsConfig")
            .field("certificate_pem", &"[PEM]")
            .field("private_key_pem", &"[REDACTED]")
            .field(
                "client_ca_pem",
                &self.client_ca_pem.as_ref().map(|_| "[PEM]"),
            )
            .finish()
    }
}

impl RestTlsConfig {
    pub fn new(certificate_pem: impl Into<String>, private_key_pem: impl Into<String>) -> Self {
        Self {
            certificate_pem: certificate_pem.into(),
            private_key_pem: private_key_pem.into(),
            client_ca_pem: None,
        }
    }

    pub fn with_client_ca(mut self, client_ca_pem: impl Into<String>) -> Self {
        self.client_ca_pem = Some(client_ca_pem.into());
        self
    }

    fn validate(&self) -> Result<(), RestServerConfigError> {
        if self.certificate_pem.trim().is_empty() {
            return Err(RestServerConfigError::Invalid(
                "TLS certificate must not be empty",
            ));
        }
        if self.private_key_pem.trim().is_empty() {
            return Err(RestServerConfigError::Invalid(
                "TLS private key must not be empty",
            ));
        }
        if self
            .client_ca_pem
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(RestServerConfigError::Invalid(
                "TLS client CA must not be empty",
            ));
        }
        Ok(())
    }

    fn rustls_config(&self) -> Result<rustls::ServerConfig, RestServerConfigError> {
        self.validate()?;
        let mut certificates = BufReader::new(Cursor::new(self.certificate_pem.as_bytes()));
        let certificates = rustls_pemfile::certs(&mut certificates)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| RestServerConfigError::Tls(error.to_string()))?;
        if certificates.is_empty() {
            return Err(RestServerConfigError::Tls(
                "TLS certificate PEM contains no certificates".to_owned(),
            ));
        }
        let mut private_key = BufReader::new(Cursor::new(self.private_key_pem.as_bytes()));
        let private_key = rustls_pemfile::private_key(&mut private_key)
            .map_err(|error| RestServerConfigError::Tls(error.to_string()))?
            .ok_or_else(|| {
                RestServerConfigError::Tls("TLS private key PEM contains no private key".to_owned())
            })?;

        let builder = rustls::ServerConfig::builder();
        let config = if let Some(client_ca) = &self.client_ca_pem {
            let mut roots = rustls::RootCertStore::empty();
            let mut reader = BufReader::new(Cursor::new(client_ca.as_bytes()));
            let roots_to_add = rustls_pemfile::certs(&mut reader)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| RestServerConfigError::Tls(error.to_string()))?;
            if roots_to_add.is_empty() {
                return Err(RestServerConfigError::Tls(
                    "TLS client CA PEM contains no certificates".to_owned(),
                ));
            }
            for certificate in roots_to_add {
                roots
                    .add(certificate)
                    .map_err(|error| RestServerConfigError::Tls(error.to_string()))?;
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .map_err(|error| RestServerConfigError::Tls(error.to_string()))?;
            builder
                .with_client_cert_verifier(verifier)
                .with_single_cert(certificates, private_key)
        } else {
            builder
                .with_no_client_auth()
                .with_single_cert(certificates, private_key)
        };
        config.map_err(|error| RestServerConfigError::Tls(error.to_string()))
    }
}

/// Cross-origin policy installed by the standard REST and serverless stacks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RestCorsConfig {
    /// Exact serialized origins, or a single `*` to reflect any request origin.
    pub allowed_origins: Vec<String>,
    /// HTTP methods accepted by preflight, or a single `*` for all standard methods.
    pub allowed_methods: Vec<String>,
    /// Request header names accepted by preflight, or a single `*` for any header.
    pub allowed_headers: Vec<String>,
    /// Response header names exposed to browser code, or a single `*` for any header.
    pub exposed_headers: Vec<String>,
    pub allow_credentials: bool,
    pub max_age_seconds: Option<usize>,
    pub automatic_preflight: bool,
}

impl Default for RestCorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: Vec::new(),
            allowed_methods: vec![
                "GET".to_owned(),
                "HEAD".to_owned(),
                "POST".to_owned(),
                "PUT".to_owned(),
                "PATCH".to_owned(),
                "DELETE".to_owned(),
                "OPTIONS".to_owned(),
            ],
            allowed_headers: Vec::new(),
            exposed_headers: Vec::new(),
            allow_credentials: false,
            max_age_seconds: Some(3_600),
            automatic_preflight: true,
        }
    }
}

impl RestCorsConfig {
    pub fn new(origins: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            allowed_origins: origins.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    pub fn with_methods(mut self, methods: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.allowed_methods = methods.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_allowed_headers(
        mut self,
        headers: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.allowed_headers = headers.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_exposed_headers(
        mut self,
        headers: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.exposed_headers = headers.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_credentials(mut self, allow: bool) -> Self {
        self.allow_credentials = allow;
        self
    }

    pub fn with_max_age(mut self, seconds: Option<usize>) -> Self {
        self.max_age_seconds = seconds;
        self
    }

    pub fn with_automatic_preflight(mut self, enabled: bool) -> Self {
        self.automatic_preflight = enabled;
        self
    }

    fn validate(&self) -> Result<(), RestServerConfigError> {
        if self.allowed_origins.is_empty() {
            return Err(RestServerConfigError::Invalid(
                "CORS allowed_origins must not be empty",
            ));
        }
        validate_wildcard_list(
            &self.allowed_origins,
            "CORS allowed_origins must contain either * or explicit origins",
        )?;
        for origin in &self.allowed_origins {
            if origin == "*" || origin == "null" {
                continue;
            }
            let uri = origin.parse::<Uri>().map_err(|_| {
                RestServerConfigError::Invalid(
                    "CORS allowed_origins must contain valid serialized origins",
                )
            })?;
            if uri.scheme().is_none()
                || uri.authority().is_none()
                || uri.path() != "/"
                || uri.query().is_some()
            {
                return Err(RestServerConfigError::Invalid(
                    "CORS allowed_origins must contain valid serialized origins",
                ));
            }
        }

        if self.allowed_methods.is_empty() {
            return Err(RestServerConfigError::Invalid(
                "CORS allowed_methods must not be empty",
            ));
        }
        validate_wildcard_list(
            &self.allowed_methods,
            "CORS allowed_methods must contain either * or explicit methods",
        )?;
        for method in &self.allowed_methods {
            if method != "*"
                && (method.trim() != method || Method::from_bytes(method.as_bytes()).is_err())
            {
                return Err(RestServerConfigError::Invalid(
                    "CORS allowed_methods contains an invalid HTTP method",
                ));
            }
        }

        validate_header_list(
            &self.allowed_headers,
            "CORS allowed_headers contains an invalid HTTP header name",
        )?;
        validate_header_list(
            &self.exposed_headers,
            "CORS exposed_headers contains an invalid HTTP header name",
        )?;
        Ok(())
    }

    fn middleware(&self) -> Cors {
        let mut cors = Cors::default();
        if self.allowed_origins == ["*"] {
            cors = cors.allow_any_origin();
        } else {
            for origin in &self.allowed_origins {
                cors = cors.allowed_origin(origin);
            }
        }
        if self.allowed_methods == ["*"] {
            cors = cors.allow_any_method();
        } else {
            cors = cors.allowed_methods(self.allowed_methods.iter().map(String::as_str));
        }
        if self.allowed_headers == ["*"] {
            cors = cors.allow_any_header();
        } else {
            cors = cors.allowed_headers(self.allowed_headers.iter().map(String::as_str));
        }
        if self.exposed_headers == ["*"] {
            cors = cors.expose_any_header();
        } else {
            cors = cors.expose_headers(self.exposed_headers.iter().map(String::as_str));
        }
        cors = cors.max_age(self.max_age_seconds);
        if self.allow_credentials {
            cors = cors.supports_credentials();
        }
        if !self.automatic_preflight {
            cors = cors.disable_preflight();
        }
        cors
    }
}

fn validate_wildcard_list(
    values: &[String],
    message: &'static str,
) -> Result<(), RestServerConfigError> {
    if values.iter().any(|value| value == "*") && values != ["*"] {
        Err(RestServerConfigError::Invalid(message))
    } else {
        Ok(())
    }
}

fn validate_header_list(
    headers: &[String],
    field: &'static str,
) -> Result<(), RestServerConfigError> {
    validate_wildcard_list(
        headers,
        "CORS header lists must contain either * or explicit header names",
    )?;
    for header in headers {
        if header != "*"
            && (header.trim() != header || HeaderName::from_bytes(header.as_bytes()).is_err())
        {
            return Err(RestServerConfigError::Invalid(field));
        }
    }
    Ok(())
}

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
        let rate_limit_enabled = config.rate_limit_requests_per_second.is_some();
        let mut rate_limit = RateLimit::new(
            config.rate_limit_requests_per_second.unwrap_or(1),
            config.rate_limit_burst.unwrap_or(1),
        );
        if config.metrics {
            rate_limit = rate_limit.with_metrics(http_metrics.clone());
        }
        let cors_enabled = config.cors.is_some();
        let cors = config
            .cors
            .as_ref()
            .map(RestCorsConfig::middleware)
            .unwrap_or_default();
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
            .wrap(Condition::new(rate_limit_enabled, rate_limit))
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
            .wrap(Condition::new(cors_enabled, cors))
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
    /// Optional process-local token-bucket refill rate. Set together with `rate_limit_burst`.
    pub rate_limit_requests_per_second: Option<u32>,
    /// Optional process-local token-bucket capacity. Set together with the refill rate.
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
            rate_limit_requests_per_second: None,
            rate_limit_burst: None,
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
            tls: None,
            cors: None,
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
        if self.rate_limit_requests_per_second.is_some() != self.rate_limit_burst.is_some() {
            return Err(RestServerConfigError::Invalid(
                "rate_limit_requests_per_second and rate_limit_burst must be configured together",
            ));
        }
        if self.rate_limit_requests_per_second == Some(0) {
            return Err(RestServerConfigError::Invalid(
                "rate_limit_requests_per_second must be greater than zero",
            ));
        }
        if self.rate_limit_burst == Some(0) {
            return Err(RestServerConfigError::Invalid(
                "rate_limit_burst must be greater than zero",
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
        if let Some(tls) = &self.tls {
            tls.validate()?;
        }
        if let Some(cors) = &self.cors {
            cors.validate()?;
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
    Tls(String),
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
            Self::Tls(error) => write!(formatter, "invalid REST TLS configuration: {error}"),
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

        let tls = config
            .tls
            .as_ref()
            .map(RestTlsConfig::rustls_config)
            .transpose()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let server = HttpServer::new(move || {
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
        .shutdown_timeout(shutdown_seconds);
        if let Some(tls) = tls {
            server
                .listen_rustls_0_23(listener, tls)
                .map(HttpServer::run)
        } else {
            server.listen(listener).map(HttpServer::run)
        }
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
rate_limit_requests_per_second = 100
rate_limit_burst = 25
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

[cors]
allowed_origins = ["https://console.example"]
allowed_methods = ["GET", "POST"]
allowed_headers = ["authorization", "content-type"]
exposed_headers = ["x-request-id"]
allow_credentials = true
max_age_seconds = 600
automatic_preflight = true

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
        assert_eq!(config.rate_limit_requests_per_second, Some(100));
        assert_eq!(config.rate_limit_burst, Some(25));
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
        let cors = config.cors.as_ref().unwrap();
        assert_eq!(cors.allowed_origins, ["https://console.example"]);
        assert_eq!(cors.allowed_methods, ["GET", "POST"]);
        assert_eq!(cors.allowed_headers, ["authorization", "content-type"]);
        assert_eq!(cors.exposed_headers, ["x-request-id"]);
        assert!(cors.allow_credentials);
        assert_eq!(cors.max_age_seconds, Some(600));
        assert!(cors.automatic_preflight);
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

        let error = RestServer::new(RestServerConfig {
            rate_limit_requests_per_second: Some(1),
            rate_limit_burst: None,
            ..RestServerConfig::default()
        })
        .err()
        .unwrap();
        assert!(error.to_string().contains("configured together"));

        for cors in [
            RestCorsConfig::new(["https://frontend.example/path"]),
            RestCorsConfig::new(["https://frontend.example"]).with_methods(["NOT A METHOD"]),
            RestCorsConfig::new(["https://frontend.example"]).with_allowed_headers(["bad header"]),
            RestCorsConfig::new(["*", "https://frontend.example"]),
        ] {
            let error = RestServer::new(RestServerConfig {
                cors: Some(cors),
                ..RestServerConfig::default()
            })
            .err()
            .unwrap();
            assert!(error.to_string().contains("CORS"));
        }
    }

    #[actix_rt::test]
    async fn configured_serverless_cors_handles_preflight_and_actual_requests() {
        let cors = RestCorsConfig::new(["https://frontend.example"])
            .with_methods(["POST"])
            .with_allowed_headers(["authorization"])
            .with_exposed_headers(["x-request-id"])
            .with_credentials(true)
            .with_max_age(Some(600));
        let handler = RestServer::new(RestServerConfig {
            cors: Some(cors),
            ..RestServerConfig::default()
        })
        .unwrap()
        .serverless_handler(|routes| {
            routes.route("/cors", web::post().to(HttpResponse::Ok));
        })
        .await
        .unwrap();

        let mut preflight = ServerlessRequest::new(
            Method::OPTIONS,
            Uri::from_static("/cors"),
            web::Bytes::new(),
        );
        preflight.headers.insert(
            actix_web::http::header::ORIGIN,
            HeaderValue::from_static("https://frontend.example"),
        );
        preflight.headers.insert(
            actix_web::http::header::ACCESS_CONTROL_REQUEST_METHOD,
            HeaderValue::from_static("POST"),
        );
        preflight.headers.insert(
            actix_web::http::header::ACCESS_CONTROL_REQUEST_HEADERS,
            HeaderValue::from_static("authorization"),
        );
        let preflight = handler.call(preflight).await.unwrap();

        assert_eq!(preflight.status, StatusCode::OK);
        assert_eq!(
            preflight
                .headers
                .get(actix_web::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "https://frontend.example"
        );
        assert_eq!(
            preflight
                .headers
                .get(actix_web::http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .unwrap(),
            "true"
        );
        assert_eq!(
            preflight
                .headers
                .get(actix_web::http::header::ACCESS_CONTROL_MAX_AGE)
                .unwrap(),
            "600"
        );

        let mut actual =
            ServerlessRequest::new(Method::POST, Uri::from_static("/cors"), web::Bytes::new());
        actual.headers.insert(
            actix_web::http::header::ORIGIN,
            HeaderValue::from_static("https://frontend.example"),
        );
        let actual = handler.call(actual).await.unwrap();
        assert_eq!(actual.status, StatusCode::OK);
        assert_eq!(
            actual
                .headers
                .get(actix_web::http::header::ACCESS_CONTROL_EXPOSE_HEADERS)
                .unwrap(),
            "x-request-id"
        );
    }

    #[actix_rt::test]
    async fn configured_cors_can_delegate_preflight_to_application_routes() {
        let cors = RestCorsConfig::new(["*"]).with_automatic_preflight(false);
        let handler = RestServer::new(RestServerConfig {
            cors: Some(cors),
            ..RestServerConfig::default()
        })
        .unwrap()
        .serverless_handler(|routes| {
            routes.route(
                "/cors",
                web::route()
                    .method(Method::OPTIONS)
                    .to(|| async { HttpResponse::Accepted().body("application preflight") }),
            );
        })
        .await
        .unwrap();
        let mut request = ServerlessRequest::new(
            Method::OPTIONS,
            Uri::from_static("/cors"),
            web::Bytes::new(),
        );
        request.headers.insert(
            actix_web::http::header::ORIGIN,
            HeaderValue::from_static("https://frontend.example"),
        );
        request.headers.insert(
            actix_web::http::header::ACCESS_CONTROL_REQUEST_METHOD,
            HeaderValue::from_static("POST"),
        );

        let response = handler.call(request).await.unwrap();
        assert_eq!(response.status, StatusCode::ACCEPTED);
        assert_eq!(response.body, "application preflight");
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
    async fn configured_standard_stack_enforces_rate_limit() {
        let handler = RestServer::new(RestServerConfig {
            rate_limit_requests_per_second: Some(1),
            rate_limit_burst: Some(1),
            ..RestServerConfig::default()
        })
        .unwrap()
        .serverless_handler(|routes| {
            routes.route("/limited", web::get().to(HttpResponse::Ok));
        })
        .await
        .unwrap();

        let first = handler
            .call(ServerlessRequest::new(
                Method::GET,
                Uri::from_static("/limited"),
                web::Bytes::new(),
            ))
            .await
            .unwrap();
        let second = handler
            .call(ServerlessRequest::new(
                Method::GET,
                Uri::from_static("/limited"),
                web::Bytes::new(),
            ))
            .await
            .unwrap();

        assert_eq!(first.status, StatusCode::OK);
        assert_eq!(second.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(second.headers.get("retry-after").unwrap(), "1");
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
    async fn configured_https_acceptor_completes_mutual_tls_handshake() {
        let (ca, certificate, private_key, client_certificate, client_key) = test_tls_material();
        let server_config = RestTlsConfig::new(certificate, private_key)
            .with_client_ca(ca.clone())
            .rustls_config()
            .unwrap();

        let mut roots = rustls::RootCertStore::empty();
        let mut ca_reader = BufReader::new(Cursor::new(ca.as_bytes()));
        for root in rustls_pemfile::certs(&mut ca_reader) {
            roots.add(root.unwrap()).unwrap();
        }
        let mut certificate_reader = BufReader::new(Cursor::new(client_certificate.as_bytes()));
        let certificates = rustls_pemfile::certs(&mut certificate_reader)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let mut key_reader = BufReader::new(Cursor::new(client_key.as_bytes()));
        let key = rustls_pemfile::private_key(&mut key_reader)
            .unwrap()
            .unwrap();
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_client_auth_cert(certificates, key)
            .unwrap();
        let server_name = rustls::pki_types::ServerName::try_from("localhost")
            .unwrap()
            .to_owned();
        let mut client =
            rustls::ClientConnection::new(Arc::new(client_config), server_name).unwrap();
        let mut server = rustls::ServerConnection::new(Arc::new(server_config)).unwrap();

        for _ in 0..20 {
            let mut client_bytes = Vec::new();
            client.write_tls(&mut client_bytes).unwrap();
            if !client_bytes.is_empty() {
                server.read_tls(&mut Cursor::new(client_bytes)).unwrap();
                server.process_new_packets().unwrap();
            }
            let mut server_bytes = Vec::new();
            server.write_tls(&mut server_bytes).unwrap();
            if !server_bytes.is_empty() {
                client.read_tls(&mut Cursor::new(server_bytes)).unwrap();
                client.process_new_packets().unwrap();
            }
            if !client.is_handshaking() && !server.is_handshaking() {
                break;
            }
        }
        assert!(!client.is_handshaking());
        assert!(!server.is_handshaking());
        assert_eq!(server.peer_certificates().map(<[_]>::len), Some(1));
    }

    fn test_tls_material() -> (String, String, String, String, String) {
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
        let server = server_params.signed_by(&server_key, &ca, &ca_key).unwrap();

        let client_key = KeyPair::generate().unwrap();
        let mut client_params = CertificateParams::new(["rust-zero-client".to_owned()]).unwrap();
        client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let client = client_params.signed_by(&client_key, &ca, &ca_key).unwrap();

        (
            ca.pem(),
            server.pem(),
            server_key.serialize_pem(),
            client.pem(),
            client_key.serialize_pem(),
        )
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
