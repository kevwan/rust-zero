use reqwest::{Client, Method, Request, RequestBuilder, Response, StatusCode};
use rust_zero_core::{CircuitBreaker, CircuitBreakerConfig, CircuitBreakerError, TraceContext};
use serde::{de::DeserializeOwned, Serialize};
use std::{fmt, sync::Arc, time::Duration};

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
        })
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    pub fn request(&self, method: Method, url: impl reqwest::IntoUrl) -> RequestBuilder {
        self.client.request(method, url)
    }

    /// Executes a pre-built request through the service circuit breaker.
    pub async fn execute(&self, request: Request) -> Result<Response, HttpClientError> {
        self.breaker
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
            })
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
    use rust_zero_core::TraceFlags;

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
}
