use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    hash::Hash,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

/// Adds a bounded, deterministic-per-call spread to an expiry duration.
///
/// Cache adapters use this to avoid a large set of records expiring at exactly the same instant.
/// The caller owns the sequence so independent cache instances do not contend on a global RNG.
#[cfg(any(feature = "stores-sql", feature = "stores-mongo", test))]
pub(crate) fn jittered_ttl(base: Duration, jitter: Duration, sequence: u64) -> Duration {
    if jitter.is_zero() {
        return base;
    }

    // SplitMix64 gives a well-distributed value without pulling an RNG into the cache hot path.
    let mut value = sequence.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^= value >> 31;

    let jitter_nanos = jitter.as_nanos();
    let added_nanos = u128::from(value) % (jitter_nanos.saturating_add(1));
    let added = Duration::new(
        (added_nanos / 1_000_000_000).min(u128::from(u64::MAX)) as u64,
        (added_nanos % 1_000_000_000) as u32,
    );
    base.saturating_add(added)
}

use crate::{SingleFlight, SingleFlightError};

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

/// A snapshot of cache hit, miss, insertion, and eviction counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub insertions: u64,
    pub evictions: u64,
}

#[derive(Default)]
struct CacheCounters {
    hits: AtomicU64,
    misses: AtomicU64,
    insertions: AtomicU64,
    evictions: AtomicU64,
}

struct MemoryState<K, V> {
    entries: HashMap<K, Entry<V>>,
    least_to_most_recent: VecDeque<K>,
}

/// A bounded in-process LRU cache with per-entry expiry and cache statistics.
///
/// Unlike [`TtlCache`], this cache has a hard item limit. Reads refresh recency, and expired
/// entries are removed lazily. This mirrors go-zero's bounded memory-cache behavior without
/// requiring a background timing-wheel task.
pub struct MemoryCache<K, V> {
    capacity: usize,
    state: Mutex<MemoryState<K, V>>,
    counters: CacheCounters,
}

impl<K, V> MemoryCache<K, V>
where
    K: Clone + Eq + Hash,
{
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "cache capacity must be greater than zero");
        Self {
            capacity,
            state: Mutex::new(MemoryState {
                entries: HashMap::new(),
                least_to_most_recent: VecDeque::new(),
            }),
            counters: CacheCounters::default(),
        }
    }

    pub fn insert(&self, key: K, value: V, ttl: Duration) {
        assert!(!ttl.is_zero(), "cache TTL must be greater than zero");
        let mut state = self.state.lock().expect("cache lock poisoned");
        remove_from_recency(&mut state.least_to_most_recent, &key);
        state.least_to_most_recent.push_back(key.clone());
        state.entries.insert(
            key,
            Entry {
                value,
                expires_at: Instant::now() + ttl,
            },
        );
        self.counters.insertions.fetch_add(1, Ordering::Relaxed);

        while state.entries.len() > self.capacity {
            let Some(oldest) = state.least_to_most_recent.pop_front() else {
                break;
            };
            if state.entries.remove(&oldest).is_some() {
                self.counters.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn get(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        let mut state = self.state.lock().expect("cache lock poisoned");
        let now = Instant::now();
        let expired = state
            .entries
            .get(key)
            .is_some_and(|entry| entry.expires_at <= now);
        if expired {
            state.entries.remove(key);
            remove_from_recency(&mut state.least_to_most_recent, key);
            self.counters.evictions.fetch_add(1, Ordering::Relaxed);
            self.counters.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        let value = state.entries.get(key).map(|entry| entry.value.clone());
        if value.is_some() {
            remove_from_recency(&mut state.least_to_most_recent, key);
            state.least_to_most_recent.push_back(key.clone());
            self.counters.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters.misses.fetch_add(1, Ordering::Relaxed);
        }
        value
    }

    pub fn remove(&self, key: &K) -> Option<V> {
        let mut state = self.state.lock().expect("cache lock poisoned");
        remove_from_recency(&mut state.least_to_most_recent, key);
        state.entries.remove(key).map(|entry| entry.value)
    }

    pub fn clear(&self) {
        let mut state = self.state.lock().expect("cache lock poisoned");
        state.entries.clear();
        state.least_to_most_recent.clear();
    }

    pub fn len(&self) -> usize {
        let now = Instant::now();
        let mut state = self.state.lock().expect("cache lock poisoned");
        let expired: Vec<_> = state
            .entries
            .iter()
            .filter(|(_, entry)| entry.expires_at <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in &expired {
            state.entries.remove(key);
            remove_from_recency(&mut state.least_to_most_recent, key);
        }
        self.counters
            .evictions
            .fetch_add(expired.len() as u64, Ordering::Relaxed);
        state.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.counters.hits.load(Ordering::Relaxed),
            misses: self.counters.misses.load(Ordering::Relaxed),
            insertions: self.counters.insertions.load(Ordering::Relaxed),
            evictions: self.counters.evictions.load(Ordering::Relaxed),
        }
    }
}

fn remove_from_recency<K>(recency: &mut VecDeque<K>, key: &K)
where
    K: Eq,
{
    if let Some(index) = recency.iter().position(|candidate| candidate == key) {
        recency.remove(index);
    }
}

/// A bounded cache that coalesces concurrent misses for the same key.
pub struct ReadThroughCache<K, V, E> {
    cache: MemoryCache<K, V>,
    flights: SingleFlight<K, V, E>,
}

impl<K, V, E> ReadThroughCache<K, V, E>
where
    K: Clone + Eq + Hash,
    V: Clone,
{
    pub fn new(capacity: usize) -> Self {
        Self {
            cache: MemoryCache::new(capacity),
            flights: SingleFlight::new(),
        }
    }

    pub fn get(&self, key: &K) -> Option<V> {
        self.cache.get(key)
    }

    /// Returns a cached value or invokes `fetch` once across concurrent callers.
    pub async fn take<F, Fut>(
        &self,
        key: K,
        ttl: Duration,
        fetch: F,
    ) -> Result<V, SingleFlightError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>>,
    {
        if let Some(value) = self.cache.get(&key) {
            return Ok(value);
        }

        self.flights
            .execute(key.clone(), || async {
                if let Some(value) = self.cache.get(&key) {
                    return Ok(value);
                }
                let value = fetch().await?;
                self.cache.insert(key, value.clone(), ttl);
                Ok(value)
            })
            .await
    }

    pub fn remove(&self, key: &K) -> Option<V> {
        self.cache.remove(key)
    }

    pub fn stats(&self) -> CacheStats {
        self.cache.stats()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_jitter_is_bounded_and_varies_by_sequence() {
        let base = Duration::from_secs(10);
        let jitter = Duration::from_secs(5);
        let values: Vec<_> = (0..8)
            .map(|sequence| jittered_ttl(base, jitter, sequence))
            .collect();

        assert!(values
            .iter()
            .all(|ttl| *ttl >= base && *ttl <= base + jitter));
        assert!(values.windows(2).any(|pair| pair[0] != pair[1]));
        assert_eq!(jittered_ttl(base, Duration::ZERO, 99), base);
    }
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

    #[test]
    fn bounded_cache_evicts_the_least_recently_used_value() {
        let cache = MemoryCache::new(2);
        cache.insert("one", 1, Duration::from_secs(1));
        cache.insert("two", 2, Duration::from_secs(1));
        assert_eq!(cache.get(&"one"), Some(1));
        cache.insert("three", 3, Duration::from_secs(1));

        assert_eq!(cache.get(&"two"), None);
        assert_eq!(cache.get(&"one"), Some(1));
        assert_eq!(cache.get(&"three"), Some(3));
        assert_eq!(cache.stats().evictions, 1);
    }

    #[tokio::test]
    async fn read_through_cache_fetches_and_reuses_a_value() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cache = ReadThroughCache::<String, usize, String>::new(10);
        let calls = AtomicUsize::new(0);
        for _ in 0..2 {
            let value = cache
                .take("answer".to_owned(), Duration::from_secs(1), || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(42)
                })
                .await
                .unwrap();
            assert_eq!(value, 42);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
