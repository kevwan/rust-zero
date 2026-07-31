use actix_web::{
    body::{EitherBody, MessageBody},
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    http::{header, StatusCode},
    Error, HttpMessage, HttpRequest, HttpResponse,
};
use futures::future::{ok, LocalBoxFuture, Ready};
use std::{
    sync::Arc,
    task::{Context, Poll},
};

type Validator<T> = dyn Fn(&str) -> Option<T> + Send + Sync;

/// Validates bearer credentials and stores the resulting identity in request extensions.
pub struct BearerAuth<T> {
    validator: Arc<Validator<T>>,
    challenge: header::HeaderValue,
}

impl<T> Clone for BearerAuth<T> {
    fn clone(&self) -> Self {
        Self {
            validator: Arc::clone(&self.validator),
            challenge: self.challenge.clone(),
        }
    }
}

impl<T> BearerAuth<T>
where
    T: Clone + 'static,
{
    pub fn new(validator: impl Fn(&str) -> Option<T> + Send + Sync + 'static) -> Self {
        Self {
            validator: Arc::new(validator),
            challenge: header::HeaderValue::from_static("Bearer"),
        }
    }

    pub fn with_realm(mut self, realm: &str) -> Result<Self, header::InvalidHeaderValue> {
        self.challenge = header::HeaderValue::from_str(&format!("Bearer realm=\"{realm}\""))?;
        Ok(self)
    }

    /// Returns the authenticated identity installed by this middleware.
    pub fn authenticated(request: &HttpRequest) -> Option<T> {
        request
            .extensions()
            .get::<Authenticated<T>>()
            .map(|identity| identity.0.clone())
    }
}

#[derive(Debug, Clone)]
struct Authenticated<T>(T);

impl<S, B, T> Transform<S, ServiceRequest> for BearerAuth<T>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
    T: Clone + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Transform = BearerAuthMiddleware<S, T>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(BearerAuthMiddleware {
            service,
            validator: Arc::clone(&self.validator),
            challenge: self.challenge.clone(),
        })
    }
}

pub struct BearerAuthMiddleware<S, T> {
    service: S,
    validator: Arc<Validator<T>>,
    challenge: header::HeaderValue,
}

impl<S, B, T> Service<ServiceRequest> for BearerAuthMiddleware<S, T>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
    T: Clone + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(context)
    }

    fn call(&self, request: ServiceRequest) -> Self::Future {
        let identity = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(bearer_token)
            .and_then(|token| (self.validator)(token));

        let Some(identity) = identity else {
            let challenge = self.challenge.clone();
            return Box::pin(async move {
                Ok(request.into_response(
                    HttpResponse::build(StatusCode::UNAUTHORIZED)
                        .insert_header((header::WWW_AUTHENTICATE, challenge))
                        .body("authentication required")
                        .map_into_right_body(),
                ))
            });
        };

        request.extensions_mut().insert(Authenticated(identity));
        let future = self.service.call(request);
        Box::pin(async move { Ok(future.await?.map_into_left_body()) })
    }
}

fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(char::is_whitespace)?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer")
        && !token.is_empty()
        && !token.contains(char::is_whitespace))
    .then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, web, App};

    #[actix_rt::test]
    async fn authenticates_and_exposes_the_identity() {
        let app = test::init_service(
            App::new()
                .wrap(BearerAuth::new(|token| {
                    (token == "valid").then(|| "user-42".to_owned())
                }))
                .route(
                    "/",
                    web::get().to(|request: HttpRequest| async move {
                        BearerAuth::<String>::authenticated(&request).unwrap()
                    }),
                ),
        )
        .await;

        let request = test::TestRequest::get()
            .uri("/")
            .insert_header((header::AUTHORIZATION, "Bearer valid"))
            .to_request();
        let response = test::call_service(&app, request).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(test::read_body(response).await, "user-42");
    }

    #[actix_rt::test]
    async fn rejects_missing_and_invalid_credentials() {
        let app = test::init_service(
            App::new()
                .wrap(
                    BearerAuth::new(|token| (token == "valid").then_some(()))
                        .with_realm("api")
                        .unwrap(),
                )
                .route("/", web::get().to(|| async { "secret" })),
        )
        .await;

        for authorization in [None, Some("Basic abc"), Some("Bearer invalid")] {
            let mut request = test::TestRequest::get().uri("/");
            if let Some(value) = authorization {
                request = request.insert_header((header::AUTHORIZATION, value));
            }
            let response = test::call_service(&app, request.to_request()).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                response.headers().get(header::WWW_AUTHENTICATE).unwrap(),
                "Bearer realm=\"api\""
            );
        }
    }
}
