use crate::{
    route::RoutePolicies, ConcurrencyLimit, HttpMetrics, LoggingMiddleware, MetricsMiddleware,
    MultipartConfig, Recover, RequestBodyLimit, RequestId, ResponsePolicy, RouteGroupConfig,
    SecurityHeaders, Timeout, TraceContextMiddleware,
};
use actix_web::{
    middleware::Condition,
    web::{self, ServiceConfig},
    App, HttpServer,
};
use rust_zero_core::Metrics;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    future::Future,
    io,
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

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
}

impl fmt::Display for RestServerConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
            Self::Metrics(error) => write!(formatter, "failed to configure REST metrics: {error}"),
            Self::RoutePolicy(error) => write!(formatter, "invalid REST route policy: {error}"),
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
        Ok(Self {
            config,
            metrics,
            http_metrics,
            route_policies,
            response_policy: ResponsePolicy::new(),
        })
    }

    /// Installs the response policy as Actix application data for all registered handlers.
    pub fn with_response_policy(mut self, response_policy: ResponsePolicy) -> Self {
        self.response_policy = response_policy;
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
        let route_policies = self.route_policies.clone();
        let response_policy = self.response_policy.clone();
        let shutdown_seconds = config.shutdown_timeout_ms.div_ceil(1_000);

        HttpServer::new(move || {
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
            App::new()
                .app_data(web::JsonConfig::default().limit(config.max_body_bytes))
                .app_data(web::FormConfig::default().limit(config.max_body_bytes))
                .app_data(web::Data::new(multipart_config))
                .app_data(web::Data::new(response_policy.clone()))
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
                    MetricsMiddleware::new(http_metrics.clone()),
                ))
                .wrap(timeout)
                .wrap(concurrency)
                .wrap(
                    RequestBodyLimit::new(config.max_body_bytes)
                        .decompress_gzip(config.decompress_gzip),
                )
                .wrap(route_policies.clone())
                .configure(configure.clone())
                .default_service(web::to(actix_web::HttpResponse::NotFound))
        })
        .workers(config.workers)
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
    use actix_web::{http::StatusCode, test as actix_test, web, HttpResponse};
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
max_body_bytes = 16777216
max_multipart_field_bytes = 32768
max_multipart_file_bytes = 8388608
max_multipart_total_bytes = 12582912
multipart_temp_dir = "/tmp/rust-zero-uploads"

[[route_groups]]
prefix = "/api"
timeout_ms = 100

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
        assert_eq!(config.max_multipart_field_bytes, 32 * 1024);
        assert_eq!(config.max_multipart_file_bytes, 8 * 1024 * 1024);
        assert_eq!(config.max_multipart_total_bytes, 12 * 1024 * 1024);
        assert_eq!(
            config.multipart_temp_dir.as_deref(),
            Some(Path::new("/tmp/rust-zero-uploads"))
        );
        assert_eq!(config.route_groups[0].prefix, "/api");
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
