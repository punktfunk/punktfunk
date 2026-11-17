//! Every environment variable this crate reads itself. [`Knobs::from_env`] reads them once per
//! open and the value rides beside [`crate::OpenParams`] to the backend; nothing else in the
//! crate reads the environment. Not `knobs`: that public name is pf-encode-core's `EncodeKnobs` module. The
//! console rows (`pf_host_config`) and the worker path (pf-zerocopy's `resolve_worker_path`) are
//! read where they live.
// A build reads only its own backends' knobs; off Linux `open_video` builds the value unread.
#![cfg_attr(
    not(all(
        target_os = "linux",
        feature = "nvenc",
        feature = "vulkan-encode",
        feature = "pyrowave"
    )),
    allow(dead_code)
)]

/// The operator's encode hatches, parsed. `None` is unset or unparseable: the backend keeps its
/// own default.
#[derive(Clone, Debug)]
pub(crate) struct Knobs {
    /// `PUNKTFUNK_VULKAN_INFLIGHT`: Vulkan Video captures in flight, clamped to 2..=6.
    pub(crate) vulkan_inflight: Option<usize>,
    /// `PUNKTFUNK_VULKAN_QUALITY`: the Vulkan Video quality level, fastest first. The open clamps
    /// it below the driver's `maxQualityLevels`; unset is 0.
    pub(crate) vulkan_quality: Option<u32>,
    /// `PUNKTFUNK_VULKAN_RGB_DIRECT`: the RGB-direct (EFC) encode source
    /// (`design/vulkan-rgb-direct-encode.md`). `Some(false)` turns it off. `Some(true)` acts as
    /// `None`, the default: on wherever the probe passes, and on a session that composites the
    /// pointer only while no cursor is visible, since EFC cannot blend. An unrecognised value is
    /// `None`.
    pub(crate) vulkan_rgb_direct: Option<bool>,
    /// `PUNKTFUNK_VULKAN_RGB_TRUE_EXTENT`, default on: an unaligned EFC source imports at its true
    /// size so RADV pads it in firmware. Off restores the padded copy.
    pub(crate) vulkan_rgb_true_extent: bool,
    /// `PUNKTFUNK_VULKAN_DIRECT_PLANES`, default on: the CSC writes the picture planes where the
    /// driver lists the format as a storage target. Off keeps the scratch-plane copies (the A/B
    /// for a driver that lists it and misrenders).
    pub(crate) vulkan_direct_planes: bool,
    /// `PUNKTFUNK_VULKAN_RC`: `cbr` forces CBR; anything else lets the driver's VBR win.
    pub(crate) vulkan_rc: Option<String>,
    /// `PUNKTFUNK_VAAPI_DUMP=<file>`: every native VAAPI access unit, for a decoder to look at.
    pub(crate) vaapi_dump: Option<String>,
    /// `PUNKTFUNK_OH264_GOP`: openh264's intra period in frames (0 = encoder-auto).
    pub(crate) oh264_gop: Option<u32>,
    /// `PUNKTFUNK_OH264_THREADS`: openh264 encode threads.
    pub(crate) oh264_threads: Option<u16>,
    /// `PUNKTFUNK_NVENC_STREAM_ORDERED`, default on: stream-ordered NVENC submit. Off is
    /// blocking copies.
    pub(crate) nvenc_stream_ordered: bool,
    /// `PYROWAVE_QUEUE_PRIORITY`: the raw global-priority intent, resolved by the PyroWave open
    /// (in this process or forwarded to the encode worker).
    pub(crate) queue_priority: Option<String>,
    /// `PUNKTFUNK_AMF_FULL_RANGE`: whether AMF's own RGB conversion on this GPU writes full
    /// range. Unset measures it; off streams studio; on trusts the GPU without measuring.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) amf_full_range: Option<bool>,
}

impl Knobs {
    pub(crate) fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok();
        Self {
            vulkan_inflight: parse::<usize>(var("PUNKTFUNK_VULKAN_INFLIGHT"))
                .map(|n| n.clamp(2, 6)),
            vulkan_quality: parse(var("PUNKTFUNK_VULKAN_QUALITY")),
            vulkan_rgb_direct: parse_rgb_request(var("PUNKTFUNK_VULKAN_RGB_DIRECT").as_deref()),
            vulkan_rgb_true_extent: pf_host_config::env_on("PUNKTFUNK_VULKAN_RGB_TRUE_EXTENT")
                .unwrap_or(true),
            vulkan_direct_planes: pf_host_config::env_on("PUNKTFUNK_VULKAN_DIRECT_PLANES")
                .unwrap_or(true),
            vulkan_rc: var("PUNKTFUNK_VULKAN_RC"),
            vaapi_dump: var("PUNKTFUNK_VAAPI_DUMP"),
            oh264_gop: parse(var("PUNKTFUNK_OH264_GOP")),
            oh264_threads: parse(var("PUNKTFUNK_OH264_THREADS")),
            nvenc_stream_ordered: pf_host_config::env_on("PUNKTFUNK_NVENC_STREAM_ORDERED")
                != Some(false),
            queue_priority: var("PYROWAVE_QUEUE_PRIORITY"),
            amf_full_range: pf_host_config::env_on("PUNKTFUNK_AMF_FULL_RANGE"),
        }
    }

    /// `ANV_DEBUG`, Mesa's Intel driver flags, which the host extends at startup. Read alone:
    /// [`Self::from_env`] reads the host config, which does not exist yet there.
    pub(crate) fn anv_debug() -> Option<String> {
        std::env::var("ANV_DEBUG").ok()
    }
}

fn parse<T: std::str::FromStr>(raw: Option<String>) -> Option<T> {
    raw?.trim().parse().ok()
}

/// The registry's boolean grammar, any case. Pure, so parallel tests need not mutate the
/// process environment.
fn parse_rgb_request(raw: Option<&str>) -> Option<bool> {
    pf_host_config::registry::parse_bool(&raw?.trim().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `PUNKTFUNK_VULKAN_RGB_DIRECT` accepts the same spellings as every sibling knob, trimmed,
    /// in any case.
    #[test]
    fn rgb_direct_knob_accepts_the_house_spellings() {
        for on in ["1", "true", "yes", "on", " 1", "1 ", "\ton\n", "TRUE", "On"] {
            assert_eq!(parse_rgb_request(Some(on)), Some(true), "{on:?}");
        }
        for off in [
            "0", "false", "no", "off", " 0", "0 ", "\toff\n", "FALSE", "Off",
        ] {
            assert_eq!(parse_rgb_request(Some(off)), Some(false), "{off:?}");
        }
    }

    /// Unset, empty, and unrecognised values mean default — never a force-on.
    #[test]
    fn rgb_direct_knob_never_force_enables_on_an_unrecognised_value() {
        assert_eq!(parse_rgb_request(None), None);
        for junk in ["", "   ", "2", "maybe", "0x0", "enabled"] {
            assert_eq!(parse_rgb_request(Some(junk)), None, "{junk:?}");
        }
    }
}
