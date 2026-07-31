use actix_web::{
    body::MessageBody,
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    http::header::{HeaderMap, HeaderName, HeaderValue},
    Error,
};
use futures::future::{ok, LocalBoxFuture, Ready};
use std::task::{Context, Poll};

/// Adds conservative browser security headers without replacing handler-provided values.
#[derive(Clone)]
pub struct SecurityHeaders {
    headers: HeaderMap,
}

impl Default for SecurityHeaders {
    fn default() -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("content-security-policy"),
            HeaderValue::from_static("default-src 'self'; frame-ancestors 'none'"),
        );
        headers.insert(
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        );
        headers.insert(
            HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static("DENY"),
        );
        headers.insert(
            HeaderName::from_static("referrer-policy"),
            HeaderValue::from_static("no-referrer"),
        );
        headers.insert(
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static("camera=(), geolocation=(), microphone=()"),
        );
        Self { headers }
    }
}

impl SecurityHeaders {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }
}

impl<S, B> Transform<S, ServiceRequest> for SecurityHeaders
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = SecurityHeadersMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(SecurityHeadersMiddleware {
            service,
            headers: self.headers.clone(),
        })
    }
}

pub struct SecurityHeadersMiddleware<S> {
    service: S,
    headers: HeaderMap,
}

impl<S, B> Service<ServiceRequest> for SecurityHeadersMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(context)
    }

    fn call(&self, request: ServiceRequest) -> Self::Future {
        let headers = self.headers.clone();
        let future = self.service.call(request);
        Box::pin(async move {
            let mut response = future.await?;
            for (name, value) in &headers {
                if !response.headers().contains_key(name) {
                    response.headers_mut().insert(name.clone(), value.clone());
                }
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, web, App, HttpResponse};

    #[actix_rt::test]
    async fn sets_defaults_but_preserves_handler_headers() {
        let app = test::init_service(App::new().wrap(SecurityHeaders::new()).route(
            "/",
            web::get().to(|| async {
                HttpResponse::Ok()
                    .insert_header(("x-frame-options", "SAMEORIGIN"))
                    .finish()
            }),
        ))
        .await;

        let response =
            test::call_service(&app, test::TestRequest::get().uri("/").to_request()).await;

        assert_eq!(
            response.headers().get("x-frame-options").unwrap(),
            "SAMEORIGIN"
        );
        assert_eq!(
            response.headers().get("x-content-type-options").unwrap(),
            "nosniff"
        );
        assert!(response.headers().contains_key("content-security-policy"));
    }
}
