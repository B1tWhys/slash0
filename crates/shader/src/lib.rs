#![no_std]

use spirv_std::glam::Vec4;
use spirv_std::spirv;

/// Full-screen triangle: (-1,-1), (3,-1), (-1,3). Clipping to [-1,1]^2
/// covers the whole viewport with no seams.
#[spirv(vertex)]
pub fn vs_main(
    #[spirv(vertex_index)] vertex_index: u32,
    #[spirv(position)] out_position: &mut Vec4,
) {
    let x = ((vertex_index & 1) as i32 * 4 - 1) as f32;
    let y = ((vertex_index & 2) as i32 * 2 - 1) as f32;
    *out_position = Vec4::new(x, y, 0.0, 1.0);
}

#[spirv(fragment)]
pub fn fs_main(out_color: &mut Vec4) {
    *out_color = Vec4::new(0.15, 0.55, 0.95, 1.0);
}
