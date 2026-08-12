//! Etcd-backed configuration and service discovery.
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use rust_zero_core::{EtcdClient, EtcdConfig};
//! use std::time::Duration;
//! let client = EtcdClient::connect(EtcdConfig::new(["http://127.0.0.1:2379"])).await?;
//! let _lease = client
//!     .publish("users", "users-1", "http://127.0.0.1:8080", Duration::from_secs(10))
//!     .await?;
//! let subscription = client.subscribe("users").await?;
//! assert!(!subscription.endpoints().is_empty());
//! # Ok(())
//! # }
//! ```

use crate::{
    ConfigFormat, DiscoveryReconnectBackoff, DynamicConfig, EndpointChangeFuture,
    EndpointSubscription,
};
use etcd_client::{
    Certificate, Client, ConnectOptions, EventType, GetOptions, Identity, PutOptions, TlsOptions,
    WatchOptions,
};
use serde::de::DeserializeOwned;
use std::{
    collections::{hash_map::DefaultHasher, BTreeMap},
    error::Error,
    fmt,
    hash::{Hash, Hasher},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
};

/// Connection and namespace settings for the etcd configuration and discovery adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EtcdConfig {
    pub endpoints: Vec<String>,
    pub namespace: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub timeout: Duration,
    pub reconnect_backoff: DiscoveryReconnectBackoff,
    pub tls: Option<EtcdTlsConfig>,
}

/// CA trust, optional client identity, and server-name override for etcd TLS/mTLS.
#[derive(Clone, PartialEq, Eq)]
pub struct EtcdTlsConfig {
    pub ca_certificate_pem: String,
    pub certificate_pem: Option<String>,
    pub private_key_pem: Option<String>,
    pub domain_name: Option<String>,
}

impl fmt::Debug for EtcdTlsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EtcdTlsConfig")
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

impl EtcdTlsConfig {
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

    fn validate(&self) -> Result<(), EtcdError> {
        if self.ca_certificate_pem.trim().is_empty() {
            return Err(EtcdError::InvalidTls(
                "etcd TLS CA certificate must not be empty",
            ));
        }
        if self.certificate_pem.is_some() != self.private_key_pem.is_some() {
            return Err(EtcdError::InvalidTls(
                "etcd TLS certificate and private key must be configured together",
            ));
        }
        if self
            .certificate_pem
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(EtcdError::InvalidTls(
                "etcd TLS client certificate must not be empty",
            ));
        }
        if self
            .private_key_pem
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(EtcdError::InvalidTls(
                "etcd TLS private key must not be empty",
            ));
        }
        if self
            .domain_name
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(EtcdError::InvalidTls(
                "etcd TLS domain name must not be empty",
            ));
        }
        Ok(())
    }

    fn options(&self) -> TlsOptions {
        let mut tls = TlsOptions::new()
            .ca_certificate(Certificate::from_pem(self.ca_certificate_pem.clone()));
        if let Some(domain_name) = &self.domain_name {
            tls = tls.domain_name(domain_name.clone());
        }
        if let (Some(certificate), Some(key)) = (&self.certificate_pem, &self.private_key_pem) {
            tls = tls.identity(Identity::from_pem(certificate.clone(), key.clone()));
        }
        tls
    }
}

impl EtcdConfig {
    pub fn new(endpoints: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            endpoints: endpoints.into_iter().map(Into::into).collect(),
            namespace: "/rust-zero".to_owned(),
            username: None,
            password: None,
            timeout: Duration::from_secs(10),
            reconnect_backoff: DiscoveryReconnectBackoff::default(),
            tls: None,
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

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        assert!(!timeout.is_zero(), "etcd timeout must be positive");
        self.timeout = timeout;
        self
    }

    pub fn with_reconnect_backoff(mut self, backoff: DiscoveryReconnectBackoff) -> Self {
        self.reconnect_backoff = backoff;
        self
    }

    pub fn with_tls(mut self, tls: EtcdTlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }
}

#[derive(Debug)]
pub enum EtcdError {
    EmptyEndpoints,
    EmptyName(&'static str),
    InvalidLeaseTtl,
    InvalidTls(&'static str),
    MissingConfig(String),
    InvalidConfig(crate::ConfigCenterError),
    Client(etcd_client::Error),
    Task(String),
}

impl fmt::Display for EtcdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyEndpoints => formatter.write_str("at least one etcd endpoint is required"),
            Self::EmptyName(kind) => write!(formatter, "etcd {kind} cannot be empty"),
            Self::InvalidLeaseTtl => formatter.write_str("etcd lease TTL must be positive"),
            Self::InvalidTls(message) => formatter.write_str(message),
            Self::MissingConfig(key) => {
                write!(formatter, "etcd configuration key {key:?} is missing")
            }
            Self::InvalidConfig(error) => error.fmt(formatter),
            Self::Client(error) => write!(formatter, "etcd operation failed: {error}"),
            Self::Task(error) => write!(formatter, "etcd background task failed: {error}"),
        }
    }
}

impl Error for EtcdError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidConfig(error) => Some(error),
            Self::Client(error) => Some(error),
            _ => None,
        }
    }
}

impl From<etcd_client::Error> for EtcdError {
    fn from(error: etcd_client::Error) -> Self {
        Self::Client(error)
    }
}

impl From<crate::ConfigCenterError> for EtcdError {
    fn from(error: crate::ConfigCenterError) -> Self {
        Self::InvalidConfig(error)
    }
}

/// A cloneable etcd client scoped to a namespace.
#[derive(Clone)]
pub struct EtcdClient {
    client: Client,
    namespace: Arc<str>,
    reconnect_backoff: DiscoveryReconnectBackoff,
}

impl EtcdClient {
    pub async fn connect(config: EtcdConfig) -> Result<Self, EtcdError> {
        if config.endpoints.is_empty() {
            return Err(EtcdError::EmptyEndpoints);
        }
        if let Some(tls) = &config.tls {
            tls.validate()?;
        }
        let namespace = normalize_namespace(&config.namespace)?;
        let reconnect_backoff = config.reconnect_backoff;
        let mut options = ConnectOptions::new().with_timeout(config.timeout);
        if let Some(username) = config.username {
            options = options.with_user(username, config.password.unwrap_or_default());
        }
        if let Some(tls) = config.tls {
            options = options.with_tls(tls.options());
        }
        let client = Client::connect(&config.endpoints, Some(options)).await?;
        Ok(Self {
            client,
            namespace: Arc::from(namespace),
            reconnect_backoff,
        })
    }

    pub fn from_client(client: Client, namespace: impl AsRef<str>) -> Result<Self, EtcdError> {
        Ok(Self {
            client,
            namespace: Arc::from(normalize_namespace(namespace.as_ref())?),
            reconnect_backoff: DiscoveryReconnectBackoff::default(),
        })
    }

    pub fn with_reconnect_backoff(mut self, backoff: DiscoveryReconnectBackoff) -> Self {
        self.reconnect_backoff = backoff;
        self
    }

    /// Loads a typed configuration value and watches future valid updates.
    ///
    /// Invalid updates leave the last known-good snapshot installed. The watcher remains active.
    pub async fn watch_config<T>(
        &self,
        name: impl AsRef<str>,
        format: ConfigFormat,
    ) -> Result<EtcdConfigWatcher<T>, EtcdError>
    where
        T: DeserializeOwned + Send + Sync + 'static,
    {
        let key = self.config_key(name.as_ref())?;
        let mut client = self.client.clone();
        let response = client.get(key.clone(), None).await?;
        let revision = response.header().map_or(0, |header| header.revision());
        let value = response
            .kvs()
            .first()
            .ok_or_else(|| EtcdError::MissingConfig(key.clone()))?
            .value_str()?;
        let config = DynamicConfig::new(value, format)?;
        let watched = config.clone();
        let task = tokio::spawn(async move {
            let options = WatchOptions::new().with_start_revision(revision + 1);
            let mut stream = client.watch(key, Some(options)).await?;
            while let Some(response) = stream.message().await? {
                for event in response.events() {
                    if event.event_type() == EventType::Put {
                        if let Some(value) = event.kv() {
                            // Invalid data is deliberately rejected by DynamicConfig while the
                            // watch continues to preserve the last known-good configuration.
                            let _ = watched.update(value.value_str()?);
                        }
                    }
                }
            }
            Err(EtcdError::Task("configuration watch closed".to_owned()))
        });
        Ok(EtcdConfigWatcher { config, task })
    }

    /// Publishes an endpoint under a renewable etcd lease.
    pub async fn publish(
        &self,
        service: impl AsRef<str>,
        instance: impl AsRef<str>,
        endpoint: impl Into<String>,
        ttl: Duration,
    ) -> Result<EtcdServiceLease, EtcdError> {
        if ttl.is_zero() {
            return Err(EtcdError::InvalidLeaseTtl);
        }
        let key = self.service_key(service.as_ref(), instance.as_ref())?;
        let endpoint = endpoint.into();
        if endpoint.trim().is_empty() {
            return Err(EtcdError::EmptyName("endpoint"));
        }
        let ttl_seconds = i64::try_from(ttl.as_secs().max(1)).unwrap_or(i64::MAX);
        let mut client = self.client.clone();
        let lease_id = client.lease_grant(ttl_seconds, None).await?.id();
        client
            .put(key, endpoint, Some(PutOptions::new().with_lease(lease_id)))
            .await?;
        let (shutdown, mut shutdown_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut keeper, mut responses) = client.lease_keep_alive(lease_id).await?;
            let mut interval = tokio::time::interval(Duration::from_secs(
                u64::try_from((ttl_seconds / 3).max(1)).unwrap_or(1),
            ));
            loop {
                tokio::select! {
                    _ = &mut shutdown_receiver => {
                        client.lease_revoke(lease_id).await?;
                        return Ok(());
                    }
                    _ = interval.tick() => {
                        keeper.keep_alive().await?;
                        if responses.message().await?.is_none() {
                            return Err(EtcdError::Task("lease keep-alive stream closed".to_owned()));
                        }
                    }
                }
            }
        });
        Ok(EtcdServiceLease {
            shutdown: Some(shutdown),
            task,
        })
    }

    /// Subscribes to the complete, sorted endpoint set for a service.
    pub async fn subscribe(
        &self,
        service: impl AsRef<str>,
    ) -> Result<EtcdServiceSubscription, EtcdError> {
        let prefix = self.service_prefix(service.as_ref())?;
        let mut client = self.client.clone();
        let response = client
            .get(prefix.clone(), Some(GetOptions::new().with_prefix()))
            .await?;
        let revision = response.header().map_or(0, |header| header.revision());
        let mut entries = BTreeMap::new();
        for value in response.kvs() {
            entries.insert(value.key().to_vec(), value.value_str()?.to_owned());
        }
        let initial = sorted_endpoints(&entries);
        let (updates, receiver) = watch::channel(initial);
        let backoff = self.reconnect_backoff;
        let task = tokio::spawn(async move {
            let seed = reconnect_seed(&prefix);
            let mut revision = revision;
            let mut attempt = 0_u32;
            loop {
                let options = WatchOptions::new()
                    .with_prefix()
                    .with_start_revision(revision.saturating_add(1));
                let watch_result = client.watch(prefix.clone(), Some(options)).await;
                if let Ok(mut stream) = watch_result {
                    while let Ok(Some(response)) = stream.message().await {
                        if let Some(header) = response.header() {
                            revision = revision.max(header.revision());
                        }
                        let mut changed = false;
                        for event in response.events() {
                            let Some(value) = event.kv() else {
                                continue;
                            };
                            match event.event_type() {
                                EventType::Put => {
                                    entries.insert(
                                        value.key().to_vec(),
                                        value.value_str()?.to_owned(),
                                    );
                                }
                                EventType::Delete => {
                                    entries.remove(value.key());
                                }
                            }
                            changed = true;
                        }
                        if changed {
                            updates.send_replace(sorted_endpoints(&entries));
                        }
                        attempt = 0;
                    }
                }

                tokio::time::sleep(backoff.delay(attempt, seed.wrapping_add(u64::from(attempt))))
                    .await;
                attempt = attempt.saturating_add(1);

                // Always relist after a broken stream. This repairs missed events and recovers
                // transparently when etcd has compacted the previous watch revision.
                match client
                    .get(prefix.clone(), Some(GetOptions::new().with_prefix()))
                    .await
                {
                    Ok(response) => {
                        revision = response
                            .header()
                            .map_or(revision, |header| header.revision());
                        entries.clear();
                        for value in response.kvs() {
                            if let Ok(endpoint) = value.value_str() {
                                entries.insert(value.key().to_vec(), endpoint.to_owned());
                            }
                        }
                        updates.send_replace(sorted_endpoints(&entries));
                    }
                    Err(_) => continue,
                }
            }
            #[allow(unreachable_code)]
            Ok(())
        });
        Ok(EtcdServiceSubscription { receiver, task })
    }

    fn config_key(&self, name: &str) -> Result<String, EtcdError> {
        validate_name("configuration name", name)?;
        Ok(format!("{}/config/{name}", self.namespace))
    }

    fn service_prefix(&self, service: &str) -> Result<String, EtcdError> {
        validate_name("service name", service)?;
        Ok(format!("{}/discovery/{service}/", self.namespace))
    }

    fn service_key(&self, service: &str, instance: &str) -> Result<String, EtcdError> {
        validate_name("instance name", instance)?;
        Ok(format!("{}{instance}", self.service_prefix(service)?))
    }
}

pub struct EtcdConfigWatcher<T> {
    config: DynamicConfig<T>,
    task: JoinHandle<Result<(), EtcdError>>,
}

impl<T> EtcdConfigWatcher<T> {
    pub fn config(&self) -> &DynamicConfig<T> {
        &self.config
    }

    pub async fn wait(mut self) -> Result<(), EtcdError> {
        (&mut self.task)
            .await
            .map_err(|error| EtcdError::Task(error.to_string()))?
    }
}

impl<T> Drop for EtcdConfigWatcher<T> {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct EtcdServiceLease {
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), EtcdError>>,
}

impl EtcdServiceLease {
    pub async fn revoke(mut self) -> Result<(), EtcdError> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        (&mut self.task)
            .await
            .map_err(|error| EtcdError::Task(error.to_string()))?
    }
}

impl Drop for EtcdServiceLease {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

pub struct EtcdServiceSubscription {
    receiver: watch::Receiver<Vec<String>>,
    task: JoinHandle<Result<(), EtcdError>>,
}

impl EtcdServiceSubscription {
    pub fn endpoints(&self) -> Vec<String> {
        self.receiver.borrow().clone()
    }

    pub async fn changed(&mut self) -> Result<Vec<String>, EtcdError> {
        self.receiver
            .changed()
            .await
            .map_err(|_| EtcdError::Task("service watch closed".to_owned()))?;
        Ok(self.endpoints())
    }
}

impl EndpointSubscription for EtcdServiceSubscription {
    type Error = EtcdError;

    fn endpoints(&self) -> Vec<String> {
        EtcdServiceSubscription::endpoints(self)
    }

    fn changed(&mut self) -> EndpointChangeFuture<'_, Self::Error> {
        Box::pin(EtcdServiceSubscription::changed(self))
    }
}

impl Drop for EtcdServiceSubscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn normalize_namespace(namespace: &str) -> Result<String, EtcdError> {
    let namespace = namespace.trim_matches('/');
    validate_name("namespace", namespace)?;
    Ok(format!("/{namespace}"))
}

fn validate_name(kind: &'static str, value: &str) -> Result<(), EtcdError> {
    if value.trim().is_empty() || value.contains('/') {
        Err(EtcdError::EmptyName(kind))
    } else {
        Ok(())
    }
}

fn sorted_endpoints(entries: &BTreeMap<Vec<u8>, String>) -> Vec<String> {
    let mut endpoints: Vec<_> = entries.values().cloned().collect();
    endpoints.sort();
    endpoints.dedup();
    endpoints
}

fn reconnect_seed(scope: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    scope.hash(&mut hasher);
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Limits {
        requests: u64,
    }

    #[test]
    fn validates_and_normalizes_namespaces_and_names() {
        assert_eq!(normalize_namespace("services").unwrap(), "/services");
        assert_eq!(normalize_namespace("/services/").unwrap(), "/services");
        assert!(normalize_namespace("/").is_err());
        assert!(validate_name("service", "bad/name").is_err());
    }

    #[test]
    fn endpoint_snapshots_are_sorted_and_deduplicated() {
        let entries = BTreeMap::from([
            (b"instance-b".to_vec(), "http://b:8080".to_owned()),
            (b"instance-a".to_vec(), "http://a:8080".to_owned()),
            (b"instance-c".to_vec(), "http://a:8080".to_owned()),
        ]);
        assert_eq!(
            sorted_endpoints(&entries),
            vec!["http://a:8080", "http://b:8080"]
        );
    }

    #[test]
    fn tls_configuration_requires_complete_client_identity() {
        let missing_key = EtcdTlsConfig::new("ca");
        let missing_key = EtcdTlsConfig {
            certificate_pem: Some("certificate".to_owned()),
            ..missing_key
        };
        assert!(missing_key
            .validate()
            .unwrap_err()
            .to_string()
            .contains("configured together"));
        assert!(EtcdTlsConfig::new("").validate().is_err());
    }

    #[tokio::test]
    async fn integration_connects_to_tls_etcd_when_configured() {
        let (Ok(endpoint), Ok(ca)) = (
            std::env::var("RUST_ZERO_ETCD_TLS_ENDPOINT"),
            std::env::var("RUST_ZERO_ETCD_CA_PEM"),
        ) else {
            return;
        };
        let mut tls = EtcdTlsConfig::new(ca);
        if let Ok(domain_name) = std::env::var("RUST_ZERO_ETCD_DOMAIN_NAME") {
            tls = tls.with_domain_name(domain_name);
        }
        if let (Ok(certificate), Ok(key)) = (
            std::env::var("RUST_ZERO_ETCD_CLIENT_CERT_PEM"),
            std::env::var("RUST_ZERO_ETCD_CLIENT_KEY_PEM"),
        ) {
            tls = tls.with_identity(certificate, key);
        }
        let client = EtcdClient::connect(EtcdConfig::new([endpoint]).with_tls(tls))
            .await
            .unwrap();
        client.subscribe("tls-probe").await.unwrap();
    }

    #[tokio::test]
    async fn integration_covers_config_discovery_and_lease_withdrawal() {
        let Ok(endpoint) = std::env::var("RUST_ZERO_ETCD_ENDPOINT") else {
            return;
        };
        let namespace = format!("rust-zero-{}", std::process::id());
        let mut raw = Client::connect([&endpoint], None).await.unwrap();
        let adapter = EtcdClient::from_client(raw.clone(), &namespace).unwrap();
        let config_key = format!("/{namespace}/config/limits");
        raw.put(config_key.clone(), "requests = 10", None)
            .await
            .unwrap();

        let watcher = adapter
            .watch_config::<Limits>("limits", ConfigFormat::Toml)
            .await
            .unwrap();
        let mut changes = watcher.config().subscribe();
        raw.put(config_key, "requests = 20", None).await.unwrap();
        changes.changed().await.unwrap();
        assert_eq!(changes.borrow().value().requests, 20);

        let mut services = adapter.subscribe("users").await.unwrap();
        let lease = adapter
            .publish(
                "users",
                "instance-a",
                "http://127.0.0.1:8080",
                Duration::from_secs(3),
            )
            .await
            .unwrap();
        assert_eq!(
            services.changed().await.unwrap(),
            vec!["http://127.0.0.1:8080"]
        );
        lease.revoke().await.unwrap();
        assert!(services.changed().await.unwrap().is_empty());
        raw.delete(
            format!("/{namespace}/"),
            Some(etcd_client::DeleteOptions::new().with_prefix()),
        )
        .await
        .unwrap();
    }
}
