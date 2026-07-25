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
client; rust-gpu invocation for the shader) do not leak into
workspace-wide cargo commands.

The whole repo is pinned to a rust-gpu-compatible nightly toolchain
(currently `nightly-2026-04-11`) via `rust-toolchain.toml`. rust-gpu
forces the shader crate onto that specific nightly, and running the whole
repo on the same one avoids toolchain gymnastics between crates.

## Shader pipeline

The shader source lives in `crates/shader/src/lib.rs` as regular Rust
code, annotated with `#[spirv(vertex)]` and `#[spirv(fragment)]`. It
goes through the following transformations to end up running on the GPU:

```
crates/shader/src/lib.rs          (Rust source)
  |
  | rust-gpu's rustc_codegen_spirv backend, driven by spirv-builder
  | from crates/client/build.rs (runs on the nightly-pinned toolchain,
  | which is why the whole repo is on nightly)
  v
slash0_shader.spv                 (Vulkan 1.2 SPIR-V binary)
  |
  | naga (as a build-dep of crates/client, still in build.rs):
  |   naga::front::spv   parses SPIR-V into naga IR
  |   naga::valid        validates
  |   naga::back::wgsl   serializes to WGSL text
  v
$OUT_DIR/slash0_shader.wgsl       (WGSL text; inspectable)
  |
  | include_str!(env!("SLASH0_SHADER_WGSL")) at compile time -
  | the WGSL string is baked into the wasm binary
  v
...ships to the browser as part of the .wasm bundle...
  |
  | at page load: wgpu::ShaderSource::Wgsl(...) is handed straight to
  | GPUDevice.createShaderModule - no in-wasm translation
  v
Browser WebGPU (Dawn in Chrome, gecko-webgpu in Firefox) parses WGSL,
compiles to native GPU code for whatever GPU is present.
```

Why go through this many steps for what looks like a WGSL shader?
Because writing shaders in Rust means we can share code with the CPU
trie (same types, same helpers, same test coverage) once we start doing
the per-pixel trie walk in the fragment shader. rust-gpu is the only
piece of that puzzle that has to be at the front; we translate to WGSL
at build time (rather than at runtime, via naga bundled in the wasm) to
keep the wasm smaller and turn any translation error into a build error
instead of a browser runtime error.

First `trunk build` after a fresh checkout (including the first `cargo
build` of the server, which drives trunk) takes ~5-10 min because Cargo
has to compile `rustc_codegen_spirv` (rust-gpu's codegen backend).
Cached afterwards.

## One-time setup

```
cargo install trunk
```

The nightly toolchain plus its components (`rust-src`, `rustc-dev`,
`llvm-tools`, `rustfmt`, `clippy`) and the `wasm32-unknown-unknown`
target are pinned in `rust-toolchain.toml`; rustup auto-installs them on
the first cargo command in the repo.

## Build / test / lint

Workspace (core + server):

```
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Building the server runs `trunk build` for the client (and, through the
client's build script, the shader) via `crates/server/build.rs`. So any
command that compiles the server also rebuilds the client bundle when its
sources change, which makes `trunk` a build-time requirement for the server.

Verify core's no-alloc feature slice compiles (this is what the shader will
see):

```
cargo build -p slash0-core --no-default-features
```

Shader crate (excluded from workspace):

```
cargo check --manifest-path crates/shader/Cargo.toml
```

## Run

The server builds the client + shader (via its build script) and serves the
whole app, so one command brings up everything. From the workspace root:

```
cargo serve        # alias for `cargo run -p slash0-server`
```

Then open http://127.0.0.1:3000 in a WebGPU-capable browser (Chrome/Edge
113+, Safari 18+). The listen address and served asset directory are
configurable: copy `config/server.example.yaml` to `config/server.yaml` to
override the defaults, or pass `--config <path>`.

For standalone client work with autoreload (the shader is a static render
for now, so no server is needed to see it):

```
cd crates/client
trunk serve
```

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

Radix trie (announce/withdraw/lookup/sweep with per-mutation dirty
tracking) is complete on the core. Client renders a solid-color fragment
shader written in Rust via rust-gpu; see the Shader pipeline section for
what happens between `.rs` and pixels. The server has its HTTP/WebSocket
scaffolding (axum static client serving, `/api`, a `/ws` upgrade stub,
figment config, graceful shutdown); the wire codec and RIS ingest are not
yet started. See `CLAUDE.md` for the architecture-of-record.
