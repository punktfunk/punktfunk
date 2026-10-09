//! DualSense pad audio (`0xD1`) over Bluetooth, the platform-free half. A Bluetooth pad has no
//! audio device: its speaker and voice coils play from HID output report `0x36`, one per 512
//! source frames. Each report carries an output-state block, 32 stereo coil frames as signed
//! 8-bit at 3 kHz, and one 10 ms Opus frame for the speaker. The client encodes the Opus and
//! writes the report; this module shapes the rest.

use super::pad_mix::PAD_CHANNELS;
use super::SAMPLE_RATE_HZ;
use std::f32::consts::PI;
use std::time::Duration;

/// 48 kHz source frames per report: the pad's media clock.
pub const BT_BLOCK_FRAMES: usize = 512;
/// One report per block, 512 / 48 kHz.
pub const BT_REPORT_PERIOD: Duration = Duration::from_nanos(10_666_667);
/// The pad plays one 480-frame Opus frame per block, so the speaker pair is resampled 16:15.
pub const BT_SPEAKER_FRAMES: usize = 480;
/// 160 kbit/s CBR at 10 ms.
pub const BT_OPUS_BITRATE: i32 = 160_000;
pub const BT_OPUS_BYTES: usize = 200;
pub const MEDIA_REPORT_LEN: usize = 398;

const MEDIA_REPORT_ID: u8 = 0x36;
const STATE_LEN: usize = 63;
/// 32 stereo frames at 3 kHz.
const COIL_BYTES: usize = 64;
const DECIMATE: usize = 16;
const TAPS: usize = 96;
/// The −6 dB point, under the 1.5 kHz output Nyquist; the stopband starts near 2 kHz.
const CUTOFF_HZ: f32 = 1_200.0;

/// The report's output-state block. Every report selects the coil source: audio haptics, or
/// the motor levels in `rumble` (`left`, `right`), so a report without the levels SDL is playing
/// would stop that rumble. No other field is marked valid; lights, triggers and volumes keep
/// what the pad was last told.
pub fn media_state(rumble: Option<(u8, u8)>) -> [u8; STATE_LEN] {
    let mut s = [0u8; STATE_LEN];
    if let Some((left, right)) = rumble {
        s[0] = 0x02; // the motor levels drive the coils
        s[2] = right;
        s[3] = left;
        s[38] = 0x04; // improved rumble emulation, as SDL selects over Bluetooth
    }
    s
}

/// Low-pass and ÷16 for the coil pair: four-channel 48 kHz float in, signed 8-bit at 3 kHz out.
pub struct HapticsDecimator {
    taps: [f32; TAPS],
    /// The last `TAPS - 1` coil frames, then the block being filtered.
    hist: Vec<[f32; 2]>,
}

impl Default for HapticsDecimator {
    fn default() -> Self {
        // Hann-windowed sinc, normalised to unity DC gain.
        let fc = CUTOFF_HZ / SAMPLE_RATE_HZ as f32;
        let mid = (TAPS - 1) as f32 / 2.0;
        let mut taps = [0f32; TAPS];
        for (i, t) in taps.iter_mut().enumerate() {
            let x = i as f32 - mid;
            let sinc = (2.0 * PI * fc * x).sin() / (PI * x);
            *t = sinc * (0.5 - 0.5 * (2.0 * PI * i as f32 / (TAPS - 1) as f32).cos());
        }
        let sum: f32 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        HapticsDecimator {
            taps,
            hist: vec![[0.0; 2]; TAPS - 1],
        }
    }
}

impl HapticsDecimator {
    /// The coils of one block (`BT_BLOCK_FRAMES` four-channel frames), as interleaved L/R bytes.
    pub fn block(&mut self, quad: &[f32]) -> [u8; COIL_BYTES] {
        debug_assert_eq!(quad.len(), BT_BLOCK_FRAMES * PAD_CHANNELS);
        self.hist
            .extend(quad.chunks_exact(PAD_CHANNELS).map(|f| [f[2], f[3]]));
        let mut out = [0u8; COIL_BYTES];
        for (k, pair) in out.chunks_exact_mut(2).enumerate() {
            // Output k lands on the last frame of its group; the taps are symmetric.
            let start = k * DECIMATE + DECIMATE - 1;
            let (mut l, mut r) = (0f32, 0f32);
            for (h, x) in self.taps.iter().zip(&self.hist[start..start + TAPS]) {
                l += h * x[0];
                r += h * x[1];
            }
            pair[0] = to_s8(l);
            pair[1] = to_s8(r);
        }
        self.hist.drain(..self.hist.len() - (TAPS - 1));
        out
    }
}

fn to_s8(x: f32) -> u8 {
    (x * 127.0).round().clamp(-128.0, 127.0) as i8 as u8
}

/// The speaker pair of one block, resampled 16:15 to the frames the pad plays per report.
pub fn speaker_block(quad: &[f32], out: &mut [f32; BT_SPEAKER_FRAMES * 2]) {
    debug_assert_eq!(quad.len(), BT_BLOCK_FRAMES * PAD_CHANNELS);
    for (j, o) in out.chunks_exact_mut(2).enumerate() {
        let pos = j * BT_BLOCK_FRAMES;
        let i = pos / BT_SPEAKER_FRAMES;
        let frac = (pos % BT_SPEAKER_FRAMES) as f32 / BT_SPEAKER_FRAMES as f32;
        let (a, b) = (&quad[i * PAD_CHANNELS..], &quad[(i + 1) * PAD_CHANNELS..]);
        o[0] = a[0] + (b[0] - a[0]) * frac;
        o[1] = a[1] + (b[1] - a[1]) * frac;
    }
}

/// Report `0x36`, with its sequence nibble and report counter.
#[derive(Default)]
pub struct MediaReport {
    seq: u8,
    counter: u8,
}

impl MediaReport {
    pub fn pack(
        &mut self,
        state: &[u8; STATE_LEN],
        coils: &[u8; COIL_BYTES],
        opus: &[u8; BT_OPUS_BYTES],
    ) -> [u8; MEDIA_REPORT_LEN] {
        let mut r = [0u8; MEDIA_REPORT_LEN];
        r[0] = MEDIA_REPORT_ID;
        r[1] = self.seq << 4;
        self.seq = (self.seq + 1) & 0x0F;
        let header = [0xFE, 0x60, 0x60, 0x60, 0x60, 0x60, self.counter];
        self.counter = self.counter.wrapping_add(1);
        // Sub-packets: id with the sized bit, length, body. The pad reads them at these offsets.
        let mut at = 2;
        for (id, body) in [
            (0x11, &header[..]),
            (0x10, &state[..]),
            (0x12, &coils[..]),
            (0x13, &opus[..]),
        ] {
            r[at] = 0x80 | id;
            r[at + 1] = body.len() as u8;
            r[at + 2..at + 2 + body.len()].copy_from_slice(body);
            at += 2 + body.len();
        }
        let crc = bt_crc(&r[..MEDIA_REPORT_LEN - 4]);
        r[MEDIA_REPORT_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
        r
    }
}

/// A Bluetooth output report ends in a little-endian CRC-32 over the HID transaction header
/// `0xA2` and the report.
pub fn bt_crc(report: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(&[0xA2]);
    h.update(report);
    h.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad(f: impl Fn(usize) -> [f32; 4]) -> Vec<f32> {
        (0..BT_BLOCK_FRAMES).flat_map(f).collect()
    }

    #[test]
    fn crc_seeds_with_the_transaction_header() {
        assert_eq!(bt_crc(&[]), 0xEADA_2D49);
    }

    #[test]
    fn media_report_layout() {
        let mut m = MediaReport::default();
        let state = media_state(None);
        let r = m.pack(&state, &[7; COIL_BYTES], &[9; BT_OPUS_BYTES]);
        assert_eq!(r[..4], [0x36, 0x00, 0x91, 7]);
        assert_eq!(r[10], 0, "first report counter");
        assert_eq!(r[11..13], [0x90, 63]);
        assert_eq!(r[76..78], [0x92, 64]);
        assert!(r[78..142].iter().all(|&b| b == 7));
        assert_eq!(r[142..144], [0x93, 200]);
        assert!(r[144..344].iter().all(|&b| b == 9));
        assert!(r[344..394].iter().all(|&b| b == 0));
        assert_eq!(r[394..], bt_crc(&r[..394]).to_le_bytes());
        for n in 1..=16u8 {
            let r = m.pack(&state, &[0; COIL_BYTES], &[0; BT_OPUS_BYTES]);
            assert_eq!(r[1], (n & 0x0F) << 4, "sequence nibble wraps");
            assert_eq!(r[10], n, "report counter");
        }
    }

    #[test]
    fn state_names_rumble_or_selects_audio_haptics() {
        assert_eq!(media_state(None), [0; STATE_LEN]);
        let s = media_state(Some((0x40, 0x80)));
        assert_eq!((s[0], s[2], s[3], s[38]), (0x02, 0x80, 0x40, 0x04));
        assert_eq!(s.iter().filter(|&&b| b != 0).count(), 4);
    }

    #[test]
    fn coils_pass_low_frequencies_and_reject_aliases() {
        let peak = |hz: f32| {
            let mut d = HapticsDecimator::default();
            let mut max = 0i8;
            for b in 0..8 {
                let block = quad(|n| {
                    let t = (b * BT_BLOCK_FRAMES + n) as f32 / SAMPLE_RATE_HZ as f32;
                    let s = (2.0 * PI * hz * t).sin();
                    [0.0, 0.0, s, -s]
                });
                let out = d.block(&block);
                if b > 0 {
                    max = out
                        .iter()
                        .map(|&x| (x as i8).saturating_abs())
                        .fold(max, i8::max);
                }
            }
            max
        };
        assert!(peak(150.0) >= 125, "150 Hz plays at full scale");
        assert!(peak(2_800.0) <= 2, "2.8 kHz would alias to 200 Hz");
    }

    #[test]
    fn coils_interleave_left_right_and_clip() {
        let mut d = HapticsDecimator::default();
        let block = quad(|_| [0.0, 0.0, 2.0, -0.25]);
        d.block(&block);
        let out = d.block(&block);
        for pair in out.chunks_exact(2) {
            assert_eq!(pair[0] as i8, 127);
            assert_eq!(pair[1] as i8, -32);
        }
    }

    #[test]
    fn speaker_resamples_sixteen_to_fifteen() {
        let block = quad(|n| [n as f32, 0.25, 0.0, 0.0]);
        let mut out = [0f32; BT_SPEAKER_FRAMES * 2];
        speaker_block(&block, &mut out);
        for (j, f) in out.chunks_exact(2).enumerate() {
            assert!((f[0] - j as f32 * 16.0 / 15.0).abs() < 1e-3);
            assert_eq!(f[1], 0.25);
        }
    }
}
