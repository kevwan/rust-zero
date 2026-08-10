# go-zero runtime feature parity

This matrix compares rust-zero with the runtime packages in
[go-zero v1.10.3](https://github.com/zeromicro/go-zero/tree/v1.10.3), the latest published
release audited on 2026-08-09. Upstream `master` was also checked through `f7805d5e`; its
post-release changes add no runtime capabilities. `goctl` and all other code generation are
intentionally out of scope.

The projects use different languages and ecosystems, so parity means equivalent production
capability rather than identical package names or APIs.

| Area | Status | rust-zero coverage |
| --- | --- | --- |
| Resilience | Covered | Selectable consecutive-failure and go-zero-style rolling adaptive circuit breakers with probabilistic rejection, bounded recovery probes, cancellation-safe protocol outcome accounting, process-CPU and rolling-throughput-aware load shedding with minimum-latency capacity, smoothed in-flight work, and overload cooldown, deadlines, fixed concurrency shedding, process-local token/period limits, and atomic standalone/clustered Redis token and aligned keyed-period quotas with bounded outage rescue and recovery monitoring |
| Load balancing | Covered at the primitive level | Consistent hashing, health-aware round robin, and P2C with inflight, latency-EWMA, and success feedback |
| Concurrency | Covered | Bounded queues, MapReduce, functional async helpers, single-flight calls, count- and byte-batched execution, coalesced delay and threshold executors, supervised periodic execution, rolling windows, and service groups |
| Process lifecycle | Covered | SIGINT/SIGTERM-aware service supervision, cooperative cancellation, fail-fast sibling shutdown, and bounded graceful draining |
| In-process caching | Covered | TTL and bounded LRU caches, statistics, explicit invalidation, and single-flight read-through fetching |
| Configuration | Covered | Typed JSON, JSON5, TOML, and YAML loading, environment expansion, production defaults, atomic dynamic snapshots, and a revision-safe etcd configuration watcher that retains the last known-good value |
| Metrics | Covered | Labeled counters, gauges, histograms, Prometheus text rendering, REST server/client request and in-flight metrics, REST protection-rejection counters, and cardinality-bounded gRPC client/server metrics through final stream status, installed by the standard transport stacks |
| Tracing | Covered | W3C propagation, exportable REST and gRPC client/server spans, parent-based ratio sampling, and batched OTLP/gRPC or OTLP/HTTP exporters behind opt-in `telemetry` features |
| Authentication | Covered | Bearer validation, HS256 JWTs with previous-secret rotation and configurable dot-path claim projection, and time-window-bounded HMAC request signatures shared by REST and gRPC with stable machine-readable failures |
| Logging | Covered | Leveled JSON/plain structured logging, trace and request context, deterministic sampling, opt-in sensitive-field masking, daily/size file rotation with retention and optional gzip compression, bounded non-blocking local or remote writers with drop accounting, and transport-aware REST and final-status gRPC slow-call fields |
| Service discovery | Covered | A backend-neutral snapshot subscription consumed directly by dynamically balanced gRPC channels, backward-compatible structured endpoint metadata with bounded relative weights, opt-in active HTTP/2 probes, watchable empty/ready/degraded counts, dynamic in-memory publish/withdraw subscriptions, feature-gated etcd discovery with renewable leases, and Kubernetes EndpointSlice discovery with atomic relists and resource-version recovery. Etcd and Kubernetes resolvers use configurable capped exponential reconnect with bounded jitter, and channel readiness projects directly into standard gRPC and dev-server health |
| In-process coordination | Covered | Typed topic fan-out plus supervised bounded queues with configurable workers, pause/resume, bounded shutdown, lifecycle events, balanced failover and fan-out pushing, and processing metrics |
| Data stores | Covered | Feature-gated standalone/clustered Redis strings, collections, JSON, TTLs, counters, publishing, reconnecting channel/pattern subscriptions, locks, pipelines, Lua scripts, streams/consumer groups, model caching, and instrumented commands; typed SQLx pools with instrumented query/execute, standardized not-found errors, and bounded bulk insertion; and MongoDB collections with instrumented typed operations, native bounded bulk insertion, transactions, cache-aware mutations, and bounded primary/secondary-index record caches. Model caches support tagged not-found entries, bounded expiry jitter, statistics, single-flight loading, serialization errors, and cross-process or mutation-wide invalidation. CI exercises all adapters against real backends |
| Validation | Covered | Multi-field typed validation plus Actix JSON, query, path, form, application-typed header, combined path/query/header/JSON, and bounded streaming multipart extractors with stable machine-readable errors |
| REST | Covered | Actix routing/extractors, resilient named HTTP clients, server-sent events, CORS, bearer/JWT/signature auth, request IDs, W3C tracing, recovery, security headers, gzip input, size/deadline/fixed-concurrency/CPU-throughput-adaptive/rate controls, metrics, validated configuration-driven assembly, declarative route groups, ordered named application middleware, per-route policy, request-aware success/error envelopes, gRPC-status translation, flush-oriented streaming, traversal-safe directory/embedded static fallback, socket-free serverless dispatch, signal-driven graceful draining, and opt-in bounded AES-256-GCM request/response encryption with pluggable current/retained-key rotation |
| gRPC | Covered | Deserializable and validated Tonic client/server transport configuration, health reporting, bearer/JWT/signature authentication, backend-neutral dynamic discovery and balancing, trace propagation, deadlines, keepalives, protocol-aware client circuit breaking, stream-lifetime-aware CPU/throughput adaptive server load shedding, and cardinality-bounded client/server transport metrics through final stream trailers. Generated-service-independent stacks are covered end to end across unary, client-streaming, server-streaming, and bidirectional calls, including cancellation, health transitions, status mapping, and bounded graceful drain |
| Gateway | Covered | Validated file-backed listener/route/upstream configuration, ordered named per-upstream middleware with request mutation, short-circuiting, and response wrapping, signal-aware bounded draining, longest-prefix streaming HTTP proxying, descriptor or live-reflection-driven HTTP-to-gRPC transcoding with explicit and `google.api.http` bindings, canonical protobuf JSON, metadata forwarding, status mapping, newline-delimited server streams, and a runnable mixed-protocol deployment example |
| MCP | Covered | The 2025-03-26 Streamable HTTP transport provides stateless or expiring stateful sessions, JSON/SSE responses, resumable GET streams with bounded event replay, explicit termination, cancellation, deterministic tool/resource/prompt dispatch, protocol errors, deadlines, origin protection, HTTP metadata projection, standalone startup, and bounded graceful draining through the shared service lifecycle |
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
