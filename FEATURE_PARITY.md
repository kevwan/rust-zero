# go-zero runtime feature parity

This matrix compares rust-zero with the runtime packages in
[go-zero v1.10.1](https://github.com/zeromicro/go-zero/tree/v1.10.1), the latest published
release audited on 2026-08-01. `goctl` and all other code generation are intentionally out of
scope.

The projects use different languages and ecosystems, so parity means equivalent production
capability rather than identical package names or APIs.

| Area | Status | rust-zero coverage |
| --- | --- | --- |
| Resilience | Covered | Sync/async circuit breakers with protocol-aware outcome classification, adaptive load shedding, deadlines, concurrency shedding, token bucket limits, and keyed period quotas |
| Load balancing | Covered at the primitive level | Consistent hashing, health-aware round robin, and P2C with inflight, latency-EWMA, and success feedback |
| Concurrency | Covered | Bounded queues, MapReduce, functional async helpers, single-flight calls, timed batching, rolling windows, and service groups |
| In-process caching | Covered | TTL and bounded LRU caches, statistics, explicit invalidation, and single-flight read-through fetching |
| Configuration | Partial | Typed JSON, TOML, and YAML loading, environment expansion, production defaults, and atomic dynamic snapshots with subscriptions; an etcd config-center subscriber is not bundled |
| Metrics | Covered | Labeled counters, gauges, histograms, Prometheus text rendering, and REST request metrics |
| Tracing | Covered | W3C propagation, exportable REST and gRPC client/server spans, parent-based ratio sampling, and batched OTLP/gRPC or OTLP/HTTP exporters behind opt-in `telemetry` features |
| Logging | Covered | Leveled JSON/plain structured logging, trace and request context, deterministic sampling, opt-in sensitive-field masking, daily/size file rotation, and REST request logging |
| Service discovery | Partial | Dynamic publish/withdraw subscriptions and balanced gRPC channels; Kubernetes and etcd adapters are not bundled |
| Messaging | Ecosystem-backed | Typed in-process topic fan-out is included; external brokers use application-selected clients |
| Data stores | Partial | Feature-gated standalone/clustered Redis strings, hashes, lists, sets, sorted sets, JSON, TTLs, counters, pub/sub publishing, ownership-safe locks, and coalesced cache-aside reads; typed SQLx pools, health checks, and transactions for SQLite, PostgreSQL, and MySQL; MongoDB and cached SQL/Mongo models remain |
| Validation | Covered | Multi-field typed validation plus Actix JSON and query extractors that reject invalid requests before handlers run |
| REST | Covered | Actix routing/extractors, resilient named HTTP clients, CORS, bearer and JWT auth, request IDs, W3C tracing, recovery, security headers, gzip input, size/deadline/concurrency/rate controls, and metrics |
| gRPC | Covered | Tonic client/server configuration, health reporting, authentication, dynamic balancing, trace propagation, deadlines, limits, and keepalives |
| Gateway | Covered for HTTP proxying | Longest-prefix routing, health-aware round robin, safe header forwarding, deadlines, body limits, and an executable Actix proxy handler |
| Profiling and diagnostics | Covered for framework diagnostics | Opt-in named duration profiling plus an internal HTTP server with route discovery, health, Prometheus metrics, aggregate profile reports, and process/runtime information; sampling/flamegraph profiling remains platform-specific |

## Remaining upstream gaps

The remaining `core/stores` gaps are MongoDB helpers and cached SQL/Mongo models. The other material
gaps are concrete etcd/Kubernetes discovery and configuration-center adapters. These are
deliberately marked as gaps rather than being counted as covered by a backend-neutral trait.

External adapters should be feature-gated and added with integration tests against the real
backend. This keeps the default framework small without presenting an unverified wrapper as
feature parity.
