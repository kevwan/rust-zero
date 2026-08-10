//! Kubernetes EndpointSlice service discovery.
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use rust_zero_core::{KubernetesDiscovery, KubernetesDiscoveryConfig};
//! let discovery = KubernetesDiscovery::infer(
//!     KubernetesDiscoveryConfig::new("production").with_port_name("grpc"),
//! ).await?;
//! let subscription = discovery.subscribe("users").await?;
//! println!("{:?}", subscription.endpoints());
//! # Ok(())
//! # }
//! ```

use crate::{DiscoveryReconnectBackoff, EndpointChangeFuture, EndpointSubscription};
use futures::StreamExt;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::{runtime::watcher, Api, Client, ResourceExt};
use std::{
    collections::{hash_map::DefaultHasher, BTreeMap, BTreeSet},
    error::Error,
    fmt,
    hash::{Hash, Hasher},
    net::IpAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinHandle};

/// Namespace, port, and URI settings for Kubernetes EndpointSlice discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KubernetesDiscoveryConfig {
    pub namespace: String,
    pub port_name: Option<String>,
    pub scheme: String,
    pub startup_timeout: Duration,
    pub reconnect_backoff: DiscoveryReconnectBackoff,
}

impl KubernetesDiscoveryConfig {
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            port_name: None,
            scheme: "http".to_owned(),
            startup_timeout: Duration::from_secs(10),
            reconnect_backoff: DiscoveryReconnectBackoff::default(),
        }
    }

    pub fn with_port_name(mut self, port_name: impl Into<String>) -> Self {
        self.port_name = Some(port_name.into());
        self
    }

    pub fn with_scheme(mut self, scheme: impl Into<String>) -> Self {
        self.scheme = scheme.into();
        self
    }

    pub fn with_startup_timeout(mut self, timeout: Duration) -> Self {
        assert!(
            !timeout.is_zero(),
            "Kubernetes discovery startup timeout must be positive"
        );
        self.startup_timeout = timeout;
        self
    }

    pub fn with_reconnect_backoff(mut self, backoff: DiscoveryReconnectBackoff) -> Self {
        self.reconnect_backoff = backoff;
        self
    }
}

#[derive(Debug)]
pub enum KubernetesDiscoveryError {
    InvalidNamespace,
    InvalidService,
    InvalidPortName,
    InvalidScheme,
    Client(kube::Error),
    StartupTimeout,
    WatchClosed,
}

impl fmt::Display for KubernetesDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidNamespace => formatter.write_str("Kubernetes namespace cannot be empty"),
            Self::InvalidService => formatter.write_str("Kubernetes service name cannot be empty"),
            Self::InvalidPortName => formatter.write_str("Kubernetes port name cannot be empty"),
            Self::InvalidScheme => formatter.write_str("Kubernetes endpoint scheme is invalid"),
            Self::Client(error) => write!(formatter, "Kubernetes client failed: {error}"),
            Self::StartupTimeout => {
                formatter.write_str("Kubernetes endpoint discovery timed out during initial list")
            }
            Self::WatchClosed => formatter.write_str("Kubernetes endpoint watch closed"),
        }
    }
}

impl Error for KubernetesDiscoveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Client(error) => Some(error),
            _ => None,
        }
    }
}

impl From<kube::Error> for KubernetesDiscoveryError {
    fn from(error: kube::Error) -> Self {
        Self::Client(error)
    }
}

/// A Kubernetes client that discovers ready addresses from EndpointSlices.
#[derive(Clone)]
pub struct KubernetesDiscovery {
    client: Client,
    config: Arc<KubernetesDiscoveryConfig>,
}

impl KubernetesDiscovery {
    /// Creates a client from the local kubeconfig or in-cluster service account.
    pub async fn infer(
        config: KubernetesDiscoveryConfig,
    ) -> Result<Self, KubernetesDiscoveryError> {
        Self::new(Client::try_default().await?, config)
    }

    pub fn new(
        client: Client,
        config: KubernetesDiscoveryConfig,
    ) -> Result<Self, KubernetesDiscoveryError> {
        validate_config(&config)?;
        Ok(Self {
            client,
            config: Arc::new(config),
        })
    }

    /// Starts a self-healing EndpointSlice watch and returns its first complete snapshot.
    ///
    /// The caller needs `list` and `watch` access to `discovery.k8s.io/v1` EndpointSlices in the
    /// configured namespace. Only ready, non-terminating endpoints are returned.
    pub async fn subscribe(
        &self,
        service: impl AsRef<str>,
    ) -> Result<KubernetesServiceSubscription, KubernetesDiscoveryError> {
        let service = service.as_ref().trim();
        if !valid_dns_label(service) {
            return Err(KubernetesDiscoveryError::InvalidService);
        }

        let api = Api::<EndpointSlice>::namespaced(self.client.clone(), &self.config.namespace);
        let watcher_config =
            watcher::Config::default().labels(&format!("kubernetes.io/service-name={service}"));
        let port_name = self.config.port_name.clone();
        let scheme: Arc<str> = Arc::from(self.config.scheme.clone());
        let backoff = self.config.reconnect_backoff;
        let reconnect_seed = reconnect_seed(service);
        let (updates, mut receiver) = watch::channel(Vec::new());

        let task = tokio::spawn(async move {
            let mut slices = BTreeMap::<String, Vec<String>>::new();
            let mut attempt = 0_u32;
            loop {
                let mut pending = BTreeMap::<String, Vec<String>>::new();
                let mut stream = Box::pin(watcher(api.clone(), watcher_config.clone()));
                let mut received_event = false;
                while let Some(result) = stream.next().await {
                    let Ok(event) = result else {
                        break;
                    };
                    received_event = true;
                    match event {
                        watcher::Event::Apply(slice) => {
                            slices.insert(
                                slice.name_any(),
                                endpoints_from_slice(&slice, port_name.as_deref(), &scheme),
                            );
                            updates.send_replace(combined_endpoints(&slices));
                        }
                        watcher::Event::Delete(slice) => {
                            slices.remove(&slice.name_any());
                            updates.send_replace(combined_endpoints(&slices));
                        }
                        watcher::Event::Init => {
                            pending.clear();
                        }
                        watcher::Event::InitApply(slice) => {
                            pending.insert(
                                slice.name_any(),
                                endpoints_from_slice(&slice, port_name.as_deref(), &scheme),
                            );
                        }
                        watcher::Event::InitDone => {
                            slices = std::mem::take(&mut pending);
                            updates.send_replace(combined_endpoints(&slices));
                        }
                    }
                }
                if received_event {
                    attempt = 0;
                }
                tokio::time::sleep(
                    backoff.delay(attempt, reconnect_seed.wrapping_add(u64::from(attempt))),
                )
                .await;
                attempt = attempt.saturating_add(1);
            }
            #[allow(unreachable_code)]
            Ok(())
        });

        match tokio::time::timeout(self.config.startup_timeout, receiver.changed()).await {
            Ok(Ok(())) => Ok(KubernetesServiceSubscription { receiver, task }),
            Ok(Err(_)) => match task.await {
                Ok(Err(error)) => Err(error),
                Ok(Ok(())) => Err(KubernetesDiscoveryError::WatchClosed),
                Err(_) => Err(KubernetesDiscoveryError::WatchClosed),
            },
            Err(_) => {
                task.abort();
                Err(KubernetesDiscoveryError::StartupTimeout)
            }
        }
    }
}

/// A live, complete endpoint snapshot for one Kubernetes Service.
pub struct KubernetesServiceSubscription {
    receiver: watch::Receiver<Vec<String>>,
    task: JoinHandle<Result<(), KubernetesDiscoveryError>>,
}

impl KubernetesServiceSubscription {
    pub fn endpoints(&self) -> Vec<String> {
        self.receiver.borrow().clone()
    }

    pub async fn changed(&mut self) -> Result<Vec<String>, KubernetesDiscoveryError> {
        self.receiver
            .changed()
            .await
            .map_err(|_| KubernetesDiscoveryError::WatchClosed)?;
        Ok(self.endpoints())
    }
}

impl EndpointSubscription for KubernetesServiceSubscription {
    type Error = KubernetesDiscoveryError;

    fn endpoints(&self) -> Vec<String> {
        KubernetesServiceSubscription::endpoints(self)
    }

    fn changed(&mut self) -> EndpointChangeFuture<'_, Self::Error> {
        Box::pin(KubernetesServiceSubscription::changed(self))
    }
}

impl Drop for KubernetesServiceSubscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn validate_config(config: &KubernetesDiscoveryConfig) -> Result<(), KubernetesDiscoveryError> {
    if !valid_dns_label(config.namespace.trim()) {
        return Err(KubernetesDiscoveryError::InvalidNamespace);
    }
    if config
        .port_name
        .as_ref()
        .is_some_and(|name| name.trim().is_empty())
    {
        return Err(KubernetesDiscoveryError::InvalidPortName);
    }
    let mut characters = config.scheme.chars();
    if !characters
        .next()
        .is_some_and(|value| value.is_ascii_alphabetic())
        || !characters
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '+' | '-' | '.'))
    {
        return Err(KubernetesDiscoveryError::InvalidScheme);
    }
    Ok(())
}

fn valid_dns_label(value: &str) -> bool {
    value.len() <= 63
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value
            .bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
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

fn endpoints_from_slice(
    slice: &EndpointSlice,
    port_name: Option<&str>,
    scheme: &str,
) -> Vec<String> {
    let port = slice.ports.as_deref().and_then(|ports| {
        ports.iter().find(|port| {
            port.port.is_some()
                && port
                    .protocol
                    .as_deref()
                    .is_none_or(|protocol| protocol == "TCP")
                && port_name.is_none_or(|name| port.name.as_deref() == Some(name))
        })
    });
    let Some(port) = port.and_then(|port| port.port) else {
        return Vec::new();
    };

    let mut endpoints = BTreeSet::new();
    for endpoint in slice.endpoints.as_deref().unwrap_or_default() {
        let ready = endpoint
            .conditions
            .as_ref()
            .and_then(|conditions| conditions.ready)
            .unwrap_or(true);
        let terminating = endpoint
            .conditions
            .as_ref()
            .and_then(|conditions| conditions.terminating)
            .unwrap_or(false);
        if !ready || terminating {
            continue;
        }
        for address in &endpoint.addresses {
            let host = match address.parse::<IpAddr>() {
                Ok(IpAddr::V6(_)) => format!("[{address}]"),
                _ => address.clone(),
            };
            endpoints.insert(format!("{scheme}://{host}:{port}"));
        }
    }
    endpoints.into_iter().collect()
}

fn combined_endpoints(slices: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    slices
        .values()
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::discovery::v1::{Endpoint, EndpointConditions, EndpointPort};

    fn slice(
        name: &str,
        address: &str,
        ready: Option<bool>,
        terminating: Option<bool>,
    ) -> EndpointSlice {
        EndpointSlice {
            metadata: kube::api::ObjectMeta {
                name: Some(name.to_owned()),
                ..Default::default()
            },
            address_type: "IPv4".to_owned(),
            endpoints: Some(vec![Endpoint {
                addresses: vec![address.to_owned()],
                conditions: Some(EndpointConditions {
                    ready,
                    serving: None,
                    terminating,
                }),
                ..Default::default()
            }]),
            ports: Some(vec![EndpointPort {
                name: Some("grpc".to_owned()),
                port: Some(8080),
                protocol: Some("TCP".to_owned()),
                ..Default::default()
            }]),
        }
    }

    #[test]
    fn validates_configuration() {
        assert!(matches!(
            validate_config(&KubernetesDiscoveryConfig::new(" ")),
            Err(KubernetesDiscoveryError::InvalidNamespace)
        ));
        assert!(matches!(
            validate_config(&KubernetesDiscoveryConfig::new("default").with_scheme("1http")),
            Err(KubernetesDiscoveryError::InvalidScheme)
        ));
        assert!(!valid_dns_label("users=anything"));
        assert!(valid_dns_label("users-api"));
    }

    #[test]
    fn extracts_only_ready_non_terminating_endpoints() {
        assert_eq!(
            endpoints_from_slice(
                &slice("a", "10.0.0.1", Some(true), None),
                Some("grpc"),
                "http"
            ),
            vec!["http://10.0.0.1:8080"]
        );
        assert!(endpoints_from_slice(
            &slice("b", "10.0.0.2", Some(false), None),
            Some("grpc"),
            "http"
        )
        .is_empty());
        assert!(endpoints_from_slice(
            &slice("c", "10.0.0.3", Some(true), Some(true)),
            Some("grpc"),
            "http"
        )
        .is_empty());
    }

    #[test]
    fn combines_slices_in_stable_deduplicated_order() {
        let slices = BTreeMap::from([
            (
                "b".to_owned(),
                vec!["http://b:80".to_owned(), "http://a:80".to_owned()],
            ),
            ("a".to_owned(), vec!["http://a:80".to_owned()]),
        ]);
        assert_eq!(
            combined_endpoints(&slices),
            vec!["http://a:80", "http://b:80"]
        );
    }

    #[tokio::test]
    async fn kubernetes_integration_lists_and_watches_a_service() {
        let Ok(service) = std::env::var("RUST_ZERO_KUBERNETES_SERVICE") else {
            return;
        };
        let namespace = std::env::var("RUST_ZERO_KUBERNETES_NAMESPACE")
            .unwrap_or_else(|_| "default".to_owned());
        let mut config = KubernetesDiscoveryConfig::new(&namespace);
        if let Ok(port_name) = std::env::var("RUST_ZERO_KUBERNETES_PORT_NAME") {
            config = config.with_port_name(port_name);
        }
        let discovery = KubernetesDiscovery::infer(config).await.unwrap();
        let api = Api::<EndpointSlice>::namespaced(discovery.client.clone(), &namespace);
        let mut subscription = discovery.subscribe(&service).await.unwrap();
        assert!(!subscription.endpoints().is_empty());

        let name = format!("rust-zero-watch-{}", std::process::id());
        let mut added = slice(&name, "10.0.0.99", Some(true), None);
        added.metadata.labels = Some(BTreeMap::from([(
            "kubernetes.io/service-name".to_owned(),
            service,
        )]));
        api.create(&kube::api::PostParams::default(), &added)
            .await
            .unwrap();
        let endpoints = tokio::time::timeout(Duration::from_secs(10), subscription.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(endpoints
            .iter()
            .any(|endpoint| endpoint.contains("10.0.0.99")));

        api.delete(&name, &kube::api::DeleteParams::default())
            .await
            .unwrap();
        let endpoints = tokio::time::timeout(Duration::from_secs(10), subscription.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(!endpoints
            .iter()
            .any(|endpoint| endpoint.contains("10.0.0.99")));
    }
}
