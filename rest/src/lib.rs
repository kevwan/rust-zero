pub mod auth;
pub mod log;
pub mod metrics;
pub mod middleware;
pub mod recovery;
pub mod resilience;
pub mod security;
pub mod trace;

pub use actix_cors::Cors;
pub use auth::{encode_hs256, BearerAuth, JwtAuth, JwtError};
pub use log::LoggingMiddleware;
pub use metrics::{HttpMetrics, MetricsMiddleware};
pub use middleware::{ConcurrencyLimit, RateLimit, RequestBodyLimit, Timeout};
pub use recovery::Recover;
pub use resilience::{RequestId, RequestIdValue};
pub use security::SecurityHeaders;
pub use trace::TraceContextMiddleware;

#[cfg(test)]
mod tests {
    use super::Cors;
    use actix_web::{
        http::{header, Method, StatusCode},
        test, web, App, HttpResponse,
    };

    #[actix_rt::test]
    async fn permissive_cors_handles_browser_preflight() {
        let app = test::init_service(
            App::new()
                .wrap(Cors::permissive())
                .route("/", web::get().to(|| async { HttpResponse::Ok().finish() })),
        )
        .await;

        let response = test::call_service(
            &app,
            test::TestRequest::default()
                .method(Method::OPTIONS)
                .uri("/")
                .insert_header((header::ORIGIN, "https://frontend.example"))
                .insert_header((header::ACCESS_CONTROL_REQUEST_METHOD, "GET"))
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "https://frontend.example"
        );
    }
}
