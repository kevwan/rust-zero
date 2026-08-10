use actix_web::{
    body::{EitherBody, MessageBody},
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    http::{header, StatusCode},
    Error, HttpMessage, HttpRequest, HttpResponse,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures::future::{ok, LocalBoxFuture, Ready};
use hmac::{Hmac, Mac};
use rust_zero_core::{
    AuthFailure, JwtClaimProjection, RequestSignature, RequestSignatureVerifier,
    AUTH_KEY_ID_HEADER, AUTH_SIGNATURE_HEADER, AUTH_TIMESTAMP_HEADER,
};
use serde::{de::DeserializeOwned, Serialize};
use sha2::Sha256;
use std::{
    collections::BTreeMap,
    fmt,
    sync::Arc,
    task::{Context, Poll},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Serialize)]
struct AuthFailureBody {
    code: &'static str,
    message: &'static str,
}

fn auth_failure_response(failure: AuthFailure) -> HttpResponse {
    HttpResponse::build(StatusCode::UNAUTHORIZED)
        .insert_header((header::WWW_AUTHENTICATE, "Bearer"))
        .json(AuthFailureBody {
            code: failure.code(),
            message: failure.message(),
        })
}

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
                let mut response = auth_failure_response(AuthFailure::InvalidCredentials);
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, challenge);
                Ok(request.into_response(response.map_into_right_body()))
            });
        };

        request.extensions_mut().insert(Authenticated(identity));
        let future = self.service.call(request);
        Box::pin(async move { Ok(future.await?.map_into_left_body()) })
    }
}

pub(crate) fn bearer_token(value: &str) -> Option<&str> {
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

impl From<JwtError> for AuthFailure {
    fn from(error: JwtError) -> Self {
        match error {
            JwtError::Malformed | JwtError::UnsupportedAlgorithm | JwtError::InvalidClaims => {
                Self::MalformedCredentials
            }
            JwtError::InvalidSignature => Self::InvalidCredentials,
            JwtError::Expired => Self::ExpiredCredentials,
            JwtError::NotYetValid => Self::NotYetValid,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProjectedClaims(pub(crate) BTreeMap<String, serde_json::Value>);

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

pub(crate) fn decode_hs256<T>(
    token: &str,
    secrets: &[Arc<[u8]>],
    leeway_seconds: u64,
) -> Result<T, JwtError>
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
    projection: JwtClaimProjection,
    marker: std::marker::PhantomData<fn() -> T>,
}

impl<T> Clone for JwtAuth<T> {
    fn clone(&self) -> Self {
        Self {
            secrets: self.secrets.clone(),
            leeway_seconds: self.leeway_seconds,
            challenge: self.challenge.clone(),
            projection: self.projection.clone(),
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
            projection: JwtClaimProjection::default(),
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

    pub fn with_claim_projection(mut self, projection: JwtClaimProjection) -> Self {
        self.projection = projection;
        self
    }

    pub fn claims(request: &HttpRequest) -> Option<T> {
        request
            .extensions()
            .get::<JwtClaims<T>>()
            .map(|claims| claims.0.clone())
    }

    pub fn projected_claims(request: &HttpRequest) -> Option<BTreeMap<String, serde_json::Value>> {
        request
            .extensions()
            .get::<ProjectedClaims>()
            .map(|claims| claims.0.clone())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct JwtClaims<T>(pub(crate) T);

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
            projection: self.projection.clone(),
            marker: std::marker::PhantomData,
        })
    }
}

pub struct JwtAuthMiddleware<S, T> {
    service: S,
    secrets: Vec<Arc<[u8]>>,
    leeway_seconds: u64,
    challenge: header::HeaderValue,
    projection: JwtClaimProjection,
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
        let token = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(bearer_token)
            .ok_or(AuthFailure::MissingCredentials);
        let result = token.and_then(|token| {
            decode_hs256::<T>(token, &self.secrets, self.leeway_seconds).map_err(AuthFailure::from)
        });

        let claims = match result {
            Ok(claims) => claims,
            Err(failure) => {
                let challenge = self.challenge.clone();
                return Box::pin(async move {
                    let mut response = auth_failure_response(failure);
                    response
                        .headers_mut()
                        .insert(header::WWW_AUTHENTICATE, challenge);
                    Ok(request.into_response(response.map_into_right_body()))
                });
            }
        };

        let projected = token
            .ok()
            .and_then(|token| {
                decode_hs256::<serde_json::Value>(token, &self.secrets, self.leeway_seconds).ok()
            })
            .map(|value| self.projection.project(&value))
            .unwrap_or_default();
        request.extensions_mut().insert(ProjectedClaims(projected));
        request.extensions_mut().insert(JwtClaims(claims));
        let future = self.service.call(request);
        Box::pin(async move { Ok(future.await?.map_into_left_body()) })
    }
}

/// Validates time-window-bounded HMAC signatures over the HTTP method and path/query target.
#[derive(Debug, Clone)]
pub struct RequestSignatureAuth {
    verifier: RequestSignatureVerifier,
}

impl RequestSignatureAuth {
    pub fn new(verifier: RequestSignatureVerifier) -> Self {
        Self { verifier }
    }

    pub fn key_id(request: &HttpRequest) -> Option<String> {
        request
            .extensions()
            .get::<SignatureKeyId>()
            .map(|id| id.0.clone())
    }
}

#[derive(Debug, Clone)]
struct SignatureKeyId(String);

impl<S, B> Transform<S, ServiceRequest> for RequestSignatureAuth
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Transform = RequestSignatureAuthMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(RequestSignatureAuthMiddleware {
            service,
            verifier: self.verifier.clone(),
        })
    }
}

pub struct RequestSignatureAuthMiddleware<S> {
    service: S,
    verifier: RequestSignatureVerifier,
}

impl<S, B> Service<ServiceRequest> for RequestSignatureAuthMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(context)
    }

    fn call(&self, request: ServiceRequest) -> Self::Future {
        let result = parse_signature(&request).and_then(|signature| {
            self.verifier.verify(
                &signature,
                request.method().as_str(),
                request
                    .uri()
                    .path_and_query()
                    .map_or(request.path(), |value| value.as_str()),
                unix_seconds(),
            )?;
            Ok(signature.key_id)
        });
        let key_id = match result {
            Ok(key_id) => key_id,
            Err(failure) => {
                return Box::pin(async move {
                    Ok(request.into_response(auth_failure_response(failure).map_into_right_body()))
                });
            }
        };
        request.extensions_mut().insert(SignatureKeyId(key_id));
        let future = self.service.call(request);
        Box::pin(async move { Ok(future.await?.map_into_left_body()) })
    }
}

fn parse_signature(request: &ServiceRequest) -> Result<RequestSignature, AuthFailure> {
    let header = |name| {
        request
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
    };
    let key_id = header(AUTH_KEY_ID_HEADER).ok_or(AuthFailure::MissingSignature)?;
    let timestamp = header(AUTH_TIMESTAMP_HEADER)
        .ok_or(AuthFailure::MissingSignature)?
        .parse()
        .map_err(|_| AuthFailure::InvalidSignature)?;
    let signature = header(AUTH_SIGNATURE_HEADER).ok_or(AuthFailure::MissingSignature)?;
    Ok(RequestSignature {
        key_id: key_id.to_owned(),
        timestamp,
        signature: signature.to_owned(),
    })
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
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

    #[actix_rt::test]
    async fn projects_selected_jwt_claims() {
        let token = encode_hs256(
            &Claims {
                sub: "user-42".to_owned(),
                exp: unix_seconds() as u64 + 60,
            },
            b"secret",
        )
        .unwrap();
        let projection = JwtClaimProjection::new([("caller".to_owned(), "sub".to_owned())]);
        let app = test::init_service(
            App::new()
                .wrap(JwtAuth::<Claims>::new("secret").with_claim_projection(projection))
                .route(
                    "/",
                    web::get().to(|request: HttpRequest| async move {
                        web::Json(JwtAuth::<Claims>::projected_claims(&request).unwrap())
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
        let projected: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(projected["caller"], "user-42");
        assert!(projected.get("exp").is_none());
    }

    #[actix_rt::test]
    async fn authenticates_signed_http_targets_and_reports_stable_errors() {
        let verifier = RequestSignatureVerifier::new(
            [("client".to_owned(), b"secret".to_vec())],
            std::time::Duration::from_secs(30),
        )
        .unwrap();
        let app = test::init_service(App::new().wrap(RequestSignatureAuth::new(verifier)).route(
            "/jobs",
            web::post().to(|request: HttpRequest| async move {
                RequestSignatureAuth::key_id(&request).unwrap()
            }),
        ))
        .await;
        let signature = rust_zero_core::sign_request(
            "client",
            b"secret",
            unix_seconds(),
            "POST",
            "/jobs?priority=high",
        )
        .unwrap();
        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/jobs?priority=high")
                .insert_header((AUTH_KEY_ID_HEADER, signature.key_id))
                .insert_header((AUTH_TIMESTAMP_HEADER, signature.timestamp.to_string()))
                .insert_header((AUTH_SIGNATURE_HEADER, signature.signature))
                .to_request(),
        )
        .await;
        assert_eq!(test::read_body(response).await, "client");

        let response =
            test::call_service(&app, test::TestRequest::post().uri("/jobs").to_request()).await;
        let failure: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(failure["code"], "auth_missing_signature");
    }
}
