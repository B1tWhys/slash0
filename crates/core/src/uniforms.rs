//! Per-frame values the render pipeline feeds to the fragment shader. Defined
//! here, alongside the other shader-visible structs, so the client and the
//! shader agree on one layout.

use crate::node::NodeIdx;
use crate::timestamp::Timestamp;

/// Per-frame globals passed to the fragment shader.
///
/// Bound as a read-only **storage** buffer, not a uniform buffer, so that we can use the more
/// relaxed std430 memory layout constraints instead of having to fit this into std140.
#[apply(Pod!)]
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderUniforms {
    /// Slab index of the tree root, or `None` for an empty tree.
    pub root: Option<NodeIdx>,
    /// Current time in milliseconds.
    pub now: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn layout_is_stable() {
        assert_eq!(size_of::<RenderUniforms>(), 12);
        assert_eq!(align_of::<RenderUniforms>(), 4);
        assert_eq!(offset_of!(RenderUniforms, root), 0);
        assert_eq!(offset_of!(RenderUniforms, now), 4);
    }
}
