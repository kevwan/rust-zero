//! gRPC transport primitives for rust-zero services.
//!
//! Service implementations remain ordinary Tonic services. [`RpcServer`] and [`RpcClient`]
//! centralize the transport safeguards that should be applied consistently across services.

use std::{error::Error as StdError, fmt, net::SocketAddr, time::Duration};

use tonic::transport::{Channel, Endpoint, Server};

pub mod echo {
    tonic::include_proto!("rust_zero.echo");
}

pub use tonic_health::server::{health_reporter, HealthReporter};

/// Transport settings applied to every gRPC service on a server.
#[derive(Debug, Clone)]
pub struct RpcServerConfig {
    address: SocketAddr,
    request_timeout: Option<Duration>,
    concurrency_limit: Option<usize>,
    max_concurrent_streams: Option<u32>,
}

impl RpcServerConfig {
    pub fn new(address: SocketAddr) -> Self {
        Self {
            address,
            request_timeout: None,
            concurrency_limit: None,
            max_concurrent_streams: None,
        }
    }

    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        assert!(
            !timeout.is_zero(),
            "request timeout must be greater than zero"
        );
        self.request_timeout = Some(timeout);
        self
    }

    pub fn with_concurrency_limit(mut self, limit: usize) -> Self {
        assert!(limit > 0, "concurrency limit must be greater than zero");
        self.concurrency_limit = Some(limit);
        self
    }

    pub fn with_max_concurrent_streams(mut self, limit: u32) -> Self {
        assert!(
            limit > 0,
            "maximum concurrent streams must be greater than zero"
        );
        self.max_concurrent_streams = Some(limit);
        self
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

/// Builds configured Tonic server routers.
#[derive(Debug, Clone)]
pub struct RpcServer {
    config: RpcServerConfig,
}

impl RpcServer {
    pub fn new(config: RpcServerConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &RpcServerConfig {
        &self.config
    }

    /// Returns a Tonic server builder with the configured timeout and load limits.
    pub fn router(&self) -> Server {
        let mut server = Server::builder();

        if let Some(timeout) = self.config.request_timeout {
            server = server.timeout(timeout);
        }
        if let Some(limit) = self.config.concurrency_limit {
            server = server.concurrency_limit_per_connection(limit);
        }
        if let Some(limit) = self.config.max_concurrent_streams {
            server = server.max_concurrent_streams(Some(limit));
        }

        server
    }
}

/// Connection settings for a gRPC client.
#[derive(Debug, Clone)]
pub struct RpcClientConfig {
    uri: String,
    request_timeout: Option<Duration>,
    connect_timeout: Option<Duration>,
    concurrency_limit: Option<usize>,
}

impl RpcClientConfig {
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            request_timeout: None,
            connect_timeout: None,
            concurrency_limit: None,
        }
    }

    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        assert!(
            !timeout.is_zero(),
            "request timeout must be greater than zero"
        );
        self.request_timeout = Some(timeout);
        self
    }

    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        assert!(
            !timeout.is_zero(),
            "connect timeout must be greater than zero"
        );
        self.connect_timeout = Some(timeout);
        self
    }

    pub fn with_concurrency_limit(mut self, limit: usize) -> Self {
        assert!(limit > 0, "concurrency limit must be greater than zero");
        self.concurrency_limit = Some(limit);
        self
    }
}

/// Connects gRPC clients using a common set of deadline and concurrency safeguards.
#[derive(Debug, Clone)]
pub struct RpcClient {
    config: RpcClientConfig,
}

impl RpcClient {
    pub fn new(config: RpcClientConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &RpcClientConfig {
        &self.config
    }

    pub async fn connect(&self) -> Result<Channel, RpcClientError> {
        let mut endpoint =
            Endpoint::from_shared(self.config.uri.clone()).map_err(RpcClientError::Transport)?;

        if let Some(timeout) = self.config.request_timeout {
            endpoint = endpoint.timeout(timeout);
        }
        if let Some(timeout) = self.config.connect_timeout {
            endpoint = endpoint.connect_timeout(timeout);
        }
        if let Some(limit) = self.config.concurrency_limit {
            endpoint = endpoint.concurrency_limit(limit);
        }

        endpoint.connect().await.map_err(RpcClientError::Transport)
    }
}

#[derive(Debug)]
pub enum RpcClientError {
    Transport(tonic::transport::Error),
}

impl fmt::Display for RpcClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(formatter, "gRPC transport error: {error}"),
        }
    }
}

impl StdError for RpcClientError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::echo::{
        echo_client::EchoClient,
        echo_server::{Echo, EchoServer},
        EchoRequest, EchoResponse,
    };
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::{Request, Response, Status};

    #[derive(Default)]
    struct EchoService;

    #[tonic::async_trait]
    impl Echo for EchoService {
        async fn echo(
            &self,
            request: Request<EchoRequest>,
        ) -> Result<Response<EchoResponse>, Status> {
            Ok(Response::new(EchoResponse {
                message: request.into_inner().message,
            }))
        }
    }

    #[test]
    fn server_configuration_preserves_address_and_limits() {
        let address = "127.0.0.1:50051".parse().unwrap();
        let config = RpcServerConfig::new(address)
            .with_request_timeout(Duration::from_secs(2))
            .with_concurrency_limit(32)
            .with_max_concurrent_streams(16);

        assert_eq!(config.address(), address);
        assert_eq!(config.request_timeout, Some(Duration::from_secs(2)));
        assert_eq!(config.concurrency_limit, Some(32));
        assert_eq!(config.max_concurrent_streams, Some(16));
    }

    #[tokio::test]
    async fn invalid_client_uri_is_reported() {
        let client = RpcClient::new(RpcClientConfig::new("not a URI"));
        let error = client.connect().await.unwrap_err();

        assert!(matches!(error, RpcClientError::Transport(_)));
    }

    #[tokio::test]
    async fn client_and_server_complete_unary_call() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = RpcServer::new(
            RpcServerConfig::new(address)
                .with_request_timeout(Duration::from_secs(1))
                .with_concurrency_limit(8),
        );
        let server_task = tokio::spawn(async move {
            server
                .router()
                .add_service(EchoServer::new(EchoService))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        let channel = RpcClient::new(
            RpcClientConfig::new(format!("http://{address}"))
                .with_connect_timeout(Duration::from_secs(1))
                .with_request_timeout(Duration::from_secs(1)),
        )
        .connect()
        .await
        .unwrap();
        let response = EchoClient::new(channel)
            .echo(Request::new(EchoRequest {
                message: "hello".to_owned(),
            }))
            .await
            .unwrap();

        assert_eq!(response.into_inner().message, "hello");
        server_task.abort();
    }
}
