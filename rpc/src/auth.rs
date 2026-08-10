use rust_zero_core::{
    decode_jwt_hs256, sign_request, AuthFailure, JwtClaimProjection, RequestSignature,
    RequestSignatureVerifier, AUTH_KEY_ID_HEADER, AUTH_SIGNATURE_HEADER, AUTH_TIMESTAMP_HEADER,
};
use serde::de::DeserializeOwned;
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use tonic::{
    metadata::{Ascii, MetadataValue},
    service::Interceptor,
    Request, Status,
};

/// Adds a bearer credential to every outgoing RPC request.
#[derive(Clone)]
pub struct BearerToken {
    authorization: MetadataValue<Ascii>,
}

impl BearerToken {
    pub fn new(token: &str) -> Result<Self, tonic::metadata::errors::InvalidMetadataValue> {
        let authorization = format!("Bearer {token}").parse()?;
        Ok(Self { authorization })
    }

    pub(crate) fn authorization(&self) -> MetadataValue<Ascii> {
        self.authorization.clone()
    }
}

impl Interceptor for BearerToken {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .insert("authorization", self.authorization.clone());
        Ok(request)
    }
}

type Validator<T> = dyn Fn(&str) -> Option<T> + Send + Sync;

/// Validates bearer credentials on incoming RPC requests.
pub struct RpcBearerAuth<T> {
    validator: Arc<Validator<T>>,
}

impl<T> Clone for RpcBearerAuth<T> {
    fn clone(&self) -> Self {
        Self {
            validator: Arc::clone(&self.validator),
        }
    }
}

impl<T> RpcBearerAuth<T>
where
    T: Clone + Send + Sync + 'static,
{
    pub fn new(validator: impl Fn(&str) -> Option<T> + Send + Sync + 'static) -> Self {
        Self {
            validator: Arc::new(validator),
        }
    }

    /// Returns the identity installed in a validated request.
    pub fn authenticated<U>(request: &Request<U>) -> Option<T> {
        request.extensions().get::<T>().cloned()
    }
}

impl<T> Interceptor for RpcBearerAuth<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let identity = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(bearer_token)
            .and_then(|token| (self.validator)(token))
            .ok_or_else(|| auth_status(AuthFailure::InvalidCredentials))?;

        request.extensions_mut().insert(identity);
        Ok(request)
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

fn auth_status(failure: AuthFailure) -> Status {
    Status::unauthenticated(format!("{}: {}", failure.code(), failure.message()))
}

/// Validates HS256 bearer tokens and exposes typed and projected claims to gRPC handlers.
#[derive(Clone)]
pub struct RpcJwtAuth<T> {
    secrets: Vec<Arc<[u8]>>,
    leeway_seconds: u64,
    projection: JwtClaimProjection,
    marker: std::marker::PhantomData<fn() -> T>,
}

impl<T> RpcJwtAuth<T>
where
    T: Clone + Send + Sync + DeserializeOwned + 'static,
{
    pub fn new(secret: impl AsRef<[u8]>) -> Self {
        let secret = secret.as_ref();
        assert!(!secret.is_empty(), "JWT secret cannot be empty");
        Self {
            secrets: vec![Arc::from(secret)],
            leeway_seconds: 0,
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

    pub fn claims<U>(request: &Request<U>) -> Option<T> {
        request
            .extensions()
            .get::<RpcJwtClaims<T>>()
            .map(|v| v.0.clone())
    }

    pub fn projected_claims<U>(
        request: &Request<U>,
    ) -> Option<BTreeMap<String, serde_json::Value>> {
        request
            .extensions()
            .get::<RpcProjectedClaims>()
            .map(|v| v.0.clone())
    }
}

#[derive(Clone)]
struct RpcJwtClaims<T>(T);

#[derive(Clone)]
struct RpcProjectedClaims(BTreeMap<String, serde_json::Value>);

impl<T> Interceptor for RpcJwtAuth<T>
where
    T: Clone + Send + Sync + DeserializeOwned + 'static,
{
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let token = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(bearer_token)
            .ok_or_else(|| auth_status(AuthFailure::MissingCredentials))?;
        let claims: T = decode_jwt_hs256(
            token,
            &self.secrets,
            self.leeway_seconds,
            unix_seconds() as u64,
        )
        .map_err(AuthFailure::from)
        .map_err(auth_status)?;
        let projected = decode_jwt_hs256::<serde_json::Value>(
            token,
            &self.secrets,
            self.leeway_seconds,
            unix_seconds() as u64,
        )
        .ok()
        .map(|value| self.projection.project(&value))
        .unwrap_or_default();
        request
            .extensions_mut()
            .insert(RpcProjectedClaims(projected));
        request.extensions_mut().insert(RpcJwtClaims(claims));
        Ok(request)
    }
}

/// Adds an HMAC request signature to outgoing gRPC metadata.
#[derive(Clone)]
pub struct RpcRequestSigner {
    key_id: String,
    secret: Arc<[u8]>,
    target: String,
}

impl RpcRequestSigner {
    pub fn new(
        key_id: impl Into<String>,
        secret: impl AsRef<[u8]>,
        target: impl Into<String>,
    ) -> Self {
        Self {
            key_id: key_id.into(),
            secret: Arc::from(secret.as_ref()),
            target: target.into(),
        }
    }
}

impl Interceptor for RpcRequestSigner {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let signature = sign_request(
            self.key_id.clone(),
            &self.secret,
            unix_seconds(),
            "POST",
            &self.target,
        )
        .map_err(auth_status)?;
        insert_signature(&mut request, &signature)?;
        Ok(request)
    }
}

/// Validates gRPC request signatures for one canonical service method target.
#[derive(Clone)]
pub struct RpcRequestSignatureAuth {
    verifier: RequestSignatureVerifier,
    target: String,
}

impl RpcRequestSignatureAuth {
    pub fn new(verifier: RequestSignatureVerifier, target: impl Into<String>) -> Self {
        Self {
            verifier,
            target: target.into(),
        }
    }

    pub fn key_id<U>(request: &Request<U>) -> Option<String> {
        request
            .extensions()
            .get::<RpcSignatureKeyId>()
            .map(|id| id.0.clone())
    }
}

#[derive(Clone)]
struct RpcSignatureKeyId(String);

impl Interceptor for RpcRequestSignatureAuth {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let signature = parse_signature(&request)?;
        self.verifier
            .verify(&signature, "POST", &self.target, unix_seconds())
            .map_err(auth_status)?;
        request
            .extensions_mut()
            .insert(RpcSignatureKeyId(signature.key_id));
        Ok(request)
    }
}

#[allow(clippy::result_large_err)] // Tonic interceptors conventionally return `Status` directly.
fn insert_signature(request: &mut Request<()>, signature: &RequestSignature) -> Result<(), Status> {
    for (name, value) in [
        (AUTH_KEY_ID_HEADER, signature.key_id.clone()),
        (AUTH_TIMESTAMP_HEADER, signature.timestamp.to_string()),
        (AUTH_SIGNATURE_HEADER, signature.signature.clone()),
    ] {
        request.metadata_mut().insert(
            name,
            value
                .parse()
                .map_err(|_| auth_status(AuthFailure::InvalidSignature))?,
        );
    }
    Ok(())
}

#[allow(clippy::result_large_err)] // Tonic interceptors conventionally return `Status` directly.
fn parse_signature(request: &Request<()>) -> Result<RequestSignature, Status> {
    let value = |name| {
        request
            .metadata()
            .get(name)
            .and_then(|value| value.to_str().ok())
    };
    let key_id =
        value(AUTH_KEY_ID_HEADER).ok_or_else(|| auth_status(AuthFailure::MissingSignature))?;
    let timestamp = value(AUTH_TIMESTAMP_HEADER)
        .ok_or_else(|| auth_status(AuthFailure::MissingSignature))?
        .parse()
        .map_err(|_| auth_status(AuthFailure::InvalidSignature))?;
    let signature =
        value(AUTH_SIGNATURE_HEADER).ok_or_else(|| auth_status(AuthFailure::MissingSignature))?;
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
    use serde::{Deserialize, Serialize};
    use std::time::Duration;
    use tonic::Code;

    #[test]
    fn client_and_server_interceptors_exchange_identity() {
        let mut client = BearerToken::new("valid").unwrap();
        let request = client.call(Request::new(())).unwrap();
        let mut server =
            RpcBearerAuth::new(|token| (token == "valid").then(|| "service-account".to_owned()));
        let request = server.call(request).unwrap();

        assert_eq!(
            RpcBearerAuth::<String>::authenticated(&request),
            Some("service-account".to_owned())
        );
    }

    #[test]
    fn server_rejects_missing_credentials() {
        let mut server = RpcBearerAuth::new(|_| Some(()));
        let error = server.call(Request::new(())).unwrap_err();

        assert_eq!(error.code(), Code::Unauthenticated);
    }

    #[derive(Clone, Deserialize, Serialize)]
    struct Claims {
        sub: String,
        exp: u64,
    }

    #[test]
    fn jwt_auth_projects_selected_claims() {
        let claims = Claims {
            sub: "service-42".to_owned(),
            exp: unix_seconds() as u64 + 60,
        };
        let token = rust_zero_core::encode_jwt_hs256(&claims, b"secret").unwrap();
        let mut request = Request::new(());
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let mut auth = RpcJwtAuth::<Claims>::new("secret").with_claim_projection(
            JwtClaimProjection::new([("caller".to_owned(), "sub".to_owned())]),
        );
        let request = auth.call(request).unwrap();

        assert_eq!(
            RpcJwtAuth::<Claims>::claims(&request).unwrap().sub,
            "service-42"
        );
        assert_eq!(
            RpcJwtAuth::<Claims>::projected_claims(&request).unwrap()["caller"],
            "service-42"
        );
    }

    #[test]
    fn rpc_request_signatures_round_trip_and_bind_the_target() {
        let verifier = RequestSignatureVerifier::new(
            [("client".to_owned(), b"secret".to_vec())],
            Duration::from_secs(30),
        )
        .unwrap();
        let target = "/rust_zero.echo.Echo/Ping";
        let request = RpcRequestSigner::new("client", "secret", target)
            .call(Request::new(()))
            .unwrap();
        let request = RpcRequestSignatureAuth::new(verifier.clone(), target)
            .call(request)
            .unwrap();
        assert_eq!(
            RpcRequestSignatureAuth::key_id(&request).as_deref(),
            Some("client")
        );

        let request = RpcRequestSigner::new("client", "secret", target)
            .call(Request::new(()))
            .unwrap();
        let error = RpcRequestSignatureAuth::new(verifier, "/other.Service/Call")
            .call(request)
            .unwrap_err();
        assert_eq!(error.code(), Code::Unauthenticated);
        assert!(error.message().starts_with("auth_invalid_signature:"));
    }
}
