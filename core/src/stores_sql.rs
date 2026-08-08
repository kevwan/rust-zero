use sqlx::{
    mysql::{MySqlPool, MySqlPoolOptions},
    postgres::{PgPool, PgPoolOptions},
    sqlite::{SqlitePool, SqlitePoolOptions},
    Database, MySql, Pool, Postgres, Sqlite, Transaction,
};
use std::{
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
#[derive(Debug, Clone)]
pub struct SqlStore<DB: Database> {
    pool: Pool<DB>,
}

/// A typed SQL pool with cache-aside record loading and mutation invalidation.
///
/// Both present records and missing records are cached, which protects the database from repeated
/// lookups for absent keys. Concurrent misses for one key are coalesced. Successful mutations
/// invalidate their keys while coordinating with in-flight loads, preventing an older query from
/// restoring stale data after the mutation completes.
pub struct CachedSqlStore<DB, K, V, E>
where
    DB: Database,
{
    store: SqlStore<DB>,
    cache: MemoryCache<K, Option<V>>,
    flights: SingleFlight<K, Option<V>, E>,
    ttl: Duration,
    not_found_ttl: Option<Duration>,
    ttl_jitter: Duration,
    expiry_sequence: AtomicU64,
    generation: AtomicU64,
    cache_gate: Mutex<()>,
}

impl<DB, K, V, E> CachedSqlStore<DB, K, V, E>
where
    DB: Database,
    K: Clone + Eq + Hash,
    V: Clone,
{
    pub fn new(store: SqlStore<DB>, config: SqlCacheConfig) -> Self {
        Self {
            store,
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

    /// Runs a SQL mutation and invalidates the affected cache keys after it succeeds.
    pub async fn execute<I, F, Fut, R>(&self, keys: I, operation: F) -> Result<R, E>
    where
        I: IntoIterator<Item = K>,
        F: FnOnce(Pool<DB>) -> Fut,
        Fut: Future<Output = Result<R, E>>,
    {
        let result = operation(self.store.pool().clone()).await?;
        self.invalidate_many(keys);
        Ok(result)
    }

    /// Invalidates a key and reports whether it held a positive or negative cached record.
    pub fn invalidate(&self, key: &K) -> bool {
        let _guard = self.cache_gate.lock().expect("SQL cache gate poisoned");
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.cache.remove(key).is_some()
    }

    pub fn invalidate_many<I>(&self, keys: I)
    where
        I: IntoIterator<Item = K>,
    {
        let _guard = self.cache_gate.lock().expect("SQL cache gate poisoned");
        self.generation.fetch_add(1, Ordering::AcqRel);
        for key in keys {
            self.cache.remove(&key);
        }
    }

    pub fn cache_stats(&self) -> CacheStats {
        self.cache.stats()
    }
}

impl<DB: Database> SqlStore<DB> {
    pub fn from_pool(pool: Pool<DB>) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &Pool<DB> {
        &self.pool
    }

    pub async fn begin(&self) -> Result<Transaction<'static, DB>, sqlx::Error> {
        self.pool.begin().await
    }

    pub async fn close(&self) {
        self.pool.close().await;
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
