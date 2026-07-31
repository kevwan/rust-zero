use actix_web::{
    body::{EitherBody, MessageBody},
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    http::{header, StatusCode},
    Error, HttpMessage, HttpRequest, HttpResponse,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures::future::{ok, LocalBoxFuture, Ready};
use hmac::{Hmac, Mac};
use serde::{de::DeserializeOwned, Serialize};
use sha2::Sha256;
use std::{
    fmt,
    sync::Arc,
    task::{Context, Poll},
    time::{SystemTime, UNIX_EPOCH},
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

type HmacSha256 = Hmac<Sha256>;

/// Errors produced while decoding or validating an HS256 JSON Web Token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwtError {
    Malformed,
    UnsupportedAlgorithm,
    InvalidSignature,
    Expired,
    NotYetValid,
    InvalidClaims,
}

impl fmt::Display for JwtError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Malformed => "malformed JWT",
            Self::UnsupportedAlgorithm => "JWT must use HS256",
            Self::InvalidSignature => "invalid JWT signature",
            Self::Expired => "JWT has expired",
            Self::NotYetValid => "JWT is not yet valid",
            Self::InvalidClaims => "invalid JWT claims",
        })
    }
}

impl std::error::Error for JwtError {}

/// Encodes serializable claims as an HS256 JSON Web Token.
pub fn encode_hs256<T>(claims: &T, secret: &[u8]) -> Result<String, JwtError>
where
    T: Serialize,
{
    if secret.is_empty() {
        return Err(JwtError::InvalidSignature);
    }
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let claims =
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).map_err(|_| JwtError::InvalidClaims)?);
    let signing_input = format!("{header}.{claims}");
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| JwtError::InvalidSignature)?;
    mac.update(signing_input.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    Ok(format!("{signing_input}.{signature}"))
}

fn decode_hs256<T>(token: &str, secrets: &[Arc<[u8]>], leeway_seconds: u64) -> Result<T, JwtError>
where
    T: DeserializeOwned,
{
    let mut segments = token.split('.');
    let header = segments.next().ok_or(JwtError::Malformed)?;
    let claims = segments.next().ok_or(JwtError::Malformed)?;
    let signature = segments.next().ok_or(JwtError::Malformed)?;
    if segments.next().is_some() {
        return Err(JwtError::Malformed);
    }

    let header_value: serde_json::Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(header)
            .map_err(|_| JwtError::Malformed)?,
    )
    .map_err(|_| JwtError::Malformed)?;
    if header_value.get("alg").and_then(|value| value.as_str()) != Some("HS256") {
        return Err(JwtError::UnsupportedAlgorithm);
    }

    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| JwtError::Malformed)?;
    let signing_input = format!("{header}.{claims}");
    let valid = secrets.iter().any(|secret| {
        HmacSha256::new_from_slice(secret)
            .map(|mut mac| {
                mac.update(signing_input.as_bytes());
                mac.verify_slice(&signature).is_ok()
            })
            .unwrap_or(false)
    });
    if !valid {
        return Err(JwtError::InvalidSignature);
    }

    let claim_bytes = URL_SAFE_NO_PAD
        .decode(claims)
        .map_err(|_| JwtError::Malformed)?;
    let claim_value: serde_json::Value =
        serde_json::from_slice(&claim_bytes).map_err(|_| JwtError::InvalidClaims)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if claim_value
        .get("exp")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|expires| now > expires.saturating_add(leeway_seconds))
    {
        return Err(JwtError::Expired);
    }
    if claim_value
        .get("nbf")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|not_before| now.saturating_add(leeway_seconds) < not_before)
    {
        return Err(JwtError::NotYetValid);
    }

    serde_json::from_slice(&claim_bytes).map_err(|_| JwtError::InvalidClaims)
}

/// Validates HS256 bearer tokens and exposes their typed claims to handlers.
///
/// A previous secret can be accepted during key rotation. Token headers are constrained to HS256
/// before signature verification to prevent algorithm-confusion attacks.
pub struct JwtAuth<T> {
    secrets: Vec<Arc<[u8]>>,
    leeway_seconds: u64,
    challenge: header::HeaderValue,
    marker: std::marker::PhantomData<fn() -> T>,
}

impl<T> Clone for JwtAuth<T> {
    fn clone(&self) -> Self {
        Self {
            secrets: self.secrets.clone(),
            leeway_seconds: self.leeway_seconds,
            challenge: self.challenge.clone(),
            marker: std::marker::PhantomData,
        }
    }
}

impl<T> JwtAuth<T>
where
    T: Clone + DeserializeOwned + 'static,
{
    pub fn new(secret: impl AsRef<[u8]>) -> Self {
        let secret = secret.as_ref();
        assert!(!secret.is_empty(), "JWT secret cannot be empty");
        Self {
            secrets: vec![Arc::from(secret)],
            leeway_seconds: 0,
            challenge: header::HeaderValue::from_static("Bearer"),
            marker: std::marker::PhantomData,
        }
    }

    pub fn with_previous_secret(mut self, secret: impl AsRef<[u8]>) -> Self {
        let secret = secret.as_ref();
        assert!(!secret.is_empty(), "previous JWT secret cannot be empty");
        self.secrets.push(Arc::from(secret));
        self
    }

    pub fn with_leeway(mut self, seconds: u64) -> Self {
        self.leeway_seconds = seconds;
        self
    }

    pub fn claims(request: &HttpRequest) -> Option<T> {
        request
            .extensions()
            .get::<JwtClaims<T>>()
            .map(|claims| claims.0.clone())
    }
}

#[derive(Debug, Clone)]
struct JwtClaims<T>(T);

impl<S, B, T> Transform<S, ServiceRequest> for JwtAuth<T>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
    T: Clone + DeserializeOwned + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Transform = JwtAuthMiddleware<S, T>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(JwtAuthMiddleware {
            service,
            secrets: self.secrets.clone(),
            leeway_seconds: self.leeway_seconds,
            challenge: self.challenge.clone(),
            marker: std::marker::PhantomData,
        })
    }
}

pub struct JwtAuthMiddleware<S, T> {
    service: S,
    secrets: Vec<Arc<[u8]>>,
    leeway_seconds: u64,
    challenge: header::HeaderValue,
    marker: std::marker::PhantomData<fn() -> T>,
}

impl<S, B, T> Service<ServiceRequest> for JwtAuthMiddleware<S, T>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
    T: Clone + DeserializeOwned + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(context)
    }

    fn call(&self, request: ServiceRequest) -> Self::Future {
        let claims = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(bearer_token)
            .and_then(|token| decode_hs256::<T>(token, &self.secrets, self.leeway_seconds).ok());

        let Some(claims) = claims else {
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

        request.extensions_mut().insert(JwtClaims(claims));
        let future = self.service.call(request);
        Box::pin(async move { Ok(future.await?.map_into_left_body()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, web, App};
    use serde::{Deserialize, Serialize};

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

    #[derive(Debug, Clone, Deserialize, Serialize)]
    struct Claims {
        sub: String,
        exp: u64,
    }

    #[actix_rt::test]
    async fn validates_jwt_claims_and_supports_secret_rotation() {
        let claims = Claims {
            sub: "user-42".to_owned(),
            exp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 60,
        };
        let token = encode_hs256(&claims, b"old-secret").unwrap();
        let app = test::init_service(
            App::new()
                .wrap(JwtAuth::<Claims>::new("new-secret").with_previous_secret("old-secret"))
                .route(
                    "/",
                    web::get().to(|request: HttpRequest| async move {
                        JwtAuth::<Claims>::claims(&request).unwrap().sub
                    }),
                ),
        )
        .await;

        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/")
                .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(test::read_body(response).await, "user-42");
    }

    #[actix_rt::test]
    async fn rejects_expired_jwts() {
        let token = encode_hs256(
            &Claims {
                sub: "user-42".to_owned(),
                exp: 1,
            },
            b"secret",
        )
        .unwrap();
        let app = test::init_service(
            App::new()
                .wrap(JwtAuth::<Claims>::new("secret"))
                .route("/", web::get().to(|| async { "secret" })),
        )
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/")
                .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
