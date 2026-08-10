use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::sync::broadcast;

const EVENT_BUFFER_SIZE: usize = 128;
const MAX_ENDPOINT_WEIGHT: u32 = 1_000;

/// Capped exponential delay used by reconnecting discovery backends.
///
/// Jitter is an absolute upper bound added to each exponential delay. Keeping it as a duration
/// makes the policy fully comparable and avoids floating-point configuration edge cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryReconnectBackoff {
    initial: Duration,
    max: Duration,
    jitter: Duration,
}

impl Default for DiscoveryReconnectBackoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(200),
            max: Duration::from_secs(10),
            jitter: Duration::from_millis(200),
        }
    }
}

impl DiscoveryReconnectBackoff {
    pub fn new(initial: Duration, max: Duration, jitter: Duration) -> Self {
        assert!(
            !initial.is_zero(),
            "discovery reconnect delay must be positive"
        );
        assert!(
            max >= initial,
            "discovery reconnect maximum must not be less than its initial delay"
        );
        Self {
            initial,
            max,
            jitter,
        }
    }

    pub fn initial(self) -> Duration {
        self.initial
    }

    pub fn max(self) -> Duration {
        self.max
    }

    pub fn jitter(self) -> Duration {
        self.jitter
    }

    /// Returns the delay for a zero-based retry attempt and caller-provided jitter sample.
    /// Supplying the sample makes reconnect schedules straightforward to test deterministically.
    pub fn delay(self, attempt: u32, jitter_sample: u64) -> Duration {
        let multiplier = 1_u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX);
        let base = self.initial.saturating_mul(multiplier).min(self.max);
        let jitter_nanos = self.jitter.as_nanos();
        if jitter_nanos == 0 {
            return base;
        }
        let sampled = u128::from(jitter_sample) % (jitter_nanos + 1);
        base.saturating_add(Duration::from_nanos(
            u64::try_from(sampled).unwrap_or(u64::MAX),
        ))
    }
}

#[cfg(test)]
mod reconnect_tests {
    use super::*;

    #[test]
    fn reconnect_backoff_is_exponential_capped_and_deterministic() {
        let policy = DiscoveryReconnectBackoff::new(
            Duration::from_millis(10),
            Duration::from_millis(40),
            Duration::from_millis(5),
        );
        assert_eq!(policy.delay(0, 0), Duration::from_millis(10));
        assert_eq!(policy.delay(1, 1_000_000), Duration::from_millis(21));
        assert_eq!(policy.delay(2, 2_000_000), Duration::from_millis(42));
        assert_eq!(policy.delay(20, 5_000_001), Duration::from_millis(40));
        assert_eq!(policy.delay(2, 123), policy.delay(2, 123));
    }

    #[test]
    #[should_panic(expected = "discovery reconnect delay must be positive")]
    fn reconnect_backoff_rejects_zero_initial_delay() {
        DiscoveryReconnectBackoff::new(Duration::ZERO, Duration::from_secs(1), Duration::ZERO);
    }
}

/// Transport-neutral metadata attached to a discovered service endpoint.
///
/// Weights are relative: an endpoint with weight `3` receives roughly three times as many
/// selections as one with weight `1`. The upper bound prevents a malformed registry value from
/// creating an unbounded number of balancing entries.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DiscoveredEndpoint {
    uri: String,
    weight: u32,
    metadata: BTreeMap<String, String>,
}

impl DiscoveredEndpoint {
    pub fn new(uri: impl Into<String>) -> Result<Self, DiscoveryError> {
        Self::weighted(uri, 1)
    }

    pub fn weighted(uri: impl Into<String>, weight: u32) -> Result<Self, DiscoveryError> {
        Ok(Self {
            uri: validate_endpoint(uri.into())?,
            weight: validate_weight(weight)?,
            metadata: BTreeMap::new(),
        })
    }

    pub fn with_metadata(mut self, metadata: impl IntoIterator<Item = (String, String)>) -> Self {
        self.metadata = metadata.into_iter().collect();
        self
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn weight(&self) -> u32 {
        self.weight
    }

    pub fn metadata(&self) -> &BTreeMap<String, String> {
        &self.metadata
    }
}

/// Future returned while waiting for a discovery snapshot to change.
pub type EndpointChangeFuture<'a, E> =
    Pin<Box<dyn Future<Output = Result<Vec<String>, E>> + Send + 'a>>;

/// A live, complete snapshot of the endpoints for one logical service.
///
/// Discovery backends expose snapshots rather than backend-specific add/remove events so
/// transports can recover consistently after a watch reconnect or a lagged consumer.
pub trait EndpointSubscription: Send + 'static {
    type Error: Send + 'static;

    /// Returns the latest complete endpoint snapshot in stable order.
    fn endpoints(&self) -> Vec<String>;

    /// Returns endpoint metadata when the backend provides it.
    ///
    /// Existing discovery implementations remain source-compatible and receive weight `1`.
    fn discovered_endpoints(&self) -> Vec<DiscoveredEndpoint> {
        self.endpoints()
            .into_iter()
            .filter_map(|uri| DiscoveredEndpoint::new(uri).ok())
            .collect()
    }

    /// Waits until a new complete endpoint snapshot is available.
    fn changed(&mut self) -> EndpointChangeFuture<'_, Self::Error>;
}

/// A local service registry with reference-counted endpoint leases and change subscriptions.
#[derive(Clone)]
pub struct ServiceRegistry {
    state: Arc<RegistryState>,
}

impl Default for ServiceRegistry {
    fn default() -> Self {
        let (changes, _) = broadcast::channel(EVENT_BUFFER_SIZE);
        Self {
            state: Arc::new(RegistryState {
                services: Mutex::new(HashMap::new()),
                changes,
            }),
        }
    }
}

impl ServiceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes an endpoint until the returned lease is dropped or explicitly released.
    pub fn publish(
        &self,
        service: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Result<ServiceLease, DiscoveryError> {
        self.publish_endpoint(service, DiscoveredEndpoint::new(endpoint)?)
    }

    /// Publishes a weighted endpoint until the returned lease is released.
    pub fn publish_weighted(
        &self,
        service: impl Into<String>,
        endpoint: impl Into<String>,
        weight: u32,
    ) -> Result<ServiceLease, DiscoveryError> {
        self.publish_endpoint(service, DiscoveredEndpoint::weighted(endpoint, weight)?)
    }

    /// Publishes a fully described endpoint until the returned lease is released.
    pub fn publish_endpoint(
        &self,
        service: impl Into<String>,
        endpoint: DiscoveredEndpoint,
    ) -> Result<ServiceLease, DiscoveryError> {
        let service = validate_service(service.into())?;
        let uri = endpoint.uri.clone();
        let added = {
            let mut services = self
                .state
                .services
                .lock()
                .expect("service registry mutex poisoned");
            let endpoints = services.entry(service.clone()).or_default();
            match endpoints.get_mut(&uri) {
                Some(entry) if entry.endpoint != endpoint => {
                    return Err(DiscoveryError::ConflictingEndpointMetadata(uri));
                }
                Some(entry) => {
                    entry.references += 1;
                    false
                }
                None => {
                    endpoints.insert(
                        uri.clone(),
                        RegistryEndpoint {
                            endpoint,
                            references: 1,
                        },
                    );
                    true
                }
            }
        };

        if added {
            let _ = self.state.changes.send(ServiceEvent::Added {
                service: service.clone(),
                endpoint: uri.clone(),
            });
        }

        Ok(ServiceLease {
            state: Arc::clone(&self.state),
            service,
            endpoint: uri,
            active: true,
        })
    }

    /// Returns the currently published endpoints for a service in stable order.
    pub fn endpoints(&self, service: &str) -> Result<Vec<String>, DiscoveryError> {
        let service = validate_service(service.to_owned())?;
        let services = self
            .state
            .services
            .lock()
            .expect("service registry mutex poisoned");
        Ok(services
            .get(&service)
            .into_iter()
            .flat_map(|endpoints| endpoints.keys())
            .cloned()
            .collect())
    }

    /// Returns the currently published endpoint metadata in stable URI order.
    pub fn discovered_endpoints(
        &self,
        service: &str,
    ) -> Result<Vec<DiscoveredEndpoint>, DiscoveryError> {
        let service = validate_service(service.to_owned())?;
        let services = self
            .state
            .services
            .lock()
            .expect("service registry mutex poisoned");
        Ok(services
            .get(&service)
            .into_iter()
            .flat_map(|endpoints| endpoints.values())
            .map(|entry| entry.endpoint.clone())
            .collect())
    }

    /// Subscribes to endpoint changes and includes the current endpoint snapshot.
    pub fn subscribe(
        &self,
        service: impl Into<String>,
    ) -> Result<ServiceSubscription, DiscoveryError> {
        let service = validate_service(service.into())?;
        let receiver = self.state.changes.subscribe();
        let endpoints = self
            .state
            .services
            .lock()
            .expect("service registry mutex poisoned")
            .get(&service)
            .into_iter()
            .flat_map(|endpoints| endpoints.keys())
            .cloned()
            .collect();

        Ok(ServiceSubscription {
            service,
            endpoints,
            receiver,
            state: Arc::clone(&self.state),
        })
    }
}

struct RegistryEndpoint {
    endpoint: DiscoveredEndpoint,
    references: usize,
}

struct RegistryState {
    services: Mutex<HashMap<String, BTreeMap<String, RegistryEndpoint>>>,
    changes: broadcast::Sender<ServiceEvent>,
}

/// A published endpoint. Dropping it withdraws the endpoint when its final lease is released.
pub struct ServiceLease {
    state: Arc<RegistryState>,
    service: String,
    endpoint: String,
    active: bool,
}

impl fmt::Debug for ServiceLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceLease")
            .field("service", &self.service)
            .field("endpoint", &self.endpoint)
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}

impl ServiceLease {
    /// Withdraws this lease early. Further calls are harmless.
    pub fn release(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;

        let removed = {
            let mut services = self
                .state
                .services
                .lock()
                .expect("service registry mutex poisoned");
            let Some(endpoints) = services.get_mut(&self.service) else {
                return;
            };
            let Some(entry) = endpoints.get_mut(&self.endpoint) else {
                return;
            };

            entry.references -= 1;
            if entry.references > 0 {
                false
            } else {
                endpoints.remove(&self.endpoint);
                if endpoints.is_empty() {
                    services.remove(&self.service);
                }
                true
            }
        };

        if removed {
            let _ = self.state.changes.send(ServiceEvent::Removed {
                service: self.service.clone(),
                endpoint: self.endpoint.clone(),
            });
        }
    }
}

impl Drop for ServiceLease {
    fn drop(&mut self) {
        self.release();
    }
}

/// Receives dynamic endpoint changes for one service.
pub struct ServiceSubscription {
    service: String,
    endpoints: BTreeSet<String>,
    receiver: broadcast::Receiver<ServiceEvent>,
    state: Arc<RegistryState>,
}

impl ServiceSubscription {
    /// Returns the known endpoint set in stable order.
    pub fn endpoints(&self) -> Vec<String> {
        self.endpoints.iter().cloned().collect()
    }

    pub fn discovered_endpoints(&self) -> Vec<DiscoveredEndpoint> {
        self.state
            .services
            .lock()
            .expect("service registry mutex poisoned")
            .get(&self.service)
            .into_iter()
            .flat_map(|endpoints| endpoints.values())
            .map(|entry| entry.endpoint.clone())
            .collect()
    }

    /// Replaces the local snapshot with the registry's current endpoints after a lagged stream.
    pub fn resync(&mut self) {
        self.endpoints = self
            .state
            .services
            .lock()
            .expect("service registry mutex poisoned")
            .get(&self.service)
            .into_iter()
            .flat_map(|endpoints| endpoints.keys())
            .cloned()
            .collect();
    }

    /// Waits for the next effective endpoint change.
    pub async fn recv(&mut self) -> Result<ServiceEvent, DiscoveryError> {
        loop {
            match self.receiver.recv().await {
                Ok(event) if event.service() == self.service => {
                    let changed = match &event {
                        ServiceEvent::Added { endpoint, .. } => {
                            self.endpoints.insert(endpoint.clone())
                        }
                        ServiceEvent::Removed { endpoint, .. } => self.endpoints.remove(endpoint),
                    };
                    if changed {
                        return Ok(event);
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    return Err(DiscoveryError::SubscriptionLagged(skipped));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(DiscoveryError::RegistryClosed);
                }
            }
        }
    }
}

impl EndpointSubscription for ServiceSubscription {
    type Error = DiscoveryError;

    fn endpoints(&self) -> Vec<String> {
        ServiceSubscription::endpoints(self)
    }

    fn discovered_endpoints(&self) -> Vec<DiscoveredEndpoint> {
        ServiceSubscription::discovered_endpoints(self)
    }

    fn changed(&mut self) -> EndpointChangeFuture<'_, Self::Error> {
        Box::pin(async move {
            match self.recv().await {
                Ok(_) => Ok(self.endpoints()),
                Err(DiscoveryError::SubscriptionLagged(_)) => {
                    self.resync();
                    Ok(self.endpoints())
                }
                Err(error) => Err(error),
            }
        })
    }
}

/// A service endpoint change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceEvent {
    Added { service: String, endpoint: String },
    Removed { service: String, endpoint: String },
}

impl ServiceEvent {
    fn service(&self) -> &str {
        match self {
            Self::Added { service, .. } | Self::Removed { service, .. } => service,
        }
    }
}

/// Errors produced while publishing or subscribing to services.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryError {
    EmptyService,
    EmptyEndpoint,
    InvalidEndpointWeight(u32),
    ConflictingEndpointMetadata(String),
    SubscriptionLagged(u64),
    RegistryClosed,
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyService => formatter.write_str("service name cannot be empty"),
            Self::EmptyEndpoint => formatter.write_str("service endpoint cannot be empty"),
            Self::InvalidEndpointWeight(weight) => write!(
                formatter,
                "service endpoint weight must be between 1 and {MAX_ENDPOINT_WEIGHT}, got {weight}"
            ),
            Self::ConflictingEndpointMetadata(endpoint) => write!(
                formatter,
                "service endpoint {endpoint} is already published with different metadata"
            ),
            Self::SubscriptionLagged(skipped) => {
                write!(formatter, "service subscription lagged by {skipped} events")
            }
            Self::RegistryClosed => formatter.write_str("service registry has closed"),
        }
    }
}

impl std::error::Error for DiscoveryError {}

fn validate_service(service: String) -> Result<String, DiscoveryError> {
    if service.trim().is_empty() {
        Err(DiscoveryError::EmptyService)
    } else {
        Ok(service)
    }
}

fn validate_endpoint(endpoint: String) -> Result<String, DiscoveryError> {
    if endpoint.trim().is_empty() {
        Err(DiscoveryError::EmptyEndpoint)
    } else {
        Ok(endpoint)
    }
}

fn validate_weight(weight: u32) -> Result<u32, DiscoveryError> {
    if (1..=MAX_ENDPOINT_WEIGHT).contains(&weight) {
        Ok(weight)
    } else {
        Err(DiscoveryError::InvalidEndpointWeight(weight))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DiscoveredEndpoint, DiscoveryError, EndpointSubscription, ServiceEvent, ServiceRegistry,
    };

    #[tokio::test]
    async fn publishes_and_withdraws_endpoints() {
        let registry = ServiceRegistry::new();
        let mut subscription = registry.subscribe("users").unwrap();
        let mut first = registry.publish("users", "http://users-a:8080").unwrap();
        let second = registry.publish("users", "http://users-b:8080").unwrap();

        assert_eq!(
            subscription.recv().await.unwrap(),
            ServiceEvent::Added {
                service: "users".to_owned(),
                endpoint: "http://users-a:8080".to_owned(),
            }
        );
        assert_eq!(
            subscription.recv().await.unwrap(),
            ServiceEvent::Added {
                service: "users".to_owned(),
                endpoint: "http://users-b:8080".to_owned(),
            }
        );
        assert_eq!(
            subscription.endpoints(),
            vec![
                "http://users-a:8080".to_owned(),
                "http://users-b:8080".to_owned()
            ]
        );

        first.release();
        assert_eq!(
            subscription.recv().await.unwrap(),
            ServiceEvent::Removed {
                service: "users".to_owned(),
                endpoint: "http://users-a:8080".to_owned(),
            }
        );
        assert_eq!(
            registry.endpoints("users").unwrap(),
            vec!["http://users-b:8080"]
        );

        drop(second);
    }

    #[tokio::test]
    async fn keeps_endpoint_published_until_its_last_lease_is_released() {
        let registry = ServiceRegistry::new();
        let mut subscription = registry.subscribe("users").unwrap();
        let first = registry.publish("users", "http://users-a:8080").unwrap();
        let second = registry.publish("users", "http://users-a:8080").unwrap();

        assert!(matches!(
            subscription.recv().await,
            Ok(ServiceEvent::Added { .. })
        ));
        drop(first);
        assert_eq!(
            registry.endpoints("users").unwrap(),
            vec!["http://users-a:8080"]
        );

        drop(second);
        assert!(matches!(
            subscription.recv().await,
            Ok(ServiceEvent::Removed { .. })
        ));
    }

    #[test]
    fn rejects_empty_service_names_and_endpoints() {
        let registry = ServiceRegistry::new();

        assert_eq!(
            registry.publish("", "http://users:8080").unwrap_err(),
            DiscoveryError::EmptyService
        );
        assert_eq!(
            registry.publish("users", " ").unwrap_err(),
            DiscoveryError::EmptyEndpoint
        );
        assert_eq!(
            registry
                .publish_weighted("users", "http://users:8080", 0)
                .unwrap_err(),
            DiscoveryError::InvalidEndpointWeight(0)
        );
    }

    #[test]
    fn preserves_weighted_endpoint_metadata_in_subscriptions() {
        let registry = ServiceRegistry::new();
        let endpoint = DiscoveredEndpoint::weighted("http://users:8080", 3)
            .unwrap()
            .with_metadata([("zone".to_owned(), "east".to_owned())]);
        let _lease = registry
            .publish_endpoint("users", endpoint.clone())
            .unwrap();
        let subscription = registry.subscribe("users").unwrap();

        assert_eq!(subscription.discovered_endpoints(), vec![endpoint.clone()]);
        assert_eq!(
            EndpointSubscription::discovered_endpoints(&subscription),
            vec![endpoint]
        );
    }

    #[test]
    fn rejects_conflicting_metadata_for_the_same_live_endpoint() {
        let registry = ServiceRegistry::new();
        let _lease = registry
            .publish_weighted("users", "http://users:8080", 2)
            .unwrap();

        assert_eq!(
            registry
                .publish_weighted("users", "http://users:8080", 3)
                .unwrap_err(),
            DiscoveryError::ConflictingEndpointMetadata("http://users:8080".to_owned())
        );
    }
}
