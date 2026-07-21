//! Bit-level helpers for the `([u32; 4], prefix_len: u32)` representation
//! used everywhere in this crate for IP prefixes and addresses.
//!
//! A "prefix" here is a 128-bit big-endian key (most significant bit lives in
//! bit 0 of `word[0]`) paired with a length in bits. IPv4 prefixes use lengths
//! `0..=32`; IPv6 uses `0..=128`. The trie, the future Hilbert encoding, and
//! the rust-gpu shader's per-pixel walk all navigate this representation, so
//! the bit-poking primitives live here rather than in any one consumer.
//!
//! Higher-level prefix types (e.g. a `Prefix` newtype, CIDR parsing, wire
//! formats) do not belong in this module.

/// Maximum prefix length in bits, sized for IPv6.
///
/// IPv4 prefixes use lengths `0..=32`; IPv6 uses `0..=128`. Callers should
/// clamp or validate lengths against this constant when accepting external
/// input.
pub const MAX_PREFIX_LEN: u32 = 128;

/// A 128-bit prefix key with an associated bit length.
///
/// Bits at positions `len..128` are guaranteed to be zero when constructed
/// via [`Prefix::new`], which lets two prefixes with equivalent CIDR
/// semantics compare bytewise-equal.
///
/// Laid out as `#[repr(C)]` so it can embed in shader-visible structs.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Prefix {
    pub bits: [u32; 4],
    pub len: u32,
}

impl Prefix {
    /// Constructs a prefix, masking any bits past `len` to zero.
    ///
    /// Panics if `len > MAX_PREFIX_LEN`.
    ///
    /// # Examples
    ///
    /// ```
    /// use slash0_core::prefix::Prefix;
    ///
    /// let p = Prefix::new([0xFFFF_FFFF, 0, 0, 0], 8);
    /// assert_eq!(p.bits, [0xFF00_0000, 0, 0, 0]);
    /// assert_eq!(p.len, 8);
    /// ```
    pub fn new(bits: [u32; 4], len: u32) -> Self {
        assert!(len <= MAX_PREFIX_LEN);
        Self {
            bits: mask_prefix(bits, len),
            len,
        }
    }

    /// Constructs without masking. Debug-asserts that `bits` are already
    /// canonical (zero past `len`) and that `len <= MAX_PREFIX_LEN`.
    ///
    /// Use this only in hot paths where the input is known-canonical (e.g.
    /// derived from another `Prefix`); use [`Prefix::new`] at API boundaries.
    pub fn new_unchecked(bits: [u32; 4], len: u32) -> Self {
        debug_assert!(len <= MAX_PREFIX_LEN);
        debug_assert_eq!(
            mask_prefix(bits, len),
            bits,
            "bits past len must already be zero",
        );
        Self { bits, len }
    }

    /// Returns the bit at `pos` (0 or 1). See [`bit_at`] for indexing rules.
    pub fn bit_at(&self, pos: u32) -> u32 {
        bit_at(&self.bits, pos)
    }

    /// Length of the longest common prefix between this and `other`, bounded
    /// by `min(self.len, other.len)`.
    pub fn common_prefix_len(&self, other: &Self) -> u32 {
        common_prefix_len(&self.bits, self.len, &other.bits, other.len)
    }

    /// Whether this prefix covers `addr` — i.e., its first `self.len` bits
    /// match `addr`'s.
    ///
    /// # Examples
    ///
    /// ```
    /// use slash0_core::prefix::Prefix;
    ///
    /// let p = Prefix::new([0x0A00_0000, 0, 0, 0], 8); // 10.0.0.0/8
    /// assert!(p.covers(&[0x0A01_0203, 0, 0, 0]));     // 10.1.2.3
    /// assert!(!p.covers(&[0xC000_0201, 0, 0, 0]));    // 192.0.2.1
    /// ```
    pub fn covers(&self, addr: &[u32; 4]) -> bool {
        common_prefix_len(&self.bits, self.len, addr, MAX_PREFIX_LEN) == self.len
    }
}

/// Returns the bit at position `pos` (0 or 1).
///
/// Position 0 is the most significant bit of `prefix[0]`; position 127 is the
/// least significant bit of `prefix[3]`. Panics if `pos >= 128`.
///
/// # Examples
///
/// ```
/// use slash0_core::prefix::bit_at;
///
/// let p = [0x8000_0000, 0, 0, 1];
/// assert_eq!(bit_at(&p, 0), 1);   // MSB of word 0
/// assert_eq!(bit_at(&p, 1), 0);
/// assert_eq!(bit_at(&p, 127), 1); // LSB of word 3
/// ```
pub fn bit_at(prefix: &[u32; 4], pos: u32) -> u32 {
    let word = prefix[(pos / 32) as usize];
    let bit_in_word = 31 - (pos % 32);
    (word >> bit_in_word) & 1
}

/// Returns the length in bits of the longest common prefix of `a` and `b`,
/// bounded by `min(a_len, b_len)`.
///
/// Bits past each key's declared length are ignored, so callers do not need
/// to have zeroed them (see [`mask_prefix`] if you do want to normalize).
///
/// # Examples
///
/// ```
/// use slash0_core::prefix::common_prefix_len;
///
/// let a = [0xFFFF_0000, 0, 0, 0];
/// let b = [0xFFFE_0000, 0, 0, 0];
/// // Both are /16; they agree on the first 15 bits.
/// assert_eq!(common_prefix_len(&a, 16, &b, 16), 15);
///
/// // Bounded by the shorter length even if more bits match.
/// assert_eq!(common_prefix_len(&a, 16, &b, 8), 8);
///
/// // Length 0 always yields 0.
/// assert_eq!(common_prefix_len(&a, 0, &b, 32), 0);
/// ```
pub fn common_prefix_len(a: &[u32; 4], a_len: u32, b: &[u32; 4], b_len: u32) -> u32 {
    let max_check = a_len.min(b_len);
    if max_check == 0 {
        return 0;
    }
    let mut common = 0u32;
    for word_idx in 0..4 {
        if common >= max_check {
            break;
        }
        let xor = a[word_idx] ^ b[word_idx];
        let word_remaining = max_check - common;
        if xor == 0 {
            common += 32.min(word_remaining);
        } else {
            let leading = xor.leading_zeros();
            common += leading.min(word_remaining);
            break;
        }
    }
    common
}

/// Zeroes the bits of `prefix` at positions `prefix_len..128`, returning the
/// normalized value.
///
/// Use this to canonicalize prefixes accepted from external sources so that
/// two prefixes with the same `(prefix, prefix_len)` semantics compare
/// bitwise-equal.
///
/// # Examples
///
/// ```
/// use slash0_core::prefix::mask_prefix;
///
/// // Everything past bit 8 is cleared.
/// assert_eq!(
///     mask_prefix([0xFFFF_FFFF, 0xFFFF_FFFF, 0, 0], 8),
///     [0xFF00_0000, 0, 0, 0]
/// );
///
/// // Boundary cases.
/// assert_eq!(mask_prefix([0xFFFF_FFFF; 4], 0), [0, 0, 0, 0]);
/// assert_eq!(mask_prefix([0xFFFF_FFFF; 4], 128), [0xFFFF_FFFF; 4]);
/// ```
pub fn mask_prefix(mut prefix: [u32; 4], prefix_len: u32) -> [u32; 4] {
    for (word_idx, word) in prefix.iter_mut().enumerate() {
        let word_start = word_idx as u32 * 32;
        if prefix_len <= word_start {
            *word = 0;
        } else if prefix_len < word_start + 32 {
            let bits_to_keep = prefix_len - word_start;
            let shift = 32 - bits_to_keep;
            *word &= !0u32 << shift;
        }
    }
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_at_boundaries() {
        let p = [0x8000_0001, 0x8000_0001, 0x8000_0001, 0x8000_0001];
        assert_eq!(bit_at(&p, 0), 1);
        assert_eq!(bit_at(&p, 1), 0);
        assert_eq!(bit_at(&p, 30), 0);
        assert_eq!(bit_at(&p, 31), 1);
        assert_eq!(bit_at(&p, 32), 1);
        assert_eq!(bit_at(&p, 63), 1);
        assert_eq!(bit_at(&p, 64), 1);
        assert_eq!(bit_at(&p, 96), 1);
        assert_eq!(bit_at(&p, 127), 1);
    }

    #[test]
    fn common_prefix_len_cases() {
        let a = [0xFF00_0000, 0, 0, 0];
        let b = [0x8000_0000, 0, 0, 0];
        assert_eq!(common_prefix_len(&a, 8, &b, 8), 1);
        assert_eq!(common_prefix_len(&a, 8, &a, 8), 8);
        let a = [0xFFFF_0000, 0, 0, 0];
        let b = [0xFFFE_0000, 0, 0, 0];
        assert_eq!(common_prefix_len(&a, 16, &b, 16), 15);
        assert_eq!(common_prefix_len(&a, 16, &b, 8), 8);
        assert_eq!(common_prefix_len(&a, 0, &b, 32), 0);
    }

    #[test]
    fn common_prefix_len_across_words() {
        let a = [0xFFFF_FFFF, 0xFFFF_0000, 0, 0];
        let b = [0xFFFF_FFFF, 0xFFFC_0000, 0, 0];
        assert_eq!(common_prefix_len(&a, 48, &b, 48), 46);
    }

    #[test]
    fn mask_prefix_zeroes_trailing_bits() {
        assert_eq!(
            mask_prefix([0xFFFF_FFFF, 0xFFFF_FFFF, 0, 0], 12),
            [0xFFF0_0000, 0, 0, 0]
        );
        assert_eq!(mask_prefix([0xFFFF_FFFF; 4], 0), [0, 0, 0, 0]);
        assert_eq!(mask_prefix([0xFFFF_FFFF; 4], 128), [0xFFFF_FFFF; 4]);
        assert_eq!(
            mask_prefix([0xFFFF_FFFF; 4], 33),
            [0xFFFF_FFFF, 0x8000_0000, 0, 0]
        );
    }
}
