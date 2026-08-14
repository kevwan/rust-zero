//! Redis data structures, locks, streams, subscriptions, and model caching.
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use rust_zero_core::{RedisStore, RedisStoreConfig};
//! let redis = RedisStore::new(RedisStoreConfig::new("redis://127.0.0.1/"))?;
//! redis.ping().await?;
//! # Ok(())
//! # }
//! ```

use futures::StreamExt;
use redis::{
    aio::{ConnectionLike, MultiplexedConnection, PubSub},
    cluster::ClusterClient,
    cluster_async::ClusterConnection,
    streams::StreamReadReply,
    AsyncCommands, Cmd, FromRedisValue, Pipeline, RedisFuture, Value,
};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    marker::PhantomData,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, oneshot, OnceCell};

use crate::{
    cache::jittered_ttl, CacheStats, CounterVec, HistogramOptions, HistogramVec, Metrics,
    MetricsError, SingleFlight, SingleFlightError, VectorOptions,
};

#[cfg(feature = "telemetry")]
use crate::{TelemetrySpan, TelemetrySpanKind};

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
    metrics: Option<RedisStoreMetrics>,
}

/// Cardinality-bounded Redis command metrics installed on [`RedisStore`].
#[derive(Clone)]
pub struct RedisStoreMetrics {
    operations: CounterVec,
    duration: HistogramVec,
}

impl RedisStoreMetrics {
    pub fn register(metrics: &Metrics) -> Result<Self, MetricsError> {
        let labels = ["operation", "outcome"];
        Ok(Self {
            operations: metrics.counter_vec(
                VectorOptions::new("operations_total", "Completed Redis store operations")
                    .with_namespace("rust_zero")
                    .with_subsystem("redis")
                    .with_labels(labels),
            )?,
            duration: metrics.histogram_vec(
                HistogramOptions::new(
                    "operation_duration_seconds",
                    "Redis store operation latency",
                )
                .with_vector_options(
                    VectorOptions::new(
                        "operation_duration_seconds",
                        "Redis store operation latency",
                    )
                    .with_namespace("rust_zero")
                    .with_subsystem("redis")
                    .with_labels(labels),
                ),
            )?,
        })
    }

    fn observe(&self, operation: &str, outcome: &str, elapsed: Duration) {
        let labels = [operation, outcome];
        let _ = self.operations.inc(&labels);
        let _ = self.duration.observe(elapsed.as_secs_f64(), &labels);
    }
}

/// Delivery and reconnect policy for a Redis Pub/Sub subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedisSubscriptionConfig {
    /// Maximum number of events waiting for the consumer.
    pub capacity: usize,
    /// Delay before the first reconnect attempt.
    pub reconnect_delay: Duration,
    /// Upper bound for exponential reconnect backoff.
    pub max_reconnect_delay: Duration,
}

impl Default for RedisSubscriptionConfig {
    fn default() -> Self {
        Self {
            capacity: 256,
            reconnect_delay: Duration::from_millis(100),
            max_reconnect_delay: Duration::from_secs(5),
        }
    }
}

impl RedisSubscriptionConfig {
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    pub fn with_reconnect_delay(mut self, delay: Duration) -> Self {
        self.reconnect_delay = delay;
        self
    }

    pub fn with_max_reconnect_delay(mut self, delay: Duration) -> Self {
        self.max_reconnect_delay = delay;
        self
    }

    fn validate(self) -> Result<Self, RedisStoreError> {
        if self.capacity < 2 {
            return Err(RedisStoreError::InvalidArgument(
                "Redis subscription capacity must be at least two",
            ));
        }
        if self.reconnect_delay.is_zero() || self.max_reconnect_delay < self.reconnect_delay {
            return Err(RedisStoreError::InvalidArgument(
                "Redis subscription reconnect delays must be positive and ordered",
            ));
        }
        Ok(self)
    }
}

/// One binary-safe message received from a channel or pattern subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisSubscriptionMessage {
    pub channel: String,
    pub pattern: Option<String>,
    pub payload: Vec<u8>,
}

/// Observable subscription state and delivery events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedisSubscriptionEvent {
    Message(RedisSubscriptionMessage),
    /// Messages were discarded because the bounded consumer queue was full.
    Lagged {
        dropped: u64,
    },
    /// The connection ended or a reconnect attempt failed and will be retried.
    Disconnected {
        error: String,
        retry_in: Duration,
    },
    /// All requested channels or patterns were restored on a new connection.
    Reconnected,
    /// The subscription was explicitly shut down.
    Closed,
}

/// Receiver for a reconnecting Redis channel or pattern subscription.
pub struct RedisSubscription {
    receiver: mpsc::Receiver<RedisSubscriptionEvent>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl RedisSubscription {
    pub async fn recv(&mut self) -> Option<RedisSubscriptionEvent> {
        self.receiver.recv().await
    }

    pub fn try_recv(&mut self) -> Result<RedisSubscriptionEvent, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    /// Requests shutdown. A final [`RedisSubscriptionEvent::Closed`] is delivered before EOF.
    pub fn shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    /// Shuts down and waits until the worker has stopped.
    pub async fn close(mut self) {
        self.shutdown();
        while let Some(event) = self.recv().await {
            if event == RedisSubscriptionEvent::Closed {
                break;
            }
        }
    }
}

impl Drop for RedisSubscription {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug, Clone, Copy)]
enum RedisSubscriptionMode {
    Channels,
    Patterns,
}

/// One cursor page returned by Redis' incremental scan commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisScanPage<T> {
    pub cursor: u64,
    pub items: Vec<T>,
}

/// Source or destination side used by [`RedisStore::list_move`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedisListSide {
    Left,
    Right,
}

impl RedisListSide {
    fn argument(self) -> &'static str {
        match self {
            Self::Left => "LEFT",
            Self::Right => "RIGHT",
        }
    }
}

/// Bitwise operation used by [`RedisStore::bitmap_operation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedisBitOperation {
    And,
    Or,
    Xor,
    Not,
}

impl RedisBitOperation {
    fn argument(self) -> &'static str {
        match self {
            Self::And => "AND",
            Self::Or => "OR",
            Self::Xor => "XOR",
            Self::Not => "NOT",
        }
    }
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
            metrics: None,
        })
    }

    /// Installs metrics for commands executed by this store.
    pub fn with_metrics(mut self, metrics: RedisStoreMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    pub async fn ping(&self) -> Result<(), RedisStoreError> {
        let mut connection = self.connection().await?;
        self.run(
            "ping",
            redis::cmd("PING").query_async::<String>(&mut connection),
        )
        .await?;
        Ok(())
    }

    /// Executes an arbitrary Redis command using this store's connection and timeout policy.
    ///
    /// Arguments are passed through exactly as provided. In particular, keys in a raw command are
    /// not automatically namespaced with [`RedisStoreConfig::key_prefix`]; callers can use
    /// [`Self::prefixed_key`] when constructing commands that should share the store namespace.
    pub async fn do_command<T: FromRedisValue>(
        &self,
        mut command: Cmd,
    ) -> Result<T, RedisStoreError> {
        self.query(&mut command).await
    }

    /// Applies this store's configured namespace to a key for use with [`Self::do_command`].
    pub fn prefixed_key(&self, key: &str) -> String {
        self.key(key)
    }

    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, RedisStoreError> {
        let mut connection = self.connection().await?;
        let key = self.key(key);
        self.run("get", connection.get(key)).await
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
                self.run(
                    "set",
                    connection.pset_ex(key, value.as_ref(), duration_millis(ttl)?),
                )
                .await
            }
            None => self.run("set", connection.set(key, value.as_ref())).await,
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
        self.run("delete", connection.del(keys)).await
    }

    /// Asynchronously unlinks keys from the keyspace and returns the number found.
    pub async fn unlink(&self, keys: &[&str]) -> Result<u64, RedisStoreError> {
        if keys.is_empty() {
            return Ok(0);
        }
        let mut command = redis::cmd("UNLINK");
        command.arg(keys.iter().map(|key| self.key(key)).collect::<Vec<_>>());
        self.query(&mut command).await
    }

    /// Incrementally scans logical keys in this store's namespace.
    ///
    /// Returned keys have the configured prefix removed. In cluster mode, Redis applies `SCAN`
    /// to the routed node; callers that need a cluster-wide inventory should scan every node.
    pub async fn scan_keys(
        &self,
        cursor: u64,
        pattern: Option<&str>,
        count: Option<usize>,
    ) -> Result<RedisScanPage<Vec<u8>>, RedisStoreError> {
        let mut command = redis::cmd("SCAN");
        command.arg(cursor);
        let pattern = pattern.map(|pattern| self.key(pattern)).or_else(|| {
            (!self.config.key_prefix.is_empty()).then(|| format!("{}*", self.config.key_prefix))
        });
        append_scan_options(&mut command, pattern.as_deref(), count)?;
        let (cursor, keys): (u64, Vec<Vec<u8>>) = self.query(&mut command).await?;
        let prefix = self.config.key_prefix.as_bytes();
        Ok(RedisScanPage {
            cursor,
            items: keys
                .into_iter()
                .map(|key| key.strip_prefix(prefix).unwrap_or(&key).to_vec())
                .collect(),
        })
    }

    pub async fn exists(&self, key: &str) -> Result<bool, RedisStoreError> {
        let mut connection = self.connection().await?;
        let key = self.key(key);
        self.run("exists", connection.exists(key)).await
    }

    pub async fn increment(&self, key: &str, amount: i64) -> Result<i64, RedisStoreError> {
        let mut connection = self.connection().await?;
        let key = self.key(key);
        self.run("increment", connection.incr(key, amount)).await
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

    pub async fn bitmap_get(&self, key: &str, offset: u64) -> Result<bool, RedisStoreError> {
        let mut command = redis::cmd("GETBIT");
        command.arg(self.key(key)).arg(offset);
        self.query(&mut command).await
    }

    /// Sets one bit and returns its previous value.
    pub async fn bitmap_set(
        &self,
        key: &str,
        offset: u64,
        value: bool,
    ) -> Result<bool, RedisStoreError> {
        let mut command = redis::cmd("SETBIT");
        command.arg(self.key(key)).arg(offset).arg(u8::from(value));
        self.query(&mut command).await
    }

    /// Counts set bits in the whole value or an inclusive byte range.
    pub async fn bitmap_count(
        &self,
        key: &str,
        byte_range: Option<(isize, isize)>,
    ) -> Result<u64, RedisStoreError> {
        let mut command = redis::cmd("BITCOUNT");
        command.arg(self.key(key));
        if let Some((start, stop)) = byte_range {
            command.arg(start).arg(stop);
        }
        self.query(&mut command).await
    }

    /// Finds the first bit matching `value`, optionally within an inclusive byte range.
    pub async fn bitmap_position(
        &self,
        key: &str,
        value: bool,
        byte_range: Option<(isize, isize)>,
    ) -> Result<Option<u64>, RedisStoreError> {
        let mut command = redis::cmd("BITPOS");
        command.arg(self.key(key)).arg(u8::from(value));
        if let Some((start, stop)) = byte_range {
            command.arg(start).arg(stop);
        }
        match self.query::<i64>(&mut command).await? {
            -1 => Ok(None),
            position if position >= 0 => Ok(Some(position as u64)),
            value => Err(RedisStoreError::UnexpectedResponse(format!(
                "BITPOS returned {value}"
            ))),
        }
    }

    /// Applies a bitwise operation into `destination` and returns its resulting byte length.
    pub async fn bitmap_operation(
        &self,
        operation: RedisBitOperation,
        destination: &str,
        sources: &[&str],
    ) -> Result<u64, RedisStoreError> {
        let valid_source_count = match operation {
            RedisBitOperation::Not => sources.len() == 1,
            _ => !sources.is_empty(),
        };
        if !valid_source_count {
            return Err(RedisStoreError::InvalidArgument(
                "Redis BITOP requires one source for NOT and at least one for other operations",
            ));
        }
        let mut command = redis::cmd("BITOP");
        command
            .arg(operation.argument())
            .arg(self.key(destination))
            .arg(sources.iter().map(|key| self.key(key)).collect::<Vec<_>>());
        self.query(&mut command).await
    }

    /// Adds values to a HyperLogLog and reports whether its registers changed.
    pub async fn hyperloglog_add<V: AsRef<[u8]>>(
        &self,
        key: &str,
        values: &[V],
    ) -> Result<bool, RedisStoreError> {
        if values.is_empty() {
            return Err(RedisStoreError::InvalidArgument(
                "Redis PFADD requires at least one value",
            ));
        }
        let mut command = redis::cmd("PFADD");
        command.arg(self.key(key));
        for value in values {
            command.arg(value.as_ref());
        }
        self.query(&mut command).await
    }

    pub async fn hyperloglog_count(&self, keys: &[&str]) -> Result<u64, RedisStoreError> {
        if keys.is_empty() {
            return Err(RedisStoreError::InvalidArgument(
                "Redis PFCOUNT requires at least one key",
            ));
        }
        let mut command = redis::cmd("PFCOUNT");
        command.arg(keys.iter().map(|key| self.key(key)).collect::<Vec<_>>());
        self.query(&mut command).await
    }

    pub async fn hyperloglog_merge(
        &self,
        destination: &str,
        sources: &[&str],
    ) -> Result<(), RedisStoreError> {
        if sources.is_empty() {
            return Err(RedisStoreError::InvalidArgument(
                "Redis PFMERGE requires at least one source",
            ));
        }
        let mut command = redis::cmd("PFMERGE");
        command
            .arg(self.key(destination))
            .arg(sources.iter().map(|key| self.key(key)).collect::<Vec<_>>());
        expect_ok(self.query(&mut command).await?, "PFMERGE")
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

    pub async fn hash_scan(
        &self,
        key: &str,
        cursor: u64,
        pattern: Option<&str>,
        count: Option<usize>,
    ) -> Result<RedisScanPage<(Vec<u8>, Vec<u8>)>, RedisStoreError> {
        let mut command = redis::cmd("HSCAN");
        command.arg(self.key(key)).arg(cursor);
        append_scan_options(&mut command, pattern, count)?;
        let (cursor, items) = self.query(&mut command).await?;
        Ok(RedisScanPage { cursor, items })
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

    /// Blocks until a value can be popped from the first non-empty key or the timeout elapses.
    pub async fn list_blocking_pop_front(
        &self,
        keys: &[&str],
        timeout: Duration,
    ) -> Result<Option<(String, Vec<u8>)>, RedisStoreError> {
        self.list_blocking_pop("BLPOP", keys, timeout).await
    }

    /// Right-side counterpart of [`Self::list_blocking_pop_front`].
    pub async fn list_blocking_pop_back(
        &self,
        keys: &[&str],
        timeout: Duration,
    ) -> Result<Option<(String, Vec<u8>)>, RedisStoreError> {
        self.list_blocking_pop("BRPOP", keys, timeout).await
    }

    /// Atomically moves one list element and returns it, if the source exists.
    pub async fn list_move(
        &self,
        source: &str,
        destination: &str,
        source_side: RedisListSide,
        destination_side: RedisListSide,
    ) -> Result<Option<Vec<u8>>, RedisStoreError> {
        let mut command = redis::cmd("LMOVE");
        command
            .arg(self.key(source))
            .arg(self.key(destination))
            .arg(source_side.argument())
            .arg(destination_side.argument());
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

    pub async fn set_scan(
        &self,
        key: &str,
        cursor: u64,
        pattern: Option<&str>,
        count: Option<usize>,
    ) -> Result<RedisScanPage<Vec<u8>>, RedisStoreError> {
        let mut command = redis::cmd("SSCAN");
        command.arg(self.key(key)).arg(cursor);
        append_scan_options(&mut command, pattern, count)?;
        let (cursor, items) = self.query(&mut command).await?;
        Ok(RedisScanPage { cursor, items })
    }

    /// Atomically moves a set member and reports whether it existed in the source.
    pub async fn set_move(
        &self,
        source: &str,
        destination: &str,
        member: impl AsRef<[u8]>,
    ) -> Result<bool, RedisStoreError> {
        let mut command = redis::cmd("SMOVE");
        command
            .arg(self.key(source))
            .arg(self.key(destination))
            .arg(member.as_ref());
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

    pub async fn sorted_set_reverse_range_with_scores(
        &self,
        key: &str,
        start: isize,
        stop: isize,
    ) -> Result<Vec<(Vec<u8>, f64)>, RedisStoreError> {
        let mut command = redis::cmd("ZREVRANGE");
        command
            .arg(self.key(key))
            .arg(start)
            .arg(stop)
            .arg("WITHSCORES");
        self.query(&mut command).await
    }

    pub async fn sorted_set_range_by_score_with_scores(
        &self,
        key: &str,
        min: f64,
        max: f64,
        limit: Option<(usize, usize)>,
    ) -> Result<Vec<(Vec<u8>, f64)>, RedisStoreError> {
        self.sorted_set_score_range("ZRANGEBYSCORE", key, min, max, limit)
            .await
    }

    pub async fn sorted_set_reverse_range_by_score_with_scores(
        &self,
        key: &str,
        min: f64,
        max: f64,
        limit: Option<(usize, usize)>,
    ) -> Result<Vec<(Vec<u8>, f64)>, RedisStoreError> {
        self.sorted_set_score_range("ZREVRANGEBYSCORE", key, min, max, limit)
            .await
    }

    pub async fn sorted_set_increment(
        &self,
        key: &str,
        amount: f64,
        member: impl AsRef<[u8]>,
    ) -> Result<f64, RedisStoreError> {
        let mut command = redis::cmd("ZINCRBY");
        command.arg(self.key(key)).arg(amount).arg(member.as_ref());
        self.query(&mut command).await
    }

    pub async fn sorted_set_rank(
        &self,
        key: &str,
        member: impl AsRef<[u8]>,
        reverse: bool,
    ) -> Result<Option<u64>, RedisStoreError> {
        let mut command = redis::cmd(if reverse { "ZREVRANK" } else { "ZRANK" });
        command.arg(self.key(key)).arg(member.as_ref());
        self.query(&mut command).await
    }

    pub async fn sorted_set_remove_by_rank(
        &self,
        key: &str,
        start: isize,
        stop: isize,
    ) -> Result<u64, RedisStoreError> {
        let mut command = redis::cmd("ZREMRANGEBYRANK");
        command.arg(self.key(key)).arg(start).arg(stop);
        self.query(&mut command).await
    }

    pub async fn sorted_set_remove_by_score(
        &self,
        key: &str,
        min: f64,
        max: f64,
    ) -> Result<u64, RedisStoreError> {
        let mut command = redis::cmd("ZREMRANGEBYSCORE");
        command.arg(self.key(key)).arg(min).arg(max);
        self.query(&mut command).await
    }

    pub async fn sorted_set_scan(
        &self,
        key: &str,
        cursor: u64,
        pattern: Option<&str>,
        count: Option<usize>,
    ) -> Result<RedisScanPage<(Vec<u8>, f64)>, RedisStoreError> {
        let mut command = redis::cmd("ZSCAN");
        command.arg(self.key(key)).arg(cursor);
        append_scan_options(&mut command, pattern, count)?;
        let (cursor, items) = self.query(&mut command).await?;
        Ok(RedisScanPage { cursor, items })
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

    /// Subscribes to exact channel names using a reconnecting, bounded receiver.
    pub async fn subscribe<I, S>(
        &self,
        channels: I,
        config: RedisSubscriptionConfig,
    ) -> Result<RedisSubscription, RedisStoreError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.start_subscription(channels, config, RedisSubscriptionMode::Channels)
            .await
    }

    /// Subscribes to Redis channel patterns using a reconnecting, bounded receiver.
    pub async fn psubscribe<I, S>(
        &self,
        patterns: I,
        config: RedisSubscriptionConfig,
    ) -> Result<RedisSubscription, RedisStoreError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.start_subscription(patterns, config, RedisSubscriptionMode::Patterns)
            .await
    }

    /// Executes a caller-built Redis pipeline using the shared connection and operation timeout.
    ///
    /// Like [`Self::do_command`], pipeline keys are passed through unchanged. Use
    /// [`Self::prefixed_key`] when adding keys that belong to this store's namespace.
    pub async fn do_pipeline<T: FromRedisValue>(
        &self,
        pipeline: &Pipeline,
    ) -> Result<T, RedisStoreError> {
        let mut connection = self.connection().await?;
        self.run("pipeline", pipeline.query_async(&mut connection))
            .await
    }

    /// Builds and executes an atomic `MULTI`/`EXEC` transaction.
    ///
    /// All keys in a clustered transaction must belong to the same Redis hash slot.
    pub async fn transaction<T, F>(&self, build: F) -> Result<T, RedisStoreError>
    where
        T: FromRedisValue,
        F: FnOnce(&mut Pipeline),
    {
        let mut pipeline = redis::pipe();
        pipeline.atomic();
        build(&mut pipeline);
        let mut connection = self.connection().await?;
        self.run("transaction", pipeline.query_async(&mut connection))
            .await
    }

    /// Evaluates a Lua script with automatically prefixed keys and binary-safe arguments.
    pub async fn eval<T: FromRedisValue, K: AsRef<str>, A: AsRef<[u8]>>(
        &self,
        script: &str,
        keys: &[K],
        arguments: &[A],
    ) -> Result<T, RedisStoreError> {
        let mut command = redis::cmd("EVAL");
        command.arg(script).arg(keys.len());
        for key in keys {
            command.arg(self.key(key.as_ref()));
        }
        for argument in arguments {
            command.arg(argument.as_ref());
        }
        self.query(&mut command).await
    }

    /// Appends a field/value entry to a Redis stream and returns its generated or explicit ID.
    pub async fn stream_add<V: AsRef<[u8]>>(
        &self,
        key: &str,
        id: Option<&str>,
        fields: &[(&str, V)],
    ) -> Result<String, RedisStoreError> {
        if fields.is_empty() {
            return Err(RedisStoreError::InvalidArgument(
                "Redis stream entries require at least one field",
            ));
        }
        let mut command = redis::cmd("XADD");
        command.arg(self.key(key)).arg(id.unwrap_or("*"));
        for (field, value) in fields {
            command.arg(field).arg(value.as_ref());
        }
        self.query(&mut command).await
    }

    /// Reads entries from one or more streams using `XREAD`.
    pub async fn stream_read(
        &self,
        streams: &[(&str, &str)],
        count: Option<usize>,
        block: Option<Duration>,
    ) -> Result<StreamReadReply, RedisStoreError> {
        let mut command = redis::cmd("XREAD");
        append_stream_read_options(&mut command, count, block, false)?;
        append_streams(&mut command, streams, |key| self.key(key))?;
        self.query(&mut command).await
    }

    /// Creates a consumer group at `id`, optionally creating an empty stream first.
    pub async fn stream_group_create(
        &self,
        key: &str,
        group: &str,
        id: &str,
        create_stream: bool,
    ) -> Result<(), RedisStoreError> {
        let mut command = redis::cmd("XGROUP");
        command.arg("CREATE").arg(self.key(key)).arg(group).arg(id);
        if create_stream {
            command.arg("MKSTREAM");
        }
        expect_ok(self.query(&mut command).await?, "XGROUP CREATE")
    }

    /// Destroys a consumer group and reports whether it existed.
    pub async fn stream_group_destroy(
        &self,
        key: &str,
        group: &str,
    ) -> Result<bool, RedisStoreError> {
        let mut command = redis::cmd("XGROUP");
        command.arg("DESTROY").arg(self.key(key)).arg(group);
        self.query(&mut command).await
    }

    /// Reads stream entries as a consumer group member using `XREADGROUP`.
    pub async fn stream_group_read(
        &self,
        group: &str,
        consumer: &str,
        streams: &[(&str, &str)],
        count: Option<usize>,
        block: Option<Duration>,
        no_ack: bool,
    ) -> Result<StreamReadReply, RedisStoreError> {
        if group.is_empty() || consumer.is_empty() {
            return Err(RedisStoreError::InvalidArgument(
                "Redis stream group and consumer names must not be empty",
            ));
        }
        let mut command = redis::cmd("XREADGROUP");
        command.arg("GROUP").arg(group).arg(consumer);
        append_stream_read_options(&mut command, count, block, no_ack)?;
        append_streams(&mut command, streams, |key| self.key(key))?;
        self.query(&mut command).await
    }

    /// Acknowledges delivered stream entries and returns the number newly acknowledged.
    pub async fn stream_ack(
        &self,
        key: &str,
        group: &str,
        ids: &[&str],
    ) -> Result<u64, RedisStoreError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut command = redis::cmd("XACK");
        command.arg(self.key(key)).arg(group).arg(ids);
        self.query(&mut command).await
    }

    /// Returns the raw `XPENDING` summary so callers retain Redis-version-specific fields.
    pub async fn stream_pending(&self, key: &str, group: &str) -> Result<Value, RedisStoreError> {
        let mut command = redis::cmd("XPENDING");
        command.arg(self.key(key)).arg(group);
        self.query(&mut command).await
    }

    /// Returns Redis-version-specific stream metadata from `XINFO STREAM`.
    pub async fn stream_info(&self, key: &str) -> Result<Value, RedisStoreError> {
        let mut command = redis::cmd("XINFO");
        command.arg("STREAM").arg(self.key(key));
        self.query(&mut command).await
    }

    /// Returns Redis-version-specific group metadata from `XINFO GROUPS`.
    pub async fn stream_group_info(&self, key: &str) -> Result<Value, RedisStoreError> {
        let mut command = redis::cmd("XINFO");
        command.arg("GROUPS").arg(self.key(key));
        self.query(&mut command).await
    }

    /// Returns Redis-version-specific consumer metadata from `XINFO CONSUMERS`.
    pub async fn stream_consumer_info(
        &self,
        key: &str,
        group: &str,
    ) -> Result<Value, RedisStoreError> {
        let mut command = redis::cmd("XINFO");
        command.arg("CONSUMERS").arg(self.key(key)).arg(group);
        self.query(&mut command).await
    }

    /// Claims pending entries for another consumer using `XCLAIM`.
    ///
    /// The response remains a raw Redis value because newer Redis versions may add optional
    /// fields while retaining wire compatibility.
    pub async fn stream_claim(
        &self,
        key: &str,
        group: &str,
        consumer: &str,
        min_idle: Duration,
        ids: &[&str],
    ) -> Result<Value, RedisStoreError> {
        if ids.is_empty() {
            return Err(RedisStoreError::InvalidArgument(
                "Redis stream claim requires at least one entry ID",
            ));
        }
        let mut command = redis::cmd("XCLAIM");
        command
            .arg(self.key(key))
            .arg(group)
            .arg(consumer)
            .arg(duration_millis(min_idle)?)
            .arg(ids);
        self.query(&mut command).await
    }

    /// Deletes stream entries and returns the number removed.
    pub async fn stream_delete(&self, key: &str, ids: &[&str]) -> Result<u64, RedisStoreError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut command = redis::cmd("XDEL");
        command.arg(self.key(key)).arg(ids);
        self.query(&mut command).await
    }

    /// Moves a Redis stream consumer group's last-delivered cursor.
    ///
    /// `id` accepts Redis stream IDs as well as the special `$` and `0` values. This corresponds
    /// to `XGROUP SETID`, including the helper added by go-zero v1.10.3.
    pub async fn stream_group_set_id(
        &self,
        key: &str,
        group: &str,
        id: &str,
    ) -> Result<(), RedisStoreError> {
        let mut command = redis::cmd("XGROUP");
        command.arg("SETID").arg(self.key(key)).arg(group).arg(id);
        let response: String = self.query(&mut command).await?;
        if response == "OK" {
            Ok(())
        } else {
            Err(RedisStoreError::UnexpectedResponse(format!(
                "XGROUP SETID returned {response}"
            )))
        }
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

    async fn start_subscription<I, S>(
        &self,
        topics: I,
        subscription_config: RedisSubscriptionConfig,
        mode: RedisSubscriptionMode,
    ) -> Result<RedisSubscription, RedisStoreError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let subscription_config = subscription_config.validate()?;
        let topics: Vec<String> = topics
            .into_iter()
            .map(|topic| topic.as_ref().to_owned())
            .collect();
        if topics.is_empty() || topics.iter().any(String::is_empty) {
            return Err(RedisStoreError::InvalidArgument(
                "Redis subscriptions require at least one non-empty channel or pattern",
            ));
        }
        let topics: Vec<String> = topics.iter().map(|topic| self.key(topic)).collect();

        let (pubsub, endpoint_index) = connect_subscription(
            &self.config.urls,
            0,
            &topics,
            mode,
            self.config.operation_timeout,
        )
        .await?;
        let (sender, receiver) = mpsc::channel(subscription_config.capacity);
        let (shutdown, shutdown_receiver) = oneshot::channel();
        tokio::spawn(run_subscription(
            pubsub,
            self.config.urls.clone(),
            endpoint_index,
            topics,
            mode,
            self.config.key_prefix.clone(),
            self.config.operation_timeout,
            subscription_config,
            sender,
            shutdown_receiver,
        ));
        Ok(RedisSubscription {
            receiver,
            shutdown: Some(shutdown),
        })
    }

    async fn run<T>(
        &self,
        operation: &'static str,
        future: impl Future<Output = redis::RedisResult<T>>,
    ) -> Result<T, RedisStoreError> {
        self.instrument(operation, future).await
    }

    async fn query<T: FromRedisValue>(&self, command: &mut Cmd) -> Result<T, RedisStoreError> {
        let mut connection = self.connection().await?;
        let operation = redis_command_name(command);
        self.instrument(operation, command.query_async(&mut connection))
            .await
    }

    async fn instrument<T>(
        &self,
        operation: &'static str,
        future: impl Future<Output = redis::RedisResult<T>>,
    ) -> Result<T, RedisStoreError> {
        let started = Instant::now();
        #[cfg(feature = "telemetry")]
        let span = TelemetrySpan::start(
            format!("redis.{operation}"),
            TelemetrySpanKind::Client,
            None,
            [("db.operation.name", operation.to_owned())],
        );
        let result = match tokio::time::timeout(self.config.operation_timeout, future).await {
            Ok(result) => result.map_err(RedisStoreError::Redis),
            Err(_) => Err(RedisStoreError::Timeout),
        };
        if let Some(metrics) = &self.metrics {
            metrics.observe(operation, redis_outcome(&result), started.elapsed());
        }
        #[cfg(feature = "telemetry")]
        if let Err(error) = &result {
            span.set_error(error.to_string());
        }
        result
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

    async fn list_blocking_pop(
        &self,
        name: &str,
        keys: &[&str],
        timeout: Duration,
    ) -> Result<Option<(String, Vec<u8>)>, RedisStoreError> {
        if keys.is_empty() || timeout.is_zero() {
            return Err(RedisStoreError::InvalidArgument(
                "Redis blocking list pop requires keys and a positive timeout",
            ));
        }
        let mut command = redis::cmd(name);
        command
            .arg(keys.iter().map(|key| self.key(key)).collect::<Vec<_>>())
            .arg(timeout.as_secs_f64());
        let result: Option<(String, Vec<u8>)> = self.query(&mut command).await?;
        Ok(result.map(|(key, value)| {
            let key = key
                .strip_prefix(&self.config.key_prefix)
                .unwrap_or(&key)
                .to_owned();
            (key, value)
        }))
    }

    async fn sorted_set_score_range(
        &self,
        name: &str,
        key: &str,
        min: f64,
        max: f64,
        limit: Option<(usize, usize)>,
    ) -> Result<Vec<(Vec<u8>, f64)>, RedisStoreError> {
        let mut command = redis::cmd(name);
        command.arg(self.key(key));
        if name == "ZREVRANGEBYSCORE" {
            command.arg(max).arg(min);
        } else {
            command.arg(min).arg(max);
        }
        command.arg("WITHSCORES");
        if let Some((offset, count)) = limit {
            if count == 0 {
                return Ok(Vec::new());
            }
            command.arg("LIMIT").arg(offset).arg(count);
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

/// Expiry and not-found policy for a Redis-backed model cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedisModelCacheConfig {
    pub ttl: Duration,
    pub not_found_ttl: Duration,
    pub expiry_jitter: Duration,
    pub cache_not_found: bool,
}

impl RedisModelCacheConfig {
    pub fn new(ttl: Duration) -> Self {
        assert!(!ttl.is_zero(), "model cache TTL must be positive");
        Self {
            ttl,
            not_found_ttl: Duration::from_secs(5),
            expiry_jitter: Duration::ZERO,
            cache_not_found: true,
        }
    }

    pub fn with_not_found_ttl(mut self, ttl: Duration) -> Self {
        assert!(!ttl.is_zero(), "model cache not-found TTL must be positive");
        self.not_found_ttl = ttl;
        self
    }

    pub fn with_expiry_jitter(mut self, jitter: Duration) -> Self {
        self.expiry_jitter = jitter;
        self
    }

    pub fn with_cache_not_found(mut self, enabled: bool) -> Self {
        self.cache_not_found = enabled;
        self
    }
}

#[derive(Serialize, serde::Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
enum ModelCacheEntry<T> {
    Value(T),
    NotFound,
}

#[derive(Default)]
struct RedisModelCacheCounters {
    hits: AtomicU64,
    misses: AtomicU64,
    insertions: AtomicU64,
    evictions: AtomicU64,
}

/// Redis-backed positive and not-found model caching with per-process miss coalescing.
///
/// Cache entries use a tagged JSON representation, so a cached not-found result cannot be
/// confused with a missing Redis key. Separate instances that use the same [`RedisStore`]
/// namespace observe writes and invalidations immediately, which supports cross-process model
/// mutation invalidation without an additional in-memory coherence layer.
pub struct RedisModelCache<T, E> {
    store: RedisStore,
    config: RedisModelCacheConfig,
    flights: SingleFlight<String, Option<T>, RedisModelCacheError<E>>,
    counters: RedisModelCacheCounters,
    ttl_sequence: AtomicU64,
    marker: PhantomData<fn() -> E>,
}

impl<T, E> RedisModelCache<T, E>
where
    T: Clone + Serialize + DeserializeOwned,
{
    pub fn new(store: RedisStore, config: RedisModelCacheConfig) -> Self {
        Self {
            store,
            config,
            flights: SingleFlight::new(),
            counters: RedisModelCacheCounters::default(),
            ttl_sequence: AtomicU64::new(0),
            marker: PhantomData,
        }
    }

    /// Returns a cached model, a cached not-found result, or invokes `fetch` on a cache miss.
    pub async fn get_or_fetch<F, Fut>(
        &self,
        key: impl Into<String>,
        fetch: F,
    ) -> Result<Option<T>, SingleFlightError<RedisModelCacheError<E>>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<T>, E>>,
    {
        let key = key.into();
        self.flights
            .execute(key.clone(), || async {
                if let Some(bytes) = self
                    .store
                    .get(&key)
                    .await
                    .map_err(RedisModelCacheError::Store)?
                {
                    let entry = serde_json::from_slice::<ModelCacheEntry<T>>(&bytes)
                        .map_err(RedisModelCacheError::Serialization)?;
                    self.counters.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(match entry {
                        ModelCacheEntry::Value(value) => Some(value),
                        ModelCacheEntry::NotFound => None,
                    });
                }

                self.counters.misses.fetch_add(1, Ordering::Relaxed);
                let value = fetch().await.map_err(RedisModelCacheError::Fetch)?;
                match &value {
                    Some(value) => {
                        self.set_entry(&key, &ModelCacheEntry::Value(value), self.config.ttl)
                            .await?;
                    }
                    None if self.config.cache_not_found => {
                        self.set_entry::<T>(
                            &key,
                            &ModelCacheEntry::NotFound,
                            self.config.not_found_ttl,
                        )
                        .await?;
                    }
                    None => {}
                }
                Ok(value)
            })
            .await
    }

    /// Deletes all supplied model/index keys, routing each key independently for Redis Cluster.
    pub async fn invalidate(&self, keys: &[&str]) -> Result<u64, RedisStoreError> {
        let mut removed = 0;
        for key in keys {
            removed += self.store.delete(&[*key]).await?;
        }
        self.counters
            .evictions
            .fetch_add(removed, Ordering::Relaxed);
        Ok(removed)
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.counters.hits.load(Ordering::Relaxed),
            misses: self.counters.misses.load(Ordering::Relaxed),
            insertions: self.counters.insertions.load(Ordering::Relaxed),
            evictions: self.counters.evictions.load(Ordering::Relaxed),
        }
    }

    async fn set_entry<U: Serialize>(
        &self,
        key: &str,
        entry: &ModelCacheEntry<U>,
        base_ttl: Duration,
    ) -> Result<(), RedisModelCacheError<E>> {
        let bytes = serde_json::to_vec(entry).map_err(RedisModelCacheError::Serialization)?;
        let sequence = self.ttl_sequence.fetch_add(1, Ordering::Relaxed);
        let ttl = jittered_ttl(base_ttl, self.config.expiry_jitter, sequence);
        self.store
            .set(key, bytes, Some(ttl))
            .await
            .map_err(RedisModelCacheError::Store)?;
        self.counters.insertions.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[derive(Debug)]
pub enum RedisModelCacheError<E> {
    Store(RedisStoreError),
    Serialization(serde_json::Error),
    Fetch(E),
}

impl<E: fmt::Display> fmt::Display for RedisModelCacheError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "model cache store failed: {error}"),
            Self::Serialization(error) => {
                write!(formatter, "model cache serialization failed: {error}")
            }
            Self::Fetch(error) => write!(formatter, "model cache fetch failed: {error}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for RedisModelCacheError<E> {}

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
                "lock_acquire",
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
                "lock_extend",
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
                "lock_release",
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
    InvalidArgument(&'static str),
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
            Self::InvalidArgument(message) => formatter.write_str(message),
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

fn redis_outcome<T>(result: &Result<T, RedisStoreError>) -> &'static str {
    match result {
        Ok(_) => "success",
        Err(RedisStoreError::Timeout) => "timeout",
        Err(_) => "error",
    }
}

/// Extracts a bounded command label from Redis' packed RESP representation.
/// Unknown application-supplied commands collapse to `other` to prevent metric-cardinality leaks.
fn redis_command_name(command: &Cmd) -> &'static str {
    let packed = command.get_packed_command();
    let name = packed
        .split(|byte| *byte == b'\n')
        .nth(2)
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line));
    match name {
        Some(b"DECRBY") => "decrement",
        Some(b"DEL") => "delete",
        Some(b"BITCOUNT") => "bitmap_count",
        Some(b"BITOP") => "bitmap_operation",
        Some(b"BITPOS") => "bitmap_position",
        Some(b"BLPOP") => "list_blocking_pop_left",
        Some(b"BRPOP") => "list_blocking_pop_right",
        Some(b"EVAL") => "eval",
        Some(b"EXISTS") => "exists",
        Some(b"GET") => "get",
        Some(b"GETBIT") => "bitmap_get",
        Some(b"HDEL") => "hash_delete",
        Some(b"HGET") => "hash_get",
        Some(b"HGETALL") => "hash_get_all",
        Some(b"HINCRBY") => "hash_increment",
        Some(b"HSCAN") => "hash_scan",
        Some(b"HSET") => "hash_set",
        Some(b"INCRBY") => "increment",
        Some(b"LLEN") => "list_len",
        Some(b"LMOVE") => "list_move",
        Some(b"LPOP") => "list_pop_left",
        Some(b"LPUSH") => "list_push_left",
        Some(b"LRANGE") => "list_range",
        Some(b"MGET") => "get_many",
        Some(b"MSET") => "set_many",
        Some(b"PERSIST") => "persist",
        Some(b"PFADD") => "hyperloglog_add",
        Some(b"PFCOUNT") => "hyperloglog_count",
        Some(b"PFMERGE") => "hyperloglog_merge",
        Some(b"PEXPIRE") => "expire",
        Some(b"PING") => "ping",
        Some(b"PTTL") => "ttl",
        Some(b"PUBLISH") => "publish",
        Some(b"RPOP") => "list_pop_right",
        Some(b"RPUSH") => "list_push_right",
        Some(b"SADD") => "set_add",
        Some(b"SCAN") => "scan_keys",
        Some(b"SCARD") => "set_len",
        Some(b"SET") => "set",
        Some(b"SETBIT") => "bitmap_set",
        Some(b"SISMEMBER") => "set_contains",
        Some(b"SMEMBERS") => "set_members",
        Some(b"SMOVE") => "set_move",
        Some(b"SREM") => "set_remove",
        Some(b"SSCAN") => "set_scan",
        Some(b"UNLINK") => "unlink",
        Some(b"XACK") => "stream_ack",
        Some(b"XADD") => "stream_add",
        Some(b"XCLAIM") => "stream_claim",
        Some(b"XDEL") => "stream_delete",
        Some(b"XGROUP") => "stream_group",
        Some(b"XINFO") => "stream_info",
        Some(b"XPENDING") => "stream_pending",
        Some(b"XREAD") => "stream_read",
        Some(b"XREADGROUP") => "stream_group_read",
        Some(b"ZADD") => "sorted_set_add",
        Some(b"ZCARD") => "sorted_set_len",
        Some(b"ZINCRBY") => "sorted_set_increment",
        Some(b"ZRANGE") => "sorted_set_range",
        Some(b"ZRANGEBYSCORE") => "sorted_set_score_range",
        Some(b"ZREM") => "sorted_set_remove",
        Some(b"ZREMRANGEBYRANK") => "sorted_set_remove_rank",
        Some(b"ZREMRANGEBYSCORE") => "sorted_set_remove_score",
        Some(b"ZREVRANGE") => "sorted_set_reverse_range",
        Some(b"ZREVRANGEBYSCORE") => "sorted_set_reverse_score_range",
        Some(b"ZREVRANK") => "sorted_set_reverse_rank",
        Some(b"ZRANK") => "sorted_set_rank",
        Some(b"ZSCAN") => "sorted_set_scan",
        Some(b"ZSCORE") => "sorted_set_score",
        _ => "other",
    }
}

async fn connect_subscription(
    urls: &[String],
    start_index: usize,
    topics: &[String],
    mode: RedisSubscriptionMode,
    timeout: Duration,
) -> Result<(PubSub, usize), RedisStoreError> {
    if urls.is_empty() {
        return Err(RedisStoreError::MissingEndpoint);
    }
    let mut last_error = None;
    for offset in 0..urls.len() {
        let index = (start_index + offset) % urls.len();
        let connection = async {
            let client = redis::Client::open(urls[index].as_str())?;
            let mut pubsub = client.get_async_pubsub().await?;
            match mode {
                RedisSubscriptionMode::Channels => pubsub.subscribe(topics).await?,
                RedisSubscriptionMode::Patterns => pubsub.psubscribe(topics).await?,
            }
            Ok::<_, redis::RedisError>(pubsub)
        };
        match tokio::time::timeout(timeout, connection).await {
            Ok(Ok(pubsub)) => return Ok((pubsub, index)),
            Ok(Err(error)) => last_error = Some(RedisStoreError::Redis(error)),
            Err(_) => last_error = Some(RedisStoreError::Timeout),
        }
    }
    Err(last_error.unwrap_or(RedisStoreError::MissingEndpoint))
}

#[allow(clippy::too_many_arguments)]
async fn run_subscription(
    mut pubsub: PubSub,
    urls: Vec<String>,
    mut endpoint_index: usize,
    topics: Vec<String>,
    mode: RedisSubscriptionMode,
    key_prefix: String,
    operation_timeout: Duration,
    config: RedisSubscriptionConfig,
    sender: mpsc::Sender<RedisSubscriptionEvent>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut dropped = 0_u64;
    loop {
        let disconnected = {
            let mut messages = pubsub.on_message();
            loop {
                tokio::select! {
                    _ = &mut shutdown => {
                        let _ = sender.send(RedisSubscriptionEvent::Closed).await;
                        return;
                    }
                    _ = sender.closed() => return,
                    message = messages.next() => match message {
                        Some(message) => enqueue_subscription_message(
                            &sender,
                            subscription_message(message, &key_prefix),
                            &mut dropped,
                        ),
                        None => break "Redis Pub/Sub connection closed".to_owned(),
                    }
                }
            }
        };

        if dropped > 0 {
            let event = RedisSubscriptionEvent::Lagged { dropped };
            if !send_subscription_event(&sender, event, &mut shutdown).await {
                return;
            }
            dropped = 0;
        }

        let mut delay = config.reconnect_delay;
        let mut error = disconnected;
        loop {
            if !send_subscription_event(
                &sender,
                RedisSubscriptionEvent::Disconnected {
                    error,
                    retry_in: delay,
                },
                &mut shutdown,
            )
            .await
            {
                return;
            }
            tokio::select! {
                _ = &mut shutdown => {
                    let _ = sender.send(RedisSubscriptionEvent::Closed).await;
                    return;
                }
                _ = sender.closed() => return,
                _ = tokio::time::sleep(delay) => {}
            }

            let start_index = (endpoint_index + 1) % urls.len();
            let reconnect =
                connect_subscription(&urls, start_index, &topics, mode, operation_timeout);
            let result = tokio::select! {
                _ = &mut shutdown => {
                    let _ = sender.send(RedisSubscriptionEvent::Closed).await;
                    return;
                }
                _ = sender.closed() => return,
                result = reconnect => result,
            };
            match result {
                Ok((connection, index)) => {
                    pubsub = connection;
                    endpoint_index = index;
                    if !send_subscription_event(
                        &sender,
                        RedisSubscriptionEvent::Reconnected,
                        &mut shutdown,
                    )
                    .await
                    {
                        return;
                    }
                    break;
                }
                Err(reconnect_error) => {
                    error = reconnect_error.to_string();
                    delay = delay.saturating_mul(2).min(config.max_reconnect_delay);
                }
            }
        }
    }
}

fn subscription_message(message: redis::Msg, key_prefix: &str) -> RedisSubscriptionMessage {
    let channel = message
        .get_channel_name()
        .strip_prefix(key_prefix)
        .unwrap_or_else(|| message.get_channel_name())
        .to_owned();
    let pattern = message
        .get_pattern::<Option<String>>()
        .unwrap_or_default()
        .map(|pattern| {
            pattern
                .strip_prefix(key_prefix)
                .unwrap_or(&pattern)
                .to_owned()
        });
    RedisSubscriptionMessage {
        channel,
        pattern,
        payload: message.get_payload_bytes().to_vec(),
    }
}

fn enqueue_subscription_message(
    sender: &mpsc::Sender<RedisSubscriptionEvent>,
    message: RedisSubscriptionMessage,
    dropped: &mut u64,
) {
    if *dropped > 0 {
        if sender
            .try_send(RedisSubscriptionEvent::Lagged { dropped: *dropped })
            .is_ok()
        {
            *dropped = 0;
        } else {
            *dropped = dropped.saturating_add(1);
            return;
        }
    }
    if sender
        .try_send(RedisSubscriptionEvent::Message(message))
        .is_err()
    {
        *dropped = dropped.saturating_add(1);
    }
}

async fn send_subscription_event(
    sender: &mpsc::Sender<RedisSubscriptionEvent>,
    event: RedisSubscriptionEvent,
    shutdown: &mut oneshot::Receiver<()>,
) -> bool {
    tokio::select! {
        _ = shutdown => {
            let _ = sender.send(RedisSubscriptionEvent::Closed).await;
            false
        }
        result = sender.send(event) => result.is_ok(),
    }
}

fn duration_millis(duration: Duration) -> Result<u64, RedisStoreError> {
    duration
        .as_millis()
        .try_into()
        .map_err(|_| RedisStoreError::DurationOverflow)
}

fn append_scan_options(
    command: &mut Cmd,
    pattern: Option<&str>,
    count: Option<usize>,
) -> Result<(), RedisStoreError> {
    if count == Some(0) {
        return Err(RedisStoreError::InvalidArgument(
            "Redis scan count must be positive",
        ));
    }
    if let Some(pattern) = pattern {
        command.arg("MATCH").arg(pattern);
    }
    if let Some(count) = count {
        command.arg("COUNT").arg(count);
    }
    Ok(())
}

fn append_stream_read_options(
    command: &mut Cmd,
    count: Option<usize>,
    block: Option<Duration>,
    no_ack: bool,
) -> Result<(), RedisStoreError> {
    if count == Some(0) {
        return Err(RedisStoreError::InvalidArgument(
            "Redis stream read count must be positive",
        ));
    }
    if let Some(count) = count {
        command.arg("COUNT").arg(count);
    }
    if let Some(block) = block {
        command.arg("BLOCK").arg(duration_millis(block)?);
    }
    if no_ack {
        command.arg("NOACK");
    }
    Ok(())
}

fn append_streams(
    command: &mut Cmd,
    streams: &[(&str, &str)],
    key: impl Fn(&str) -> String,
) -> Result<(), RedisStoreError> {
    if streams.is_empty() {
        return Err(RedisStoreError::InvalidArgument(
            "Redis stream reads require at least one stream",
        ));
    }
    command.arg("STREAMS");
    for (stream, _) in streams {
        command.arg(key(stream));
    }
    for (_, id) in streams {
        command.arg(id);
    }
    Ok(())
}

fn expect_ok(response: String, operation: &str) -> Result<(), RedisStoreError> {
    if response == "OK" {
        Ok(())
    } else {
        Err(RedisStoreError::UnexpectedResponse(format!(
            "{operation} returned {response}"
        )))
    }
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

    #[tokio::test]
    async fn operation_hooks_emit_bounded_success_and_error_metrics() {
        let registry = Metrics::new();
        let metrics = RedisStoreMetrics::register(&registry).unwrap();
        let store = RedisStore::new(RedisStoreConfig::new("redis://127.0.0.1/"))
            .unwrap()
            .with_metrics(metrics.clone());

        store
            .instrument("get", async { Ok::<_, redis::RedisError>(()) })
            .await
            .unwrap();
        let error = store
            .instrument("get", async {
                Err::<(), _>(redis::RedisError::from((
                    redis::ErrorKind::TypeError,
                    "test failure",
                )))
            })
            .await
            .unwrap_err();
        assert!(matches!(error, RedisStoreError::Redis(_)));

        let mut known = redis::cmd("GET");
        known.arg("key");
        assert_eq!(redis_command_name(&known), "get");
        let mut unknown = redis::cmd("APPLICATION_PRIVATE_COMMAND");
        unknown.arg("key");
        assert_eq!(redis_command_name(&unknown), "other");

        let timed = RedisStore::new(
            RedisStoreConfig::new("redis://127.0.0.1/")
                .with_operation_timeout(Duration::from_millis(1)),
        )
        .unwrap()
        .with_metrics(metrics);
        assert!(matches!(
            timed
                .instrument("get", std::future::pending::<redis::RedisResult<()>>())
                .await,
            Err(RedisStoreError::Timeout)
        ));

        let rendered = registry.render();
        assert!(rendered
            .contains("rust_zero_redis_operations_total{operation=\"get\",outcome=\"success\"} 1"));
        assert!(rendered
            .contains("rust_zero_redis_operations_total{operation=\"get\",outcome=\"error\"} 1"));
        assert!(rendered
            .contains("rust_zero_redis_operations_total{operation=\"get\",outcome=\"timeout\"} 1"));
    }

    #[test]
    fn prefixes_keys_and_creates_unique_lock_tokens() {
        let store =
            RedisStore::new(RedisStoreConfig::new("redis://127.0.0.1/").with_key_prefix("test:"))
                .unwrap();
        assert_eq!(store.key("users"), "test:users");
        assert_eq!(store.prefixed_key("users"), "test:users");
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

    #[test]
    fn configures_model_cache_expiry_policy() {
        let config = RedisModelCacheConfig::new(Duration::from_secs(60))
            .with_not_found_ttl(Duration::from_secs(3))
            .with_expiry_jitter(Duration::from_secs(7))
            .with_cache_not_found(false);
        assert_eq!(config.ttl, Duration::from_secs(60));
        assert_eq!(config.not_found_ttl, Duration::from_secs(3));
        assert_eq!(config.expiry_jitter, Duration::from_secs(7));
        assert!(!config.cache_not_found);
    }

    #[tokio::test]
    async fn rejects_invalid_stream_operations_before_connecting() {
        let store = RedisStore::new(RedisStoreConfig::new("redis://127.0.0.1/")).unwrap();
        assert!(matches!(
            store.stream_add::<&[u8]>("events", None, &[]).await,
            Err(RedisStoreError::InvalidArgument(_))
        ));
        assert!(matches!(
            store.stream_read(&[], None, None).await,
            Err(RedisStoreError::InvalidArgument(_))
        ));
        assert!(matches!(
            store
                .stream_group_read("", "worker", &[("events", ">")], None, None, false)
                .await,
            Err(RedisStoreError::InvalidArgument(_))
        ));
        assert!(matches!(
            store
                .stream_claim("events", "workers", "worker", Duration::ZERO, &[])
                .await,
            Err(RedisStoreError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn rejects_invalid_typed_operations_before_connecting() {
        let store = RedisStore::new(RedisStoreConfig::new("redis://127.0.0.1/")).unwrap();
        assert!(matches!(
            store.scan_keys(0, None, Some(0)).await,
            Err(RedisStoreError::InvalidArgument(_))
        ));
        assert!(matches!(
            store
                .list_blocking_pop_front(&[], Duration::from_secs(1))
                .await,
            Err(RedisStoreError::InvalidArgument(_))
        ));
        assert!(matches!(
            store
                .bitmap_operation(RedisBitOperation::Not, "destination", &["one", "two"])
                .await,
            Err(RedisStoreError::InvalidArgument(_))
        ));
        assert!(matches!(
            store.hyperloglog_add::<&[u8]>("visitors", &[]).await,
            Err(RedisStoreError::InvalidArgument(_))
        ));
        assert_eq!(
            store
                .sorted_set_range_by_score_with_scores("scores", 0.0, 1.0, Some((0, 0)))
                .await
                .unwrap(),
            Vec::new()
        );
    }

    #[tokio::test]
    async fn rejects_invalid_subscription_configuration_before_connecting() {
        let store = RedisStore::new(RedisStoreConfig::new("redis://127.0.0.1/")).unwrap();
        assert!(matches!(
            store
                .subscribe(Vec::<String>::new(), RedisSubscriptionConfig::default())
                .await,
            Err(RedisStoreError::InvalidArgument(_))
        ));
        assert!(matches!(
            store
                .psubscribe(
                    ["events:*"],
                    RedisSubscriptionConfig::default().with_capacity(1),
                )
                .await,
            Err(RedisStoreError::InvalidArgument(_))
        ));
        assert!(matches!(
            store
                .subscribe(
                    ["events"],
                    RedisSubscriptionConfig::default()
                        .with_reconnect_delay(Duration::from_secs(2))
                        .with_max_reconnect_delay(Duration::from_secs(1)),
                )
                .await,
            Err(RedisStoreError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn bounded_subscription_delivery_reports_lag() {
        let (sender, mut receiver) = mpsc::channel(2);
        let message = |payload: u8| RedisSubscriptionMessage {
            channel: "events".to_owned(),
            pattern: None,
            payload: vec![payload],
        };
        let mut dropped = 0;
        enqueue_subscription_message(&sender, message(1), &mut dropped);
        enqueue_subscription_message(&sender, message(2), &mut dropped);
        enqueue_subscription_message(&sender, message(3), &mut dropped);
        assert_eq!(dropped, 1);
        assert!(matches!(
            receiver.recv().await,
            Some(RedisSubscriptionEvent::Message(message)) if message.payload == [1]
        ));
        assert!(matches!(
            receiver.recv().await,
            Some(RedisSubscriptionEvent::Message(message)) if message.payload == [2]
        ));
        enqueue_subscription_message(&sender, message(4), &mut dropped);
        assert_eq!(dropped, 0);
        assert!(matches!(
            receiver.recv().await,
            Some(RedisSubscriptionEvent::Lagged { dropped: 1 })
        ));
        assert!(matches!(
            receiver.recv().await,
            Some(RedisSubscriptionEvent::Message(message)) if message.payload == [4]
        ));
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
        let namespace = format!("rust-zero:{}:", std::process::id());
        let store =
            RedisStore::new(RedisStoreConfig::new(url.clone()).with_key_prefix(namespace.clone()))
                .unwrap();
        let peer_store =
            RedisStore::new(RedisStoreConfig::new(url).with_key_prefix(namespace)).unwrap();
        store.ping().await.unwrap();
        let mut channel_subscription = store
            .subscribe(["events:created"], RedisSubscriptionConfig::default())
            .await
            .unwrap();
        let mut pattern_subscription = store
            .psubscribe(["events:*"], RedisSubscriptionConfig::default())
            .await
            .unwrap();
        assert_eq!(store.publish("events:created", b"user-7").await.unwrap(), 2);
        let channel_event =
            tokio::time::timeout(Duration::from_secs(2), channel_subscription.recv())
                .await
                .unwrap()
                .unwrap();
        assert!(matches!(
            channel_event,
            RedisSubscriptionEvent::Message(RedisSubscriptionMessage {
                channel,
                pattern: None,
                payload,
            }) if channel == "events:created" && payload == b"user-7"
        ));
        let pattern_event =
            tokio::time::timeout(Duration::from_secs(2), pattern_subscription.recv())
                .await
                .unwrap()
                .unwrap();
        assert!(matches!(
            pattern_event,
            RedisSubscriptionEvent::Message(RedisSubscriptionMessage {
                channel,
                pattern: Some(pattern),
                payload,
            }) if channel == "events:created" && pattern == "events:*" && payload == b"user-7"
        ));
        channel_subscription.shutdown();
        pattern_subscription.shutdown();
        assert_eq!(
            channel_subscription.recv().await,
            Some(RedisSubscriptionEvent::Closed)
        );
        assert_eq!(
            pattern_subscription.recv().await,
            Some(RedisSubscriptionEvent::Closed)
        );
        store
            .delete(&[
                "user",
                "count",
                "lock",
                "string",
                "hash",
                "list",
                "list-moved",
                "set",
                "set-moved",
                "sorted",
                "stream",
                "pipeline",
                "transaction",
                "bitmap-a",
                "bitmap-b",
                "bitmap-destination",
                "hll-a",
                "hll-b",
                "hll-merged",
                "unlink-temp",
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
        let raw_count: i64 = store
            .do_command(
                redis::cmd("INCRBY")
                    .arg(store.prefixed_key("count"))
                    .arg(4)
                    .to_owned(),
            )
            .await
            .unwrap();
        assert_eq!(raw_count, 5);
        store.set("unlink-temp", "value", None).await.unwrap();
        assert_eq!(store.unlink(&["unlink-temp"]).await.unwrap(), 1);
        assert!(!store.exists("unlink-temp").await.unwrap());

        let mut pipeline = redis::pipe();
        pipeline
            .cmd("SET")
            .arg(store.prefixed_key("pipeline"))
            .arg("batched")
            .ignore()
            .cmd("GET")
            .arg(store.prefixed_key("pipeline"));
        let (pipeline_value,): (String,) = store.do_pipeline(&pipeline).await.unwrap();
        assert_eq!(pipeline_value, "batched");
        let no_arguments: [&[u8]; 0] = [];
        let scripted: String = store
            .eval(
                "return redis.call('GET', KEYS[1])",
                &["pipeline"],
                &no_arguments,
            )
            .await
            .unwrap();
        assert_eq!(scripted, "batched");

        store
            .stream_group_create("stream", "workers", "0", true)
            .await
            .unwrap();
        store
            .stream_group_set_id("stream", "workers", "$")
            .await
            .unwrap();
        let stream_id = store
            .stream_add("stream", None, &[("kind", "created"), ("id", "7")])
            .await
            .unwrap();
        let delivered = store
            .stream_group_read(
                "workers",
                "worker-1",
                &[("stream", ">")],
                Some(10),
                None,
                false,
            )
            .await
            .unwrap();
        assert_eq!(delivered.keys.len(), 1);
        assert_eq!(delivered.keys[0].ids[0].id, stream_id);
        assert!(!matches!(
            store.stream_pending("stream", "workers").await.unwrap(),
            Value::Nil
        ));
        let claimed = store
            .stream_claim(
                "stream",
                "workers",
                "worker-2",
                Duration::ZERO,
                &[&stream_id],
            )
            .await
            .unwrap();
        assert!(!matches!(claimed, Value::Nil));
        assert!(!matches!(
            store.stream_info("stream").await.unwrap(),
            Value::Nil
        ));
        assert!(!matches!(
            store.stream_group_info("stream").await.unwrap(),
            Value::Nil
        ));
        assert!(!matches!(
            store
                .stream_consumer_info("stream", "workers")
                .await
                .unwrap(),
            Value::Nil
        ));
        assert_eq!(
            store
                .stream_ack("stream", "workers", &[&stream_id])
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store.stream_delete("stream", &[&stream_id]).await.unwrap(),
            1
        );
        assert!(store
            .stream_group_destroy("stream", "workers")
            .await
            .unwrap());

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
        let hash_page = store
            .hash_scan("hash", 0, Some("n*"), Some(10))
            .await
            .unwrap();
        assert_eq!(hash_page.cursor, 0);
        assert_eq!(hash_page.items, vec![(b"name".to_vec(), b"Ada".to_vec())]);

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
        assert_eq!(
            store
                .list_move(
                    "list",
                    "list-moved",
                    RedisListSide::Left,
                    RedisListSide::Right,
                )
                .await
                .unwrap(),
            Some(b"two".to_vec())
        );
        store.list_push_back("list", &["blocking"]).await.unwrap();
        assert_eq!(
            store
                .list_blocking_pop_back(&["list"], Duration::from_millis(100))
                .await
                .unwrap(),
            Some(("list".to_owned(), b"blocking".to_vec()))
        );

        assert_eq!(store.set_add("set", &["one", "two"]).await.unwrap(), 2);
        assert!(store.set_contains("set", "two").await.unwrap());
        assert_eq!(store.set_len("set").await.unwrap(), 2);
        let set_page = store.set_scan("set", 0, None, Some(10)).await.unwrap();
        assert_eq!(set_page.cursor, 0);
        assert_eq!(set_page.items.len(), 2);
        assert!(store.set_move("set", "set-moved", "two").await.unwrap());

        assert!(store.sorted_set_add("sorted", 2.0, "two").await.unwrap());
        assert!(store.sorted_set_add("sorted", 1.0, "one").await.unwrap());
        assert_eq!(
            store
                .sorted_set_range_with_scores("sorted", 0, -1)
                .await
                .unwrap(),
            vec![(b"one".to_vec(), 1.0), (b"two".to_vec(), 2.0)]
        );
        assert_eq!(
            store
                .sorted_set_increment("sorted", 2.0, "one")
                .await
                .unwrap(),
            3.0
        );
        assert_eq!(
            store.sorted_set_rank("sorted", "one", true).await.unwrap(),
            Some(0)
        );
        assert_eq!(
            store
                .sorted_set_range_by_score_with_scores("sorted", 2.0, 3.0, Some((0, 10)))
                .await
                .unwrap(),
            vec![(b"two".to_vec(), 2.0), (b"one".to_vec(), 3.0)]
        );
        assert_eq!(
            store
                .sorted_set_reverse_range_by_score_with_scores("sorted", 2.0, 3.0, None)
                .await
                .unwrap(),
            vec![(b"one".to_vec(), 3.0), (b"two".to_vec(), 2.0)]
        );
        assert_eq!(
            store
                .sorted_set_reverse_range_with_scores("sorted", 0, -1)
                .await
                .unwrap(),
            vec![(b"one".to_vec(), 3.0), (b"two".to_vec(), 2.0)]
        );
        assert_eq!(
            store
                .sorted_set_scan("sorted", 0, None, Some(10))
                .await
                .unwrap()
                .items
                .len(),
            2
        );
        assert_eq!(
            store
                .sorted_set_remove_by_score("sorted", 3.0, 3.0)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .sorted_set_remove_by_rank("sorted", 0, 0)
                .await
                .unwrap(),
            1
        );

        assert!(!store.bitmap_set("bitmap-a", 3, true).await.unwrap());
        assert!(store.bitmap_get("bitmap-a", 3).await.unwrap());
        assert_eq!(store.bitmap_count("bitmap-a", None).await.unwrap(), 1);
        assert!(!store.bitmap_set("bitmap-b", 4, true).await.unwrap());
        assert_eq!(
            store
                .bitmap_operation(
                    RedisBitOperation::Or,
                    "bitmap-destination",
                    &["bitmap-a", "bitmap-b"],
                )
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .bitmap_count("bitmap-destination", None)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            store
                .bitmap_position("bitmap-destination", true, None)
                .await
                .unwrap(),
            Some(3)
        );

        assert!(store
            .hyperloglog_add("hll-a", &["one", "two"])
            .await
            .unwrap());
        assert!(store
            .hyperloglog_add("hll-b", &["two", "three"])
            .await
            .unwrap());
        store
            .hyperloglog_merge("hll-merged", &["hll-a", "hll-b"])
            .await
            .unwrap();
        assert_eq!(store.hyperloglog_count(&["hll-merged"]).await.unwrap(), 3);

        let transaction_key = store.prefixed_key("transaction");
        let (transaction_value,): (String,) = store
            .transaction(|pipeline| {
                pipeline
                    .cmd("SET")
                    .arg(&transaction_key)
                    .arg("committed")
                    .ignore()
                    .cmd("GET")
                    .arg(&transaction_key);
            })
            .await
            .unwrap();
        assert_eq!(transaction_value, "committed");

        let key_page = store.scan_keys(0, Some("*"), Some(1_000)).await.unwrap();
        assert!(key_page.items.iter().any(|key| key == b"bitmap-a"));

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

        let model_config = RedisModelCacheConfig::new(Duration::from_secs(10))
            .with_not_found_ttl(Duration::from_secs(2))
            .with_expiry_jitter(Duration::from_secs(1));
        let model_a = RedisModelCache::<User, String>::new(store.clone(), model_config);
        let model_b = RedisModelCache::<User, String>::new(peer_store, model_config);
        model_a
            .invalidate(&["model-user", "missing-model", "broken-model"])
            .await
            .unwrap();

        store
            .set("broken-model", b"not-json", Some(Duration::from_secs(10)))
            .await
            .unwrap();
        let malformed = model_a
            .get_or_fetch("broken-model", || async { Ok(None) })
            .await
            .unwrap_err();
        assert!(matches!(
            malformed,
            SingleFlightError::Operation(error)
                if matches!(error.as_ref(), RedisModelCacheError::Serialization(_))
        ));

        let first = model_a
            .get_or_fetch("model-user", || async {
                Ok(Some(User {
                    id: 10,
                    name: "Shared".to_owned(),
                }))
            })
            .await
            .unwrap();
        assert_eq!(
            first.as_ref().map(|user| user.name.as_str()),
            Some("Shared")
        );
        let shared = model_b
            .get_or_fetch("model-user", || async {
                Err("must use shared cache".to_owned())
            })
            .await
            .unwrap();
        assert_eq!(shared, first);

        assert_eq!(
            model_a
                .get_or_fetch("missing-model", || async { Ok(None) })
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            model_b
                .get_or_fetch("missing-model", || async {
                    Err("must use not-found sentinel".to_owned())
                })
                .await
                .unwrap(),
            None
        );

        assert_eq!(model_a.invalidate(&["model-user"]).await.unwrap(), 1);
        let refreshed = model_b
            .get_or_fetch("model-user", || async {
                Ok(Some(User {
                    id: 10,
                    name: "Refreshed".to_owned(),
                }))
            })
            .await
            .unwrap();
        assert_eq!(
            refreshed.as_ref().map(|user| user.name.as_str()),
            Some("Refreshed")
        );
        assert_eq!(model_a.stats().misses, 2);
        assert_eq!(model_b.stats().hits, 2);
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
        let user = User {
            id: 9,
            name: "Cluster".to_owned(),
        };
        store
            .set_json("{user:9}:json", &user, Some(Duration::from_secs(10)))
            .await
            .unwrap();
        assert_eq!(
            store.get_json::<User>("{user:9}:json").await.unwrap(),
            Some(user)
        );
        let mut lock = store.lock("{user:9}:lock", Duration::from_secs(5));
        assert!(lock.acquire().await.unwrap());
        assert!(lock.extend(Duration::from_secs(10)).await.unwrap());
        assert!(lock.release().await.unwrap());

        assert!(!store.bitmap_set("{typed}:bitmap-a", 1, true).await.unwrap());
        assert!(!store.bitmap_set("{typed}:bitmap-b", 2, true).await.unwrap());
        assert_eq!(
            store
                .bitmap_operation(
                    RedisBitOperation::Or,
                    "{typed}:bitmap-result",
                    &["{typed}:bitmap-a", "{typed}:bitmap-b"],
                )
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .bitmap_count("{typed}:bitmap-result", None)
                .await
                .unwrap(),
            2
        );
        assert!(store
            .hyperloglog_add("{typed}:hll", &["one", "two"])
            .await
            .unwrap());
        assert_eq!(store.hyperloglog_count(&["{typed}:hll"]).await.unwrap(), 2);
        store
            .sorted_set_add("{typed}:sorted", 1.0, "one")
            .await
            .unwrap();
        assert_eq!(
            store
                .sorted_set_range_by_score_with_scores("{typed}:sorted", 0.0, 2.0, None,)
                .await
                .unwrap(),
            vec![(b"one".to_vec(), 1.0)]
        );
        let transaction_key = store.prefixed_key("{typed}:transaction");
        let (value,): (String,) = store
            .transaction(|pipeline| {
                pipeline
                    .cmd("SET")
                    .arg(&transaction_key)
                    .arg("clustered")
                    .ignore()
                    .cmd("GET")
                    .arg(&transaction_key);
            })
            .await
            .unwrap();
        assert_eq!(value, "clustered");
        assert_eq!(
            store
                .delete(&[
                    "{one}:value",
                    "{two}:value",
                    "{user:9}:json",
                    "{user:9}:lock",
                ])
                .await
                .unwrap(),
            3
        );
        assert_eq!(
            store
                .unlink(&[
                    "{typed}:bitmap-a",
                    "{typed}:bitmap-b",
                    "{typed}:bitmap-result",
                    "{typed}:hll",
                    "{typed}:sorted",
                    "{typed}:transaction",
                ])
                .await
                .unwrap(),
            6
        );

        let model = RedisModelCache::<User, String>::new(
            store.clone(),
            RedisModelCacheConfig::new(Duration::from_secs(10)),
        );
        let key = "{user:11}:model";
        model.invalidate(&[key]).await.unwrap();
        let cached = model
            .get_or_fetch(key, || async {
                Ok(Some(User {
                    id: 11,
                    name: "Cluster model".to_owned(),
                }))
            })
            .await
            .unwrap();
        assert_eq!(cached.as_ref().map(|user| user.id), Some(11));
        assert_eq!(model.invalidate(&[key]).await.unwrap(), 1);
    }
}
