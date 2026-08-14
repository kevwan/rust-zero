# Stabilization and adoption gates

Runtime feature coverage is not the same as production maturity. rust-zero remains alpha until all
gates below have reviewable evidence. Missing evidence keeps the gate open; it must not be inferred
from demos, CI smoke tests, benchmarks, or go-zero feature parity.

## Alpha-exit gates

1. **API and upgrades**
   - Every published crate has a recorded public-API review.
   - Serialized configuration names and defaults are frozen or have a documented migration.
   - A release candidate has no unreviewed breaking public-API change from the preceding
     prerelease. Intentional breaks appear in `CHANGELOG.md` with before/after migration examples.
   - Patch releases remain source compatible. Before 1.0, a minor release may break APIs only with
     release notes; one-prerelease deprecation is preferred when safety does not require an
     immediate break.
2. **Continuous verification**
   - The MSRV, stable all-feature, documentation, packaging, and real-backend jobs remain green for
     30 consecutive days before promotion, excluding documented provider outages and cancelled
     superseded runs.
   - No unresolved critical/high security advisory applies to shipped code or enabled production
     dependencies.
3. **Sustained soak and fault evidence**
   - At least one 24-hour release-build campaign is retained for each supported architecture used
     in the maturity claim, plus a 72-hour Linux release-candidate campaign.
   - Each campaign completes with zero panics, deadlocks, invariant failures, lost queue messages,
     incomplete discovery snapshots, or failed overload recovery checks.
   - After the first warm hour, peak RSS may grow by no more than the larger of 10% or 64 MiB.
     No workload's recorded minimum throughput may fall below 50% of its same-host baseline, and
     no recorded maximum p99 may exceed 2x that baseline during the campaign.
   - A qualifying campaign records revision, compiler, target, configuration, hardware, UTC start
     and end, raw JSON, and every operator interruption. Restarts invalidate the duration.
4. **Fault recovery**
   - Release-candidate evidence includes repeated dependency refusal/recovery, malformed discovery
     updates, endpoint churn, overload, queue saturation, and graceful termination while requests
     are in flight. Recovery must not require a process restart unless the documented contract says
     otherwise.
5. **Independent production adoption**
   - At least one deployment owned by a user other than the rust-zero maintainer runs a published
     release for 30 consecutive days and serves at least one real REST or gRPC workload.
   - The deployment record identifies the version, enabled crates/features, approximate request
     volume, availability/error objective, observed incidents, rollback procedure, and an owner who
     approved publication. An anonymized organization is acceptable; fabricated or unverifiable
     claims are not.

## Public-API and upgrade review — 2026-08-14

The first review covered all re-exported items, configuration types, error types, feature flags,
and framework types exposed by the six published crates.

| Crate | Review result | Compatibility risk retained for alpha |
| --- | --- | --- |
| `rust-zero-core` | Builders and typed primitives are coherent; external adapters remain feature-gated and `do_command` is an explicit Redis escape hatch | Several configuration structs have public fields and several enums are exhaustive, so adding fields/variants can break source consumers |
| `rust-zero-rest` | Standard assembly provides one documented path while raw Actix composition remains possible | Public APIs intentionally expose Actix request, response, body, and middleware types; Actix major upgrades can therefore require a rust-zero minor migration |
| `rust-zero-rpc` | Generated-client-independent builders and status/error boundaries are documented | Tonic/Tower service types and public status snapshots expose dependency and struct-shape changes; snapshot fields must not be added casually |
| `rust-zero-gateway` | Configuration-driven proxy/transcoding is the stable entry point | Descriptor/reflection and dynamic protobuf types expose prost/tonic compatibility constraints |
| `rust-zero-mcp` | Protocol version and transport selection are explicit and legacy compatibility is opt-in | MCP specification changes may add protocol variants; handlers must continue receiving unknown-method errors rather than relying on exhaustive matching |
| `rust-zero-mapreduce` | The small functional surface has no external framework types | Closure/future bounds and error ownership are the principal source-compatibility constraints |

Review decisions:

- Keep all crates on one version and publish migration notes once for the workspace.
- Treat serialized configuration keys/defaults as a compatibility surface, not an implementation
  detail. New optional keys should preserve old behavior.
- Prefer additive builder methods over new required constructor arguments.
- Do not re-export more dependency types when an owned stable type can express the contract.
- Before beta, audit which exhaustive public enums and public-field structs can become
  `#[non_exhaustive]`; that change itself is source-breaking and belongs in a documented alpha
  minor release.
- Add automated API-diff enforcement before beta. Until then, release review compares generated
  documentation and the public re-export lists against the previous published tag.

## Current evidence

- Reproducible performance/fault baselines and raw results are retained under
  `benchmarks/results/`.
- The benchmark binary supports a same-process soak mode, and CI runs the ten-second smoke profile
  on every change. Smoke results validate the harness only and do not satisfy the duration gate.
- Real Redis, SQL, MongoDB, etcd, Kubernetes, REST, gRPC, gateway, and MCP integration tests are in
  CI.
- No qualifying 24/72-hour campaign or independent 30-day production deployment is recorded yet.

## Recording new evidence

Store immutable soak artifacts under `benchmarks/results/soak-<architecture>-<date>/` and link them
from this section. Production adopters may open a PR adding a deployment record under
`deployments/`; secrets, customer data, internal hostnames, and proprietary architecture are not
required. The project only changes its maturity claim in a reviewed release PR after every gate is
checked.
