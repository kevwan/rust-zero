use actix_web::{
    http::{header, StatusCode},
    web::Bytes,
    HttpRequest, HttpResponse, ResponseError,
};
use futures::Stream;
use serde::Serialize;
use serde_json::{json, Value};
use std::{fmt, sync::Arc};
use tonic::{Code, Status};

type SuccessMapper = Arc<dyn Fn(&HttpRequest, Value) -> Value + Send + Sync>;
type ErrorMapper =
    Arc<dyn Fn(&HttpRequest, StatusCode, &str, &str, Option<Value>) -> Value + Send + Sync>;

/// An application error with a stable machine-readable code and HTTP status.
#[derive(Debug, Clone)]
pub struct ApiError {
    status: StatusCode,
    code: String,
    message: String,
    details: Option<Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: impl Serialize) -> Result<Self, serde_json::Error> {
        self.details = Some(serde_json::to_value(details)?);
        Ok(self)
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn details(&self) -> Option<&Value> {
        self.details.as_ref()
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        self.status
    }

    fn error_response(&self) -> HttpResponse {
        json_response(
            self.status,
            json!({
                "code": self.code,
                "message": self.message,
                "details": self.details,
            }),
        )
    }
}

/// Per-application JSON success and error policy.
///
/// Mappers receive the current request, allowing envelopes to include request IDs or other
/// context without process-global handlers. Values are fully serialized before headers are
/// committed, so serialization failures become a deterministic HTTP 500 response.
#[derive(Clone)]
pub struct ResponsePolicy {
    success_mapper: SuccessMapper,
    error_mapper: ErrorMapper,
}

impl Default for ResponsePolicy {
    fn default() -> Self {
        Self {
            success_mapper: Arc::new(|_, value| value),
            error_mapper: Arc::new(|_, _, code, message, details| {
                json!({
                    "code": code,
                    "message": message,
                    "details": details,
                })
            }),
        }
    }
}

impl ResponsePolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_success_mapper<F>(mut self, mapper: F) -> Self
    where
        F: Fn(&HttpRequest, Value) -> Value + Send + Sync + 'static,
    {
        self.success_mapper = Arc::new(mapper);
        self
    }

    pub fn with_error_mapper<F>(mut self, mapper: F) -> Self
    where
        F: Fn(&HttpRequest, StatusCode, &str, &str, Option<Value>) -> Value + Send + Sync + 'static,
    {
        self.error_mapper = Arc::new(mapper);
        self
    }

    pub fn ok<T>(&self, request: &HttpRequest, value: T) -> HttpResponse
    where
        T: Serialize,
    {
        match serde_json::to_value(value) {
            Ok(value) => json_response(StatusCode::OK, (self.success_mapper)(request, value)),
            Err(error) => {
                tracing::error!(error = %error, "failed to serialize HTTP response");
                self.mapped_error(
                    request,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "response_serialization_failed",
                    "failed to serialize response",
                    None,
                )
            }
        }
    }

    pub fn error(&self, request: &HttpRequest, error: &ApiError) -> HttpResponse {
        self.mapped_error(
            request,
            error.status,
            &error.code,
            &error.message,
            error.details.clone(),
        )
    }

    pub fn respond<T>(&self, request: &HttpRequest, result: Result<T, ApiError>) -> HttpResponse
    where
        T: Serialize,
    {
        match result {
            Ok(value) => self.ok(request, value),
            Err(error) => self.error(request, &error),
        }
    }

    /// Converts a Tonic status using the same status families as go-zero's REST bridge.
    pub fn grpc_error(&self, request: &HttpRequest, status: &Status) -> HttpResponse {
        let http_status = grpc_status_to_http(status.code());
        self.mapped_error(
            request,
            http_status,
            grpc_code(status.code()),
            status.message(),
            None,
        )
    }

    fn mapped_error(
        &self,
        request: &HttpRequest,
        status: StatusCode,
        code: &str,
        message: &str,
        details: Option<Value>,
    ) -> HttpResponse {
        json_response(
            status,
            (self.error_mapper)(request, status, code, message, details),
        )
    }
}

/// Maps a gRPC status code to its conventional HTTP equivalent.
pub fn grpc_status_to_http(code: Code) -> StatusCode {
    match code {
        Code::Ok => StatusCode::OK,
        Code::InvalidArgument | Code::FailedPrecondition | Code::OutOfRange => {
            StatusCode::BAD_REQUEST
        }
        Code::Unauthenticated => StatusCode::UNAUTHORIZED,
        Code::PermissionDenied => StatusCode::FORBIDDEN,
        Code::NotFound => StatusCode::NOT_FOUND,
        Code::Cancelled => StatusCode::REQUEST_TIMEOUT,
        Code::AlreadyExists | Code::Aborted => StatusCode::CONFLICT,
        Code::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
        Code::Internal | Code::DataLoss | Code::Unknown => StatusCode::INTERNAL_SERVER_ERROR,
        Code::Unimplemented => StatusCode::NOT_IMPLEMENTED,
        Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        Code::DeadlineExceeded => StatusCode::GATEWAY_TIMEOUT,
    }
}

/// Builds a chunked response whose stream items are explicit flush opportunities.
///
/// Actix forwards each yielded `Bytes` value as a body chunk. The anti-buffering headers keep
/// common reverse proxies from coalescing those chunks indefinitely.
pub fn streaming_response<S, E>(
    status: StatusCode,
    content_type: impl Into<String>,
    stream: S,
) -> HttpResponse
where
    S: Stream<Item = Result<Bytes, E>> + 'static,
    E: std::error::Error + 'static,
{
    HttpResponse::build(status)
        .insert_header((header::CONTENT_TYPE, content_type.into()))
        .insert_header((header::CACHE_CONTROL, "no-cache, no-transform"))
        .insert_header(("x-accel-buffering", "no"))
        .streaming(stream)
}

fn json_response(status: StatusCode, value: Value) -> HttpResponse {
    // Value serialization is infallible for valid serde_json values. Serializing before building
    // the response guarantees no success status is committed if that invariant ever changes.
    match serde_json::to_vec(&value) {
        Ok(body) => HttpResponse::build(status)
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .body(body),
        Err(error) => {
            tracing::error!(error = %error, "failed to serialize mapped HTTP response");
            HttpResponse::InternalServerError()
                .insert_header((header::CONTENT_TYPE, "application/json"))
                .body(r#"{"code":"response_serialization_failed","message":"failed to serialize response"}"#)
        }
    }
}

fn grpc_code(code: Code) -> &'static str {
    match code {
        Code::Ok => "ok",
        Code::Cancelled => "cancelled",
        Code::Unknown => "unknown",
        Code::InvalidArgument => "invalid_argument",
        Code::DeadlineExceeded => "deadline_exceeded",
        Code::NotFound => "not_found",
        Code::AlreadyExists => "already_exists",
        Code::PermissionDenied => "permission_denied",
        Code::ResourceExhausted => "resource_exhausted",
        Code::FailedPrecondition => "failed_precondition",
        Code::Aborted => "aborted",
        Code::OutOfRange => "out_of_range",
        Code::Unimplemented => "unimplemented",
        Code::Internal => "internal",
        Code::Unavailable => "unavailable",
        Code::DataLoss => "data_loss",
        Code::Unauthenticated => "unauthenticated",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{body::to_bytes, test};
    use futures::stream;
    use serde::ser::{Error as _, Serializer};

    struct Unserializable;

    impl Serialize for Unserializable {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Err(S::Error::custom("no representation"))
        }
    }

    #[actix_rt::test]
    async fn applies_context_aware_success_and_error_envelopes() {
        let policy = ResponsePolicy::new()
            .with_success_mapper(|request, data| {
                json!({
                    "request_id": request.headers().get("x-request-id").unwrap().to_str().unwrap(),
                    "data": data,
                })
            })
            .with_error_mapper(|request, _, code, message, details| {
                json!({
                    "request_id": request.headers().get("x-request-id").unwrap().to_str().unwrap(),
                    "error": {"code": code, "message": message, "details": details},
                })
            });
        let request = test::TestRequest::default()
            .insert_header(("x-request-id", "req-42"))
            .to_http_request();

        let response = policy.ok(&request, json!({"name": "Ada"}));
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body()).await.unwrap()).unwrap();
        assert_eq!(
            body,
            json!({
                "data": {"name": "Ada"},
                "request_id": "req-42",
            })
        );

        let error = ApiError::new(StatusCode::CONFLICT, "duplicate", "already exists")
            .with_details(json!({"field": "email"}))
            .unwrap();
        let response = policy.error(&request, &error);
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body()).await.unwrap()).unwrap();
        assert_eq!(body["request_id"], "req-42");
        assert_eq!(body["error"]["code"], "duplicate");
        assert_eq!(body["error"]["details"]["field"], "email");
    }

    #[actix_rt::test]
    async fn converts_serialization_failures_before_committing_success() {
        let request = test::TestRequest::default().to_http_request();
        let response = ResponsePolicy::new().ok(&request, Unserializable);

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body()).await.unwrap()).unwrap();
        assert_eq!(body["code"], "response_serialization_failed");
    }

    #[actix_rt::test]
    async fn translates_grpc_statuses_through_the_error_policy() {
        let request = test::TestRequest::default().to_http_request();
        let response = ResponsePolicy::new().grpc_error(
            &request,
            &Status::new(Code::Unavailable, "users backend is unavailable"),
        );

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body()).await.unwrap()).unwrap();
        assert_eq!(body["code"], "unavailable");
        assert_eq!(body["message"], "users backend is unavailable");
        assert_eq!(
            grpc_status_to_http(Code::Cancelled),
            StatusCode::REQUEST_TIMEOUT
        );
        assert_eq!(
            grpc_status_to_http(Code::DeadlineExceeded),
            StatusCode::GATEWAY_TIMEOUT
        );
    }

    #[actix_rt::test]
    async fn streams_each_flush_chunk_with_anti_buffering_headers() {
        let response = streaming_response(
            StatusCode::OK,
            "application/x-ndjson",
            stream::iter([
                Ok::<_, actix_web::Error>(Bytes::from_static(b"{\"id\":1}\n")),
                Ok::<_, actix_web::Error>(Bytes::from_static(b"{\"id\":2}\n")),
            ]),
        );

        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/x-ndjson"
        );
        assert_eq!(response.headers().get("x-accel-buffering").unwrap(), "no");
        assert_eq!(
            to_bytes(response.into_body()).await.unwrap(),
            Bytes::from_static(b"{\"id\":1}\n{\"id\":2}\n")
        );
    }
}
