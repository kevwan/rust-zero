use redis::{
    aio::{ConnectionLike, MultiplexedConnection},
    cluster::ClusterClient,
    cluster_async::ClusterConnection,
    AsyncCommands, Cmd, FromRedisValue, Pipeline, RedisFuture, Value,
};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    marker::PhantomData,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::OnceCell;

use crate::{SingleFlight, SingleFlightError};

const RELEASE_LOCK: &str =
    "if redis.call('get', KEYS[1]) == ARGV[1] then return redis.call('del', KEYS[1]) else return 0 end";
const EXTEND_LOCK: &str =
    "if redis.call('get', KEYS[1]) == ARGV[1] then return redis.call('pexpire', KEYS[1], ARGV[2]) else return 0 end";

static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisStoreConfig {
    pub urls: Vec<String>,
    pub cluster: bool,
    pub key_prefix: String,
    pub operation_timeout: Duration,
}

impl RedisStoreConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            urls: vec![url.into()],
            cluster: false,
            key_prefix: String::new(),
            operation_timeout: Duration::from_secs(3),
        }
    }

    pub fn cluster<I, S>(nodes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            urls: nodes.into_iter().map(Into::into).collect(),
            cluster: true,
            key_prefix: String::new(),
            operation_timeout: Duration::from_secs(3),
        }
    }

    pub fn with_key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.key_prefix = prefix.into();
        self
    }

    pub fn with_operation_timeout(mut self, timeout: Duration) -> Self {
        assert!(
            !timeout.is_zero(),
            "Redis operation timeout must be positive"
        );
        self.operation_timeout = timeout;
        self
    }
}

#[derive(Clone)]
pub struct RedisStore {
    client: RedisClient,
    connection: std::sync::Arc<OnceCell<RedisConnection>>,
    config: RedisStoreConfig,
}

impl RedisStore {
    pub fn new(config: RedisStoreConfig) -> Result<Self, RedisStoreError> {
        let client = if config.cluster {
            RedisClient::Cluster(ClusterClient::new(config.urls.clone())?)
        } else {
            let url = config
                .urls
                .first()
                .ok_or(RedisStoreError::MissingEndpoint)?;
            RedisClient::Standalone(redis::Client::open(url.as_str())?)
        };
        Ok(Self {
            client,
            connection: std::sync::Arc::new(OnceCell::new()),
            config,
        })
    }

    pub async fn ping(&self) -> Result<(), RedisStoreError> {
        let mut connection = self.connection().await?;
        self.run(redis::cmd("PING").query_async::<String>(&mut connection))
            .await?;
        Ok(())
    }

    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, RedisStoreError> {
        let mut connection = self.connection().await?;
        let key = self.key(key);
        self.run(connection.get(key)).await
    }

    pub async fn get_json<T: DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, RedisStoreError> {
        self.get(key)
            .await?
            .map(|value| serde_json::from_slice(&value).map_err(RedisStoreError::Json))
            .transpose()
    }

    pub async fn get_string(&self, key: &str) -> Result<Option<String>, RedisStoreError> {
        let mut command = redis::cmd("GET");
        command.arg(self.key(key));
        self.query(&mut command).await
    }

    pub async fn set(
        &self,
        key: &str,
        value: impl AsRef<[u8]>,
        ttl: Option<Duration>,
    ) -> Result<(), RedisStoreError> {
        let mut connection = self.connection().await?;
        let key = self.key(key);
        match ttl {
            Some(ttl) if ttl.is_zero() => Err(RedisStoreError::InvalidTtl),
            Some(ttl) => {
                self.run(connection.pset_ex(key, value.as_ref(), duration_millis(ttl)?))
                    .await
            }
            None => self.run(connection.set(key, value.as_ref())).await,
        }
    }

    pub async fn set_json<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        ttl: Option<Duration>,
    ) -> Result<(), RedisStoreError> {
        self.set(key, serde_json::to_vec(value)?, ttl).await
    }

    pub async fn set_if_absent(
        &self,
        key: &str,
        value: impl AsRef<[u8]>,
        ttl: Option<Duration>,
    ) -> Result<bool, RedisStoreError> {
        if ttl.is_some_and(|ttl| ttl.is_zero()) {
            return Err(RedisStoreError::InvalidTtl);
        }
        let mut command = redis::cmd("SET");
        command.arg(self.key(key)).arg(value.as_ref()).arg("NX");
        if let Some(ttl) = ttl {
            command.arg("PX").arg(duration_millis(ttl)?);
        }
        Ok(self.query::<Option<String>>(&mut command).await?.is_some())
    }

    pub async fn get_many(&self, keys: &[&str]) -> Result<Vec<Option<Vec<u8>>>, RedisStoreError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut command = redis::cmd("MGET");
        command.arg(keys.iter().map(|key| self.key(key)).collect::<Vec<_>>());
        self.query(&mut command).await
    }

    pub async fn set_many<V: AsRef<[u8]>>(
        &self,
        entries: &[(&str, V)],
    ) -> Result<(), RedisStoreError> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut command = redis::cmd("MSET");
        for (key, value) in entries {
            command.arg(self.key(key)).arg(value.as_ref());
        }
        self.query(&mut command).await
    }

    pub async fn delete(&self, keys: &[&str]) -> Result<u64, RedisStoreError> {
        if keys.is_empty() {
            return Ok(0);
        }
        let mut connection = self.connection().await?;
        let keys: Vec<_> = keys.iter().map(|key| self.key(key)).collect();
        self.run(connection.del(keys)).await
    }

    pub async fn exists(&self, key: &str) -> Result<bool, RedisStoreError> {
        let mut connection = self.connection().await?;
        let key = self.key(key);
        self.run(connection.exists(key)).await
    }

    pub async fn increment(&self, key: &str, amount: i64) -> Result<i64, RedisStoreError> {
        let mut connection = self.connection().await?;
        let key = self.key(key);
        self.run(connection.incr(key, amount)).await
    }

    pub async fn decrement(&self, key: &str, amount: i64) -> Result<i64, RedisStoreError> {
        let mut command = redis::cmd("DECRBY");
        command.arg(self.key(key)).arg(amount);
        self.query(&mut command).await
    }

    pub async fn expire(&self, key: &str, ttl: Duration) -> Result<bool, RedisStoreError> {
        if ttl.is_zero() {
            return Err(RedisStoreError::InvalidTtl);
        }
        let mut command = redis::cmd("PEXPIRE");
        command.arg(self.key(key)).arg(duration_millis(ttl)?);
        self.query(&mut command).await
    }

    pub async fn persist(&self, key: &str) -> Result<bool, RedisStoreError> {
        let mut command = redis::cmd("PERSIST");
        command.arg(self.key(key));
        self.query(&mut command).await
    }

    pub async fn ttl(&self, key: &str) -> Result<RedisTtl, RedisStoreError> {
        let mut command = redis::cmd("PTTL");
        command.arg(self.key(key));
        match self.query::<i64>(&mut command).await? {
            -2 => Ok(RedisTtl::Missing),
            -1 => Ok(RedisTtl::Persistent),
            millis if millis >= 0 => Ok(RedisTtl::ExpiresIn(Duration::from_millis(millis as u64))),
            value => Err(RedisStoreError::UnexpectedResponse(format!(
                "PTTL returned {value}"
            ))),
        }
    }

    pub async fn hash_get(
        &self,
        key: &str,
        field: &str,
    ) -> Result<Option<Vec<u8>>, RedisStoreError> {
        let mut command = redis::cmd("HGET");
        command.arg(self.key(key)).arg(field);
        self.query(&mut command).await
    }

    pub async fn hash_set(
        &self,
        key: &str,
        field: &str,
        value: impl AsRef<[u8]>,
    ) -> Result<bool, RedisStoreError> {
        let mut command = redis::cmd("HSET");
        command.arg(self.key(key)).arg(field).arg(value.as_ref());
        self.query(&mut command).await
    }

    pub async fn hash_get_all(
        &self,
        key: &str,
    ) -> Result<HashMap<Vec<u8>, Vec<u8>>, RedisStoreError> {
        let mut command = redis::cmd("HGETALL");
        command.arg(self.key(key));
        self.query(&mut command).await
    }

    pub async fn hash_delete(&self, key: &str, fields: &[&str]) -> Result<u64, RedisStoreError> {
        if fields.is_empty() {
            return Ok(0);
        }
        let mut command = redis::cmd("HDEL");
        command.arg(self.key(key)).arg(fields);
        self.query(&mut command).await
    }

    pub async fn hash_increment(
        &self,
        key: &str,
        field: &str,
        amount: i64,
    ) -> Result<i64, RedisStoreError> {
        let mut command = redis::cmd("HINCRBY");
        command.arg(self.key(key)).arg(field).arg(amount);
        self.query(&mut command).await
    }

    pub async fn list_push_front<V: AsRef<[u8]>>(
        &self,
        key: &str,
        values: &[V],
    ) -> Result<u64, RedisStoreError> {
        self.list_push("LPUSH", key, values).await
    }

    pub async fn list_push_back<V: AsRef<[u8]>>(
        &self,
        key: &str,
        values: &[V],
    ) -> Result<u64, RedisStoreError> {
        self.list_push("RPUSH", key, values).await
    }

    pub async fn list_pop_front(&self, key: &str) -> Result<Option<Vec<u8>>, RedisStoreError> {
        let mut command = redis::cmd("LPOP");
        command.arg(self.key(key));
        self.query(&mut command).await
    }

    pub async fn list_pop_back(&self, key: &str) -> Result<Option<Vec<u8>>, RedisStoreError> {
        let mut command = redis::cmd("RPOP");
        command.arg(self.key(key));
        self.query(&mut command).await
    }

    pub async fn list_range(
        &self,
        key: &str,
        start: isize,
        stop: isize,
    ) -> Result<Vec<Vec<u8>>, RedisStoreError> {
        let mut command = redis::cmd("LRANGE");
        command.arg(self.key(key)).arg(start).arg(stop);
        self.query(&mut command).await
    }

    pub async fn list_len(&self, key: &str) -> Result<u64, RedisStoreError> {
        let mut command = redis::cmd("LLEN");
        command.arg(self.key(key));
        self.query(&mut command).await
    }

    pub async fn set_add<V: AsRef<[u8]>>(
        &self,
        key: &str,
        members: &[V],
    ) -> Result<u64, RedisStoreError> {
        self.set_members_command("SADD", key, members).await
    }

    pub async fn set_remove<V: AsRef<[u8]>>(
        &self,
        key: &str,
        members: &[V],
    ) -> Result<u64, RedisStoreError> {
        self.set_members_command("SREM", key, members).await
    }

    pub async fn set_members(&self, key: &str) -> Result<HashSet<Vec<u8>>, RedisStoreError> {
        let mut command = redis::cmd("SMEMBERS");
        command.arg(self.key(key));
        self.query(&mut command).await
    }

    pub async fn set_contains(
        &self,
        key: &str,
        member: impl AsRef<[u8]>,
    ) -> Result<bool, RedisStoreError> {
        let mut command = redis::cmd("SISMEMBER");
        command.arg(self.key(key)).arg(member.as_ref());
        self.query(&mut command).await
    }

    pub async fn set_len(&self, key: &str) -> Result<u64, RedisStoreError> {
        let mut command = redis::cmd("SCARD");
        command.arg(self.key(key));
        self.query(&mut command).await
    }

    pub async fn sorted_set_add(
        &self,
        key: &str,
        score: f64,
        member: impl AsRef<[u8]>,
    ) -> Result<bool, RedisStoreError> {
        let mut command = redis::cmd("ZADD");
        command.arg(self.key(key)).arg(score).arg(member.as_ref());
        self.query(&mut command).await
    }

    pub async fn sorted_set_remove<V: AsRef<[u8]>>(
        &self,
        key: &str,
        members: &[V],
    ) -> Result<u64, RedisStoreError> {
        if members.is_empty() {
            return Ok(0);
        }
        let mut command = redis::cmd("ZREM");
        command.arg(self.key(key));
        for member in members {
            command.arg(member.as_ref());
        }
        self.query(&mut command).await
    }

    pub async fn sorted_set_range_with_scores(
        &self,
        key: &str,
        start: isize,
        stop: isize,
    ) -> Result<Vec<(Vec<u8>, f64)>, RedisStoreError> {
        let mut command = redis::cmd("ZRANGE");
        command
            .arg(self.key(key))
            .arg(start)
            .arg(stop)
            .arg("WITHSCORES");
        self.query(&mut command).await
    }

    pub async fn sorted_set_score(
        &self,
        key: &str,
        member: impl AsRef<[u8]>,
    ) -> Result<Option<f64>, RedisStoreError> {
        let mut command = redis::cmd("ZSCORE");
        command.arg(self.key(key)).arg(member.as_ref());
        self.query(&mut command).await
    }

    pub async fn sorted_set_len(&self, key: &str) -> Result<u64, RedisStoreError> {
        let mut command = redis::cmd("ZCARD");
        command.arg(self.key(key));
        self.query(&mut command).await
    }

    pub async fn publish(
        &self,
        channel: &str,
        message: impl AsRef<[u8]>,
    ) -> Result<u64, RedisStoreError> {
        let mut command = redis::cmd("PUBLISH");
        command.arg(self.key(channel)).arg(message.as_ref());
        self.query(&mut command).await
    }

    pub fn lock(&self, key: impl Into<String>, ttl: Duration) -> RedisLock {
        assert!(!ttl.is_zero(), "Redis lock TTL must be positive");
        RedisLock {
            store: self.clone(),
            key: key.into(),
            token: unique_token(),
            ttl,
            held: false,
        }
    }

    async fn connection(&self) -> Result<RedisConnection, RedisStoreError> {
        let connection = self.connection.get_or_try_init(|| async {
            match &self.client {
                RedisClient::Standalone(client) => client
                    .get_multiplexed_async_connection()
                    .await
                    .map(RedisConnection::Standalone),
                RedisClient::Cluster(client) => client
                    .get_async_connection()
                    .await
                    .map(RedisConnection::Cluster),
            }
        });
        tokio::time::timeout(self.config.operation_timeout, connection)
            .await
            .map_err(|_| RedisStoreError::Timeout)?
            .cloned()
            .map_err(RedisStoreError::Redis)
    }

    async fn run<T>(
        &self,
        future: impl Future<Output = redis::RedisResult<T>>,
    ) -> Result<T, RedisStoreError> {
        tokio::time::timeout(self.config.operation_timeout, future)
            .await
            .map_err(|_| RedisStoreError::Timeout)?
            .map_err(RedisStoreError::Redis)
    }

    async fn query<T: FromRedisValue>(&self, command: &mut Cmd) -> Result<T, RedisStoreError> {
        let mut connection = self.connection().await?;
        self.run(command.query_async(&mut connection)).await
    }

    async fn list_push<V: AsRef<[u8]>>(
        &self,
        name: &str,
        key: &str,
        values: &[V],
    ) -> Result<u64, RedisStoreError> {
        if values.is_empty() {
            return self.list_len(key).await;
        }
        let mut command = redis::cmd(name);
        command.arg(self.key(key));
        for value in values {
            command.arg(value.as_ref());
        }
        self.query(&mut command).await
    }

    async fn set_members_command<V: AsRef<[u8]>>(
        &self,
        name: &str,
        key: &str,
        members: &[V],
    ) -> Result<u64, RedisStoreError> {
        if members.is_empty() {
            return Ok(0);
        }
        let mut command = redis::cmd(name);
        command.arg(self.key(key));
        for member in members {
            command.arg(member.as_ref());
        }
        self.query(&mut command).await
    }

    fn key(&self, key: &str) -> String {
        format!("{}{}", self.config.key_prefix, key)
    }
}

#[derive(Clone)]
enum RedisClient {
    Standalone(redis::Client),
    Cluster(ClusterClient),
}

#[derive(Clone)]
enum RedisConnection {
    Standalone(MultiplexedConnection),
    Cluster(ClusterConnection),
}

impl ConnectionLike for RedisConnection {
    fn req_packed_command<'a>(&'a mut self, command: &'a Cmd) -> RedisFuture<'a, Value> {
        match self {
            Self::Standalone(connection) => connection.req_packed_command(command),
            Self::Cluster(connection) => connection.req_packed_command(command),
        }
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        pipeline: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        match self {
            Self::Standalone(connection) => connection.req_packed_commands(pipeline, offset, count),
            Self::Cluster(connection) => connection.req_packed_commands(pipeline, offset, count),
        }
    }

    fn get_db(&self) -> i64 {
        match self {
            Self::Standalone(connection) => connection.get_db(),
            Self::Cluster(connection) => connection.get_db(),
        }
    }
}

pub struct RedisLock {
    store: RedisStore,
    key: String,
    token: String,
    ttl: Duration,
    held: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedisTtl {
    Missing,
    Persistent,
    ExpiresIn(Duration),
}

/// JSON cache-aside access backed by Redis with per-key miss coalescing.
pub struct RedisJsonCache<T, E> {
    store: RedisStore,
    ttl: Duration,
    flights: SingleFlight<String, T, RedisCacheError<E>>,
    marker: PhantomData<fn() -> E>,
}

impl<T, E> RedisJsonCache<T, E>
where
    T: Clone + Serialize + DeserializeOwned,
{
    pub fn new(store: RedisStore, ttl: Duration) -> Self {
        assert!(!ttl.is_zero(), "cache-aside TTL must be positive");
        Self {
            store,
            ttl,
            flights: SingleFlight::new(),
            marker: PhantomData,
        }
    }

    pub async fn get_or_fetch<F, Fut>(
        &self,
        key: impl Into<String>,
        fetch: F,
    ) -> Result<T, SingleFlightError<RedisCacheError<E>>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let key = key.into();
        self.flights
            .execute(key.clone(), || async {
                if let Some(value) = self
                    .store
                    .get_json(&key)
                    .await
                    .map_err(RedisCacheError::Store)?
                {
                    return Ok(value);
                }
                let value = fetch().await.map_err(RedisCacheError::Fetch)?;
                self.store
                    .set_json(&key, &value, Some(self.ttl))
                    .await
                    .map_err(RedisCacheError::Store)?;
                Ok(value)
            })
            .await
    }

    pub async fn invalidate(&self, key: &str) -> Result<bool, RedisStoreError> {
        Ok(self.store.delete(&[key]).await? > 0)
    }
}

#[derive(Debug)]
pub enum RedisCacheError<E> {
    Store(RedisStoreError),
    Fetch(E),
}

impl<E: fmt::Display> fmt::Display for RedisCacheError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "cache store failed: {error}"),
            Self::Fetch(error) => write!(formatter, "cache fetch failed: {error}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for RedisCacheError<E> {}

impl RedisLock {
    pub async fn acquire(&mut self) -> Result<bool, RedisStoreError> {
        let mut connection = self.store.connection().await?;
        let result = self
            .store
            .run(
                redis::cmd("SET")
                    .arg(self.store.key(&self.key))
                    .arg(&self.token)
                    .arg("NX")
                    .arg("PX")
                    .arg(duration_millis(self.ttl)?)
                    .query_async::<Option<String>>(&mut connection),
            )
            .await?;
        self.held = result.is_some();
        Ok(self.held)
    }

    pub async fn extend(&self, ttl: Duration) -> Result<bool, RedisStoreError> {
        if !self.held || ttl.is_zero() {
            return Ok(false);
        }
        let mut connection = self.store.connection().await?;
        let changed = self
            .store
            .run(
                redis::Script::new(EXTEND_LOCK)
                    .key(self.store.key(&self.key))
                    .arg(&self.token)
                    .arg(duration_millis(ttl)?)
                    .invoke_async::<i64>(&mut connection),
            )
            .await?;
        Ok(changed == 1)
    }

    pub async fn release(&mut self) -> Result<bool, RedisStoreError> {
        if !self.held {
            return Ok(false);
        }
        let mut connection = self.store.connection().await?;
        let deleted = self
            .store
            .run(
                redis::Script::new(RELEASE_LOCK)
                    .key(self.store.key(&self.key))
                    .arg(&self.token)
                    .invoke_async::<i64>(&mut connection),
            )
            .await?;
        self.held = false;
        Ok(deleted == 1)
    }

    pub fn is_held(&self) -> bool {
        self.held
    }
}

#[derive(Debug)]
pub enum RedisStoreError {
    Redis(redis::RedisError),
    Json(serde_json::Error),
    Timeout,
    InvalidTtl,
    DurationOverflow,
    MissingEndpoint,
    UnexpectedResponse(String),
}

impl fmt::Display for RedisStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Redis(error) => write!(formatter, "Redis operation failed: {error}"),
            Self::Json(error) => write!(formatter, "Redis JSON conversion failed: {error}"),
            Self::Timeout => formatter.write_str("Redis operation timed out"),
            Self::InvalidTtl => formatter.write_str("Redis TTL must be positive"),
            Self::DurationOverflow => {
                formatter.write_str("Redis duration exceeds u64 milliseconds")
            }
            Self::MissingEndpoint => formatter.write_str("Redis configuration has no endpoint"),
            Self::UnexpectedResponse(response) => {
                write!(
                    formatter,
                    "Redis returned an unexpected response: {response}"
                )
            }
        }
    }
}

impl std::error::Error for RedisStoreError {}

impl From<redis::RedisError> for RedisStoreError {
    fn from(error: redis::RedisError) -> Self {
        Self::Redis(error)
    }
}

impl From<serde_json::Error> for RedisStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

fn duration_millis(duration: Duration) -> Result<u64, RedisStoreError> {
    duration
        .as_millis()
        .try_into()
        .map_err(|_| RedisStoreError::DurationOverflow)
}

fn unique_token() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{timestamp}-{sequence}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[test]
    fn prefixes_keys_and_creates_unique_lock_tokens() {
        let store =
            RedisStore::new(RedisStoreConfig::new("redis://127.0.0.1/").with_key_prefix("test:"))
                .unwrap();
        assert_eq!(store.key("users"), "test:users");
        assert_ne!(
            store.lock("lock", Duration::from_secs(1)).token,
            store.lock("lock", Duration::from_secs(1)).token
        );
    }

    #[test]
    fn rejects_zero_ttl_without_connecting() {
        let store = RedisStore::new(RedisStoreConfig::new("redis://127.0.0.1/")).unwrap();
        let lock = store.lock("valid", Duration::from_secs(1));
        assert!(!lock.held);
    }

    #[test]
    fn builds_cluster_from_multiple_seed_nodes() {
        let config =
            RedisStoreConfig::cluster(["redis://127.0.0.1:7000/", "redis://127.0.0.1:7001/"])
                .with_key_prefix("cluster:");
        let store = RedisStore::new(config).unwrap();
        assert!(matches!(store.client, RedisClient::Cluster(_)));
        assert_eq!(store.key("{user:7}:profile"), "cluster:{user:7}:profile");
    }

    #[test]
    fn rejects_an_empty_cluster_seed_list() {
        let error = RedisStore::new(RedisStoreConfig::cluster(Vec::<String>::new()))
            .err()
            .expect("empty cluster configuration must fail");
        assert!(matches!(error, RedisStoreError::Redis(_)));
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct User {
        id: u64,
        name: String,
    }

    #[tokio::test]
    async fn redis_integration_covers_json_counters_locks_and_cache_aside() {
        let Ok(url) = std::env::var("RUST_ZERO_REDIS_URL") else {
            return;
        };
        let store = RedisStore::new(
            RedisStoreConfig::new(url)
                .with_key_prefix(format!("rust-zero:{}:", std::process::id())),
        )
        .unwrap();
        store.ping().await.unwrap();
        store
            .delete(&[
                "user", "count", "lock", "string", "hash", "list", "set", "sorted",
            ])
            .await
            .unwrap();

        let user = User {
            id: 7,
            name: "Ada".to_owned(),
        };
        store
            .set_json("user", &user, Some(Duration::from_secs(10)))
            .await
            .unwrap();
        assert_eq!(store.get_json::<User>("user").await.unwrap(), Some(user));
        assert_eq!(store.increment("count", 2).await.unwrap(), 2);
        assert_eq!(store.decrement("count", 1).await.unwrap(), 1);

        assert!(store
            .set_if_absent("string", "first", Some(Duration::from_secs(10)))
            .await
            .unwrap());
        assert!(!store.set_if_absent("string", "second", None).await.unwrap());
        assert_eq!(
            store.get_string("string").await.unwrap().as_deref(),
            Some("first")
        );
        assert!(matches!(
            store.ttl("string").await.unwrap(),
            RedisTtl::ExpiresIn(_)
        ));
        assert!(store.persist("string").await.unwrap());
        assert_eq!(store.ttl("string").await.unwrap(), RedisTtl::Persistent);

        assert!(store.hash_set("hash", "name", "Ada").await.unwrap());
        assert_eq!(
            store.hash_get("hash", "name").await.unwrap().as_deref(),
            Some(b"Ada".as_slice())
        );
        assert_eq!(store.hash_increment("hash", "visits", 2).await.unwrap(), 2);
        assert_eq!(store.hash_get_all("hash").await.unwrap().len(), 2);

        assert_eq!(
            store.list_push_back("list", &["one", "two"]).await.unwrap(),
            2
        );
        assert_eq!(
            store.list_range("list", 0, -1).await.unwrap(),
            vec![b"one".to_vec(), b"two".to_vec()]
        );
        assert_eq!(
            store.list_pop_front("list").await.unwrap(),
            Some(b"one".to_vec())
        );

        assert_eq!(store.set_add("set", &["one", "two"]).await.unwrap(), 2);
        assert!(store.set_contains("set", "two").await.unwrap());
        assert_eq!(store.set_len("set").await.unwrap(), 2);

        assert!(store.sorted_set_add("sorted", 2.0, "two").await.unwrap());
        assert!(store.sorted_set_add("sorted", 1.0, "one").await.unwrap());
        assert_eq!(
            store
                .sorted_set_range_with_scores("sorted", 0, -1)
                .await
                .unwrap(),
            vec![(b"one".to_vec(), 1.0), (b"two".to_vec(), 2.0)]
        );

        let mut owner = store.lock("lock", Duration::from_secs(5));
        let mut contender = store.lock("lock", Duration::from_secs(5));
        assert!(owner.acquire().await.unwrap());
        assert!(!contender.acquire().await.unwrap());
        assert!(owner.extend(Duration::from_secs(10)).await.unwrap());
        assert!(owner.release().await.unwrap());
        assert!(contender.acquire().await.unwrap());
        assert!(contender.release().await.unwrap());

        let cache = RedisJsonCache::<User, String>::new(store.clone(), Duration::from_secs(10));
        cache.invalidate("cached-user").await.unwrap();
        let cached = cache
            .get_or_fetch("cached-user", || async {
                Ok(User {
                    id: 8,
                    name: "Grace".to_owned(),
                })
            })
            .await
            .unwrap();
        assert_eq!(cached.name, "Grace");
        let reused = cache
            .get_or_fetch("cached-user", || async { Err("must not fetch".to_owned()) })
            .await
            .unwrap();
        assert_eq!(reused, cached);
    }

    #[tokio::test]
    async fn redis_cluster_integration_routes_across_seed_nodes() {
        let Ok(nodes) = std::env::var("RUST_ZERO_REDIS_CLUSTER_URLS") else {
            return;
        };
        let store = RedisStore::new(
            RedisStoreConfig::cluster(
                nodes
                    .split(',')
                    .map(str::trim)
                    .filter(|node| !node.is_empty()),
            )
            .with_key_prefix(format!("rust-zero-cluster:{}:", std::process::id())),
        )
        .unwrap();

        store.ping().await.unwrap();
        store
            .set_many(&[("{one}:value", "one"), ("{two}:value", "two")])
            .await
            .unwrap();
        assert_eq!(
            store
                .get_many(&["{one}:value", "{two}:value"])
                .await
                .unwrap(),
            vec![Some(b"one".to_vec()), Some(b"two".to_vec())]
        );
        assert_eq!(
            store.delete(&["{one}:value", "{two}:value"]).await.unwrap(),
            2
        );
    }
}
