//! Encoder backend ids, as they travel in
//! [`SetEncodeRequest::backends`](super::SetEncodeRequest::backends),
//! [`SetEncodeReply::backend_opened`](super::SetEncodeReply::backend_opened) and
//! [`EncodeProbeRequest::backend`](crate::control::EncodeProbeRequest::backend).
//!
//! The host picks by these numbers, the driver opens by them and names them back, so both
//! sides read the table here rather than restating it. A doc that restated it had already
//! drifted: Media Foundation was missing from two of them while the host was sending it.

pub const NVENC: u32 = 1;
pub const AMF: u32 = 2;
pub const QSV: u32 = 3;
pub const PYROWAVE: u32 = 4;
pub const MEDIA_FOUNDATION: u32 = 5;

/// Indexed by `id - 1`; also the stage tag a `SET_ENCODE` reply carries.
pub const NAMES: [&str; 5] = ["nvenc", "amf", "qsv", "pyrowave", "mf"];

#[must_use]
pub fn name(id: u32) -> Option<&'static str> {
    NAMES.get(id.checked_sub(1)? as usize).copied()
}

/// Whether `id` may appear in [`SetEncodeRequest::backends`](super::SetEncodeRequest::backends).
/// `0` terminates the list, so it passes here and is simply never opened.
#[must_use]
pub const fn listed(id: u32) -> bool {
    (id as usize) <= NAMES.len()
}
