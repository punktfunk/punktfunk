//! Sample work between the process tap and [`AudioCapturer`](crate::AudioCapturer): the
//! delivered rate, stereo gathering, resampling, and channel placement. Pure, so it unit-tests.

use crate::SAMPLE_RATE;

/// The rate to deliver: the request, never above the tap, never below what libopus takes.
/// A 44.1 kHz tap still delivers 48 kHz; a 96 kHz request on a 48 kHz tap gets 48 kHz,
/// because upsampling past the tap would advertise content that is not there.
pub(super) fn settle_rate(tap_hz: u32, requested: u32) -> u32 {
    requested.min(tap_hz.max(SAMPLE_RATE))
}

/// Interleaved stereo from the tap's buffers. Each entry is one `AudioBuffer`: its samples and
/// its channel count, so one interleaved buffer and one buffer per channel both work. A mono
/// source feeds both sides.
pub(super) fn gather_stereo(bufs: &[(&[f32], usize)], out: &mut Vec<f32>) {
    let frames = bufs
        .iter()
        .map(|(s, ch)| s.len() / (*ch).max(1))
        .min()
        .unwrap_or(0);
    // The first two channels across the buffers, as (buffer, channel) pairs.
    let mut picks = bufs
        .iter()
        .enumerate()
        .flat_map(|(b, (_, ch))| (0..*ch).map(move |c| (b, c)));
    let Some(left) = picks.next() else { return };
    let right = picks.next().unwrap_or(left);
    let at = |(b, c): (usize, usize), f: usize| bufs[b].0[f * bufs[b].1 + c];
    out.reserve(frames * 2);
    for f in 0..frames {
        out.push(at(left, f));
        out.push(at(right, f));
    }
}

/// Stereo laid into `channels` in GameStream order: FL FR, every other channel silent.
pub(super) fn place(stereo: &[f32], channels: usize, out: &mut Vec<f32>) {
    out.reserve(stereo.len() / 2 * channels);
    for lr in stereo.chunks_exact(2) {
        out.extend_from_slice(lr);
        out.extend(std::iter::repeat_n(0.0, channels.saturating_sub(2)));
    }
}

/// Taps either side of the output instant. 64 taps keep the Blackman transition band near
/// 0.09 of the input rate, so a 44.1 kHz source stays flat to about 18 kHz.
const HALF: usize = 32;
/// Cutoff as a fraction of the lower Nyquist: the stopband starts at that Nyquist.
const CUTOFF: f64 = 0.91;
/// Polyphase rows for one rate pair. Standard rates need at most 640; a pair needing more
/// is refused rather than built.
const MAX_PHASES: u64 = 4096;

/// Streaming windowed-sinc resampler between two fixed rates, interleaved. Exact rational
/// stepping: one row per output phase, so the output clock never drifts from the input.
pub(super) struct Resampler {
    channels: usize,
    passthrough: bool,
    /// Input frames per output frame, in `1/phases` units.
    step: u64,
    phases: u64,
    /// `phases` rows of `2 * HALF` taps.
    table: Vec<f32>,
    /// Input frames not yet consumed, interleaved; starts with `HALF - 1` frames of silence.
    hist: Vec<f32>,
    /// The next output instant, in input frames from `hist[0]`, times `phases`.
    pos: u64,
}

impl Resampler {
    pub(super) fn new(from_hz: u32, to_hz: u32, channels: usize) -> anyhow::Result<Resampler> {
        anyhow::ensure!(from_hz > 0 && to_hz > 0, "resample rates must be positive");
        let g = gcd(u64::from(from_hz), u64::from(to_hz));
        let (step, phases) = (u64::from(from_hz) / g, u64::from(to_hz) / g);
        anyhow::ensure!(
            phases <= MAX_PHASES,
            "resample {from_hz} Hz to {to_hz} Hz: the rate pair needs {phases} filter phases"
        );
        let passthrough = from_hz == to_hz;
        let fc = CUTOFF * (f64::from(to_hz) / f64::from(from_hz)).min(1.0);
        let table = if passthrough {
            Vec::new()
        } else {
            (0..phases)
                .flat_map(|p| kernel_row(p as f64 / phases as f64, fc))
                .collect()
        };
        Ok(Resampler {
            channels,
            passthrough,
            step,
            phases,
            table,
            hist: vec![0.0; (HALF - 1) * channels],
            pos: (HALF as u64 - 1) * phases,
        })
    }

    /// Resamples `input` (whole interleaved frames) onto the end of `out`. Output trails input
    /// by up to `HALF` input frames: the filter needs that much lookahead.
    pub(super) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        let ch = self.channels;
        self.hist.extend_from_slice(input);
        let frames = self.hist.len() / ch;
        let taps = 2 * HALF;
        loop {
            let i = (self.pos / self.phases) as usize;
            if i + HALF >= frames {
                break;
            }
            let p = (self.pos % self.phases) as usize;
            let row = &self.table[p * taps..(p + 1) * taps];
            let first = (i + 1 - HALF) * ch;
            for c in 0..ch {
                let acc: f32 = row
                    .iter()
                    .enumerate()
                    .map(|(k, h)| h * self.hist[first + k * ch + c])
                    .sum();
                out.push(acc);
            }
            self.pos += self.step;
        }
        // Frames before the next output's first tap are spent.
        let spent = ((self.pos / self.phases) as usize + 1)
            .saturating_sub(HALF)
            .min(frames);
        self.hist.drain(..spent * ch);
        self.pos -= spent as u64 * self.phases;
    }
}

/// One phase's taps, input offsets `1 - HALF ..= HALF` around an output `frac` past the
/// centre tap. Normalised to unity DC gain.
fn kernel_row(frac: f64, fc: f64) -> Vec<f32> {
    use std::f64::consts::PI;
    let raw: Vec<f64> = (0..2 * HALF)
        .map(|k| {
            let x = k as f64 - (HALF as f64 - 1.0) - frac;
            let sinc = if x == 0.0 {
                1.0
            } else {
                (PI * fc * x).sin() / (PI * fc * x)
            };
            let u = x / HALF as f64;
            let blackman = 0.42 + 0.5 * (PI * u).cos() + 0.08 * (2.0 * PI * u).cos();
            sinc * blackman
        })
        .collect();
    let sum: f64 = raw.iter().sum();
    raw.iter().map(|h| (h / sum) as f32).collect()
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f64, rate: u32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|n| {
                let s = (std::f64::consts::TAU * hz * n as f64 / f64::from(rate)).sin() as f32;
                [s, -s]
            })
            .collect()
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0, |m, s| m.max(s.abs()))
    }

    #[test]
    fn the_rate_never_rises_past_the_tap_or_falls_below_opus() {
        assert_eq!(settle_rate(48_000, 96_000), 48_000);
        assert_eq!(settle_rate(44_100, 48_000), 48_000);
        assert_eq!(settle_rate(44_100, 96_000), 48_000);
        assert_eq!(settle_rate(96_000, 96_000), 96_000);
        assert_eq!(settle_rate(192_000, 96_000), 96_000);
        assert_eq!(settle_rate(96_000, 48_000), 48_000);
    }

    #[test]
    fn stereo_gathers_from_interleaved_planar_and_mono_buffers() {
        let mut out = Vec::new();
        gather_stereo(&[(&[1.0, 2.0, 3.0, 4.0], 2)], &mut out);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        out.clear();
        gather_stereo(&[(&[1.0, 3.0], 1), (&[2.0, 4.0], 1)], &mut out);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        out.clear();
        gather_stereo(&[(&[5.0, 6.0], 1)], &mut out);
        assert_eq!(out, [5.0, 5.0, 6.0, 6.0]);
        out.clear();
        gather_stereo(&[], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn surround_carries_stereo_in_front_left_and_right() {
        let mut out = Vec::new();
        place(&[0.5, -0.5], 6, &mut out);
        assert_eq!(out, [0.5, -0.5, 0.0, 0.0, 0.0, 0.0]);
        out.clear();
        place(&[0.5, -0.5, 0.25, -0.25], 2, &mut out);
        assert_eq!(out, [0.5, -0.5, 0.25, -0.25]);
    }

    #[test]
    fn equal_rates_pass_through_untouched() {
        let mut r = Resampler::new(48_000, 48_000, 2).unwrap();
        let input = tone(1_000.0, 48_000, 100);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert_eq!(out, input);
    }

    /// 44.1 kHz to 48 kHz: the right number of frames, and a 1 kHz tone comes out as the same
    /// tone at the new rate, delayed by nothing.
    #[test]
    fn upsampling_keeps_the_tone_and_the_clock() {
        let mut r = Resampler::new(44_100, 48_000, 2).unwrap();
        let mut out = Vec::new();
        r.process(&tone(1_000.0, 44_100, 44_100), &mut out);
        let frames = out.len() / 2;
        assert!((47_960..=48_000).contains(&frames), "{frames} frames");
        let want = tone(1_000.0, 48_000, frames);
        let err = out[2_000..]
            .iter()
            .zip(&want[2_000..])
            .fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(err < 1e-3, "max error {err}");
    }

    #[test]
    fn chunking_does_not_change_the_output() {
        let input = tone(3_000.0, 44_100, 4_410);
        let mut whole = Vec::new();
        Resampler::new(44_100, 48_000, 2)
            .unwrap()
            .process(&input, &mut whole);
        let mut r = Resampler::new(44_100, 48_000, 2).unwrap();
        let mut parts = Vec::new();
        let mut rest = &input[..];
        for n in [1usize, 7, 64, 441, 512, 3].iter().cycle() {
            if rest.is_empty() {
                break;
            }
            let take = (n * 2).min(rest.len());
            r.process(&rest[..take], &mut parts);
            rest = &rest[take..];
        }
        assert_eq!(whole, parts);
    }

    /// 96 kHz to 48 kHz: a tone above the new Nyquist is filtered out, not folded down.
    #[test]
    fn downsampling_rejects_what_would_alias() {
        let mut r = Resampler::new(96_000, 48_000, 2).unwrap();
        let mut kept = Vec::new();
        r.process(&tone(10_000.0, 96_000, 9_600), &mut kept);
        assert!(peak(&kept[400..]) > 0.95);
        let mut r = Resampler::new(96_000, 48_000, 2).unwrap();
        let mut gone = Vec::new();
        r.process(&tone(30_000.0, 96_000, 9_600), &mut gone);
        assert!(
            peak(&gone[400..]) < 1e-3,
            "alias peak {}",
            peak(&gone[400..])
        );
    }

    #[test]
    fn a_rate_pair_with_too_many_phases_is_refused() {
        assert!(Resampler::new(48_001, 48_000, 2).is_err());
        assert!(Resampler::new(22_050, 96_000, 2).is_ok());
    }
}
