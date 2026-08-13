# go-zero runtime feature parity

This matrix compares rust-zero with the runtime packages in
[go-zero v1.10.3](https://github.com/zeromicro/go-zero/tree/v1.10.3), the latest published
release audited on 2026-08-09. A [follow-up comparison](https://github.com/zeromicro/go-zero/compare/f7805d5e322361f65561e8f562121b35404593a3...91a4cdbaf4e987f1c44ab14fb639756f213328f0)
through `91a4cdba` found no newer runtime capability on upstream `master`. `goctl`, API/protobuf/
model/client generation, templates, and deployment scaffolding are intentionally excluded from
the runtime claim.

The projects use different languages and ecosystems, so parity means equivalent production
capability rather than identical package names or APIs.

| Area | Status | rust-zero coverage |
| --- | --- | --- |
| Resilience | Covered | Selectable consecutive-failure and go-zero-style rolling adaptive circuit breakers with probabilistic rejection, bounded recovery probes, cancellation-safe protocol outcome accounting, client transport integration, and default result-aware per-route REST and per-method gRPC server integration. CPU/throughput-aware shedding, deadlines, concurrency shedding, and local/distributed limiting are covered |
| Load balancing | Covered at the primitive level | Consistent hashing, health-aware round robin, and P2C with inflight, latency-EWMA, and success feedback |
| Concurrency | Covered | Bounded queues, MapReduce, functional async helpers, single-flight calls, count- and byte-batched execution, coalesced delay and threshold executors, supervised periodic execution, rolling windows, and service groups |
| Process lifecycle | Covered | SIGINT/SIGTERM-aware service supervision, cooperative cancellation, fail-fast sibling shutdown, and bounded graceful draining |
| In-process caching | Covered | TTL and bounded LRU caches, statistics, explicit invalidation, and single-flight read-through fetching |
| Configuration | Covered | Typed JSON, JSON5, TOML, and YAML loading, environment expansion, production defaults, atomic dynamic snapshots, and a revision-safe etcd configuration watcher that retains the last known-good value |
| Metrics | Covered | Labeled counters, gauges, histograms, Prometheus text rendering, REST server/client request and in-flight metrics, REST protection-rejection counters, and cardinality-bounded gRPC client/server metrics through final stream status, installed by the standard transport stacks |
| Tracing | Covered | W3C propagation, exportable REST and gRPC client/server spans, parent-based ratio sampling, and batched OTLP/gRPC or OTLP/HTTP exporters behind opt-in `telemetry` features |
| Authentication | Covered | Bearer validation, HS256 JWTs with previous-secret rotation and configurable dot-path claim projection, and time-window-bounded HMAC request signatures shared by REST and gRPC with stable machine-readable failures |
| Logging | Covered | Leveled JSON/plain structured logging, trace and request context, deterministic sampling, opt-in sensitive-field masking, daily/size file rotation with retention and optional gzip compression, bounded non-blocking local or remote writers with drop accounting, and transport-aware REST and final-status gRPC slow-call fields |
| Service discovery | Broad coverage | Backend-neutral snapshots, dynamically balanced gRPC channels, endpoint metadata/weights, active probes, readiness, in-memory discovery, TLS/mTLS-capable renewable etcd leases, automatic gRPC server registration, and Kubernetes EndpointSlice recovery are covered |
| In-process coordination | Covered | Typed topic fan-out plus supervised bounded queues with configurable workers, pause/resume, bounded shutdown, lifecycle events, balanced failover and fan-out pushing, and processing metrics |
| Data stores | Covered | Feature-gated standalone/clustered Redis strings, collections, JSON, TTLs, counters, publishing, reconnecting channel/pattern subscriptions, locks, pipelines, Lua scripts, streams/consumer groups, model caching, and instrumented commands; typed SQLx pools with instrumented query/execute, standardized not-found errors, and bounded bulk insertion; and MongoDB collections with instrumented typed operations, native bounded bulk insertion, transactions, cache-aware mutations, and bounded primary/secondary-index record caches. Model caches support tagged not-found entries, bounded expiry jitter, statistics, single-flight loading, serialization errors, and cross-process or mutation-wide invalidation. CI exercises all adapters against real backends |
| Validation | Covered | Multi-field typed validation plus Actix JSON, query, path, form, application-typed header, combined path/query/header/JSON, and bounded streaming multipart extractors with stable machine-readable errors |
| REST | Broad coverage | Actix routing/extractors, HTTP clients, SSE, validated configuration-driven CORS, TLS/mTLS, auth, observability, recovery, request controls, standard assembly, default per-route result-aware circuit breaking, route policy, static/serverless serving, graceful draining, and content encryption are covered |
| gRPC | Broad coverage | Validated Tonic TLS/mTLS transport configuration, health, auth, discovery/balancing, tracing, global/service/method deadlines, keepalives, client circuit breaking, default per-method result-aware server circuit breaking, adaptive shedding, metrics, streaming, automatic etcd registration, and graceful drain are covered |
| Gateway | Covered | Validated file-backed listener/route/upstream configuration, ordered named per-upstream middleware with request mutation, short-circuiting, and response wrapping, signal-aware bounded draining, longest-prefix streaming HTTP proxying, descriptor or live-reflection-driven HTTP-to-gRPC transcoding with explicit and `google.api.http` bindings, canonical protobuf JSON, metadata forwarding, status mapping, newline-delimited server streams, and a runnable mixed-protocol deployment example |
| MCP | Covered | Explicitly selectable 2025-03-26 Streamable HTTP and legacy 2024-11-05 HTTP+SSE transports provide stateless or expiring stateful sessions, JSON/SSE responses, session-specific legacy message endpoints, resumable GET streams, termination, cancellation, dispatch, protocol errors, protection, startup, and graceful draining |
| Profiling and diagnostics | Covered | Opt-in named duration profiling plus an internal HTTP server with route discovery, health, Prometheus metrics, task/runtime and allocator-memory diagnostics, constant-time bearer protection, private-only binding, and feature-gated Unix sampling flamegraphs |
| Release readiness | Covered | Published Rust 1.89 MSRV and compatibility policy, locked minimal/adapter/telemetry/all-feature CI, warning-free rustdoc and compiled feature examples, plus runnable etcd, Kubernetes, OTLP, Redis, SQL, and MongoDB deployment examples |

## External adapters

External adapters should be feature-gated and added with integration tests against the real
backend. This keeps the default framework small without presenting an unverified wrapper as
feature parity.

Durable messaging is an intentional ecosystem boundary, not a rust-zero parity claim. Applications
should select Kafka, RabbitMQ, or another broker client according to their delivery and operational
requirements, then hand decoded work to rust-zero's in-process queue runtime when its worker
supervision and backpressure semantics are useful.

See [BACKLOG.md](BACKLOG.md) for prioritized gaps and acceptance criteria.
