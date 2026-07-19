#![no_std]

// Placeholder. The real fragment entry point will be:
//   #[spirv(fragment)] fn main_fs(...) { ... }
// once rust-gpu deps (spirv-std, spirv-builder in the client's build.rs) are
// wired up.
pub fn hilbert_order() -> u32 {
    slash0_core::hilbert::ORDER
}
