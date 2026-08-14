# Changelog

All notable changes to rust-zero are documented in this file. The project follows Semantic
Versioning, with the pre-1.0 compatibility policy described in the README.

## [Unreleased]

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

[Unreleased]: https://github.com/kevwan/rust-zero/compare/v0.1.0-alpha.1...HEAD
[0.1.0-alpha.1]: https://github.com/kevwan/rust-zero/releases/tag/v0.1.0-alpha.1
