pub const WIRE_VERSION: u16 = 0;

#[derive(Copy, Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(u8)]
pub enum IpVersion {
    V4 = 4,
    V6 = 6,
}
