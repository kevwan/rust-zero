pub mod auth;
pub mod client;
pub mod content_encryption;
pub mod devserver;
pub mod extract;
pub mod log;
pub mod metrics;
pub mod middleware;
pub mod recovery;
pub mod resilience;
pub mod response;
pub mod route;
pub mod security;
pub mod server;
pub mod sse;
pub mod static_assets;
pub mod trace;

pub use actix_cors::Cors;
pub use auth::{encode_hs256, BearerAuth, JwtAuth, JwtError, RequestSignatureAuth};
pub use client::{HttpClient, HttpClientConfig, HttpClientError};
pub use content_encryption::{
    ContentEncryption, ContentEncryptionError, ContentEncryptionKey, ContentKeyProvider,
    StaticContentKeyProvider, CONTENT_ENCRYPTION_HEADER, CONTENT_KEY_ID_HEADER,
};
pub use devserver::{DevServer, DevServerConfig};
pub use extract::{
    FromRequestHeaders, MultipartConfig, MultipartForm, RequestExtractionError, UploadedFile,
    ValidatedForm, ValidatedHeader, ValidatedJson, ValidatedPath, ValidatedQuery, ValidatedRequest,
};
pub use log::{LoggingMiddleware, StructuredLogging};
pub use metrics::{HttpClientMetrics, HttpMetrics, MetricsMiddleware};
pub use middleware::{AdaptiveLoadShed, ConcurrencyLimit, RateLimit, RequestBodyLimit, Timeout};
pub use recovery::Recover;
pub use resilience::{RequestId, RequestIdValue};
pub use response::{grpc_status_to_http, streaming_response, ApiError, ResponsePolicy};
pub use route::{
    RouteGroupConfig, RouteJwtConfig, RouteMiddleware, RouteMiddlewareFuture, RouteMiddlewareNext,
    RoutePolicyConfig,
};
pub use rust_zero_core::{
    sign_request, AuthFailure, JwtClaimProjection, RequestSignature, RequestSignatureVerifier,
    AUTH_KEY_ID_HEADER, AUTH_SIGNATURE_HEADER, AUTH_TIMESTAMP_HEADER,
};
pub use security::SecurityHeaders;
pub use server::{
    RestServer, RestServerConfig, RestServerConfigError, ServerlessHandler, ServerlessRequest,
    ServerlessResponse,
};
pub use sse::{sse_response, SseEvent};
pub use static_assets::{EmbeddedAsset, StaticAssets, StaticAssetsError};
#[cfg(feature = "telemetry")]
pub use trace::OpenTelemetryTracing;
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
