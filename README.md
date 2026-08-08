# rust-zero

A Rust web and RPC framework inspired by [go-zero](https://github.com/zeromicro/go-zero).

See [FEATURE_PARITY.md](FEATURE_PARITY.md) for runtime coverage against go-zero v1.10.3 and
[BACKLOG.md](BACKLOG.md) for the prioritized remaining work.

## Available features

- Actix Web middleware for structured request logging, request identity propagation, CORS, bearer
  authentication, panic recovery, browser security headers, timeout control, overload shedding,
  and token-bucket rate limiting.
- A validated, deserializable REST server configuration that binds Actix and installs the standard
  logging, recovery, identity, tracing, metrics, security, timeout, request-size, and shedding stack.
- Per-server REST response policies with request-aware success/error envelopes, typed stable
  application errors, gRPC-to-HTTP status translation, safe serialization failure handling, and
  anti-buffered chunk streaming.
- Declarative REST route groups with inherited and per-route JWT, timeout, body-size, priority, and
  SSE policies, plus signal-driven serving that gracefully drains in-flight requests.
- Tonic-based gRPC client and server builders with deadline, connection concurrency, and stream
  limits plus gRPC health reporting, bearer-auth interceptors, and backend-neutral dynamic endpoint
  balancing for in-memory, etcd, or Kubernetes discovery, protocol-aware client circuit breaking,
  adaptive server load shedding, and reusable
  client/server layers for cardinality-bounded request, latency, in-flight, final-status, error,
  and cancellation metrics across unary and streaming calls.
- A generated-service-independent gRPC server layer that installs bearer authentication, W3C
  trace extraction, adaptive shedding, panic-to-`Internal` recovery, and transport metrics once
  for every generated service on a Tonic server.
- A generated-client-friendly gRPC service stack that installs bearer credentials, W3C trace
  propagation, default deadlines, transport metrics, and trailer-aware circuit breaking around a
  direct or discovery-balanced channel.
- Descriptor-driven HTTP-to-gRPC transcoding with compiled descriptor sets or live gRPC server
  reflection, explicit and `google.api.http` bindings, protobuf JSON, metadata forwarding,
  canonical status mapping, and newline-delimited server streaming.
- Optional OpenTelemetry tracing with parent-based sampling, full REST and gRPC client/server
  spans, and batched OTLP export over gRPC or HTTP.
- A Tokio-based MapReduce primitive with bounded parallelism.
- Framework-neutral runtime primitives in `rust-zero-core`: typed JSON/TOML/YAML configuration
  loading with environment expansion, atomic dynamic configuration subscriptions, sync/async
  circuit breaking, adaptive concurrency shedding, consistent hashing, feedback-aware P2C
  balancing, TTL caching, bounded exponential retry, async deadlines, keyed single-flight work
  coalescing, and typed validation. It also provides a dependency-free Prometheus text-format
  metrics registry with labeled counters, gauges, and histograms, a reference-counted service
  registry for dynamic endpoint publication and subscriptions, Bloom filters, rolling statistics,
  timed batch execution, fail-fast service groups with graceful shutdown, and a standalone
  structured logger with trace context, sensitive-field masking, sampling, and file rotation.
- Signal-aware service supervision that turns SIGINT/SIGTERM into cooperative cancellation and
  enforces a bounded graceful-shutdown window across all background and transport services.
- Feature-gated etcd coordination with typed last-known-good configuration watches, renewable
  service leases, and revision-safe endpoint subscriptions.
- Feature-gated Kubernetes EndpointSlice discovery with readiness filtering, atomic relists,
  resource-version recovery, stable snapshots, and IPv6-safe endpoint URIs.
- A resilient named REST client with request deadlines, circuit breaking, response-size limits,
  JSON helpers, W3C trace propagation, and optional request/duration/in-flight metrics, plus
  validated JSON, query, path, and form extractors for
  inbound APIs, including application-typed headers, combined path/query/header/JSON parsing, and
  stable machine-readable extraction errors. Multipart forms stream uploads to automatically
  cleaned temporary files with independently configurable text-field, file, and aggregate limits.
- EventSource-compatible server-sent event responses with multiline events, event IDs, retry hints,
  heartbeat comments, and proxy anti-buffering headers.
- Non-overlapping periodic background execution with surfaced job failures and a bounded shutdown
  deadline, alongside count- and byte-batched execution, coalesced delayed work, and
  threshold-based execution suppression.
- Supervised in-process queues with configurable worker pools, pause/resume, bounded shutdown,
  lifecycle and failure events, Prometheus processing metrics, round-robin failover pushing, and
  fan-out delivery.
- An opt-in named duration profiler and an internal Actix dev server exposing route discovery,
  health, Prometheus metrics, profiling reports, and process/runtime diagnostics.
- Feature-gated external stores: an async standalone/clustered Redis adapter with strings, hashes,
  lists, sets, sorted sets, JSON values, TTLs, counters, ownership-safe distributed locks, and
  consumer-group cursor updates, plus coalesced cache-aside reads; and typed SQLx pools for SQLite,
  PostgreSQL, and MySQL with
  standardized lifecycle, health checks, transactions, and bounded cache-aside record loading
  with configurable negative caching, bounded TTL jitter, statistics, and race-safe mutation
  invalidation; plus typed MongoDB collections, health checks, sessions, transactions, and the
  same configurable cached-record expiry policy.

Configuration files may use JSON, JSON5, TOML, YAML, or YML. JSON5 supports comments, trailing
commas, single-quoted strings, and unquoted object keys while retaining environment expansion.

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

Dynamic configuration updates are parsed before publication, so consumers always observe a
complete, last-known-good typed value:

```rust
use rust_zero_core::{ConfigFormat, DynamicConfig};

let limits = DynamicConfig::<std::collections::HashMap<String, u64>>::new(
    "requests = 100",
    ConfigFormat::Toml,
)?;
let mut changes = limits.subscribe();
limits.update("requests = 200")?;
changes.changed().await?;
assert_eq!(changes.borrow().value()["requests"], 200);
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

Unary gRPC calls can use transport-aware resilience without counting application statuses such as
`InvalidArgument` or `NotFound` as dependency failures:

```rust
use rpc::{RpcCircuitBreaker, RpcLoadShedder};
use rust_zero_core::{CircuitBreakerConfig, LoadShedderConfig};
use std::time::Duration;

let breaker = RpcCircuitBreaker::new(CircuitBreakerConfig::new(5, Duration::from_secs(30)));
let response = breaker.call(|| client.echo(request)).await?;

let shedder = RpcLoadShedder::new(LoadShedderConfig::new(
    1_024,
    Duration::from_millis(100),
));
let response = shedder.call(|| service.echo(request)).await?;
# Ok::<(), tonic::Status>(())
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
counts, latency histograms, and in-flight gauges labeled by method, route, and response status.
The standard REST server also counts timeout, concurrency-shed, and rate-limit rejections. Attach a
shared `HttpClientMetrics` instance with `HttpClient::with_metrics` to record bounded service,
method, status/error, latency, and in-flight client metrics without labeling raw URLs.

For the standard production stack, load `RestServerConfig` and provide only application routes:

```rust
use actix_web::{web, HttpResponse};
use rest::{RestServer, RestServerConfig};
use rust_zero_core::load_config;

let config: RestServerConfig = load_config("etc/users.toml")?;
let server = RestServer::new(config)?.run(|routes| {
    routes.route("/healthz", web::get().to(HttpResponse::Ok));
})?;
server.await?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Install a response policy on the standard server and extract it as Actix application data from
handlers. Mappers receive the current request, so envelopes can include request identity without
global mutable hooks:

```rust
use actix_web::{web, HttpRequest};
use rest::{ApiError, ResponsePolicy, RestServer};
use serde_json::json;

let policy = ResponsePolicy::new().with_success_mapper(|request, data| {
    json!({
        "request_id": request.headers().get("x-request-id").and_then(|v| v.to_str().ok()),
        "data": data,
    })
});
let server = RestServer::new(Default::default())?.with_response_policy(policy);

async fn handler(request: HttpRequest, policy: web::Data<ResponsePolicy>) -> actix_web::HttpResponse {
    policy.respond(&request, Ok::<_, ApiError>(json!({"ready": true})))
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

Route policies are matched by HTTP method and the registered Actix route pattern. Group settings
are inherited, while a route can override them or set `public = true` to opt out of group JWT:

```toml
[[route_groups]]
prefix = "/api"
timeout_ms = 2000
max_body_bytes = 1048576

[route_groups.jwt]
secret = "${API_JWT_SECRET}"
leeway_seconds = 30

[[route_groups.routes]]
method = "GET"
path = "/users/{id}"

[[route_groups.routes]]
method = "GET"
path = "/events"
public = true
sse = true
priority = true
```

Use `serve_until` (or `serve_on_until` with a pre-bound listener) to connect an application signal
future to graceful Actix draining.

REST and RPC duration fields in serialized transport configs use millisecond-suffixed names such
as `request_timeout_ms`, `shutdown_timeout_ms`, and `connect_timeout_ms`.

## Internal diagnostics

`DevServer` provides the internal observability listener corresponding to go-zero's dev server.
It defaults to port `6060` and publishes `/healthz`, `/metrics`, `/debug/profile`,
`/debug/runtime`, and a route index at `/`. Profiling is opt-in at the core primitive and enabled
automatically by a dev server configured with profiling support.

```rust
use rest::{DevServer, DevServerConfig};
use rust_zero_core::{Metrics, Profiler};
use std::sync::Arc;

let diagnostics = DevServer::new(
    DevServerConfig::default(),
    Arc::new(Metrics::new()),
    Arc::new(Profiler::new()),
);

// Spawn or await the returned Actix server alongside the application service.
let server = diagnostics.run()?;
actix_web::rt::spawn(server);
# Ok::<(), std::io::Error>(())
```

Long-running services can be supervised as one fail-fast group. `wait_for_signal` listens for
SIGINT/SIGTERM, broadcasts cancellation, and waits up to the configured deadline for every task.

```rust
use rust_zero_core::ServiceGroup;
use std::{io, time::Duration};

let mut services = ServiceGroup::new().with_shutdown_timeout(Duration::from_secs(30));
services.add("worker", |mut shutdown| async move {
    shutdown.requested().await;
    Ok::<_, io::Error>(())
});
services.start()?.wait_for_signal().await?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Etcd configuration and discovery

Enable the `etcd` feature on `rust-zero-core`. Configuration watches start from the revision of
their initial read, so no update is lost between loading and subscribing. Published endpoints are
automatically withdrawn when their renewable lease is revoked or expires.

```rust
use rust_zero_core::{EtcdClient, EtcdConfig};
use std::time::Duration;

let etcd = EtcdClient::connect(EtcdConfig::new(["http://127.0.0.1:2379"])).await?;
let mut users = etcd.subscribe("users").await?;
let lease = etcd
    .publish(
        "users",
        "users-1",
        "http://127.0.0.1:8080",
        Duration::from_secs(10),
    )
    .await?;

let endpoints = users.changed().await?;
lease.revoke().await?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Kubernetes discovery

Enable the `kubernetes` feature on `rust-zero-core`. The adapter uses the local kubeconfig or an
in-cluster service account and needs `list` and `watch` RBAC permissions for
`discovery.k8s.io/v1` EndpointSlices. It atomically replaces snapshots after relists and excludes
unready or terminating endpoints.

```rust
use rust_zero_core::{KubernetesDiscovery, KubernetesDiscoveryConfig};

let discovery = KubernetesDiscovery::infer(
    KubernetesDiscoveryConfig::new("production")
        .with_port_name("grpc")
        .with_scheme("http"),
)
.await?;
let mut users = discovery.subscribe("users").await?;

for endpoint in users.endpoints() {
    println!("ready user service: {endpoint}");
}
let changed_endpoints = users.changed().await?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## External stores

Enable `stores-redis`, `stores-sql`, `stores-mongo`, or the combined `stores` feature on
`rust-zero-core`.
Applications retain direct access to SQLx's typed pools and compile-time checked queries.

```rust
use rust_zero_core::{
    CachedSqlStore, MongoCacheConfig, MongoStore, MongoStoreConfig, RedisJsonCache, RedisStore,
    RedisStoreConfig, SqlCacheConfig, SqlStoreConfig, SqliteStore,
};
use std::time::Duration;

let redis = RedisStore::new(RedisStoreConfig::new("redis://127.0.0.1/"))?;
redis
    .set_json("user:42", &serde_json::json!({"name": "Ada"}), Some(Duration::from_secs(60)))
    .await?;

// Seed addresses may point at any reachable nodes in the same Redis Cluster.
let cluster = RedisStore::new(
    RedisStoreConfig::cluster([
        "redis://redis-0:6379/",
        "redis://redis-1:6379/",
        "redis://redis-2:6379/",
    ])
    .with_key_prefix("users:"),
)?;
cluster.hash_set("{42}:profile", "name", "Ada").await?;

let users = RedisJsonCache::<serde_json::Value, std::io::Error>::new(
    redis,
    Duration::from_secs(60),
);
let user = users
    .get_or_fetch("user:43", || async {
        Ok(serde_json::json!({"name": "Grace"}))
    })
    .await?;

let sql = SqliteStore::connect_sqlite(SqlStoreConfig::new("sqlite://service.db")).await?;
let mut transaction = sql.begin().await?;

let users = CachedSqlStore::<sqlx::Sqlite, i64, String, sqlx::Error>::new(
    sql,
    SqlCacheConfig::new(10_000, Duration::from_secs(60)),
);
let name = users
    .find(42, |pool| async move {
        sqlx::query_scalar("SELECT name FROM users WHERE id = ?")
            .bind(42_i64)
            .fetch_optional(&pool)
            .await
    })
    .await?;

let mongo = MongoStore::connect(MongoStoreConfig::new(
    "mongodb://127.0.0.1:27017",
    "service",
))
.await?;
let profiles = mongo.cached_collection::<i64, mongodb::bson::Document>(
    "profiles",
    MongoCacheConfig::new(10_000, Duration::from_secs(60)),
);
let profile = profiles
    .find(42, |collection| async move {
        collection
            .find_one(mongodb::bson::doc! { "user_id": 42_i64 })
            .await
    })
    .await?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

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
