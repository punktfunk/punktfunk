//! The audio plane a session resolved, and the decoder every client runs on it.
//!
//! [`PlaneFormat::from_welcome`] is the one clamp table for the `Welcome` audio fields: the C ABI,
//! the desktop client and Android size every buffer from it. [`PlaneDecoder`] decodes into a fixed
//! slice: Opus on `0xC9`, raw PCM on `0xD3` ([`pcm`]). AC-3 / E-AC-3 on `0xD4` ([`bitstream`])
//! has no decoder here; a passthrough sink takes its packets.

use super::{bitstream, pcm};

/// The `Welcome` audio fields, clamped so every buffer and divisor can be built from them.
///
/// Everything here is what the host resolved, never what the client asked for. A 48 kHz Opus
/// session and a 48 kHz/16-bit PCM one agree on every field but `codec`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneFormat {
    /// [`AUDIO_CODEC_PCM`](crate::quic::AUDIO_CODEC_PCM) selects the PCM plane,
    /// [`bitstream::is_bitstream`] the passthrough plane; anything else decodes as Opus.
    pub codec: u8,
    /// Never 0. 48 000 on bitstream (forced) and on Opus (from a conforming host); any
    /// [`pcm::rate_is_supported`] rate on PCM.
    pub rate_hz: u32,
    /// PCM unpack stride, 16 or 24. Unused on Opus and bitstream.
    pub bits: u8,
    /// 2, 6 or 8 ([`super::normalize_channels`]).
    pub channels: u8,
    /// One protocol frame: 5 ms on Opus, a [`pcm::FRAME_US_LADDER`] rung on PCM, an access unit's
    /// [`bitstream::frame_us`] label on bitstream. A label, not a duration: size from
    /// [`Self::frame_samples`], time from [`pcm::frame_duration_ns`].
    pub frame_us: u32,
    /// Surround coupling, the [`super::AudioLayout`] wire id verbatim.
    pub layout: u8,
}

impl PlaneFormat {
    /// Clamp a host's audio fields, from the `Welcome` or a later `AudioConfig`. Each clamp
    /// guards a buffer or a divisor against a non-conforming host; a conforming one passes
    /// through unchanged. A rate the plane does not carry reads as 48 kHz: playout sizes its
    /// ring from it. PCM `frame_us` stays on the ladder, because decode scratch holds its
    /// longest rung; 0 (no duration stated) reads as that rung. Opus frames are always 5 ms.
    /// Bitstream is 48 kHz, and its label snaps to the nearest block count
    /// ([`bitstream::blocks_for`]).
    pub fn from_welcome(
        codec: u8,
        rate_hz: u32,
        bits: u8,
        channels: u8,
        frame_us: u32,
        layout: u8,
    ) -> PlaneFormat {
        let ladder = pcm::FRAME_US_LADDER;
        let rate_hz = if !pcm::rate_is_supported(rate_hz) || bitstream::is_bitstream(codec) {
            super::SAMPLE_RATE_HZ
        } else {
            rate_hz
        };
        let frame_us = match (codec, frame_us) {
            (c, us) if bitstream::is_bitstream(c) => {
                u32::from(bitstream::frame_us(c, bitstream::blocks_for(c, us)))
            }
            (crate::quic::AUDIO_CODEC_PCM, 0) => ladder[0],
            (crate::quic::AUDIO_CODEC_PCM, us) => us.clamp(ladder[ladder.len() - 1], ladder[0]),
            _ => super::FRAME_MS * 1000,
        };
        PlaneFormat {
            codec,
            rate_hz,
            bits,
            channels: super::normalize_channels(channels),
            frame_us,
            layout,
        }
    }

    /// The session's resolved format, read off the connector.
    #[cfg(feature = "quic")]
    pub fn of(c: &crate::client::NativeClient) -> PlaneFormat {
        PlaneFormat::from_welcome(
            c.audio_codec,
            c.audio_sample_rate_hz,
            c.audio_bits,
            c.audio_channels,
            u32::from(c.audio_frame_us),
            c.audio_layout,
        )
    }

    /// True on the lossless `0xD3` plane.
    pub fn is_pcm(&self) -> bool {
        self.codec == crate::quic::AUDIO_CODEC_PCM
    }

    /// True on the `0xD4` passthrough plane.
    pub fn is_bitstream(&self) -> bool {
        bitstream::is_bitstream(self.codec)
    }

    /// `ms` of audio in interleaved samples, exact at every supported rate.
    pub fn ms_samples(&self, ms: u32) -> usize {
        super::ms_to_samples(self.rate_hz, self.channels, ms)
    }

    /// Interleaved samples back to whole milliseconds; the inverse of [`Self::ms_samples`].
    pub fn samples_ms(&self, samples: usize) -> u32 {
        super::samples_to_ms(self.rate_hz, self.channels, samples)
    }

    /// Interleaved samples in one frame, floored per channel as the host fills it. 5 ms at
    /// 44 100 Hz stereo is 440, not the 441 that 5 ms of audio would be. A bitstream access
    /// unit is whole blocks, never its rounded label.
    pub fn frame_samples(&self) -> usize {
        if self.is_bitstream() {
            return bitstream::frame_samples(self.codec, self.frame_us)
                * usize::from(self.channels);
        }
        pcm::samples_per_frame(self.rate_hz, self.frame_us, self.channels)
    }

    /// Interleaved samples in the largest frame this plane can carry: 120 ms on Opus (the
    /// codec's maximum), the longest ladder rung on PCM, 6 blocks on bitstream. Decode scratch
    /// is sized from this.
    pub fn max_frame_samples(&self) -> usize {
        if self.is_bitstream() {
            return 6 * bitstream::BLOCK_SAMPLES * usize::from(self.channels);
        }
        let us = if self.is_pcm() {
            pcm::FRAME_US_LADDER[0]
        } else {
            120_000
        };
        pcm::samples_per_frame(self.rate_hz, us, self.channels)
    }
}

/// Why a [`PlaneDecoder`] did not build, or a frame did not decode.
#[cfg(feature = "quic")]
#[derive(Debug)]
pub enum PlaneError {
    /// An Opus coupling this build does not know. A guess would play the wrong speakers.
    UnknownLayout(u8),
    Opus(opus::Error),
    /// A PCM payload that is not a whole number of samples at the negotiated depth.
    Ragged,
    /// A `0xD4` codec: core has no decoder for it.
    Passthrough(u8),
}

#[cfg(feature = "quic")]
impl std::fmt::Display for PlaneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlaneError::UnknownLayout(id) => write!(f, "unknown audio layout {id}"),
            PlaneError::Opus(e) => write!(f, "opus: {e}"),
            PlaneError::Ragged => f.write_str("PCM payload is not a whole number of samples"),
            PlaneError::Passthrough(id) => write!(
                f,
                "no decoder for audio codec {id}: hand its packets to a passthrough sink"
            ),
        }
    }
}

#[cfg(feature = "quic")]
impl std::error::Error for PlaneError {}

/// Decoder for the Opus and PCM planes, writing interleaved f32 into a caller-owned slice.
///
/// Opus runs one libopus multistream decoder; stereo is a single coupled stream. PCM is a stride
/// unpack and conceals with [`pcm::PcmConceal`], since a raw frame has no state to interpolate
/// from. Counts are per channel. `out` never grows: a frame that does not fit truncates to the
/// whole frames that do, so a pointer an embedder holds into it stays valid.
///
/// Empty input is Opus DTX or a torn PCM datagram: `Ok(0)`, nothing written. libopus would read
/// it as a loss and fill `out` with PLC, and `PcmConceal::accept` would drop the frame the next
/// loss repeats.
#[cfg(feature = "quic")]
pub struct PlaneDecoder {
    channels: usize,
    kind: Kind,
}

#[cfg(feature = "quic")]
enum Kind {
    Opus(opus::MSDecoder),
    /// `scratch` stages the unpack: `pcm::to_f32` writes a `Vec`, never the caller's slice.
    Pcm {
        bits: u8,
        scratch: Vec<f32>,
        conceal: pcm::PcmConceal,
    },
}

#[cfg(feature = "quic")]
impl PlaneDecoder {
    /// Build for the plane the host resolved. PCM has no coupling and ignores `layout`; Opus
    /// refuses one this build does not know. libopus takes only 8, 12, 16, 24 and 48 kHz.
    /// Bitstream is [`PlaneError::Passthrough`].
    pub fn new(fmt: &PlaneFormat) -> Result<PlaneDecoder, PlaneError> {
        if fmt.is_bitstream() {
            return Err(PlaneError::Passthrough(fmt.codec));
        }
        let kind = if fmt.is_pcm() {
            Kind::Pcm {
                bits: fmt.bits,
                scratch: Vec::with_capacity(fmt.max_frame_samples()),
                conceal: pcm::PcmConceal::new(),
            }
        } else {
            let layout = super::AudioLayout::from_wire(fmt.layout)
                .ok_or(PlaneError::UnknownLayout(fmt.layout))?;
            let l = super::layout_for(fmt.channels, layout);
            let dec = opus::MSDecoder::new(fmt.rate_hz, l.streams, l.coupled, l.mapping)
                .map_err(PlaneError::Opus)?;
            Kind::Opus(dec)
        };
        Ok(PlaneDecoder {
            channels: usize::from(fmt.channels.max(1)),
            kind,
        })
    }

    /// Decode one arrived frame into `out`; returns samples per channel.
    pub fn decode(&mut self, input: &[u8], out: &mut [f32]) -> Result<usize, PlaneError> {
        if input.is_empty() {
            return Ok(0);
        }
        let ch = self.channels;
        match &mut self.kind {
            Kind::Opus(d) => d.decode_float(input, out, false).map_err(PlaneError::Opus),
            Kind::Pcm {
                bits,
                scratch,
                conceal,
            } => {
                let n = pcm::to_f32(input, *bits, scratch).ok_or(PlaneError::Ragged)?;
                let n = whole_frames(n.min(out.len()), ch);
                if n == 0 {
                    return Ok(0);
                }
                out[..n].copy_from_slice(&scratch[..n]);
                // Conceal from what was written, so the source stays inside `out`'s bound.
                conceal.accept(&out[..n]);
                Ok(n / ch)
            }
        }
    }

    /// Synthesise one frame for a datagram that never arrived; returns samples per channel, 0
    /// when there is nothing to build from yet. `frame_samples` is the last decoded frame per
    /// channel, the unit libopus PLC fills; PCM repeats its own last frame and ignores it.
    pub fn conceal(&mut self, frame_samples: usize, out: &mut [f32]) -> Result<usize, PlaneError> {
        let ch = self.channels;
        match &mut self.kind {
            Kind::Opus(d) => {
                let plc = (frame_samples * ch).min(out.len());
                if plc == 0 {
                    return Ok(0);
                }
                d.decode_float(&[], &mut out[..plc], false)
                    .map_err(PlaneError::Opus)
            }
            Kind::Pcm {
                scratch, conceal, ..
            } => {
                if !conceal.conceal(scratch) {
                    return Ok(0);
                }
                let n = whole_frames(scratch.len().min(out.len()), ch);
                out[..n].copy_from_slice(&scratch[..n]);
                Ok(n / ch)
            }
        }
    }
}

/// `samples` floored to whole interleaved frames; a partial frame walks the channels around.
#[cfg(feature = "quic")]
fn whole_frames(samples: usize, channels: usize) -> usize {
    samples - samples % channels
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::{AUDIO_CODEC_AC3, AUDIO_CODEC_EAC3, AUDIO_CODEC_OPUS, AUDIO_CODEC_PCM};

    /// The clamp table. Every row but the first is a host out of spec.
    #[test]
    fn a_welcome_clamps_to_a_format_every_buffer_can_be_sized_from() {
        // A pre-lossless host sends none of the fields.
        let legacy = PlaneFormat::from_welcome(AUDIO_CODEC_OPUS, 0, 0, 0, 0, 0);
        assert!(!legacy.is_pcm());
        assert_eq!(legacy.rate_hz, crate::audio::SAMPLE_RATE_HZ);
        assert_eq!(legacy.channels, 2);
        assert_eq!(legacy.frame_us, 5_000);
        // `audio_frame_us` is a `0xD3` field; Opus frames stay 5 ms.
        let opus = PlaneFormat::from_welcome(AUDIO_CODEC_OPUS, 48_000, 16, 2, 2_000, 0);
        assert_eq!(opus.frame_us, 5_000);
        // PCM stays on the ladder: no duration reads as the longest rung, and the scratch
        // holds no longer frame than that.
        let pcm_at = |us| PlaneFormat::from_welcome(AUDIO_CODEC_PCM, 96_000, 24, 6, us, 0);
        assert_eq!(pcm_at(0).frame_us, 5_000);
        assert_eq!(pcm_at(60_000).frame_us, 5_000);
        assert_eq!(pcm_at(500).frame_us, 1_000);
        assert_eq!(pcm_at(2_000).frame_us, 2_000);
        assert_eq!(pcm_at(2_000).channels, 6);
        // 3 channels is no layout a decoder or device opens.
        assert_eq!(
            PlaneFormat::from_welcome(AUDIO_CODEC_PCM, 44_100, 24, 3, 5_000, 0).channels,
            2
        );
        // An `AudioConfig` rate no plane carries would size a ring of gigabytes.
        let huge = PlaneFormat::from_welcome(AUDIO_CODEC_PCM, 4_000_000_000, 24, 8, 5_000, 0);
        assert_eq!(huge.rate_hz, crate::audio::SAMPLE_RATE_HZ);
        assert_eq!(pcm_at(5_000).rate_hz, 96_000);
    }

    /// Bitstream runs at 48 kHz and its label snaps to a block count; AC-3 is always 6 blocks.
    #[test]
    fn a_bitstream_welcome_snaps_to_whole_blocks() {
        let eac3 = |us| PlaneFormat::from_welcome(AUDIO_CODEC_EAC3, 96_000, 24, 6, us, 0);
        assert!(eac3(0).is_bitstream() && !eac3(0).is_pcm());
        assert_eq!(eac3(0).rate_hz, 48_000, "the rate is forced");
        assert_eq!(eac3(0).frame_us, 16_000, "the host default, 3 blocks");
        assert_eq!(eac3(5_000).frame_us, 5_333);
        assert_eq!(eac3(10_667).frame_us, 10_667);
        assert_eq!(eac3(30_000).frame_us, 32_000);
        assert_eq!(eac3(5_333).frame_samples(), 256 * 6);
        assert_eq!(eac3(5_333).max_frame_samples(), 1_536 * 6);
        let ac3 = PlaneFormat::from_welcome(AUDIO_CODEC_AC3, 0, 16, 2, 5_333, 0);
        assert_eq!((ac3.rate_hz, ac3.frame_us), (48_000, 32_000));
        assert_eq!(ac3.frame_samples(), 1_536 * 2);
    }

    /// A frame is the wire's sample count, not its label's milliseconds.
    #[test]
    fn a_frame_is_the_wires_sample_count() {
        let f = PlaneFormat::from_welcome(AUDIO_CODEC_PCM, 44_100, 24, 2, 5_000, 0);
        assert_eq!(f.frame_samples(), 440, "220 per channel, floored");
        assert_eq!(f.ms_samples(5), 441);
        assert_eq!(f.samples_ms(88_200), 1_000);
        assert_eq!(f.max_frame_samples(), 440);
        let opus = PlaneFormat::from_welcome(AUDIO_CODEC_OPUS, 48_000, 16, 8, 0, 0);
        assert_eq!(
            opus.max_frame_samples(),
            5_760 * 8,
            "120 ms, the Opus maximum"
        );
    }

    /// Opus refuses a coupling it does not know; PCM has none to refuse.
    #[cfg(feature = "quic")]
    #[test]
    fn only_opus_refuses_an_unknown_layout() {
        let opus = PlaneFormat::from_welcome(AUDIO_CODEC_OPUS, 48_000, 16, 6, 0, 9);
        assert!(matches!(
            PlaneDecoder::new(&opus),
            Err(PlaneError::UnknownLayout(9))
        ));
        let pcm = PlaneFormat::from_welcome(AUDIO_CODEC_PCM, 96_000, 24, 6, 2_000, 9);
        assert!(PlaneDecoder::new(&pcm).is_ok());
    }

    /// A passthrough codec builds no decoder: its packets are for the receiver.
    #[cfg(feature = "quic")]
    #[test]
    fn a_bitstream_plane_has_no_decoder() {
        for codec in [AUDIO_CODEC_AC3, AUDIO_CODEC_EAC3] {
            let fmt = PlaneFormat::from_welcome(codec, 48_000, 16, 6, 0, 0);
            let err = PlaneDecoder::new(&fmt).err().expect("no decoder");
            assert!(matches!(err, PlaneError::Passthrough(c) if c == codec));
            assert!(err.to_string().starts_with("no decoder for audio codec"));
        }
    }

    /// PCM: bit-exact unpack, repeat-and-fade concealment, torn datagrams refused.
    #[cfg(feature = "quic")]
    #[test]
    fn the_pcm_plane_decodes_and_conceals_per_channel() {
        let fmt = PlaneFormat::from_welcome(AUDIO_CODEC_PCM, 96_000, pcm::BITS_24, 2, 2_000, 0);
        let mut dec = PlaneDecoder::new(&fmt).expect("PCM builds no codec");
        let mut out = vec![0f32; fmt.max_frame_samples()];
        // Nothing to repeat yet: the caller lets the ring carry the gap.
        assert_eq!(dec.conceal(0, &mut out).unwrap(), 0);

        let frame = fmt.frame_samples();
        let mut wire = Vec::new();
        pcm::from_f32(&vec![0.5f32; frame], pcm::BITS_24, &mut wire);
        assert_eq!(dec.decode(&wire, &mut out).unwrap(), frame / 2);
        assert!(out[..frame].iter().all(|s| (s - 0.5).abs() < 1e-3));
        assert_eq!(dec.conceal(0, &mut out).unwrap(), frame / 2);

        assert!(matches!(
            dec.decode(&wire[..wire.len() - 1], &mut out),
            Err(PlaneError::Ragged)
        ));
        // A torn empty datagram keeps the frame to repeat.
        assert_eq!(dec.decode(&[], &mut out).unwrap(), 0);
        assert_eq!(dec.conceal(0, &mut out).unwrap(), frame / 2);
    }

    /// An oversized or odd-length PCM frame truncates to whole frames inside `out`.
    #[cfg(feature = "quic")]
    #[test]
    fn a_pcm_frame_that_does_not_fit_truncates_to_whole_frames() {
        let fmt = PlaneFormat::from_welcome(AUDIO_CODEC_PCM, 48_000, pcm::BITS_16, 2, 5_000, 0);
        let mut dec = PlaneDecoder::new(&fmt).unwrap();
        let mut out = vec![7f32; 101];
        let mut wire = Vec::new();
        pcm::from_f32(&[0.25f32; 481], pcm::BITS_16, &mut wire);
        assert_eq!(dec.decode(&wire, &mut out).unwrap(), 50);
        assert_eq!(out[100], 7.0, "the partial frame is never written");
        // The concealment source is what was written, not the oversized datagram.
        assert_eq!(dec.conceal(0, &mut out).unwrap(), 50);
        // One sample of a stereo frame is no frame at all, and not a new concealment source.
        let mut one = Vec::new();
        pcm::from_f32(&[0.25f32], pcm::BITS_16, &mut one);
        assert_eq!(dec.decode(&one, &mut out).unwrap(), 0);
        assert_eq!(dec.conceal(0, &mut out).unwrap(), 50);
    }

    /// Opus: DTX writes nothing, PLC fills exactly the frame it is asked for.
    #[cfg(feature = "quic")]
    #[test]
    fn the_opus_plane_decodes_and_conceals_per_channel() {
        let mut enc = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)
            .expect("opus encoder");
        let mut packet = [0u8; 4_000];
        let n = enc
            .encode_float(&[0.0f32; 240 * 2], &mut packet)
            .expect("encode one 5 ms stereo frame");
        let fmt = PlaneFormat::from_welcome(AUDIO_CODEC_OPUS, 48_000, 16, 2, 0, 0);
        let mut dec = PlaneDecoder::new(&fmt).expect("opus decoder");
        let mut out = vec![0f32; fmt.max_frame_samples()];
        // Nothing decoded yet: no frame length to ask PLC for.
        assert_eq!(dec.conceal(0, &mut out).unwrap(), 0);
        assert_eq!(dec.decode(&packet[..n], &mut out).unwrap(), 240);
        out.fill(7.0);
        assert_eq!(dec.decode(&[], &mut out).unwrap(), 0);
        assert!(out.iter().all(|&s| s == 7.0), "DTX runs no PLC");
        assert_eq!(dec.conceal(240, &mut out).unwrap(), 240);
        assert_eq!(dec.conceal(240, &mut []).unwrap(), 0);
    }
}
