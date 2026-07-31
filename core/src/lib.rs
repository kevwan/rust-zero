//! Framework-neutral runtime primitives shared by rust-zero services.
//!
//! The crate provides configuration loading, circuit breaking, adaptive load shedding,
//! consistent hashing, and async retry helpers.  They intentionally do not depend on a
//! particular HTTP or gRPC framework so REST, RPC, and background services can share them.

pub mod bloom;
pub mod breaker;
pub mod cache;
pub mod config;
pub mod discov;
pub mod executor;
pub mod fx;
pub mod hash;
pub mod load;
pub mod metric;
pub mod queue;
pub mod rolling;
pub mod service;
pub mod singleflight;

pub use bloom::{BloomError, BloomFilter};
pub use breaker::{BreakerState, CircuitBreaker, CircuitBreakerConfig, CircuitBreakerError};
pub use cache::TtlCache;
pub use config::{load_config, parse_config, ConfigError, ConfigFormat, ServiceConfig};
pub use discov::{
    DiscoveryError, ServiceEvent, ServiceLease, ServiceRegistry, ServiceSubscription,
};
pub use executor::{BatchExecutor, BatchExecutorError};
pub use fx::{retry, timeout, RetryPolicy};
pub use hash::ConsistentHash;
pub use load::{AdaptiveShedder, LoadShedderConfig, ShedPermit};
pub use metric::{
    CounterVec, GaugeVec, HistogramOptions, HistogramVec, Metrics, MetricsError, VectorOptions,
};
pub use queue::{QueueReceiver, QueueSender};
pub use rolling::{RollingSnapshot, RollingWindow};
pub use service::{RunningServices, ServiceGroup, ServiceGroupError, Shutdown, ShutdownHandle};
pub use singleflight::{SingleFlight, SingleFlightError};
