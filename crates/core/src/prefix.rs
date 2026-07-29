//! The `[u32; 4]` address key used everywhere in this crate, plus the prefix
//! (address + length) built on top of it.
//!
//! An [`Address`] is a 128-bit big-endian key: the most significant bit lives
//! in bit 0 of `word[0]`, the least significant in bit 127 of `word[3]`. A
//! [`Prefix`] pairs one with a length in bits. IPv4 prefixes use lengths
//! `0..=32`; IPv6 uses `0..=128`. The trie, the Hilbert encoding, and the
//! rust-gpu shader's per-pixel walk all navigate this representation, so the
//! bit-poking primitives live on `Address` rather than in any one consumer.
//!
//! This module also owns the address-family tag ([`IpVersion`]) and, behind the
//! `parse` feature, CIDR-string parsing ([`Prefix::parse_cidr`]).

/// Maximum prefix length in bits, sized for IPv6.
///
/// IPv4 prefixes use lengths `0..=32`; IPv6 uses `0..=128`. Callers should
/// clamp or validate lengths against this constant when accepting external
/// input.
pub const MAX_PREFIX_LEN: u32 = 128;

/// Which IP address family a [`Prefix`] belongs to.
///
/// Kept out of the trie node payload — the trie is family-agnostic — but needed
/// by consumers that maintain separate v4/v6 tries and by any wire encoding. The
/// discriminants match the IP version numbers so the tag can serialize directly.
#[derive(Copy, Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(u8)]
pub enum IpVersion {
    V4 = 4,
    V6 = 6,
}

/// A 128-bit IP address: a big-endian `[u32; 4]` key with the most significant
/// bit in bit 0 of word 0.
///
/// This is the bare-address half of a [`Prefix`] (which pairs one with a
/// length) and the value the trie walk and Hilbert mapping navigate. Laid out
/// `#[repr(transparent)]` so it shares `[u32; 4]`'s layout and can embed in
/// shader-visible structs or be memcpy'd into GPU buffers.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Address(pub [u32; 4]);

impl Address {
    /// Returns the bit at position `pos` (0 or 1).
    ///
    /// Position 0 is the most significant bit of word 0; position 127 is the
    /// least significant bit of word 3. Panics if `pos >= 128`.
    ///
    /// # Examples
    ///
    /// ```
    /// use slash0_core::prefix::Address;
    ///
    /// let a = Address([0x8000_0000, 0, 0, 1]);
    /// assert_eq!(a.bit_at(0), 1);   // MSB of word 0
    /// assert_eq!(a.bit_at(1), 0);
    /// assert_eq!(a.bit_at(127), 1); // LSB of word 3
    /// ```
    pub fn bit_at(&self, pos: u32) -> u32 {
        let word = self.0[(pos / 32) as usize];
        let bit_in_word = 31 - (pos % 32);
        (word >> bit_in_word) & 1
    }

    /// Length in bits of the longest common prefix between `self` (declared
    /// length `self_len`) and `other` (declared length `other_len`), bounded by
    /// `min(self_len, other_len)`.
    ///
    /// Bits past each declared length are ignored, so callers do not need to
    /// have zeroed them (see [`Address::masked`] to normalize).
    ///
    /// # Examples
    ///
    /// ```
    /// use slash0_core::prefix::Address;
    ///
    /// let a = Address([0xFFFF_0000, 0, 0, 0]);
    /// let b = Address([0xFFFE_0000, 0, 0, 0]);
    /// // Both are /16; they agree on the first 15 bits.
    /// assert_eq!(a.common_prefix_len(16, &b, 16), 15);
    ///
    /// // Bounded by the shorter length even if more bits match.
    /// assert_eq!(a.common_prefix_len(16, &b, 8), 8);
    ///
    /// // Length 0 always yields 0.
    /// assert_eq!(a.common_prefix_len(0, &b, 32), 0);
    /// ```
    pub fn common_prefix_len(&self, self_len: u32, other: &Address, other_len: u32) -> u32 {
        let max_check = self_len.min(other_len);
        if max_check == 0 {
            return 0;
        }
        let mut common = 0u32;
        for word_idx in 0..4 {
            if common >= max_check {
                break;
            }
            let xor = self.0[word_idx] ^ other.0[word_idx];
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

    /// Returns a copy with the bits at positions `len..128` zeroed.
    ///
    /// Canonicalizes an address to a given prefix length so that two addresses
    /// with the same `(address, len)` semantics compare bitwise-equal.
    ///
    /// # Examples
    ///
    /// ```
    /// use slash0_core::prefix::Address;
    ///
    /// // Everything past bit 8 is cleared.
    /// assert_eq!(
    ///     Address([0xFFFF_FFFF, 0xFFFF_FFFF, 0, 0]).masked(8),
    ///     Address([0xFF00_0000, 0, 0, 0])
    /// );
    ///
    /// // Boundary cases.
    /// assert_eq!(Address([0xFFFF_FFFF; 4]).masked(0), Address([0, 0, 0, 0]));
    /// assert_eq!(Address([0xFFFF_FFFF; 4]).masked(128), Address([0xFFFF_FFFF; 4]));
    /// ```
    pub fn masked(self, len: u32) -> Address {
        let mut words = self.0;
        for (word_idx, word) in words.iter_mut().enumerate() {
            let word_start = word_idx as u32 * 32;
            if len <= word_start {
                *word = 0;
            } else if len < word_start + 32 {
                let bits_to_keep = len - word_start;
                let shift = 32 - bits_to_keep;
                *word &= !0u32 << shift;
            }
        }
        Address(words)
    }
}

#[cfg(not(target_arch = "spirv"))]
impl From<u128> for Address {
    /// Construct an (ipv6) Address from a u128
    ///
    /// # Example
    /// ```
    /// use slash0_core::prefix::Address;
    /// let addr: Address = 0xFFFFFFFF_EEEEEEEE_DDDDDDDD_CCCCCCCC_u128.into();
    /// assert_eq!(addr.0, [0xFFFFFFFF, 0xEEEEEEEE, 0xDDDDDDDD, 0xCCCCCCCC]);
    /// ```
    fn from(value: u128) -> Self {
        let mut chunks = [0u32; 4];

        for (i, cell) in chunks.iter_mut().enumerate() {
            *cell = (value >> ((3 - i) * 32) & (!0u32 as u128)) as u32;
        }

        Self(chunks)
    }
}

impl From<u32> for Address {
    /// Construct an (ipv4) Address from a u32
    ///
    /// # Example
    /// ```
    /// use slash0_core::prefix::Address;
    /// let addr: Address = 0xFFFF_EEEEu32.into();
    /// assert_eq!(addr.0, [0xFFFF_EEEE, 0, 0, 0]);
    /// ```
    fn from(value: u32) -> Self {
        Self([value, 0, 0, 0])
    }
}

/// A 128-bit prefix key with an associated bit length.
///
/// Bits at positions `len..128` are guaranteed to be zero when constructed
/// via [`Prefix::new`], which lets two prefixes with equivalent CIDR
/// semantics compare bytewise-equal.
///
/// Laid out as `#[repr(C)]` so it can embed in shader-visible structs.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
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
            bits: Address(bits).masked(len).0,
            len,
        }
    }

    /// Constructs a prefix from the address, masking any bits past `len` to zero.
    ///
    /// Panics if `len > MAX_PREFIX_LEN`.
    ///
    /// # Examples
    ///
    /// ```
    /// use slash0_core::prefix::Prefix;
    /// use slash0_core::prefix::Address;
    ///
    /// let p = Prefix::from_address(Address([!0, !0, !0, !0]), 8);
    /// assert_eq!(p.bits, [0xFF00_0000, 0, 0, 0]);
    /// assert_eq!(p.len, 8);
    /// ```
    pub fn from_address(addr: Address, len: u32) -> Self {
        assert!(len <= MAX_PREFIX_LEN);
        Self {
            bits: addr.masked(len).0,
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
            Address(bits).masked(len).0,
            bits,
            "bits past len must already be zero",
        );
        Self { bits, len }
    }

    /// The bare address, dropping the length.
    pub fn address(&self) -> Address {
        Address(self.bits)
    }

    /// Returns the bit at `pos` (0 or 1). See [`Address::bit_at`] for indexing
    /// rules.
    pub fn bit_at(&self, pos: u32) -> u32 {
        self.address().bit_at(pos)
    }

    /// Length of the longest common prefix between this and `other`, bounded
    /// by `min(self.len, other.len)`.
    pub fn common_prefix_len(&self, other: &Self) -> u32 {
        self.address()
            .common_prefix_len(self.len, &other.address(), other.len)
    }

    /// Whether this prefix covers `addr` -- i.e., its first `self.len` bits
    /// match `addr`'s.
    ///
    /// # Examples
    ///
    /// ```
    /// use slash0_core::prefix::{Address, Prefix};
    ///
    /// let p = Prefix::new([0x0A00_0000, 0, 0, 0], 8);      // 10.0.0.0/8
    /// assert!(p.covers(Address([0x0A01_0203, 0, 0, 0])));  // 10.1.2.3
    /// assert!(!p.covers(Address([0xC000_0201, 0, 0, 0]))); // 192.0.2.1
    /// ```
    pub fn covers(&self, addr: Address) -> bool {
        self.address()
            .common_prefix_len(self.len, &addr, MAX_PREFIX_LEN)
            == self.len
    }
}

/// Why a CIDR string could not be parsed into a [`Prefix`].
#[cfg(feature = "parse")]
#[derive(Debug, thiserror::Error)]
pub enum CidrParseError {
    #[error("missing '/' length separator")]
    MissingSeparator,
    #[error("invalid IP address")]
    Address(#[from] core::net::AddrParseError),
    #[error("invalid prefix length")]
    Length(#[from] core::num::ParseIntError),
    #[error("prefix length {len} exceeds the {max}-bit maximum for its address family")]
    LengthTooLong { len: u32, max: u32 },
}

#[cfg(feature = "parse")]
impl Prefix {
    /// Parses a CIDR string (e.g. `192.0.2.0/24`, `2001:db8::/32`) into the
    /// address family it belongs to and the canonical prefix.
    ///
    /// The family is taken from the parsed address itself. Host bits past the
    /// length are masked off (so `10.1.2.3/8` canonicalizes to `10.0.0.0/8`),
    /// and the length is validated against the family maximum (32 for IPv4, 128
    /// for IPv6) before construction.
    pub fn parse_cidr(cidr: &str) -> Result<(IpVersion, Prefix), CidrParseError> {
        use core::net::IpAddr;

        let (addr, len) = cidr
            .split_once('/')
            .ok_or(CidrParseError::MissingSeparator)?;
        let addr: IpAddr = addr.parse()?;
        let len: u32 = len.parse()?;

        match addr {
            IpAddr::V4(addr) => {
                if len > 32 {
                    return Err(CidrParseError::LengthTooLong { len, max: 32 });
                }
                Ok((IpVersion::V4, Prefix::new([addr.to_bits(), 0, 0, 0], len)))
            }
            IpAddr::V6(addr) => {
                if len > 128 {
                    return Err(CidrParseError::LengthTooLong { len, max: 128 });
                }
                let bits = addr.to_bits();
                let words = [
                    (bits >> 96) as u32,
                    (bits >> 64) as u32,
                    (bits >> 32) as u32,
                    bits as u32,
                ];
                Ok((IpVersion::V6, Prefix::new(words, len)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_at_boundaries() {
        let a = Address([0x8000_0001, 0x8000_0001, 0x8000_0001, 0x8000_0001]);
        assert_eq!(a.bit_at(0), 1);
        assert_eq!(a.bit_at(1), 0);
        assert_eq!(a.bit_at(30), 0);
        assert_eq!(a.bit_at(31), 1);
        assert_eq!(a.bit_at(32), 1);
        assert_eq!(a.bit_at(63), 1);
        assert_eq!(a.bit_at(64), 1);
        assert_eq!(a.bit_at(96), 1);
        assert_eq!(a.bit_at(127), 1);
    }

    #[test]
    fn common_prefix_len_cases() {
        let a = Address([0xFF00_0000, 0, 0, 0]);
        let b = Address([0x8000_0000, 0, 0, 0]);
        assert_eq!(a.common_prefix_len(8, &b, 8), 1);
        assert_eq!(a.common_prefix_len(8, &a, 8), 8);
        let a = Address([0xFFFF_0000, 0, 0, 0]);
        let b = Address([0xFFFE_0000, 0, 0, 0]);
        assert_eq!(a.common_prefix_len(16, &b, 16), 15);
        assert_eq!(a.common_prefix_len(16, &b, 8), 8);
        assert_eq!(a.common_prefix_len(0, &b, 32), 0);
    }

    #[test]
    fn common_prefix_len_across_words() {
        let a = Address([0xFFFF_FFFF, 0xFFFF_0000, 0, 0]);
        let b = Address([0xFFFF_FFFF, 0xFFFC_0000, 0, 0]);
        assert_eq!(a.common_prefix_len(48, &b, 48), 46);
    }

    #[test]
    fn masked_zeroes_trailing_bits() {
        assert_eq!(
            Address([0xFFFF_FFFF, 0xFFFF_FFFF, 0, 0]).masked(12),
            Address([0xFFF0_0000, 0, 0, 0])
        );
        assert_eq!(Address([0xFFFF_FFFF; 4]).masked(0), Address([0, 0, 0, 0]));
        assert_eq!(
            Address([0xFFFF_FFFF; 4]).masked(128),
            Address([0xFFFF_FFFF; 4])
        );
        assert_eq!(
            Address([0xFFFF_FFFF; 4]).masked(33),
            Address([0xFFFF_FFFF, 0x8000_0000, 0, 0])
        );
    }
}

#[cfg(all(test, feature = "parse"))]
mod parse_tests {
    use super::{CidrParseError, IpVersion, Prefix};

    fn parse(cidr: &str) -> (IpVersion, Prefix) {
        Prefix::parse_cidr(cidr).expect("expected a parseable prefix")
    }

    #[test]
    fn parses_ipv4() {
        let (version, prefix) = parse("192.0.2.0/24");
        assert!(matches!(version, IpVersion::V4));
        assert_eq!(prefix, Prefix::new([0xC000_0200, 0, 0, 0], 24));
    }

    #[test]
    fn parses_ipv6() {
        let (version, prefix) = parse("2001:db8::/32");
        assert!(matches!(version, IpVersion::V6));
        assert_eq!(prefix, Prefix::new([0x2001_0db8, 0, 0, 0], 32));
    }

    #[test]
    fn masks_host_bits() {
        let (_, prefix) = parse("10.1.2.3/8");
        assert_eq!(prefix, Prefix::new([0x0A00_0000, 0, 0, 0], 8));
    }

    #[test]
    fn parses_full_length_prefixes() {
        let (_, v4) = parse("192.0.2.1/32");
        assert_eq!(v4, Prefix::new([0xC000_0201, 0, 0, 0], 32));
        let (_, v6) = parse("2001:db8::1/128");
        assert_eq!(v6, Prefix::new([0x2001_0db8, 0, 0, 1], 128));
    }

    #[test]
    fn parses_default_routes() {
        let (v4_version, v4) = parse("0.0.0.0/0");
        assert!(matches!(v4_version, IpVersion::V4));
        assert_eq!(v4, Prefix::new([0, 0, 0, 0], 0));
        let (v6_version, v6) = parse("::/0");
        assert!(matches!(v6_version, IpVersion::V6));
        assert_eq!(v6, Prefix::new([0, 0, 0, 0], 0));
    }

    #[test]
    fn rejects_over_length() {
        assert!(matches!(
            Prefix::parse_cidr("192.0.2.0/33"),
            Err(CidrParseError::LengthTooLong { len: 33, max: 32 })
        ));
        assert!(matches!(
            Prefix::parse_cidr("2001:db8::/129"),
            Err(CidrParseError::LengthTooLong { len: 129, max: 128 })
        ));
    }

    #[test]
    fn rejects_missing_separator() {
        assert!(matches!(
            Prefix::parse_cidr("192.0.2.0"),
            Err(CidrParseError::MissingSeparator)
        ));
        assert!(matches!(
            Prefix::parse_cidr("::"),
            Err(CidrParseError::MissingSeparator)
        ));
        assert!(matches!(
            Prefix::parse_cidr(""),
            Err(CidrParseError::MissingSeparator)
        ));
    }

    #[test]
    fn rejects_bad_address() {
        assert!(matches!(
            Prefix::parse_cidr("not-an-ip/24"),
            Err(CidrParseError::Address(_))
        ));
    }

    #[test]
    fn rejects_bad_length() {
        assert!(matches!(
            Prefix::parse_cidr("192.0.2.0/abc"),
            Err(CidrParseError::Length(_))
        ));
    }
}
