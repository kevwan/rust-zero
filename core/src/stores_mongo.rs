use mongodb::{
    bson::doc, error::Error as DriverError, options::ClientOptions, Client, ClientSession,
    Collection, Database,
};
use std::{
    error::Error,
    fmt,
    future::Future,
    hash::Hash,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::Duration,
};

use crate::{cache::jittered_ttl, CacheStats, MemoryCache, SingleFlight, SingleFlightError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MongoStoreConfig {
    pub uri: String,
    pub database: String,
    pub application_name: Option<String>,
    pub min_pool_size: Option<u32>,
    pub max_pool_size: Option<u32>,
    pub connect_timeout: Option<Duration>,
    pub server_selection_timeout: Option<Duration>,
}

impl MongoStoreConfig {
    pub fn new(uri: impl Into<String>, database: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            database: database.into(),
            application_name: None,
            min_pool_size: None,
            max_pool_size: None,
            connect_timeout: Some(Duration::from_secs(10)),
            server_selection_timeout: Some(Duration::from_secs(10)),
        }
    }

    pub fn with_application_name(mut self, name: impl Into<String>) -> Self {
        self.application_name = Some(name.into());
        self
    }

    pub fn with_pool_size(mut self, min: u32, max: u32) -> Self {
        assert!(max > 0, "MongoDB maximum pool size must be positive");
        assert!(
            min <= max,
            "MongoDB minimum pool size cannot exceed maximum"
        );
        self.min_pool_size = Some(min);
        self.max_pool_size = Some(max);
        self
    }

    pub fn with_timeouts(mut self, connect: Duration, server_selection: Duration) -> Self {
        assert!(
            !connect.is_zero(),
            "MongoDB connect timeout must be positive"
        );
        assert!(
            !server_selection.is_zero(),
            "MongoDB server selection timeout must be positive"
        );
        self.connect_timeout = Some(connect);
        self.server_selection_timeout = Some(server_selection);
        self
    }
}

/// Settings for the in-process cache used by [`CachedMongoCollection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MongoCacheConfig {
    pub capacity: usize,
    pub ttl: Duration,
    pub not_found_ttl: Option<Duration>,
    pub ttl_jitter: Duration,
}

impl MongoCacheConfig {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        assert!(capacity > 0, "MongoDB cache capacity must be positive");
        assert!(!ttl.is_zero(), "MongoDB cache TTL must be positive");
        Self {
            capacity,
            ttl,
            not_found_ttl: Some(ttl),
            ttl_jitter: Duration::ZERO,
        }
    }

    /// Controls negative caching. `None` makes every missing record hit MongoDB.
    pub fn with_not_found_ttl(mut self, ttl: Option<Duration>) -> Self {
        assert!(
            ttl.is_none_or(|ttl| !ttl.is_zero()),
            "MongoDB not-found cache TTL must be positive"
        );
        self.not_found_ttl = ttl;
        self
    }

    /// Randomizes each positive and negative expiry by up to `jitter`.
    pub fn with_ttl_jitter(mut self, jitter: Duration) -> Self {
        self.ttl_jitter = jitter;
        self
    }
}

#[derive(Debug)]
pub enum MongoStoreError {
    InvalidDatabase,
    Driver(DriverError),
}

impl fmt::Display for MongoStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDatabase => formatter.write_str("MongoDB database name cannot be empty"),
            Self::Driver(error) => write!(formatter, "MongoDB operation failed: {error}"),
        }
    }
}

impl Error for MongoStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidDatabase => None,
            Self::Driver(error) => Some(error),
        }
    }
}

impl From<DriverError> for MongoStoreError {
    fn from(error: DriverError) -> Self {
        Self::Driver(error)
    }
}

/// A reusable MongoDB client and selected database.
#[derive(Debug, Clone)]
pub struct MongoStore {
    client: Client,
    database: Database,
}

impl MongoStore {
    pub async fn connect(config: MongoStoreConfig) -> Result<Self, MongoStoreError> {
        if config.database.trim().is_empty() {
            return Err(MongoStoreError::InvalidDatabase);
        }

        let mut options = ClientOptions::parse(&config.uri).await?;
        options.app_name = config.application_name;
        options.min_pool_size = config.min_pool_size;
        options.max_pool_size = config.max_pool_size;
        options.connect_timeout = config.connect_timeout;
        options.server_selection_timeout = config.server_selection_timeout;
        let client = Client::with_options(options)?;
        let database = client.database(&config.database);
        Ok(Self { client, database })
    }

    pub fn from_client(client: Client, database: impl AsRef<str>) -> Result<Self, MongoStoreError> {
        let database = database.as_ref();
        if database.trim().is_empty() {
            return Err(MongoStoreError::InvalidDatabase);
        }
        Ok(Self {
            database: client.database(database),
            client,
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    pub fn collection<T>(&self, name: impl AsRef<str>) -> Collection<T>
    where
        T: Send + Sync,
    {
        self.database.collection(name.as_ref())
    }

    pub fn cached_collection<K, T>(
        &self,
        name: impl AsRef<str>,
        config: MongoCacheConfig,
    ) -> CachedMongoCollection<K, T>
    where
        K: Clone + Eq + Hash,
        T: Clone + Send + Sync,
    {
        CachedMongoCollection::new(self.collection(name), config)
    }

    pub async fn health_check(&self) -> Result<(), MongoStoreError> {
        self.database.run_command(doc! { "ping": 1 }).await?;
        Ok(())
    }

    /// Starts a client session and transaction. Pass the returned session to each operation.
    pub async fn begin(&self) -> Result<ClientSession, MongoStoreError> {
        let mut session = self.client.start_session().await?;
        session.start_transaction().await?;
        Ok(session)
    }
}

/// A typed MongoDB collection with bounded positive/negative record caching.
pub struct CachedMongoCollection<K, T>
where
    T: Send + Sync,
{
    collection: Collection<T>,
    cache: MemoryCache<K, Option<T>>,
    flights: SingleFlight<K, Option<T>, DriverError>,
    ttl: Duration,
    not_found_ttl: Option<Duration>,
    ttl_jitter: Duration,
    expiry_sequence: AtomicU64,
    generation: AtomicU64,
    cache_gate: Mutex<()>,
}

impl<K, T> CachedMongoCollection<K, T>
where
    K: Clone + Eq + Hash,
    T: Clone + Send + Sync,
{
    pub fn new(collection: Collection<T>, config: MongoCacheConfig) -> Self {
        Self {
            collection,
            cache: MemoryCache::new(config.capacity),
            flights: SingleFlight::new(),
            ttl: config.ttl,
            not_found_ttl: config.not_found_ttl,
            ttl_jitter: config.ttl_jitter,
            expiry_sequence: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            cache_gate: Mutex::new(()),
        }
    }

    pub fn collection(&self) -> &Collection<T> {
        &self.collection
    }

    pub async fn find<F, Fut>(
        &self,
        key: K,
        query: F,
    ) -> Result<Option<T>, SingleFlightError<DriverError>>
    where
        F: FnOnce(Collection<T>) -> Fut,
        Fut: Future<Output = Result<Option<T>, DriverError>>,
    {
        if let Some(value) = self.cache.get(&key) {
            return Ok(value);
        }

        self.flights
            .execute(key.clone(), || async {
                if let Some(value) = self.cache.get(&key) {
                    return Ok(value);
                }
                let generation = self.generation.load(Ordering::Acquire);
                let value = query(self.collection.clone()).await?;
                let _guard = self.cache_gate.lock().expect("MongoDB cache gate poisoned");
                if self.generation.load(Ordering::Acquire) == generation {
                    let base_ttl = if value.is_some() {
                        Some(self.ttl)
                    } else {
                        self.not_found_ttl
                    };
                    if let Some(base_ttl) = base_ttl {
                        let sequence = self.expiry_sequence.fetch_add(1, Ordering::Relaxed);
                        self.cache.insert(
                            key,
                            value.clone(),
                            jittered_ttl(base_ttl, self.ttl_jitter, sequence),
                        );
                    }
                }
                Ok(value)
            })
            .await
    }

    pub async fn execute<I, F, Fut, R>(&self, keys: I, operation: F) -> Result<R, DriverError>
    where
        I: IntoIterator<Item = K>,
        F: FnOnce(Collection<T>) -> Fut,
        Fut: Future<Output = Result<R, DriverError>>,
    {
        let result = operation(self.collection.clone()).await?;
        self.invalidate_many(keys);
        Ok(result)
    }

    pub fn invalidate(&self, key: &K) -> bool {
        let _guard = self.cache_gate.lock().expect("MongoDB cache gate poisoned");
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.cache.remove(key).is_some()
    }

    pub fn invalidate_many<I>(&self, keys: I)
    where
        I: IntoIterator<Item = K>,
    {
        let _guard = self.cache_gate.lock().expect("MongoDB cache gate poisoned");
        self.generation.fetch_add(1, Ordering::AcqRel);
        for key in keys {
            self.cache.remove(&key);
        }
    }

    pub fn cache_stats(&self) -> CacheStats {
        self.cache.stats()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct User {
        #[serde(rename = "_id")]
        id: i64,
        name: String,
    }

    #[test]
    fn cache_policy_supports_jitter_and_optional_negative_entries() {
        let config = MongoCacheConfig::new(128, Duration::from_secs(30))
            .with_not_found_ttl(None)
            .with_ttl_jitter(Duration::from_secs(5));

        assert_eq!(config.not_found_ttl, None);
        assert_eq!(config.ttl_jitter, Duration::from_secs(5));
    }

    #[tokio::test]
    async fn validates_configuration_and_caches_positive_and_negative_results() {
        let error = MongoStore::connect(MongoStoreConfig::new("mongodb://127.0.0.1:27017", " "))
            .await
            .unwrap_err();
        assert!(matches!(error, MongoStoreError::InvalidDatabase));

        let store = MongoStore::connect(MongoStoreConfig::new(
            "mongodb://127.0.0.1:27017",
            "rust_zero_test",
        ))
        .await
        .unwrap();
        let cached = store.cached_collection::<i64, User>(
            "users",
            MongoCacheConfig::new(10, Duration::from_secs(30)),
        );
        let queries = AtomicUsize::new(0);

        for _ in 0..2 {
            let queries = &queries;
            let user = cached
                .find(7, move |_collection| async move {
                    queries.fetch_add(1, Ordering::SeqCst);
                    Ok(Some(User {
                        id: 7,
                        name: "Ada".to_owned(),
                    }))
                })
                .await
                .unwrap();
            assert_eq!(user.unwrap().name, "Ada");
        }
        for _ in 0..2 {
            let queries = &queries;
            assert!(cached
                .find(404, move |_collection| async move {
                    queries.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                })
                .await
                .unwrap()
                .is_none());
        }
        assert_eq!(queries.load(Ordering::SeqCst), 2);
        assert!(cached.invalidate(&7));
    }

    #[tokio::test]
    async fn mongodb_integration_covers_health_crud_cache_and_transactions() {
        let Ok(uri) = std::env::var("RUST_ZERO_MONGODB_URI") else {
            return;
        };
        let database = format!("rust_zero_{}", std::process::id());
        let store = MongoStore::connect(MongoStoreConfig::new(uri, &database))
            .await
            .unwrap();
        store.health_check().await.unwrap();
        let users = store.cached_collection::<i64, User>(
            "users",
            MongoCacheConfig::new(100, Duration::from_secs(30)),
        );
        users
            .collection()
            .insert_one(User {
                id: 7,
                name: "Ada".to_owned(),
            })
            .await
            .unwrap();
        let user = users
            .find(7, |collection| async move {
                collection.find_one(doc! { "_id": 7_i64 }).await
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.name, "Ada");
        users
            .execute([7], |collection| async move {
                collection
                    .update_one(doc! { "_id": 7_i64 }, doc! { "$set": { "name": "Grace" } })
                    .await
            })
            .await
            .unwrap();
        let user = users
            .find(7, |collection| async move {
                collection.find_one(doc! { "_id": 7_i64 }).await
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.name, "Grace");

        if std::env::var_os("RUST_ZERO_MONGODB_TRANSACTIONS").is_some() {
            let mut transaction = store.begin().await.unwrap();
            users
                .collection()
                .delete_one(doc! { "_id": 7_i64 })
                .session(&mut transaction)
                .await
                .unwrap();
            transaction.abort_transaction().await.unwrap();
        }
        store.database().drop().await.unwrap();
    }
}
