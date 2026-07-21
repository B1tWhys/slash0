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

pub const MAX_PREFIX_LEN: u32 = 128;

pub fn bit_at(prefix: &[u32; 4], pos: u32) -> u32 {
    let word = prefix[(pos / 32) as usize];
    let bit_in_word = 31 - (pos % 32);
    (word >> bit_in_word) & 1
}

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
