//! No GPU encode backend on this target: nothing is probed and nothing opens.

use super::*;

pub(crate) fn open(_: &OpenParams) -> Result<(Box<dyn Encoder>, &'static str)> {
    anyhow::bail!("video encode requires Linux or Windows")
}

/// The unprobed advertisement: H.264 for a software pin, else HEVC.
pub(crate) fn wire_caps() -> u8 {
    if matches!(
        pf_host_config::config().encoder_pref.as_str(),
        "software" | "sw" | "openh264"
    ) {
        punktfunk_core::quic::CODEC_H264
    } else {
        punktfunk_core::quic::CODEC_HEVC
    }
}

pub(crate) fn hevc_444() -> bool {
    false
}

pub(crate) fn ten_bit(_codec: Codec) -> bool {
    false
}

pub(crate) fn sdr10(_codec: Codec) -> bool {
    false
}

/// No resolver: only a software pin is a CPU path.
pub(crate) fn is_gpu() -> bool {
    !matches!(
        pf_host_config::config().encoder_pref.as_str(),
        "software" | "sw" | "openh264"
    )
}

pub(crate) fn ingests_rgb_444() -> bool {
    false
}
