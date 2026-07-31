# go-zero runtime feature parity

This matrix compares rust-zero with the runtime packages in
[go-zero v1.10.2](https://github.com/zeromicro/go-zero/tree/v1.10.2). `goctl` and all other code
generation are intentionally out of scope.

The projects use different languages and ecosystems, so parity means equivalent production
capability rather than identical package names or APIs.

| Area | Status | rust-zero coverage |
| --- | --- | --- |
| Resilience | Covered | Circuit breaker, adaptive load shedding, deadlines, concurrency shedding, token bucket limits, and keyed period quotas |
| Concurrency | Covered | Bounded queues, MapReduce, functional async helpers, single-flight calls, timed batching, rolling windows, and service groups |
| In-process caching | Covered | TTL and bounded LRU caches, statistics, explicit invalidation, and single-flight read-through fetching |
| Configuration | Covered | Typed JSON, TOML, and YAML loading with environment expansion and production service defaults |
| Metrics | Covered | Labeled counters, gauges, histograms, Prometheus text rendering, and REST request metrics |
| Tracing | Partial | W3C trace-context creation and propagation for REST and gRPC; OpenTelemetry exporters are not bundled |
| Logging | Partial | Structured REST request logging through `tracing`; standalone `logx`-style rotation and field masking are not bundled |
| Service discovery | Partial | Dynamic publish/withdraw subscriptions and balanced gRPC channels; Kubernetes and etcd adapters are not bundled |
| Messaging | Partial | Typed in-process topic fan-out; Kafka and RabbitMQ adapters are not bundled |
| Data stores | Backend-specific | No built-in Redis, SQL, MongoDB, or cache-aside database adapters; applications use the Rust ecosystem clients directly |
| REST | Covered | Actix routing/extractors, CORS, bearer and JWT auth, request IDs, W3C tracing, recovery, security headers, gzip input, size/deadline/concurrency/rate controls, and metrics |
| gRPC | Covered | Tonic client/server configuration, health reporting, authentication, dynamic balancing, trace propagation, deadlines, limits, and keepalives |
| Gateway | Covered for HTTP proxying | Longest-prefix routing, health-aware round robin, safe header forwarding, deadlines, body limits, and an executable Actix proxy handler |
| Profiling | Ecosystem-provided | Use platform profilers such as `perf`, Instruments, or Tokio Console; no always-on profiler is bundled |

## Design boundary

External systems are kept behind application-selected Rust clients. A Redis, Kafka, SQL, MongoDB,
Kubernetes, etcd, or OpenTelemetry adapter should only be added with integration tests against that
backend. This keeps the default framework small and avoids presenting unverified adapters as
feature parity.
