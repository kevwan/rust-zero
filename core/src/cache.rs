use std::{
    collections::HashMap,
    hash::Hash,
    sync::Mutex,
    time::{Duration, Instant},
};

struct Entry<V> {
    value: V,
    expires_at: Instant,
}

/// A thread-safe in-memory cache that expires entries on access.
pub struct TtlCache<K, V> {
    entries: Mutex<HashMap<K, Entry<V>>>,
}

impl<K, V> Default for TtlCache<K, V>
where
    K: Eq + Hash,
{
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl<K, V> TtlCache<K, V>
where
    K: Eq + Hash,
{
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, key: K, value: V, ttl: Duration) {
        assert!(!ttl.is_zero(), "cache TTL must be greater than zero");
        self.entries.lock().expect("cache lock poisoned").insert(
            key,
            Entry {
                value,
                expires_at: Instant::now() + ttl,
            },
        );
    }

    pub fn get(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        let mut entries = self.entries.lock().expect("cache lock poisoned");
        let expired = entries
            .get(key)
            .is_some_and(|entry| entry.expires_at <= Instant::now());
        if expired {
            entries.remove(key);
            None
        } else {
            entries.get(key).map(|entry| entry.value.clone())
        }
    }

    pub fn remove(&self, key: &K) -> Option<V> {
        self.entries
            .lock()
            .expect("cache lock poisoned")
            .remove(key)
            .map(|entry| entry.value)
    }

    pub fn len(&self) -> usize {
        let now = Instant::now();
        let mut entries = self.entries.lock().expect("cache lock poisoned");
        entries.retain(|_, entry| entry.expires_at > now);
        entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn returns_a_value_until_its_ttl_expires() {
        let cache = TtlCache::new();
        cache.insert("user-42", "cached", Duration::from_millis(5));

        assert_eq!(cache.get(&"user-42"), Some("cached"));
        thread::sleep(Duration::from_millis(10));
        assert_eq!(cache.get(&"user-42"), None);
        assert!(cache.is_empty());
    }

    #[test]
    fn remove_returns_the_cached_value() {
        let cache = TtlCache::new();
        cache.insert("user-42", 42, Duration::from_secs(1));

        assert_eq!(cache.remove(&"user-42"), Some(42));
        assert_eq!(cache.get(&"user-42"), None);
    }
}
