//! gRPC transport primitives for rust-zero services.
//!
//! Service implementations remain ordinary Tonic services. [`RpcServer`] and [`RpcClient`]
//! centralize the transport safeguards that should be applied consistently across services.

use std::{
    collections::{hash_map::RandomState, BTreeMap, BTreeSet},
    error::Error as StdError,
    fmt,
    future::Future,
    hash::{BuildHasher, Hasher},
    net::SocketAddr,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use rust_zero_core::{
    DiscoveredEndpoint, DiscoveryError, EndpointSubscription, EtcdClient, EtcdConfig, EtcdError,
    EtcdTlsConfig, HealthRegistry, ServiceRegistry,
};
use serde::{Deserialize, Serialize};
use tonic::transport::{
    Certificate, Channel, ClientTlsConfig, Endpoint, Identity as TonicIdentity, Server,
    ServerTlsConfig,
};
use tower::discover::Change;
use tower::layer::util::{Identity as TowerIdentity, Stack};

pub mod auth;
pub mod metrics;
pub mod resilience;
pub mod stack;
pub mod timeout;
pub mod trace;

pub mod echo {
    tonic::include_proto!("rust_zero.echo");
}

pub use auth::{BearerToken, RpcBearerAuth, RpcJwtAuth, RpcRequestSignatureAuth, RpcRequestSigner};
pub use metrics::{RpcMetricMode, RpcMetrics, RpcMetricsLayer};
pub use resilience::{acceptable_status, circuit_outcome, RpcCircuitBreaker, RpcLoadShedder};
pub use rust_zero_core::{AuthFailure, JwtClaimProjection, RequestSignatureVerifier};
pub use stack::{
    RpcClientStack, RpcClientStackBuilder, RpcClientStackService, RpcServerStack,
    RpcServerStackBuilder,
};
pub use timeout::{RpcServerTimeoutLayer, RpcServerTimeoutService};
pub use tonic_health::server::{health_reporter, HealthReporter};
pub use trace::RpcTrace;
#[cfg(feature = "telemetry")]
pub use trace::{RpcTelemetryLayer, RpcTelemetryMode};

/// PEM material used by a gRPC server, with optional client-certificate verification.
#[derive(Clone, Serialize, Deserialize)]
pub struct RpcServerTlsConfig {
    pub certificate_pem: String,
    pub private_key_pem: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ca_pem: Option<String>,
}

impl fmt::Debug for RpcServerTlsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpcServerTlsConfig")
            .field("certificate_pem", &"[PEM]")
            .field("private_key_pem", &"[REDACTED]")
            .field(
                "client_ca_pem",
                &self.client_ca_pem.as_ref().map(|_| "[PEM]"),
            )
            .finish()
    }
}

impl RpcServerTlsConfig {
    pub fn new(certificate_pem: impl Into<String>, private_key_pem: impl Into<String>) -> Self {
        Self {
            certificate_pem: certificate_pem.into(),
            private_key_pem: private_key_pem.into(),
            client_ca_pem: None,
        }
    }

    pub fn with_client_ca(mut self, client_ca_pem: impl Into<String>) -> Self {
        self.client_ca_pem = Some(client_ca_pem.into());
        self
    }

    fn validate(&self) -> Result<(), RpcConfigError> {
        validate_pem(
            &self.certificate_pem,
            "server TLS certificate must not be empty",
        )?;
        validate_pem(
            &self.private_key_pem,
            "server TLS private key must not be empty",
        )?;
        if let Some(ca) = &self.client_ca_pem {
            validate_pem(ca, "server TLS client CA must not be empty")?;
        }
        Ok(())
    }

    fn tonic_config(&self) -> ServerTlsConfig {
        let mut tls = ServerTlsConfig::new().identity(TonicIdentity::from_pem(
            self.certificate_pem.clone(),
            self.private_key_pem.clone(),
        ));
        if let Some(ca) = &self.client_ca_pem {
            tls = tls.client_ca_root(Certificate::from_pem(ca.clone()));
        }
        tls
    }
}

/// Trust and optional client identity used by direct and discovered gRPC channels.
#[derive(Clone, Serialize, Deserialize)]
pub struct RpcClientTlsConfig {
    pub ca_certificate_pem: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain_name: Option<String>,
}

impl fmt::Debug for RpcClientTlsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpcClientTlsConfig")
            .field("ca_certificate_pem", &"[PEM]")
            .field(
                "certificate_pem",
                &self.certificate_pem.as_ref().map(|_| "[PEM]"),
            )
            .field(
                "private_key_pem",
                &self.private_key_pem.as_ref().map(|_| "[REDACTED]"),
            )
            .field("domain_name", &self.domain_name)
            .finish()
    }
}

impl RpcClientTlsConfig {
    pub fn new(ca_certificate_pem: impl Into<String>) -> Self {
        Self {
            ca_certificate_pem: ca_certificate_pem.into(),
            certificate_pem: None,
            private_key_pem: None,
            domain_name: None,
        }
    }

    pub fn with_identity(
        mut self,
        certificate_pem: impl Into<String>,
        private_key_pem: impl Into<String>,
    ) -> Self {
        self.certificate_pem = Some(certificate_pem.into());
        self.private_key_pem = Some(private_key_pem.into());
        self
    }

    pub fn with_domain_name(mut self, domain_name: impl Into<String>) -> Self {
        self.domain_name = Some(domain_name.into());
        self
    }

    fn validate(&self) -> Result<(), RpcConfigError> {
        validate_pem(
            &self.ca_certificate_pem,
            "client TLS CA certificate must not be empty",
        )?;
        if self.certificate_pem.is_some() != self.private_key_pem.is_some() {
            return Err(RpcConfigError::Invalid(
                "client TLS certificate and private key must be configured together",
            ));
        }
        if let Some(certificate) = &self.certificate_pem {
            validate_pem(certificate, "client TLS certificate must not be empty")?;
        }
        if let Some(key) = &self.private_key_pem {
            validate_pem(key, "client TLS private key must not be empty")?;
        }
        if self
            .domain_name
            .as_ref()
            .is_some_and(|name| name.trim().is_empty())
        {
            return Err(RpcConfigError::Invalid(
                "client TLS domain name must not be empty",
            ));
        }
        Ok(())
    }

    fn tonic_config(&self) -> ClientTlsConfig {
        let mut tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(self.ca_certificate_pem.clone()));
        if let Some(domain_name) = &self.domain_name {
            tls = tls.domain_name(domain_name.clone());
        }
        if let (Some(certificate), Some(key)) = (&self.certificate_pem, &self.private_key_pem) {
            tls = tls.identity(TonicIdentity::from_pem(certificate.clone(), key.clone()));
        }
        tls
    }
}

fn validate_pem(value: &str, message: &'static str) -> Result<(), RpcConfigError> {
    if value.trim().is_empty() {
        Err(RpcConfigError::Invalid(message))
    } else {
        Ok(())
    }
}

/// Etcd connection and lease settings used to publish a running gRPC server.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RpcEtcdRegistrationConfig {
    pub endpoints: Vec<String>,
    pub namespace: String,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(rename = "connect_timeout_ms", with = "duration_millis")]
    pub connect_timeout: Duration,
    pub tls: Option<EtcdTlsConfig>,
    pub service: String,
    pub instance: String,
    pub endpoint: String,
    #[serde(rename = "lease_ttl_ms", with = "duration_millis")]
    pub lease_ttl: Duration,
}

impl fmt::Debug for RpcEtcdRegistrationConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpcEtcdRegistrationConfig")
            .field("endpoints", &self.endpoints)
            .field("namespace", &self.namespace)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("connect_timeout", &self.connect_timeout)
            .field("tls", &self.tls)
            .field("service", &self.service)
            .field("instance", &self.instance)
            .field("endpoint", &self.endpoint)
            .field("lease_ttl", &self.lease_ttl)
            .finish()
    }
}

impl Default for RpcEtcdRegistrationConfig {
    fn default() -> Self {
        Self {
            endpoints: Vec::new(),
            namespace: "/rust-zero".to_owned(),
            username: None,
            password: None,
            connect_timeout: Duration::from_secs(10),
            tls: None,
            service: String::new(),
            instance: String::new(),
            endpoint: String::new(),
            lease_ttl: Duration::from_secs(10),
        }
    }
}

impl RpcEtcdRegistrationConfig {
    pub fn new(
        endpoints: impl IntoIterator<Item = impl Into<String>>,
        service: impl Into<String>,
        instance: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Self {
        Self {
            endpoints: endpoints.into_iter().map(Into::into).collect(),
            service: service.into(),
            instance: instance.into(),
            endpoint: endpoint.into(),
            ..Self::default()
        }
    }

    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = namespace.into();
        self
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        assert!(!timeout.is_zero(), "etcd connect timeout must be positive");
        self.connect_timeout = timeout;
        self
    }

    pub fn with_tls(mut self, tls: EtcdTlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    pub fn with_lease_ttl(mut self, ttl: Duration) -> Self {
        assert!(
            !ttl.is_zero(),
            "etcd registration lease TTL must be positive"
        );
        self.lease_ttl = ttl;
        self
    }

    fn validate(&self) -> Result<(), RpcConfigError> {
        if self.endpoints.is_empty()
            || self
                .endpoints
                .iter()
                .any(|endpoint| endpoint.trim().is_empty())
        {
            return Err(RpcConfigError::Invalid(
                "etcd registration requires non-empty etcd endpoints",
            ));
        }
        validate_rpc_name(
            &self.service,
            "etcd registration service must be a non-empty name",
        )?;
        validate_rpc_name(
            &self.instance,
            "etcd registration instance must be a non-empty name",
        )?;
        if self.endpoint.trim().is_empty() {
            return Err(RpcConfigError::Invalid(
                "etcd registration endpoint must not be empty",
            ));
        }
        if self.connect_timeout.is_zero() {
            return Err(RpcConfigError::Invalid(
                "etcd connect timeout must be greater than zero",
            ));
        }
        if self.lease_ttl.is_zero() {
            return Err(RpcConfigError::Invalid(
                "etcd registration lease TTL must be greater than zero",
            ));
        }
        if self.username.is_some() != self.password.is_some() {
            return Err(RpcConfigError::Invalid(
                "etcd registration username and password must be configured together",
            ));
        }
        let namespace = self.namespace.trim_matches('/');
        if namespace.is_empty() || namespace.contains('/') {
            return Err(RpcConfigError::Invalid(
                "etcd registration namespace must be a non-empty single path segment",
            ));
        }
        if let Some(tls) = &self.tls {
            if tls.ca_certificate_pem.trim().is_empty() {
                return Err(RpcConfigError::Invalid(
                    "etcd TLS CA certificate must not be empty",
                ));
            }
            if tls.certificate_pem.is_some() != tls.private_key_pem.is_some() {
                return Err(RpcConfigError::Invalid(
                    "etcd TLS certificate and private key must be configured together",
                ));
            }
            if tls
                .certificate_pem
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err(RpcConfigError::Invalid(
                    "etcd TLS client certificate must not be empty",
                ));
            }
            if tls
                .private_key_pem
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err(RpcConfigError::Invalid(
                    "etcd TLS private key must not be empty",
                ));
            }
            if tls
                .domain_name
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err(RpcConfigError::Invalid(
                    "etcd TLS domain name must not be empty",
                ));
            }
        }
        let uri = self.endpoint.parse::<http::Uri>().map_err(|_| {
            RpcConfigError::Invalid("etcd registration endpoint must be an absolute URI")
        })?;
        if uri.scheme().is_none() || uri.authority().is_none() {
            return Err(RpcConfigError::Invalid(
                "etcd registration endpoint must be an absolute URI",
            ));
        }
        Ok(())
    }

    fn etcd_config(&self) -> EtcdConfig {
        let mut config = EtcdConfig::new(self.endpoints.clone())
            .with_namespace(&self.namespace)
            .with_timeout(self.connect_timeout);
        if let (Some(username), Some(password)) = (&self.username, &self.password) {
            config = config.with_credentials(username, password);
        }
        if let Some(tls) = &self.tls {
            config = config.with_tls(tls.clone());
        }
        config
    }
}

fn validate_rpc_name(value: &str, message: &'static str) -> Result<(), RpcConfigError> {
    if value.trim().is_empty() || value.contains('/') {
        Err(RpcConfigError::Invalid(message))
    } else {
        Ok(())
    }
}

/// Transport settings applied to every gRPC service on a server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RpcServerConfig {
    address: SocketAddr,
    #[serde(rename = "request_timeout_ms", with = "optional_duration_millis")]
    request_timeout: Option<Duration>,
    #[serde(rename = "method_timeouts_ms", with = "duration_map_millis")]
    method_timeouts: BTreeMap<String, Duration>,
    #[serde(rename = "service_timeouts_ms", with = "duration_map_millis")]
    service_timeouts: BTreeMap<String, Duration>,
    concurrency_limit: Option<usize>,
    max_concurrent_streams: Option<u32>,
    #[serde(rename = "shutdown_timeout_ms", with = "duration_millis")]
    shutdown_timeout: Duration,
    tls: Option<RpcServerTlsConfig>,
    etcd_registration: Option<RpcEtcdRegistrationConfig>,
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
            method_timeouts: BTreeMap::new(),
            service_timeouts: BTreeMap::new(),
            concurrency_limit: None,
            max_concurrent_streams: None,
            shutdown_timeout: Duration::from_secs(30),
            tls: None,
            etcd_registration: None,
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

    /// Sets the timeout for one canonical gRPC path, such as `/pkg.Service/Method`.
    pub fn with_method_timeout(mut self, method: impl Into<String>, timeout: Duration) -> Self {
        assert!(
            !timeout.is_zero(),
            "method timeout must be greater than zero"
        );
        let method = normalize_method(method.into())
            .expect("gRPC method must have the form /package.Service/Method");
        self.method_timeouts.insert(method, timeout);
        self
    }

    /// Sets the fallback for every method of a service. Exact method settings take precedence.
    pub fn with_service_timeout(mut self, service: impl Into<String>, timeout: Duration) -> Self {
        assert!(
            !timeout.is_zero(),
            "service timeout must be greater than zero"
        );
        let service = normalize_service(service.into())
            .expect("gRPC service must have the form package.Service or /package.Service");
        self.service_timeouts.insert(service, timeout);
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

    pub fn with_tls(mut self, tls: RpcServerTlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    pub fn with_etcd_registration(mut self, registration: RpcEtcdRegistrationConfig) -> Self {
        self.etcd_registration = Some(registration);
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
        for (method, timeout) in &self.method_timeouts {
            if timeout.is_zero() || normalize_method(method.clone()).as_deref() != Some(method) {
                return Err(RpcConfigError::Invalid(
                    "gRPC method timeout keys must have the form /package.Service/Method and positive values",
                ));
            }
        }
        for (service, timeout) in &self.service_timeouts {
            if timeout.is_zero() || normalize_service(service.clone()).as_deref() != Some(service) {
                return Err(RpcConfigError::Invalid(
                    "gRPC service timeout keys must have the form /package.Service and positive values",
                ));
            }
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
        if let Some(tls) = &self.tls {
            tls.validate()?;
        }
        if let Some(registration) = &self.etcd_registration {
            registration.validate()?;
        }
        Ok(())
    }
}

fn normalize_method(mut method: String) -> Option<String> {
    if !method.starts_with('/') {
        method.insert(0, '/');
    }
    let mut parts = method.split('/');
    if parts.next() != Some("") {
        return None;
    }
    let service = parts.next()?;
    let rpc = parts.next()?;
    if service.is_empty() || rpc.is_empty() || parts.next().is_some() {
        None
    } else {
        Some(method)
    }
}

fn normalize_service(mut service: String) -> Option<String> {
    if !service.starts_with('/') {
        service.insert(0, '/');
    }
    if service.len() > 1 && !service[1..].contains('/') {
        Some(service)
    } else {
        None
    }
}

/// Builds configured Tonic server routers.
#[derive(Debug, Clone)]
pub struct RpcServer {
    config: RpcServerConfig,
}

/// Concrete Tonic builder returned by [`RpcServer::router`].
pub type RpcRouterBuilder = Server<Stack<RpcServerTimeoutLayer, TowerIdentity>>;

/// Tonic router assembled from [`RpcRouterBuilder`].
pub type RpcRouter = tonic::transport::server::Router<Stack<RpcServerTimeoutLayer, TowerIdentity>>;

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
    pub fn router(&self) -> RpcRouterBuilder {
        self.try_router()
            .expect("validated gRPC server TLS configuration")
    }

    /// Returns a configured Tonic builder and reports malformed TLS PEM material.
    pub fn try_router(&self) -> Result<RpcRouterBuilder, RpcServerError> {
        self.config
            .validate()
            .map_err(RpcServerError::Configuration)?;
        let mut server = Server::builder();

        if let Some(limit) = self.config.concurrency_limit {
            server = server.concurrency_limit_per_connection(limit);
        }
        if let Some(limit) = self.config.max_concurrent_streams {
            server = server.max_concurrent_streams(Some(limit));
        }

        if let Some(tls) = &self.config.tls {
            server = server
                .tls_config(tls.tonic_config())
                .map_err(RpcServerError::Transport)?;
        }

        Ok(server.layer(RpcServerTimeoutLayer::new(
            self.config.request_timeout,
            self.config.method_timeouts.clone(),
            self.config.service_timeouts.clone(),
        )))
    }

    /// Serves an assembled Tonic router and bounds draining after the shutdown signal fires.
    pub async fn serve_with_shutdown<F>(
        &self,
        router: RpcRouter,
        signal: F,
    ) -> Result<(), RpcServerError>
    where
        F: Future<Output = ()>,
    {
        self.config
            .validate()
            .map_err(RpcServerError::Configuration)?;
        let mut lease = if let Some(registration) = &self.config.etcd_registration {
            let client = EtcdClient::connect(registration.etcd_config())
                .await
                .map_err(RpcServerError::Registration)?;
            Some(
                client
                    .publish(
                        &registration.service,
                        &registration.instance,
                        registration.endpoint.clone(),
                        registration.lease_ttl,
                    )
                    .await
                    .map_err(RpcServerError::Registration)?,
            )
        } else {
            None
        };
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let serving = router.serve_with_shutdown(self.config.address, async move {
            let _ = stopped.await;
        });
        tokio::pin!(serving);
        tokio::pin!(signal);

        let serving_result = tokio::select! {
            result = &mut serving => result.map_err(RpcServerError::Transport),
            _ = &mut signal => {
                let _ = stop.send(());
                tokio::time::timeout(self.config.shutdown_timeout, serving)
                    .await
                    .map_err(|_| RpcServerError::ShutdownTimeout)?
                    .map_err(RpcServerError::Transport)
            }
        };
        let revoke_result = match lease.take() {
            Some(lease) => lease.revoke().await.map_err(RpcServerError::Registration),
            None => Ok(()),
        };
        serving_result.and(revoke_result)
    }
}

#[derive(Debug)]
pub enum RpcServerError {
    Configuration(RpcConfigError),
    Transport(tonic::transport::Error),
    Registration(EtcdError),
    ShutdownTimeout,
}

impl fmt::Display for RpcServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(error) => write!(formatter, "invalid gRPC configuration: {error}"),
            Self::Transport(error) => write!(formatter, "gRPC server transport error: {error}"),
            Self::Registration(error) => {
                write!(formatter, "gRPC etcd registration failed: {error}")
            }
            Self::ShutdownTimeout => formatter.write_str("gRPC graceful shutdown timed out"),
        }
    }
}

impl StdError for RpcServerError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Configuration(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::Registration(error) => Some(error),
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
    #[serde(
        rename = "discovery_health_interval_ms",
        with = "optional_duration_millis"
    )]
    discovery_health_interval: Option<Duration>,
    #[serde(
        rename = "discovery_health_timeout_ms",
        with = "optional_duration_millis"
    )]
    discovery_health_timeout: Option<Duration>,
    /// Maximum number of unique discovery endpoints connected by one client.
    /// `None` preserves the opt-out behavior and connects to every valid endpoint.
    discovery_subset_size: Option<usize>,
    /// Optional stable seed for repeatable subset membership across client restarts.
    discovery_subset_seed: Option<u64>,
    tls: Option<RpcClientTlsConfig>,
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
            discovery_health_interval: None,
            discovery_health_timeout: None,
            discovery_subset_size: None,
            discovery_subset_seed: None,
            tls: None,
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

    /// Enables periodic HTTP/2 connection probes for discovered endpoints.
    pub fn with_discovery_health_check(mut self, interval: Duration, timeout: Duration) -> Self {
        assert!(
            !interval.is_zero(),
            "discovery health interval must be positive"
        );
        assert!(
            !timeout.is_zero(),
            "discovery health timeout must be positive"
        );
        self.discovery_health_interval = Some(interval);
        self.discovery_health_timeout = Some(timeout);
        self
    }

    /// Limits a discovery-backed channel to a randomized, low-churn endpoint subset.
    ///
    /// Membership is stable for the lifetime of the client and across snapshot reordering. Use
    /// [`Self::with_discovery_subset_seed`] when membership must also survive process restarts.
    pub fn with_discovery_subset(mut self, size: usize) -> Self {
        assert!(size > 0, "discovery subset size must be greater than zero");
        self.discovery_subset_size = Some(size);
        self
    }

    /// Sets a repeatable seed for discovery subsetting across client restarts.
    pub fn with_discovery_subset_seed(mut self, seed: u64) -> Self {
        self.discovery_subset_seed = Some(seed);
        self
    }

    pub fn with_tls(mut self, tls: RpcClientTlsConfig) -> Self {
        self.tls = Some(tls);
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
            ("discovery health interval", self.discovery_health_interval),
            ("discovery health timeout", self.discovery_health_timeout),
        ] {
            if duration.is_some_and(|duration| duration.is_zero()) {
                return Err(RpcConfigError::Invalid(match name {
                    "request timeout" => "request timeout must be greater than zero",
                    "connect timeout" => "connect timeout must be greater than zero",
                    "TCP keepalive interval" => "TCP keepalive interval must be greater than zero",
                    "HTTP/2 keepalive interval" => {
                        "HTTP/2 keepalive interval must be greater than zero"
                    }
                    "discovery health interval" => {
                        "discovery health interval must be greater than zero"
                    }
                    "discovery health timeout" => {
                        "discovery health timeout must be greater than zero"
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
        if self.discovery_subset_size == Some(0) {
            return Err(RpcConfigError::Invalid(
                "discovery subset size must be greater than zero",
            ));
        }
        if self.http2_keepalive_interval.is_some() != self.keepalive_timeout.is_some() {
            return Err(RpcConfigError::Invalid(
                "HTTP/2 keepalive interval and timeout must be configured together",
            ));
        }
        if self.discovery_health_interval.is_some() != self.discovery_health_timeout.is_some() {
            return Err(RpcConfigError::Invalid(
                "discovery health interval and timeout must be configured together",
            ));
        }
        if let Some(tls) = &self.tls {
            tls.validate()?;
        }
        Ok(())
    }
}

/// Aggregate availability exposed by a discovered RPC channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryReadiness {
    Empty,
    Ready,
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryStatusSnapshot {
    pub readiness: DiscoveryReadiness,
    /// Endpoints in the complete backend snapshot, including malformed entries.
    pub discovered: usize,
    /// Valid endpoints selected for this client's balanced channel.
    pub selected: usize,
    pub available: usize,
    pub rejected: usize,
}

impl DiscoveryStatusSnapshot {
    pub fn is_ready(self) -> bool {
        self.readiness == DiscoveryReadiness::Ready
    }
}

/// Watchable readiness for a channel created from service discovery.
#[derive(Debug, Clone)]
pub struct DiscoveryStatus {
    receiver: tokio::sync::watch::Receiver<DiscoveryStatusSnapshot>,
}

impl DiscoveryStatus {
    pub fn snapshot(&self) -> DiscoveryStatusSnapshot {
        *self.receiver.borrow()
    }

    pub async fn changed(
        &mut self,
    ) -> Result<DiscoveryStatusSnapshot, tokio::sync::watch::error::RecvError> {
        self.receiver.changed().await?;
        Ok(self.snapshot())
    }

    /// Projects channel readiness into the shared HTTP/dev-server health aggregate.
    pub fn project_to_health(
        mut self,
        registry: HealthRegistry,
        dependency: impl Into<String>,
    ) -> tokio::task::JoinHandle<()> {
        let dependency = dependency.into();
        tokio::spawn(async move {
            registry.set(&dependency, self.snapshot().is_ready());
            while self.receiver.changed().await.is_ok() {
                registry.set(&dependency, self.snapshot().is_ready());
            }
            registry.set(dependency, false);
        })
    }

    /// Projects channel readiness into a standard gRPC health service entry.
    pub fn project_to_grpc_health(
        mut self,
        mut reporter: HealthReporter,
        service_name: impl Into<String>,
    ) -> tokio::task::JoinHandle<()> {
        let service_name = service_name.into();
        tokio::spawn(async move {
            loop {
                let serving = if self.snapshot().is_ready() {
                    tonic_health::ServingStatus::Serving
                } else {
                    tonic_health::ServingStatus::NotServing
                };
                reporter.set_service_status(&service_name, serving).await;
                if self.receiver.changed().await.is_err() {
                    reporter
                        .set_service_status(&service_name, tonic_health::ServingStatus::NotServing)
                        .await;
                    return;
                }
            }
        })
    }
}

/// Connects gRPC clients using a common set of deadline and concurrency safeguards.
#[derive(Debug, Clone)]
pub struct RpcClient {
    config: RpcClientConfig,
    discovery_subset_seed: u64,
}

impl RpcClient {
    pub fn new(config: RpcClientConfig) -> Self {
        let discovery_subset_seed = config
            .discovery_subset_seed
            .unwrap_or_else(random_discovery_subset_seed);
        Self {
            config,
            discovery_subset_seed,
        }
    }

    pub fn try_new(config: RpcClientConfig) -> Result<Self, RpcConfigError> {
        config.validate()?;
        Ok(Self::new(config))
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
    pub fn connect_discovered<S>(&self, subscription: S) -> Channel
    where
        S: EndpointSubscription,
    {
        self.connect_discovered_with_status(subscription).0
    }

    /// Creates a weighted discovery channel and a watchable readiness handle.
    ///
    /// Invalid endpoint URIs are excluded and make the snapshot degraded. When active health
    /// checking is configured, failed probes are removed until a later probe succeeds.
    pub fn connect_discovered_with_status<S>(
        &self,
        mut subscription: S,
    ) -> (Channel, DiscoveryStatus)
    where
        S: EndpointSubscription,
    {
        let initial = subscription.discovered_endpoints();
        let (configured, discovered, rejected) = self.configure_discovered(initial);

        let capacity = configured
            .values()
            .map(|(_, endpoint)| endpoint.weight() as usize)
            .sum::<usize>()
            .max(128);
        let (channel, changes) = Channel::balance_channel(capacity);
        let mut installed = BTreeSet::new();
        for (uri, (endpoint, discovered)) in &configured {
            for slot in 0..discovered.weight() {
                let key = weighted_key(uri, slot);
                installed.insert(key.clone());
                changes
                    .try_send(Change::Insert(key, endpoint.clone()))
                    .expect(
                        "discovery channel is sized for its weighted initial endpoint snapshot",
                    );
            }
        }
        let initial_status =
            discovery_status(discovered, configured.len(), configured.len(), rejected);
        let (status_updates, status_receiver) = tokio::sync::watch::channel(initial_status);

        let client = self.clone();
        tokio::spawn(async move {
            let mut configured = configured;
            let mut discovered = discovered;
            let mut available: BTreeSet<String> = configured.keys().cloned().collect();
            let mut rejected = rejected;
            let mut health_ticks = client.discovery_health_ticks();
            loop {
                tokio::select! {
                    _ = changes.closed() => return,
                    snapshot = subscription.changed() => {
                        if snapshot.is_err() {
                            let mut closed = discovery_status(
                                discovered,
                                configured.len(),
                                0,
                                rejected,
                            );
                            closed.readiness = DiscoveryReadiness::Degraded;
                            status_updates.send_replace(closed);
                            return;
                        }
                        let (next, next_discovered, next_rejected) =
                            client.configure_discovered(subscription.discovered_endpoints());
                        configured = next;
                        discovered = next_discovered;
                        rejected = next_rejected;
                        available.retain(|uri| configured.contains_key(uri));
                        available.extend(configured.keys().cloned());
                    }
                    _ = health_ticks.tick(), if client.config.discovery_health_interval.is_some() => {
                        available = client.probe_discovered(&configured).await;
                    }
                }

                let desired = weighted_keys(&configured, &available);
                for key in installed.difference(&desired).cloned().collect::<Vec<_>>() {
                    if changes.send(Change::Remove(key.clone())).await.is_err() {
                        return;
                    }
                    installed.remove(&key);
                }
                for key in desired.difference(&installed).cloned().collect::<Vec<_>>() {
                    let Some((uri, _)) = key.rsplit_once('\0') else {
                        continue;
                    };
                    let Some((endpoint, _)) = configured.get(uri) else {
                        continue;
                    };
                    if changes
                        .send(Change::Insert(key.clone(), endpoint.clone()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    installed.insert(key);
                }
                status_updates.send_replace(discovery_status(
                    discovered,
                    configured.len(),
                    available.len(),
                    rejected,
                ));
            }
        });

        (
            channel,
            DiscoveryStatus {
                receiver: status_receiver,
            },
        )
    }

    fn configure_discovered(
        &self,
        endpoints: Vec<DiscoveredEndpoint>,
    ) -> (
        BTreeMap<String, (Endpoint, DiscoveredEndpoint)>,
        usize,
        usize,
    ) {
        let discovered = endpoints.len();
        let mut configured: BTreeMap<_, _> = endpoints
            .into_iter()
            .filter_map(|discovered| {
                self.endpoint(discovered.uri().to_owned())
                    .ok()
                    .map(|endpoint| (discovered.uri().to_owned(), (endpoint, discovered)))
            })
            .collect();
        let rejected = discovered.saturating_sub(configured.len());
        if let Some(limit) = self.config.discovery_subset_size {
            configured = select_discovery_subset(configured, limit, self.discovery_subset_seed);
        }
        (configured, discovered, rejected)
    }

    fn discovery_health_ticks(&self) -> tokio::time::Interval {
        let interval = self
            .config
            .discovery_health_interval
            .unwrap_or(Duration::from_secs(86_400));
        let mut ticks = tokio::time::interval(interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticks
    }

    async fn probe_discovered(
        &self,
        configured: &BTreeMap<String, (Endpoint, DiscoveredEndpoint)>,
    ) -> BTreeSet<String> {
        let timeout = self
            .config
            .discovery_health_timeout
            .expect("probe timeout configured");
        let probes = configured.iter().map(|(uri, (endpoint, _))| {
            let uri = uri.clone();
            let endpoint = endpoint.clone();
            async move {
                let healthy = tokio::time::timeout(timeout, endpoint.connect())
                    .await
                    .is_ok_and(|result| result.is_ok());
                (uri, healthy)
            }
        });
        futures::future::join_all(probes)
            .await
            .into_iter()
            .filter_map(|(uri, healthy)| healthy.then_some(uri))
            .collect()
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

        if let Some(tls) = &self.config.tls {
            endpoint = endpoint
                .tls_config(tls.tonic_config())
                .map_err(RpcClientError::Transport)?;
        }

        Ok(endpoint)
    }
}

fn weighted_key(uri: &str, slot: u32) -> String {
    format!("{uri}\0{slot}")
}

static DISCOVERY_SUBSET_SEED_COUNTER: AtomicU64 = AtomicU64::new(0);

fn random_discovery_subset_seed() -> u64 {
    let state = RandomState::new();
    let mut hasher = state.build_hasher();
    hasher.write_u64(DISCOVERY_SUBSET_SEED_COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.finish()
}

fn discovery_subset_score(uri: &str, seed: u64) -> u64 {
    // FNV-1a followed by SplitMix64 finalization gives deterministic, well-distributed rendezvous
    // scores without adding a random-number dependency to the transport crate.
    let mut score = 0xcbf29ce484222325_u64 ^ seed;
    for byte in uri.as_bytes() {
        score ^= u64::from(*byte);
        score = score.wrapping_mul(0x100000001b3);
    }
    score = (score ^ (score >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    score = (score ^ (score >> 27)).wrapping_mul(0x94d049bb133111eb);
    score ^ (score >> 31)
}

fn select_discovery_subset(
    configured: BTreeMap<String, (Endpoint, DiscoveredEndpoint)>,
    limit: usize,
    seed: u64,
) -> BTreeMap<String, (Endpoint, DiscoveredEndpoint)> {
    if configured.len() <= limit {
        return configured;
    }
    let mut ranked: Vec<_> = configured
        .keys()
        .map(|uri| (discovery_subset_score(uri, seed), uri.clone()))
        .collect();
    ranked.sort_unstable_by(|left, right| right.cmp(left));
    let selected: BTreeSet<_> = ranked.into_iter().take(limit).map(|(_, uri)| uri).collect();
    configured
        .into_iter()
        .filter(|(uri, _)| selected.contains(uri))
        .collect()
}

fn weighted_keys(
    configured: &BTreeMap<String, (Endpoint, DiscoveredEndpoint)>,
    available: &BTreeSet<String>,
) -> BTreeSet<String> {
    configured
        .iter()
        .filter(|(uri, _)| available.contains(*uri))
        .flat_map(|(uri, (_, endpoint))| {
            (0..endpoint.weight()).map(move |slot| weighted_key(uri, slot))
        })
        .collect()
}

fn discovery_status(
    discovered: usize,
    selected: usize,
    available: usize,
    rejected: usize,
) -> DiscoveryStatusSnapshot {
    let readiness = if discovered == 0 {
        DiscoveryReadiness::Empty
    } else if selected > 0 && available == selected && rejected == 0 {
        DiscoveryReadiness::Ready
    } else {
        DiscoveryReadiness::Degraded
    };
    DiscoveryStatusSnapshot {
        readiness,
        discovered,
        selected,
        available,
        rejected,
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

mod duration_map_millis {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::{collections::BTreeMap, time::Duration};

    pub fn serialize<S>(
        values: &BTreeMap<String, Duration>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        values
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    value.as_millis().try_into().unwrap_or(u64::MAX),
                )
            })
            .collect::<BTreeMap<_, _>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<BTreeMap<String, Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        BTreeMap::<String, u64>::deserialize(deserializer).map(|values| {
            values
                .into_iter()
                .map(|(key, value)| (key, Duration::from_millis(value)))
                .collect()
        })
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
            "uri = \"http://127.0.0.1:50052\"\nconnect_timeout_ms = 250\ndiscovery_subset_size = 32\ndiscovery_subset_seed = 7",
            rust_zero_core::ConfigFormat::Toml,
        )
        .unwrap();
        assert_eq!(client.connect_timeout, Some(Duration::from_millis(250)));
        assert_eq!(client.discovery_subset_size, Some(32));
        assert_eq!(client.discovery_subset_seed, Some(7));
        client.validate().unwrap();
    }

    #[test]
    fn server_config_deserializes_and_matches_timeout_scopes() {
        let config: RpcServerConfig = rust_zero_core::parse_config(
            r#"
                address = "127.0.0.1:50052"
                request_timeout_ms = 30000
                method_timeouts_ms = { "/rust_zero.echo.Echo/Echo" = 100 }
                service_timeouts_ms = { "/rust_zero.echo.Echo" = 5000 }
            "#,
            rust_zero_core::ConfigFormat::Toml,
        )
        .unwrap();
        config.validate().unwrap();
        let policy = RpcServerTimeoutLayer::new(
            config.request_timeout,
            config.method_timeouts,
            config.service_timeouts,
        );

        assert_eq!(
            policy.timeout("/rust_zero.echo.Echo/Echo"),
            Some(Duration::from_millis(100))
        );
        assert_eq!(
            policy.timeout("/rust_zero.echo.Echo/ServerStream"),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            policy.timeout("/grpc.health.v1.Health/Check"),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn server_config_validates_etcd_registration_before_startup() {
        let valid = RpcEtcdRegistrationConfig::new(
            ["http://127.0.0.1:2379"],
            "echo",
            "echo-1",
            "http://127.0.0.1:50051",
        );
        RpcServerConfig::default()
            .with_etcd_registration(valid)
            .validate()
            .unwrap();

        let invalid = RpcEtcdRegistrationConfig::new(
            ["http://127.0.0.1:2379"],
            "echo",
            "echo/1",
            "not-a-uri",
        );
        assert!(RpcServerConfig::default()
            .with_etcd_registration(invalid)
            .validate()
            .is_err());
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

    #[test]
    fn tls_configs_reject_incomplete_identity_material() {
        let server = RpcServerConfig::default().with_tls(RpcServerTlsConfig::new("", "key"));
        assert!(server
            .validate()
            .unwrap_err()
            .to_string()
            .contains("certificate"));

        let client = RpcClientConfig::new("https://localhost:50051")
            .with_tls(RpcClientTlsConfig::new("ca").with_identity("certificate", ""));
        assert!(client
            .validate()
            .unwrap_err()
            .to_string()
            .contains("private key"));
    }

    #[tokio::test]
    async fn configured_server_and_client_complete_mutual_tls_call() {
        let (ca, certificate, private_key, client_certificate, client_key) = test_tls_material();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let (shutdown, stopped) = oneshot::channel();
        let server = RpcServer::new(
            RpcServerConfig::new(address).with_tls(
                RpcServerTlsConfig::new(certificate.clone(), private_key.clone())
                    .with_client_ca(ca.clone()),
            ),
        );
        let router = server
            .try_router()
            .unwrap()
            .add_service(EchoServer::new(EchoService));
        let task = tokio::spawn(async move {
            server
                .serve_with_shutdown(router, async {
                    let _ = stopped.await;
                })
                .await
        });

        let mut client = loop {
            let config = RpcClientConfig::new(format!("https://localhost:{}", address.port()))
                .with_connect_timeout(Duration::from_millis(100))
                .with_tls(
                    RpcClientTlsConfig::new(ca.clone())
                        .with_identity(client_certificate.clone(), client_key.clone())
                        .with_domain_name("localhost"),
                );
            match RpcClient::new(config).connect().await {
                Ok(channel) => break EchoClient::new(channel),
                Err(_) => tokio::task::yield_now().await,
            }
        };
        let response = client
            .echo(EchoRequest {
                message: "mutual tls".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(response.into_inner().message, "mutual tls");

        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
    }

    fn test_tls_material() -> (String, String, String, String, String) {
        use rcgen::{
            BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair,
            KeyUsagePurpose,
        };

        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
        let ca_key = KeyPair::generate().unwrap();
        let ca = ca_params.self_signed(&ca_key).unwrap();

        let server_key = KeyPair::generate().unwrap();
        let mut server_params = CertificateParams::new(["localhost".to_owned()]).unwrap();
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server = server_params.signed_by(&server_key, &ca, &ca_key).unwrap();

        let client_key = KeyPair::generate().unwrap();
        let mut client_params = CertificateParams::new(["rust-zero-client".to_owned()]).unwrap();
        client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let client = client_params.signed_by(&client_key, &ca, &ca_key).unwrap();

        (
            ca.pem(),
            server.pem(),
            server_key.serialize_pem(),
            client.pem(),
            client_key.serialize_pem(),
        )
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

    #[tokio::test]
    async fn configured_method_timeout_returns_deadline_exceeded() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let server = RpcServer::new(
            RpcServerConfig::new(address)
                .with_request_timeout(Duration::from_secs(1))
                .with_service_timeout("rust_zero.echo.Echo", Duration::from_millis(100))
                .with_method_timeout("rust_zero.echo.Echo/Echo", Duration::from_millis(10)),
        );
        let server_task = tokio::spawn(async move {
            server
                .router()
                .add_service(EchoServer::new(DrainEchoService { entered, release }))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let channel = RpcClient::new(
            RpcClientConfig::new(format!("http://{address}"))
                .with_connect_timeout(Duration::from_secs(1)),
        )
        .connect()
        .await
        .unwrap();

        let error = EchoClient::new(channel)
            .echo(EchoRequest {
                message: "slow".to_owned(),
            })
            .await
            .unwrap_err();

        assert_eq!(error.code(), tonic::Code::DeadlineExceeded);
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
        let channel = RpcClient::new(
            RpcClientConfig::new("http://unused")
                .with_discovery_subset(1)
                .with_discovery_subset_seed(5),
        )
        .connect_discovered(TestSubscription {
            receiver,
            dropped: None,
        });
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

    #[tokio::test]
    async fn active_discovery_health_marks_failed_endpoints_degraded() {
        let healthy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let healthy_address = healthy_listener.local_addr().unwrap();
        let healthy_server = tokio::spawn(async move {
            Server::builder()
                .add_service(EchoServer::new(EchoService))
                .serve_with_incoming(TcpListenerStream::new(healthy_listener))
                .await
                .unwrap();
        });
        let unavailable_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unavailable_address = unavailable_listener.local_addr().unwrap();
        drop(unavailable_listener);

        let (_updates, receiver) = watch::channel(vec![
            format!("http://{healthy_address}"),
            format!("http://{unavailable_address}"),
        ]);
        let config = RpcClientConfig::new("http://unused")
            .with_discovery_health_check(Duration::from_millis(20), Duration::from_millis(100));
        let (channel, mut status) =
            RpcClient::new(config).connect_discovered_with_status(TestSubscription {
                receiver,
                dropped: None,
            });

        let snapshot = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = status.changed().await.unwrap();
                if snapshot.readiness == DiscoveryReadiness::Degraded {
                    break snapshot;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(snapshot.discovered, 2);
        assert_eq!(snapshot.available, 1);

        let recovered_listener = TcpListener::bind(unavailable_address).await.unwrap();
        let recovered_server = tokio::spawn(async move {
            Server::builder()
                .add_service(EchoServer::new(EchoService))
                .serve_with_incoming(TcpListenerStream::new(recovered_listener))
                .await
                .unwrap();
        });
        let recovered = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = status.changed().await.unwrap();
                if snapshot.readiness == DiscoveryReadiness::Ready {
                    break snapshot;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(recovered.available, 2);

        drop(channel);
        healthy_server.abort();
        recovered_server.abort();
    }

    #[tokio::test]
    async fn discovery_status_projects_into_shared_health() {
        let (updates, receiver) = watch::channel(DiscoveryStatusSnapshot {
            readiness: DiscoveryReadiness::Empty,
            discovered: 0,
            selected: 0,
            available: 0,
            rejected: 0,
        });
        let registry = HealthRegistry::new();
        let mut health_updates = registry.subscribe();
        let task = DiscoveryStatus { receiver }.project_to_health(registry.clone(), "users-rpc");
        health_updates.changed().await.unwrap();
        assert_eq!(registry.snapshot().unhealthy(), vec!["users-rpc"]);

        updates
            .send(DiscoveryStatusSnapshot {
                readiness: DiscoveryReadiness::Ready,
                discovered: 1,
                selected: 1,
                available: 1,
                rejected: 0,
            })
            .unwrap();
        health_updates.changed().await.unwrap();
        assert!(registry.snapshot().is_ready());
        task.abort();
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

    #[test]
    fn weighted_discovery_keys_preserve_relative_capacity() {
        let client = RpcClient::new(RpcClientConfig::new("http://unused"));
        let (configured, discovered, rejected) = client.configure_discovered(vec![
            DiscoveredEndpoint::weighted("http://one:8080", 3).unwrap(),
            DiscoveredEndpoint::weighted("http://two:8080", 1).unwrap(),
            DiscoveredEndpoint::new("not a URI").unwrap(),
        ]);
        let available = configured.keys().cloned().collect();
        let keys = weighted_keys(&configured, &available);

        assert_eq!(keys.len(), 4);
        assert_eq!(rejected, 1);
        assert_eq!(
            discovery_status(discovered, configured.len(), configured.len(), rejected),
            DiscoveryStatusSnapshot {
                readiness: DiscoveryReadiness::Degraded,
                discovered: 3,
                selected: 2,
                available: 2,
                rejected: 1,
            }
        );
    }

    #[test]
    fn discovery_status_distinguishes_empty_ready_and_degraded() {
        assert_eq!(
            discovery_status(0, 0, 0, 0).readiness,
            DiscoveryReadiness::Empty
        );
        assert_eq!(
            discovery_status(2, 2, 2, 0).readiness,
            DiscoveryReadiness::Ready
        );
        assert_eq!(
            discovery_status(2, 2, 1, 0).readiness,
            DiscoveryReadiness::Degraded
        );
    }

    fn discovered_range(count: usize) -> Vec<DiscoveredEndpoint> {
        (0..count)
            .map(|index| DiscoveredEndpoint::new(format!("http://service-{index}:8080")).unwrap())
            .collect()
    }

    #[test]
    fn discovery_subsetting_caps_connections_and_can_be_disabled() {
        let endpoints = discovered_range(10_000);
        let subset_client = RpcClient::new(
            RpcClientConfig::new("http://unused")
                .with_discovery_subset(64)
                .with_discovery_subset_seed(11),
        );
        let (subset, discovered, rejected) = subset_client.configure_discovered(endpoints.clone());
        assert_eq!(discovered, 10_000);
        assert_eq!(subset.len(), 64);
        assert_eq!(rejected, 0);

        let (all, discovered, rejected) =
            RpcClient::new(RpcClientConfig::new("http://unused")).configure_discovered(endpoints);
        assert_eq!(discovered, 10_000);
        assert_eq!(all.len(), 10_000);
        assert_eq!(rejected, 0);
    }

    #[test]
    fn discovery_subsets_are_repeatable_and_low_churn() {
        let config = RpcClientConfig::new("http://unused")
            .with_discovery_subset(32)
            .with_discovery_subset_seed(23);
        let first = RpcClient::new(config.clone())
            .configure_discovered(discovered_range(1_000))
            .0;
        let reordered = RpcClient::new(config.clone())
            .configure_discovered(discovered_range(1_000).into_iter().rev().collect())
            .0;
        assert_eq!(
            first.keys().collect::<Vec<_>>(),
            reordered.keys().collect::<Vec<_>>()
        );

        let grown = RpcClient::new(config)
            .configure_discovered(discovered_range(1_001))
            .0;
        let retained = first.keys().filter(|uri| grown.contains_key(*uri)).count();
        assert!(
            retained >= 31,
            "one added endpoint replaced too many members"
        );
    }

    #[test]
    fn discovery_subsets_distribute_clients_across_the_fleet() {
        let endpoints = discovered_range(128);
        let subsets: BTreeSet<Vec<String>> = (0..32)
            .map(|seed| {
                RpcClient::new(
                    RpcClientConfig::new("http://unused")
                        .with_discovery_subset(8)
                        .with_discovery_subset_seed(seed),
                )
                .configure_discovered(endpoints.clone())
                .0
                .into_keys()
                .collect()
            })
            .collect();
        assert!(
            subsets.len() >= 30,
            "client seeds should spread subset membership"
        );
    }
}
