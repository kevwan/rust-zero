# Proposal 0001: `.api` DSL and rust-zero REST codegen

Status: draft
Date: 2026-08-14
Crates: `rust-zero-ast`, `rust-zero-codegen`

## Summary

Add an optional developer-experience path that compiles a rust-native `.api`
contract into rust-zero REST source. This is not a runtime-parity item and does
not change `RestServer`. It is a separate, swappable backend: parse once, emit
files that sit on the existing Actix + extractor stack.

## Why this is a proposal

`FEATURE_PARITY.md` and `BACKLOG.md` currently exclude `goctl`, API generation,
templates, and scaffolding from the runtime claim. That boundary is correct for
production maturity. This work does not reopen that claim. It asks to land an
opt-in DX crate family beside the runtime, the same way `goctl` sits beside
go-zero rather than inside the server.

The design is large enough that a PR without a written contract would look like
an accidental `goctl` port. It is not.

## Goals

- Keep the HTTP contract in a `.api` file, not in Rust macros.
- Borrow go-zero's file / service / route skeleton, not Go types or `@`
  annotations.
- Keep parse and generate separate. The parser must not know Actix.
- Emit ordinary rust-zero REST code: serde types, `async fn` handlers, and
  `web::get().to(...)` registration into `RestServer`.
- Leave generated handler bodies editable. Overwrite contract and route files.

## Non-goals

- Runtime changes to `rest`, `core`, or the standard middleware stack.
- A proc-macro `#[api("greet.api")]` entry point. `quote` is used only to build
  token streams that are written to disk.
- Import resolution, semantic checking, or a full project scaffolder in this
  slice.
- OpenAPI YAML or client SDK backends. Those are later generators over the
  same AST.
- Claiming goctl parity.

## DSL

A file is `syntax`, optional `import` / `info`, then `struct` and `service`
blocks.

```
syntax = "v1"

info (
    title: "greet"
    desc: "minimal ping"
)

struct Resp {
    msg: String,
}

struct PayReq {
    #[json("order_id")]
    orderId: String,
}

#[server(prefix = "/v1", timeout = "3s")]
service greet {
    #[doc = "health"]
    get /ping -> Resp

    #[handler(createPay)]
    post /pay (PayReq) -> Resp
}
```

Locked surface:

- Types: `T ::= Name | [T] | {K: V} | T?`. Codegen maps these to `String` /
  `Vec` / `HashMap` / `Option`. No `map[string]int`, `[]T`, `*T`, or backtick
  tags.
- Routes: `get /ping -> Resp`, `post /pay (PayReq) -> PayResp`. Empty `()` and
  a trailing `;` are optional.
- Attributes use Rust shape and `=`, not go-zero `@` or `:`.
  - `#[server(group = "form", prefix = "/v1")]`
  - `#[doc = "text"]` or `#[doc(desc = "text", key = "value")]`
  - `#[handler(name)]`, optional; default name comes from the path
  - Field attrs: `#[path]`, `#[path("id")]`, `#[query]`, `#[form]`,
    `#[header("Authorization")]`, `#[json("user_id")]`. Stacked or one-liner.
    Default JSON name is the field name; no silent snake_case.
- `info (...)` stays a keyword block, not `#[info]`.

The parser (`rust-zero-ast`) tokenizes with logos and parses with lalrpop. It
records import paths and does not open those files. Comments are dropped.
Duplicate types and paths are not rejected at parse time.

## Codegen

`rust-zero-codegen` exposes:

```
fn generate(ast: &ApiFile) -> Vec<GeneratedFile>
```

Each file is a relative path plus source. The first backend writes a service
crate layout:

```
Cargo.toml                  # scaffold once; do not overwrite if present
src/main.rs                 # scaffold once; do not overwrite if present
src/types.rs                # generated, overwrite
src/routes.rs               # generated, overwrite
src/handlers/mod.rs         # generated, overwrite; omitted if no routes
src/handlers/<name>.rs      # scaffold; do not overwrite if present
```

A file with only types and no `service` / routes does not create `handlers/`.
`main.rs` then omits `mod handlers`.

`main.rs` builds `RestServerConfig::default()`, assigns
`routes::route_groups()`, and calls `RestServer::run(routes::configure)`.
That is the standard rust-zero stack (timeouts, shedding, per-route
breakers, metrics, tracing), not a bare Actix `HttpServer`. Handlers are
ordinary `async fn`s. Field locations become rust-zero extractors:

| `.api` attr | Generated extractor |
| --- | --- |
| `#[path]` / `#[path("id")]` | `ValidatedPath<T>` |
| `#[query]` | `ValidatedQuery<T>` |
| `#[header]` | `ValidatedHeader<T>` |
| `#[form]` | `ValidatedForm<T>` |
| default / `#[json]` | `ValidatedJson<T>` |

A request struct that mixes locations is split into `FooPath` / `FooQuery` /
`FooHeaders` / `FooForm` / `FooBody`. Path patterns `/users/:id` become
`/users/{id}`. `#[json("user_id")]` becomes `#[serde(rename = "user_id")]`.
`#[server]` maps onto `RouteGroupConfig`:

- `prefix` / `timeout` / `maxBytes` / `middleware` names become group policy
- jwt secrets and middleware implementations are not invented; names are
  emitted so `RestServer::with_route_middleware` / a later jwt config can
  fill them in
- registered Actix paths still include the prefix, matching
  `RoutePolicies` lookup

Generated Rust is assembled with `quote` / `syn` / `prettyplease`, then passed
through `rustfmt` when it is on PATH. Route groups keep only set fields:

```
let greet = RouteGroupConfig {
    routes: vec![RoutePolicyConfig::public("GET", "/ping")],
    ..RouteGroupConfig::default()
};
```

`RoutePolicyConfig::public` lives on the runtime type so generated code does
not invent a pile of `None`s. This is not a procedural macro.

`Cargo.toml` is a first-run scaffold. Three version classes stay separate:

1. The service crate version is the user's. Later writes must not overwrite
   an existing manifest.
2. `rust-zero-core` / `rust-zero-rest` follow the generator. Default is
   crates.io at `CARGO_PKG_VERSION`. A later `generate(ast, deps)` can switch
   that to `path` or `workspace = true`. Those pins do not belong in the DSL.
3. Direct `actix-web` / `serde` uses stay because generated files import them.
   Their ranges should be copied from `rust-zero-rest`, not typed by hand.
   Wiring that copy is deferred until a CLI exists.

## Why not a macro backend first

Our contract is the `.api` file. Generated `types.rs` and `routes.rs` need to
be reviewable diffs. Handler files are marked safe to edit. A
`#[api("greet.api")]` module would hide that output and point diagnostics at
the macro, not the service tree.

## Testing

Parser and codegen both use file-driven suites, not one Rust test per
statement.

- Parser: `ast/tests/testdata/parse_ok.txt` + `parse_err.txt`, with long
  inputs via `file: name.api`. Refresh with
  `UPDATE_AST=1 cargo test -p rust-zero-ast --test parser`.
- Codegen: `codegen/tests/testdata/*.api` and golden trees under the same
  stem. Refresh with
  `UPDATE_CODEGEN=1 cargo test -p rust-zero-codegen --test codegen`.

## Compatibility

New crates, additive workspace members. No change to published runtime APIs.
The runtime parity matrix stays unchanged: generation remains outside that
claim until the project explicitly expands it.

## Follow-ups

1. CLI / `build.rs` writer with overwrite policy for scaffold vs generated
   files. That writer is also the place to pass `generate(ast, deps)`.
2. Copy `actix-web` / `serde` version ranges from `rust-zero-rest` instead of
   hard-coding them in the manifest template.
3. Import graph + semantic checks (duplicate type, duplicate method+path).
4. Additional backends: OpenAPI YAML, client SDK.
