//! Order-64 Hilbert curve mapping between 128-bit IP addresses and 2D points.
//!
//! A 128-bit IP address is the 1D index (distance) along a Hilbert curve; an
//! order-64 curve (64 subdivision levels) fills a `2^64 x 2^64` grid, so each 2D
//! axis is 64 bits. The mapping is address-generic: v4 addresses simply occupy
//! the high 32 bits of the 128-bit input, so a v4 `/32` is a `/32` cell in the
//! shared v4/v6 space.
//!
//! The same code runs on the CPU (future jump-to-prefix / click-to-inspect UX)
//! and inside the rust-gpu fragment shader, where [`point_to_ip`] is the
//! per-pixel hot path (inverse-Hilbert: pixel position back to an address to
//! walk the trie). To stay shader-representable the implementation uses only
//! `u32` bit math over fixed-size arrays: no recursion, no allocation, no
//! `u64`/`u128`, and no `Option`/enum types in the public signatures.
//!
//! # Bit conventions
//!
//! IP addresses are the crate-wide [`Address`] (`[u32; 4]`, big-endian, most
//! significant bit in bit 0 of word 0). A 2D point's axes follow the same
//! convention in `[u32; 2]`, so the coarsest Hilbert subdivision corresponds to
//! the most significant axis bit.
//!
//! The transform is the canonical iterative Hilbert algorithm. Because each of
//! the 64 levels touches exactly one bit per axis and two bits of the index,
//! every arithmetic step degenerates into a bit operation: setting the next
//! axis/index bit is a bit-set, and the rotate/reflect step `x = 2^k - 1 - x`
//! (with `x < 2^k`) is exactly `x ^= 2^k - 1` (complement the low `k` bits).

use crate::prefix::Address;

/// A point in Hilbert 2D space.
///
/// Each axis is 64 bits stored big-endian in `[u32; 2]` (most significant bit
/// in bit 0 of word 0), matching the [`Address`] layout. Laid out `#[repr(C)]`
/// so it can embed in shader-visible structs and uniforms.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HilbertPoint {
    pub x: [u32; 2],
    pub y: [u32; 2],
}

/// Number of subdivision levels (the curve order), which is also the number of
/// bits per 2D axis.
const LEVELS: u32 = 64;

/// Maps a 128-bit IP address (the Hilbert index) to its 2D point.
///
/// Inverse of [`point_to_ip`]. This is `d2xy` in Hilbert-curve literature.
///
/// # Examples
///
/// ```
/// use slash0_core::hilbert::{ip_to_point, point_to_ip, HilbertPoint};
/// use slash0_core::prefix::Address;
///
/// // The all-zero address sits at the curve's origin.
/// assert_eq!(ip_to_point(Address([0, 0, 0, 0])), HilbertPoint::default());
///
/// // Every address round-trips through its 2D point.
/// let ip = Address([0x2001_0db8, 0, 0, 0x0000_0001]);
/// assert_eq!(point_to_ip(ip_to_point(ip)), ip);
/// ```
pub fn ip_to_point(ip: Address) -> HilbertPoint {
    let ip = ip.0;
    let mut x = [0u32; 2];
    let mut y = [0u32; 2];

    // Build the axes least-significant bit first (finest detail first): level
    // `level` consumes index bits `2*level` and `2*level + 1`.
    for level in 0..LEVELS {
        let rx = index_bit(&ip, 2 * level + 1);
        let ry = index_bit(&ip, 2 * level) ^ rx;

        if ry == 0 {
            if rx == 1 {
                complement_low_axis_bits(&mut x, level);
                complement_low_axis_bits(&mut y, level);
            }
            core::mem::swap(&mut x, &mut y);
        }

        if rx == 1 {
            set_axis_bit(&mut x, level);
        }
        if ry == 1 {
            set_axis_bit(&mut y, level);
        }
    }

    HilbertPoint { x, y }
}

/// Maps a 2D point back to its 128-bit IP address.
///
/// Inverse of [`ip_to_point`]; this is the shader's per-pixel inverse-Hilbert
/// step (`xy2d` in the literature).
///
/// # Examples
///
/// ```
/// use slash0_core::hilbert::{ip_to_point, point_to_ip, HilbertPoint};
/// use slash0_core::prefix::Address;
///
/// assert_eq!(point_to_ip(HilbertPoint::default()), Address([0, 0, 0, 0]));
///
/// let point = ip_to_point(Address([0, 0, 0, 42]));
/// assert_eq!(point_to_ip(point), Address([0, 0, 0, 42]));
/// ```
pub fn point_to_ip(point: HilbertPoint) -> Address {
    let mut x = point.x;
    let mut y = point.y;
    let mut ip = [0u32; 4];

    // Walk levels most-significant (coarsest) first.
    for level in (0..LEVELS).rev() {
        let rx = axis_bit(&x, level);
        let ry = axis_bit(&y, level);

        let quadrant = (3 * rx) ^ ry;
        if quadrant & 1 == 1 {
            set_index_bit(&mut ip, 2 * level);
        }
        if quadrant & 2 == 2 {
            set_index_bit(&mut ip, 2 * level + 1);
        }

        if ry == 0 {
            if rx == 1 {
                // Reflect within the full square: complement all 64 axis bits.
                x[0] ^= !0;
                x[1] ^= !0;
                y[0] ^= !0;
                y[1] ^= !0;
            }
            core::mem::swap(&mut x, &mut y);
        }
    }

    Address(ip)
}

/// Returns bit `bit` (0 = least significant) of the 128-bit index, viewing the
/// big-endian `[u32; 4]` as an integer with word 0 most significant.
fn index_bit(ip: &[u32; 4], bit: u32) -> u32 {
    (ip[3 - (bit / 32) as usize] >> (bit % 32)) & 1
}

/// Sets bit `bit` (0 = least significant) of the 128-bit index.
fn set_index_bit(ip: &mut [u32; 4], bit: u32) {
    ip[3 - (bit / 32) as usize] |= 1u32 << (bit % 32);
}

/// Returns bit `bit` (0 = least significant) of a 64-bit axis.
fn axis_bit(axis: &[u32; 2], bit: u32) -> u32 {
    (axis[1 - (bit / 32) as usize] >> (bit % 32)) & 1
}

/// Sets bit `bit` (0 = least significant) of a 64-bit axis.
fn set_axis_bit(axis: &mut [u32; 2], bit: u32) {
    axis[1 - (bit / 32) as usize] |= 1u32 << (bit % 32);
}

/// Complements the low `bits` bits of a 64-bit axis (`bits` in `0..=64`).
fn complement_low_axis_bits(axis: &mut [u32; 2], bits: u32) {
    axis[1] ^= low_bit_mask(bits);
    axis[0] ^= low_bit_mask(bits.saturating_sub(32));
}

/// A `u32` with the low `n` bits set, for `n` in `0..=32`.
fn low_bit_mask(n: u32) -> u32 {
    if n >= 32 { !0 } else { (1u32 << n) - 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u64) -> u32 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (*seed >> 32) as u32
    }

    fn random_ip(seed: &mut u64) -> [u32; 4] {
        [lcg(seed), lcg(seed), lcg(seed), lcg(seed)]
    }

    fn index_to_u128(ip: [u32; 4]) -> u128 {
        ((ip[0] as u128) << 96)
            | ((ip[1] as u128) << 64)
            | ((ip[2] as u128) << 32)
            | (ip[3] as u128)
    }

    fn u128_to_index(v: u128) -> [u32; 4] {
        [
            (v >> 96) as u32,
            (v >> 64) as u32,
            (v >> 32) as u32,
            v as u32,
        ]
    }

    fn axes(point: HilbertPoint) -> (u64, u64) {
        let x = ((point.x[0] as u64) << 32) | point.x[1] as u64;
        let y = ((point.y[0] as u64) << 32) | point.y[1] as u64;
        (x, y)
    }

    fn point_from_axes(x: u64, y: u64) -> HilbertPoint {
        HilbertPoint {
            x: [(x >> 32) as u32, x as u32],
            y: [(y >> 32) as u32, y as u32],
        }
    }

    /// Independent oracle: the textbook algorithm using plain arithmetic (real
    /// subtraction and multiply-add, not the bitwise simplification), so it
    /// cross-checks the wide-array implementation rather than mirroring it.
    fn oracle_d2xy(d: u128) -> (u64, u64) {
        let mut x = 0u64;
        let mut y = 0u64;
        for level in 0..64u32 {
            let s = 1u64 << level;
            let rx = ((d >> (2 * level + 1)) & 1) as u64;
            let ry = (((d >> (2 * level)) & 1) as u64) ^ rx;
            if ry == 0 {
                if rx == 1 {
                    x = s - 1 - x;
                    y = s - 1 - y;
                }
                core::mem::swap(&mut x, &mut y);
            }
            x += s * rx;
            y += s * ry;
        }
        (x, y)
    }

    fn oracle_xy2d(mut x: u64, mut y: u64) -> u128 {
        let mut d = 0u128;
        for level in (0..64u32).rev() {
            let s = 1u64 << level;
            let rx = if x & s > 0 { 1u128 } else { 0 };
            let ry = if y & s > 0 { 1u128 } else { 0 };
            d += (s as u128) * (s as u128) * ((3 * rx) ^ ry);
            if ry == 0 {
                if rx == 1 {
                    x = u64::MAX - x;
                    y = u64::MAX - y;
                }
                core::mem::swap(&mut x, &mut y);
            }
        }
        d
    }

    fn manhattan(a: HilbertPoint, b: HilbertPoint) -> u128 {
        let (ax, ay) = axes(a);
        let (bx, by) = axes(b);
        ax.abs_diff(bx) as u128 + ay.abs_diff(by) as u128
    }

    #[test]
    fn zero_maps_to_origin() {
        assert_eq!(ip_to_point(Address([0; 4])), HilbertPoint::default());
        assert_eq!(point_to_ip(HilbertPoint::default()), Address([0; 4]));
    }

    #[test]
    fn all_ones_round_trips() {
        let ip = Address([u32::MAX; 4]);
        assert_eq!(point_to_ip(ip_to_point(ip)), ip);
    }

    #[test]
    fn round_trips_over_random_ips() {
        let mut seed = 0xDEAD_BEEF_CAFE_BABE_u64;
        for _ in 0..500 {
            let ip = Address(random_ip(&mut seed));
            assert_eq!(point_to_ip(ip_to_point(ip)), ip);
        }
    }

    #[test]
    fn axis_extremes() {
        let corners = [
            HilbertPoint {
                x: [0, 0],
                y: [0, 0],
            },
            HilbertPoint {
                x: [u32::MAX, u32::MAX],
                y: [0, 0],
            },
            HilbertPoint {
                x: [0, 0],
                y: [u32::MAX, u32::MAX],
            },
            HilbertPoint {
                x: [u32::MAX, u32::MAX],
                y: [u32::MAX, u32::MAX],
            },
        ];
        for point in corners {
            assert_eq!(ip_to_point(point_to_ip(point)), point);
        }
    }

    #[test]
    fn matches_arithmetic_oracle() {
        let mut seed = 0xDEAD_BEEF_CAFE_BABE_u64;
        for _ in 0..500 {
            let ip = random_ip(&mut seed);
            assert_eq!(
                axes(ip_to_point(Address(ip))),
                oracle_d2xy(index_to_u128(ip))
            );

            let x = ((lcg(&mut seed) as u64) << 32) | lcg(&mut seed) as u64;
            let y = ((lcg(&mut seed) as u64) << 32) | lcg(&mut seed) as u64;
            assert_eq!(
                index_to_u128(point_to_ip(point_from_axes(x, y)).0),
                oracle_xy2d(x, y)
            );
        }
    }

    #[test]
    fn consecutive_ips_are_adjacent() {
        // The defining Hilbert property: successive indices land on grid
        // neighbours (Manhattan distance 1). Catches bit-ordering and
        // reflection-table mistakes that round-tripping alone would miss.
        for d in 0u128..2000 {
            let a = ip_to_point(Address(u128_to_index(d)));
            let b = ip_to_point(Address(u128_to_index(d + 1)));
            assert_eq!(manhattan(a, b), 1, "d = {d}");
        }

        let mut seed = 0x1234_5678_9ABC_DEF0_u64;
        for _ in 0..500 {
            // Clear the top bit so `d + 1` cannot overflow the 128-bit space.
            let d = index_to_u128(random_ip(&mut seed)) & ((1u128 << 127) - 1);
            let a = ip_to_point(Address(u128_to_index(d)));
            let b = ip_to_point(Address(u128_to_index(d + 1)));
            assert_eq!(manhattan(a, b), 1, "d = {d}");
        }
    }

    #[test]
    fn word_boundary_bits_round_trip() {
        for pos in [0u32, 1, 31, 32, 33, 63, 64, 95, 96, 127] {
            let mut ip = [0u32; 4];
            // Set the bit at MSB-indexed position `pos`, matching Address::bit_at.
            ip[(pos / 32) as usize] |= 0x8000_0000u32 >> (pos % 32);
            let addr = Address(ip);
            assert_eq!(point_to_ip(ip_to_point(addr)), addr, "pos = {pos}");
        }
    }
}
