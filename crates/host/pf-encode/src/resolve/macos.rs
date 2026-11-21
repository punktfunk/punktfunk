//! macOS backend selection: VideoToolbox is the one encoder, whatever the Encoder row says. Each
//! answer comes from asking VideoToolbox once and keeping it: the codec set from its hardware
//! encoder list, 10-bit and 4:4:4 from whether it opens a session at that profile.

use super::*;
use crate::env_knobs::Knobs;
use crate::videotoolbox::{self, VtEncoder};

/// What resolution found on macOS. Each cell is filled the first time an answer needs it.
#[derive(Debug, Default)]
pub(crate) struct Probes {
    /// The hardware encoder list, kept once it names a codec.
    codecs: Probed<CodecSupport>,
    /// Set when the `codecs` probe found nothing, so the miss warns once.
    codecs_missed: OnceLock<()>,
}

pub(crate) fn open(
    _: &Resolved,
    p: &OpenParams,
    _: &Knobs,
) -> Result<(Box<dyn Encoder>, &'static str)> {
    Ok((Box::new(VtEncoder::open(p)?), "videotoolbox"))
}

/// The codec set VideoToolbox encodes in hardware, else H.264 and HEVC while the list names none.
pub(crate) fn wire_caps(r: &Resolved) -> u8 {
    r.codecs()
        .and_then(codec_support_wire_mask)
        .unwrap_or(punktfunk_core::quic::CODEC_H264 | punktfunk_core::quic::CODEC_HEVC)
}

pub(crate) fn advertised(r: &Resolved) -> Option<CodecSupport> {
    r.codecs()
}

/// Whether a hardware HEVC session opens at the 8-bit 4:4:4 profile.
pub(crate) fn hevc_444(_: &Resolved) -> Option<bool> {
    Some(videotoolbox::opens(
        Codec::H265,
        false,
        ChromaFormat::Yuv444,
    ))
}

/// Whether a hardware HEVC session opens at Main 10. H.264 is 8-bit; AV1 is not wired.
pub(crate) fn ten_bit(_: &Resolved, codec: Codec) -> Option<bool> {
    Some(codec == Codec::H265 && videotoolbox::opens(Codec::H265, true, ChromaFormat::Yuv420))
}

/// VideoToolbox encodes the depth it is handed; it never widens 8-bit SDR to 10.
pub(crate) fn sdr10(_: &Resolved, _codec: Codec) -> bool {
    false
}

/// VideoToolbox converts BGRA to studio range whatever the session asks
/// (`the_stream_signals_the_buffer_range_and_session_colour` pins it).
pub(crate) fn rgb_full_range(_: &Resolved, _codec: Codec) -> RgbFullRange {
    RgbFullRange {
        sdr: false,
        hdr: false,
    }
}

pub(crate) fn vcn_firmware(_: &Resolved) -> Option<VcnFirmware> {
    None
}

/// Capture hands over IOSurface-backed pixel buffers.
pub(crate) fn is_gpu(_: &Resolved) -> bool {
    true
}

/// A 4:4:4 session takes BGRA and VideoToolbox converts it.
pub(crate) fn ingests_rgb_444(_: &Resolved) -> bool {
    true
}

impl Resolved {
    /// VideoToolbox's hardware codecs. `None` while the list names none; it is read again after
    /// [`RETRY_AFTER`].
    fn codecs(&self) -> Option<CodecSupport> {
        self.probes.codecs.ask(|| {
            codec_found(&self.probes.codecs_missed, "VideoToolbox", || {
                let found = videotoolbox::hardware_codecs();
                tracing::info!(
                    h264 = found.h264,
                    h265 = found.h265,
                    "VideoToolbox hardware encoders listed"
                );
                found
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::{CODEC_H264, CODEC_HEVC};

    fn resolved() -> Resolved {
        Resolved::new(ResolveInputs {
            pref: "auto".into(),
            gpu: String::new(),
            vulkan_row: false,
            nvidia: false,
            features: Features::BUILT,
        })
    }

    /// The advertisement is the hardware list, and the superset of the two codecs VideoToolbox
    /// carries while the list is unanswered.
    #[test]
    fn wire_caps_reads_the_hardware_list() {
        let r = resolved();
        r.probes.codecs.fill(CodecSupport {
            h264: false,
            h265: true,
            av1: false,
        });
        assert_eq!(r.wire_caps(), CODEC_HEVC);
        assert_eq!(r.advertised().map(|c| c.h265), Some(true));
        let none = resolved();
        let t0 = std::time::Instant::now();
        assert!(none.probes.codecs.ask_at(t0, || None).is_none());
        assert_eq!(none.wire_caps(), CODEC_H264 | CODEC_HEVC);
    }

    /// PyroWave and AV1 never resolve here, whatever the cells hold.
    #[test]
    fn pyrowave_and_av1_are_never_offered() {
        let r = resolved();
        assert!(!r.can_encode_10bit(Codec::PyroWave));
        assert!(!r.can_encode_10bit(Codec::Av1));
        assert!(!r.can_encode_444(Codec::PyroWave));
        assert!(!r.can_encode_444(Codec::H264));
    }

    /// One resolution asks VideoToolbox and opens the encoder it advertises.
    #[test]
    #[ignore = "needs the media engine"]
    fn a_resolution_asks_videotoolbox_and_opens_it() {
        let r = resolved();
        assert_eq!(r.wire_caps(), CODEC_H264 | CODEC_HEVC);
        let (ten, yuv444) = (
            r.can_encode_10bit(Codec::H265),
            r.can_encode_444(Codec::H265),
        );
        println!("HEVC 10-bit {ten}, 4:4:4 {yuv444}");
        let params = OpenParams {
            codec: Codec::H265,
            format: pf_frame::PixelFormat::P010,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: 20_000_000,
            cuda: false,
            bit_depth: if ten { 10 } else { 8 },
            hdr: ten,
            chroma: ChromaFormat::Yuv420,
            cursor_blend: false,
            slices: 1,
            full_range: false,
        };
        let enc = crate::open_video(&r, params).expect("open_video");
        assert!(!enc.caps().supports_rfi, "loss recovery is a keyframe");
    }
}
