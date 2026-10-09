//! Lossless PCM payload for the `0xD3` audio plane.
//!
//! Interleaved little-endian integer samples. No codec, no container. The `0xC9`
//! plane is Opus at 48 kHz only; this one carries 16/24-bit PCM at the 48 kHz and
//! 44.1 kHz families.
//!
//! [`from_f32`] quantises f32 once, with no dither. After that, [`from_f32`] →
//! [`to_f32`] → [`from_f32`] is the identity (see the round-trip test). A datagram
//! larger than the path MTU is not sent, and this plane is never fragmented, so
//! [`frame_us_for`] sizes from the raw frame.
//!
//! Why not FLAC: a VERBATIM subframe is raw samples plus a header, so the worst
//! case — the only case this plane can provision, sitting outside ABR — does not
//! shrink. Evidence: `design/hi-res-audio.md`.

/// Tag + `u32` seq + `u64` pts_ns. Same shape as `0xC9` so the gap tracker and
/// A/V-sync plumbing stay shared. `quic::datagram` asserts this against its encoder.
pub const PCM_HEADER_LEN: usize = 1 + 4 + 8;

/// Frame durations the plane may negotiate, longest first.
///
/// Every rung divides the 48 kHz family into whole samples per channel.
/// The 44.1 kHz family does not: a rung is a whole sample only when
/// `rate_hz × µs` is a multiple of 1_000_000, so 44_100 has none of these
/// rungs, 88_200 has 5000, 176_400 has 5000 and 2500. Other pairings floor
/// in [`samples_per_frame`] and run shorter than the label.
///
/// A rung is a wire/ring size, not a duration. Advance `pts_ns` with
/// [`frame_duration_ns`] of the real sample count, never `frame_us`.
pub const FRAME_US_LADDER: [u32; 7] = [5000, 4000, 3000, 2500, 2000, 1500, 1000];

/// 32-bit float is absent: 24 bits already captures the mix, and 32 would cost 33 % more.
pub const BITS_16: u8 = 16;
pub const BITS_24: u8 = 24;

/// Symmetric full-scale. The most-negative code (`-2^(n-1)`) is unused so
/// [`from_f32`]/[`to_f32`] round-trip both ways; using it would fold one code
/// onto its neighbour and make the bit-exactness test unsatisfiable.
const fn full_scale(bits: u8) -> i32 {
    match bits {
        BITS_16 => 32_767,
        _ => 8_388_607,
    }
}

pub const fn bytes_per_sample(bits: u8) -> usize {
    match bits {
        BITS_16 => 2,
        _ => 3,
    }
}

pub const fn depth_is_supported(bits: u8) -> bool {
    matches!(bits, BITS_16 | BITS_24)
}

/// Sample rates this plane can carry. One expression, so host negotiation and
/// client validation cannot drift.
///
/// 48 kHz family (48_000 / 96_000) and 44.1 kHz family (44_100 / 88_200 /
/// 176_400). A supported rate is not a promise the path can afford it: most
/// 44.1 rungs floor in [`FRAME_US_LADDER`], and 176_400/24-bit stereo is 8.5 Mbps
/// the ABR loop cannot reclaim. 192 kHz is out of scope; nothing here would reject it.
pub const fn rate_is_supported(rate_hz: u32) -> bool {
    matches!(rate_hz, 44_100 | 48_000 | 88_200 | 96_000 | 176_400)
}

/// Interleaved samples in one `frame_us` frame (per channel × channels).
///
/// Host fill and client drain both use this, so they agree by construction.
/// A second derivation is a second rounding; one sample off on an interleaved
/// stream walks the channels around each other.
///
/// Floors per channel: 220.5 samples do not exist. At 44_100 Hz a nominal
/// 5 ms frame is 220 samples/ch — [`frame_duration_ns`] of it is 4_988_662 ns.
/// Size from this; time from [`frame_duration_ns`].
pub const fn samples_per_frame(rate_hz: u32, frame_us: u32, channels: u8) -> usize {
    // Multiply first (`rate_hz / 1_000_000` is 0 below 1 MHz). u64: `usize` is
    // 32 bits on some embedders, and 176_400 Hz against a long frame wraps.
    // Saturate: a wrapped count is a small one, which under-sizes the buffer.
    let total = (rate_hz as u64 * frame_us as u64 / 1_000_000) * channels as u64;
    if total > u32::MAX as u64 {
        u32::MAX as usize
    } else {
        total as usize
    }
}

/// Real duration of `samples` interleaved samples, in nanoseconds.
///
/// Inverse of [`samples_per_frame`], and the only figure a `pts_ns` may
/// advance by. Feed the session's cumulative sample count
/// (`pts_ns = base + frame_duration_ns(samples_so_far, …)`): a per-frame
/// call floors (~0.2 µs/s at 44.1 kHz). Advancing by `frame_us` is 0.23 %
/// fast at 44_100 Hz.
///
/// `channels` is the interleaved count, so
/// `frame_duration_ns(samples_per_frame(r, us, ch), r, ch) <= us × 1000`.
pub const fn frame_duration_ns(samples: usize, rate_hz: u32, channels: u8) -> u64 {
    // u128 so the numerator cannot overflow for any `usize` a caller can hand us.
    let per_sec = rate_hz as u128 * channels as u128;
    if per_sec == 0 {
        return 0; // no duration rather than a division fault
    }
    let ns = samples as u128 * 1_000_000_000 / per_sec;
    if ns > u64::MAX as u128 {
        u64::MAX
    } else {
        ns as u64
    }
}

/// Wire bytes of one `frame_us` frame, excluding [`PCM_HEADER_LEN`].
pub const fn frame_payload_bytes(rate_hz: u32, bits: u8, channels: u8, frame_us: u32) -> usize {
    samples_per_frame(rate_hz, frame_us, channels) * bytes_per_sample(bits)
}

/// Payload kbps for [`crate::audio::plan_audio_budget`]. Floors: 44_100/16-bit
/// is 1_411.2, reported as 1_411. Rounding down keeps a borderline session
/// from being declined over 0.2 kbps it would have had.
pub const fn bitrate_kbps(rate_hz: u32, bits: u8, channels: u8) -> u32 {
    (rate_hz as u64 * bits as u64 * channels as u64 / 1000) as u32
}

/// Longest [`FRAME_US_LADDER`] rung whose frame fits one `max_datagram` byte
/// datagram, or `None` if even the shortest does not.
///
/// Sized from the raw frame: this plane is never fragmented, so the only
/// safe bound is the size the payload cannot exceed. Call after QUIC MTU
/// discovery has settled, or the session stays on the conservative initial
/// MTU for its whole life.
///
/// Fit goes through [`samples_per_frame`], which floors, so a 44.1-family
/// frame is at most the rung's nominal size — the rounding that keeps an
/// oversized datagram from being chosen.
pub fn frame_us_for(rate_hz: u32, bits: u8, channels: u8, max_datagram: usize) -> Option<u32> {
    let budget = max_datagram.checked_sub(PCM_HEADER_LEN)?;
    FRAME_US_LADDER
        .iter()
        .copied()
        .find(|&us| frame_payload_bytes(rate_hz, bits, channels, us) <= budget)
}

/// Quantise one interleaved f32 frame onto the wire, appending to `out`.
///
/// Scale-and-clamp, no dither: the mix is already quantised upstream, and
/// dither would add noise while breaking the bit-exact round trip.
pub fn from_f32(samples: &[f32], bits: u8, out: &mut Vec<u8>) {
    let fs = full_scale(bits);
    let scale = fs as f32;
    out.reserve(samples.len() * bytes_per_sample(bits));
    if bits == BITS_16 {
        for &s in samples {
            let v = (s * scale).round().clamp(-scale, scale) as i32;
            out.extend_from_slice(&(v as i16).to_le_bytes());
        }
    } else {
        for &s in samples {
            let v = (s * scale).round().clamp(-scale, scale) as i32;
            let b = v.to_le_bytes();
            out.extend_from_slice(&b[..3]);
        }
    }
}

/// Reverse of [`from_f32`]. `None` if `bytes` is not a whole number of samples at `bits`.
pub fn to_f32(bytes: &[u8], bits: u8, out: &mut Vec<f32>) -> Option<usize> {
    let step = bytes_per_sample(bits);
    if bytes.len() % step != 0 {
        return None;
    }
    let inv = 1.0 / full_scale(bits) as f32;
    out.clear();
    out.reserve(bytes.len() / step);
    if bits == BITS_16 {
        for c in bytes.chunks_exact(2) {
            out.push(i16::from_le_bytes([c[0], c[1]]) as f32 * inv);
        }
    } else {
        for c in bytes.chunks_exact(3) {
            // Sign-extend 24 bits: sample in the top three bytes, arithmetic-shift down.
            let v = i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8;
            out.push(v as f32 * inv);
        }
    }
    Some(out.len())
}

/// Packet-loss concealment for a plane that has none.
///
/// Opus PLC interpolates a lost `0xC9` datagram. A raw frame has nothing to
/// synthesise a successor from. One lost frame: repeat the previous with a
/// raised-cosine fade. Two or more: fade to silence across the gap. Tuning
/// wants a loss-injection listen, not just the unit tests; see
/// `design/hi-res-audio.md`.
#[derive(Debug, Default, Clone)]
pub struct PcmConceal {
    prev: Vec<f32>,
    run: u32,
}

impl PcmConceal {
    pub fn new() -> PcmConceal {
        PcmConceal::default()
    }

    pub fn accept(&mut self, frame: &[f32]) {
        self.prev.clear();
        self.prev.extend_from_slice(frame);
        self.run = 0;
    }

    pub fn run(&self) -> u32 {
        self.run
    }

    /// Write one concealed frame into `out`. `false` when no frame has arrived
    /// yet — caller should emit silence and let the ring re-prime.
    pub fn conceal(&mut self, out: &mut Vec<f32>) -> bool {
        if self.prev.is_empty() {
            return false;
        }
        self.run = self.run.saturating_add(1);
        out.clear();
        out.extend_from_slice(&self.prev);
        let n = out.len();
        match self.run {
            // First loss: previous frame, faded across its tail so the splice does not step.
            1 => raised_cosine_tail(out, n),
            // Sustained loss: decay toward silence rather than looping a fragment.
            r => {
                let g = 0.5f32.powi(r.min(8) as i32 - 1);
                for s in out.iter_mut() {
                    *s *= g;
                }
                raised_cosine_tail(out, n);
            }
        }
        // The faded frame is the next source, so a run decays instead of restarting from the last loud frame.
        self.prev.clear();
        self.prev.extend_from_slice(out);
        true
    }
}

/// Raised-cosine fade of the last `n` samples in place, so a splice does not step.
///
/// `pub` for host capture-hole infill (`punktfunk-host::native::audio`). `n`
/// is interleaved: a multi-channel fade passes `frames × channels`, so
/// adjacent channels of one frame sit one step apart on the curve.
pub fn raised_cosine_tail(buf: &mut [f32], n: usize) {
    let n = n.min(buf.len());
    if n == 0 {
        return;
    }
    let start = buf.len() - n;
    for (i, s) in buf[start..].iter_mut().enumerate() {
        let t = (i as f32 + 0.5) / n as f32;
        *s *= 0.5 * (1.0 + (std::f32::consts::PI * t).cos());
    }
}

/// Mirror of [`raised_cosine_tail`]: fade the first `n` interleaved samples in from zero.
pub fn raised_cosine_head(buf: &mut [f32], n: usize) {
    let n = n.min(buf.len());
    if n == 0 {
        return;
    }
    for (i, s) in buf[..n].iter_mut().enumerate() {
        let t = (i as f32 + 0.5) / n as f32;
        *s *= 0.5 * (1.0 - (std::f32::consts::PI * t).cos());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goertzel single-bin DFT. Magnitude relative to a full-scale sine:
    /// 1.0 is "the whole signal is this tone".
    fn tone_energy(samples: &[f32], rate_hz: u32, freq_hz: f32) -> f32 {
        let n = samples.len();
        let k = (n as f32 * freq_hz / rate_hz as f32).round();
        let w = 2.0 * std::f32::consts::PI * k / n as f32;
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &x in samples {
            let s0 = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        2.0 * power.max(0.0).sqrt() / n as f32
    }

    /// A tone above 24 kHz cannot exist on the Opus plane. Once a 30 kHz tone
    /// is in the pipeline, `0xD3` must carry it out. Capture resampling
    /// (WASAPI autoconvert, PipeWire) is not this test; a brick wall at 24 kHz
    /// on glass then indicts capture, not transport.
    #[test]
    fn a_tone_above_the_opus_ceiling_survives_the_plane() {
        // Above Opus Nyquist (24 kHz), inside 96 kHz Nyquist (48 kHz).
        const TONE_HZ: f32 = 30_000.0;
        for rate in [96_000u32, 176_400] {
            let n = rate as usize / 10; // 100 ms, plenty of bins at 30 kHz
            let src: Vec<f32> = (0..n)
                .map(|i| {
                    (2.0 * std::f32::consts::PI * TONE_HZ * i as f32 / rate as f32).sin() * 0.5
                })
                .collect();

            let before = tone_energy(&src, rate, TONE_HZ);
            assert!(
                before > 0.45,
                "{rate} Hz: the source tone is not there ({before})"
            );
            // 12 kHz is silent here. A bin that reads high everywhere would "prove" a deleted tone survived.
            let absent = tone_energy(&src, rate, 12_000.0);
            assert!(
                absent < 0.01,
                "{rate} Hz: the tone detector reads {absent} where there is no tone, so it \
                 cannot tell survival from loss"
            );

            let mut wire = Vec::new();
            from_f32(&src, BITS_24, &mut wire);
            let mut out = Vec::new();
            to_f32(&wire, BITS_24, &mut out).expect("whole samples");

            let after = tone_energy(&out, rate, TONE_HZ);
            assert!(
                (after - before).abs() < 0.001,
                "{rate} Hz: 30 kHz tone lost {:.4} of its energy crossing the plane \
                 (before {before:.4}, after {after:.4})",
                before - after
            );

            // 24-bit quantisation is far below audible; the reconstruction must be sample-accurate.
            let worst = src
                .iter()
                .zip(&out)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            assert!(
                worst < 1.0 / full_scale(BITS_24) as f32,
                "{rate} Hz: worst sample error {worst} exceeds one 24-bit code"
            );
        }
    }

    #[test]
    fn every_code_round_trips_bit_exactly() {
        for bits in [BITS_16, BITS_24] {
            let fs = full_scale(bits);
            let mut codes: Vec<i32> = vec![0, 1, -1, fs, -fs, fs - 1, -fs + 1];
            let stride = (fs / 4096).max(1);
            codes.extend((-fs..=fs).step_by(stride as usize));

            let mut wire = Vec::new();
            for &c in &codes {
                if bits == BITS_16 {
                    wire.extend_from_slice(&(c as i16).to_le_bytes());
                } else {
                    wire.extend_from_slice(&c.to_le_bytes()[..3]);
                }
            }

            let mut floats = Vec::new();
            let n = to_f32(&wire, bits, &mut floats).expect("whole samples");
            assert_eq!(n, codes.len());

            let mut back = Vec::new();
            from_f32(&floats, bits, &mut back);
            assert_eq!(
                back, wire,
                "{bits}-bit PCM must survive wire → f32 → wire unchanged"
            );
        }
    }

    /// A missing 24-bit sign-extend turns every negative into a large positive.
    #[test]
    fn twenty_four_bit_negatives_sign_extend() {
        let mut wire = Vec::new();
        from_f32(&[-0.5, 0.5, -1.0, 1.0], BITS_24, &mut wire);
        let mut out = Vec::new();
        to_f32(&wire, BITS_24, &mut out).expect("whole samples");
        assert!(out[0] < -0.4 && out[0] > -0.6, "got {}", out[0]);
        assert!(out[1] > 0.4 && out[1] < 0.6, "got {}", out[1]);
        assert!((out[2] + 1.0).abs() < 1e-6, "got {}", out[2]);
        assert!((out[3] - 1.0).abs() < 1e-6, "got {}", out[3]);
    }

    /// Clamp, not wrap. A wrap is full-scale noise of the opposite sign.
    #[test]
    fn out_of_range_input_clamps_instead_of_wrapping() {
        for bits in [BITS_16, BITS_24] {
            let mut wire = Vec::new();
            from_f32(
                &[9.0, -9.0, f32::INFINITY, f32::NEG_INFINITY],
                bits,
                &mut wire,
            );
            let mut out = Vec::new();
            to_f32(&wire, bits, &mut out).expect("whole samples");
            for v in &out {
                assert!((-1.0..=1.0).contains(v), "{bits}-bit clamp failed: {v}");
            }
            assert!(out[0] > 0.99 && out[1] < -0.99);
            assert!(out[2] > 0.99 && out[3] < -0.99);
        }
    }

    /// Both families, so a rung that divides only one cannot pin the other by accident.
    const RATES: [u32; 5] = [44_100, 48_000, 88_200, 96_000, 176_400];

    /// The ladder must take the longest rung that fits, and never one that does not.
    #[test]
    fn the_frame_ladder_never_exceeds_the_datagram() {
        for rate in RATES {
            for bits in [BITS_16, BITS_24] {
                for budget in [600usize, 900, 1200, 1387, 1400, 1440, 9000] {
                    let chosen = frame_us_for(rate, bits, 2, budget);
                    if let Some(us) = chosen {
                        let bytes = frame_payload_bytes(rate, bits, 2, us);
                        assert!(
                            bytes + PCM_HEADER_LEN <= budget,
                            "{rate}/{bits} at {budget} B chose {us} µs = {bytes} B + header"
                        );
                    }
                    // `None` is a real answer: 176_400/24-bit needs 1_069 B for a 1 ms frame.
                    let longer_than = chosen.unwrap_or(0);
                    for &longer in FRAME_US_LADDER.iter().take_while(|&&x| x > longer_than) {
                        assert!(
                            frame_payload_bytes(rate, bits, 2, longer) + PCM_HEADER_LEN > budget,
                            "{rate}/{bits} at {budget} B should have chosen {longer} µs"
                        );
                    }
                }
            }
        }
        assert_eq!(frame_us_for(176_400, BITS_24, 2, 1_000), None);
        assert_eq!(frame_us_for(176_400, BITS_24, 2, 1_400), Some(1000));
    }

    /// Ladder at the 1472-byte MTU ceiling. Silent change here is a silent change in packet rate.
    #[test]
    fn the_default_mtu_yields_the_documented_ladder() {
        // Conservative usable datagram at the 1472-byte discovery ceiling.
        let d = 1400;
        assert_eq!(frame_us_for(48_000, BITS_16, 2, d), Some(5000));
        assert_eq!(frame_us_for(48_000, BITS_24, 2, d), Some(4000));
        assert_eq!(frame_us_for(96_000, BITS_16, 2, d), Some(3000));
        assert_eq!(frame_us_for(96_000, BITS_24, 2, d), Some(2000));
        // 96/24 at 2.5 ms is 240 × 2 × 3 = 1440 B of payload against ~1387 B.
        assert!(frame_payload_bytes(96_000, BITS_24, 2, 2500) + PCM_HEADER_LEN > d);
    }

    /// Every rung is a whole number of samples at 48/96 kHz, so those sessions can treat the rung as a duration.
    #[test]
    fn every_ladder_rung_is_whole_samples_at_the_48k_family() {
        for &us in &FRAME_US_LADDER {
            for rate in [48_000u64, 96_000] {
                assert_eq!(
                    rate * us as u64 % 1_000_000,
                    0,
                    "{us} µs is not a whole number of samples at {rate} Hz"
                );
            }
        }
    }

    /// A fractional 44.1-family rung carries the floor: short of its label, never over.
    /// Short is safe for payload and ring sizing; it is the wrong direction for a clock.
    #[test]
    fn a_fractional_rung_carries_the_floor_and_is_short_of_its_label() {
        // Whole samples per channel, not interleaved: 5 ms at 44_100 Hz stereo is 441 interleaved, 220.5 per channel.
        let divides = |rate: u64, us: u64| rate * us % 1_000_000 == 0;
        let mut fractional = 0;
        for &us in &FRAME_US_LADDER {
            for rate in RATES {
                for ch in [2u8, 6] {
                    let n = samples_per_frame(rate, us, ch);
                    assert_eq!(
                        n % ch as usize,
                        0,
                        "{n} samples is not whole frames at {ch}ch"
                    );
                    let real_ns = frame_duration_ns(n, rate, ch);
                    let nominal_ns = us as u64 * 1_000;
                    assert!(
                        real_ns <= nominal_ns,
                        "{rate} Hz/{ch}ch at {us} µs carries {real_ns} ns — LONGER than its label, \
                         so the payload could outgrow the datagram it was sized against"
                    );
                    if divides(rate as u64, us as u64) {
                        assert_eq!(real_ns, nominal_ns, "{rate} Hz at {us} µs must be exact");
                    } else {
                        fractional += 1;
                        // Under one sample per channel short — the floor, not a lost frame.
                        assert!(
                            nominal_ns - real_ns < frame_duration_ns(ch as usize, rate, ch) + 1
                        );
                    }
                }
            }
        }
        // 44_100 divides no rung, 88_200 only 5000 µs, 176_400 only 5000 and 2500:
        // 7 + 6 + 5 = 18 pairings × 2 channel counts.
        assert_eq!(
            fractional, 36,
            "the fractional set is not what the doc claims"
        );

        assert_eq!(samples_per_frame(44_100, 5000, 2), 440, "220 per channel");
        assert_eq!(frame_duration_ns(440, 44_100, 2), 4_988_662);
    }

    /// Advancing `pts_ns` by negotiated `frame_us` runs 0.23 % fast at 44_100 Hz
    /// (2.3 ms/s of invented time). The timestamps stay self-consistent.
    #[test]
    fn advancing_a_clock_by_the_nominal_frame_would_drift() {
        let (rate, us, ch) = (44_100u32, 5000u32, 2u8);
        let n = samples_per_frame(rate, us, ch);
        let frames = 1_000_000 / us as u64;
        let nominal_ns = frames * us as u64 * 1_000;
        let real_ns = frames * frame_duration_ns(n, rate, ch);
        let fast_ppm = (nominal_ns - real_ns) * 1_000_000 / real_ns;
        assert!(
            (2_200..2_400).contains(&fast_ppm),
            "the nominal clock runs {fast_ppm} ppm fast — expected ~2 268 (0.23 %)"
        );
        // Running total vs summed floors: 26 ns over 1 s, not the 2.3 ms the nominal clock invents.
        let exact_ns = frame_duration_ns(frames as usize * n, rate, ch);
        assert!(
            exact_ns >= real_ns && exact_ns - real_ns < frames,
            "summing floored frames must stay within 1 ns per frame of the running total \
             ({exact_ns} vs {real_ns} over {frames} frames)"
        );

        // On a rate the rung divides, both clocks agree.
        let n48 = samples_per_frame(48_000, us, ch);
        assert_eq!(frame_duration_ns(n48, 48_000, ch), us as u64 * 1_000);
    }

    /// A rate added here still needs `JitterPolicy` arithmetic that does not divide-then-multiply.
    #[test]
    fn the_supported_rate_set_is_both_families() {
        for rate in RATES {
            assert!(rate_is_supported(rate), "{rate} Hz must be carried");
        }
        // 192 kHz is out of scope. `0` is the wire's "absent" value.
        for rate in [
            0u32, 8_000, 16_000, 22_050, 32_000, 64_000, 192_000, 384_000,
        ] {
            assert!(!rate_is_supported(rate), "{rate} Hz must not be offered");
        }
    }

    #[test]
    fn the_plane_costs_what_the_design_says() {
        assert_eq!(bitrate_kbps(48_000, BITS_16, 2), 1_536);
        assert_eq!(bitrate_kbps(48_000, BITS_24, 2), 2_304);
        assert_eq!(bitrate_kbps(96_000, BITS_16, 2), 3_072);
        assert_eq!(bitrate_kbps(96_000, BITS_24, 2), 4_608);
        // 44.1 family, floored. 176_400/24 stereo is 8.5 Mbps ABR cannot reclaim.
        assert_eq!(bitrate_kbps(44_100, BITS_16, 2), 1_411); // 1 411.2, floored
        assert_eq!(bitrate_kbps(44_100, BITS_24, 2), 2_116); // 2 116.8, floored
        assert_eq!(bitrate_kbps(88_200, BITS_24, 2), 4_233); // 4 233.6, floored
        assert_eq!(bitrate_kbps(176_400, BITS_24, 2), 8_467); // 8 467.2, floored
    }

    /// A truncated datagram must not decode as a shifted frame.
    #[test]
    fn a_partial_sample_is_rejected() {
        let mut out = Vec::new();
        assert!(to_f32(&[0, 0, 0], BITS_16, &mut out).is_none());
        assert!(to_f32(&[0, 0], BITS_24, &mut out).is_none());
        assert_eq!(to_f32(&[], BITS_24, &mut out), Some(0));
    }

    /// No prior frame: say so, so the caller emits silence instead of an uninitialised buffer.
    #[test]
    fn concealment_needs_a_frame_to_build_from() {
        let mut c = PcmConceal::new();
        let mut out = Vec::new();
        assert!(!c.conceal(&mut out), "nothing to repeat yet");
        c.accept(&[0.5; 240]);
        assert!(c.conceal(&mut out));
        assert_eq!(out.len(), 240);
    }

    #[test]
    fn a_sustained_gap_decays_to_silence() {
        let mut c = PcmConceal::new();
        c.accept(&[1.0; 128]);
        let mut out = Vec::new();
        let mut peaks = Vec::new();
        for _ in 0..8 {
            assert!(c.conceal(&mut out));
            peaks.push(out.iter().fold(0f32, |m, s| m.max(s.abs())));
        }
        for w in peaks.windows(2) {
            assert!(w[1] <= w[0] + 1e-6, "concealment grew louder: {peaks:?}");
        }
        assert!(peaks[0] <= 1.0, "never louder than the source: {peaks:?}");
        assert!(
            *peaks.last().unwrap() < 0.05,
            "should have faded out: {peaks:?}"
        );
        assert_eq!(c.run(), 8);
        c.accept(&[0.25; 128]);
        assert_eq!(c.run(), 0);
    }

    /// The fade must reach (near) zero at the splice; a mid-waveform end steps into what follows.
    #[test]
    fn the_fade_lands_on_silence() {
        let mut c = PcmConceal::new();
        c.accept(&[1.0; 64]);
        let mut out = Vec::new();
        c.conceal(&mut out);
        assert!(out[0] > 0.9, "starts at full level: {}", out[0]);
        assert!(
            *out.last().unwrap() < 0.01,
            "ends at silence: {}",
            out.last().unwrap()
        );
    }

    /// Head is tail run backwards: together they are unity, so tail then head is a crossfade, not a dip.
    #[test]
    fn the_head_fade_mirrors_the_tail_fade() {
        let mut head = vec![1.0f32; 64];
        raised_cosine_head(&mut head, 32);
        assert!(head[0] < 0.01, "starts at silence: {}", head[0]);
        assert!(head[31] > 0.99, "lands at full level: {}", head[31]);
        assert_eq!(head[32], 1.0, "untouched past n");
        let mut tail = vec![1.0f32; 32];
        raised_cosine_tail(&mut tail, 32);
        for i in 0..32 {
            let sum = head[i] + tail[i];
            assert!((sum - 1.0).abs() < 1e-5, "unity at {i}: {sum}");
        }
        // Degenerate lengths must not panic or touch anything.
        let mut short = vec![1.0f32; 4];
        raised_cosine_head(&mut short, 0);
        raised_cosine_head(&mut short, 100);
        assert!(short[3] > 0.9);
    }
}
