# slash0

Real-time visualization of global BGP route tables (IPv4 and IPv6) rendered
via WebGPU in Hilbert space. See `CLAUDE.md` for the architecture-of-record.

## Layout

```
crates/
  core/     no_std shared logic: radix trie, hilbert, wire types, timestamps
  server/   RIS Live ingest, WAL, WebSocket fanout (stub)
  client/   Rust -> WASM CPU, wgpu -> WebGPU (stub)
  shader/   fragment shader, rust-gpu (stub; excluded from workspace)
```

Everything except the shader lives in the top-level Cargo workspace. The
shader is excluded so its rust-gpu toolchain does not leak into
workspace-wide cargo commands.

## Build / test / lint

Workspace (core + server + client):

```
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Verify core's no-alloc feature slice compiles (this is what the shader will
see):

```
cargo build -p slash0-core --no-default-features
```

Shader crate (excluded from workspace, so build directly):

```
cargo check --manifest-path crates/shader/Cargo.toml
```

## Run

Server (currently just a stub main):

```
cargo run -p slash0-server
```

Client is not yet runnable -- wgpu/wasm-bindgen/trunk wiring is deferred
until the render loop is implemented.

## Pre-commit hooks

Managed via [`prek`](https://github.com/j178/prek). One-time setup:

```
prek install
```

Run against all files:

```
prek run --all-files
```

Hooks: `no-emoji`, `rustfmt`, `cargo clippy` (workspace), `cargo clippy`
(shader).

## Status

Scaffolding only. No trie, wire codec, RIS ingest, render pipeline, or
rust-gpu integration yet. See `CLAUDE.md` for what has been agreed on and
`/Users/Skyler/.claude/plans/greedy-tickling-marble.md` for the package
structure rationale.
