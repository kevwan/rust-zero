#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repository_root}"

package_options=(--exclude-lockfile)
if [[ "${1:-}" == "--allow-dirty" ]]; then
  package_options+=(--allow-dirty)
  shift
fi
if (( $# != 0 )); then
  echo "usage: $0 [--allow-dirty]" >&2
  exit 2
fi

packages=(
  rust-zero-core
  rust-zero-rest
  rust-zero-rpc
  rust-zero-gateway
  rust-zero-mapreduce
  rust-zero-mcp
)

core_path="${repository_root}/core"
rest_path="${repository_root}/rest"
rpc_path="${repository_root}/rpc"

for package in "${packages[@]}"; do
  case "${package}" in
    rust-zero-rest|rust-zero-rpc)
      cargo package "${package_options[@]}" \
        --config "patch.crates-io.rust-zero-core.path='${core_path}'" \
        --package "${package}"
      ;;
    rust-zero-gateway)
      # Before the first coordinated publish, resolve normalized registry dependencies to the
      # exact local workspace sources. These CLI patches do not become part of the archive.
      cargo package "${package_options[@]}" \
        --config "patch.crates-io.rust-zero-core.path='${core_path}'" \
        --config "patch.crates-io.rust-zero-rest.path='${rest_path}'" \
        --config "patch.crates-io.rust-zero-rpc.path='${rpc_path}'" \
        --package "${package}"
      ;;
    *)
      cargo package "${package_options[@]}" --package "${package}"
      ;;
  esac
done
