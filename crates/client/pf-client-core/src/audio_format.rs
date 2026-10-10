//! Stored `audio_format` values, the settings-menu table, and the wire pair each value asks for.
//!
//! No session or decoder deps: the Android console shell renders the same row without the
//! session pump. `session` re-exports these names.

pub const AUDIO_FORMAT_OPUS: &str = "opus";

/// PCM 48 kHz / 24-bit (~2.3 Mbps). No Opus stage, and no extra resample on a 48 kHz host engine.
pub const AUDIO_FORMAT_LOSSLESS_48: &str = "lossless48";

/// PCM 96 kHz / 24-bit (~4.6 Mbps). The host declines if capture is not 96 kHz; it does
/// not upsample.
pub const AUDIO_FORMAT_LOSSLESS_96: &str = "lossless96";

/// `(stored value, label)` both desktop settings UIs render.
///
/// Values are shared verbatim with Apple `AudioFormatChoice` and Android `AUDIO_FORMAT_*`.
/// A preset round-trips through all four clients (`presets.rs`); a spelling mismatch is
/// stored as-is and silently falls back to the global default. Change in lockstep with
/// `clients/apple/Sources/PunktfunkShared/EffectiveSettings.swift` and
/// `clients/android/app/src/main/kotlin/io/unom/punktfunk/Settings.kt`.
///
/// 48/96 kHz only: jitter buffers are `ms × samples-per-ms` with an integer per-ms, so
/// 44 100 Hz truncates to 44 (a silent 2.3 % error). See `design/hi-res-audio.md`.
/// 48 kHz / 16-bit is omitted: ~1.5 Mbps for what 256 kbps Opus already is.
pub const AUDIO_FORMATS: &[(&str, &str)] = &[
    (AUDIO_FORMAT_OPUS, "Standard (Opus)"),
    (AUDIO_FORMAT_LOSSLESS_48, "Lossless 48 kHz / 24-bit"),
    (AUDIO_FORMAT_LOSSLESS_96, "Lossless 96 kHz / 24-bit"),
];

/// `(rate_hz, bits)` for a stored [`AUDIO_FORMATS`] value. `None` is Opus: send wire `0`/`0`,
/// not 48 000/16 — any non-zero pair is the lossless plane. Unknown strings return `None`
/// (do not fail the connect).
pub fn audio_format_wire(setting: &str) -> Option<(u32, u8)> {
    use punktfunk_core::audio::pcm::BITS_24;
    match setting {
        AUDIO_FORMAT_LOSSLESS_48 => Some((48_000, BITS_24)),
        AUDIO_FORMAT_LOSSLESS_96 => Some((96_000, BITS_24)),
        _ => None,
    }
}
