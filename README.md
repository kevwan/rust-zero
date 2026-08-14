# rust-zero

[![CI](https://github.com/kevwan/rust-zero/actions/workflows/rust.yml/badge.svg)](https://github.com/kevwan/rust-zero/actions/workflows/rust.yml)
[![Crates.io](https://img.shields.io/crates/v/rust-zero-core.svg)](https://crates.io/crates/rust-zero-core)
[![Documentation](https://docs.rs/rust-zero-core/badge.svg)](https://docs.rs/rust-zero-core)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Production building blocks for Rust services, from HTTP and gRPC to discovery, resilience,
observability, gateways, and MCP. Inspired by [go-zero](https://github.com/zeromicro/go-zero),
rust-zero combines an opinionated production stack with modular crates, so applications only pay
for the transports and integrations they use.

> [!IMPORTANT]
> rust-zero is currently a `0.1.0-alpha` prerelease. It is suitable for evaluation and early
> adopters, but minor releases may change APIs before 1.0. See the [compatibility policy](#compatibility)
> before adopting it in production.

## Why rust-zero?

- **Production defaults:** timeouts, recovery, request IDs, metrics, tracing, load shedding, and
  graceful shutdown are built into the standard server stacks.
- **One toolkit, multiple transports:** build REST, gRPC, HTTP-to-gRPC gateways, and MCP services
  with shared configuration, authentication, discovery, and lifecycle primitives.
- **Use only what you need:** external stores, etcd, Kubernetes, telemetry, and profiling are
  feature-gated; the core runtime has no default external-service adapters.
- **Operationally testable:** CI covers the Rust 1.89 MSRV, feature combinations, documentation,
  publishable packages, benchmark artifacts, and real external backends.

## Quick start

You need [Rust 1.89 or newer](https://www.rust-lang.org/tools/install). Clone the repository and
run the self-contained REST demo—no database or external service is required:

```bash
git clone https://github.com/kevwan/rust-zero.git
cd rust-zero
cargo run -p rust-zero-demo
```

In another terminal:

```bash
curl http://127.0.0.1:8080/
# Hello world!
```

The demo is a small Actix Web service with structured request logging, a 10-second timeout,
concurrency control, and token-bucket rate limiting. Its complete source is in
[`demo/src/main.rs`](demo/src/main.rs).

To use rust-zero in an existing project, add only the crates you need:

```bash
cargo add rust-zero-core rust-zero-rest
```

Package names use the `rust-zero-` prefix. Some library names are intentionally shorter, so the
corresponding imports are:

```rust
use rust_zero_core::CircuitBreaker;
use rest::RestServerConfig;
```

## Choose a crate

All public crates are released at the same version.

| Crate | Use it for |
| --- | --- |
| [`rust-zero-core`](https://docs.rs/rust-zero-core) | Configuration, resilience, discovery, metrics, logging, caching, stores, and service lifecycle |
| [`rust-zero-rest`](https://docs.rs/rust-zero-rest) | Actix Web servers, clients, middleware, extractors, SSE, static files, and serverless handlers |
| [`rust-zero-rpc`](https://docs.rs/rust-zero-rpc) | Tonic gRPC clients and servers, health, balancing, discovery, and transport policies |
| [`rust-zero-gateway`](https://docs.rs/rust-zero-gateway) | Streaming HTTP proxying and descriptor/reflection-driven gRPC transcoding |
| [`rust-zero-mcp`](https://docs.rs/rust-zero-mcp) | MCP servers using Streamable HTTP, legacy HTTP+SSE, tools, resources, and prompts |
| [`rust-zero-mapreduce`](https://docs.rs/rust-zero-mapreduce) | Bounded asynchronous MapReduce execution |

## Explore by task

- Build a production REST stack: [REST services](#rest-services)
- Run a gRPC service: [RPC](#rpc)
- Proxy HTTP or transcode JSON to gRPC: [Gateway routing](#gateway-routing)
- Add runtime primitives: [Core runtime](#core-runtime)
- Add service discovery: [etcd](#etcd-configuration-and-discovery) or
  [Kubernetes](#kubernetes-discovery)
- Connect Redis, SQL, or MongoDB: [External stores](#external-stores)
- Expose health, metrics, and profiling: [Internal diagnostics](#internal-diagnostics)
- Compare runtime coverage with go-zero: [Feature parity](FEATURE_PARITY.md)

## Runnable examples

| Example | Command | External dependency |
| --- | --- | --- |
| REST middleware demo | `cargo run -p rust-zero-demo` | None |
| gRPC echo server | `cargo run -p rust-zero-rpc --example echo_server` | None |
| HTTP + gRPC gateway | `cargo run -p rust-zero-gateway --example mixed_upstreams` | None |
| MCP server | `cargo run -p rust-zero-mcp --example server` | None |
| etcd discovery | `cargo run -p rust-zero-core --features etcd --example etcd_discovery` | etcd |
| Kubernetes discovery | `cargo run -p rust-zero-core --features kubernetes --example kubernetes_discovery` | Cluster or kubeconfig |
| OpenTelemetry export | `cargo run -p rust-zero-core --features telemetry --example telemetry` | OTLP collector |
| Redis, SQL, and MongoDB | `cargo run -p rust-zero-core --features stores --example external_stores` | Configured stores |

Examples that use external systems read connection settings from environment variables; open the
linked [example sources](core/examples) for the accepted names and defaults. The mixed gateway
example starts its own HTTP and gRPC upstreams, making it the quickest way to try transcoding.

## What is included

rust-zero provides composable production primitives rather than a code generator:

- REST and gRPC authentication, middleware, clients, servers, streaming, and graceful draining.
- Circuit breaking, bounded retries, deadlines, adaptive shedding, rate limiting, load balancing,
  single-flight work, queues, batching, and MapReduce.
- Typed JSON/JSON5/TOML/YAML configuration with environment expansion and dynamic updates.
- Prometheus metrics, structured logging, W3C trace propagation, optional OpenTelemetry export,
  health reporting, diagnostics, and profiling.
- In-memory, etcd, and Kubernetes discovery; optional Redis, SQLite/PostgreSQL/MySQL, and MongoDB
  adapters; and HTTP/gRPC gateway routing.

The detailed [feature-parity matrix](FEATURE_PARITY.md) defines exact coverage and deliberate
ecosystem boundaries. [BACKLOG.md](BACKLOG.md) tracks audit status and remaining work.

## Compatibility

rust-zero supports Rust 1.89 and newer. Linux is the primary deployment target; macOS is supported
for local development. CI checks the locked dependency graph on Rust 1.89 with all features and
also tests minimal, adapter, telemetry, and all-feature combinations on stable Rust.

An MSRV increase is announced in the [release notes](CHANGELOG.md) and requires at least a minor
version change. Before 1.0, minor releases may contain API changes; patch releases preserve public
APIs except when a security or soundness fix makes that impossible. After 1.0, the project follows
[Semantic Versioning](https://semver.org/).

## Configuration

Configuration files may use JSON, JSON5, TOML, YAML, or YML. Environment variables can be expanded
with `$VAR` or `${VAR}`. JSON5 additionally supports comments, trailing commas, single-quoted
strings, and unquoted object keys.

REST and gRPC share `JwtClaimProjection`, `RequestSignatureVerifier`, and the stable `AuthFailure`
taxonomy. Request signatures use HMAC-SHA256 with named rotation keys and a configurable clock-skew
window; REST sends the values in `x-rust-zero-*` headers and gRPC uses corresponding metadata.

---

## Core runtime

`rust-zero-core` supplies the production controls that are shared by transport services and
background workers:

```rust
use rust_zero_core::{
    retry, AdaptiveShedder, CircuitBreaker, CircuitBreakerConfig, ConsistentHash,
    LoadShedderConfig, RetryPolicy, RollingCircuitBreakerConfig,
};
use std::time::Duration;

let breaker = CircuitBreaker::new(CircuitBreakerConfig::new(5, Duration::from_secs(30)));
let adaptive_breaker = CircuitBreaker::new(CircuitBreakerConfig::rolling(
    RollingCircuitBreakerConfig::new(),
));
let shedder = AdaptiveShedder::new(LoadShedderConfig::production(128));
let mut backends = ConsistentHash::new(100);
backends.add("http://users-a:8080");

let _permit = shedder.try_acquire();
let _response = breaker.execute(|| {
    // call a selected backend
    Ok::<_, std::io::Error>(())
})?;

// Rolling breakers expose current accepted/total history, drop probability, and lifetime outcomes.
let _adaptive_health = adaptive_breaker.snapshot();

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

let shedder = RpcLoadShedder::new(LoadShedderConfig::production(1_024));
let response = shedder.call(|| service.echo(request)).await?;
# Ok::<(), tonic::Status>(())
```

## REST services

The REST middleware is composable and returns standard HTTP responses when protection activates:

| Middleware | Response |
| --- | --- |
| `Timeout` | `504 Gateway Timeout` |
| `ConcurrencyLimit` | `503 Service Unavailable` |
| `AdaptiveLoadShed` | `503 Service Unavailable` |
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
The standard REST server enables CPU- and throughput-aware adaptive shedding by default and also
counts timeout, fixed-concurrency, adaptive-shed, and rate-limit rejections. Its load-shed settings
control the process CPU threshold, rolling bucket duration/count, and post-overload cooldown.
Permits remain active until HTTP or gRPC response streams finish, so long-lived work contributes to
latency and in-flight measurements. Attach a shared `HttpClientMetrics` instance with
`HttpClient::with_metrics` to record bounded service, method, status/error, latency, and in-flight
client metrics without labeling raw URLs.

For the standard production stack, load `RestServerConfig` and provide only application routes:

```rust
use actix_web::{web, HttpResponse};
use rest::{RestCorsConfig, RestServer, RestServerConfig};
use rust_zero_core::load_config;

let mut config: RestServerConfig = load_config("etc/users.toml")?;
config.cors = Some(
    RestCorsConfig::new(["https://console.example"])
        .with_methods(["GET", "POST"])
        .with_allowed_headers(["authorization", "content-type"])
        .with_exposed_headers(["x-request-id"])
        .with_credentials(true)
        .with_max_age(Some(600)),
);
let server = RestServer::new(config)?.run(|routes| {
    routes.route("/healthz", web::get().to(HttpResponse::Ok));
})?;
server.await?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

The same policy can be loaded from a `cors` configuration table. Use a single `"*"` entry to
allow any origin, standard method, or header; wildcard origins are reflected rather than emitted
as `*`, so credentialed requests remain standards-compliant. Set `automatic_preflight = false` to
route `OPTIONS` requests through application handlers. Use `with_max_age(None)` (or JSON/YAML
`null`) to omit the preflight cache header. CORS is disabled when the `cors` table is absent.

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
are inherited, while a route can override them or set `public = true` to opt out of group JWT.
The optional `middleware` list refers to application middleware registered on `RestServer`; it runs
in declaration order and can short-circuit or wrap the downstream response:

```toml
[[route_groups]]
prefix = "/api"
timeout_ms = 2000
max_body_bytes = 1048576
middleware = ["api-key"]

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

```rust
use actix_web::{dev::ServiceRequest, HttpResponse};
use rest::{RestServer, RouteMiddlewareNext};

let server = RestServer::new(Default::default())?.with_route_middleware(
    "api-key",
    |request: ServiceRequest, next: RouteMiddlewareNext| async move {
        if !request.headers().contains_key("x-api-key") {
            return Ok(request
                .into_response(HttpResponse::Forbidden().finish())
                .map_into_boxed_body());
        }
        next.call(request).await
    },
)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Use `serve_until` (or `serve_on_until` with a pre-bound listener) to connect an application signal
future to graceful Actix draining.

Static files can be installed as the default-service fallback without shadowing explicit API
routes. Embedded assets take precedence when combined with a directory:

```rust
use rest::{EmbeddedAsset, RestServer, StaticAssets};

let assets = StaticAssets::directory("public")?
    .with_embedded("version.txt", EmbeddedAsset::inferred("1.0.0"))?;
let server = RestServer::new(Default::default())?.with_static_assets(assets);
# Ok::<(), Box<dyn std::error::Error>>(())
```

For serverless platforms, build the routes once and translate each platform event into a
`ServerlessRequest`. The fully buffered response retains status, headers, and binary body bytes:

```rust
use actix_web::{http::Method, web, HttpResponse};
use rest::{RestServer, ServerlessRequest};

let handler = RestServer::new(Default::default())?
    .serverless_handler(|routes| {
        routes.route("/healthz", web::get().to(HttpResponse::Ok));
    })
    .await?;
let response = handler
    .call(ServerlessRequest::new(
        Method::GET,
        "/healthz".parse()?,
        web::Bytes::new(),
    ))
    .await?;
assert!(response.status.is_success());
# Ok::<(), Box<dyn std::error::Error>>(())
```

For APIs that require application-layer body secrecy, install `ContentEncryption` on the standard
server. Keep the previous key available while clients rotate; responses always use the provider's
current key:

```rust
use rest::{
    ContentEncryption, ContentEncryptionKey, RestServer, StaticContentKeyProvider,
};

let current = ContentEncryptionKey::new("2026-08", [7_u8; 32])?;
let previous = ContentEncryptionKey::new("2026-07", [3_u8; 32])?;
let keys = StaticContentKeyProvider::new(current).with_decryption_key(previous)?;
let server = RestServer::new(Default::default())?
    .with_content_encryption(ContentEncryption::new(keys, 4 * 1024 * 1024));
# Ok::<(), Box<dyn std::error::Error>>(())
```

Non-empty requests send `x-content-encryption: aes-256-gcm-v1` and `x-content-key-id`. The body is
standard base64 over `RZC1 || 12-byte nonce || ciphertext || 16-byte GCM tag`. Authentication binds
the request method and URI; response authentication also binds the HTTP status. The exported key
helpers implement the exact client-side format. This protects body confidentiality and integrity,
but does not hide HTTP metadata, prevent replay, or replace TLS and request-signature verification.
Because authenticated encryption must finish before a response is committed, encrypted servers
reject SSE responses and bound all other response buffering.

REST and RPC duration fields in serialized transport configs use millisecond-suffixed names such
as `request_timeout_ms`, `shutdown_timeout_ms`, and `connect_timeout_ms`.

## Internal diagnostics

`DevServer` provides the internal observability listener corresponding to go-zero's dev server.
It defaults to port `6060` and publishes `/healthz`, `/metrics`, `/debug/profile`,
`/debug/runtime`, `/debug/tasks`, `/debug/allocator`, and a route index at `/`. Profiling is opt-in
at the core primitive and enabled automatically by a dev server configured with profiling support.
Set `auth_token` to protect every endpoint with constant-time bearer authentication and combine
`private_only: true` with a literal loopback, private, or link-local `host` to prevent accidental
public binding. Unix builds can enable the `rest/sampling-profiler` feature and
`enable_sampling_profiler` to expose a bounded SVG flamegraph at `/debug/flamegraph`.

```rust
use rest::{DevServer, DevServerConfig};
use rust_zero_core::{HealthRegistry, Metrics, Profiler};
use std::sync::Arc;

let health = HealthRegistry::new();
let diagnostics_config = DevServerConfig {
    host: "127.0.0.1".to_owned(),
    private_only: true,
    auth_token: Some("load-this-from-a-secret-store".to_owned()),
    ..DevServerConfig::default()
};
let diagnostics = DevServer::new(
    diagnostics_config,
    Arc::new(Metrics::new()),
    Arc::new(Profiler::new()),
).with_health_registry(health.clone());

// A discovered RPC channel can update this aggregate directly via
// discovery_status.project_to_health(health, "users-rpc").

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
use rust_zero_core::{DiscoveryReconnectBackoff, EtcdClient, EtcdConfig};
use std::time::Duration;

let etcd = EtcdClient::connect(
    EtcdConfig::new(["http://127.0.0.1:2379"]).with_reconnect_backoff(
        DiscoveryReconnectBackoff::new(
            Duration::from_millis(200),
            Duration::from_secs(10),
            Duration::from_millis(200),
        ),
    ),
).await?;
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
use rust_zero_core::{DiscoveryReconnectBackoff, KubernetesDiscovery, KubernetesDiscoveryConfig};
use std::time::Duration;

let discovery = KubernetesDiscovery::infer(
    KubernetesDiscoveryConfig::new("production")
        .with_port_name("grpc")
        .with_scheme("http")
        .with_reconnect_backoff(DiscoveryReconnectBackoff::new(
            Duration::from_millis(200),
            Duration::from_secs(10),
            Duration::from_millis(200),
        )),
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
SQL stores also provide bounded bulk insertion and typed query/execute wrappers with stable
not-found errors, Prometheus operation metrics, and opt-in OpenTelemetry spans.
MongoDB stores provide the same typed operation, not-found, metrics, and tracing baseline, plus
native bounded `insert_many` batching and cache-aware mutation helpers.
Redis commands expose the same Prometheus and OpenTelemetry hooks, with explicit timeout outcomes
and bounded labels for application-supplied raw commands.
The Redis-backed token and keyed-period limiters execute atomic Lua scripts against standalone or
clustered deployments. Fixed windows use Redis server time and aligned boundaries. During an
outage, a bounded process-local limiter protects the instance while a single caller periodically
probes for recovery; `snapshot()` exposes Redis failures, recoveries, and remote/rescue outcomes.

```rust
use rust_zero_core::{
    CachedSqlStore, MongoCacheConfig, MongoStore, MongoStoreConfig, RedisModelCache,
    RedisModelCacheConfig, RedisPeriodLimiter, RedisStore, RedisStoreConfig, RedisTokenLimiter,
    SqlCacheConfig, SqlStoreConfig, SqliteStore,
};
use std::time::Duration;

let redis = RedisStore::new(RedisStoreConfig::new("redis://127.0.0.1/"))?;
redis
    .set_json("user:42", &serde_json::json!({"name": "Ada"}), Some(Duration::from_secs(60)))
    .await?;

let requests = RedisTokenLimiter::new(redis.clone(), "checkout", 100, 200);
if !requests.allow().await {
    // Return a stable over-quota response to this request.
}
let per_user = RedisPeriodLimiter::new(
    redis.clone(),
    "checkout-users",
    Duration::from_secs(60),
    20,
    4_096,
);
let decision = per_user.take("user-42").await;
println!("limit decision: {decision:?}");

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

let users = RedisModelCache::<serde_json::Value, std::io::Error>::new(
    redis,
    RedisModelCacheConfig::new(Duration::from_secs(60))
        .with_not_found_ttl(Duration::from_secs(5))
        .with_expiry_jitter(Duration::from_secs(3)),
);
let user = users
    .get_or_fetch("user:43", || async {
        Ok(Some(serde_json::json!({"name": "Grace"})))
    })
    .await?;

let sql = SqliteStore::connect_sqlite(SqlStoreConfig::new("sqlite://service.db")).await?;
let mut transaction = sql.begin().await?;

let users = CachedSqlStore::<sqlx::Sqlite, i64, String, sqlx::Error, String>::new(
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
let by_email = users
    .find_by_index("ada@example.com".to_owned(), |pool| async move {
        sqlx::query_as::<_, (i64, String)>(
            "SELECT id, name FROM users WHERE email = ?",
        )
        .bind("ada@example.com")
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
files, expire daily files, and gzip rotated files. Bounded non-blocking constructors keep local or
application-provided remote writer I/O off request threads and expose shed or failed records
through `dropped_records()`. `StructuredLogging` connects it to Actix requests, includes request
and W3C trace identifiers when the corresponding middleware runs outside it, and can classify slow
calls with stable transport-aware fields:

```rust
use rust_zero_core::{LogConfig, Logger, RotationPolicy};
use rest::{RequestId, StructuredLogging, TraceContextMiddleware};

let logger = Logger::new_non_blocking(
    LogConfig::file(
        "users-api",
        "logs",
        RotationPolicy::Size {
            max_bytes: 100 * 1024 * 1024,
            max_backups: 10,
        },
    )
    .with_rotated_compression(true),
    4_096,
)?;

// App::new()
//     .wrap(StructuredLogging::new(logger).with_slow_threshold(std::time::Duration::from_secs(1)))
//     .wrap(TraceContextMiddleware::new())
//     .wrap(RequestId::new())
```

## RPC

`rpc` provides common transport controls while leaving protobuf service implementations as normal
Tonic services. Static clients use `connect`; registry-backed clients use `connect_service` and
automatically follow endpoint publication and withdrawal events. `RpcServerStackBuilder` can attach
the structured logger with `with_slow_call_logging`; it records method, final trailer status,
declared deadline, elapsed time, cancellation, and trace context for unary and streaming calls.
The crate includes a generated `rust_zero.echo` service and runnable server:

```bash
cargo run -p rust-zero-rpc --example echo_server
```

```rust
use rpc::{health_reporter, BearerToken, RpcClient, RpcClientConfig, RpcServer, RpcServerConfig};
use rust_zero_core::{HealthRegistry, ServiceRegistry};
use std::time::Duration;

let server = RpcServer::new(
    RpcServerConfig::new("127.0.0.1:50051".parse()?)
        .with_request_timeout(Duration::from_secs(10))
        .with_service_timeout("rust_zero.echo.Echo", Duration::from_secs(5))
        .with_method_timeout("/rust_zero.echo.Echo/Echo", Duration::from_secs(1))
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
let _lease = registry.publish_weighted("users", "http://127.0.0.1:50051", 3)?;
let subscription = registry.subscribe("users")?;
let (balanced_channel, discovery_status) = RpcClient::new(
    RpcClientConfig::new("http://unused")
        .with_discovery_subset(64)
        .with_discovery_subset_seed(7)
        .with_discovery_health_check(Duration::from_secs(5), Duration::from_secs(1)),
)
.connect_discovered_with_status(subscription);
let current_discovery_status = discovery_status.snapshot();
let health_projection = discovery_status.clone().project_to_health(
    HealthRegistry::new(),
    "users-rpc",
);
let (reporter, _health_service) = health_reporter();
let grpc_health_projection = discovery_status.project_to_grpc_health(reporter, "users-rpc");
let client_auth = BearerToken::new("service-token")?;
```

`with_discovery_subset` bounds the number of unique connections a client opens for a large
discovery snapshot. Membership uses randomized rendezvous ranking, so snapshot reordering does not
cause churn and adding or removing an endpoint replaces only the affected members. Omit the option
to connect to every valid endpoint. The optional seed makes membership repeatable across process
restarts; without it, each client instance receives a random seed to spread load across the fleet.
`DiscoveryStatusSnapshot` reports both the complete `discovered` count and the locally `selected`
count.

Exact gRPC method timeouts override service-level timeouts, which override the global request
timeout. The same settings deserialize from `method_timeouts_ms` and `service_timeouts_ms` maps.
Servers using `serve_with_shutdown` can also publish themselves under a renewable etcd lease and
withdraw after graceful draining:

```rust
use rpc::{RpcEtcdRegistrationConfig, RpcServer, RpcServerConfig};
use std::time::Duration;

let registration = RpcEtcdRegistrationConfig::new(
    ["http://127.0.0.1:2379"],
    "users",
    "users-1",
    "http://127.0.0.1:50051",
)
.with_lease_ttl(Duration::from_secs(10));
let server = RpcServer::new(
    RpcServerConfig::new("127.0.0.1:50051".parse()?)
        .with_etcd_registration(registration),
);
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

The `gateway` binary loads JSON, JSON5, TOML, or YAML, validates every route and upstream before
binding, streams upstream responses, and drains in-flight calls on SIGINT/SIGTERM:

```toml
address = "0.0.0.0:8080"
workers = 4
request_timeout_ms = 30000
shutdown_timeout_ms = 30000
request_body_limit = 10485760
response_body_limit = 52428800
max_concurrent_requests = 1024
priority_concurrency_reserve = 256
rate_limit_requests_per_second = 10000
rate_limit_burst = 20000

[cors]
allowed_origins = ["https://console.example.com"]
allowed_methods = ["GET", "POST"]
allowed_headers = ["authorization", "content-type"]
exposed_headers = ["x-request-id"]
allow_credentials = true

[[route_groups]]
prefix = "/api"

[route_groups.jwt]
secret = "${DOWNSTREAM_JWT_SECRET}"

[[route_groups.routes]]
method = "GET"
path = "/{tail:.*}"

[[route_groups.routes]]
method = "GET"
path = "" # protect the exact /api prefix too

[[routes]]
prefix = "/api"
upstreams = ["http://users-a:8080", "http://users-b:8080"]

[[routes]]
prefix = "/"
upstreams = ["http://frontend:8080"]

[[grpc]]
prefix = "/grpc"
endpoints = ["https://greeter-a:50051", "https://greeter-b:50051"]
descriptor_set = "./descriptors/greeter.bin"
annotated_bindings = true
bearer_token = "${GREETER_TOKEN}"

[grpc.tls]
ca_certificate_pem = "${GRPC_CA_PEM}"
domain_name = "greeter.internal"

[[grpc.bindings]]
verb = "get"
path = "/grpc/greeters/{id}"
rpc = "acme.greeter.v1.Greeter.Get"
```

Run it with `cargo run -p rust-zero-gateway -- gateway.toml`. Set `reflection = true` instead of
`descriptor_set` to load descriptors from the upstream reflection service. A gRPC route accepts
either direct `endpoints` or live etcd discovery; for discovery, omit `endpoints` and add:

```toml
[grpc.discovery]
endpoints = ["https://etcd:2379"]
namespace = "/rust-zero"
service = "greeter"
username = "${ETCD_USERNAME}"
password = "${ETCD_PASSWORD}"
```

The discovery block also accepts `connect_timeout_ms` and an optional `tls` table. gRPC
transcoding supports unary and newline-delimited server-streaming responses. Client-streaming
methods are rejected during startup.

`GatewayServer` runs these routes inside the standard REST production stack. Logging, panic
recovery, trace propagation, metrics, request IDs, security headers, timeouts, body limits,
concurrency limiting, adaptive shedding, and per-route result-aware circuit breaking are enabled
by default. CORS, downstream JWT route policies, token-bucket rate limiting, and listener TLS/mTLS
are configured with the same fields as `RestServerConfig`; set `tls.certificate_pem`,
`tls.private_key_pem`, and optionally `tls.client_ca_pem` to enable HTTPS or mutual TLS. Gateway
catch-all policy paths use `/{tail:.*}` beneath the configured HTTP or gRPC prefix; add an empty
route path when the exact prefix is also a valid endpoint, as shown above.

Middleware names are resolved by applications embedding `GatewayServer`. They run in declaration
order, can mutate the outbound request, short-circuit dispatch, and wrap the streamed response;
blank, duplicate, and unregistered names are rejected before serving:

Add `middleware = ["service-token"]` to the corresponding `[[routes]]` table, then register it
while assembling the server:

```rust
use gateway::{GatewayMiddlewareNext, GatewayMiddlewareRequest, GatewayServer};

let server = GatewayServer::new(config)?.with_upstream_middleware(
    "service-token",
    |mut request: GatewayMiddlewareRequest, next: GatewayMiddlewareNext| async move {
        request.request_mut().headers_mut().insert(
            "authorization",
            "Bearer internal-token".parse().unwrap(),
        );
        next.call(request).await
    },
)?;
```

The stock `gateway` binary has no application policy registry, so its configuration should omit
`middleware`; use the library assembly above when named policies are configured.

For a self-contained mixed-protocol deployment, run the example below. It starts an HTTP upstream,
a gRPC upstream, and one public gateway listener. Requests under `/http` use streaming reverse
proxying, while requests under `/grpc` use descriptor-driven JSON-to-gRPC transcoding:

```bash
cargo run -p rust-zero-gateway --example mixed_upstreams
curl 'http://127.0.0.1:8080/http/orders?limit=2'
curl 'http://127.0.0.1:8080/grpc/greeters/7?view=full'
curl --no-buffer 'http://127.0.0.1:8080/grpc/greeters/7/watch'
```

Set `GATEWAY_ADDR`, `HTTP_UPSTREAM_ADDR`, or `GRPC_UPSTREAM_ADDR` to override the example's
listener addresses. SIGINT or SIGTERM gracefully drains both HTTP listeners and shuts down the
gRPC server.
