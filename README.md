# rust-zero

A Rust web and RPC framework inspired by [go-zero](https://github.com/zeromicro/go-zero).

## Available features

- Actix Web middleware for structured request logging, request identity propagation, CORS, timeout
  control, overload shedding, and token-bucket rate limiting.
- Tonic-based gRPC client and server builders with deadline, connection concurrency, and stream
  limits plus gRPC health reporting.
- A Tokio-based MapReduce primitive with bounded parallelism.
- Framework-neutral runtime primitives in `rust-zero-core`: typed JSON/TOML/YAML configuration
  loading with environment expansion, circuit breaking, adaptive concurrency shedding, consistent
  hashing, bounded exponential retry, and async deadlines.

## Core runtime

`rust-zero-core` supplies the production controls that are shared by transport services and
background workers:

```rust
use rust_zero_core::{
    retry, AdaptiveShedder, CircuitBreaker, CircuitBreakerConfig, ConsistentHash,
    LoadShedderConfig, RetryPolicy,
};
use std::time::Duration;

let breaker = CircuitBreaker::new(CircuitBreakerConfig::new(5, Duration::from_secs(30)));
let shedder = AdaptiveShedder::new(LoadShedderConfig::new(128, Duration::from_millis(100)));
let mut backends = ConsistentHash::new(100);
backends.add("http://users-a:8080");

let _permit = shedder.try_acquire();
let _response = breaker.execute(|| {
    // call a selected backend
    Ok::<_, std::io::Error>(())
})?;

retry(RetryPolicy::new(3, Duration::from_millis(50)), || async {
    Ok::<_, std::io::Error>(())
})
.await?;
```

Configuration is deserialized directly into service types, selected by file extension, and
expands `$VAR` or `${VAR}` values from the process environment:

```rust
use rust_zero_core::{load_config, ServiceConfig};

let config: ServiceConfig = load_config("etc/users.toml")?;
assert_eq!(config.address(), "0.0.0.0:8080");
```

The REST middleware is composable and returns standard HTTP responses when protection activates:

| Middleware | Response |
| --- | --- |
| `Timeout` | `504 Gateway Timeout` |
| `ConcurrencyLimit` | `503 Service Unavailable` |
| `RateLimit` | `429 Too Many Requests` with `Retry-After` |

```rust
use actix_web::{App, HttpServer};
use rest::{ConcurrencyLimit, Cors, LoggingMiddleware, RateLimit, RequestId, Timeout};
use std::time::Duration;

let concurrency_limit = ConcurrencyLimit::new(1_024);
let rate_limit = RateLimit::new(1_000, 2_000);

HttpServer::new(move || {
    App::new()
        .wrap(LoggingMiddleware)
        .wrap(RequestId::new())
        .wrap(Cors::permissive())
        .wrap(Timeout::new(Duration::from_secs(10)))
        .wrap(concurrency_limit.clone())
        .wrap(rate_limit.clone())
})
```

## RPC

`rpc` provides common transport controls while leaving protobuf service implementations as normal
Tonic services. The crate includes a generated `rust_zero.echo` service and runnable server:

```bash
cargo run -p rpc --example echo_server
```

```rust
use rpc::{RpcClient, RpcClientConfig, RpcServer, RpcServerConfig};
use std::time::Duration;

let server = RpcServer::new(
    RpcServerConfig::new("127.0.0.1:50051".parse()?)
        .with_request_timeout(Duration::from_secs(10))
        .with_concurrency_limit(1_024),
);

let channel = RpcClient::new(
    RpcClientConfig::new("http://127.0.0.1:50051")
        .with_connect_timeout(Duration::from_secs(3))
        .with_request_timeout(Duration::from_secs(10)),
)
.connect()
.await?;
```
