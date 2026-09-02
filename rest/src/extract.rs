use actix_multipart::Multipart;
use actix_web::{
    dev::Payload,
    http::{header::HeaderMap, StatusCode},
    web, Error, FromRequest, HttpRequest, HttpResponse, ResponseError,
};
use futures::{
    future::{ready, LocalBoxFuture, Ready},
    TryStreamExt,
};
use rust_zero_core::{Validate, Violation};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::HashMap,
    fmt,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};
use tempfile::TempPath;
use tokio::io::AsyncWriteExt;

/// Stable, machine-readable failure returned by validated request extractors.
#[derive(Debug, Serialize)]
pub struct RequestExtractionError {
    pub code: &'static str,
    pub source: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<Violation>,
}

impl RequestExtractionError {
    fn parse(source: &'static str, error: impl fmt::Display) -> Self {
        Self {
            code: "invalid_request",
            source,
            message: error.to_string(),
            violations: Vec::new(),
        }
    }

    fn validation(source: &'static str, error: rust_zero_core::ValidationErrors) -> Self {
        Self {
            code: "validation_failed",
            source,
            message: error.to_string(),
            violations: error.into_violations(),
        }
    }

    fn payload_too_large(message: impl Into<String>) -> Self {
        Self {
            code: "payload_too_large",
            source: "multipart",
            message: message.into(),
            violations: Vec::new(),
        }
    }

    fn internal(source: &'static str, message: impl Into<String>) -> Self {
        Self {
            code: "request_storage_failed",
            source,
            message: message.into(),
            violations: Vec::new(),
        }
    }
}

impl fmt::Display for RequestExtractionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {}: {}", self.source, self.code, self.message)
    }
}

impl std::error::Error for RequestExtractionError {}

impl ResponseError for RequestExtractionError {
    fn status_code(&self) -> StatusCode {
        match self.code {
            "payload_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
            "request_storage_failed" => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        }
    }

    fn error_response(&self) -> HttpResponse {
        HttpResponse::build(self.status_code()).json(self)
    }
}

/// Limits and temporary storage settings for [`MultipartForm`] extraction.
///
/// File bodies are streamed to randomized temporary files and are removed when the extracted
/// form is dropped. Applications that need to retain an upload should copy or rename it while
/// handling the request.
#[derive(Clone, Debug)]
pub struct MultipartConfig {
    pub max_field_bytes: usize,
    pub max_file_bytes: usize,
    pub max_total_bytes: usize,
    pub temp_dir: PathBuf,
}

impl Default for MultipartConfig {
    fn default() -> Self {
        Self {
            max_field_bytes: 64 * 1024,
            max_file_bytes: 32 * 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024,
            temp_dir: std::env::temp_dir(),
        }
    }
}

impl MultipartConfig {
    pub fn new(max_field_bytes: usize, max_file_bytes: usize, max_total_bytes: usize) -> Self {
        Self {
            max_field_bytes,
            max_file_bytes,
            max_total_bytes,
            ..Self::default()
        }
    }

    pub fn with_temp_dir(mut self, temp_dir: impl Into<PathBuf>) -> Self {
        self.temp_dir = temp_dir.into();
        self
    }
}

/// A file streamed from a multipart request into temporary storage.
#[derive(Debug)]
pub struct UploadedFile {
    field_name: String,
    file_name: String,
    content_type: Option<String>,
    size: usize,
    path: TempPath,
}

impl UploadedFile {
    pub fn field_name(&self) -> &str {
        &self.field_name
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }
}

/// Streaming multipart form extractor with bounded text fields, files, and aggregate payloads.
#[derive(Debug)]
pub struct MultipartForm {
    fields: HashMap<String, Vec<String>>,
    files: Vec<UploadedFile>,
    total_bytes: usize,
}

impl MultipartForm {
    pub fn text(&self, name: &str) -> Option<&str> {
        self.fields
            .get(name)
            .and_then(|values| values.first())
            .map(String::as_str)
    }

    pub fn text_values(&self, name: &str) -> &[String] {
        self.fields.get(name).map(Vec::as_slice).unwrap_or_default()
    }

    pub fn files(&self) -> &[UploadedFile] {
        &self.files
    }

    pub fn files_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a UploadedFile> + 'a {
        self.files
            .iter()
            .filter(move |file| file.field_name == name)
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }
}

impl FromRequest for MultipartForm {
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self, Self::Error>>;

    fn from_request(request: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let config = request
            .app_data::<web::Data<MultipartConfig>>()
            .map(|config| config.get_ref().clone())
            .or_else(|| request.app_data::<MultipartConfig>().cloned())
            .unwrap_or_default();
        let extraction = Multipart::from_request(request, payload);

        Box::pin(async move {
            let mut multipart = extraction
                .await
                .map_err(|error| RequestExtractionError::parse("multipart", error))?;
            let mut fields = HashMap::<String, Vec<String>>::new();
            let mut files = Vec::new();
            let mut total_bytes = 0usize;

            while let Some(mut field) = multipart
                .try_next()
                .await
                .map_err(|error| RequestExtractionError::parse("multipart", error))?
            {
                let disposition = field.content_disposition();
                let field_name = disposition
                    .and_then(|value| value.get_name())
                    .ok_or_else(|| {
                        RequestExtractionError::parse(
                            "multipart",
                            "part is missing a content-disposition name",
                        )
                    })?
                    .to_owned();
                let file_name = disposition
                    .and_then(|value| value.get_filename())
                    .map(str::to_owned);
                let content_type = field.content_type().map(ToString::to_string);

                if let Some(file_name) = file_name {
                    let temporary =
                        tempfile::NamedTempFile::new_in(&config.temp_dir).map_err(|_| {
                            RequestExtractionError::internal(
                                "multipart",
                                "failed to create temporary upload storage",
                            )
                        })?;
                    let (temporary_file, temporary_path) = temporary.into_parts();
                    let mut output = tokio::fs::File::from_std(temporary_file);
                    let mut file_bytes = 0usize;

                    while let Some(chunk) = field
                        .try_next()
                        .await
                        .map_err(|error| RequestExtractionError::parse("multipart", error))?
                    {
                        file_bytes = checked_payload_size(
                            file_bytes,
                            chunk.len(),
                            config.max_file_bytes,
                            "multipart file exceeds the configured limit",
                        )?;
                        total_bytes = checked_payload_size(
                            total_bytes,
                            chunk.len(),
                            config.max_total_bytes,
                            "multipart request exceeds the configured aggregate limit",
                        )?;
                        output.write_all(&chunk).await.map_err(|_| {
                            RequestExtractionError::internal(
                                "multipart",
                                "failed to write temporary upload storage",
                            )
                        })?;
                    }
                    output.flush().await.map_err(|_| {
                        RequestExtractionError::internal(
                            "multipart",
                            "failed to flush temporary upload storage",
                        )
                    })?;
                    files.push(UploadedFile {
                        field_name,
                        file_name,
                        content_type,
                        size: file_bytes,
                        path: temporary_path,
                    });
                } else {
                    let mut value = Vec::new();
                    while let Some(chunk) = field
                        .try_next()
                        .await
                        .map_err(|error| RequestExtractionError::parse("multipart", error))?
                    {
                        checked_payload_size(
                            value.len(),
                            chunk.len(),
                            config.max_field_bytes,
                            "multipart field exceeds the configured limit",
                        )?;
                        total_bytes = checked_payload_size(
                            total_bytes,
                            chunk.len(),
                            config.max_total_bytes,
                            "multipart request exceeds the configured aggregate limit",
                        )?;
                        value.extend_from_slice(&chunk);
                    }
                    let value = String::from_utf8(value).map_err(|_| {
                        RequestExtractionError::parse("multipart", "text field is not valid UTF-8")
                    })?;
                    fields.entry(field_name).or_default().push(value);
                }
            }

            Ok(Self {
                fields,
                files,
                total_bytes,
            })
        })
    }
}

fn checked_payload_size(
    current: usize,
    additional: usize,
    limit: usize,
    message: &'static str,
) -> Result<usize, RequestExtractionError> {
    let next = current
        .checked_add(additional)
        .ok_or_else(|| RequestExtractionError::payload_too_large(message))?;
    if next > limit {
        return Err(RequestExtractionError::payload_too_large(message));
    }
    Ok(next)
}

/// Converts request headers into an application type before validation.
pub trait FromRequestHeaders: Sized {
    type Error: fmt::Display;

    fn from_headers(headers: &HeaderMap) -> Result<Self, Self::Error>;
}

/// JSON extractor that runs the request type's [`Validate`] implementation.
#[derive(Debug)]
pub struct ValidatedJson<T>(pub T);

impl<T> Deref for ValidatedJson<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for ValidatedJson<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> FromRequest for ValidatedJson<T>
where
    T: DeserializeOwned + Validate + 'static,
{
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self, Self::Error>>;

    fn from_request(request: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let extraction = web::Json::<T>::from_request(request, payload);
        Box::pin(async move {
            let value = extraction
                .await
                .map_err(|error| RequestExtractionError::parse("body", error))?
                .into_inner();
            value
                .validate()
                .map_err(|error| RequestExtractionError::validation("body", error))?;
            Ok(Self(value))
        })
    }
}

/// Query-string extractor that runs the request type's [`Validate`] implementation.
#[derive(Debug)]
pub struct ValidatedQuery<T>(pub T);

impl<T> Deref for ValidatedQuery<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Header extractor that converts and validates an application-defined type.
#[derive(Debug)]
pub struct ValidatedHeader<T>(pub T);

impl<T> Deref for ValidatedHeader<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for ValidatedHeader<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> FromRequest for ValidatedHeader<T>
where
    T: FromRequestHeaders + Validate + 'static,
{
    type Error = Error;
    type Future = Ready<Result<Self, Self::Error>>;

    fn from_request(request: &HttpRequest, _payload: &mut Payload) -> Self::Future {
        let result: Result<Self, Error> = T::from_headers(request.headers())
            .map_err(|error| Error::from(RequestExtractionError::parse("headers", error)))
            .and_then(|value| {
                value.validate().map_err(|error| {
                    Error::from(RequestExtractionError::validation("headers", error))
                })?;
                Ok(Self(value))
            });
        ready(result)
    }
}

/// Route-path extractor that runs the request type's [`Validate`] implementation.
#[derive(Debug)]
pub struct ValidatedPath<T>(pub T);

impl<T> Deref for ValidatedPath<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for ValidatedPath<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> FromRequest for ValidatedPath<T>
where
    T: DeserializeOwned + Validate + 'static,
{
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self, Self::Error>>;

    fn from_request(request: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let extraction = web::Path::<T>::from_request(request, payload);
        Box::pin(async move {
            let value = extraction
                .await
                .map_err(|error| RequestExtractionError::parse("path", error))?
                .into_inner();
            value
                .validate()
                .map_err(|error| RequestExtractionError::validation("path", error))?;
            Ok(Self(value))
        })
    }
}

/// URL-encoded form extractor that runs the request type's [`Validate`] implementation.
#[derive(Debug)]
pub struct ValidatedForm<T>(pub T);

impl<T> Deref for ValidatedForm<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for ValidatedForm<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> FromRequest for ValidatedForm<T>
where
    T: DeserializeOwned + Validate + 'static,
{
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self, Self::Error>>;

    fn from_request(request: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let extraction = web::Form::<T>::from_request(request, payload);
        Box::pin(async move {
            let value = extraction
                .await
                .map_err(|error| RequestExtractionError::parse("form", error))?
                .into_inner();
            value
                .validate()
                .map_err(|error| RequestExtractionError::validation("form", error))?;
            Ok(Self(value))
        })
    }
}

impl<T> DerefMut for ValidatedQuery<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> FromRequest for ValidatedQuery<T>
where
    T: DeserializeOwned + Validate + 'static,
{
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self, Self::Error>>;

    fn from_request(request: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let extraction = web::Query::<T>::from_request(request, payload);
        Box::pin(async move {
            let value = extraction
                .await
                .map_err(|error| RequestExtractionError::parse("query", error))?
                .into_inner();
            value
                .validate()
                .map_err(|error| RequestExtractionError::validation("query", error))?;
            Ok(Self(value))
        })
    }
}

/// A request parsed and validated from its path, query, headers, and JSON body.
///
/// This extractor keeps each transport source in a separate application type, avoiding
/// ambiguous precedence when the same field name appears in more than one source.
#[derive(Debug)]
pub struct ValidatedRequest<P, Q, H, B> {
    pub path: P,
    pub query: Q,
    pub headers: H,
    pub body: B,
}

impl<P, Q, H, B> FromRequest for ValidatedRequest<P, Q, H, B>
where
    P: DeserializeOwned + Validate + 'static,
    Q: DeserializeOwned + Validate + 'static,
    H: FromRequestHeaders + Validate + 'static,
    B: DeserializeOwned + Validate + 'static,
{
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self, Self::Error>>;

    fn from_request(request: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let path = ValidatedPath::<P>::from_request(request, payload);
        let query = ValidatedQuery::<Q>::from_request(request, payload);
        let headers = ValidatedHeader::<H>::from_request(request, payload);
        let body = ValidatedJson::<B>::from_request(request, payload);

        Box::pin(async move {
            Ok(Self {
                path: path.await?.0,
                query: query.await?.0,
                headers: headers.await?.0,
                body: body.await?.0,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{
        http::{header::HeaderMap, StatusCode},
        test, web, App, HttpResponse,
    };
    use rust_zero_core::{Validation, ValidationErrors};
    use serde::Deserialize;

    fn multipart_body(boundary: &str, title: &str, file: &[u8]) -> Vec<u8> {
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\n{title}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"asset\"; filename=\"note.txt\"\r\nContent-Type: text/plain\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(file);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        body
    }

    #[derive(Deserialize)]
    struct Request {
        name: String,
    }

    #[derive(Deserialize)]
    struct NumberRequest {
        value: u64,
    }

    impl Validate for NumberRequest {}

    impl Validate for Request {
        fn validate(&self) -> Result<(), ValidationErrors> {
            Validation::new().required("name", &self.name).finish()
        }
    }

    impl FromRequestHeaders for Request {
        type Error = &'static str;

        fn from_headers(headers: &HeaderMap) -> Result<Self, Self::Error> {
            let name = headers
                .get("x-name")
                .ok_or("x-name is required")?
                .to_str()
                .map_err(|_| "x-name must be text")?;
            Ok(Self {
                name: name.to_owned(),
            })
        }
    }

    #[actix_web::test]
    async fn rejects_invalid_json_before_the_handler() {
        let app = test::init_service(App::new().route(
            "/",
            web::post().to(|_: ValidatedJson<Request>| async { HttpResponse::Ok().finish() }),
        ))
        .await;

        let request = test::TestRequest::post()
            .uri("/")
            .set_json(serde_json::json!({ "name": " " }))
            .to_request();
        let response = test::call_service(&app, request).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(body["code"], "validation_failed");
        assert_eq!(body["source"], "body");
        assert_eq!(body["violations"][0]["field"], "name");
    }

    #[actix_web::test]
    async fn extracts_and_validates_typed_headers() {
        let app = test::init_service(App::new().route(
            "/",
            web::get().to(|value: ValidatedHeader<Request>| async move {
                HttpResponse::Ok().body(value.name.clone())
            }),
        ))
        .await;

        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/")
                .insert_header(("x-name", "Ada"))
                .to_request(),
        )
        .await;
        assert_eq!(test::read_body(response).await, "Ada");

        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/")
                .insert_header(("x-name", " "))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(body["source"], "headers");
        assert_eq!(body["violations"][0]["code"], "required");
    }

    #[actix_web::test]
    async fn accepts_valid_json_and_query_values() {
        let app = test::init_service(
            App::new()
                .route(
                    "/json",
                    web::post().to(|value: ValidatedJson<Request>| async move {
                        HttpResponse::Ok().body(value.name.clone())
                    }),
                )
                .route(
                    "/query",
                    web::get().to(|value: ValidatedQuery<Request>| async move {
                        HttpResponse::Ok().body(value.name.clone())
                    }),
                ),
        )
        .await;

        let json_response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/json")
                .set_json(serde_json::json!({ "name": "Ada" }))
                .to_request(),
        )
        .await;
        assert_eq!(test::read_body(json_response).await, "Ada");

        let query_response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/query?name=Grace")
                .to_request(),
        )
        .await;
        assert_eq!(test::read_body(query_response).await, "Grace");
    }

    #[actix_web::test]
    async fn rejects_invalid_queries_before_the_handler() {
        let app = test::init_service(App::new().route(
            "/",
            web::get().to(|_: ValidatedQuery<Request>| async { HttpResponse::Ok().finish() }),
        ))
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get().uri("/?name=%20").to_request(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[actix_web::test]
    async fn validates_path_and_form_values() {
        let app = test::init_service(
            App::new()
                .route(
                    "/path/{name}",
                    web::get().to(|value: ValidatedPath<Request>| async move {
                        HttpResponse::Ok().body(value.name.clone())
                    }),
                )
                .route(
                    "/form",
                    web::post().to(|value: ValidatedForm<Request>| async move {
                        HttpResponse::Ok().body(value.name.clone())
                    }),
                ),
        )
        .await;

        let path_response =
            test::call_service(&app, test::TestRequest::get().uri("/path/Ada").to_request()).await;
        assert_eq!(test::read_body(path_response).await, "Ada");

        let invalid_path =
            test::call_service(&app, test::TestRequest::get().uri("/path/%20").to_request()).await;
        assert_eq!(invalid_path.status(), StatusCode::BAD_REQUEST);

        let form_response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/form")
                .insert_header(("content-type", "application/x-www-form-urlencoded"))
                .set_payload("name=Grace")
                .to_request(),
        )
        .await;
        assert_eq!(test::read_body(form_response).await, "Grace");

        let invalid_form = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/form")
                .insert_header(("content-type", "application/x-www-form-urlencoded"))
                .set_payload("name=%20")
                .to_request(),
        )
        .await;
        assert_eq!(invalid_form.status(), StatusCode::BAD_REQUEST);
    }

    #[actix_web::test]
    async fn streams_multipart_files_and_cleans_temporary_storage() {
        let temp_dir = tempfile::tempdir().unwrap();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(
                    MultipartConfig::new(32, 32, 64).with_temp_dir(temp_dir.path()),
                ))
                .route(
                    "/",
                    web::post().to(|form: MultipartForm| async move {
                        let file = &form.files()[0];
                        let contents = tokio::fs::read(file.path()).await.unwrap();
                        HttpResponse::Ok().json(serde_json::json!({
                            "title": form.text("title"),
                            "file_name": file.file_name(),
                            "content_type": file.content_type(),
                            "size": file.size(),
                            "total": form.total_bytes(),
                            "contents": String::from_utf8(contents).unwrap(),
                            "path": file.path(),
                        }))
                    }),
                ),
        )
        .await;

        let boundary = "rust-zero-boundary";
        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/")
                .insert_header((
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                ))
                .set_payload(multipart_body(boundary, "hello", b"upload"))
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(body["title"], "hello");
        assert_eq!(body["file_name"], "note.txt");
        assert_eq!(body["content_type"], "text/plain");
        assert_eq!(body["size"], 6);
        assert_eq!(body["total"], 11);
        assert_eq!(body["contents"], "upload");
        assert!(!Path::new(body["path"].as_str().unwrap()).exists());
    }

    #[actix_web::test]
    async fn enforces_each_multipart_size_limit_without_leaking_files() {
        async fn call_with_limits(
            config: MultipartConfig,
            body: Vec<u8>,
        ) -> (StatusCode, serde_json::Value) {
            let app = test::init_service(App::new().app_data(web::Data::new(config)).route(
                "/",
                web::post().to(|_: MultipartForm| async { HttpResponse::Ok().finish() }),
            ))
            .await;
            let response = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri("/")
                    .insert_header((
                        "content-type",
                        "multipart/form-data; boundary=limit-boundary",
                    ))
                    .set_payload(body)
                    .to_request(),
            )
            .await;
            let status = response.status();
            (status, test::read_body_json(response).await)
        }

        let temp_dir = tempfile::tempdir().unwrap();
        let field = call_with_limits(
            MultipartConfig::new(3, 32, 64).with_temp_dir(temp_dir.path()),
            multipart_body("limit-boundary", "four", b"ok"),
        )
        .await;
        assert_eq!(field.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(field.1["code"], "payload_too_large");

        let file = call_with_limits(
            MultipartConfig::new(32, 3, 64).with_temp_dir(temp_dir.path()),
            multipart_body("limit-boundary", "ok", b"four"),
        )
        .await;
        assert_eq!(file.0, StatusCode::PAYLOAD_TOO_LARGE);

        let aggregate = call_with_limits(
            MultipartConfig::new(32, 32, 5).with_temp_dir(temp_dir.path()),
            multipart_body("limit-boundary", "abc", b"def"),
        )
        .await;
        assert_eq!(aggregate.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(std::fs::read_dir(temp_dir.path()).unwrap().count(), 0);
    }

    #[actix_web::test]
    async fn wrappers_support_mutable_dereferencing() {
        let mut json = ValidatedJson(Request {
            name: "before".to_owned(),
        });
        json.name = "after".to_owned();
        assert_eq!(json.name, "after");

        let mut query = ValidatedQuery(Request {
            name: "before".to_owned(),
        });
        query.name = "after".to_owned();
        assert_eq!(query.name, "after");

        let mut path = ValidatedPath(Request {
            name: "before".to_owned(),
        });
        path.name = "after".to_owned();
        assert_eq!(path.name, "after");

        let mut form = ValidatedForm(Request {
            name: "before".to_owned(),
        });
        form.name = "after".to_owned();
        assert_eq!(form.name, "after");

        let mut header = ValidatedHeader(Request {
            name: "before".to_owned(),
        });
        header.name = "after".to_owned();
        assert_eq!(header.name, "after");
    }

    #[actix_web::test]
    async fn combines_all_request_sources_without_field_precedence() {
        let app =
            test::init_service(
                App::new().route(
                    "/users/{name}",
                    web::post().to(
                        |request: ValidatedRequest<
                            Request,
                            NumberRequest,
                            Request,
                            NumberRequest,
                        >| async move {
                            HttpResponse::Ok().json(serde_json::json!({
                                "path": request.path.name,
                                "query": request.query.value,
                                "header": request.headers.name,
                                "body": request.body.value,
                            }))
                        },
                    ),
                ),
            )
            .await;

        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/users/Ada?value=7")
                .insert_header(("x-name", "Grace"))
                .set_json(serde_json::json!({ "value": 11 }))
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(
            body,
            serde_json::json!({
                "path": "Ada",
                "query": 7,
                "header": "Grace",
                "body": 11,
            })
        );
    }
}
