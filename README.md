# slash0

Real-time visualization of global BGP route tables (IPv4 and IPv6) rendered
via WebGPU in Hilbert space. See `CLAUDE.md` for the architecture-of-record.

## Layout

```
crates/
  core/     no_std shared logic: radix trie, hilbert, wire types, timestamps
  server/   RIS Live ingest, WAL, WebSocket fanout (stub)
  client/   Rust -> WASM CPU, wgpu -> WebGPU (excluded from workspace)
  shader/   fragment shader, rust-gpu (stub; excluded from workspace)
```

Core and server live in the top-level Cargo workspace. Client and shader
are excluded so their special build stories (wasm target + trunk for the
client; pinned rust-gpu nightly for the shader) do not leak into
workspace-wide cargo commands.

## One-time setup

For the client:

```
rustup target add wasm32-unknown-unknown
cargo install trunk
```

## Build / test / lint

Workspace (core + server):

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

Client crate (excluded from workspace, wasm target):

```
cd crates/client
trunk build
```

Shader crate (excluded from workspace):

```
cargo check --manifest-path crates/shader/Cargo.toml
```

## Run

Server (currently just a stub main):

```
cargo run -p slash0-server
```

Client:

```
cd crates/client
trunk build
python3 -m http.server --directory dist 8080
```

Then open http://localhost:8080 in a WebGPU-capable browser
(Chrome/Edge 113+, Safari 18+).

Firefox does not ship WebGPU enabled by default yet. To enable it, open
`about:config`, accept the warning, search for `dom.webgpu.enabled`, and
toggle it to `true`. Firefox Nightly has it on by default.

## IDE setup

The client crate is excluded from the workspace and only targets
`wasm32-unknown-unknown`. `crates/client/.cargo/config.toml` sets that as
the default build target, so `cargo check` / `cargo clippy` / rust-analyzer
all pick it up when invoked from `crates/client/` without needing an
explicit `--target` flag.

For IntelliJ / RustRover: the plugin only sees crates that belong to a
Cargo project it has attached. Since the client is excluded from the
top-level workspace, open its `Cargo.toml` explicitly (right-click the
file -> Cargo -> Attach Cargo Project, or open `crates/client/` as its own
project) so analysis targets wasm32.

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
(shader), `cargo clippy` (client).

## Status

Radix trie (announce/withdraw/lookup/sweep with per-mutation dirty tracking)
is complete on the core. Client renders a solid-color WGSL fragment shader
to a canvas via wgpu 30 (WebGPU only). Server, wire codec, RIS ingest, and
rust-gpu integration are not yet started. See `CLAUDE.md` for the
architecture-of-record.
