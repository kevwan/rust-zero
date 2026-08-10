//! MongoDB collections, transactions, instrumentation, and cache-aside records.
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use rust_zero_core::{MongoStore, MongoStoreConfig};
//! let store = MongoStore::connect(MongoStoreConfig::new(
//!     "mongodb://127.0.0.1:27017", "service",
//! )).await?;
//! store.health_check().await?;
//! # Ok(())
//! # }
//! ```

use mongodb::{
    bson::{doc, Document},
    error::Error as DriverError,
    options::ClientOptions,
    results::{DeleteResult, InsertManyResult, InsertOneResult, UpdateResult},
    Client, ClientSession, Collection, Database,
};
use serde::Serialize;
use std::{
    error::Error,
    fmt,
    future::Future,
    hash::Hash,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use crate::{
    cache::jittered_ttl, CacheStats, CounterVec, HistogramOptions, HistogramVec, MemoryCache,
    Metrics, MetricsError, SingleFlight, SingleFlightError, VectorOptions,
};

#[cfg(feature = "telemetry")]
use crate::{TelemetrySpan, TelemetrySpanKind};

/// Cardinality-bounded MongoDB operation metrics used by [`MongoStore`]'s typed helpers.
#[derive(Clone)]
pub struct MongoStoreMetrics {
    operations: CounterVec,
    duration: HistogramVec,
}

impl MongoStoreMetrics {
    pub fn register(metrics: &Metrics) -> Result<Self, MetricsError> {
        let labels = ["operation", "kind", "outcome"];
        Ok(Self {
            operations: metrics.counter_vec(
                VectorOptions::new("operations_total", "Completed MongoDB store operations")
                    .with_namespace("rust_zero")
                    .with_subsystem("mongo")
                    .with_labels(labels),
            )?,
            duration: metrics.histogram_vec(
                HistogramOptions::new(
                    "operation_duration_seconds",
                    "MongoDB store operation latency",
                )
                .with_vector_options(
                    VectorOptions::new(
                        "operation_duration_seconds",
                        "MongoDB store operation latency",
                    )
                    .with_namespace("rust_zero")
                    .with_subsystem("mongo")
                    .with_labels(labels),
                ),
            )?,
        })
    }

    fn observe(&self, operation: &str, kind: MongoOperationKind, outcome: &str, elapsed: Duration) {
        let labels = [operation, kind.as_str(), outcome];
        let _ = self.operations.inc(&labels);
        let _ = self.duration.observe(elapsed.as_secs_f64(), &labels);
    }
}

#[derive(Debug, Clone, Copy)]
enum MongoOperationKind {
    Query,
    Execute,
    BulkInsert,
}

impl MongoOperationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Execute => "execute",
            Self::BulkInsert => "bulk_insert",
        }
    }
}

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
    InvalidBatchSize,
    NotFound { entity: String },
    Driver(DriverError),
}

impl MongoStoreError {
    pub fn not_found(entity: impl Into<String>) -> Self {
        Self::NotFound {
            entity: entity.into(),
        }
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. })
    }
}

impl fmt::Display for MongoStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDatabase => formatter.write_str("MongoDB database name cannot be empty"),
            Self::InvalidBatchSize => {
                formatter.write_str("MongoDB bulk batch size must be positive")
            }
            Self::NotFound { entity } => write!(formatter, "{entity} not found"),
            Self::Driver(error) => write!(formatter, "MongoDB operation failed: {error}"),
        }
    }
}

impl Error for MongoStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidDatabase | Self::InvalidBatchSize | Self::NotFound { .. } => None,
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
#[derive(Clone)]
pub struct MongoStore {
    client: Client,
    database: Database,
    metrics: Option<MongoStoreMetrics>,
}

impl fmt::Debug for MongoStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MongoStore")
            .field("client", &self.client)
            .field("database", &self.database)
            .field("instrumented", &self.metrics.is_some())
            .finish()
    }
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
        Ok(Self {
            client,
            database,
            metrics: None,
        })
    }

    pub fn from_client(client: Client, database: impl AsRef<str>) -> Result<Self, MongoStoreError> {
        let database = database.as_ref();
        if database.trim().is_empty() {
            return Err(MongoStoreError::InvalidDatabase);
        }
        Ok(Self {
            database: client.database(database),
            client,
            metrics: None,
        })
    }

    /// Installs metrics for operations run through the typed helper methods.
    pub fn with_metrics(mut self, metrics: MongoStoreMetrics) -> Self {
        self.metrics = Some(metrics);
        self
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

    /// Creates a cached collection with distinct primary- and secondary-key types.
    pub fn cached_indexed_collection<K, I, T>(
        &self,
        name: impl AsRef<str>,
        config: MongoCacheConfig,
    ) -> CachedMongoCollection<K, T, I>
    where
        K: Clone + Eq + Hash,
        I: Clone + Eq + Hash,
        T: Clone + Send + Sync,
    {
        CachedMongoCollection::new(self.collection(name), config)
    }

    pub async fn health_check(&self) -> Result<(), MongoStoreError> {
        self.database.run_command(doc! { "ping": 1 }).await?;
        Ok(())
    }

    /// Runs a typed collection query with consistent metrics, tracing, and error conversion.
    pub async fn query<T, R, F, Fut>(
        &self,
        operation: &'static str,
        collection: impl AsRef<str>,
        query: F,
    ) -> Result<R, MongoStoreError>
    where
        T: Send + Sync,
        F: FnOnce(Collection<T>) -> Fut,
        Fut: Future<Output = Result<R, DriverError>>,
    {
        let collection = self.collection(collection);
        self.instrument(operation, MongoOperationKind::Query, async move {
            query(collection).await.map_err(MongoStoreError::Driver)
        })
        .await
    }

    /// Runs an optional query and converts an absent document into a stable not-found error.
    pub async fn query_one<T, F, Fut>(
        &self,
        operation: &'static str,
        collection: impl AsRef<str>,
        entity: impl Into<String>,
        query: F,
    ) -> Result<T, MongoStoreError>
    where
        T: Send + Sync,
        F: FnOnce(Collection<T>) -> Fut,
        Fut: Future<Output = Result<Option<T>, DriverError>>,
    {
        let collection = self.collection(collection);
        let entity = entity.into();
        self.instrument(operation, MongoOperationKind::Query, async move {
            query(collection)
                .await
                .map_err(MongoStoreError::Driver)?
                .ok_or_else(|| MongoStoreError::not_found(entity))
        })
        .await
    }

    /// Runs a typed collection mutation with consistent metrics and tracing.
    pub async fn execute<T, R, F, Fut>(
        &self,
        operation: &'static str,
        collection: impl AsRef<str>,
        execute: F,
    ) -> Result<R, MongoStoreError>
    where
        T: Send + Sync,
        F: FnOnce(Collection<T>) -> Fut,
        Fut: Future<Output = Result<R, DriverError>>,
    {
        let collection = self.collection(collection);
        self.instrument(operation, MongoOperationKind::Execute, async move {
            execute(collection).await.map_err(MongoStoreError::Driver)
        })
        .await
    }

    /// Inserts documents with MongoDB's native `insert_many` in bounded batches.
    pub async fn bulk_insert<T>(
        &self,
        operation: &'static str,
        collection: impl AsRef<str>,
        items: impl IntoIterator<Item = T>,
        batch_size: usize,
    ) -> Result<Vec<InsertManyResult>, MongoStoreError>
    where
        T: Serialize + Send + Sync,
    {
        if batch_size == 0 {
            return Err(MongoStoreError::InvalidBatchSize);
        }

        let collection = self.collection::<T>(collection);
        let mut items = items.into_iter();
        self.instrument(operation, MongoOperationKind::BulkInsert, async move {
            let mut results = Vec::new();
            loop {
                let batch: Vec<_> = items.by_ref().take(batch_size).collect();
                if batch.is_empty() {
                    return Ok(results);
                }
                results.push(collection.insert_many(batch).await?);
            }
        })
        .await
    }

    async fn instrument<R, Fut>(
        &self,
        operation: &'static str,
        kind: MongoOperationKind,
        future: Fut,
    ) -> Result<R, MongoStoreError>
    where
        Fut: Future<Output = Result<R, MongoStoreError>>,
    {
        let started = Instant::now();
        #[cfg(feature = "telemetry")]
        let span = TelemetrySpan::start(
            format!("mongo.{operation}"),
            TelemetrySpanKind::Client,
            None,
            [
                ("db.operation.name", operation.to_owned()),
                ("rust_zero.mongo.kind", kind.as_str().to_owned()),
            ],
        );
        let result = future.await;
        let outcome = mongo_outcome(&result);
        if let Some(metrics) = &self.metrics {
            metrics.observe(operation, kind, outcome, started.elapsed());
        }
        #[cfg(feature = "telemetry")]
        if let Err(error) = &result {
            span.set_error(error.to_string());
        }
        result
    }

    /// Starts a client session and transaction. Pass the returned session to each operation.
    pub async fn begin(&self) -> Result<ClientSession, MongoStoreError> {
        let mut session = self.client.start_session().await?;
        session.start_transaction().await?;
        Ok(session)
    }
}

fn mongo_outcome<T>(result: &Result<T, MongoStoreError>) -> &'static str {
    match result {
        Ok(_) => "success",
        Err(MongoStoreError::NotFound { .. }) => "not_found",
        Err(
            MongoStoreError::InvalidDatabase
            | MongoStoreError::InvalidBatchSize
            | MongoStoreError::Driver(_),
        ) => "error",
    }
}

/// A typed MongoDB collection with bounded positive/negative record caching.
pub struct CachedMongoCollection<K, T, I = K>
where
    T: Send + Sync,
{
    collection: Collection<T>,
    cache: MemoryCache<K, Option<T>>,
    indexes: MemoryCache<I, Option<K>>,
    flights: SingleFlight<K, Option<T>, DriverError>,
    index_flights: SingleFlight<I, Option<(K, T)>, DriverError>,
    ttl: Duration,
    not_found_ttl: Option<Duration>,
    ttl_jitter: Duration,
    expiry_sequence: AtomicU64,
    generation: AtomicU64,
    cache_gate: Mutex<()>,
}

impl<K, T, I> CachedMongoCollection<K, T, I>
where
    K: Clone + Eq + Hash,
    I: Clone + Eq + Hash,
    T: Clone + Send + Sync,
{
    pub fn new(collection: Collection<T>, config: MongoCacheConfig) -> Self {
        Self {
            collection,
            cache: MemoryCache::new(config.capacity),
            indexes: MemoryCache::new(config.capacity),
            flights: SingleFlight::new(),
            index_flights: SingleFlight::new(),
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

    /// Returns a document selected by a secondary key while caching its primary-key mapping.
    pub async fn find_by_index<F, Fut>(
        &self,
        index: I,
        query: F,
    ) -> Result<Option<T>, SingleFlightError<DriverError>>
    where
        F: FnOnce(Collection<T>) -> Fut,
        Fut: Future<Output = Result<Option<(K, T)>, DriverError>>,
    {
        if let Some(primary) = self.indexes.get(&index) {
            match primary {
                Some(primary) => {
                    if let Some(value) = self.cache.get(&primary) {
                        return Ok(value);
                    }
                }
                None => return Ok(None),
            }
        }

        let loaded = self
            .index_flights
            .execute(index.clone(), || async {
                if let Some(primary) = self.indexes.get(&index) {
                    match primary {
                        Some(primary) => {
                            if let Some(Some(value)) = self.cache.get(&primary) {
                                return Ok(Some((primary, value)));
                            }
                        }
                        None => return Ok(None),
                    }
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
                        let ttl = jittered_ttl(base_ttl, self.ttl_jitter, sequence);
                        let primary = value.as_ref().map(|(primary, _)| primary.clone());
                        self.indexes.insert(index, primary, ttl);
                        if let Some((primary, value)) = &value {
                            self.cache.insert(primary.clone(), Some(value.clone()), ttl);
                        }
                    }
                }
                Ok(value)
            })
            .await?;
        Ok(loaded.map(|(_, value)| value))
    }

    pub async fn execute<PI, F, Fut, R>(&self, keys: PI, operation: F) -> Result<R, DriverError>
    where
        PI: IntoIterator<Item = K>,
        F: FnOnce(Collection<T>) -> Fut,
        Fut: Future<Output = Result<R, DriverError>>,
    {
        let result = operation(self.collection.clone()).await?;
        self.invalidate_many(keys);
        Ok(result)
    }

    /// Runs a mutation and invalidates primary keys plus explicitly affected secondary keys.
    ///
    /// Learned mappings for the primary keys are removed automatically. Pass changed/new index
    /// keys as well so negative index entries cannot hide inserts or updates.
    pub async fn execute_indexed<PI, SI, F, Fut, R>(
        &self,
        primary_keys: PI,
        index_keys: SI,
        operation: F,
    ) -> Result<R, DriverError>
    where
        PI: IntoIterator<Item = K>,
        SI: IntoIterator<Item = I>,
        F: FnOnce(Collection<T>) -> Fut,
        Fut: Future<Output = Result<R, DriverError>>,
    {
        let result = operation(self.collection.clone()).await?;
        self.invalidate_related(primary_keys, index_keys);
        Ok(result)
    }

    /// Inserts one document and invalidates any cached negative primary or secondary lookups.
    pub async fn insert_one<SI>(
        &self,
        primary_key: K,
        index_keys: SI,
        document: T,
    ) -> Result<InsertOneResult, DriverError>
    where
        SI: IntoIterator<Item = I>,
        T: Serialize,
    {
        self.execute_indexed([primary_key], index_keys, move |collection| async move {
            collection.insert_one(document).await
        })
        .await
    }

    /// Inserts documents in one native MongoDB operation and invalidates their cached keys.
    pub async fn insert_many<PI, SI>(
        &self,
        primary_keys: PI,
        index_keys: SI,
        documents: Vec<T>,
    ) -> Result<InsertManyResult, DriverError>
    where
        PI: IntoIterator<Item = K>,
        SI: IntoIterator<Item = I>,
        T: Serialize,
    {
        self.execute_indexed(primary_keys, index_keys, move |collection| async move {
            collection.insert_many(documents).await
        })
        .await
    }

    /// Updates one document and invalidates all affected primary and secondary cache entries.
    pub async fn update_one<PI, SI>(
        &self,
        primary_keys: PI,
        index_keys: SI,
        filter: Document,
        update: Document,
    ) -> Result<UpdateResult, DriverError>
    where
        PI: IntoIterator<Item = K>,
        SI: IntoIterator<Item = I>,
    {
        self.execute_indexed(primary_keys, index_keys, move |collection| async move {
            collection.update_one(filter, update).await
        })
        .await
    }

    /// Replaces one document and invalidates all affected primary and secondary cache entries.
    pub async fn replace_one<PI, SI>(
        &self,
        primary_keys: PI,
        index_keys: SI,
        filter: Document,
        replacement: T,
    ) -> Result<UpdateResult, DriverError>
    where
        PI: IntoIterator<Item = K>,
        SI: IntoIterator<Item = I>,
        T: Serialize,
    {
        self.execute_indexed(primary_keys, index_keys, move |collection| async move {
            collection.replace_one(filter, replacement).await
        })
        .await
    }

    /// Deletes one document and invalidates all affected primary and secondary cache entries.
    pub async fn delete_one<PI, SI>(
        &self,
        primary_keys: PI,
        index_keys: SI,
        filter: Document,
    ) -> Result<DeleteResult, DriverError>
    where
        PI: IntoIterator<Item = K>,
        SI: IntoIterator<Item = I>,
    {
        self.execute_indexed(primary_keys, index_keys, move |collection| async move {
            collection.delete_one(filter).await
        })
        .await
    }

    pub fn invalidate(&self, key: &K) -> bool {
        let _guard = self.cache_gate.lock().expect("MongoDB cache gate poisoned");
        self.generation.fetch_add(1, Ordering::AcqRel);
        let removed = self.cache.remove(key).is_some();
        self.indexes
            .remove_where(|_, primary| primary.as_ref() == Some(key));
        removed
    }

    pub fn invalidate_many<PI>(&self, keys: PI)
    where
        PI: IntoIterator<Item = K>,
    {
        let _guard = self.cache_gate.lock().expect("MongoDB cache gate poisoned");
        self.generation.fetch_add(1, Ordering::AcqRel);
        let keys: std::collections::HashSet<_> = keys.into_iter().collect();
        for key in &keys {
            self.cache.remove(key);
        }
        self.indexes
            .remove_where(|_, primary| primary.as_ref().is_some_and(|key| keys.contains(key)));
    }

    pub fn invalidate_index(&self, index: &I) -> bool {
        let _guard = self.cache_gate.lock().expect("MongoDB cache gate poisoned");
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.indexes.remove(index).is_some()
    }

    pub fn invalidate_related<PI, SI>(&self, primary_keys: PI, index_keys: SI)
    where
        PI: IntoIterator<Item = K>,
        SI: IntoIterator<Item = I>,
    {
        let _guard = self.cache_gate.lock().expect("MongoDB cache gate poisoned");
        self.generation.fetch_add(1, Ordering::AcqRel);
        let primary_keys: std::collections::HashSet<_> = primary_keys.into_iter().collect();
        for key in &primary_keys {
            self.cache.remove(key);
        }
        self.indexes.remove_where(|_, primary| {
            primary
                .as_ref()
                .is_some_and(|key| primary_keys.contains(key))
        });
        for index in index_keys {
            self.indexes.remove(&index);
        }
    }

    pub fn cache_stats(&self) -> CacheStats {
        self.cache.stats()
    }

    pub fn index_cache_stats(&self) -> CacheStats {
        self.indexes.stats()
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
    async fn typed_helpers_standardize_not_found_validate_batches_and_emit_metrics() {
        let registry = Metrics::new();
        let metrics = MongoStoreMetrics::register(&registry).unwrap();
        let store = MongoStore::connect(MongoStoreConfig::new(
            "mongodb://127.0.0.1:27017",
            "rust_zero_test",
        ))
        .await
        .unwrap()
        .with_metrics(metrics);

        let user = store
            .query::<User, _, _, _>("find_user", "users", |_collection| async {
                Ok(User {
                    id: 7,
                    name: "Ada".to_owned(),
                })
            })
            .await
            .unwrap();
        assert_eq!(user.name, "Ada");

        let missing = store
            .query_one::<User, _, _>("find_user", "users", "user", |_collection| async {
                Ok(None)
            })
            .await
            .unwrap_err();
        assert!(missing.is_not_found());
        assert_eq!(missing.to_string(), "user not found");

        assert!(matches!(
            store.bulk_insert::<User>("invalid", "users", [], 0).await,
            Err(MongoStoreError::InvalidBatchSize)
        ));
        let rendered = registry.render();
        assert!(rendered.contains(
            "rust_zero_mongo_operations_total{operation=\"find_user\",kind=\"query\",outcome=\"success\"} 1"
        ));
        assert!(rendered.contains(
            "rust_zero_mongo_operations_total{operation=\"find_user\",kind=\"query\",outcome=\"not_found\"} 1"
        ));
    }

    #[tokio::test]
    async fn secondary_indexes_share_documents_and_follow_primary_invalidation() {
        let store = MongoStore::connect(MongoStoreConfig::new(
            "mongodb://127.0.0.1:27017",
            "rust_zero_test",
        ))
        .await
        .unwrap();
        let cached = store.cached_indexed_collection::<i64, String, User>(
            "users",
            MongoCacheConfig::new(10, Duration::from_secs(30)),
        );
        let queries = AtomicUsize::new(0);

        for _ in 0..2 {
            let queries = &queries;
            let user = cached
                .find_by_index("ada@test".to_owned(), move |_collection| async move {
                    queries.fetch_add(1, Ordering::SeqCst);
                    Ok(Some((
                        7,
                        User {
                            id: 7,
                            name: "Ada".to_owned(),
                        },
                    )))
                })
                .await
                .unwrap();
            assert_eq!(user.unwrap().name, "Ada");
        }
        assert_eq!(queries.load(Ordering::SeqCst), 1);

        assert!(cached.invalidate(&7));
        let queries = &queries;
        let user = cached
            .find_by_index("ada@test".to_owned(), move |_collection| async move {
                queries.fetch_add(1, Ordering::SeqCst);
                Ok(Some((
                    7,
                    User {
                        id: 7,
                        name: "Grace".to_owned(),
                    },
                )))
            })
            .await
            .unwrap();
        assert_eq!(user.unwrap().name, "Grace");
        assert_eq!(queries.load(Ordering::SeqCst), 2);
        assert!(cached.index_cache_stats().insertions >= 2);
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
            .insert_one(
                7,
                std::iter::empty(),
                User {
                    id: 7,
                    name: "Ada".to_owned(),
                },
            )
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
            .update_one(
                [7],
                std::iter::empty(),
                doc! { "_id": 7_i64 },
                doc! { "$set": { "name": "Grace" } },
            )
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

        let bulk = store
            .bulk_insert(
                "insert_users",
                "users",
                (8_i64..=10).map(|id| User {
                    id,
                    name: format!("user-{id}"),
                }),
                2,
            )
            .await
            .unwrap();
        assert_eq!(bulk.len(), 2);
        assert_eq!(
            bulk.iter()
                .map(|result| result.inserted_ids.len())
                .sum::<usize>(),
            3
        );

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
