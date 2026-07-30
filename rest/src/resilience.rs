use actix_web::{
    body::MessageBody,
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    http::header::{HeaderName, HeaderValue},
    Error, HttpMessage, HttpRequest,
};
use futures::future::{ok, LocalBoxFuture, Ready};
use std::{
    sync::atomic::{AtomicU64, Ordering},
    task::{Context, Poll},
};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Propagates a client request ID or assigns one when it is absent.
#[derive(Clone)]
pub struct RequestId {
    header: HeaderName,
}

impl Default for RequestId {
    fn default() -> Self {
        Self {
            header: HeaderName::from_static("x-request-id"),
        }
    }
}

impl RequestId {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_header(header: HeaderName) -> Self {
        Self { header }
    }

    /// Retrieves the request ID assigned by this middleware.
    pub fn request_id(request: &HttpRequest) -> Option<String> {
        request
            .extensions()
            .get::<RequestIdValue>()
            .map(|value| value.0.clone())
    }
}

/// The request ID attached to an Actix request extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestIdValue(String);

impl RequestIdValue {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<S, B> Transform<S, ServiceRequest> for RequestId
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = RequestIdMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(RequestIdMiddleware {
            service,
            header: self.header.clone(),
        })
    }
}

pub struct RequestIdMiddleware<S> {
    service: S,
    header: HeaderName,
}

impl<S, B> Service<ServiceRequest> for RequestIdMiddleware<S>
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
        let request_id = request
            .headers()
            .get(&self.header)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("{:016x}", NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)));
        request
            .extensions_mut()
            .insert(RequestIdValue(request_id.clone()));
        let header = self.header.clone();
        let future = self.service.call(request);

        Box::pin(async move {
            let mut response = future.await?;
            response.headers_mut().insert(
                header,
                HeaderValue::from_str(&request_id)
                    .expect("request IDs are sourced from validated HTTP headers"),
            );
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, web, App, HttpResponse};

    #[actix_rt::test]
    async fn preserves_the_client_request_id() {
        let app = test::init_service(App::new().wrap(RequestId::new()).route(
            "/",
            web::get().to(|request: HttpRequest| async move {
                HttpResponse::Ok().body(RequestId::request_id(&request).unwrap())
            }),
        ))
        .await;

        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/")
                .insert_header(("x-request-id", "from-client"))
                .to_request(),
        )
        .await;

        assert_eq!(
            response.headers().get("x-request-id").unwrap(),
            "from-client"
        );
    }
}
