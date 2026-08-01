//! Framework-neutral runtime primitives shared by rust-zero services.
//!
//! The crate provides configuration loading, circuit breaking, adaptive load shedding,
//! consistent hashing, and async retry helpers.  They intentionally do not depend on a
//! particular HTTP or gRPC framework so REST, RPC, and background services can share them.

pub mod balancer;
pub mod bloom;
pub mod breaker;
pub mod cache;
pub mod config;
pub mod config_center;
pub mod discov;
pub mod executor;
pub mod fx;
pub mod hash;
pub mod limit;
pub mod load;
pub mod log;
pub mod metric;
pub mod profile;
pub mod pubsub;
pub mod queue;
pub mod rolling;
pub mod service;
pub mod singleflight;
#[cfg(feature = "stores-redis")]
pub mod stores_redis;
#[cfg(feature = "stores-sql")]
pub mod stores_sql;
#[cfg(feature = "telemetry")]
pub mod telemetry;
pub mod trace;
pub mod validation;

pub use balancer::{BalancerError, NodeSnapshot, P2cBalancer, P2cRequest};
pub use bloom::{BloomError, BloomFilter};
pub use breaker::{BreakerState, CircuitBreaker, CircuitBreakerConfig, CircuitBreakerError};
pub use cache::{CacheStats, MemoryCache, ReadThroughCache, TtlCache};
pub use config::{load_config, parse_config, ConfigError, ConfigFormat, ServiceConfig};
pub use config_center::{ConfigCenterError, ConfigSnapshot, DynamicConfig};
pub use discov::{
    DiscoveryError, ServiceEvent, ServiceLease, ServiceRegistry, ServiceSubscription,
};
pub use executor::{BatchExecutor, BatchExecutorError};
pub use fx::{retry, timeout, RetryPolicy};
pub use hash::ConsistentHash;
pub use limit::{LimitDecision, PeriodLimiter, TokenLimiter};
pub use load::{AdaptiveShedder, LoadShedderConfig, ShedPermit};
pub use log::{
    LogConfig, LogContext, LogEncoding, LogError, LogField, LogLevel, LogSampler, LogTarget,
    Logger, RotationPolicy, Sensitive,
};
pub use metric::{
    CounterVec, GaugeVec, HistogramOptions, HistogramVec, Metrics, MetricsError, VectorOptions,
};
pub use profile::{ProfilePoint, ProfileSnapshot, Profiler};
pub use pubsub::{Broker, Subscription};
pub use queue::{QueueReceiver, QueueSender};
pub use rolling::{RollingSnapshot, RollingWindow};
pub use service::{RunningServices, ServiceGroup, ServiceGroupError, Shutdown, ShutdownHandle};
pub use singleflight::{SingleFlight, SingleFlightError};
#[cfg(feature = "stores-redis")]
pub use stores_redis::{
    RedisCacheError, RedisJsonCache, RedisLock, RedisStore, RedisStoreConfig, RedisStoreError,
    RedisTtl,
};
#[cfg(feature = "stores-sql")]
pub use stores_sql::{MySqlStore, PostgresStore, SqlStore, SqlStoreConfig, SqliteStore};
#[cfg(feature = "telemetry")]
pub use telemetry::{
    OtlpTransport, Telemetry, TelemetryConfig, TelemetryError, TelemetrySpan, TelemetrySpanKind,
};
pub use trace::{TraceContext, TraceContextError, TraceFlags};
pub use validation::{Validate, Validation, ValidationErrors, Violation};
