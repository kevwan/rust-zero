use actix_web::{
    body::{self, BoxBody, MessageBody},
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    http::{header, Method, StatusCode, Uri},
    web::{Bytes, BytesMut},
    Error, HttpMessage, HttpResponse,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures::{future::LocalBoxFuture, future::Ready, Stream, StreamExt};
use ring::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use std::{
    collections::HashMap,
    fmt,
    pin::Pin,
    rc::Rc,
    sync::Arc,
    task::{Context, Poll},
};

pub const CONTENT_ENCRYPTION_HEADER: &str = "x-content-encryption";
pub const CONTENT_KEY_ID_HEADER: &str = "x-content-key-id";
const ALGORITHM: &str = "aes-256-gcm-v1";
const MAGIC: &[u8; 4] = b"RZC1";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// A named AES-256-GCM key. Debug output deliberately omits the key material.
#[derive(Clone, PartialEq, Eq)]
pub struct ContentEncryptionKey {
    id: String,
    bytes: [u8; 32],
}

impl ContentEncryptionKey {
    pub fn new(id: impl Into<String>, bytes: [u8; 32]) -> Result<Self, ContentEncryptionError> {
        let id = id.into();
        if id.trim().is_empty() || id.len() > 128 || !id.bytes().all(is_key_id_byte) {
            return Err(ContentEncryptionError::InvalidKeyId);
        }
        Ok(Self { id, bytes })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Encodes a request body using the middleware's documented wire format.
    pub fn encrypt_request(
        &self,
        method: &Method,
        uri: &Uri,
        plaintext: &[u8],
    ) -> Result<String, ContentEncryptionError> {
        self.seal(&request_aad(method, uri), plaintext)
    }

    pub fn decrypt_request(
        &self,
        method: &Method,
        uri: &Uri,
        encoded: &[u8],
    ) -> Result<Bytes, ContentEncryptionError> {
        self.open(&request_aad(method, uri), encoded)
    }

    pub fn encrypt_response(
        &self,
        method: &Method,
        uri: &Uri,
        status: StatusCode,
        plaintext: &[u8],
    ) -> Result<String, ContentEncryptionError> {
        self.seal(&response_aad(method, uri, status), plaintext)
    }

    pub fn decrypt_response(
        &self,
        method: &Method,
        uri: &Uri,
        status: StatusCode,
        encoded: &[u8],
    ) -> Result<Bytes, ContentEncryptionError> {
        self.open(&response_aad(method, uri, status), encoded)
    }

    fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<String, ContentEncryptionError> {
        let key = less_safe_key(&self.bytes)?;
        let mut nonce = [0_u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| ContentEncryptionError::RandomnessUnavailable)?;
        let mut ciphertext = plaintext.to_vec();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut ciphertext,
        )
        .map_err(|_| ContentEncryptionError::EncryptionFailed)?;

        let mut envelope = Vec::with_capacity(MAGIC.len() + nonce.len() + ciphertext.len());
        envelope.extend_from_slice(MAGIC);
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&ciphertext);
        Ok(STANDARD.encode(envelope))
    }

    fn open(&self, aad: &[u8], encoded: &[u8]) -> Result<Bytes, ContentEncryptionError> {
        let mut envelope = STANDARD
            .decode(encoded)
            .map_err(|_| ContentEncryptionError::InvalidEnvelope)?;
        if envelope.len() < MAGIC.len() + NONCE_LEN + TAG_LEN || &envelope[..MAGIC.len()] != MAGIC {
            return Err(ContentEncryptionError::InvalidEnvelope);
        }
        let nonce_start = MAGIC.len();
        let ciphertext_start = nonce_start + NONCE_LEN;
        let nonce: [u8; NONCE_LEN] = envelope[nonce_start..ciphertext_start]
            .try_into()
            .map_err(|_| ContentEncryptionError::InvalidEnvelope)?;
        let key = less_safe_key(&self.bytes)?;
        let plaintext = key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut envelope[ciphertext_start..],
            )
            .map_err(|_| ContentEncryptionError::AuthenticationFailed)?;
        Ok(Bytes::copy_from_slice(plaintext))
    }
}

impl fmt::Debug for ContentEncryptionKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContentEncryptionKey")
            .field("id", &self.id)
            .field("bytes", &"[REDACTED]")
            .finish()
    }
}

/// Supplies the current response key and retained request-decryption keys.
///
/// Providers should return immutable key snapshots and perform remote refreshes outside the
/// request path. Rotation is safe when old keys remain available until all clients have moved to
/// the new `current_key` ID.
pub trait ContentKeyProvider: Send + Sync + 'static {
    fn current_key(&self) -> Option<ContentEncryptionKey>;
    fn key(&self, id: &str) -> Option<ContentEncryptionKey>;
}

/// In-memory provider useful for configuration-backed current/previous key rotation.
#[derive(Debug, Clone)]
pub struct StaticContentKeyProvider {
    current_id: String,
    keys: HashMap<String, ContentEncryptionKey>,
}

impl StaticContentKeyProvider {
    pub fn new(current: ContentEncryptionKey) -> Self {
        let current_id = current.id.clone();
        Self {
            current_id,
            keys: HashMap::from([(current.id.clone(), current)]),
        }
    }

    pub fn with_decryption_key(
        mut self,
        key: ContentEncryptionKey,
    ) -> Result<Self, ContentEncryptionError> {
        if self.keys.insert(key.id.clone(), key).is_some() {
            return Err(ContentEncryptionError::DuplicateKeyId);
        }
        Ok(self)
    }
}

impl ContentKeyProvider for StaticContentKeyProvider {
    fn current_key(&self) -> Option<ContentEncryptionKey> {
        self.keys.get(&self.current_id).cloned()
    }

    fn key(&self, id: &str) -> Option<ContentEncryptionKey> {
        self.keys.get(id).cloned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentEncryptionError {
    InvalidKeyId,
    DuplicateKeyId,
    InvalidEnvelope,
    AuthenticationFailed,
    EncryptionFailed,
    RandomnessUnavailable,
}

impl fmt::Display for ContentEncryptionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidKeyId => "invalid content-encryption key ID",
            Self::DuplicateKeyId => "duplicate content-encryption key ID",
            Self::InvalidEnvelope => "invalid encrypted-content envelope",
            Self::AuthenticationFailed => "encrypted-content authentication failed",
            Self::EncryptionFailed => "content encryption failed",
            Self::RandomnessUnavailable => "secure randomness is unavailable",
        })
    }
}

impl std::error::Error for ContentEncryptionError {}

/// Opt-in authenticated request decryption and response encryption middleware.
///
/// Non-empty requests must contain `x-content-encryption: aes-256-gcm-v1` and a
/// `x-content-key-id`. Envelopes are standard base64 over `RZC1 || nonce || ciphertext || tag`.
/// Authentication additionally binds the method, path/query, direction, and response status.
#[derive(Clone)]
pub struct ContentEncryption {
    provider: Option<Arc<dyn ContentKeyProvider>>,
    max_request_bytes: usize,
    max_response_bytes: usize,
}

impl ContentEncryption {
    pub fn new<P>(provider: P, max_body_bytes: usize) -> Self
    where
        P: ContentKeyProvider,
    {
        assert!(
            max_body_bytes > 0,
            "encrypted body limit must be greater than zero"
        );
        Self {
            provider: Some(Arc::new(provider)),
            max_request_bytes: max_body_bytes,
            max_response_bytes: max_body_bytes,
        }
    }

    pub fn with_response_limit(mut self, max_response_bytes: usize) -> Self {
        assert!(
            max_response_bytes > 0,
            "encrypted response limit must be greater than zero"
        );
        self.max_response_bytes = max_response_bytes;
        self
    }

    pub(crate) fn disabled(max_body_bytes: usize) -> Self {
        Self {
            provider: None,
            max_request_bytes: max_body_bytes,
            max_response_bytes: max_body_bytes,
        }
    }
}

impl<S, B> Transform<S, ServiceRequest> for ContentEncryption
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = Error;
    type Transform = ContentEncryptionMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        futures::future::ready(Ok(ContentEncryptionMiddleware {
            service: Rc::new(service),
            provider: self.provider.clone(),
            max_request_bytes: self.max_request_bytes,
            max_response_bytes: self.max_response_bytes,
        }))
    }
}

pub struct ContentEncryptionMiddleware<S> {
    service: Rc<S>,
    provider: Option<Arc<dyn ContentKeyProvider>>,
    max_request_bytes: usize,
    max_response_bytes: usize,
}

impl<S, B> Service<ServiceRequest> for ContentEncryptionMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(context)
    }

    fn call(&self, mut request: ServiceRequest) -> Self::Future {
        let service = Rc::clone(&self.service);
        let provider = self.provider.clone();
        let max_request_bytes = self.max_request_bytes;
        let max_response_bytes = self.max_response_bytes;

        Box::pin(async move {
            let Some(provider) = provider else {
                return Ok(service.call(request).await?.map_into_boxed_body());
            };
            let method = request.method().clone();
            let uri = request.uri().clone();
            let mut payload = request.take_payload();
            let mut encoded = BytesMut::new();
            while let Some(chunk) = payload.next().await {
                let chunk = chunk.map_err(actix_web::error::ErrorBadRequest)?;
                if encoded.len().saturating_add(chunk.len()) > max_request_bytes {
                    return Ok(error_response(request, StatusCode::PAYLOAD_TOO_LARGE));
                }
                encoded.extend_from_slice(&chunk);
            }

            if !encoded.is_empty() {
                let algorithm = request
                    .headers()
                    .get(CONTENT_ENCRYPTION_HEADER)
                    .and_then(|value| value.to_str().ok());
                if algorithm != Some(ALGORITHM) {
                    return Ok(error_response(request, StatusCode::UNSUPPORTED_MEDIA_TYPE));
                }
                let Some(key_id) = request
                    .headers()
                    .get(CONTENT_KEY_ID_HEADER)
                    .and_then(|value| value.to_str().ok())
                else {
                    return Ok(error_response(request, StatusCode::BAD_REQUEST));
                };
                let Some(key) = provider.key(key_id) else {
                    return Ok(error_response(request, StatusCode::BAD_REQUEST));
                };
                let plaintext = match key.decrypt_request(&method, &uri, &encoded) {
                    Ok(plaintext) => plaintext,
                    Err(_) => return Ok(error_response(request, StatusCode::BAD_REQUEST)),
                };
                request.headers_mut().remove(CONTENT_ENCRYPTION_HEADER);
                request.headers_mut().remove(CONTENT_KEY_ID_HEADER);
                request.headers_mut().remove(header::CONTENT_LENGTH);
                set_payload(&mut request, plaintext);
            } else {
                set_payload(&mut request, Bytes::new());
            }

            let response = service.call(request).await?.map_into_boxed_body();
            let status = response.status();
            if response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/event-stream"))
                || response
                    .headers()
                    .get(header::CONTENT_ENCODING)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| !value.eq_ignore_ascii_case("identity"))
            {
                let (request, _) = response.into_parts();
                return Ok(error_from_request(
                    request,
                    StatusCode::INTERNAL_SERVER_ERROR,
                ));
            }

            let (request, response) = response.into_parts();
            let response_headers = response.headers().clone();
            let plaintext =
                match body::to_bytes_limited(response.into_body(), max_response_bytes).await {
                    Ok(Ok(body)) => body,
                    _ => {
                        return Ok(error_from_request(
                            request,
                            StatusCode::INTERNAL_SERVER_ERROR,
                        ))
                    }
                };
            let Some(key) = provider.current_key() else {
                return Ok(error_from_request(
                    request,
                    StatusCode::INTERNAL_SERVER_ERROR,
                ));
            };
            let encoded = match key.encrypt_response(&method, &uri, status, &plaintext) {
                Ok(encoded) => encoded,
                Err(_) => {
                    return Ok(error_from_request(
                        request,
                        StatusCode::INTERNAL_SERVER_ERROR,
                    ))
                }
            };
            let mut builder = HttpResponse::build(status);
            for (name, value) in response_headers.iter() {
                if name != header::CONTENT_LENGTH && name != header::TRANSFER_ENCODING {
                    builder.append_header((name.clone(), value.clone()));
                }
            }
            builder.insert_header((CONTENT_ENCRYPTION_HEADER, ALGORITHM));
            builder.insert_header((CONTENT_KEY_ID_HEADER, key.id()));
            Ok(ServiceResponse::new(request, builder.body(encoded)))
        })
    }
}

fn set_payload(request: &mut ServiceRequest, body: Bytes) {
    let payload =
        futures::stream::once(async move { Ok::<_, actix_web::error::PayloadError>(body) });
    let payload: Pin<
        Box<dyn Stream<Item = Result<actix_web::web::Bytes, actix_web::error::PayloadError>>>,
    > = Box::pin(payload);
    request.set_payload(payload.into());
}

fn error_response(request: ServiceRequest, status: StatusCode) -> ServiceResponse<BoxBody> {
    request.into_response(HttpResponse::build(status).finish().map_into_boxed_body())
}

fn error_from_request(
    request: actix_web::HttpRequest,
    status: StatusCode,
) -> ServiceResponse<BoxBody> {
    ServiceResponse::new(request, HttpResponse::build(status).finish())
}

fn less_safe_key(bytes: &[u8; 32]) -> Result<LessSafeKey, ContentEncryptionError> {
    UnboundKey::new(&aead::AES_256_GCM, bytes)
        .map(LessSafeKey::new)
        .map_err(|_| ContentEncryptionError::EncryptionFailed)
}

fn request_aad(method: &Method, uri: &Uri) -> Vec<u8> {
    format!("rust-zero-content-v1\0request\0{method}\0{uri}").into_bytes()
}

fn response_aad(method: &Method, uri: &Uri, status: StatusCode) -> Vec<u8> {
    format!(
        "rust-zero-content-v1\0response\0{method}\0{uri}\0{}",
        status.as_u16()
    )
    .into_bytes()
}

fn is_key_id_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test as actix_test, web, App};

    fn key(id: &str, byte: u8) -> ContentEncryptionKey {
        ContentEncryptionKey::new(id, [byte; 32]).unwrap()
    }

    #[test]
    fn envelope_authenticates_context_and_ciphertext() {
        let key = key("primary", 7);
        let method = Method::POST;
        let uri: Uri = "/records?tenant=one".parse().unwrap();
        let encoded = key.encrypt_request(&method, &uri, b"secret").unwrap();
        assert_eq!(
            key.decrypt_request(&method, &uri, encoded.as_bytes())
                .unwrap(),
            Bytes::from_static(b"secret")
        );
        assert_eq!(
            key.decrypt_request(&Method::PUT, &uri, encoded.as_bytes()),
            Err(ContentEncryptionError::AuthenticationFailed)
        );

        let mut tampered = STANDARD.decode(encoded).unwrap();
        *tampered.last_mut().unwrap() ^= 1;
        assert_eq!(
            key.decrypt_request(&method, &uri, STANDARD.encode(tampered).as_bytes()),
            Err(ContentEncryptionError::AuthenticationFailed)
        );
    }

    #[actix_rt::test]
    async fn middleware_decrypts_request_and_encrypts_response_with_rotated_key() {
        let previous = key("previous", 3);
        let current = key("current", 9);
        let provider = StaticContentKeyProvider::new(current.clone())
            .with_decryption_key(previous.clone())
            .unwrap();
        let app = actix_test::init_service(
            App::new()
                .wrap(ContentEncryption::new(provider, 4096))
                .route("/echo", web::post().to(|body: Bytes| async move { body })),
        )
        .await;
        let method = Method::POST;
        let uri: Uri = "/echo".parse().unwrap();
        let encrypted = previous.encrypt_request(&method, &uri, b"hello").unwrap();
        let request = actix_test::TestRequest::post()
            .uri("/echo")
            .insert_header((CONTENT_ENCRYPTION_HEADER, ALGORITHM))
            .insert_header((CONTENT_KEY_ID_HEADER, previous.id()))
            .set_payload(encrypted)
            .to_request();
        let response = actix_test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_KEY_ID_HEADER).unwrap(),
            current.id()
        );
        let body = actix_test::read_body(response).await;
        assert_eq!(
            current
                .decrypt_response(&method, &uri, StatusCode::OK, &body)
                .unwrap(),
            Bytes::from_static(b"hello")
        );
    }

    #[actix_rt::test]
    async fn malformed_or_unknown_ciphertext_fails_before_handler() {
        let provider = StaticContentKeyProvider::new(key("current", 9));
        let app = actix_test::init_service(
            App::new()
                .wrap(ContentEncryption::new(provider, 4096))
                .route("/", web::post().to(HttpResponse::Ok)),
        )
        .await;
        let response = actix_test::call_service(
            &app,
            actix_test::TestRequest::post()
                .uri("/")
                .insert_header((CONTENT_ENCRYPTION_HEADER, ALGORITHM))
                .insert_header((CONTENT_KEY_ID_HEADER, "missing"))
                .set_payload("not ciphertext")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[actix_rt::test]
    async fn middleware_rejects_streaming_and_oversized_responses() {
        let provider = StaticContentKeyProvider::new(key("current", 9));
        let app = actix_test::init_service(
            App::new()
                .wrap(ContentEncryption::new(provider, 4096).with_response_limit(4))
                .route(
                    "/events",
                    web::get().to(|| async {
                        HttpResponse::Ok()
                            .content_type("text/event-stream")
                            .streaming(futures::stream::iter([Ok::<_, Error>(Bytes::from_static(
                                b"data: ready\n\n",
                            ))]))
                    }),
                )
                .route(
                    "/large",
                    web::get().to(|| async { HttpResponse::Ok().body("12345") }),
                ),
        )
        .await;

        for uri in ["/events", "/large"] {
            let response = actix_test::call_service(
                &app,
                actix_test::TestRequest::get().uri(uri).to_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            assert!(!response.headers().contains_key(CONTENT_ENCRYPTION_HEADER));
        }
    }
}
