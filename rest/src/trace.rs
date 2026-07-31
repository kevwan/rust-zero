use actix_web::{
    body::MessageBody,
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    http::header::{HeaderName, HeaderValue},
    Error, HttpMessage, HttpRequest,
};
use futures::future::{ok, LocalBoxFuture, Ready};
use rust_zero_core::{TraceContext, TraceFlags};
use std::task::{Context, Poll};

const TRACEPARENT: HeaderName = HeaderName::from_static("traceparent");

/// Propagates W3C trace context and creates a server span for every request.
#[derive(Debug, Clone, Copy, Default)]
pub struct TraceContextMiddleware;

impl TraceContextMiddleware {
    pub fn new() -> Self {
        Self
    }

    pub fn context(request: &HttpRequest) -> Option<TraceContext> {
        request.extensions().get::<TraceContext>().cloned()
    }
}

impl<S, B> Transform<S, ServiceRequest> for TraceContextMiddleware
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = TraceContextService<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(TraceContextService { service })
    }
}

pub struct TraceContextService<S> {
    service: S,
}

impl<S, B> Service<ServiceRequest> for TraceContextService<S>
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
        let context = request
            .headers()
            .get(&TRACEPARENT)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| TraceContext::parse(value).ok())
            .map(|parent| parent.child())
            .unwrap_or_else(|| TraceContext::root(TraceFlags::SAMPLED));
        let traceparent = context.traceparent();
        request.extensions_mut().insert(context);
        let future = self.service.call(request);

        Box::pin(async move {
            let mut response = future.await?;
            response.headers_mut().insert(
                TRACEPARENT,
                HeaderValue::from_str(&traceparent)
                    .expect("generated traceparent values are valid HTTP headers"),
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
    async fn preserves_trace_id_and_creates_a_server_span() {
        let inbound = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let app = test::init_service(App::new().wrap(TraceContextMiddleware::new()).route(
            "/",
            web::get().to(|request: HttpRequest| async move {
                HttpResponse::Ok().body(
                    TraceContextMiddleware::context(&request)
                        .unwrap()
                        .parent_span_id()
                        .unwrap(),
                )
            }),
        ))
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/")
                .insert_header(("traceparent", inbound))
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), actix_web::http::StatusCode::OK);
        assert!(response
            .headers()
            .get("traceparent")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("4bf92f3577b34da6a3ce929d0e0e4736"));
        assert_eq!(test::read_body(response).await, "00f067aa0ba902b7");
    }
}
