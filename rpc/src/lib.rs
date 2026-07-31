//! gRPC transport primitives for rust-zero services.
//!
//! Service implementations remain ordinary Tonic services. [`RpcServer`] and [`RpcClient`]
//! centralize the transport safeguards that should be applied consistently across services.

use std::{collections::BTreeSet, error::Error as StdError, fmt, net::SocketAddr, time::Duration};

use rust_zero_core::{DiscoveryError, ServiceEvent, ServiceRegistry};
use tonic::transport::{Channel, Endpoint, Server};
use tower::discover::Change;

pub mod auth;
pub mod trace;

pub mod echo {
    tonic::include_proto!("rust_zero.echo");
}

pub use auth::{BearerToken, RpcBearerAuth};
pub use tonic_health::server::{health_reporter, HealthReporter};
pub use trace::RpcTrace;
#[cfg(feature = "telemetry")]
pub use trace::{RpcTelemetryLayer, RpcTelemetryMode};

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
    tcp_keepalive: Option<Duration>,
    http2_keepalive_interval: Option<Duration>,
    keepalive_timeout: Option<Duration>,
    keepalive_while_idle: bool,
}

impl RpcClientConfig {
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            request_timeout: None,
            connect_timeout: None,
            concurrency_limit: None,
            tcp_keepalive: None,
            http2_keepalive_interval: None,
            keepalive_timeout: None,
            keepalive_while_idle: false,
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

    pub fn with_tcp_keepalive(mut self, interval: Duration) -> Self {
        assert!(
            !interval.is_zero(),
            "TCP keepalive interval must be greater than zero"
        );
        self.tcp_keepalive = Some(interval);
        self
    }

    pub fn with_http2_keepalive(mut self, interval: Duration, timeout: Duration) -> Self {
        assert!(
            !interval.is_zero(),
            "HTTP/2 keepalive interval must be greater than zero"
        );
        assert!(
            !timeout.is_zero(),
            "HTTP/2 keepalive timeout must be greater than zero"
        );
        self.http2_keepalive_interval = Some(interval);
        self.keepalive_timeout = Some(timeout);
        self
    }

    pub fn keepalive_while_idle(mut self, enabled: bool) -> Self {
        self.keepalive_while_idle = enabled;
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
        let endpoint = self.endpoint(self.config.uri.clone())?;
        endpoint.connect().await.map_err(RpcClientError::Transport)
    }

    /// Creates a channel that follows a service registry and balances calls across its endpoints.
    ///
    /// Existing endpoints are installed before this method returns. A background task applies
    /// subsequent publications and withdrawals; it stops automatically when all channel clones
    /// have been dropped.
    pub fn connect_service(
        &self,
        registry: &ServiceRegistry,
        service: impl Into<String>,
    ) -> Result<Channel, RpcClientError> {
        let mut subscription = registry
            .subscribe(service)
            .map_err(RpcClientError::Discovery)?;
        let initial = subscription.endpoints();
        let mut configured = Vec::with_capacity(initial.len());
        for uri in initial {
            configured.push((uri.clone(), self.endpoint(uri)?));
        }

        let capacity = configured.len().max(128);
        let (channel, changes) = Channel::balance_channel(capacity);
        let mut known = BTreeSet::new();
        for (uri, endpoint) in configured {
            known.insert(uri.clone());
            changes
                .try_send(Change::Insert(uri, endpoint))
                .expect("discovery channel is sized for its initial endpoint snapshot");
        }

        let client = self.clone();
        tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    _ = changes.closed() => return,
                    event = subscription.recv() => event,
                };
                match event {
                    Ok(ServiceEvent::Added { endpoint: uri, .. }) => {
                        let Ok(endpoint) = client.endpoint(uri.clone()) else {
                            continue;
                        };
                        if changes
                            .send(Change::Insert(uri.clone(), endpoint))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        known.insert(uri);
                    }
                    Ok(ServiceEvent::Removed { endpoint: uri, .. }) => {
                        if changes.send(Change::Remove(uri.clone())).await.is_err() {
                            return;
                        }
                        known.remove(&uri);
                    }
                    Err(DiscoveryError::SubscriptionLagged(_)) => {
                        subscription.resync();
                        let current: BTreeSet<_> = subscription.endpoints().into_iter().collect();

                        for uri in known.difference(&current).cloned().collect::<Vec<_>>() {
                            if changes.send(Change::Remove(uri.clone())).await.is_err() {
                                return;
                            }
                            known.remove(&uri);
                        }
                        for uri in current.difference(&known).cloned().collect::<Vec<_>>() {
                            let Ok(endpoint) = client.endpoint(uri.clone()) else {
                                continue;
                            };
                            if changes
                                .send(Change::Insert(uri.clone(), endpoint))
                                .await
                                .is_err()
                            {
                                return;
                            }
                            known.insert(uri);
                        }
                    }
                    Err(DiscoveryError::RegistryClosed) => return,
                    Err(_) => return,
                }
            }
        });

        Ok(channel)
    }

    fn endpoint(&self, uri: String) -> Result<Endpoint, RpcClientError> {
        let mut endpoint = Endpoint::from_shared(uri).map_err(RpcClientError::Transport)?;

        if let Some(timeout) = self.config.request_timeout {
            endpoint = endpoint.timeout(timeout);
        }
        if let Some(timeout) = self.config.connect_timeout {
            endpoint = endpoint.connect_timeout(timeout);
        }
        if let Some(limit) = self.config.concurrency_limit {
            endpoint = endpoint.concurrency_limit(limit);
        }
        if let Some(interval) = self.config.tcp_keepalive {
            endpoint = endpoint.tcp_keepalive(Some(interval));
        }
        if let Some(interval) = self.config.http2_keepalive_interval {
            endpoint = endpoint.http2_keep_alive_interval(interval);
        }
        if let Some(timeout) = self.config.keepalive_timeout {
            endpoint = endpoint.keep_alive_timeout(timeout);
        }
        endpoint = endpoint.keep_alive_while_idle(self.config.keepalive_while_idle);

        Ok(endpoint)
    }
}

#[derive(Debug)]
pub enum RpcClientError {
    Transport(tonic::transport::Error),
    Discovery(DiscoveryError),
}

impl fmt::Display for RpcClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(formatter, "gRPC transport error: {error}"),
            Self::Discovery(error) => write!(formatter, "gRPC service discovery error: {error}"),
        }
    }
}

impl StdError for RpcClientError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Discovery(error) => Some(error),
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

    #[tokio::test]
    async fn discovered_client_tracks_published_rpc_endpoints() {
        let first_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first_address = first_listener.local_addr().unwrap();
        let first_server = tokio::spawn(async move {
            Server::builder()
                .add_service(EchoServer::new(EchoService))
                .serve_with_incoming(TcpListenerStream::new(first_listener))
                .await
                .unwrap();
        });

        let registry = ServiceRegistry::new();
        let channel = RpcClient::new(RpcClientConfig::new("http://unused"))
            .connect_service(&registry, "echo")
            .unwrap();
        let first_lease = registry
            .publish("echo", format!("http://{first_address}"))
            .unwrap();
        let response = EchoClient::new(channel.clone())
            .echo(Request::new(EchoRequest {
                message: "discovered".to_owned(),
            }))
            .await
            .unwrap();
        assert_eq!(response.into_inner().message, "discovered");

        drop(first_lease);
        drop(channel);
        first_server.abort();
    }

    #[test]
    fn discovered_client_rejects_invalid_service_names() {
        let registry = ServiceRegistry::new();
        let client = RpcClient::new(RpcClientConfig::new("http://unused"));

        assert!(matches!(
            client.connect_service(&registry, ""),
            Err(RpcClientError::Discovery(DiscoveryError::EmptyService))
        ));
    }
}
