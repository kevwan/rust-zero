# go-zero parity backlog

This backlog tracks production capabilities still needed after the
[go-zero v1.10.3](https://github.com/zeromicro/go-zero/tree/v1.10.3) runtime audit on
2026-08-09. A [follow-up comparison](https://github.com/zeromicro/go-zero/compare/f7805d5e322361f65561e8f562121b35404593a3...91a4cdbaf4e987f1c44ab14fb639756f213328f0)
from the recorded audit point through `91a4cdba` found only dependency changes on upstream
`master`; v1.10.3 remains the runtime baseline. Equivalent
Rust/Actix/Tonic behavior is the goal; identical Go APIs are not. `goctl`, API/protobuf/model/client
generation, templates, and deployment scaffolding remain an intentional developer-experience
boundary rather than a runtime-parity claim.

## Reopened parity gaps

These items were identified after the original matrix was marked covered. Until the P0 items are
complete, the project claims **broad runtime coverage**, not full runtime parity.

### P0 — runtime parity blockers

- [x] **End-to-end TLS and mTLS.**
  - [x] The standard REST listener accepts validated PEM certificate/key material and an optional
    client CA, builds a Rustls 0.23 HTTPS acceptor, redacts private keys from debug output, and
    completes a CA-signed mutual-TLS handshake test.
  - [x] Tonic TLS is enabled for standard gRPC server/client assembly with server identity, client
    trust roots, optional client identity, domain-name verification, redacted configuration, and a
    live unary mTLS lifecycle test.
  - [x] Authenticated etcd connections support CA trust, optional client certificate/key, and a
    server-name override. Validation tests cover incomplete identities, while the real-backend test
    accepts `RUST_ZERO_ETCD_TLS_ENDPOINT` and PEM environment variables for TLS/mTLS CI coverage.
- [x] **Result-aware server-side circuit breaking.** Install independent breaker state per REST
  route and gRPC method by default, retain permits through streaming completion, and classify
  protocol results and cancellations without allowing one route/method to poison another.
  - [x] The standard gRPC server stack installs a configurable rolling breaker per method, observes
    final trailers and stream cancellation, rejects open methods with `UNAVAILABLE`, and supports
    explicit opt-out or selection of the consecutive-failure policy.
  - [x] The standard REST and serverless stacks install configurable rolling breakers per stable
    method/route pattern, classify 5xx responses and body errors as failures, retain permits through
    response streaming, record early body drops as cancellation, expose rejection metrics, isolate
    route state, and support explicit opt-out or the consecutive-failure policy.
- [x] **Complete configuration-driven gRPC lifecycle.**
  - [x] Support validated per-method server timeouts with exact-method and service-level matching
    while retaining the global fallback.
  - [x] Add optional automatic etcd registration/lease renewal and withdrawal to `RpcServerConfig`,
    using the existing `EtcdServiceLease` and coordinating it with graceful startup/shutdown.

### P1 — standard assembly

- [ ] **Configuration-driven CORS.** Add validated origin, method, header, credential, max-age, and
  preflight settings to `RestServerConfig` and install them in the standard REST/serverless stack.

### P2 — compatibility

- [ ] **Legacy MCP SSE transport.** Add the MCP 2024-11-05 SSE transport alongside the implemented
  2025-03-26 Streamable HTTP transport for older clients, with explicit transport selection and
  protocol-version tests.

## P0 — framework-level parity

- [x] **Baseline configuration-driven REST server assembly.** The standard stack, declarative
  route-policy layer, and end-to-end lifecycle proof from the original audit are present; the
  reopened TLS, CORS, and server-breaker extensions are tracked above.
  - [x] Add a deserializable, validated `RestServerConfig` with production defaults.
  - [x] Assemble logging, recovery, request IDs, tracing, metrics, security headers, timeout,
    decompression/body limits, concurrency shedding, listener binding, and bounded Actix shutdown.
  - [x] Accept an Actix route configuration callback so ordinary routes and scoped groups do not
    require manual server or middleware wiring.
  - [x] Add declarative route groups and per-route JWT/timeout/body-limit/priority/SSE overrides.
  - [x] Prove signal-driven graceful draining with an in-flight request integration test.
- [x] **Baseline configuration-driven gRPC assembly.** Transport configuration and the
  generated-service-independent interceptor and lifecycle layer from the original audit are
  present; the reopened TLS, per-method timeout, and automatic registration work is complete.
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
- [x] **Static/serverless serving and custom route middleware.** Add embedded/static file fallback,
  a prebuilt serverless handler, and application-defined middleware lists on declarative route
  groups. Prefixes and per-route JWT/timeout/body-limit/priority/SSE settings are present.
  - [x] Add ordered named application middleware on declarative groups, including async
    short-circuiting, response wrapping, duplicate/missing-name validation, and route isolation.
  - [x] Add a traversal-safe static-directory and embedded-asset fallback.
  - [x] Add a prebuilt serverless request handler that reuses the standard REST stack.
- [x] **Automatic RPC transport metrics.** Reusable gRPC client/server layers cover request counts,
  latency, in-flight calls, final trailer status, transport/body errors, cancellation, and bounded
  method cardinality for unary and streaming calls. REST server/client request and in-flight
  metrics plus timeout, concurrency-shed, rate-limit, and client-circuit rejection outcomes are
  present. The standard generated-service-independent client and server stacks install them.
- [x] **Model-cache semantics.** Add multi-key index-to-primary mappings, randomized expirations,
  cache-not-found policy, shared invalidation after mutations, cache statistics, and Redis cluster
  cache nodes matching go-zero's `sqlc`/`cache` behavior.
  - [x] Support separate positive/not-found TTLs, optional negative caching, bounded TTL jitter,
    cache statistics, and race-safe invalidation in both SQL and Mongo wrappers.
  - [x] Add secondary-index-to-primary mappings and invalidate every related key atomically after
    insert, update, or delete.
  - [x] Add Redis-backed cache nodes with standalone/cluster routing, serialization errors,
    not-found sentinels, and cross-process invalidation tests.
- [x] **Store helper depth.** Complete the remaining SQL, Mongo, and Redis operational helpers.
  - [x] Add timeout-aware Redis pipelines and Lua scripts plus stream append/read,
    consumer-group create/destroy/read/cursor, pending/claim, acknowledgement, and deletion helpers.
  - [x] Add reconnecting Redis channel/pattern subscriptions with bounded delivery and explicit
    lag/closure behavior.
  - [x] Add SQL bulk insert, typed query/execute instrumentation, and standardized not-found
    errors.
  - [x] Add Mongo native bounded bulk insertion, standardized not-found errors, cache-aware
    insert/update/replace/delete helpers, cardinality-bounded metrics, and opt-in tracing.
  - [x] Add equivalent operation metrics and tracing hooks to the Redis adapter so all three
    external-store families share the same observability baseline.
- [x] **Queue runtime.** Supervised bounded queues provide configurable worker pools,
  pause/resume, bounded shutdown, surfaced worker panics, per-message lifecycle/failure events,
  balanced failover pushing, fan-out pushing, and Prometheus processing metrics.
- [x] **MCP server runtime.** Add Streamable HTTP transport, tool/resource/prompt registration,
  session lifecycle, protocol-compliant errors, graceful shutdown, and an opt-in bridge that makes
  HTTP headers, query values, and path parameters available to handlers. Cover initialization,
  stateless/stateful sessions, notifications, cancellation, and reconnect behavior in integration
  tests.
  - [x] Add the protocol core and stateless 2025-03-26 Streamable HTTP endpoint with validated
    configuration, JSON/SSE responses, tool/resource/prompt registration and dispatch,
    protocol-compliant errors, request timeouts, notifications, origin protection, and HTTP
    header/query/path metadata projection.
  - [x] Add stateful session IDs, GET event streams, resumable event cursors, explicit DELETE
    termination, request cancellation, and reconnect integration tests.
  - [x] Integrate MCP startup and bounded graceful draining with the standard service lifecycle
    and add an end-to-end example covering tools, resources, and prompts.
- [x] **Executor family.** Byte-weighted chunk batching, coalesced delayed execution, and
  threshold/"less" execution are present. Chunk and delay executors provide explicit shutdown;
  delayed and periodic jobs surface worker failures and support bounded shutdown.
- [x] **Authentication parity.** Configurable dot-path JWT claim projection, time-window-bounded HMAC
  request signatures, current/previous signing keys, and stable `auth_*` failures are shared by
  REST and gRPC.
- [x] **Logging operations.** Bounded non-blocking output, application-provided local/remote writer
  hooks, dropped-record accounting, rotated-file retention/compression, and transport-aware REST
  and gRPC slow-call classification are present.
  - [x] Add bounded non-blocking writer threads that shed instead of stalling callers, expose a
    dropped-record count, and accept application-provided remote writers.
  - [x] Add stable HTTP slow-call fields covering the transport, route template, elapsed time,
    configured threshold, and slow classification.
  - [x] Add retention and optional compression for daily and size-rotated files.
  - [x] Add equivalent method/status/deadline-aware slow-call fields to the gRPC stack.

## P2 — completeness and operational polish

- [x] **Discovery readiness and balancing.** Weighted endpoint metadata, active transport probes,
  and observable empty/ready/degraded channel states are present.
  - [x] Add bounded endpoint weights and arbitrary metadata to the backend-neutral subscription
    API while keeping string-only discovery implementations source-compatible.
  - [x] Make dynamic gRPC balancing honor relative weights, actively remove failed probes, restore
    recovered endpoints, and expose discovered/available/rejected counts.
  - [x] Add configurable exponential reconnect backoff with jitter to the etcd and Kubernetes
    resolver loops, including deterministic reconnect tests.
  - [x] Feed discovery channel status into the standard gRPC health service and dev-server health
    aggregation without requiring application polling.
- [x] **Production dev diagnostics.** The dev server provides feature-gated sampling flamegraphs,
  Tokio task/runtime and process allocator-memory diagnostics, constant-time bearer protection,
  secret-redacted configuration, bounded sampling settings, and validated private-only binds.
- [x] **REST content encryption.** Opt-in request decryption and response encryption use a
  versioned, authenticated AES-256-GCM envelope, bounded buffering, explicit key IDs, a pluggable
  current/retained-key provider, rotation coverage, and fail-closed handling for malformed input
  and incompatible streaming responses.
- [x] **Gateway operations.** Configuration-driven startup, named upstream policy chains,
  streaming-safe graceful draining, and a runnable mixed-protocol deployment are present.
  - [x] Add validated JSON/JSON5/TOML/YAML configuration loading for listener, timeout, body-limit,
    worker, route, and upstream settings, plus a signal-aware runnable binary.
  - [x] Add named per-upstream middleware/policies.
  - [x] Add bounded Actix connection draining and prove an in-flight proxy call completes.
  - [x] Stream upstream response bodies without whole-response buffering and cover delayed chunks.
  - [x] Add an end-to-end deployment example for mixed HTTP and gRPC upstreams.
- [x] **Release operations.** Publish a compatibility policy and Rust 1.89 MSRV, enforce locked
  minimal/adapter/telemetry/all-feature and rustdoc CI, and provide compiled runnable deployment
  examples for etcd, Kubernetes, OTLP telemetry, and Redis/SQL/MongoDB stores.

## Recommended execution order

The reopened P0 runtime items are complete. Add configurable CORS next and legacy MCP SSE only
where older-client compatibility is required. Durable brokers and the `goctl` developer toolchain
remain intentional ecosystem boundaries.

## Remaining catch-up items

`goctl` and all other code generation remain outside this runtime catch-up scope. MCP Streamable
HTTP is covered; only its legacy SSE compatibility transport remains above.

### P0 — runtime semantics

- [x] **Redis-backed distributed rate limiting.** Atomic Redis/Lua token and keyed-period
  limiters so quotas are shared across service instances, while retaining a bounded process-local
  rescue limiter when Redis is unavailable. Async cancellation-safe operations, Redis-server-time
  aligned period boundaries, stable allowed/hit/over-quota outcomes, request-driven single-probe
  recovery monitoring, and real standalone and clustered Redis tests are present.
- [x] **Rolling adaptive circuit breaker.** Add a go-zero-equivalent rolling-window breaker mode
  that derives a probabilistic drop ratio from recent accepted and total requests, guarantees
  occasional probe traffic, and records success, failure, rejection, cancellation, and protocol
  outcomes correctly under concurrency. Preserve the existing consecutive-failure breaker as an
  explicitly selectable policy and add deterministic fault-pattern tests for both modes.
- [x] **CPU- and throughput-aware load shedding.** A production shedder mode combines
  process CPU pressure, rolling maximum throughput, minimum response time, current and smoothed
  in-flight work, and a post-overload cooldown window. It is integrated into the standard REST and
  gRPC server stacks, retains permits through response-stream completion, and covers saturation,
  recovery, sparse traffic, and concurrent permit completion.

### P1 — production confidence and adoption

- [x] **Reproducible performance and fault benchmarks.** Versioned, configurable workloads cover
  REST and gRPC throughput and tail latency, allocation/memory behavior, circuit breaking under
  partial dependency failure, overload recovery, large discovery snapshots, and queue saturation.
  The runner emits raw JSON with configuration and build identity, while the benchmark guide
  documents hardware capture, commands, result retention, and regression comparison methodology.
- [x] **Publishable, versioned crates.** Give every public crate a stable rust-zero-prefixed package
  name and complete license, repository, documentation, and description metadata; add registry
  versions to workspace path dependencies; verify independent `cargo package` builds; publish API
  documentation and a changelog; and cut the first tagged prerelease without weakening the stated
  Rust 1.89 MSRV policy.
  - [x] Assign the six public crates `rust-zero-*` package names and a shared `0.1.0-alpha.1`
    version while preserving the existing short Rust library import names.
  - [x] Add SPDX license, repository, homepage, README, description, category, keyword, and
    docs.rs metadata, registry versions on every path dependency, and explicit non-publishable
    demo/benchmark packages.
  - [x] Build and verify every normalized package archive independently in CI, including the
    unpublished-core bootstrap used only before the first registry release.
  - [x] Add the public crate/documentation index, changelog, and repeatable release checklist.
  - [x] Publish the six crates in dependency order, verify their live docs.rs builds, and create
    the signed `v0.1.0-alpha.1` tag from the release commit.
- [x] **Durable broker ecosystem boundary.** External messaging is not a rust-zero parity claim.
  Applications select Kafka, RabbitMQ, or another client according to the required delivery and
  operational semantics, and may hand decoded work to rust-zero's supervised in-process queue.
  This avoids a lowest-common-denominator broker abstraction and keeps broker-specific retry,
  acknowledgement, offset, topology, and transaction behavior visible to applications.

## Recently completed

- [x] End-to-end TLS/mTLS configuration for the standard REST and gRPC server/client transports and
  authenticated etcd, including private-key redaction, PEM/identity validation, CA and hostname
  verification, direct REST Rustls and live gRPC mutual handshakes, and opt-in real-etcd TLS CI.
- [x] Default result-aware server circuit breaking with isolated REST route and gRPC method state,
  configurable rolling/consecutive policies, streaming-body/trailer lifetime permits, cancellation
  handling, stable overload responses, protection metrics, opt-out controls, and isolation tests.
- [x] A versioned benchmark runner covering live REST/gRPC throughput and tail latency, allocator
  behavior and peak RSS, partial-failure circuit breaking, overload recovery, 10,000-endpoint
  discovery snapshots, and saturated supervised queues, with raw JSON output and a reproducibility
  guide.
- [x] Process-CPU and rolling-throughput-aware production shedding with learned minimum-latency
  capacity, smoothed and current in-flight checks, bounded cooldown recovery, shared REST worker
  state, gRPC stack integration, stream-lifetime permits, and deterministic saturation, sparse-
  traffic, recovery, and concurrency tests.
- [x] Selectable consecutive and rolling adaptive circuit breakers with configurable bucketed
  histories, go-zero-compatible accepted/total drop ratios, probabilistic rejection, bounded
  recovery probes, deterministic fault patterns, concurrent outcome snapshots, and cancellation-
  aware HTTP/gRPC transport feedback.
- [x] Atomic standalone/clustered Redis token and aligned keyed-period limiters with stable quota
  outcomes, bounded local outage rescue, single-caller recovery probing, observable failure and
  recovery counters, concurrent quota tests, and real-backend CI coverage.
- [x] Opt-in authenticated REST body encryption with a documented AES-256-GCM/base64 envelope,
  method/URI/status-bound authentication, bounded request/response buffering, explicit key IDs,
  current/previous key rotation, pluggable providers, standard-server integration, and tamper tests.
- [x] Rust 1.89 MSRV and compatibility policy, locked minimal/adapter/telemetry/all-feature CI,
  warning-free rustdoc with six compiled feature examples, and runnable etcd, Kubernetes, OTLP,
  and external-store deployment examples.
- [x] A self-contained mixed-protocol gateway deployment with an HTTP reverse-proxy route,
  descriptor-driven JSON-to-gRPC unary and streaming routes, configurable listener addresses, and
  coordinated signal-driven shutdown of the gateway and both sample upstreams.
- [x] Ordered named gateway middleware on configured upstream pools, with outbound request
  mutation, short-circuiting, response wrapping, route isolation, and pre-serve validation for
  blank, duplicate, or unregistered policy names.
- [x] Validated file-backed gateway configuration and runnable signal-aware startup, true streamed
  HTTP proxy responses with cumulative limits, and bounded graceful draining proven with an
  in-flight request.
- [x] Authenticated internal diagnostics with private-interface enforcement, Tokio task/runtime
  metrics, process allocator-memory high-water reporting, and bounded opt-in sampling flamegraphs.
- [x] Configurable capped exponential resolver reconnect with bounded jitter for etcd and
  Kubernetes, etcd relisting after broken or compacted watches, and direct discovery-readiness
  projection into Tonic health and the shared dev-server health aggregate.
- [x] Backward-compatible structured discovery endpoints with bounded relative weights and
  arbitrary metadata, weighted dynamic gRPC channel entries, opt-in active HTTP/2 probes that
  remove and restore peers, and watchable empty/ready/degraded counts.
- [x] Stateful MCP sessions with idle expiry, resumable event IDs and bounded replay, long-lived GET
  streams, explicit DELETE termination, cancellation of in-flight requests, reconnect coverage,
  standalone startup, graceful draining, shared service supervision, and a runnable example.
- [x] Stateless MCP Streamable HTTP protocol core with validated configuration, deterministic
  tool/resource/prompt registries, initialization and notification handling, JSON-RPC errors,
  JSON/SSE responses, origin checks, deadlines, and request metadata projection.
- [x] Daily log retention and opt-in gzip compression for daily and size-rotated files, plus final-
  trailer-aware gRPC server logs with stable method, status, deadline, elapsed, slow, and trace
  fields.
- [x] Shared REST/gRPC authentication primitives with configurable JWT claim projection,
  current/previous HS256 secrets, method/target-bound HMAC signatures with bounded clock skew,
  caller key propagation, and stable machine-readable failure codes.
- [x] Cardinality-bounded Redis command metrics and opt-in OpenTelemetry spans, including distinct
  timeout outcomes and bounded labels for application-supplied raw commands.
- [x] MongoDB typed query/execute helpers with stable not-found errors, native bounded bulk
  insertion, Prometheus operation metrics, opt-in OpenTelemetry spans, and cache-aware
  insert/update/replace/delete helpers that invalidate primary and secondary keys after success.
- [x] Typed SQL query/execute helpers with standardized database and not-found errors,
  cardinality-bounded Prometheus metrics, opt-in OpenTelemetry spans, and database-neutral bounded
  bulk insertion across caller-supplied SQLx statements.
- [x] Namespace-aware Redis channel and pattern subscriptions with bounded delivery, explicit lag,
  disconnect/reconnect/closure events, exponential reconnect backoff across seed endpoints, and
  deterministic shutdown.
- [x] Timeout-aware Redis pipeline and Lua execution plus namespaced stream append/read,
  consumer-group lifecycle/read/cursor, pending/claim, acknowledgement, and deletion helpers.
- [x] Redis-backed model caching over standalone or clustered stores, with tagged positive and
  not-found JSON entries, separate jittered TTLs, per-process single-flight loading, statistics,
  explicit serialization failures, and cluster-safe cross-process invalidation.
- [x] Bounded SQL/Mongo secondary-to-primary cache mappings with distinct key types, negative
  index caching, single-flight loads, shared primary records/statistics, and atomic mutation-wide
  invalidation of learned and newly introduced index keys.
- [x] A reusable socket-free REST handler that runs platform-neutral requests through the same
  routes, extractors, application middleware, protection, observability, response policy, and
  static fallback as the listener-based server.
- [x] Opt-in REST static fallback with embedded assets, canonicalized directory roots, index-file
  handling, MIME types, `GET`/`HEAD` semantics, and traversal/symlink-escape protection.
- [x] Ordered, named application middleware for declarative REST route groups, with full async
  wrapping/short-circuit behavior and validation of duplicate or unregistered names.
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
