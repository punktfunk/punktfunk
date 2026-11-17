//! The audio bytes a host sends, through the decoders every client runs on them: the Opus
//! and PCM planes, and a pad's haptics or speaker stream. libopus is C; the fuzzer instruments
//! the Rust that sizes its buffers, and the CI job's `CFLAGS` instrument the C.
//!
//! `data[0]` picks the arm, the next bytes are the `Welcome` fields a host could send, and the
//! rest is a burst of length-prefixed frames, so one input drives a decoder through gaps,
//! concealment and DTX.
#![no_main]

use libfuzzer_sys::fuzz_target;
use punktfunk_core::audio::pad_mix::{PadDecode, QuadMixer, MAX_FRAME_SAMPLES};
use punktfunk_core::audio::plane::{PlaneDecoder, PlaneFormat};
use punktfunk_core::quic::PadAudioFrame;
use std::time::Instant;

/// Rates libopus takes, PCM rates (`pcm::rate_is_supported`), and 0 and 192 kHz, which a host
/// must not send.
const RATES: [u32; 9] = [
    0, 8_000, 12_000, 16_000, 24_000, 44_100, 48_000, 96_000, 192_000,
];

/// The length-prefixed frames of a burst; a prefix past the end takes what is left.
fn frames(mut b: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    while let [l0, l1, rest @ ..] = b {
        let n = usize::from(u16::from_le_bytes([*l0, *l1])).min(rest.len());
        let (frame, next) = rest.split_at(n);
        out.push(frame);
        b = next;
    }
    out
}

fuzz_target!(|data: &[u8]| {
    let [arm, a, b, c, d, e, f, rest @ ..] = data else {
        return;
    };
    if arm & 1 == 0 {
        let fmt = PlaneFormat::from_welcome(
            *a,
            RATES[usize::from(*b) % RATES.len()],
            *c,
            *d,
            u32::from(u16::from_le_bytes([*e, *f])) * 100,
            *arm >> 1,
        );
        let Ok(mut dec) = PlaneDecoder::new(&fmt) else {
            return;
        };
        let mut out = vec![0f32; fmt.max_frame_samples()];
        let mut last = fmt.frame_samples() / usize::from(fmt.channels.max(1));
        for frame in frames(rest) {
            if let Ok(n) = dec.decode(frame, &mut out) {
                last = n;
            }
            let _ = dec.conceal(last, &mut out);
        }
    } else {
        let mut dec = PadDecode::new(true, true);
        let mut mixer = QuadMixer::<f32>::new(8);
        let mut pcm = vec![0f32; MAX_FRAME_SAMPLES * 2];
        let mut mixed = Vec::new();
        let mut seq = u32::from_le_bytes([*b, *c, *d, *e]);
        for frame in frames(rest) {
            let frame = PadAudioFrame {
                pad: *a,
                kind: *f & 1,
                seq,
                pts_ns: 0,
                opus: frame.to_vec(),
            };
            let _ = dec.decode_frame(&frame, &mut pcm, &mut mixer);
            // A gap now and then, for the concealment path.
            seq = seq.wrapping_add(1 + u32::from(frame.opus.len() as u8 & 3));
            mixer.pop(&mut mixed, Instant::now());
        }
    }
});
