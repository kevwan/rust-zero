# rust-zero

A Rust web and RPC framework inspired by [go-zero](https://github.com/zeromicro/go-zero).

See [FEATURE_PARITY.md](FEATURE_PARITY.md) for runtime coverage against go-zero v1.10.2.

## Available features

- Actix Web middleware for structured request logging, request identity propagation, CORS, bearer
  authentication, panic recovery, browser security headers, timeout control, overload shedding,
  and token-bucket rate limiting.
- Tonic-based gRPC client and server builders with deadline, connection concurrency, and stream
  limits plus gRPC health reporting, bearer-auth interceptors, and registry-backed dynamic endpoint
  balancing.
- Optional OpenTelemetry tracing with parent-based sampling, full REST and gRPC client/server
  spans, and batched OTLP export over gRPC or HTTP.
- A Tokio-based MapReduce primitive with bounded parallelism.
- Framework-neutral runtime primitives in `rust-zero-core`: typed JSON/TOML/YAML configuration
  loading with environment expansion, circuit breaking, adaptive concurrency shedding, consistent
  hashing, TTL caching, bounded exponential retry, async deadlines, and keyed single-flight work
  coalescing. It also provides a dependency-free Prometheus text-format metrics registry with
  labeled counters, gauges, and histograms, a reference-counted service registry for dynamic
  endpoint publication and subscriptions, Bloom filters, rolling statistics, timed batch
  execution, fail-fast service groups with graceful shutdown, and a standalone structured logger
  with trace context, sensitive-field masking, sampling, and file rotation.

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

## OpenTelemetry

Enable the `telemetry` feature on `rust-zero-core` and whichever transport crates need
instrumentation. OTLP exporters are opt-in so applications that only need local W3C propagation
do not pay their dependency or runtime cost.

```rust
use rust_zero_core::{OtlpTransport, Telemetry, TelemetryConfig};

let telemetry = Telemetry::init(
    TelemetryConfig::new(
        "users-api",
        "http://otel-collector:4317",
        OtlpTransport::Grpc,
    )
    .with_sample_ratio(0.1),
)?;

// Keep `telemetry` alive for the service lifetime. Dropping it flushes and
// shuts down the exporter.
```

For REST, use `OpenTelemetryTracing` in place of `TraceContextMiddleware`. It records method,
path, host, status, errors, and exposes the matching `TraceContext` through request extensions:

```rust
use rest::OpenTelemetryTracing;

// App::new().wrap(OpenTelemetryTracing::new())
```

For gRPC, apply the server layer to the Tonic builder and wrap client channels:

```rust
use rpc::RpcTelemetryLayer;

let server = tonic::transport::Server::builder()
    .layer(RpcTelemetryLayer::server());
let traced_channel = RpcTelemetryLayer::client().wrap(channel);
```

The REST middleware is composable and returns standard HTTP responses when protection activates:

| Middleware | Response |
| --- | --- |
| `Timeout` | `504 Gateway Timeout` |
| `ConcurrencyLimit` | `503 Service Unavailable` |
| `RateLimit` | `429 Too Many Requests` with `Retry-After` |

```rust
use actix_web::{App, HttpServer};
use rest::{
    BearerAuth, ConcurrencyLimit, Cors, LoggingMiddleware, RateLimit, Recover, RequestId,
    SecurityHeaders, Timeout,
};
use std::time::Duration;

let concurrency_limit = ConcurrencyLimit::new(1_024);
let rate_limit = RateLimit::new(1_000, 2_000);

HttpServer::new(move || {
    App::new()
        .wrap(LoggingMiddleware)
        .wrap(RequestId::new())
        .wrap(Recover::new())
        .wrap(SecurityHeaders::new())
        .wrap(BearerAuth::new(|token| (token == "secret").then_some(())))
        .wrap(Cors::permissive())
        .wrap(Timeout::new(Duration::from_secs(10)))
        .wrap(concurrency_limit.clone())
        .wrap(rate_limit.clone())
})
```

Register `HttpMetrics` in a shared `Metrics` registry to emit Prometheus-compatible request
counts and latency histograms labeled by method, route, and response status.

The standalone logger can write JSON or plain records to the console or to daily/size-rotated
files. `StructuredLogging` connects it to Actix requests and includes request and W3C trace
identifiers when the corresponding middleware runs outside it:

```rust
use rust_zero_core::{LogConfig, Logger, RotationPolicy};
use rest::{RequestId, StructuredLogging, TraceContextMiddleware};

let logger = Logger::new(LogConfig::file(
    "users-api",
    "logs",
    RotationPolicy::Size {
        max_bytes: 100 * 1024 * 1024,
        max_backups: 10,
    },
))?;

// App::new()
//     .wrap(StructuredLogging::new(logger))
//     .wrap(TraceContextMiddleware::new())
//     .wrap(RequestId::new())
```

## RPC

`rpc` provides common transport controls while leaving protobuf service implementations as normal
Tonic services. Static clients use `connect`; registry-backed clients use `connect_service` and
automatically follow endpoint publication and withdrawal events. The crate includes a generated
`rust_zero.echo` service and runnable server:

```bash
cargo run -p rpc --example echo_server
```

```rust
use rpc::{BearerToken, RpcClient, RpcClientConfig, RpcServer, RpcServerConfig};
use rust_zero_core::ServiceRegistry;
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

let registry = ServiceRegistry::new();
let _lease = registry.publish("users", "http://127.0.0.1:50051")?;
let balanced_channel = RpcClient::new(RpcClientConfig::new("http://unused"))
    .connect_service(&registry, "users")?;
let client_auth = BearerToken::new("service-token")?;
```

## Service lifecycle and background batching

`ServiceGroup` supervises long-running tasks. An unexpected exit stops sibling services, while an
explicit shutdown waits for every task up to the configured deadline. `BatchExecutor` flushes
background work when either a size or time threshold is reached.

```rust
use rust_zero_core::{BatchExecutor, ServiceGroup};
use std::{io, time::Duration};

let mut services = ServiceGroup::new().with_shutdown_timeout(Duration::from_secs(10));
services.add("worker", |mut shutdown| async move {
    shutdown.requested().await;
    Ok::<_, io::Error>(())
});
let running = services.start()?;
let shutdown = running.shutdown_handle();
shutdown.shutdown();
running.wait().await?;

let batches = BatchExecutor::new(100, Duration::from_secs(1), |items: Vec<String>| async move {
    // persist items
});
batches.push("event".to_owned()).await?;
batches.shutdown().await?;
```

## Gateway routing

`gateway` routes a request to the most-specific configured path prefix and balances calls across
that route's upstreams:

```rust
use gateway::{GatewayRoute, GatewayRouter};

let gateway = GatewayRouter::new([
    GatewayRoute::new("/", vec!["http://frontend:8080".to_owned()])?,
    GatewayRoute::new("/api", vec![
        "http://users-a:8080".to_owned(),
        "http://users-b:8080".to_owned(),
    ])?,
])?;

assert!(gateway.select("/api/users").is_some());
```
