# v1 Apple M3 Max baseline

- Captured: 2026-08-14 in an otherwise idle local session
- Hardware: MacBook Pro (Mac15,9), Apple M3 Max, 16 CPU cores (12 performance, 4 efficiency)
- Memory: 64 GiB
- OS: macOS 26.6.1 (25G76), Darwin 25.6.0 arm64
- Power: AC power, fully charged, default automatic energy mode (`powermode 0`)
- Virtualization: none
- Rust: rustc 1.97.1 (8bab26f4f 2026-07-14), aarch64-apple-darwin
- Go: go1.26.3 darwin/arm64
- Frameworks: rust-zero 0.1.0-alpha.1 worktree after `321512a`; go-zero v1.10.3
- Workload: `benchmarks/config/v1.toml`, release/trimmed builds, loopback transports
- Sampling: one complete warm-up run discarded, followed by five retained runs per framework;
  frameworks alternated during collection

The machine identifiers, serial number, and hardware UUID are intentionally omitted. Raw reports
record `working-tree-after-321512a` because the benchmark identity fields and companion runner were
part of the uncommitted baseline change being measured.
