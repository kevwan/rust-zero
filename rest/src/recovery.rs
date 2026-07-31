use actix_web::{
    body::MessageBody,
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    Error,
};
use futures::{
    future::{ok, LocalBoxFuture, Ready},
    FutureExt,
};
use std::{
    panic::AssertUnwindSafe,
    task::{Context, Poll},
};

/// Converts panics from downstream handlers and middleware into HTTP 500 responses.
#[derive(Clone)]
pub struct Recover {
    response_body: String,
}

impl Default for Recover {
    fn default() -> Self {
        Self {
            response_body: "internal server error".to_owned(),
        }
    }
}

impl Recover {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_response_body(mut self, body: impl Into<String>) -> Self {
        self.response_body = body.into();
        self
    }
}

impl<S, B> Transform<S, ServiceRequest> for Recover
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = RecoverMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(RecoverMiddleware {
            service,
            response_body: self.response_body.clone(),
        })
    }
}

pub struct RecoverMiddleware<S> {
    service: S,
    response_body: String,
}

impl<S, B> Service<ServiceRequest> for RecoverMiddleware<S>
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
        let response_body = self.response_body.clone();
        let future = self.service.call(request);

        Box::pin(async move {
            match AssertUnwindSafe(future).catch_unwind().await {
                Ok(response) => response,
                Err(_) => {
                    tracing::error!("recovered panic while handling HTTP request");
                    Err(actix_web::error::ErrorInternalServerError(response_body))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{http::StatusCode, test, web, App};

    #[actix_rt::test]
    async fn converts_handler_panics_to_internal_server_errors() {
        let app = test::init_service(App::new().wrap(Recover::new()).route(
            "/",
            web::get().to(|| async {
                panic!("boom");
                #[allow(unreachable_code)]
                "never"
            }),
        ))
        .await;

        let error = test::try_call_service(&app, test::TestRequest::get().uri("/").to_request())
            .await
            .unwrap_err();

        assert_eq!(
            actix_web::error::ResponseError::status_code(error.as_response_error()),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
