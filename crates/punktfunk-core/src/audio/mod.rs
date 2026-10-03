//! Shared audio layout: Opus (multi)stream surround for the host, GameStream, and every
//! client decoder.
//!
//! Wire order is `FL FR FC LFE RL RR SL SR` (GameStream/Moonlight and the PipeWire/PulseAudio
//! 6/8 map). Capturers and decoders both use it; the Opus multistream `mapping` only decides
//! which slots share a coupled stream ([`AudioLayout`]), never the order samples come out in.
//! GFE pre-rotation (`gamestream::audio::surround_params`) is GameStream-only; it never
//! touches `punktfunk/1`.
//!
//! Negotiated counts: `2`, `6`, `8`. Anything else clamps to stereo ([`normalize_channels`]).
//! Opus is 48 kHz; the lossless plane is a second plane — [`pcm`], `design/hi-res-audio.md`.

/// cbindgen:ignore
#[cfg(feature = "quic")]
pub mod mic;
/// cbindgen:ignore
pub mod pad_bt;
/// cbindgen:ignore
pub mod pad_mix;
pub mod pcm;
/// cbindgen:ignore
pub mod plane;
#[cfg(test)]
mod vectors;

mod jitter;
mod layout;
mod recovery;
mod sync;

pub use jitter::*;
pub use layout::*;
pub use recovery::*;
pub use sync::*;

use std::time::Duration;

/// This client silenced its own speakers (`client::NativeClient::set_audio_muted`). The host
/// keeps sending, so a session joined to the same sink still hears the game.
pub const AUDIO_MUTE_LOCAL: u8 = 1 << 0;
/// The operator muted this session from the console (`quic::AudioState`). The host
/// stopped encoding this session's audio, so a local unmute brings nothing back.
pub const AUDIO_MUTE_HOST: u8 = 1 << 1;

/// The sentence the overlay shows for a mute mask; `None` when the stream is audible. One
/// place, so no client invents its own wording for whose mute it is.
pub fn audio_mute_label(mask: u8) -> Option<&'static str> {
    match (mask & AUDIO_MUTE_HOST != 0, mask & AUDIO_MUTE_LOCAL != 0) {
        (true, true) => Some("Muted by the host and on this device"),
        (true, false) => Some("Muted by the host"),
        (false, true) => Some("Muted on this device"),
        (false, false) => None,
    }
}

/// How long a mute the player made themselves names itself on screen.
pub const LOCAL_MUTE_NOTICE: Duration = Duration::from_secs(5);

/// [`audio_mute_label`] with the badge's lifetime applied, `since` the mask last changed.
///
/// A host mute stands for the whole session: an operator silencing a client must not be
/// able to hide behind a local unmute. A local mute is the player's own press from the dial
/// that still shows its state, so it says so long enough to read and then leaves the picture
/// alone — a standing badge over the game is the operator's language, not the player's.
pub fn audio_mute_notice(mask: u8, since: Duration) -> Option<&'static str> {
    if mask & AUDIO_MUTE_HOST == 0 && since >= LOCAL_MUTE_NOTICE {
        return None;
    }
    audio_mute_label(mask)
}

/// Opus-plane frame length in milliseconds, and the default. One datagram carries exactly one
/// ([`crate::quic::encode_audio_datagram`]), so it is also the smallest shed unit.
///
/// The lossless plane negotiates shorter frames ([`pcm::frame_us_for`]); the resolved value is
/// `audio_frame_us` on `Welcome`. Exported to C as `PUNKTFUNK_AUDIO_FRAME_MS` and kept at 5 —
/// embedders size rings from it. Sizing as *frames × this* is wrong by up to 2.5× on lossless;
/// drain `punktfunk_connection_next_audio_pcm` and use `frame_count` instead.
pub const FRAME_MS: u32 = 5;

/// Opus-plane sample rate, and the protocol default. Lossless negotiates via [`pcm`].
pub const SAMPLE_RATE_HZ: u32 = 48_000;

// ---- ms ⇄ interleaved-sample conversion ---------------------------------------------------
// Multiply first, divide last. `per_ms = rate_hz / 1000 * channels` truncates 44 100 Hz to 44
// samples/ms and puts every depth 2.3 % low. 48/96 kHz only look exact because they divide.
// See `design/hi-res-audio.md`.

/// Interleaved samples per second at a negotiated layout — the denominator both conversions
/// share. `u64` because it is a factor in products that reach 10¹² below.
const fn interleaved_per_sec(rate_hz: u32, channels: u8) -> u64 {
    // `max(1)` on both: a degenerate layout must not divide by zero in a realtime callback.
    // The constructors already clamp.
    let hz = if rate_hz == 0 { 1 } else { rate_hz };
    let ch = if channels == 0 { 1 } else { channels };
    hz as u64 * ch as u64
}

/// `ms` milliseconds of audio, in interleaved samples.
///
/// u64 intermediates: [`SYNC_BACKOFF_MAX_MS`] at 176 400 Hz × 8 ch is 6.8 × 10¹¹ before the
/// divide. Saturating: a wrapped window would fire immediately instead of never.
fn ms_to_samples(rate_hz: u32, channels: u8, ms: u32) -> usize {
    let n = ms as u64 * interleaved_per_sec(rate_hz, channels) / 1000;
    if n > u32::MAX as u64 {
        u32::MAX as usize
    } else {
        n as usize
    }
}

/// Interleaved samples back to whole milliseconds — the inverse of [`ms_to_samples`], so
/// `depth_ms(target)` round-trips to `target_ms()` at every rate.
///
/// u128 because `samples` arrives unbounded through [`JitterPolicy::depth_ms`]:
/// `usize::MAX * 1000` overflows a u64 on a 64-bit target.
fn samples_to_ms(rate_hz: u32, channels: u8, samples: usize) -> u32 {
    let ms = samples as u128 * 1000 / interleaved_per_sec(rate_hz, channels) as u128;
    if ms > u32::MAX as u128 {
        u32::MAX
    } else {
        ms as u32
    }
}
