//! Audio: channels, format, host audio, microphone and this device's endpoints.

use super::{
    advanced_group, described_labeled, described_overridable, group, labeled, presets, rows,
    setting_combo, setting_toggle, Cx,
};
use crate::trust::Settings;
// The audio-format table lives in the session crate, not here: the same three stored values also
// have to reach the wire, and they are shared verbatim with the Apple and Android clients so one
// preset round-trips. A second copy of the spellings in this file is exactly the drift the
// shared table exists to prevent — which is why this row has no `const` beside AUDIO_CHANNELS.
use pf_client_core::session::AUDIO_FORMATS;
use pf_client_core::settings::Label;
use pf_l10n::client;
use windows_reactor::*;

/// Audio channel presets: `(channel count, label)`. The host clamps to what it can capture;
/// the resolved count drives the decoder + WASAPI render layout.
const AUDIO_CHANNELS: &[(u8, Label)] = &[
    (2, client::player_audio_channels_2),
    (6, client::player_audio_channels_6),
    (8, client::player_audio_channels_8),
];

/// Audio: channels, format, host audio, microphone and this device's endpoints.
pub(super) fn audio_section(cx: &Cx) -> Vec<View> {
    let Cx {
        ctx,
        scope,
        ref s,
        preset_mode,
        ..
    } = *cx;
    let (ac_names, ac_i) = presets(&labeled(AUDIO_CHANNELS), |v| *v == s.audio_channels);
    let channels_combo = setting_combo(cx, scope, "audio_channels", ac_names, ac_i, |s, i| {
        s.audio_channels = AUDIO_CHANNELS[i].0;
    });
    // The lossless-audio opt-in. An unknown stored value (a newer client's row, arriving through a
    // shared preset) shows as Opus — which is what the session resolves it to as well, so the row
    // and the wire agree rather than the combo silently rewriting the user's choice on save.
    let (af_names, af_i) = presets(&labeled(AUDIO_FORMATS), |v| *v == s.audio_format);
    let format_combo = setting_combo(cx, scope, "audio_format", af_names, af_i, |s, i| {
        s.audio_format = AUDIO_FORMATS[i].0.to_string();
    });
    let keep_host_audio_toggle =
        setting_toggle(cx, scope, "keep_host_audio", s.keep_host_audio, |s, on| {
            s.keep_host_audio = on
        });
    let mic_toggle = setting_toggle(cx, scope, "mic_enabled", s.mic_enabled, |s, on| {
        s.mic_enabled = on
    });
    // Endpoint pickers (the WASAPI probe — the GTK client's PipeWire twins): visible
    // labels are friendly names, the stored value is the endpoint id. Hidden when the
    // probe found at most the default; a saved device that's gone keeps a revertable
    // "(not detected)" entry, like the GPU row. Device facts — defaults scope only, probed
    // once per visit (`refresh_snapshot`).
    let (speakers, mics) = {
        let p = ctx.probes.lock().unwrap();
        (p.speakers.clone(), p.mics.clone())
    };
    let dev_combo = |field: &'static str,
                     saved: &str,
                     devs: &[pf_client_core::audio::AudioDevice],
                     apply: fn(&mut Settings, String),
                     system: &str,
                     missing: &dyn Fn(&str) -> String| {
        let mut names = vec![system.to_string()];
        let mut keys = vec![String::new()];
        for d in devs {
            names.push(d.description.clone());
            keys.push(d.name.clone());
        }
        if !saved.is_empty() && !keys.iter().any(|k| k == saved) {
            names.push(missing(saved));
            keys.push(saved.to_string());
        }
        (keys.len() > 1).then(|| {
            let current = keys.iter().position(|k| k == saved).unwrap_or(0);
            setting_combo(cx, scope, field, names, current, move |s, i| {
                apply(s, keys[i.min(keys.len() - 1)].clone());
            })
        })
    };
    let speaker_combo = dev_combo(
        "speaker_device",
        &s.speaker_device,
        &speakers,
        |s, v| s.speaker_device = v,
        client::player_speaker_device_system(),
        &|name| client::player_speaker_device_missing(name),
    );
    let mic_dev_combo = dev_combo(
        "mic_device",
        &s.mic_device,
        &mics,
        |s, v| s.mic_device = v,
        client::player_mic_device_system(),
        &|name| client::player_mic_device_missing(name),
    );
    // Echo cancellation is meaningless without an uplink, so it greys out with the mic above
    // it. Every commit bumps `rev` and re-renders this screen, so the two stay in step live.
    let echo_toggle = setting_toggle(cx, scope, "echo_cancel", s.echo_cancel, |s, on| {
        s.echo_cancel = on
    })
    .is_enabled(s.mic_enabled);

    let mut out = group(
        cx.pal,
        None,
        [
            Some(described_overridable(
                cx,
                &rows::AUDIO_CHANNELS,
                channels_combo,
                client::player_audio_channels_hint(),
            )),
            // The endpoint picks are facts about THIS device's hardware — never
            // per preset, like Decoder/GPU.
            (!preset_mode)
                .then(|| {
                    speaker_combo.map(|c| {
                        described_labeled(
                            cx.pal,
                            rows::SPEAKER.label(),
                            c,
                            client::player_speaker_device_hint(),
                        )
                    })
                })
                .flatten(),
            Some(described_overridable(
                cx,
                &rows::MIC,
                mic_toggle,
                &format!(
                    "{} {}",
                    client::player_mic_enabled_hint(),
                    client::player_mic_enabled_keys_hint("Ctrl+Alt+Shift+V"),
                ),
            )),
            (!preset_mode)
                .then(|| {
                    mic_dev_combo.map(|c| {
                        described_labeled(
                            cx.pal,
                            rows::MIC_DEVICE.label(),
                            c,
                            client::player_mic_device_hint(),
                        )
                    })
                })
                .flatten(),
        ]
        .into_iter()
        .flatten()
        .collect(),
        Some(client::prefs_applies_next_session()),
    );

    let d = Settings::default();
    let stereo = s.audio_channels == 2;
    let mut advanced = Vec::new();
    // Stereo-only, so HIDDEN under 5.1/7.1: a lossless surround frame does not fit one
    // QUIC datagram at the default MTU (design/hi-res-audio.md §4.2).
    if stereo {
        advanced.push(described_overridable(
            cx,
            &rows::AUDIO_FORMAT,
            format_combo,
            client::player_audio_format_hint(),
        ));
    }
    advanced.extend([
        described_overridable(
            cx,
            &rows::KEEP_HOST_AUDIO,
            keep_host_audio_toggle,
            client::player_keep_host_audio_hint(),
        ),
        described_overridable(
            cx,
            &rows::ECHO_CANCEL,
            echo_toggle,
            client::player_echo_cancel_hint(),
        ),
    ]);
    let changed = [
        stereo && s.audio_format != d.audio_format,
        s.keep_host_audio != d.keep_host_audio,
        s.echo_cancel != d.echo_cancel,
    ];
    out.extend(advanced_group(
        cx,
        advanced,
        changed.into_iter().filter(|c| *c).count(),
        ["audio_format", "keep_host_audio", "echo_cancel"]
            .into_iter()
            .any(|f| cx.overrides(f)),
    ));
    out
}
