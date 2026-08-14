use actix_web::{
    dev::Server,
    http::header,
    web::{self, ServiceConfig},
    App, HttpRequest, HttpResponse, HttpServer,
};
use rust_zero_core::{HealthRegistry, Metrics, Profiler};
use serde::{Deserialize, Serialize};
#[cfg(all(feature = "sampling-profiler", unix))]
use std::time::Duration;
use std::{
    io,
    net::IpAddr,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// Configuration for the framework's internal observability server.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DevServerConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub health_path: String,
    pub metrics_path: String,
    pub profile_path: String,
    pub flamegraph_path: String,
    pub runtime_path: String,
    pub tasks_path: String,
    pub allocator_path: String,
    pub health_response: String,
    pub enable_metrics: bool,
    pub enable_profiling: bool,
    pub enable_sampling_profiler: bool,
    pub sampling_seconds: u64,
    pub sampling_frequency: i32,
    /// When set, every diagnostic endpoint requires this bearer token.
    #[serde(skip_serializing)]
    pub auth_token: Option<String>,
    /// Reject non-private listener addresses when starting the server.
    pub private_only: bool,
}

impl std::fmt::Debug for DevServerConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DevServerConfig")
            .field("enabled", &self.enabled)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("health_path", &self.health_path)
            .field("metrics_path", &self.metrics_path)
            .field("profile_path", &self.profile_path)
            .field("flamegraph_path", &self.flamegraph_path)
            .field("runtime_path", &self.runtime_path)
            .field("tasks_path", &self.tasks_path)
            .field("allocator_path", &self.allocator_path)
            .field("health_response", &self.health_response)
            .field("enable_metrics", &self.enable_metrics)
            .field("enable_profiling", &self.enable_profiling)
            .field("enable_sampling_profiler", &self.enable_sampling_profiler)
            .field("sampling_seconds", &self.sampling_seconds)
            .field("sampling_frequency", &self.sampling_frequency)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("private_only", &self.private_only)
            .finish()
    }
}

impl Default for DevServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: String::new(),
            port: 6060,
            health_path: "/healthz".to_owned(),
            metrics_path: "/metrics".to_owned(),
            profile_path: "/debug/profile".to_owned(),
            flamegraph_path: "/debug/flamegraph".to_owned(),
            runtime_path: "/debug/runtime".to_owned(),
            tasks_path: "/debug/tasks".to_owned(),
            allocator_path: "/debug/allocator".to_owned(),
            health_response: "OK".to_owned(),
            enable_metrics: true,
            enable_profiling: true,
            enable_sampling_profiler: false,
            sampling_seconds: 10,
            sampling_frequency: 99,
            auth_token: None,
            private_only: false,
        }
    }
}

impl DevServerConfig {
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.sampling_seconds == 0 || self.sampling_seconds > 300 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sampling_seconds must be between 1 and 300",
            ));
        }
        if !(1..=1_000).contains(&self.sampling_frequency) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sampling_frequency must be between 1 and 1000",
            ));
        }
        if self.auth_token.as_ref().is_some_and(String::is_empty) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "auth_token cannot be empty",
            ));
        }
        if self.private_only {
            let host = self.host.parse::<IpAddr>().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "private_only requires a literal private IP address",
                )
            })?;
            if !is_private_address(host) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "private_only rejects public or unspecified listener addresses",
                ));
            }
        }
        Ok(())
    }
}

fn is_private_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            address.is_loopback() || address.is_private() || address.is_link_local()
        }
        IpAddr::V6(address) => {
            address.is_loopback()
                || address.is_unique_local()
                || (address.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Health, metrics, profiling, and runtime-diagnostic endpoints for a service.
#[derive(Clone)]
pub struct DevServer {
    config: DevServerConfig,
    metrics: Arc<Metrics>,
    profiler: Arc<Profiler>,
    health: HealthRegistry,
    started_at: Instant,
}

impl DevServer {
    pub fn new(config: DevServerConfig, metrics: Arc<Metrics>, profiler: Arc<Profiler>) -> Self {
        if config.enable_profiling {
            profiler.enable();
        }
        Self {
            config,
            metrics,
            profiler,
            health: HealthRegistry::new(),
            started_at: Instant::now(),
        }
    }

    pub fn with_health_registry(mut self, health: HealthRegistry) -> Self {
        self.health = health;
        self
    }

    pub fn health_registry(&self) -> HealthRegistry {
        self.health.clone()
    }

    pub fn config(&self) -> &DevServerConfig {
        &self.config
    }

    pub fn routes(&self) -> Vec<String> {
        let mut routes = vec!["/".to_owned(), self.config.health_path.clone()];
        if self.config.enable_metrics {
            routes.push(self.config.metrics_path.clone());
        }
        if self.config.enable_profiling {
            routes.push(self.config.profile_path.clone());
        }
        routes.push(self.config.runtime_path.clone());
        routes.push(self.config.tasks_path.clone());
        routes.push(self.config.allocator_path.clone());
        if self.config.enable_sampling_profiler {
            routes.push(self.config.flamegraph_path.clone());
        }
        routes
    }

    /// Registers the diagnostic routes in an existing Actix application.
    pub fn configure(&self, services: &mut ServiceConfig) {
        let routes = self.routes();
        services
            .app_data(web::Data::new(self.clone()))
            .route(
                "/",
                web::get().to(move |request: HttpRequest, server: web::Data<Self>| {
                    let routes = routes.clone();
                    async move {
                        if let Err(response) = server.authorize(&request) {
                            return response;
                        }
                        HttpResponse::Ok().json(routes)
                    }
                }),
            )
            .route(
                &self.config.health_path,
                web::get().to(|request: HttpRequest, server: web::Data<Self>| async move {
                    if let Err(response) = server.authorize(&request) {
                        return response;
                    }
                    let health = server.health.snapshot();
                    let mut response = if health.is_ready() {
                        HttpResponse::Ok()
                    } else {
                        HttpResponse::ServiceUnavailable()
                    };
                    let body = if health.is_ready() {
                        server.config.health_response.clone()
                    } else {
                        format!("NOT READY: {}", health.unhealthy().join(","))
                    };
                    response
                        .insert_header((header::CONTENT_TYPE, "text/plain; charset=utf-8"))
                        .body(body)
                }),
            )
            .route(
                &self.config.runtime_path,
                web::get().to(|request: HttpRequest, server: web::Data<Self>| async move {
                    if let Err(response) = server.authorize(&request) {
                        return response;
                    }
                    HttpResponse::Ok().json(RuntimeStats::capture(server.started_at))
                }),
            )
            .route(
                &self.config.tasks_path,
                web::get().to(|request: HttpRequest, server: web::Data<Self>| async move {
                    if let Err(response) = server.authorize(&request) {
                        return response;
                    }
                    HttpResponse::Ok().json(TaskStats::capture())
                }),
            )
            .route(
                &self.config.allocator_path,
                web::get().to(|request: HttpRequest, server: web::Data<Self>| async move {
                    if let Err(response) = server.authorize(&request) {
                        return response;
                    }
                    HttpResponse::Ok().json(AllocatorStats::capture())
                }),
            );

        if self.config.enable_metrics {
            services.route(
                &self.config.metrics_path,
                web::get().to(|request: HttpRequest, server: web::Data<Self>| async move {
                    if let Err(response) = server.authorize(&request) {
                        return response;
                    }
                    HttpResponse::Ok()
                        .insert_header((
                            header::CONTENT_TYPE,
                            "text/plain; version=0.0.4; charset=utf-8",
                        ))
                        .body(server.metrics.render())
                }),
            );
        }

        if self.config.enable_profiling {
            services.route(
                &self.config.profile_path,
                web::get().to(|request: HttpRequest, server: web::Data<Self>| async move {
                    if let Err(response) = server.authorize(&request) {
                        return response;
                    }
                    HttpResponse::Ok()
                        .insert_header((header::CONTENT_TYPE, "text/plain; charset=utf-8"))
                        .body(server.profiler.render_report())
                }),
            );
        }

        if self.config.enable_sampling_profiler {
            services.route(
                &self.config.flamegraph_path,
                web::get().to(|request: HttpRequest, server: web::Data<Self>| async move {
                    if let Err(response) = server.authorize(&request) {
                        return response;
                    }
                    server.flamegraph().await
                }),
            );
        }
    }

    /// Builds the internal HTTP server. The returned future starts when it is awaited or spawned.
    pub fn run(self) -> io::Result<Server> {
        self.config.validate()?;
        let address = self.config.address();
        HttpServer::new(move || {
            let server = self.clone();
            App::new().configure(move |services| server.configure(services))
        })
        .bind(address)
        .map(HttpServer::run)
    }

    fn authorize(&self, request: &HttpRequest) -> Result<(), HttpResponse> {
        let Some(expected) = &self.config.auth_token else {
            return Ok(());
        };
        let provided = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if provided
            .is_some_and(|provided| constant_time_eq(provided.as_bytes(), expected.as_bytes()))
        {
            Ok(())
        } else {
            Err(HttpResponse::Unauthorized()
                .insert_header((header::WWW_AUTHENTICATE, "Bearer"))
                .finish())
        }
    }

    async fn flamegraph(&self) -> HttpResponse {
        #[cfg(all(feature = "sampling-profiler", unix))]
        {
            let seconds = self.config.sampling_seconds;
            let frequency = self.config.sampling_frequency;
            match tokio::task::spawn_blocking(move || render_flamegraph(seconds, frequency)).await {
                Ok(Ok(svg)) => HttpResponse::Ok()
                    .insert_header((header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"))
                    .body(svg),
                Ok(Err(error)) => HttpResponse::InternalServerError().body(error),
                Err(error) => HttpResponse::InternalServerError()
                    .body(format!("sampling profiler task failed: {error}")),
            }
        }
        #[cfg(not(all(feature = "sampling-profiler", unix)))]
        HttpResponse::NotImplemented()
            .body("sampling profiling requires the rest/sampling-profiler feature on a Unix target")
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

#[cfg(all(feature = "sampling-profiler", unix))]
fn render_flamegraph(seconds: u64, frequency: i32) -> Result<Vec<u8>, String> {
    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(frequency)
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()
        .map_err(|error| error.to_string())?;
    std::thread::sleep(Duration::from_secs(seconds));
    let report = guard.report().build().map_err(|error| error.to_string())?;
    let mut svg = Vec::new();
    report
        .flamegraph(&mut svg)
        .map_err(|error| error.to_string())?;
    ensure_flamegraph_svg(&mut svg);
    Ok(svg)
}

#[cfg(all(feature = "sampling-profiler", unix))]
fn ensure_flamegraph_svg(svg: &mut Vec<u8>) {
    if svg.is_empty() {
        // pprof can legitimately collect no samples in a short window on idle or restricted CI
        // hosts. Keep the endpoint's image/svg+xml contract while making that state explicit.
        svg.extend_from_slice(
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="640" height="80" viewBox="0 0 640 80"><rect width="100%" height="100%" fill="#fafafa"/><text x="20" y="46" font-family="sans-serif" font-size="16">No samples captured during this profiling window</text></svg>"##,
        );
    }
}

#[derive(Debug, Serialize)]
struct RuntimeStats {
    process_id: u32,
    available_parallelism: usize,
    uptime_seconds: f64,
    unix_time_seconds: u64,
}

#[derive(Debug, Serialize)]
struct TaskStats {
    runtime_available: bool,
    worker_threads: usize,
    alive_tasks: usize,
    global_queue_depth: usize,
}

impl TaskStats {
    fn capture() -> Self {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let metrics = handle.metrics();
                Self {
                    runtime_available: true,
                    worker_threads: metrics.num_workers(),
                    alive_tasks: metrics.num_alive_tasks(),
                    global_queue_depth: metrics.global_queue_depth(),
                }
            }
            Err(_) => Self {
                runtime_available: false,
                worker_threads: 0,
                alive_tasks: 0,
                global_queue_depth: 0,
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct AllocatorStats {
    allocator: &'static str,
    resident_set_high_water_bytes: Option<u64>,
}

impl AllocatorStats {
    fn capture() -> Self {
        Self {
            allocator: "system",
            resident_set_high_water_bytes: resident_set_high_water_bytes(),
        }
    }
}

#[cfg(unix)]
fn resident_set_high_water_bytes() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` points to writable storage for the `rusage` result.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: successful `getrusage` initialized the structure.
    let bytes = unsafe { usage.assume_init() }.ru_maxrss as u64;
    #[cfg(target_os = "macos")]
    return Some(bytes);
    #[cfg(not(target_os = "macos"))]
    Some(bytes.saturating_mul(1024))
}

#[cfg(not(unix))]
fn resident_set_high_water_bytes() -> Option<u64> {
    None
}

impl RuntimeStats {
    fn capture(started_at: Instant) -> Self {
        Self {
            process_id: std::process::id(),
            available_parallelism: std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1),
            uptime_seconds: started_at.elapsed().as_secs_f64(),
            unix_time_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{body::to_bytes, http::StatusCode, test};
    use rust_zero_core::VectorOptions;
    use std::time::Duration;

    fn server(config: DevServerConfig) -> DevServer {
        let metrics = Arc::new(Metrics::new());
        metrics
            .counter_vec(VectorOptions::new("requests_total", "requests"))
            .unwrap()
            .inc(&[])
            .unwrap();
        let profiler = Arc::new(Profiler::new());
        let server = DevServer::new(config, metrics, profiler.clone());
        profiler.record("database", Duration::from_millis(2));
        server
    }

    #[actix_rt::test]
    async fn configuration_deserialization_preserves_production_defaults() {
        let config: DevServerConfig =
            serde_json::from_str(r#"{"port":7070,"enable_profiling":false}"#).unwrap();

        assert_eq!(config.port, 7070);
        assert_eq!(config.health_path, "/healthz");
        assert!(config.enable_metrics);
        assert!(!config.enable_profiling);
    }

    #[actix_rt::test]
    async fn serves_health_metrics_profile_and_runtime_diagnostics() {
        let server = server(DevServerConfig::default());
        let app =
            test::init_service(App::new().configure(move |config| server.configure(config))).await;

        for path in [
            "/healthz",
            "/metrics",
            "/debug/profile",
            "/debug/runtime",
            "/debug/tasks",
            "/debug/allocator",
        ] {
            let response =
                test::call_service(&app, test::TestRequest::get().uri(path).to_request()).await;
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }

        let response =
            test::call_service(&app, test::TestRequest::get().uri("/metrics").to_request()).await;
        let body = to_bytes(response.into_body()).await.unwrap();
        assert!(std::str::from_utf8(&body)
            .unwrap()
            .contains("requests_total 1"));
    }

    #[actix_rt::test]
    async fn disabled_optional_routes_are_not_registered() {
        let server = server(DevServerConfig {
            enable_metrics: false,
            enable_profiling: false,
            ..DevServerConfig::default()
        });
        let routes = server.routes();
        let app =
            test::init_service(App::new().configure(move |config| server.configure(config))).await;

        assert!(!routes.contains(&"/metrics".to_owned()));
        for path in ["/metrics", "/debug/profile"] {
            let response =
                test::call_service(&app, test::TestRequest::get().uri(path).to_request()).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[actix_rt::test]
    async fn aggregates_dependency_health_without_handler_polling() {
        let health = HealthRegistry::new();
        health.set("users-rpc", false);
        let server = server(DevServerConfig::default()).with_health_registry(health.clone());
        let app =
            test::init_service(App::new().configure(move |config| server.configure(config))).await;

        let response =
            test::call_service(&app, test::TestRequest::get().uri("/healthz").to_request()).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body()).await.unwrap();
        assert_eq!(&body[..], b"NOT READY: users-rpc");

        health.set("users-rpc", true);
        let response =
            test::call_service(&app, test::TestRequest::get().uri("/healthz").to_request()).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[actix_rt::test]
    async fn bearer_authentication_protects_every_diagnostic_route() {
        let server = server(DevServerConfig {
            auth_token: Some("diagnostics-secret".to_owned()),
            ..DevServerConfig::default()
        });
        let app =
            test::init_service(App::new().configure(move |config| server.configure(config))).await;

        for path in ["/", "/healthz", "/metrics", "/debug/tasks"] {
            let response =
                test::call_service(&app, test::TestRequest::get().uri(path).to_request()).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");

            let response = test::call_service(
                &app,
                test::TestRequest::get()
                    .uri(path)
                    .insert_header((header::AUTHORIZATION, "Bearer diagnostics-secret"))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
    }

    #[actix_rt::test]
    async fn validates_private_binding_and_sampling_bounds() {
        let private = DevServerConfig {
            host: "10.2.3.4".to_owned(),
            private_only: true,
            ..DevServerConfig::default()
        };
        assert!(private.validate().is_ok());

        let public = DevServerConfig {
            host: "8.8.8.8".to_owned(),
            private_only: true,
            ..DevServerConfig::default()
        };
        assert!(public.validate().is_err());

        let wildcard = DevServerConfig {
            host: String::new(),
            private_only: true,
            ..DevServerConfig::default()
        };
        assert!(wildcard.validate().is_err());

        let invalid_sampling = DevServerConfig {
            sampling_seconds: 0,
            ..DevServerConfig::default()
        };
        assert!(invalid_sampling.validate().is_err());
    }

    #[actix_rt::test]
    async fn exposes_bounded_task_and_allocator_diagnostics() {
        let server = server(DevServerConfig::default());
        let app =
            test::init_service(App::new().configure(move |config| server.configure(config))).await;

        let tasks: serde_json::Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::get().uri("/debug/tasks").to_request(),
        )
        .await;
        assert_eq!(tasks["runtime_available"], true);
        assert!(tasks["worker_threads"].as_u64().unwrap() >= 1);

        let allocator: serde_json::Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::get()
                .uri("/debug/allocator")
                .to_request(),
        )
        .await;
        assert_eq!(allocator["allocator"], "system");
        assert!(allocator["resident_set_high_water_bytes"].is_number());
    }

    #[cfg(all(feature = "sampling-profiler", unix))]
    #[actix_rt::test]
    async fn sampling_profiler_produces_an_svg_flamegraph() {
        let load = tokio::task::spawn_blocking(|| {
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(1) {
                std::hint::black_box(started.elapsed());
            }
        });
        let profile = tokio::task::spawn_blocking(|| render_flamegraph(1, 99));
        let (load, svg) = tokio::join!(load, profile);
        load.unwrap();
        let svg = svg.unwrap().unwrap();
        let svg = std::str::from_utf8(&svg).unwrap();
        assert!(
            svg.contains("<svg"),
            "unexpected flamegraph output: {svg:?}"
        );
        assert!(svg.contains("</svg>"));
    }

    #[cfg(all(feature = "sampling-profiler", unix))]
    #[actix_rt::test]
    async fn empty_sampling_window_has_a_valid_svg_fallback() {
        let mut svg = Vec::new();
        ensure_flamegraph_svg(&mut svg);
        let svg = std::str::from_utf8(&svg).unwrap();
        assert!(svg.contains("<svg"));
        assert!(svg.contains("No samples captured"));
        assert!(svg.contains("</svg>"));
    }
}
