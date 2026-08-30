# Changelog

All notable changes to rust-zero are documented in this file. The project follows Semantic
Versioning, with the pre-1.0 compatibility policy described in the README.

## [Unreleased]

## [0.1.0-alpha.2] - 2026-08-21

- Upgraded the public gRPC/protobuf stack from Tonic 0.12 and Prost 0.13 to Tonic 0.14 and
  Prost 0.14, aligned the etcd, RPC, gateway, reflection, health, and generated-code dependency
  graph, and moved protobuf generation to `tonic-prost-build`. Downstream build scripts must use
  `tonic_prost_build::configure`, and generated services must depend on `tonic-prost` 0.14.
- Added the `rust-zero` CLI with non-destructive REST project creation, protobuf build wiring,
  deterministic OpenAPI route stubs, and Docker/Kubernetes deployment scaffolding.
- Added discovery-aware gRPC consistent-hash channels with request affinity, weighted endpoint
  rings, health filtering, subsetting, atomic snapshot replacement, and round-robin fallback for
  requests without a hash key.
- Improved the deterministic consistent-hash avalanche for sequential keys. Existing
  `ConsistentHash` assignments may move once when upgrading to this release.
- Added opt-in SSE slow-consumer detection and disconnection with validated thresholds and
  bounded-cardinality Prometheus decision metrics.
- Added weekly dependency-policy enforcement and release-to-release public API compatibility
  checks, including documented advisory exceptions for unavoidable or offline-only transitive
  dependencies.
- Updated HTTP and profiling dependencies, removed a direct deprecated PEM parser, and migrated
  TLS key/certificate loading to Rustls' maintained PKI types.
- Added opt-in, CPU-triggered continuous profile export to Pyroscope with validated and redacted
  configuration, application tags, basic authentication, bounded sampling windows, and graceful
  final-profile flushing.
- Added objective stabilization gates, a public-API/upgrade review, and a repeated
  same-process soak/fault mode with invariant enforcement, RSS growth, and CI smoke coverage.
- Expanded the typed Redis API with scans, bitmaps, HyperLogLog, score/rank sorted-set operations,
  blocking and atomic list/set moves, unlink, stream inspection, and transactional pipelines.
- Added configurable, randomized, low-churn client-side discovery subsetting for large endpoint
  fleets, including repeatable seeds, complete-snapshot status, and an explicit connect-all opt-out.
- Added stock mixed-protocol gateway configuration for HTTP proxy and descriptor/reflection-driven
  gRPC routes, including explicit/annotated bindings, TLS, bearer auth, static balancing, and live
  etcd discovery.
- Added explicitly selectable MCP 2024-11-05 HTTP+SSE compatibility alongside the default
  2025-03-26 Streamable HTTP transport, with session-specific legacy message endpoints and
  protocol-version coverage.
- Added validated configuration-driven CORS to the standard REST and serverless stacks, including
  explicit origins, methods, request/response headers, credentials, cache age, and preflight mode.
- Added exact-method and service-level gRPC server timeouts with global fallback, plus optional
  configuration-driven etcd registration, lease renewal, and graceful withdrawal.

## [0.1.0-alpha.1] - 2026-08-10

First public prerelease.

- Added configuration-driven REST, gRPC, gateway, and MCP server runtimes with bounded graceful
  shutdown.
- Added shared authentication, observability, discovery, resilience, load-shedding, caching,
  queue, executor, and external-store primitives.
- Added HTTP-to-gRPC transcoding, static/serverless REST handling, content encryption, and
  supervised background work.
- Established Rust 1.89 as the minimum supported Rust version and added reproducible performance,
  fault, and real-backend coverage.

[Unreleased]: https://github.com/kevwan/rust-zero/compare/v0.1.0-alpha.2...HEAD
[0.1.0-alpha.2]: https://github.com/kevwan/rust-zero/compare/v0.1.0-alpha.1...v0.1.0-alpha.2
[0.1.0-alpha.1]: https://github.com/kevwan/rust-zero/releases/tag/v0.1.0-alpha.1
