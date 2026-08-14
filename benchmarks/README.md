# Reproducible benchmarks

This package contains the versioned `v1` workload used to track rust-zero throughput, tail
latency, allocation/memory behavior, and failure recovery. It intentionally uses a standalone
binary instead of a micro-benchmark harness: the REST and gRPC cases traverse real loopback
transports, and every workload emits the same machine-readable schema.

## Run

Use an otherwise idle machine, performance power mode, and a release build. Record the exact
revision and compiler in the output:

```sh
RUST_ZERO_GIT_REVISION="$(git rev-parse HEAD)" \
RUST_ZERO_RUSTC="$(rustc -Vv | tr '\n' ' ')" \
cargo run --release --locked -p rust-zero-benchmarks -- benchmarks/config/v1.toml \
  > benchmarks/results/v1-$(uname -m)-$(uname -s | tr A-Z a-z).json
```

Run at least five times after one discarded warm-up run. Keep every raw JSON file; compare the
median `operations_per_second`, `p95_us`, `p99_us`, `allocations`, and `allocated_bytes`. Treat a
change as a regression when the five-run median exceeds the versioned limits in
`thresholds/v1.json`. Transport results are loopback-only and should only be compared on the same
machine and OS configuration. The default limits are 10% lower throughput or 15% higher p95/p99,
allocation count, or allocated bytes; the timer-sensitive saturated-queue workload allows 20%
throughput, 25% tail-latency, and 30% allocation movement.

The pinned go-zero v1.10.3 companion uses the same configuration and output schema:

```sh
go build -trimpath -o /tmp/go-zero-benchmark ./benchmarks/go-zero
RUST_ZERO_GIT_REVISION="$(git rev-parse HEAD)" \
  /tmp/go-zero-benchmark benchmarks/config/v1.toml \
  > benchmarks/results/<host>/go-run1.json
```

Discard one warm-up from each executable, then retain at least five measured files from each. Build
the medians and check a later same-host candidate with:

```sh
python3 benchmarks/compare.py summarize --minimum-samples 5 \
  --output benchmarks/results/<host>/baseline-summary.json \
  benchmarks/results/<host>/*-run*.json
python3 benchmarks/compare.py check \
  --baseline benchmarks/results/<host>/baseline-summary.json \
  --candidate /tmp/candidate-summary.json \
  --thresholds benchmarks/thresholds/v1.json
```

The workloads are:

- `rest_transport` and `grpc_transport`: concurrent echo traffic over TCP;
- `circuit_breaker_partial_failure`: deterministic partial dependency failure and rejection;
- `overload_recovery`: admission rejection at capacity followed by immediate recovery;
- `large_discovery_snapshot`: cloning a 10,000-endpoint complete snapshot;
- `queue_saturation`: producer latency while a bounded supervised queue remains saturated.

The Go companion traverses go-zero's REST serverless route stack and Google circuit breaker. Its
gRPC case uses the same `grpc-go` transport underlying `zrpc`; the overload, complete-snapshot, and
bounded-queue harnesses intentionally use framework-neutral equivalents so both sides receive the
same deterministic inputs. The comparison is evidence for relative workload behavior, not a claim
that unlike APIs or allocators perform identical work.

The counting global allocator reports total allocation calls and requested bytes during each
measured interval. `peak_rss_kib` comes from `getrusage`; because it is a process high-water mark,
it is cumulative across workloads. The JSON configuration is embedded in every result.

## Soak and fault campaigns

Set `soak_duration_seconds` in a benchmark configuration to repeat all six workloads in one
process until the requested wall-clock duration has elapsed. Every cycle enforces transport
completion, overload rejection and recovery, complete discovery snapshots, and lossless queue
processing. The `soak` JSON object records cycle/operation totals, worst per-cycle p99 and
allocation values, minimum throughput, and peak-RSS growth across the campaign.

Run the ten-second smoke profile with:

```sh
cargo run --release --locked -p rust-zero-benchmarks -- benchmarks/config/soak-smoke.toml \
  > /tmp/rust-zero-soak-smoke.json
python3 benchmarks/validate_soak.py --minimum-seconds 10 /tmp/rust-zero-soak-smoke.json
```

The smoke profile catches panics, deadlocks, invariant failures, and schema regressions, but it is
not qualifying maturity evidence. Qualifying campaigns use a copied configuration with at least
86,400 seconds and follow the hardware, threshold, and retention rules in
[`STABILIZATION.md`](../STABILIZATION.md).

## Hardware record

Alongside raw output, record CPU model/core count, RAM, OS/kernel, power mode, compiler, target,
and whether the machine was virtualized. Useful commands are `uname -a`, `rustc -Vv`, and, on
Linux, `lscpu` plus `free -h`; on macOS use `system_profiler SPHardwareDataType` and `sw_vers`.

Commit representative raw runs under `results/` only when the hardware record is complete. Do not
compare absolute numbers across different hosts.
