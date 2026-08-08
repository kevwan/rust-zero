use crate::HttpClientMetrics;
use reqwest::{Client, Method, Request, RequestBuilder, Response, StatusCode};
use rust_zero_core::{CircuitBreaker, CircuitBreakerConfig, CircuitBreakerError, TraceContext};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

/// Production defaults for calls to a named HTTP dependency.
#[derive(Debug, Clone)]
pub struct HttpClientConfig {
    pub service: String,
    pub timeout: Duration,
    pub max_response_bytes: usize,
    pub breaker: CircuitBreakerConfig,
}

impl HttpClientConfig {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            timeout: Duration::from_secs(10),
            max_response_bytes: 10 * 1024 * 1024,
            breaker: CircuitBreakerConfig::new(5, Duration::from_secs(30)),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        assert!(!timeout.is_zero(), "HTTP timeout must be greater than zero");
        self.timeout = timeout;
        self
    }

    pub fn with_max_response_bytes(mut self, bytes: usize) -> Self {
        assert!(bytes > 0, "HTTP response limit must be greater than zero");
        self.max_response_bytes = bytes;
        self
    }

    pub fn with_breaker(mut self, breaker: CircuitBreakerConfig) -> Self {
        self.breaker = breaker;
        self
    }
}

/// Named HTTP service client with deadlines, W3C propagation, response limits,
/// and circuit breaking that treats 5xx responses as dependency failures.
#[derive(Clone)]
pub struct HttpClient {
    service: Arc<str>,
    client: Client,
    breaker: Arc<CircuitBreaker>,
    max_response_bytes: usize,
    metrics: Option<HttpClientMetrics>,
}

impl HttpClient {
    pub fn new(config: HttpClientConfig) -> Result<Self, HttpClientError> {
        if config.service.trim().is_empty() {
            return Err(HttpClientError::InvalidServiceName);
        }
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(HttpClientError::Build)?;

        Ok(Self {
            service: Arc::from(config.service),
            client,
            breaker: Arc::new(CircuitBreaker::new(config.breaker)),
            max_response_bytes: config.max_response_bytes,
            metrics: None,
        })
    }

    /// Records transport outcomes for this client in a shared metrics registry.
    pub fn with_metrics(mut self, metrics: HttpClientMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    pub fn request(&self, method: Method, url: impl reqwest::IntoUrl) -> RequestBuilder {
        self.client.request(method, url)
    }

    /// Executes a pre-built request through the service circuit breaker.
    pub async fn execute(&self, request: Request) -> Result<Response, HttpClientError> {
        let method = request.method().as_str().to_owned();
        let started_at = Instant::now();
        let _in_flight = self
            .metrics
            .as_ref()
            .map(|metrics| metrics.track_in_flight(self.service.to_string(), method.clone()));
        let result = self
            .breaker
            .execute_async_with_accept(
                || self.client.execute(request),
                |result| match result {
                    Ok(response) => !response.status().is_server_error(),
                    Err(_) => false,
                },
            )
            .await
            .map_err(|error| match error {
                CircuitBreakerError::Open => HttpClientError::CircuitOpen {
                    service: self.service.to_string(),
                },
                CircuitBreakerError::Operation(error) => HttpClientError::Transport(error),
            });

        if let Some(metrics) = &self.metrics {
            let result_label = match &result {
                Ok(response) => response.status().as_str().to_owned(),
                Err(HttpClientError::CircuitOpen { .. }) => "circuit_open".to_owned(),
                Err(HttpClientError::Transport(_)) => "transport_error".to_owned(),
                Err(_) => "client_error".to_owned(),
            };
            metrics.record(
                &self.service,
                &method,
                &result_label,
                started_at.elapsed().as_secs_f64(),
            );
        }

        result
    }

    /// Adds a child `traceparent` header and executes the request.
    pub async fn execute_traced(
        &self,
        mut request: Request,
        parent: &TraceContext,
    ) -> Result<Response, HttpClientError> {
        let child = parent.child();
        request.headers_mut().insert(
            "traceparent",
            child
                .traceparent()
                .parse()
                .expect("generated traceparent must be a valid header"),
        );
        self.execute(request).await
    }

    pub async fn get_json<T>(&self, url: impl reqwest::IntoUrl) -> Result<T, HttpClientError>
    where
        T: DeserializeOwned,
    {
        let request = self
            .request(Method::GET, url)
            .build()
            .map_err(HttpClientError::Build)?;
        let response = self.execute(request).await?;
        self.decode_json(response).await
    }

    pub async fn post_json<B, T>(
        &self,
        url: impl reqwest::IntoUrl,
        body: &B,
    ) -> Result<T, HttpClientError>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let request = self
            .request(Method::POST, url)
            .json(body)
            .build()
            .map_err(HttpClientError::Build)?;
        let response = self.execute(request).await?;
        self.decode_json(response).await
    }

    pub async fn decode_json<T>(&self, response: Response) -> Result<T, HttpClientError>
    where
        T: DeserializeOwned,
    {
        let status = response.status();
        if !status.is_success() {
            return Err(HttpClientError::Status(status));
        }
        if response
            .content_length()
            .is_some_and(|length| length > self.max_response_bytes as u64)
        {
            return Err(HttpClientError::BodyTooLarge {
                limit: self.max_response_bytes,
            });
        }

        let bytes = response.bytes().await.map_err(HttpClientError::Transport)?;
        if bytes.len() > self.max_response_bytes {
            return Err(HttpClientError::BodyTooLarge {
                limit: self.max_response_bytes,
            });
        }
        serde_json::from_slice(&bytes).map_err(HttpClientError::Decode)
    }
}

#[derive(Debug)]
pub enum HttpClientError {
    InvalidServiceName,
    Build(reqwest::Error),
    CircuitOpen { service: String },
    Transport(reqwest::Error),
    Status(StatusCode),
    BodyTooLarge { limit: usize },
    Decode(serde_json::Error),
}

impl fmt::Display for HttpClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidServiceName => formatter.write_str("HTTP service name cannot be empty"),
            Self::Build(error) => write!(formatter, "failed to build HTTP request: {error}"),
            Self::CircuitOpen { service } => {
                write!(formatter, "HTTP circuit for service {service} is open")
            }
            Self::Transport(error) => write!(formatter, "HTTP transport failed: {error}"),
            Self::Status(status) => write!(formatter, "HTTP service returned {status}"),
            Self::BodyTooLarge { limit } => {
                write!(formatter, "HTTP response exceeds the {limit}-byte limit")
            }
            Self::Decode(error) => {
                write!(formatter, "failed to decode HTTP JSON response: {error}")
            }
        }
    }
}

impl std::error::Error for HttpClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Build(error) | Self::Transport(error) => Some(error),
            Self::Decode(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer};
    use futures::stream;
    use rust_zero_core::{Metrics, TraceFlags};
    use serde_json::{json, Value};

    async fn spawn_server() -> (String, actix_web::dev::ServerHandle) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = HttpServer::new(|| {
            App::new()
                .route(
                    "/get",
                    web::get().to(|| async { HttpResponse::Ok().json(json!({"method": "get"})) }),
                )
                .route(
                    "/post",
                    web::post().to(|body: web::Json<Value>| async move {
                        HttpResponse::Ok().json(body.into_inner())
                    }),
                )
                .route(
                    "/trace",
                    web::get().to(|request: HttpRequest| async move {
                        HttpResponse::Ok().json(json!({
                            "traceparent": request
                                .headers()
                                .get("traceparent")
                                .unwrap()
                                .to_str()
                                .unwrap()
                        }))
                    }),
                )
                .route(
                    "/failure",
                    web::get().to(|| async { HttpResponse::ServiceUnavailable().finish() }),
                )
                .route(
                    "/invalid",
                    web::get().to(|| async { HttpResponse::Ok().body("not json") }),
                )
                .route(
                    "/chunked",
                    web::get().to(|| async {
                        HttpResponse::Ok().streaming(stream::once(async {
                            Ok::<_, actix_web::Error>(web::Bytes::from_static(b"123456"))
                        }))
                    }),
                )
        })
        .listen(listener)
        .unwrap()
        .run();
        let handle = server.handle();
        actix_web::rt::spawn(server);
        (format!("http://{address}"), handle)
    }

    #[test]
    fn rejects_empty_service_names() {
        assert!(matches!(
            HttpClient::new(HttpClientConfig::new(" ")),
            Err(HttpClientError::InvalidServiceName)
        ));
    }

    #[test]
    fn builds_requests_with_json_and_trace_headers() {
        let client = HttpClient::new(HttpClientConfig::new("users")).unwrap();
        let parent = TraceContext::root(TraceFlags::SAMPLED);
        let mut request = client
            .request(Method::GET, "http://localhost/users")
            .build()
            .unwrap();
        let child = parent.child();
        request
            .headers_mut()
            .insert("traceparent", child.traceparent().parse().unwrap());

        assert!(request.headers().contains_key("traceparent"));
    }

    #[actix_web::test]
    async fn gets_posts_and_propagates_trace_context() {
        let (base_url, server) = spawn_server().await;
        let client = HttpClient::new(
            HttpClientConfig::new("users")
                .with_timeout(Duration::from_secs(1))
                .with_max_response_bytes(1024),
        )
        .unwrap();

        assert_eq!(client.service(), "users");
        assert_eq!(
            client
                .get_json::<Value>(format!("{base_url}/get"))
                .await
                .unwrap(),
            json!({"method": "get"})
        );
        assert_eq!(
            client
                .post_json::<_, Value>(format!("{base_url}/post"), &json!({"id": 42}))
                .await
                .unwrap(),
            json!({"id": 42})
        );

        let parent = TraceContext::root(TraceFlags::SAMPLED);
        let request = client
            .request(Method::GET, format!("{base_url}/trace"))
            .build()
            .unwrap();
        let response: Value = client
            .decode_json(client.execute_traced(request, &parent).await.unwrap())
            .await
            .unwrap();
        let propagated = response["traceparent"].as_str().unwrap();
        assert!(propagated.starts_with(&format!("00-{}-", parent.trace_id())));

        server.stop(true).await;
    }

    #[actix_web::test]
    async fn reports_status_decode_and_response_limit_errors() {
        let (base_url, server) = spawn_server().await;
        let client =
            HttpClient::new(HttpClientConfig::new("users").with_max_response_bytes(4)).unwrap();

        let status = client
            .get_json::<Value>(format!("{base_url}/failure"))
            .await
            .unwrap_err();
        assert!(matches!(
            status,
            HttpClientError::Status(StatusCode::SERVICE_UNAVAILABLE)
        ));

        let invalid = client
            .get_json::<Value>(format!("{base_url}/invalid"))
            .await
            .unwrap_err();
        assert!(matches!(
            invalid,
            HttpClientError::BodyTooLarge { limit: 4 }
        ));

        let chunked = client
            .get_json::<Value>(format!("{base_url}/chunked"))
            .await
            .unwrap_err();
        assert!(matches!(
            chunked,
            HttpClientError::BodyTooLarge { limit: 4 }
        ));

        let decode_client = HttpClient::new(HttpClientConfig::new("users")).unwrap();
        let decode = decode_client
            .get_json::<Value>(format!("{base_url}/invalid"))
            .await
            .unwrap_err();
        assert!(matches!(decode, HttpClientError::Decode(_)));

        server.stop(true).await;
    }

    #[actix_web::test]
    async fn opens_the_circuit_after_a_server_failure() {
        let (base_url, server) = spawn_server().await;
        let metrics = Metrics::new();
        let client = HttpClient::new(
            HttpClientConfig::new("inventory")
                .with_breaker(CircuitBreakerConfig::new(1, Duration::from_secs(60))),
        )
        .unwrap()
        .with_metrics(HttpClientMetrics::new(&metrics, "test").unwrap());

        let first = client
            .execute(
                client
                    .request(Method::GET, format!("{base_url}/failure"))
                    .build()
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::SERVICE_UNAVAILABLE);

        let second = client
            .execute(
                client
                    .request(Method::GET, format!("{base_url}/get"))
                    .build()
                    .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            second,
            HttpClientError::CircuitOpen { service } if service == "inventory"
        ));

        let rendered = metrics.render();
        assert!(rendered.contains(
            "test_http_client_requests_total{service=\"inventory\",method=\"GET\",result=\"503\"} 1"
        ));
        assert!(rendered.contains(
            "test_http_client_requests_total{service=\"inventory\",method=\"GET\",result=\"circuit_open\"} 1"
        ));
        assert!(rendered.contains(
            "test_http_client_requests_in_flight{service=\"inventory\",method=\"GET\"} 0"
        ));

        server.stop(true).await;
    }

    #[actix_web::test]
    async fn reports_request_build_and_transport_errors() {
        let client = HttpClient::new(HttpClientConfig::new("users")).unwrap();
        let build = client.get_json::<Value>("not a URL").await.unwrap_err();
        assert!(matches!(build, HttpClientError::Build(_)));
        assert!(std::error::Error::source(&build).is_some());

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let request = client
            .request(Method::GET, format!("http://{address}"))
            .build()
            .unwrap();
        let transport = client.execute(request).await.unwrap_err();
        assert!(matches!(transport, HttpClientError::Transport(_)));
        assert!(std::error::Error::source(&transport).is_some());
    }

    #[test]
    fn formats_public_errors() {
        let invalid = HttpClientError::InvalidServiceName;
        assert_eq!(invalid.to_string(), "HTTP service name cannot be empty");
        assert!(std::error::Error::source(&invalid).is_none());

        assert_eq!(
            HttpClientError::CircuitOpen {
                service: "users".to_owned()
            }
            .to_string(),
            "HTTP circuit for service users is open"
        );
        assert_eq!(
            HttpClientError::Status(StatusCode::BAD_GATEWAY).to_string(),
            "HTTP service returned 502 Bad Gateway"
        );
        assert_eq!(
            HttpClientError::BodyTooLarge { limit: 16 }.to_string(),
            "HTTP response exceeds the 16-byte limit"
        );
    }

    #[test]
    #[should_panic(expected = "HTTP timeout must be greater than zero")]
    fn rejects_zero_timeouts() {
        let _ = HttpClientConfig::new("users").with_timeout(Duration::ZERO);
    }

    #[test]
    #[should_panic(expected = "HTTP response limit must be greater than zero")]
    fn rejects_zero_response_limits() {
        let _ = HttpClientConfig::new("users").with_max_response_bytes(0);
    }
}
