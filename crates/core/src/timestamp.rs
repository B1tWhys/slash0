#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(C)]
pub struct Timestamp(pub [u32; 2]);

impl Timestamp {
    pub const ZERO: Self = Self([0, 0]);

    pub const fn from_sec(s: f64) -> Self {
        Self::from_millis((s * 1000.0) as u64)
    }

    pub const fn from_millis(ms: u64) -> Self {
        Self([(ms >> 32) as u32, ms as u32])
    }

    pub const fn as_millis(self) -> u64 {
        ((self.0[0] as u64) << 32) | (self.0[1] as u64)
    }
}
