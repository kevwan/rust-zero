# go-zero parity backlog

This backlog tracks production capabilities still needed after the
[go-zero v1.10.3](https://github.com/zeromicro/go-zero/tree/v1.10.3) runtime audit on
2026-08-08. Equivalent Rust/Actix/Tonic behavior is the goal; identical Go APIs are not.
`goctl`, API/protobuf code generation, and language-client generation remain out of scope.

## P0 — framework-level parity

- [x] **Configuration-driven REST server assembly.** The standard stack, declarative route-policy
  layer, and end-to-end lifecycle proof are present.
  - [x] Add a deserializable, validated `RestServerConfig` with production defaults.
  - [x] Assemble logging, recovery, request IDs, tracing, metrics, security headers, timeout,
    decompression/body limits, concurrency shedding, listener binding, and bounded Actix shutdown.
  - [x] Accept an Actix route configuration callback so ordinary routes and scoped groups do not
    require manual server or middleware wiring.
  - [x] Add declarative route groups and per-route JWT/timeout/body-limit/priority/SSE overrides.
  - [x] Prove signal-driven graceful draining with an in-flight request integration test.
- [x] **Configuration-driven gRPC assembly.** Transport configuration and the
  generated-service-independent interceptor and lifecycle layer are complete.
  - [x] Deserialize and validate server/client deadlines, concurrency, streaming, connection,
    keepalive, endpoint, and shutdown settings using millisecond config values.
  - [x] Retain configured Tonic server construction and direct/dynamic client channel construction,
    and add signal-triggered graceful serving with a bounded drain timeout.
  - [x] Compose server auth, panic recovery, tracing, metrics, adaptive shedding, and health without
    requiring each generated service to repeat the stack.
  - [x] Compose client auth, tracing, metrics, default deadlines, protocol-aware circuit breaking,
    and discovery behind a generated-client-friendly service wrapper.
  - [x] Add unary and streaming integration tests that verify interceptor ordering, status mapping,
    cancellation, health transitions, and in-flight draining on both sides.
- [x] **Discovery-to-transport integration.** A common complete-snapshot subscription trait lets
  `RpcClient` consume in-memory, etcd, or Kubernetes discovery directly. Empty snapshots recover,
  malformed endpoints do not poison valid peers, live replacement rebalances the channel, lagged
  in-memory subscriptions resynchronize, backend watch implementations reconnect, and the adapter
  task stops when the discovery stream or balanced channel closes.
- [x] **HTTP-to-gRPC gateway transcoding.** Protobuf descriptor and live gRPC reflection loading,
  annotated and
  explicit route mappings, JSON/protobuf conversion, metadata forwarding, gRPC-to-HTTP status
  mapping, and server-streaming behavior are covered end to end. Client-streaming HTTP bindings
  are rejected during assembly because HTTP/JSON has no portable request-stream representation.
- [x] **Real-backend conformance tests.** Exercise etcd lease/watch recovery, Kubernetes
  EndpointSlice relists, Redis standalone/cluster locks and cache operations, all three SQL engines,
  and Mongo sessions/transactions in CI. A feature must not move to “Covered” based only on mocks or
  compile tests.

## P1 — material runtime gaps

- [x] **Complete REST request parsing.** Streaming multipart uploads now enforce per-field,
  per-file, and aggregate limits while spilling files to automatically cleaned temporary storage.
  Validated JSON, query, path, URL-encoded form, typed header, and combined
  path/query/header/JSON extraction are also present, with configured JSON/form limits and stable
  structured error bodies.
- [x] **Response and error policy.** Per-server response policies provide request-aware,
  application-configurable success/error envelopes, stable typed errors, go-zero-compatible
  gRPC-status translation, pre-commit JSON serialization failure handling, and chunked
  anti-buffered streaming helpers.
- [ ] **Static/serverless serving and custom route middleware.** Add embedded/static file fallback,
  a prebuilt serverless handler, and application-defined middleware lists on declarative route
  groups. Prefixes and per-route JWT/timeout/body-limit/priority/SSE settings are present.
- [x] **Automatic RPC transport metrics.** Reusable gRPC client/server layers cover request counts,
  latency, in-flight calls, final trailer status, transport/body errors, cancellation, and bounded
  method cardinality for unary and streaming calls. REST server/client request and in-flight
  metrics plus timeout, concurrency-shed, rate-limit, and client-circuit rejection outcomes are
  present. The standard generated-service-independent client and server stacks install them.
- [ ] **Model-cache semantics.** Add multi-key index-to-primary mappings, randomized expirations,
  cache-not-found policy, shared invalidation after mutations, cache statistics, and Redis cluster
  cache nodes matching go-zero's `sqlc`/`cache` behavior. The current SQL/Mongo cache wrappers are
  single-key read-through helpers.
  - [x] Support separate positive/not-found TTLs, optional negative caching, bounded TTL jitter,
    cache statistics, and race-safe invalidation in both SQL and Mongo wrappers.
  - [ ] Add secondary-index-to-primary mappings and invalidate every related key atomically after
    insert, update, or delete.
  - [ ] Add Redis-backed cache nodes with standalone/cluster routing, serialization errors,
    not-found sentinels, and cross-process invalidation tests.
- [ ] **Store helper depth.** Add Redis subscription, remaining streams/consumer-group helpers,
  pipelines and script helpers; SQL bulk insert, typed
  query/execute instrumentation, and standardized not-found errors; Mongo bulk insertion,
  cached-model mutation helpers, and tracing/metrics hooks.
- [x] **Queue runtime.** Supervised bounded queues provide configurable worker pools,
  pause/resume, bounded shutdown, surfaced worker panics, per-message lifecycle/failure events,
  balanced failover pushing, fan-out pushing, and Prometheus processing metrics.
- [ ] **MCP server runtime.** Add Streamable HTTP transport, tool/resource/prompt registration,
  session lifecycle, protocol-compliant errors, graceful shutdown, and an opt-in bridge that makes
  HTTP headers, query values, and path parameters available to handlers. Cover initialization,
  stateless/stateful sessions, notifications, cancellation, and reconnect behavior in integration
  tests.
- [x] **Executor family.** Byte-weighted chunk batching, coalesced delayed execution, and
  threshold/"less" execution are present. Chunk and delay executors provide explicit shutdown;
  delayed and periodic jobs surface worker failures and support bounded shutdown.
- [ ] **Authentication parity.** Add configurable JWT claim projection, request-signature
  authentication, and consistent REST/gRPC auth failure responses. HS256 validation and previous-
  secret rotation are already supported.
- [ ] **Logging operations.** Add non-blocking buffered output, retention and compression of rotated
  files, remote writer hooks, dropped-log accounting, and transport-aware slow-call fields.

## P2 — completeness and operational polish

- [ ] Add weighted endpoint metadata, active health checking, resolver backoff/jitter, and explicit
  degraded/ready states to discovery and balancing.
- [ ] Expand the dev server with sampling profiles/flamegraphs, task and allocator diagnostics,
  authenticated access, and an option to bind diagnostics only to a private interface.
- [ ] Add REST content encryption/decryption middleware with a pluggable key provider if real users
  require parity with go-zero's cryption handler.
- [ ] Add gateway configuration loading, per-upstream middleware, connection draining, streaming
  proxy tests, and end-to-end examples for mixed HTTP and gRPC upstreams.
- [ ] Publish compatibility policy, MSRV, feature-combination CI, rustdoc examples, and runnable
  deployment examples for etcd, Kubernetes, telemetry, and external stores.

## Recommended execution order

These are the next independently shippable slices, ordered by parity impact and dependency:

1. **REST extension points:** named application middleware registry for declarative groups, then a
   safe static-directory/embedded fallback and a prebuilt serverless handler.
2. **Model cache indexes:** secondary-to-primary key mapping and mutation-wide invalidation, then a
   Redis-backed implementation tested against standalone and clustered Redis.
3. **Store depth:** Redis pub/sub, streams, pipelines, and scripts; standardized SQL not-found and
   bulk helpers; Mongo bulk/mutation helpers; shared tracing and metrics hooks.
4. **Authentication:** JWT claim projection and request-signature verification shared by REST and
   gRPC, with one stable error taxonomy.
5. **Logging operations:** bounded non-blocking writer, dropped-record metric, rotated-file
   retention/compression, remote sink hooks, and slow-call fields.
6. **MCP runtime:** protocol core and stateless Streamable HTTP first; stateful sessions,
   reconnect/cancellation, metadata bridging, and graceful drain second.
7. **Discovery readiness:** weighted metadata, active probes, jittered reconnect backoff, and
   explicit ready/degraded states consumed by balancers and health endpoints.
8. **Operational polish:** gateway configuration/draining, authenticated richer diagnostics,
   feature-matrix CI, MSRV policy, and deployment examples.

## Recently completed

- [x] Generated-service-independent gRPC lifecycle coverage for unary, client-streaming,
  server-streaming, and bidirectional calls, including auth/trace ordering, health transitions,
  final status mapping, client/server cancellation metrics, circuit feedback, successful graceful
  drain, and bounded drain timeout.
- [x] A generated-client-friendly gRPC service stack that composes bearer credentials, W3C trace
  propagation, default deadlines, cardinality-bounded metrics, and trailer-aware protocol circuit
  breaking, including cancellation-safe half-open accounting.
- [x] Configurable SQL/Mongo not-found caching and bounded TTL jitter to prevent synchronized model
  cache expiry, while retaining cache statistics and race-safe mutation invalidation.
- [x] Supervised queue workers with pause/resume, bounded shutdown, lifecycle events, failure
  reporting, balanced and fan-out pushers, and processing metrics.
- [x] Per-server, request-aware REST success/error policies with stable typed errors,
  gRPC-to-HTTP translation, deterministic serialization failures, and flush-oriented streaming.
- [x] Backend-neutral discovery snapshots wired directly into balanced gRPC channels, including
  empty-set recovery, malformed-peer isolation, live add/remove replacement, lag recovery, and
  channel-driven watcher shutdown.
- [x] Cardinality-safe REST client metrics, REST server in-flight gauges, and timeout,
  concurrency-shed, rate-limit, and client-circuit rejection outcomes, with automatic server-stack
  installation.
- [x] Cardinality-bounded gRPC client/server request, latency, in-flight, status, transport-error,
  and cancellation metrics that observe streaming trailers and wrap generated Tonic transports.
- [x] Descriptor/reflection-driven HTTP-to-gRPC transcoding with annotated/explicit bindings,
  protobuf JSON, metadata, canonical status mapping, and newline-delimited server streams.
- [x] Real-backend CI for etcd, Kubernetes EndpointSlices, standalone/clustered Redis, SQLite,
  PostgreSQL, MySQL, and transactional MongoDB.
- [x] Streaming multipart form extraction with bounded text fields, bounded temporary files,
  aggregate request limits, repeated-field support, metadata preservation, and cleanup on success
  or failure.
- [x] Chunk, delay, and threshold/"less" executors with coalescing, backpressure, explicit flush,
  failure propagation, and shutdown tests.
- [x] Declarative REST route groups with inherited and per-route JWT, timeout, body-limit,
  priority-capacity, and SSE policies.
- [x] Signal-driven REST serving with bounded Actix shutdown and an in-flight drain integration
  test.
- [x] Combined validated path/query/typed-header/JSON request extraction without ambiguous field
  precedence.
- [x] Supervised, non-overlapping periodic execution with failure reporting and bounded shutdown.
- [x] Validated route-path and URL-encoded form extractors.
- [x] Typed validated header extraction, stable JSON extraction errors, and configuration-driven
  JSON/form limits.
- [x] Redis consumer-group cursor updates corresponding to go-zero v1.10.3's `XGroupSetID`.
- [x] Server-sent events with IDs, retry hints, multiline data, heartbeat comments, and
  anti-buffering headers.
- [x] Protocol-aware unary gRPC circuit breaking and adaptive server load shedding wrappers.
- [x] Signal-aware service groups with fail-fast cancellation and bounded graceful shutdown.
- [x] Feature-gated etcd configuration/discovery, Kubernetes EndpointSlice discovery, MongoDB,
  and cached SQL/Mongo record helpers (pending the P0 real-backend conformance gate).
