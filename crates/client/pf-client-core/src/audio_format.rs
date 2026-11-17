//! Stored `audio_format` values, the settings-menu table, and the wire pair each value asks for.
//!
//! No session or decoder deps: the Android console shell renders the same row without the
//! session pump. `session` re-exports these names.

pub const AUDIO_FORMAT_OPUS: &str = "opus";

/// PCM 44.1 kHz / 24-bit (~2.1 Mbps). No resample on a 44.1 kHz host engine.
pub const AUDIO_FORMAT_LOSSLESS_441: &str = "lossless441";

/// PCM 48 kHz / 24-bit (~2.3 Mbps). No Opus stage, and no extra resample on a 48 kHz host engine.
pub const AUDIO_FORMAT_LOSSLESS_48: &str = "lossless48";

/// PCM 88.2 kHz / 24-bit (~4.2 Mbps).
pub const AUDIO_FORMAT_LOSSLESS_882: &str = "lossless882";

/// PCM 96 kHz / 24-bit (~4.6 Mbps). The host falls back to Opus unless its capture runs at
/// 96 kHz or above; it does not upsample.
pub const AUDIO_FORMAT_LOSSLESS_96: &str = "lossless96";

/// PCM 176.4 kHz / 24-bit (~8.5 Mbps).
pub const AUDIO_FORMAT_LOSSLESS_1764: &str = "lossless1764";

/// `(stored value, label)` the settings UIs render, in the order of
/// `clients/shared/settings-catalog.json`. The label is read when the row is drawn.
///
/// Values are shared verbatim with Apple `AudioFormatChoice` and Android `AUDIO_FORMAT_*`.
/// A preset round-trips between the clients (`presets.rs`); a spelling mismatch is
/// stored as-is and silently falls back to the global default. Change in lockstep with
/// `clients/apple/Sources/PunktfunkShared/EffectiveSettings.swift` and
/// `clients/android/app/src/main/kotlin/io/unom/punktfunk/Settings.kt`.
///
/// 16-bit is omitted at every rate: ~1.5 Mbps for what 256 kbps Opus already is.
pub const AUDIO_FORMATS: &[(&str, crate::settings::Label)] = &[
    (AUDIO_FORMAT_OPUS, pf_l10n::client::player_audio_format_opus),
    (
        AUDIO_FORMAT_LOSSLESS_441,
        pf_l10n::client::player_audio_format_lossless441,
    ),
    (
        AUDIO_FORMAT_LOSSLESS_48,
        pf_l10n::client::player_audio_format_lossless48,
    ),
    (
        AUDIO_FORMAT_LOSSLESS_882,
        pf_l10n::client::player_audio_format_lossless882,
    ),
    (
        AUDIO_FORMAT_LOSSLESS_96,
        pf_l10n::client::player_audio_format_lossless96,
    ),
    (
        AUDIO_FORMAT_LOSSLESS_1764,
        pf_l10n::client::player_audio_format_lossless1764,
    ),
];

/// `(rate_hz, bits)` for a stored [`AUDIO_FORMATS`] value. `None` is Opus: send wire `0`/`0`,
/// not 48 000/16 — any non-zero pair is the lossless plane. Unknown strings return `None`
/// (do not fail the connect).
pub fn audio_format_wire(setting: &str) -> Option<(u32, u8)> {
    use punktfunk_core::audio::pcm::BITS_24;
    let rate_hz = match setting {
        AUDIO_FORMAT_LOSSLESS_441 => 44_100,
        AUDIO_FORMAT_LOSSLESS_48 => 48_000,
        AUDIO_FORMAT_LOSSLESS_882 => 88_200,
        AUDIO_FORMAT_LOSSLESS_96 => 96_000,
        AUDIO_FORMAT_LOSSLESS_1764 => 176_400,
        _ => return None,
    };
    Some((rate_hz, BITS_24))
}

/// True when `setting` at `channels` can only come back as Opus: 7.1 above 96 kHz fits the
/// datagram send buffer in no frame, whole or in parts, and the host declines. The settings
/// row says so.
pub fn surround_over_limit(setting: &str, channels: u8) -> bool {
    channels == 8 && audio_format_wire(setting).is_some_and(|(rate_hz, _)| rate_hz > 96_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::audio::pcm;

    /// A settled 1500-MTU QUIC datagram, the number the host's ladder test uses.
    const DGRAM: usize = 1400;

    /// The limit the row states is where the host's frame ladder runs out, not a second rule.
    #[test]
    fn the_surround_limit_is_where_the_frame_ladder_runs_out() {
        for (setting, _) in AUDIO_FORMATS {
            assert!(!surround_over_limit(setting, 2), "{setting} stereo");
            let Some((rate_hz, bits)) = audio_format_wire(setting) else {
                assert!(!surround_over_limit(setting, 8), "{setting} is Opus");
                continue;
            };
            assert!(pcm::rate_is_supported(rate_hz), "{setting}");
            for channels in [6, 8] {
                let fits = pcm::frame_us_for(rate_hz, bits, channels, DGRAM)
                    .or_else(|| pcm::parted_frame_us_for(rate_hz, bits, channels, DGRAM));
                assert_eq!(
                    surround_over_limit(setting, channels),
                    fits.is_none(),
                    "{setting} at {channels} channels"
                );
            }
        }
    }
}
