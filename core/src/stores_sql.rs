use sqlx::{
    mysql::{MySqlPool, MySqlPoolOptions},
    postgres::{PgPool, PgPoolOptions},
    sqlite::{SqlitePool, SqlitePoolOptions},
    Database, MySql, Pool, Postgres, Sqlite, Transaction,
};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlStoreConfig {
    pub url: String,
    pub min_connections: u32,
    pub max_connections: u32,
    pub acquire_timeout: Duration,
    pub idle_timeout: Option<Duration>,
    pub max_lifetime: Option<Duration>,
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
}
