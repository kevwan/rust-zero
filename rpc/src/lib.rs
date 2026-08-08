//! gRPC transport primitives for rust-zero services.
//!
//! Service implementations remain ordinary Tonic services. [`RpcServer`] and [`RpcClient`]
//! centralize the transport safeguards that should be applied consistently across services.

use std::{
    collections::BTreeSet, error::Error as StdError, fmt, future::Future, net::SocketAddr,
    time::Duration,
};

use rust_zero_core::{DiscoveryError, EndpointSubscription, ServiceRegistry};
use serde::{Deserialize, Serialize};
use tonic::transport::{Channel, Endpoint, Server};
use tower::discover::Change;

pub mod auth;
pub mod metrics;
pub mod resilience;
pub mod stack;
pub mod trace;

pub mod echo {
    tonic::include_proto!("rust_zero.echo");
}

pub use auth::{BearerToken, RpcBearerAuth};
pub use metrics::{RpcMetricMode, RpcMetrics, RpcMetricsLayer};
pub use resilience::{acceptable_status, RpcCircuitBreaker, RpcLoadShedder};
pub use stack::{
    RpcClientStack, RpcClientStackBuilder, RpcClientStackService, RpcServerStack,
    RpcServerStackBuilder,
};
pub use tonic_health::server::{health_reporter, HealthReporter};
pub use trace::RpcTrace;
#[cfg(feature = "telemetry")]
pub use trace::{RpcTelemetryLayer, RpcTelemetryMode};

/// Transport settings applied to every gRPC service on a server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RpcServerConfig {
    address: SocketAddr,
    #[serde(rename = "request_timeout_ms", with = "optional_duration_millis")]
    request_timeout: Option<Duration>,
    concurrency_limit: Option<usize>,
    max_concurrent_streams: Option<u32>,
    #[serde(rename = "shutdown_timeout_ms", with = "duration_millis")]
    shutdown_timeout: Duration,
}

impl Default for RpcServerConfig {
    fn default() -> Self {
        Self::new(
            "0.0.0.0:50051"
                .parse()
                .expect("default RPC address is valid"),
        )
    }
}

impl RpcServerConfig {
    pub fn new(address: SocketAddr) -> Self {
        Self {
            address,
            request_timeout: None,
            concurrency_limit: None,
            max_concurrent_streams: None,
            shutdown_timeout: Duration::from_secs(30),
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

    pub fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        assert!(
            !timeout.is_zero(),
            "shutdown timeout must be greater than zero"
        );
        self.shutdown_timeout = timeout;
        self
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn shutdown_timeout(&self) -> Duration {
        self.shutdown_timeout
    }

    pub fn validate(&self) -> Result<(), RpcConfigError> {
        if self
            .request_timeout
            .is_some_and(|duration| duration.is_zero())
        {
            return Err(RpcConfigError::Invalid(
                "request timeout must be greater than zero",
            ));
        }
        if self.concurrency_limit == Some(0) {
            return Err(RpcConfigError::Invalid(
                "concurrency limit must be greater than zero",
            ));
        }
        if self.max_concurrent_streams == Some(0) {
            return Err(RpcConfigError::Invalid(
                "maximum concurrent streams must be greater than zero",
            ));
        }
        if self.shutdown_timeout.is_zero() {
            return Err(RpcConfigError::Invalid(
                "shutdown timeout must be greater than zero",
            ));
        }
        Ok(())
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

    pub fn try_new(config: RpcServerConfig) -> Result<Self, RpcConfigError> {
        config.validate()?;
        Ok(Self { config })
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

    /// Serves an assembled Tonic router and bounds draining after the shutdown signal fires.
    pub async fn serve_with_shutdown<F>(
        &self,
        router: tonic::transport::server::Router,
        signal: F,
    ) -> Result<(), RpcServerError>
    where
        F: Future<Output = ()>,
    {
        self.config
            .validate()
            .map_err(RpcServerError::Configuration)?;
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let serving = router.serve_with_shutdown(self.config.address, async move {
            let _ = stopped.await;
        });
        tokio::pin!(serving);
        tokio::pin!(signal);

        tokio::select! {
            result = &mut serving => result.map_err(RpcServerError::Transport),
            _ = &mut signal => {
                let _ = stop.send(());
                tokio::time::timeout(self.config.shutdown_timeout, serving)
                    .await
                    .map_err(|_| RpcServerError::ShutdownTimeout)?
                    .map_err(RpcServerError::Transport)
            }
        }
    }
}

#[derive(Debug)]
pub enum RpcServerError {
    Configuration(RpcConfigError),
    Transport(tonic::transport::Error),
    ShutdownTimeout,
}

impl fmt::Display for RpcServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(error) => write!(formatter, "invalid gRPC configuration: {error}"),
            Self::Transport(error) => write!(formatter, "gRPC server transport error: {error}"),
            Self::ShutdownTimeout => formatter.write_str("gRPC graceful shutdown timed out"),
        }
    }
}

impl StdError for RpcServerError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Configuration(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::ShutdownTimeout => None,
        }
    }
}

/// Connection settings for a gRPC client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RpcClientConfig {
    uri: String,
    #[serde(rename = "request_timeout_ms", with = "optional_duration_millis")]
    request_timeout: Option<Duration>,
    #[serde(rename = "connect_timeout_ms", with = "optional_duration_millis")]
    connect_timeout: Option<Duration>,
    concurrency_limit: Option<usize>,
    #[serde(rename = "tcp_keepalive_ms", with = "optional_duration_millis")]
    tcp_keepalive: Option<Duration>,
    #[serde(
        rename = "http2_keepalive_interval_ms",
        with = "optional_duration_millis"
    )]
    http2_keepalive_interval: Option<Duration>,
    #[serde(rename = "keepalive_timeout_ms", with = "optional_duration_millis")]
    keepalive_timeout: Option<Duration>,
    keepalive_while_idle: bool,
}

impl Default for RpcClientConfig {
    fn default() -> Self {
        Self::new(String::new())
    }
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

    pub fn validate(&self) -> Result<(), RpcConfigError> {
        if self.uri.trim().is_empty() {
            return Err(RpcConfigError::Invalid("client URI must not be empty"));
        }
        for (name, duration) in [
            ("request timeout", self.request_timeout),
            ("connect timeout", self.connect_timeout),
            ("TCP keepalive interval", self.tcp_keepalive),
            ("HTTP/2 keepalive interval", self.http2_keepalive_interval),
            ("HTTP/2 keepalive timeout", self.keepalive_timeout),
        ] {
            if duration.is_some_and(|duration| duration.is_zero()) {
                return Err(RpcConfigError::Invalid(match name {
                    "request timeout" => "request timeout must be greater than zero",
                    "connect timeout" => "connect timeout must be greater than zero",
                    "TCP keepalive interval" => "TCP keepalive interval must be greater than zero",
                    "HTTP/2 keepalive interval" => {
                        "HTTP/2 keepalive interval must be greater than zero"
                    }
                    _ => "HTTP/2 keepalive timeout must be greater than zero",
                }));
            }
        }
        if self.concurrency_limit == Some(0) {
            return Err(RpcConfigError::Invalid(
                "concurrency limit must be greater than zero",
            ));
        }
        if self.http2_keepalive_interval.is_some() != self.keepalive_timeout.is_some() {
            return Err(RpcConfigError::Invalid(
                "HTTP/2 keepalive interval and timeout must be configured together",
            ));
        }
        Ok(())
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

    pub fn try_new(config: RpcClientConfig) -> Result<Self, RpcConfigError> {
        config.validate()?;
        Ok(Self { config })
    }

    pub fn config(&self) -> &RpcClientConfig {
        &self.config
    }

    pub async fn connect(&self) -> Result<Channel, RpcClientError> {
        self.config
            .validate()
            .map_err(RpcClientError::Configuration)?;
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
        let subscription = registry
            .subscribe(service)
            .map_err(RpcClientError::Discovery)?;
        Ok(self.connect_discovered(subscription))
    }

    /// Creates a channel driven by any discovery backend that publishes complete snapshots.
    ///
    /// Empty snapshots are supported and recover when endpoints appear. Malformed endpoints are
    /// ignored without poisoning the rest of a snapshot. The watcher exits when the discovery
    /// stream closes or all clones of the returned channel have been dropped.
    pub fn connect_discovered<S>(&self, mut subscription: S) -> Channel
    where
        S: EndpointSubscription,
    {
        let initial = subscription.endpoints();
        let mut configured = Vec::with_capacity(initial.len());
        for uri in initial {
            if let Ok(endpoint) = self.endpoint(uri.clone()) {
                configured.push((uri, endpoint));
            }
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
                let snapshot = tokio::select! {
                    _ = changes.closed() => return,
                    snapshot = subscription.changed() => snapshot,
                };
                let Ok(snapshot) = snapshot else {
                    return;
                };
                let current: BTreeSet<_> = snapshot
                    .into_iter()
                    .filter(|uri| client.endpoint(uri.clone()).is_ok())
                    .collect();

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
        });

        channel
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
    Configuration(RpcConfigError),
    Transport(tonic::transport::Error),
    Discovery(DiscoveryError),
}

impl fmt::Display for RpcClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(error) => write!(formatter, "invalid gRPC configuration: {error}"),
            Self::Transport(error) => write!(formatter, "gRPC transport error: {error}"),
            Self::Discovery(error) => write!(formatter, "gRPC service discovery error: {error}"),
        }
    }
}

impl StdError for RpcClientError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Configuration(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::Discovery(error) => Some(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcConfigError {
    Invalid(&'static str),
}

impl fmt::Display for RpcConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl StdError for RpcConfigError {}

mod duration_millis {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(value.as_millis().try_into().unwrap_or(u64::MAX))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        u64::deserialize(deserializer).map(Duration::from_millis)
    }
}

mod optional_duration_millis {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(value: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value
            .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<u64>::deserialize(deserializer).map(|value| value.map(Duration::from_millis))
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
    use futures::{Stream, StreamExt};
    use std::{pin::Pin, sync::Arc};
    use tokio::{
        net::TcpListener,
        sync::{oneshot, watch, Notify},
    };
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::{Request, Response, Status};

    #[test]
    fn transport_configs_deserialize_millisecond_durations() {
        let server: RpcServerConfig = rust_zero_core::parse_config(
            "address = \"127.0.0.1:50052\"\nrequest_timeout_ms = 750\nshutdown_timeout_ms = 5000",
            rust_zero_core::ConfigFormat::Toml,
        )
        .unwrap();
        assert_eq!(server.request_timeout, Some(Duration::from_millis(750)));
        assert_eq!(server.shutdown_timeout(), Duration::from_secs(5));
        server.validate().unwrap();

        let client: RpcClientConfig = rust_zero_core::parse_config(
            "uri = \"http://127.0.0.1:50052\"\nconnect_timeout_ms = 250",
            rust_zero_core::ConfigFormat::Toml,
        )
        .unwrap();
        assert_eq!(client.connect_timeout, Some(Duration::from_millis(250)));
        client.validate().unwrap();
    }

    #[test]
    fn transport_configs_reject_incomplete_keepalive_settings() {
        let client: RpcClientConfig = rust_zero_core::parse_config(
            r#"{"uri":"http://localhost:50051","http2_keepalive_interval_ms":1000}"#,
            rust_zero_core::ConfigFormat::Json,
        )
        .unwrap();
        assert!(client.validate().is_err());
    }

    #[derive(Default)]
    struct EchoService;

    type EchoStream = Pin<Box<dyn Stream<Item = Result<EchoResponse, Status>> + Send>>;

    #[tonic::async_trait]
    impl Echo for EchoService {
        type ServerStreamStream = EchoStream;
        type BidirectionalStreamStream = EchoStream;

        async fn echo(
            &self,
            request: Request<EchoRequest>,
        ) -> Result<Response<EchoResponse>, Status> {
            Ok(Response::new(EchoResponse {
                message: request.into_inner().message,
            }))
        }

        async fn server_stream(
            &self,
            request: Request<EchoRequest>,
        ) -> Result<Response<Self::ServerStreamStream>, Status> {
            Ok(Response::new(Box::pin(tokio_stream::iter([Ok(
                EchoResponse {
                    message: request.into_inner().message,
                },
            )]))))
        }

        async fn client_stream(
            &self,
            request: Request<tonic::Streaming<EchoRequest>>,
        ) -> Result<Response<EchoResponse>, Status> {
            let mut input = request.into_inner();
            let mut messages = Vec::new();
            while let Some(message) = input.message().await? {
                messages.push(message.message);
            }
            Ok(Response::new(EchoResponse {
                message: messages.join(","),
            }))
        }

        async fn bidirectional_stream(
            &self,
            request: Request<tonic::Streaming<EchoRequest>>,
        ) -> Result<Response<Self::BidirectionalStreamStream>, Status> {
            let replies = futures::stream::unfold(request.into_inner(), |mut input| async move {
                match input.message().await {
                    Ok(Some(message)) => Some((
                        Ok(EchoResponse {
                            message: message.message,
                        }),
                        input,
                    )),
                    Err(status) => Some((Err(status), input)),
                    Ok(None) => None,
                }
            });
            Ok(Response::new(Box::pin(replies)))
        }
    }

    struct NamedEchoService(&'static str);

    #[tonic::async_trait]
    impl Echo for NamedEchoService {
        type ServerStreamStream = EchoStream;
        type BidirectionalStreamStream = EchoStream;

        async fn echo(&self, _: Request<EchoRequest>) -> Result<Response<EchoResponse>, Status> {
            Ok(Response::new(EchoResponse {
                message: self.0.to_owned(),
            }))
        }

        async fn server_stream(
            &self,
            _: Request<EchoRequest>,
        ) -> Result<Response<Self::ServerStreamStream>, Status> {
            Ok(Response::new(Box::pin(tokio_stream::iter([Ok(
                EchoResponse {
                    message: self.0.to_owned(),
                },
            )]))))
        }

        async fn client_stream(
            &self,
            _: Request<tonic::Streaming<EchoRequest>>,
        ) -> Result<Response<EchoResponse>, Status> {
            Ok(Response::new(EchoResponse {
                message: self.0.to_owned(),
            }))
        }

        async fn bidirectional_stream(
            &self,
            _: Request<tonic::Streaming<EchoRequest>>,
        ) -> Result<Response<Self::BidirectionalStreamStream>, Status> {
            Ok(Response::new(Box::pin(tokio_stream::iter([Ok(
                EchoResponse {
                    message: self.0.to_owned(),
                },
            )]))))
        }
    }

    struct StackedEchoService;

    #[tonic::async_trait]
    impl Echo for StackedEchoService {
        type ServerStreamStream = EchoStream;
        type BidirectionalStreamStream = EchoStream;

        async fn echo(
            &self,
            request: Request<EchoRequest>,
        ) -> Result<Response<EchoResponse>, Status> {
            assert_eq!(
                request.extensions().get::<String>().map(String::as_str),
                Some("caller")
            );
            assert!(request
                .extensions()
                .get::<rust_zero_core::TraceContext>()
                .is_some());
            if request.get_ref().message == "panic" {
                panic!("intentional handler panic");
            }
            Ok(Response::new(EchoResponse {
                message: request.into_inner().message,
            }))
        }

        async fn server_stream(
            &self,
            request: Request<EchoRequest>,
        ) -> Result<Response<Self::ServerStreamStream>, Status> {
            if let Some(error) = stack_extension_error(&request) {
                return Err(error);
            }
            let message = request.into_inner().message;
            let stream: EchoStream = match message.as_str() {
                "status" => Box::pin(tokio_stream::iter([
                    Ok(EchoResponse {
                        message: "first".to_owned(),
                    }),
                    Err(Status::unavailable("stream failed")),
                ])),
                "cancel" => Box::pin(
                    tokio_stream::once(Ok(EchoResponse {
                        message: "first".to_owned(),
                    }))
                    .chain(futures::stream::pending()),
                ),
                _ => Box::pin(tokio_stream::iter([Ok(EchoResponse { message })])),
            };
            Ok(Response::new(stream))
        }

        async fn client_stream(
            &self,
            request: Request<tonic::Streaming<EchoRequest>>,
        ) -> Result<Response<EchoResponse>, Status> {
            if let Some(error) = stack_extension_error(&request) {
                return Err(error);
            }
            EchoService.client_stream(request).await
        }

        async fn bidirectional_stream(
            &self,
            request: Request<tonic::Streaming<EchoRequest>>,
        ) -> Result<Response<Self::BidirectionalStreamStream>, Status> {
            if let Some(error) = stack_extension_error(&request) {
                return Err(error);
            }
            EchoService.bidirectional_stream(request).await
        }
    }

    fn stack_extension_error<T>(request: &Request<T>) -> Option<Status> {
        if request.extensions().get::<String>().map(String::as_str) != Some("caller") {
            return Some(Status::internal(
                "authentication did not run before handler",
            ));
        }
        if request
            .extensions()
            .get::<rust_zero_core::TraceContext>()
            .is_none()
        {
            return Some(Status::internal("tracing did not run before handler"));
        }
        None
    }

    struct DrainEchoService {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[tonic::async_trait]
    impl Echo for DrainEchoService {
        type ServerStreamStream = EchoStream;
        type BidirectionalStreamStream = EchoStream;

        async fn echo(
            &self,
            request: Request<EchoRequest>,
        ) -> Result<Response<EchoResponse>, Status> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(Response::new(EchoResponse {
                message: request.into_inner().message,
            }))
        }

        async fn server_stream(
            &self,
            _: Request<EchoRequest>,
        ) -> Result<Response<Self::ServerStreamStream>, Status> {
            Err(Status::unimplemented("not used by drain tests"))
        }

        async fn client_stream(
            &self,
            _: Request<tonic::Streaming<EchoRequest>>,
        ) -> Result<Response<EchoResponse>, Status> {
            Err(Status::unimplemented("not used by drain tests"))
        }

        async fn bidirectional_stream(
            &self,
            _: Request<tonic::Streaming<EchoRequest>>,
        ) -> Result<Response<Self::BidirectionalStreamStream>, Status> {
            Err(Status::unimplemented("not used by drain tests"))
        }
    }

    struct TestSubscription {
        receiver: watch::Receiver<Vec<String>>,
        dropped: Option<oneshot::Sender<()>>,
    }

    impl EndpointSubscription for TestSubscription {
        type Error = watch::error::RecvError;

        fn endpoints(&self) -> Vec<String> {
            self.receiver.borrow().clone()
        }

        fn changed(&mut self) -> rust_zero_core::EndpointChangeFuture<'_, Self::Error> {
            Box::pin(async move {
                self.receiver.changed().await?;
                Ok(self.receiver.borrow().clone())
            })
        }
    }

    impl Drop for TestSubscription {
        fn drop(&mut self) {
            if let Some(dropped) = self.dropped.take() {
                let _ = dropped.send(());
            }
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
        use std::sync::Arc;
        use tower::Layer;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let metrics = Arc::new(rust_zero_core::Metrics::new());
        let server_metrics = RpcMetrics::new(
            metrics.as_ref(),
            "echo",
            RpcMetricMode::Server,
            ["/rust_zero.echo.Echo/Echo"],
        )
        .unwrap();
        let server = RpcServer::new(
            RpcServerConfig::new(address)
                .with_request_timeout(Duration::from_secs(1))
                .with_concurrency_limit(8),
        );
        let server_task = tokio::spawn(async move {
            server
                .router()
                .layer(RpcMetricsLayer::new(server_metrics))
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
        let client_metrics = RpcMetrics::new(
            metrics.as_ref(),
            "echo",
            RpcMetricMode::Client,
            ["/rust_zero.echo.Echo/Echo"],
        )
        .unwrap();
        let client_stack = RpcClientStackBuilder::new(client_metrics)
            .with_default_timeout(Duration::from_secs(1))
            .with_circuit_breaker(rust_zero_core::CircuitBreakerConfig::new(
                3,
                Duration::from_secs(30),
            ))
            .build();
        let response = EchoClient::new(client_stack.layer(channel))
            .echo(Request::new(EchoRequest {
                message: "hello".to_owned(),
            }))
            .await
            .unwrap();

        assert_eq!(response.into_inner().message, "hello");
        let rendered = metrics.render();
        assert!(rendered.contains(
            "echo_rpc_server_requests_total{method=\"/rust_zero.echo.Echo/Echo\",code=\"0\"} 1"
        ));
        assert!(rendered.contains(
            "echo_rpc_client_requests_total{method=\"/rust_zero.echo.Echo/Echo\",code=\"0\"} 1"
        ));
        server_task.abort();
    }

    async fn connect_eventually(address: SocketAddr) -> Channel {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(channel) = RpcClient::new(
                RpcClientConfig::new(format!("http://{address}"))
                    .with_connect_timeout(Duration::from_millis(100)),
            )
            .connect()
            .await
            {
                return channel;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "gRPC server did not start listening"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn configured_server_drains_in_flight_calls_and_bounds_shutdown() {
        let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = reservation.local_addr().unwrap();
        drop(reservation);

        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let server = RpcServer::new(
            RpcServerConfig::new(address).with_shutdown_timeout(Duration::from_secs(1)),
        );
        let router = server
            .router()
            .add_service(EchoServer::new(DrainEchoService {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            }));
        let (shutdown, shutdown_signal) = oneshot::channel();
        let server_task = tokio::spawn(async move {
            server
                .serve_with_shutdown(router, async {
                    let _ = shutdown_signal.await;
                })
                .await
        });

        let channel = connect_eventually(address).await;
        let call = tokio::spawn(async move {
            EchoClient::new(channel)
                .echo(EchoRequest {
                    message: "drained".to_owned(),
                })
                .await
        });
        entered.notified().await;
        shutdown.send(()).unwrap();
        tokio::task::yield_now().await;
        assert!(
            !server_task.is_finished(),
            "server must wait for an in-flight call"
        );
        release.notify_one();
        assert_eq!(call.await.unwrap().unwrap().into_inner().message, "drained");
        tokio::time::timeout(Duration::from_secs(1), server_task)
            .await
            .expect("server should finish after its in-flight call")
            .unwrap()
            .unwrap();

        let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = reservation.local_addr().unwrap();
        drop(reservation);
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let server = RpcServer::new(
            RpcServerConfig::new(address).with_shutdown_timeout(Duration::from_millis(50)),
        );
        let router = server
            .router()
            .add_service(EchoServer::new(DrainEchoService {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            }));
        let (shutdown, shutdown_signal) = oneshot::channel();
        let server_task = tokio::spawn(async move {
            server
                .serve_with_shutdown(router, async {
                    let _ = shutdown_signal.await;
                })
                .await
        });
        let channel = connect_eventually(address).await;
        let call = tokio::spawn(async move {
            EchoClient::new(channel)
                .echo(EchoRequest {
                    message: "too-slow".to_owned(),
                })
                .await
        });
        entered.notified().await;
        shutdown.send(()).unwrap();
        assert!(matches!(
            server_task.await.unwrap(),
            Err(RpcServerError::ShutdownTimeout)
        ));
        release.notify_one();
        let _ = call.await;
    }

    #[tokio::test]
    async fn standard_server_stack_composes_auth_trace_metrics_recovery_and_health() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let registry = Arc::new(rust_zero_core::Metrics::new());
        let metrics = RpcMetrics::new(
            registry.as_ref(),
            "stacked",
            RpcMetricMode::Server,
            [
                "/rust_zero.echo.Echo/Echo",
                "/rust_zero.echo.Echo/ServerStream",
                "/rust_zero.echo.Echo/ClientStream",
                "/rust_zero.echo.Echo/BidirectionalStream",
            ],
        )
        .unwrap();
        let stack = RpcServerStackBuilder::new(metrics)
            .with_bearer_auth(|token| (token == "secret").then(|| "caller".to_owned()))
            .with_load_shedder(rust_zero_core::LoadShedderConfig::new(
                8,
                Duration::from_secs(1),
            ))
            .build();
        let (mut reporter, health) = health_reporter();
        reporter
            .set_serving::<EchoServer<StackedEchoService>>()
            .await;
        let server = tokio::spawn(async move {
            Server::builder()
                .layer(stack)
                .add_service(health)
                .add_service(EchoServer::new(StackedEchoService))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        let channel = RpcClient::new(RpcClientConfig::new(format!("http://{address}")))
            .connect()
            .await
            .unwrap();
        let error = EchoClient::new(channel.clone())
            .echo(EchoRequest {
                message: "denied".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::Unauthenticated);

        let mut client =
            EchoClient::with_interceptor(channel.clone(), BearerToken::new("secret").unwrap());
        let response = client
            .echo(EchoRequest {
                message: "accepted".into(),
            })
            .await
            .unwrap();
        assert_eq!(response.into_inner().message, "accepted");
        let error = client
            .echo(EchoRequest {
                message: "panic".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::Internal);
        let response = client
            .echo(EchoRequest {
                message: "still-serving".into(),
            })
            .await
            .unwrap();
        assert_eq!(response.into_inner().message, "still-serving");

        let mut health_client = tonic_health::pb::health_client::HealthClient::with_interceptor(
            channel.clone(),
            BearerToken::new("secret").unwrap(),
        );
        let health_request = || tonic_health::pb::HealthCheckRequest {
            service: "rust_zero.echo.Echo".to_owned(),
        };
        assert_eq!(
            health_client
                .check(health_request())
                .await
                .unwrap()
                .into_inner()
                .status,
            tonic_health::pb::health_check_response::ServingStatus::Serving as i32
        );
        reporter
            .set_not_serving::<EchoServer<StackedEchoService>>()
            .await;
        assert_eq!(
            health_client
                .check(health_request())
                .await
                .unwrap()
                .into_inner()
                .status,
            tonic_health::pb::health_check_response::ServingStatus::NotServing as i32
        );

        let client_metrics = RpcMetrics::new(
            registry.as_ref(),
            "stacked",
            RpcMetricMode::Client,
            [
                "/rust_zero.echo.Echo/Echo",
                "/rust_zero.echo.Echo/ServerStream",
                "/rust_zero.echo.Echo/ClientStream",
                "/rust_zero.echo.Echo/BidirectionalStream",
            ],
        )
        .unwrap();
        let client_stack = RpcClientStackBuilder::new(client_metrics)
            .with_bearer_token(BearerToken::new("secret").unwrap())
            .with_default_timeout(Duration::from_secs(2))
            .with_circuit_breaker(rust_zero_core::CircuitBreakerConfig::new(
                1,
                Duration::from_secs(30),
            ))
            .build();
        let mut client = EchoClient::new(tower::Layer::layer(&client_stack, channel));

        let client_stream =
            tokio_stream::iter(["one", "two", "three"].map(|message| EchoRequest {
                message: message.to_owned(),
            }));
        assert_eq!(
            client
                .client_stream(client_stream)
                .await
                .unwrap()
                .into_inner()
                .message,
            "one,two,three"
        );

        let bidi_input = tokio_stream::iter(["left", "right"].map(|message| EchoRequest {
            message: message.to_owned(),
        }));
        let mut bidi = client
            .bidirectional_stream(bidi_input)
            .await
            .unwrap()
            .into_inner();
        assert_eq!(bidi.message().await.unwrap().unwrap().message, "left");
        assert_eq!(bidi.message().await.unwrap().unwrap().message, "right");
        assert!(bidi.message().await.unwrap().is_none());

        let mut cancelled = client
            .server_stream(EchoRequest {
                message: "cancel".to_owned(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(cancelled.message().await.unwrap().unwrap().message, "first");
        drop(cancelled);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let metrics = registry.render();
                if metrics.contains(
                    "stacked_rpc_client_requests_total{method=\"/rust_zero.echo.Echo/ServerStream\",code=\"cancelled\"} 1",
                ) && metrics.contains(
                    "stacked_rpc_server_requests_total{method=\"/rust_zero.echo.Echo/ServerStream\",code=\"cancelled\"} 1",
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping a response stream should record client cancellation");

        let mut failed = client
            .server_stream(EchoRequest {
                message: "status".to_owned(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(failed.message().await.unwrap().unwrap().message, "first");
        assert_eq!(
            failed.message().await.unwrap_err().code(),
            tonic::Code::Unavailable
        );
        let rejected = client
            .echo(EchoRequest {
                message: "circuit-open".to_owned(),
            })
            .await
            .unwrap_err();
        assert_eq!(rejected.code(), tonic::Code::Unavailable);

        assert!(registry.render().contains(
            "stacked_rpc_server_requests_total{method=\"/rust_zero.echo.Echo/Echo\",code=\"0\"} 2"
        ));
        assert!(registry.render().contains(
            "stacked_rpc_client_requests_total{method=\"/rust_zero.echo.Echo/ServerStream\",code=\"14\"} 1"
        ));
        assert!(registry.render().contains(
            "stacked_rpc_server_requests_total{method=\"/rust_zero.echo.Echo/ServerStream\",code=\"14\"} 1"
        ));
        server.abort();
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

    #[tokio::test]
    async fn generic_discovery_recovers_from_empty_and_malformed_snapshots() {
        let first_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first_address = first_listener.local_addr().unwrap();
        let first_server = tokio::spawn(async move {
            Server::builder()
                .add_service(EchoServer::new(NamedEchoService("first")))
                .serve_with_incoming(TcpListenerStream::new(first_listener))
                .await
                .unwrap();
        });
        let second_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second_address = second_listener.local_addr().unwrap();
        let second_server = tokio::spawn(async move {
            Server::builder()
                .add_service(EchoServer::new(NamedEchoService("second")))
                .serve_with_incoming(TcpListenerStream::new(second_listener))
                .await
                .unwrap();
        });

        let (updates, receiver) = watch::channel(Vec::new());
        let channel = RpcClient::new(RpcClientConfig::new("http://unused")).connect_discovered(
            TestSubscription {
                receiver,
                dropped: None,
            },
        );
        updates.send_replace(vec![
            "not a URI".to_owned(),
            format!("http://{first_address}"),
        ]);
        let response = EchoClient::new(channel.clone())
            .echo(Request::new(EchoRequest::default()))
            .await
            .unwrap();
        assert_eq!(response.into_inner().message, "first");

        updates.send_replace(vec![format!("http://{second_address}")]);
        let message = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let result = EchoClient::new(channel.clone())
                    .echo(Request::new(EchoRequest::default()))
                    .await;
                if let Ok(response) = result {
                    if response.get_ref().message == "second" {
                        break response.into_inner().message;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(message, "second");

        drop(channel);
        first_server.abort();
        second_server.abort();
    }

    #[tokio::test]
    async fn discovery_watcher_stops_when_channel_is_dropped() {
        let (_updates, receiver) = watch::channel(Vec::new());
        let (dropped, stopped) = oneshot::channel();
        let channel = RpcClient::new(RpcClientConfig::new("http://unused")).connect_discovered(
            TestSubscription {
                receiver,
                dropped: Some(dropped),
            },
        );

        drop(channel);
        tokio::time::timeout(Duration::from_secs(1), stopped)
            .await
            .expect("discovery watcher should stop")
            .expect("drop notification should be delivered");
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
