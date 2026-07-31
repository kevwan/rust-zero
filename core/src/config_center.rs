use crate::{parse_config, ConfigError, ConfigFormat};
use serde::de::DeserializeOwned;
use std::{
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock,
    },
};
use tokio::sync::watch;

/// An immutable, versioned view of a dynamic configuration value.
#[derive(Debug)]
pub struct ConfigSnapshot<T> {
    generation: u64,
    raw: Arc<str>,
    value: Arc<T>,
}

impl<T> Clone for ConfigSnapshot<T> {
    fn clone(&self) -> Self {
        Self {
            generation: self.generation,
            raw: Arc::clone(&self.raw),
            value: Arc::clone(&self.value),
        }
    }
}

impl<T> ConfigSnapshot<T> {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub fn value(&self) -> &Arc<T> {
        &self.value
    }
}

struct DynamicConfigState<T> {
    snapshot: RwLock<ConfigSnapshot<T>>,
    generation: AtomicU64,
    updates: watch::Sender<ConfigSnapshot<T>>,
}

/// A typed configuration-center primitive with atomic updates and subscriptions.
///
/// Backend adapters feed new serialized values through [`Self::update`]. Invalid
/// updates are rejected without disturbing the last known-good snapshot.
#[derive(Clone)]
pub struct DynamicConfig<T> {
    format: ConfigFormat,
    state: Arc<DynamicConfigState<T>>,
}

impl<T> DynamicConfig<T>
where
    T: DeserializeOwned + Send + Sync + 'static,
{
    pub fn new(contents: &str, format: ConfigFormat) -> Result<Self, ConfigCenterError> {
        let value = Arc::new(parse_config(contents, format)?);
        let snapshot = ConfigSnapshot {
            generation: 1,
            raw: Arc::from(contents),
            value,
        };
        let (updates, _) = watch::channel(snapshot.clone());

        Ok(Self {
            format,
            state: Arc::new(DynamicConfigState {
                snapshot: RwLock::new(snapshot),
                generation: AtomicU64::new(1),
                updates,
            }),
        })
    }

    pub fn snapshot(&self) -> ConfigSnapshot<T> {
        self.state
            .snapshot
            .read()
            .expect("dynamic configuration lock poisoned")
            .clone()
    }

    pub fn current(&self) -> Arc<T> {
        Arc::clone(self.snapshot().value())
    }

    /// Subscribes to future changes. The receiver starts with the current value.
    pub fn subscribe(&self) -> watch::Receiver<ConfigSnapshot<T>> {
        self.state.updates.subscribe()
    }

    /// Atomically installs a new value after parsing it successfully.
    pub fn update(&self, contents: &str) -> Result<ConfigSnapshot<T>, ConfigCenterError> {
        let value = Arc::new(parse_config(contents, self.format)?);
        let generation = self.state.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let snapshot = ConfigSnapshot {
            generation,
            raw: Arc::from(contents),
            value,
        };

        *self
            .state
            .snapshot
            .write()
            .expect("dynamic configuration lock poisoned") = snapshot.clone();
        self.state.updates.send_replace(snapshot.clone());
        Ok(snapshot)
    }
}

#[derive(Debug)]
pub struct ConfigCenterError(ConfigError);

impl fmt::Display for ConfigCenterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "dynamic configuration update failed: {}", self.0)
    }
}

impl std::error::Error for ConfigCenterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl From<ConfigError> for ConfigCenterError {
    fn from(error: ConfigError) -> Self {
        Self(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct Limits {
        requests: u64,
    }

    #[tokio::test]
    async fn publishes_valid_atomic_updates() {
        let config = DynamicConfig::<Limits>::new("requests = 10", ConfigFormat::Toml).unwrap();
        let mut changes = config.subscribe();

        let snapshot = config.update("requests = 20").unwrap();
        changes.changed().await.unwrap();

        assert_eq!(snapshot.generation(), 2);
        assert_eq!(changes.borrow().value().requests, 20);
        assert_eq!(config.current().requests, 20);
    }

    #[test]
    fn retains_last_known_good_value() {
        let config = DynamicConfig::<Limits>::new("requests = 10", ConfigFormat::Toml).unwrap();

        assert!(config.update("requests = \"invalid\"").is_err());
        assert_eq!(config.snapshot().generation(), 1);
        assert_eq!(config.current().requests, 10);
    }
}
