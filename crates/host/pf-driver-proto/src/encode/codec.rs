//! Codec ids for [`SetEncodeRequest::codec`](super::SetEncodeRequest::codec) and
//! [`EncodeProbeRequest::codec`](crate::control::EncodeProbeRequest::codec). PyroWave is a
//! codec AND a backend, and only ever pairs with itself.

pub const H264: u32 = 1;
pub const HEVC: u32 = 2;
pub const AV1: u32 = 3;
pub const PYROWAVE: u32 = 4;

pub const NAMES: [&str; 4] = ["h264", "hevc", "av1", "pyrowave"];

#[must_use]
pub fn name(id: u32) -> Option<&'static str> {
    NAMES.get(id.checked_sub(1)? as usize).copied()
}

/// Whether `id` names a codec. Unlike a backend list, `0` is not legal here.
#[must_use]
pub const fn valid(id: u32) -> bool {
    id != 0 && (id as usize) <= NAMES.len()
}
