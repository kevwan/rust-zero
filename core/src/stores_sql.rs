//! Typed SQLx pools, instrumentation, batching, and cache-aside records.
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use rust_zero_core::{SqlStoreConfig, SqliteStore};
//! let store = SqliteStore::connect_sqlite(SqlStoreConfig::new("sqlite::memory:")).await?;
//! store.health_check().await?;
//! # Ok(())
//! # }
//! ```

use sqlx::{
    mysql::{MySqlPool, MySqlPoolOptions},
    postgres::{PgPool, PgPoolOptions},
    sqlite::{SqlitePool, SqlitePoolOptions},
    Database, MySql, Pool, Postgres, Sqlite, Transaction,
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
    time::{Duration, Instant},
};

use crate::{
    cache::jittered_ttl, CacheStats, CounterVec, HistogramOptions, HistogramVec, MemoryCache,
    Metrics, MetricsError, SingleFlight, SingleFlightError, VectorOptions,
};

#[cfg(feature = "telemetry")]
use crate::{TelemetrySpan, TelemetrySpanKind};

/// Stable failures returned by the typed SQL helpers.
#[derive(Debug)]
pub enum SqlStoreError {
    Database(sqlx::Error),
    NotFound { entity: String },
    InvalidBatchSize,
}

impl SqlStoreError {
    pub fn not_found(entity: impl Into<String>) -> Self {
        Self::NotFound {
            entity: entity.into(),
        }
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. })
    }
}

impl fmt::Display for SqlStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "SQL operation failed: {error}"),
            Self::NotFound { entity } => write!(formatter, "{entity} not found"),
            Self::InvalidBatchSize => formatter.write_str("SQL bulk batch size must be positive"),
        }
    }
}

impl Error for SqlStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::NotFound { .. } | Self::InvalidBatchSize => None,
        }
    }
}

impl From<sqlx::Error> for SqlStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// Cardinality-bounded SQL operation metrics used by [`SqlStore`]'s typed helpers.
#[derive(Clone)]
pub struct SqlStoreMetrics {
    operations: CounterVec,
    duration: HistogramVec,
}

impl SqlStoreMetrics {
    pub fn register(metrics: &Metrics) -> Result<Self, MetricsError> {
        let labels = ["operation", "kind", "outcome"];
        Ok(Self {
            operations: metrics.counter_vec(
                VectorOptions::new("operations_total", "Completed SQL store operations")
                    .with_namespace("rust_zero")
                    .with_subsystem("sql")
                    .with_labels(labels),
            )?,
            duration: metrics.histogram_vec(
                HistogramOptions::new("operation_duration_seconds", "SQL store operation latency")
                    .with_vector_options(
                        VectorOptions::new(
                            "operation_duration_seconds",
                            "SQL store operation latency",
                        )
                        .with_namespace("rust_zero")
                        .with_subsystem("sql")
                        .with_labels(labels),
                    ),
            )?,
        })
    }

    fn observe(&self, operation: &str, kind: SqlOperationKind, outcome: &str, elapsed: Duration) {
        let labels = [operation, kind.as_str(), outcome];
        // Registration validates the instruments and labels. Recording failures are deliberately
        // non-fatal: observability must never change the result of a database operation.
        let _ = self.operations.inc(&labels);
        let _ = self.duration.observe(elapsed.as_secs_f64(), &labels);
    }
}

#[derive(Debug, Clone, Copy)]
enum SqlOperationKind {
    Query,
    Execute,
    BulkInsert,
}

impl SqlOperationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Execute => "execute",
            Self::BulkInsert => "bulk_insert",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlStoreConfig {
    pub url: String,
    pub min_connections: u32,
    pub max_connections: u32,
    pub acquire_timeout: Duration,
    pub idle_timeout: Option<Duration>,
    pub max_lifetime: Option<Duration>,
}

/// Settings for the in-process cache used by [`CachedSqlStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlCacheConfig {
    pub capacity: usize,
    pub ttl: Duration,
    pub not_found_ttl: Option<Duration>,
    pub ttl_jitter: Duration,
}

impl SqlCacheConfig {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        assert!(capacity > 0, "SQL cache capacity must be positive");
        assert!(!ttl.is_zero(), "SQL cache TTL must be positive");
        Self {
            capacity,
            ttl,
            not_found_ttl: Some(ttl),
            ttl_jitter: Duration::ZERO,
        }
    }

    /// Controls negative caching. `None` makes every missing record hit the database.
    pub fn with_not_found_ttl(mut self, ttl: Option<Duration>) -> Self {
        assert!(
            ttl.is_none_or(|ttl| !ttl.is_zero()),
            "SQL not-found cache TTL must be positive"
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

impl SqlStoreConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            min_connections: 0,
            max_connections: 10,
            acquire_timeout: Duration::from_secs(3),
            idle_timeout: Some(Duration::from_secs(10 * 60)),
            max_lifetime: Some(Duration::from_secs(30 * 60)),
        }
    }

    pub fn with_pool_size(mut self, min: u32, max: u32) -> Self {
        assert!(max > 0, "SQL maximum pool size must be positive");
        assert!(min <= max, "SQL minimum pool size cannot exceed maximum");
        self.min_connections = min;
        self.max_connections = max;
        self
    }

    pub fn with_acquire_timeout(mut self, timeout: Duration) -> Self {
        assert!(!timeout.is_zero(), "SQL acquire timeout must be positive");
        self.acquire_timeout = timeout;
        self
    }
}

/// A typed SQL connection pool.
///
/// The underlying [`Pool`] remains available so applications retain SQLx's compile-time checked
/// queries. The wrapper standardizes pool configuration, health checks, and transaction creation.
#[derive(Clone)]
pub struct SqlStore<DB: Database> {
    pool: Pool<DB>,
    metrics: Option<SqlStoreMetrics>,
}

impl<DB: Database> fmt::Debug for SqlStore<DB> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SqlStore")
            .field("pool", &self.pool)
            .field("instrumented", &self.metrics.is_some())
            .finish()
    }
}

/// A typed SQL pool with cache-aside record loading and mutation invalidation.
///
/// Both present records and missing records are cached, which protects the database from repeated
/// lookups for absent keys. Concurrent misses for one key are coalesced. Successful mutations
/// invalidate their keys while coordinating with in-flight loads, preventing an older query from
/// restoring stale data after the mutation completes.
pub struct CachedSqlStore<DB, K, V, E, I = K>
where
    DB: Database,
{
    store: SqlStore<DB>,
    cache: MemoryCache<K, Option<V>>,
    indexes: MemoryCache<I, Option<K>>,
    flights: SingleFlight<K, Option<V>, E>,
    index_flights: SingleFlight<I, Option<(K, V)>, E>,
    ttl: Duration,
    not_found_ttl: Option<Duration>,
    ttl_jitter: Duration,
    expiry_sequence: AtomicU64,
    generation: AtomicU64,
    cache_gate: Mutex<()>,
}

impl<DB, K, V, E, I> CachedSqlStore<DB, K, V, E, I>
where
    DB: Database,
    K: Clone + Eq + Hash,
    I: Clone + Eq + Hash,
    V: Clone,
{
    pub fn new(store: SqlStore<DB>, config: SqlCacheConfig) -> Self {
        Self {
            store,
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

    pub fn store(&self) -> &SqlStore<DB> {
        &self.store
    }

    pub fn pool(&self) -> &Pool<DB> {
        self.store.pool()
    }

    /// Returns a cached record or loads it from SQL once across concurrent callers.
    ///
    /// The query receives a cheap clone of the underlying SQLx pool, allowing its returned future
    /// to own everything it needs without boxed futures or lifetime plumbing.
    pub async fn find<F, Fut>(&self, key: K, query: F) -> Result<Option<V>, SingleFlightError<E>>
    where
        F: FnOnce(Pool<DB>) -> Fut,
        Fut: Future<Output = Result<Option<V>, E>>,
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
                let value = query(self.store.pool().clone()).await?;

                // Checking the generation and inserting while holding the same gate used by
                // invalidation closes the otherwise-small stale repopulation race.
                let _guard = self.cache_gate.lock().expect("SQL cache gate poisoned");
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

    /// Returns a record selected by a secondary key.
    ///
    /// Successful lookups cache both the primary record and the secondary-to-primary mapping.
    /// Missing secondary keys use the configured not-found policy. Concurrent misses for the same
    /// secondary key are coalesced independently from primary-key lookups.
    pub async fn find_by_index<F, Fut>(
        &self,
        index: I,
        query: F,
    ) -> Result<Option<V>, SingleFlightError<E>>
    where
        F: FnOnce(Pool<DB>) -> Fut,
        Fut: Future<Output = Result<Option<(K, V)>, E>>,
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
                let value = query(self.store.pool().clone()).await?;
                let _guard = self.cache_gate.lock().expect("SQL cache gate poisoned");
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

    /// Runs a SQL mutation and invalidates the affected cache keys after it succeeds.
    pub async fn execute<PI, F, Fut, R>(&self, keys: PI, operation: F) -> Result<R, E>
    where
        PI: IntoIterator<Item = K>,
        F: FnOnce(Pool<DB>) -> Fut,
        Fut: Future<Output = Result<R, E>>,
    {
        let result = operation(self.store.pool().clone()).await?;
        self.invalidate_many(keys);
        Ok(result)
    }

    /// Runs a mutation and invalidates primary keys plus explicitly affected secondary keys.
    ///
    /// Learned secondary mappings for each primary key are removed automatically. Callers should
    /// also pass newly introduced or changed secondary keys so a previously cached not-found
    /// result cannot hide an insert or update.
    pub async fn execute_indexed<PI, SI, F, Fut, R>(
        &self,
        primary_keys: PI,
        index_keys: SI,
        operation: F,
    ) -> Result<R, E>
    where
        PI: IntoIterator<Item = K>,
        SI: IntoIterator<Item = I>,
        F: FnOnce(Pool<DB>) -> Fut,
        Fut: Future<Output = Result<R, E>>,
    {
        let result = operation(self.store.pool().clone()).await?;
        self.invalidate_related(primary_keys, index_keys);
        Ok(result)
    }

    /// Invalidates a key and reports whether it held a positive or negative cached record.
    pub fn invalidate(&self, key: &K) -> bool {
        let _guard = self.cache_gate.lock().expect("SQL cache gate poisoned");
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
        let _guard = self.cache_gate.lock().expect("SQL cache gate poisoned");
        self.generation.fetch_add(1, Ordering::AcqRel);
        let keys: std::collections::HashSet<_> = keys.into_iter().collect();
        for key in &keys {
            self.cache.remove(key);
        }
        self.indexes
            .remove_where(|_, primary| primary.as_ref().is_some_and(|key| keys.contains(key)));
    }

    pub fn invalidate_index(&self, index: &I) -> bool {
        let _guard = self.cache_gate.lock().expect("SQL cache gate poisoned");
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.indexes.remove(index).is_some()
    }

    pub fn invalidate_related<PI, SI>(&self, primary_keys: PI, index_keys: SI)
    where
        PI: IntoIterator<Item = K>,
        SI: IntoIterator<Item = I>,
    {
        let _guard = self.cache_gate.lock().expect("SQL cache gate poisoned");
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

impl<DB: Database> SqlStore<DB> {
    pub fn from_pool(pool: Pool<DB>) -> Self {
        Self {
            pool,
            metrics: None,
        }
    }

    /// Installs metrics for operations run through the typed helper methods.
    pub fn with_metrics(mut self, metrics: SqlStoreMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    pub fn pool(&self) -> &Pool<DB> {
        &self.pool
    }

    pub async fn begin(&self) -> Result<Transaction<'static, DB>, sqlx::Error> {
        self.pool.begin().await
    }

    /// Runs a typed query closure with consistent metrics, tracing, and error conversion.
    pub async fn query<T, F, Fut>(
        &self,
        operation: &'static str,
        query: F,
    ) -> Result<T, SqlStoreError>
    where
        F: FnOnce(Pool<DB>) -> Fut,
        Fut: Future<Output = Result<T, sqlx::Error>>,
    {
        let pool = self.pool.clone();
        self.instrument(operation, SqlOperationKind::Query, async move {
            query(pool).await.map_err(SqlStoreError::Database)
        })
        .await
    }

    /// Runs an optional typed query and converts an absent row into [`SqlStoreError::NotFound`].
    pub async fn query_one<T, F, Fut>(
        &self,
        operation: &'static str,
        entity: impl Into<String>,
        query: F,
    ) -> Result<T, SqlStoreError>
    where
        F: FnOnce(Pool<DB>) -> Fut,
        Fut: Future<Output = Result<Option<T>, sqlx::Error>>,
    {
        let pool = self.pool.clone();
        let entity = entity.into();
        self.instrument(operation, SqlOperationKind::Query, async move {
            query(pool)
                .await
                .map_err(SqlStoreError::Database)?
                .ok_or_else(|| SqlStoreError::not_found(entity))
        })
        .await
    }

    /// Runs a typed mutation closure with consistent metrics, tracing, and error conversion.
    pub async fn execute<T, F, Fut>(
        &self,
        operation: &'static str,
        execute: F,
    ) -> Result<T, SqlStoreError>
    where
        F: FnOnce(Pool<DB>) -> Fut,
        Fut: Future<Output = Result<T, sqlx::Error>>,
    {
        let pool = self.pool.clone();
        self.instrument(operation, SqlOperationKind::Execute, async move {
            execute(pool).await.map_err(SqlStoreError::Database)
        })
        .await
    }

    /// Inserts items in bounded batches using a database-specific typed SQLx closure.
    ///
    /// The returned vector contains one result per batch. The helper intentionally leaves SQL
    /// construction to the caller because placeholder syntax and optimal multi-row statements
    /// differ across SQLite, PostgreSQL, and MySQL.
    pub async fn bulk_insert<T, R, F, Fut>(
        &self,
        operation: &'static str,
        items: impl IntoIterator<Item = T>,
        batch_size: usize,
        mut insert_batch: F,
    ) -> Result<Vec<R>, SqlStoreError>
    where
        F: FnMut(Pool<DB>, Vec<T>) -> Fut,
        Fut: Future<Output = Result<R, sqlx::Error>>,
    {
        if batch_size == 0 {
            return Err(SqlStoreError::InvalidBatchSize);
        }

        let started = Instant::now();
        #[cfg(feature = "telemetry")]
        let span = TelemetrySpan::start(
            format!("sql.{operation}"),
            TelemetrySpanKind::Client,
            None,
            [
                ("db.operation.name", operation.to_owned()),
                (
                    "rust_zero.sql.kind",
                    SqlOperationKind::BulkInsert.as_str().to_owned(),
                ),
            ],
        );
        let mut results = Vec::new();
        let mut batch = Vec::with_capacity(batch_size);
        let mut items = items.into_iter();
        let result = loop {
            batch.extend(items.by_ref().take(batch_size));
            if batch.is_empty() {
                break Ok(results);
            }
            let current = std::mem::replace(&mut batch, Vec::with_capacity(batch_size));
            match insert_batch(self.pool.clone(), current).await {
                Ok(result) => results.push(result),
                Err(error) => break Err(SqlStoreError::Database(error)),
            }
        };
        let outcome = sql_outcome(&result);
        if let Some(metrics) = &self.metrics {
            metrics.observe(
                operation,
                SqlOperationKind::BulkInsert,
                outcome,
                started.elapsed(),
            );
        }
        #[cfg(feature = "telemetry")]
        if let Err(error) = &result {
            span.set_error(error.to_string());
        }
        result
    }

    async fn instrument<T, Fut>(
        &self,
        operation: &'static str,
        kind: SqlOperationKind,
        future: Fut,
    ) -> Result<T, SqlStoreError>
    where
        Fut: Future<Output = Result<T, SqlStoreError>>,
    {
        let started = Instant::now();
        #[cfg(feature = "telemetry")]
        let span = TelemetrySpan::start(
            format!("sql.{operation}"),
            TelemetrySpanKind::Client,
            None,
            [
                ("db.operation.name", operation.to_owned()),
                ("rust_zero.sql.kind", kind.as_str().to_owned()),
            ],
        );
        let result = future.await;
        let outcome = sql_outcome(&result);
        if let Some(metrics) = &self.metrics {
            metrics.observe(operation, kind, outcome, started.elapsed());
        }
        #[cfg(feature = "telemetry")]
        if let Err(error) = &result {
            span.set_error(error.to_string());
        }
        result
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

fn sql_outcome<T>(result: &Result<T, SqlStoreError>) -> &'static str {
    match result {
        Ok(_) => "success",
        Err(SqlStoreError::NotFound { .. }) => "not_found",
        Err(SqlStoreError::Database(_) | SqlStoreError::InvalidBatchSize) => "error",
    }
}

impl SqlStore<Sqlite> {
    pub async fn connect_sqlite(config: SqlStoreConfig) -> Result<Self, sqlx::Error> {
        let pool = configure_sqlite(&config).connect(&config.url).await?;
        Ok(Self::from_pool(pool))
    }

    pub async fn health_check(&self) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }
}

impl SqlStore<Postgres> {
    pub async fn connect_postgres(config: SqlStoreConfig) -> Result<Self, sqlx::Error> {
        let pool = configure_postgres(&config).connect(&config.url).await?;
        Ok(Self::from_pool(pool))
    }

    pub async fn health_check(&self) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }
}

impl SqlStore<MySql> {
    pub async fn connect_mysql(config: SqlStoreConfig) -> Result<Self, sqlx::Error> {
        let pool = configure_mysql(&config).connect(&config.url).await?;
        Ok(Self::from_pool(pool))
    }

    pub async fn health_check(&self) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }
}

fn configure_sqlite(config: &SqlStoreConfig) -> SqlitePoolOptions {
    SqlitePoolOptions::new()
        .min_connections(config.min_connections)
        .max_connections(config.max_connections)
        .acquire_timeout(config.acquire_timeout)
        .idle_timeout(config.idle_timeout)
        .max_lifetime(config.max_lifetime)
}

fn configure_postgres(config: &SqlStoreConfig) -> PgPoolOptions {
    PgPoolOptions::new()
        .min_connections(config.min_connections)
        .max_connections(config.max_connections)
        .acquire_timeout(config.acquire_timeout)
        .idle_timeout(config.idle_timeout)
        .max_lifetime(config.max_lifetime)
}

fn configure_mysql(config: &SqlStoreConfig) -> MySqlPoolOptions {
    MySqlPoolOptions::new()
        .min_connections(config.min_connections)
        .max_connections(config.max_connections)
        .acquire_timeout(config.acquire_timeout)
        .idle_timeout(config.idle_timeout)
        .max_lifetime(config.max_lifetime)
}

pub type SqliteStore = SqlStore<Sqlite>;
pub type PostgresStore = SqlStore<Postgres>;
pub type MySqlStore = SqlStore<MySql>;

// Keep the concrete pool aliases visible in generated API documentation.
const _: Option<(SqlitePool, PgPool, MySqlPool)> = None;

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::sync::Notify;

    #[tokio::test]
    async fn sqlite_supports_queries_and_commit_or_rollback_transactions() {
        let store = SqliteStore::connect_sqlite(
            SqlStoreConfig::new("sqlite::memory:").with_pool_size(1, 1),
        )
        .await
        .unwrap();
        store.health_check().await.unwrap();
        sqlx::query("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
            .execute(store.pool())
            .await
            .unwrap();

        let mut transaction = store.begin().await.unwrap();
        sqlx::query("INSERT INTO users (name) VALUES (?)")
            .bind("Ada")
            .execute(&mut *transaction)
            .await
            .unwrap();
        transaction.commit().await.unwrap();

        let row = sqlx::query("SELECT name FROM users")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(row.try_get::<String, _>("name").unwrap(), "Ada");

        let mut transaction = store.begin().await.unwrap();
        sqlx::query("DELETE FROM users")
            .execute(&mut *transaction)
            .await
            .unwrap();
        transaction.rollback().await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn typed_helpers_batch_inserts_standardize_not_found_and_emit_metrics() {
        let registry = Metrics::new();
        let metrics = SqlStoreMetrics::register(&registry).unwrap();
        let store = SqliteStore::connect_sqlite(
            SqlStoreConfig::new("sqlite::memory:").with_pool_size(1, 1),
        )
        .await
        .unwrap()
        .with_metrics(metrics);

        store
            .execute("create_users", |pool| async move {
                sqlx::query("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
                    .execute(&pool)
                    .await
            })
            .await
            .unwrap();

        let batches = store
            .bulk_insert(
                "insert_users",
                (1_i64..=5).map(|id| (id, format!("user-{id}"))),
                2,
                |pool, users| async move {
                    let mut query =
                        sqlx::QueryBuilder::<Sqlite>::new("INSERT INTO users (id, name) ");
                    query.push_values(users, |mut row, (id, name)| {
                        row.push_bind(id).push_bind(name);
                    });
                    query
                        .build()
                        .execute(&pool)
                        .await
                        .map(|result| result.rows_affected())
                },
            )
            .await
            .unwrap();
        assert_eq!(batches, vec![2, 2, 1]);

        let name: String = store
            .query_one("find_user", "user", |pool| async move {
                sqlx::query_scalar("SELECT name FROM users WHERE id = ?")
                    .bind(3_i64)
                    .fetch_optional(&pool)
                    .await
            })
            .await
            .unwrap();
        assert_eq!(name, "user-3");

        let missing = store
            .query_one::<String, _, _>("find_user", "user", |pool| async move {
                sqlx::query_scalar("SELECT name FROM users WHERE id = ?")
                    .bind(404_i64)
                    .fetch_optional(&pool)
                    .await
            })
            .await
            .unwrap_err();
        assert!(missing.is_not_found());
        assert_eq!(missing.to_string(), "user not found");

        let rendered = registry.render();
        assert!(rendered.contains(
            "rust_zero_sql_operations_total{operation=\"insert_users\",kind=\"bulk_insert\",outcome=\"success\"} 1"
        ));
        assert!(rendered.contains(
            "rust_zero_sql_operations_total{operation=\"find_user\",kind=\"query\",outcome=\"not_found\"} 1"
        ));
        assert!(matches!(
            store
                .bulk_insert::<i64, (), _, _>("invalid", [], 0, |_pool, _items| async { Ok(()) })
                .await,
            Err(SqlStoreError::InvalidBatchSize)
        ));
    }

    #[test]
    #[should_panic(expected = "minimum pool size")]
    fn rejects_invalid_pool_bounds() {
        let _ = SqlStoreConfig::new("sqlite::memory:").with_pool_size(2, 1);
    }

    #[tokio::test]
    async fn cached_store_caches_records_and_missing_rows_then_invalidates_mutations() {
        let store = SqliteStore::connect_sqlite(
            SqlStoreConfig::new("sqlite::memory:").with_pool_size(1, 1),
        )
        .await
        .unwrap();
        sqlx::query("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO users (id, name) VALUES (1, 'Ada')")
            .execute(store.pool())
            .await
            .unwrap();

        let cached = CachedSqlStore::<Sqlite, i64, String, sqlx::Error>::new(
            store,
            SqlCacheConfig::new(10, Duration::from_secs(30)),
        );
        let queries = AtomicUsize::new(0);

        for _ in 0..2 {
            let queries = &queries;
            let name = cached
                .find(1, move |pool| async move {
                    queries.fetch_add(1, Ordering::SeqCst);
                    sqlx::query_scalar("SELECT name FROM users WHERE id = ?")
                        .bind(1_i64)
                        .fetch_optional(&pool)
                        .await
                })
                .await
                .unwrap();
            assert_eq!(name.as_deref(), Some("Ada"));
        }
        for _ in 0..2 {
            let queries = &queries;
            assert_eq!(
                cached
                    .find(404, move |pool| async move {
                        queries.fetch_add(1, Ordering::SeqCst);
                        sqlx::query_scalar("SELECT name FROM users WHERE id = ?")
                            .bind(404_i64)
                            .fetch_optional(&pool)
                            .await
                    })
                    .await
                    .unwrap(),
                None
            );
        }
        assert_eq!(queries.load(Ordering::SeqCst), 2);

        cached
            .execute([1], |pool| async move {
                sqlx::query("UPDATE users SET name = 'Grace' WHERE id = 1")
                    .execute(&pool)
                    .await
            })
            .await
            .unwrap();
        let name = cached
            .find(1, |pool| async move {
                sqlx::query_scalar("SELECT name FROM users WHERE id = 1")
                    .fetch_optional(&pool)
                    .await
            })
            .await
            .unwrap();
        assert_eq!(name.as_deref(), Some("Grace"));
    }

    #[tokio::test]
    async fn secondary_indexes_share_primary_records_and_are_invalidated_with_mutations() {
        let store = SqliteStore::connect_sqlite(
            SqlStoreConfig::new("sqlite::memory:").with_pool_size(1, 1),
        )
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT UNIQUE, name TEXT NOT NULL)",
        )
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query("INSERT INTO users (id, email, name) VALUES (1, 'ada@test', 'Ada')")
            .execute(store.pool())
            .await
            .unwrap();

        let cached = CachedSqlStore::<Sqlite, i64, String, sqlx::Error, String>::new(
            store,
            SqlCacheConfig::new(10, Duration::from_secs(30)),
        );
        let queries = AtomicUsize::new(0);

        for _ in 0..2 {
            let queries = &queries;
            let user = cached
                .find_by_index("ada@test".to_owned(), move |pool| async move {
                    queries.fetch_add(1, Ordering::SeqCst);
                    sqlx::query_as::<_, (i64, String)>(
                        "SELECT id, name FROM users WHERE email = 'ada@test'",
                    )
                    .fetch_optional(&pool)
                    .await
                })
                .await
                .unwrap();
            assert_eq!(user.as_deref(), Some("Ada"));
        }
        assert_eq!(queries.load(Ordering::SeqCst), 1);

        // A secondary lookup populates the ordinary primary cache as well.
        let primary = cached
            .find(1, |_pool| async { Ok(Some("uncached".to_owned())) })
            .await
            .unwrap();
        assert_eq!(primary.as_deref(), Some("Ada"));

        // Cache a miss for the key that the mutation is about to introduce.
        assert!(cached
            .find_by_index("grace@test".to_owned(), |pool| async move {
                sqlx::query_as::<_, (i64, String)>(
                    "SELECT id, name FROM users WHERE email = 'grace@test'",
                )
                .fetch_optional(&pool)
                .await
            })
            .await
            .unwrap()
            .is_none());

        cached
            .execute_indexed(
                [1],
                ["ada@test".to_owned(), "grace@test".to_owned()],
                |pool| async move {
                    sqlx::query(
                        "UPDATE users SET email = 'grace@test', name = 'Grace' WHERE id = 1",
                    )
                    .execute(&pool)
                    .await
                },
            )
            .await
            .unwrap();

        let old = cached
            .find_by_index("ada@test".to_owned(), |pool| async move {
                sqlx::query_as::<_, (i64, String)>(
                    "SELECT id, name FROM users WHERE email = 'ada@test'",
                )
                .fetch_optional(&pool)
                .await
            })
            .await
            .unwrap();
        assert_eq!(old, None);
        let renamed = cached
            .find_by_index("grace@test".to_owned(), |pool| async move {
                sqlx::query_as::<_, (i64, String)>(
                    "SELECT id, name FROM users WHERE email = 'grace@test'",
                )
                .fetch_optional(&pool)
                .await
            })
            .await
            .unwrap();
        assert_eq!(renamed.as_deref(), Some("Grace"));
        assert!(cached.index_cache_stats().insertions >= 4);
    }

    #[tokio::test]
    async fn cached_store_can_disable_or_shorten_not_found_caching() {
        let store = SqliteStore::connect_sqlite(
            SqlStoreConfig::new("sqlite::memory:").with_pool_size(1, 1),
        )
        .await
        .unwrap();
        let cached = CachedSqlStore::<Sqlite, i64, String, sqlx::Error>::new(
            store,
            SqlCacheConfig::new(10, Duration::from_secs(30))
                .with_not_found_ttl(None)
                .with_ttl_jitter(Duration::from_secs(5)),
        );
        let queries = AtomicUsize::new(0);

        for _ in 0..2 {
            let queries = &queries;
            let value = cached
                .find(404, move |_pool| async move {
                    queries.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                })
                .await
                .unwrap();
            assert_eq!(value, None);
        }

        assert_eq!(queries.load(Ordering::SeqCst), 2);
        assert_eq!(cached.cache_stats().insertions, 0);
    }

    #[tokio::test]
    async fn mutation_prevents_an_inflight_read_from_repopulating_stale_data() {
        let store = SqliteStore::connect_sqlite(
            SqlStoreConfig::new("sqlite::memory:").with_pool_size(1, 1),
        )
        .await
        .unwrap();
        let cached = Arc::new(CachedSqlStore::<Sqlite, i64, String, sqlx::Error>::new(
            store,
            SqlCacheConfig::new(10, Duration::from_secs(30)),
        ));
        let query_started = Arc::new(Notify::new());
        let release_query = Arc::new(Notify::new());

        let stale_read = {
            let cached = Arc::clone(&cached);
            let query_started = Arc::clone(&query_started);
            let release_query = Arc::clone(&release_query);
            tokio::spawn(async move {
                cached
                    .find(1, move |_pool| async move {
                        query_started.notify_one();
                        release_query.notified().await;
                        Ok(Some("stale".to_owned()))
                    })
                    .await
            })
        };
        query_started.notified().await;
        cached
            .execute([1], |_pool| async { Ok::<_, sqlx::Error>(()) })
            .await
            .unwrap();
        release_query.notify_one();
        assert_eq!(stale_read.await.unwrap().unwrap().as_deref(), Some("stale"));
        assert_eq!(cached.cache.get(&1), None);
    }

    #[tokio::test]
    async fn postgres_integration_covers_health_crud_and_transactions() {
        let Ok(url) = std::env::var("RUST_ZERO_POSTGRES_URL") else {
            return;
        };
        let store = PostgresStore::connect_postgres(SqlStoreConfig::new(url).with_pool_size(1, 1))
            .await
            .unwrap();
        store.health_check().await.unwrap();
        sqlx::query(
            "CREATE TEMPORARY TABLE rust_zero_users (id BIGINT PRIMARY KEY, name TEXT NOT NULL)",
        )
        .execute(store.pool())
        .await
        .unwrap();
        let mut transaction = store.begin().await.unwrap();
        sqlx::query("INSERT INTO rust_zero_users (id, name) VALUES ($1, $2)")
            .bind(1_i64)
            .bind("Ada")
            .execute(&mut *transaction)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        let name: String = sqlx::query_scalar("SELECT name FROM rust_zero_users WHERE id = $1")
            .bind(1_i64)
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(name, "Ada");
    }

    #[tokio::test]
    async fn mysql_integration_covers_health_crud_and_transactions() {
        let Ok(url) = std::env::var("RUST_ZERO_MYSQL_URL") else {
            return;
        };
        let store = MySqlStore::connect_mysql(SqlStoreConfig::new(url).with_pool_size(1, 1))
            .await
            .unwrap();
        store.health_check().await.unwrap();
        sqlx::query(
            "CREATE TEMPORARY TABLE rust_zero_users (id BIGINT PRIMARY KEY, name TEXT NOT NULL)",
        )
        .execute(store.pool())
        .await
        .unwrap();
        let mut transaction = store.begin().await.unwrap();
        sqlx::query("INSERT INTO rust_zero_users (id, name) VALUES (?, ?)")
            .bind(1_i64)
            .bind("Ada")
            .execute(&mut *transaction)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        let name: String = sqlx::query_scalar("SELECT name FROM rust_zero_users WHERE id = ?")
            .bind(1_i64)
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(name, "Ada");
    }
}
