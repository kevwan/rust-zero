use actix_web::{
    dev::Server,
    http::header,
    web::{self, ServiceConfig},
    App, HttpResponse, HttpServer,
};
use rust_zero_core::{Metrics, Profiler};
use serde::{Deserialize, Serialize};
use std::{
    io,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// Configuration for the framework's internal observability server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DevServerConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub health_path: String,
    pub metrics_path: String,
    pub profile_path: String,
    pub runtime_path: String,
    pub health_response: String,
    pub enable_metrics: bool,
    pub enable_profiling: bool,
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
            runtime_path: "/debug/runtime".to_owned(),
            health_response: "OK".to_owned(),
            enable_metrics: true,
            enable_profiling: true,
        }
    }
}

impl DevServerConfig {
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Health, metrics, profiling, and runtime-diagnostic endpoints for a service.
#[derive(Clone)]
pub struct DevServer {
    config: DevServerConfig,
    metrics: Arc<Metrics>,
    profiler: Arc<Profiler>,
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
            started_at: Instant::now(),
        }
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
        routes
    }

    /// Registers the diagnostic routes in an existing Actix application.
    pub fn configure(&self, services: &mut ServiceConfig) {
        let routes = self.routes();
        services
            .app_data(web::Data::new(self.clone()))
            .route(
                "/",
                web::get().to(move || {
                    let routes = routes.clone();
                    async move { web::Json(routes) }
                }),
            )
            .route(
                &self.config.health_path,
                web::get().to(|server: web::Data<Self>| async move {
                    HttpResponse::Ok()
                        .insert_header((header::CONTENT_TYPE, "text/plain; charset=utf-8"))
                        .body(server.config.health_response.clone())
                }),
            )
            .route(
                &self.config.runtime_path,
                web::get().to(|server: web::Data<Self>| async move {
                    web::Json(RuntimeStats::capture(server.started_at))
                }),
            );

        if self.config.enable_metrics {
            services.route(
                &self.config.metrics_path,
                web::get().to(|server: web::Data<Self>| async move {
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
                web::get().to(|server: web::Data<Self>| async move {
                    HttpResponse::Ok()
                        .insert_header((header::CONTENT_TYPE, "text/plain; charset=utf-8"))
                        .body(server.profiler.render_report())
                }),
            );
        }
    }

    /// Builds the internal HTTP server. The returned future starts when it is awaited or spawned.
    pub fn run(self) -> io::Result<Server> {
        let address = self.config.address();
        HttpServer::new(move || {
            let server = self.clone();
            App::new().configure(move |services| server.configure(services))
        })
        .bind(address)
        .map(HttpServer::run)
    }
}

#[derive(Debug, Serialize)]
struct RuntimeStats {
    process_id: u32,
    available_parallelism: usize,
    uptime_seconds: f64,
    unix_time_seconds: u64,
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

        for path in ["/healthz", "/metrics", "/debug/profile", "/debug/runtime"] {
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
}
