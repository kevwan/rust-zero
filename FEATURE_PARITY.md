# go-zero runtime feature parity

This matrix compares rust-zero with the runtime packages in
[go-zero v1.10.3](https://github.com/zeromicro/go-zero/tree/v1.10.3), the latest published
release audited on 2026-08-08. `goctl` and all other code generation are intentionally out of
scope.

The projects use different languages and ecosystems, so parity means equivalent production
capability rather than identical package names or APIs.

| Area | Status | rust-zero coverage |
| --- | --- | --- |
| Resilience | Covered | Sync/async circuit breakers with protocol-aware outcome classification, adaptive load shedding, deadlines, concurrency shedding, token bucket limits, and keyed period quotas |
| Load balancing | Covered at the primitive level | Consistent hashing, health-aware round robin, and P2C with inflight, latency-EWMA, and success feedback |
| Concurrency | Covered | Bounded queues, MapReduce, functional async helpers, single-flight calls, count- and byte-batched execution, coalesced delay and threshold executors, supervised periodic execution, rolling windows, and service groups |
| Process lifecycle | Covered | SIGINT/SIGTERM-aware service supervision, cooperative cancellation, fail-fast sibling shutdown, and bounded graceful draining |
| In-process caching | Covered | TTL and bounded LRU caches, statistics, explicit invalidation, and single-flight read-through fetching |
| Configuration | Covered | Typed JSON, JSON5, TOML, and YAML loading, environment expansion, production defaults, atomic dynamic snapshots, and a revision-safe etcd configuration watcher that retains the last known-good value |
| Metrics | Covered | Labeled counters, gauges, histograms, Prometheus text rendering, REST server/client request and in-flight metrics, REST protection-rejection counters, and cardinality-bounded gRPC client/server metrics through final stream status, installed by the standard transport stacks |
| Tracing | Covered | W3C propagation, exportable REST and gRPC client/server spans, parent-based ratio sampling, and batched OTLP/gRPC or OTLP/HTTP exporters behind opt-in `telemetry` features |
| Logging | Covered | Leveled JSON/plain structured logging, trace and request context, deterministic sampling, opt-in sensitive-field masking, daily/size file rotation, and REST request logging |
| Service discovery | Covered | A backend-neutral snapshot subscription consumed directly by balanced gRPC channels, dynamic in-memory publish/withdraw subscriptions, feature-gated etcd discovery with renewable leases, and Kubernetes EndpointSlice discovery with atomic relists and self-healing resource-version watches |
| Messaging | Covered for in-process workloads | Typed topic fan-out plus supervised bounded queues with configurable workers, pause/resume, bounded shutdown, lifecycle events, balanced failover and fan-out pushing, and processing metrics. External brokers use application-selected clients |
| Data stores | Substantial | Feature-gated standalone/clustered Redis strings, collections, JSON, TTLs, counters, publishing, locks, and cache-aside reads; typed SQLx pools, transactions, and single-key cached records for SQLite, PostgreSQL, and MySQL; typed MongoDB collections, health checks, sessions, transactions, and cached records. SQL/Mongo caches support separate not-found policy, bounded expiry jitter, statistics, and race-safe invalidation. CI exercises all adapters against real backends; multi-key/Redis-backed model caches, deeper helpers, and instrumentation remain |
| Validation | Covered | Multi-field typed validation plus Actix JSON, query, path, form, application-typed header, combined path/query/header/JSON, and bounded streaming multipart extractors with stable machine-readable errors |
| REST | Substantial | Actix routing/extractors, resilient named HTTP clients, server-sent events, CORS, bearer and JWT auth, request IDs, W3C tracing, recovery, security headers, gzip input, size/deadline/concurrency/rate controls, metrics, validated configuration-driven assembly, declarative route groups, per-route policy, request-aware success/error envelopes, gRPC-status translation, flush-oriented streaming, and signal-driven graceful draining. Static/serverless serving and custom group middleware remain |
| gRPC | Covered | Deserializable and validated Tonic client/server transport configuration, health reporting, authentication, backend-neutral dynamic discovery and balancing, trace propagation, deadlines, keepalives, protocol-aware client circuit breaking, adaptive server load shedding, and cardinality-bounded client/server transport metrics through final stream trailers. Generated-service-independent stacks are covered end to end across unary, client-streaming, server-streaming, and bidirectional calls, including cancellation, health transitions, status mapping, and bounded graceful drain |
| Gateway | Covered | Longest-prefix HTTP proxying plus descriptor or live-reflection-driven HTTP-to-gRPC transcoding, explicit and `google.api.http` bindings, canonical protobuf JSON, metadata forwarding, status mapping, and newline-delimited server streams |
| MCP | Missing | go-zero's Streamable HTTP MCP server, tools/resources/prompts, session lifecycle, and request-metadata bridge are not implemented |
| Profiling and diagnostics | Covered for framework diagnostics | Opt-in named duration profiling plus an internal HTTP server with route discovery, health, Prometheus metrics, aggregate profile reports, and process/runtime information; sampling/flamegraph profiling remains platform-specific |

## External adapters

External adapters should be feature-gated and added with integration tests against the real
backend. This keeps the default framework small without presenting an unverified wrapper as
feature parity.

See [BACKLOG.md](BACKLOG.md) for prioritized gaps and acceptance criteria.
