# Releasing rust-zero

The six public crates use one version and are published as a unit. A release commit must have a
clean worktree, pass CI on Rust 1.89 and stable, and contain matching versions in `Cargo.toml`,
`Cargo.lock`, and `CHANGELOG.md`.

## Prerelease checklist

1. Confirm the proposed `rust-zero-*` package names are available for the first release, or owned
   by the project for later releases, on crates.io.
2. Run `./scripts/package.sh`. It builds each normalized `.crate` archive independently. Before
   the first release, the script supplies a command-line-only patch for the unpublished
   `rust-zero-core`; the patch is never written to an archive.
3. Publish in dependency order, waiting for crates.io to index core before publishing dependants:

   ```bash
   cargo publish -p rust-zero-core
   cargo publish -p rust-zero-rest
   cargo publish -p rust-zero-rpc
   cargo publish -p rust-zero-gateway
   cargo publish -p rust-zero-mapreduce
   cargo publish -p rust-zero-mcp
   ```

4. Confirm that all six releases build on docs.rs and that their documentation links resolve.
5. Tag the exact release commit and push the tag only after every crate is visible:

   ```bash
   git tag -s v0.1.0-alpha.1 -m "rust-zero v0.1.0-alpha.1"
   git push origin v0.1.0-alpha.1
   ```

6. Create the GitHub release from the matching changelog entry. Do not reuse a published version
   or move an existing release tag.

## Maturity promotion

Changing the README status from alpha requires every gate in `STABILIZATION.md` to be satisfied by
linked, reviewable evidence. A normal crate release, green CI, a short smoke run, or feature parity
alone does not authorize a maturity claim. Record the evidence links and approval in the release
PR; if any time-based or independent-deployment gate is missing, keep the alpha designation.
