#![no_std]

use slash0_core::hilbert::{HilbertPoint, point_to_ip};
use slash0_core::node::Node;
use slash0_core::thin::ThinData;
use slash0_core::uniforms::RenderUniforms;
use spirv_std::glam::{Vec2, Vec4};
use spirv_std::spirv;

/// Full-screen triangle: (-1,-1), (3,-1), (-1,3). Clipping to [-1,1]^2 covers
/// the whole viewport with no seams. Emits a [0,1]^2 UV so the fragment stage
/// can place itself without a resolution uniform.
#[spirv(vertex)]
pub fn vs_main(
    #[spirv(vertex_index)] vertex_index: u32,
    #[spirv(position)] out_position: &mut Vec4,
    out_uv: &mut Vec2,
) {
    let x = ((vertex_index & 1) as i32 * 4 - 1) as f32;
    let y = ((vertex_index & 2) as i32 * 2 - 1) as f32;
    *out_position = Vec4::new(x, y, 0.0, 1.0);
    *out_uv = Vec2::new(x * 0.5 + 0.5, y * 0.5 + 0.5);
}

#[spirv(fragment)]
pub fn fs_main(
    _uv: Vec2,
    out_color: &mut Vec4,
    #[spirv(storage_buffer, descriptor_set = 0, binding = 0)] tree_slab: &[Node<ThinData>],
    #[spirv(storage_buffer, descriptor_set = 0, binding = 1)] uniforms: &RenderUniforms,
) {
    // Placeholder body: prove the slab + uniform bindings resolve against real
    // node data by coloring the whole frame from the root node's timestamp. The
    // per-pixel Hilbert walk that picks a node per pixel comes in a later phase.
    match uniforms.root {
        Some(root_idx) => {
            let root = &tree_slab[root_idx.get() as usize];
            // WGSL has no u64, so approximate "age since last update" from the
            // low millisecond word and fade red to black over ~1s.
            let age_ms = uniforms.now.0[1].wrapping_sub(root.data.timestamp.0[1]);
            let brightness = 1.0 - clamp01(age_ms as f32 / 1000.0);
            *out_color = Vec4::new(brightness, 0.0, 0.0, 1.0);
        }
        None => *out_color = Vec4::new(0.0, 0.0, 0.0, 1.0),
    }
}

/// Demonstration of the shared Hilbert code: map each pixel to a point in
/// Hilbert space, invert the curve to the distance ("IP") at that point, and
/// color by how far along the curve it sits. The result is a rainbow that
/// snakes through screen space in the Hilbert pattern, so adjacent hues trace
/// the curve. No zoom: UV lands directly on the high bits of the 64-bit axes,
/// so screen resolution sets the effective curve order.
#[spirv(fragment)]
pub fn fs_hilbert_poc(uv: Vec2, out_color: &mut Vec4) {
    // Pack a 24-bit UV coordinate into the top bits of each 64-bit axis. Word 0
    // is the most significant half, so the screen drives the coarsest Hilbert
    // subdivisions and the low bits stay zero.
    let scale = 16_777_215.0; // 2^24 - 1
    let ax = ((uv.x * scale) as u32 & 0x00FF_FFFF) << 8;
    let ay = ((uv.y * scale) as u32 & 0x00FF_FFFF) << 8;
    let ip = point_to_ip(HilbertPoint {
        x: [ax, 0],
        y: [ay, 0],
    });

    // Fraction along the curve: word 0 holds the top of the distance.
    let distance = ip.0[0] as f32 / 4_294_967_296.0; // 2^32

    // Sweep the hue several times over the full curve so Hilbert locality shows
    // up as tight, fast-repeating bands rather than one slow gradient.
    let cycles = 1.0;
    let swept = distance * cycles;
    let h = swept - (swept as u32 as f32); // fractional part, in [0, 1)

    // Trig-free hue -> RGB so the fragment stays shader-simple.
    let r = clamp01(absf(h * 6.0 - 3.0) - 1.0);
    let g = clamp01(2.0 - absf(h * 6.0 - 2.0));
    let b = clamp01(2.0 - absf(h * 6.0 - 4.0));
    *out_color = Vec4::new(r, g, b, 1.0);
}

fn absf(v: f32) -> f32 {
    if v < 0.0 { -v } else { v }
}

fn clamp01(v: f32) -> f32 {
    let lower = if v < 0.0 { 0.0 } else { v };
    if lower > 1.0 { 1.0 } else { lower }
}

#[spirv(fragment)]
pub fn fs_foo(_uv: Vec2, out_color: &mut Vec4) {
    *out_color = Vec4::new(1.0, 0.0, 0.0, 1.0);
}
