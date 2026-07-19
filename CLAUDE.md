# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project overview

`slash0` is a real-time visualization of the global BGP route tables (both IPv4 and IPv6), rendered via WebGPU in Hilbert space. A server ingests RIS Live updates from a single filtered peer and maintains in-memory radix trees; a WASM client subscribes to one protocol at a time, applies updates locally, and renders each frame with a fragment shader that walks the trie per pixel to color by "time since last update."

Implementation has not started yet -- this file records the architecture that has been agreed on so future instances can pick up where the design left off.

## Common commands

- Build: `cargo build`
- Run: `cargo run`
- Test (all): `cargo test`
- Test (single): `cargo test <test_name>` (substring match on test path)
- Lint: `cargo clippy --all-targets -- -D warnings`
- Format: `cargo fmt`

## Architecture

### Radix trie

- Slab-backed. Nodes reference each other via `u32` indices, never pointers. Pre-allocate a generous slab on init; accept the cost of a rare dynamic resize.
- Shared `no_std` crate defines the tree topology, traversal, insert/withdraw, and ancestor timestamp propagation. This crate is consumed by the server, the WASM client, and the shader (via `rust-gpu`).
- The tree is generic over node data: `RadixTree<D: NodeData>`. The shared crate holds all D-agnostic tree logic; D-specific update logic lives on the `NodeData` trait impls.
    - Server uses `RadixTree<ThickData>` -- full BGP metadata (AS path, communities, next hop, etc.).
    - Client and shader use `RadixTree<ThinData>` -- just what rendering needs (timestamps for now; grows over time).
    - A projection `impl From<&ThickData> for ThinData` lives in the shared crate. Snapshot generation walks the thick trie and emits thin nodes.
- Timestamps are `[u32; 2]` (u64 milliseconds under the hood) with a `Timestamp` newtype providing `u64` ergonomics on the CPU. WGSL has no `u64`, so we do not rely on it in the node struct.
- Withdrawn nodes are **tombstoned until end-of-frame**, then swept back to the free list. This eliminates the "GPU follows a recycled index into a different subtree mid-frame" hazard for one frame's worth of memory. Server side follows the same pattern tied to WAL flush cadence.
- Free-list vs never-reuse policy beyond the tombstone frame is deliberately deferred.

### Timestamp semantics

- Every update (announce **and** withdraw) propagates its timestamp up to the root -- max-to-root, log-depth walk.
- One BGP update potentially dirties up to 128 nodes (IPv6 depth). Client/GPU delta pipelines must be sized for that amplification.

### Rendering

- Fragment shader does a per-pixel radix trie walk. Color reflects the propagated timestamp on whichever node's Hilbert cell contains the pixel.
- Hilbert curve is **fixed-order-128**, always, for both v4 and v6. The CPU sends a viewport rectangle in Hilbert 2D space as a uniform; the shader does inverse-Hilbert per pixel.
- Zoom range spans `/0` all the way to `/128`. The whole trie fits in client memory (~1M v4 prefixes, ~200k v6), so there is no tile-fetching plumbing -- the shader just clips to the viewport.
- Hilbert math is currently GPU-only. UX features (jump-to-prefix, click-to-inspect, prefix highlight) will pull it back to CPU later. Write the shared Hilbert function so it is reusable from Rust CPU code from day one, even though only the shader consumes it initially.
- Shader written via `rust-gpu`. The `NodeData` monomorphization the shader sees (`ThinData`) must contain only shader-representable fields -- u32s, arrays of u32, no `Option`, no payload enums.
- Aliasing at extreme zoom-out is expected. Multisample if it looks bad.

### GPU update path

Start with **sparse `queue.write_buffer` calls from the trie's write path** -- no explicit dirty-set required, since the write path inherently knows which slab indices it touched. `wgpu` batches per submit.

Profile before changing. If per-frame `queue.write_buffer` submission overhead dominates (not PCIe bandwidth), fall back to a compute scatter: CPU accumulates `Vec<(u32, ThinData)>` per frame, uploads once, compute pass scatters into the slab.

Full-buffer re-upload each frame is the fallback-fallback.

### Server

- Single process on a single VM, `systemd`-managed. No containerization for now.
- Subscribes to RIS Live and filters to a **single peer**. The visualization is one peer's view of the internet, not a global merged view.
- Ingests updates into a **write-ahead log** (in-memory ring buffer) and applies them to the thick trie. WAL retention window sizes the "how far behind can a client fall before it needs a resnapshot" budget -- pick based on max acceptable client lag, not comfort.
- Each WAL entry has a monotonic sequence number. Snapshots record "everything up to seq N," clients resume at N+1.
- WAL entries are serialized **once** and fanned out as identical bytes to every subscribed connection. Never per-connection serialize.
- Thin snapshots are generated periodically and cached as `Arc<Vec<u8>>` (bytes-on-wire, not an in-memory trie). Cadence is snapshot-on-demand with debouncing, or slow-periodic (every few seconds), *not* a fixed 300 ms tick -- 300 ms is the worst of both worlds.
- Snapshot generation currently pauses thick-WAL apply. RCU/arc-swap is the escape hatch if pause latency turns out to hurt new-client onboarding at scale.
- IPv4 and IPv6 have separate tries and separate WALs. Clients subscribe to one at a time.

### Wire protocol

- **Snapshot format:** flat prefix list. Client rebuilds the thin trie on load by running each prefix through the shared write path. Slower first-paint than shipping serialized slab bytes, but the wire format doesn't leak internal memory layout (which would collide with the WASM/native repr divergence).
- **Streaming format:** raw-ish BGP updates. Both server and client feed them through the shared write path -- the fact that the same code runs on both sides is the point.
- **Bootstrap:** client opens one WebSocket per session, receives the snapshot as the first message, then an ordered stream of updates. Same-socket avoids the race window that separate-HTTP-snapshot-plus-WS-stream would open.
- Version the wire format from day one. Thin will grow as new color modes are added.

### Client

- Rust -> WASM for CPU, `wgpu` -> WebGPU for GPU. Whole update feed is streamed (no viewport-scoped filtering server-side); part of the goal is stress-testing WASM performance.
- User selects v4 or v6 up front. Switching protocols means a new subscription + snapshot.
- During bootstrap, WAL messages that arrive while the snapshot is still being applied are queued and drained after the snapshot finishes.

## Deferred decisions

Explicitly parked; revisit when relevant:

- Node lifecycle after end-of-frame tombstone sweep (free-list reuse vs never-reuse).
- Client clock-offset strategy for "time since update" rendering.
- Wire-format versioning specifics for future thin-node fields (e.g., color-by-AS mode).
- Snapshot generation without pausing thick-WAL apply (RCU / arc-swap).
- Retention/history for BGP route flapping analytics -- would be a separate API, not part of the live viz.
