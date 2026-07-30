//! Framework-neutral runtime primitives shared by rust-zero services.
//!
//! The crate provides configuration loading, circuit breaking, adaptive load shedding,
//! consistent hashing, and async retry helpers.  They intentionally do not depend on a
//! particular HTTP or gRPC framework so REST, RPC, and background services can share them.

pub mod breaker;
pub mod cache;
pub mod config;
pub mod fx;
pub mod hash;
pub mod load;
pub mod queue;

pub use breaker::{BreakerState, CircuitBreaker, CircuitBreakerConfig, CircuitBreakerError};
pub use cache::TtlCache;
pub use config::{load_config, parse_config, ConfigError, ConfigFormat, ServiceConfig};
pub use fx::{retry, timeout, RetryPolicy};
pub use hash::ConsistentHash;
pub use load::{AdaptiveShedder, LoadShedderConfig, ShedPermit};
pub use queue::{QueueReceiver, QueueSender};
