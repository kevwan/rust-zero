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

for package in "${packages[@]}"; do
  if [[ "${package}" == rust-zero-core || "${package}" == rust-zero-mapreduce ]]; then
    cargo package "${package_options[@]}" --package "${package}"
  else
    # Before the first publish, resolve the normalized registry dependency to the exact local core
    # source. The CLI patch does not become part of the archive; after core is published the same
    # package command also succeeds without it.
    cargo package "${package_options[@]}" \
      --config "patch.crates-io.rust-zero-core.path='${core_path}'" \
      --package "${package}"
  fi
done
