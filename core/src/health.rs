use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

/// Cloneable aggregate of named dependency readiness states.
///
/// An empty registry is healthy. Once dependencies are registered, the aggregate is ready only
/// when every dependency is ready. Updates are watchable so transports can project health without
/// polling application code.
#[derive(Debug, Clone)]
pub struct HealthRegistry {
    state: Arc<Mutex<BTreeMap<String, bool>>>,
    updates: watch::Sender<HealthSnapshot>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HealthSnapshot {
    pub dependencies: BTreeMap<String, bool>,
}

impl HealthSnapshot {
    pub fn is_ready(&self) -> bool {
        self.dependencies.values().all(|ready| *ready)
    }

    pub fn unhealthy(&self) -> Vec<String> {
        self.dependencies
            .iter()
            .filter_map(|(name, ready)| (!ready).then_some(name.clone()))
            .collect()
    }
}

impl Default for HealthRegistry {
    fn default() -> Self {
        let snapshot = HealthSnapshot::default();
        let (updates, _) = watch::channel(snapshot);
        Self {
            state: Arc::new(Mutex::new(BTreeMap::new())),
            updates,
        }
    }
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, name: impl Into<String>, ready: bool) {
        let mut state = self.state.lock().expect("health registry mutex poisoned");
        state.insert(name.into(), ready);
        self.updates.send_replace(HealthSnapshot {
            dependencies: state.clone(),
        });
    }

    pub fn remove(&self, name: &str) {
        let mut state = self.state.lock().expect("health registry mutex poisoned");
        state.remove(name);
        self.updates.send_replace(HealthSnapshot {
            dependencies: state.clone(),
        });
    }

    pub fn snapshot(&self) -> HealthSnapshot {
        self.updates.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<HealthSnapshot> {
        self.updates.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn aggregates_and_publishes_dependency_readiness() {
        let registry = HealthRegistry::new();
        let mut updates = registry.subscribe();
        assert!(registry.snapshot().is_ready());

        registry.set("users", false);
        updates.changed().await.unwrap();
        assert_eq!(updates.borrow().unhealthy(), vec!["users"]);

        registry.set("users", true);
        updates.changed().await.unwrap();
        assert!(updates.borrow().is_ready());
    }
}
