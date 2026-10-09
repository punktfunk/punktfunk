//! Audio: channels, format, host audio, microphone and this device's endpoints.

use super::{
    advanced_group, described_labeled, described_overridable, group, presets, setting_combo,
    setting_toggle, Cx,
};
use crate::trust::Settings;
// The audio-format table lives in the session crate, not here: the same three stored values also
// have to reach the wire, and they are shared verbatim with the Apple and Android clients so one
// preset round-trips. A second copy of the spellings in this file is exactly the drift the
// shared table exists to prevent — which is why this row has no `const` beside AUDIO_CHANNELS.
use pf_client_core::session::AUDIO_FORMATS;
use windows_reactor::*;

/// Audio channel presets: `(channel count, display label)`. The host clamps to what it can
/// capture; the resolved count drives the decoder + WASAPI render layout.
const AUDIO_CHANNELS: &[(u8, &str)] = &[(2, "Stereo"), (6, "5.1 Surround"), (8, "7.1 Surround")];

/// Audio: channels, format, host audio, microphone and this device's endpoints.
pub(super) fn audio_section(cx: &Cx) -> Vec<Element> {
    let Cx {
        ctx,
        scope,
        rev,
        set_rev,
        ref s,
        ref over,
        preset_mode,
        ..
    } = *cx;
    let (ac_names, ac_i) = presets(AUDIO_CHANNELS, |v| *v == s.audio_channels);
    let channels_combo = setting_combo(ctx, scope, (rev, set_rev), ac_names, ac_i, |s, i| {
        s.audio_channels = AUDIO_CHANNELS[i].0;
    });
    // The lossless-audio opt-in. An unknown stored value (a newer client's row, arriving through a
    // shared preset) shows as Opus — which is what the session resolves it to as well, so the row
    // and the wire agree rather than the combo silently rewriting the user's choice on save.
    let (af_names, af_i) = presets(AUDIO_FORMATS, |v| *v == s.audio_format);
    let format_combo = setting_combo(ctx, scope, (rev, set_rev), af_names, af_i, |s, i| {
        s.audio_format = AUDIO_FORMATS[i].0.to_string();
    });
    let keep_host_audio_toggle =
        setting_toggle(ctx, scope, (rev, set_rev), s.keep_host_audio, |s, on| {
            s.keep_host_audio = on
        });
    let mic_toggle = setting_toggle(ctx, scope, (rev, set_rev), s.mic_enabled, |s, on| {
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
    let dev_combo = |saved: &str,
                     devs: &[pf_client_core::audio::AudioDevice],
                     apply: fn(&mut Settings, String)| {
        let mut names = vec!["System default".to_string()];
        let mut keys = vec![String::new()];
        for d in devs {
            names.push(d.description.clone());
            keys.push(d.name.clone());
        }
        if !saved.is_empty() && !keys.iter().any(|k| k == saved) {
            names.push(format!("{saved} (not detected)"));
            keys.push(saved.to_string());
        }
        (keys.len() > 1).then(|| {
            let current = keys.iter().position(|k| k == saved).unwrap_or(0);
            setting_combo(ctx, scope, (rev, set_rev), names, current, move |s, i| {
                apply(s, keys[i.min(keys.len() - 1)].clone());
            })
        })
    };
    let speaker_combo = dev_combo(&s.speaker_device, &speakers, |s, v| s.speaker_device = v);
    let mic_dev_combo = dev_combo(&s.mic_device, &mics, |s, v| s.mic_device = v);
    // Echo cancellation is meaningless without an uplink, so it greys out with the mic above
    // it. Every commit bumps `rev` and re-renders this screen, so the two stay in step live.
    let echo_toggle = setting_toggle(ctx, scope, (rev, set_rev), s.echo_cancel, |s, on| {
        s.echo_cancel = on
    })
    .enabled(s.mic_enabled);

    let mut out = group(
        None,
        [
            Some(described_overridable(
                (rev, set_rev),
                scope,
                "audio_channels",
                "Audio channels",
                over.audio_channels,
                channels_combo,
                "The speaker layout requested from the host. It downmixes if its own \
                 output has fewer channels.",
            )),
            // The endpoint picks are facts about THIS device's hardware — never
            // per preset, like Decoder/GPU.
            (!preset_mode)
                .then(|| {
                    speaker_combo.map(|c| {
                        described_labeled(
                            "Speaker",
                            c,
                            "Host audio plays here \u{2014} System default follows \
                             the Windows output device.",
                        )
                    })
                })
                .flatten(),
            Some(described_overridable(
                (rev, set_rev),
                scope,
                "mic_enabled",
                "Stream microphone",
                over.mic_enabled,
                mic_toggle,
                "This device\u{2019}s microphone feeds the host\u{2019}s virtual mic. \
                 Ctrl+Alt+Shift+V mutes and unmutes it during a stream.",
            )),
            (!preset_mode)
                .then(|| {
                    mic_dev_combo.map(|c| {
                        described_labeled(
                            "Microphone",
                            c,
                            "The input that feeds the host\u{2019}s virtual mic.",
                        )
                    })
                })
                .flatten(),
        ]
        .into_iter()
        .flatten()
        .collect(),
        Some("Applies from the next session."),
    );

    let d = Settings::default();
    let stereo = s.audio_channels == 2;
    let mut advanced = Vec::new();
    // Stereo-only, so HIDDEN under 5.1/7.1: a lossless surround frame does not fit one
    // QUIC datagram at the default MTU (design/hi-res-audio.md §4.2).
    if stereo {
        advanced.push(described_overridable(
            (rev, set_rev),
            scope,
            "audio_format",
            "Audio quality",
            over.audio_format,
            format_combo,
            "Lossless sends uncompressed PCM instead of Opus \u{2014} bit-exact, at \
             2.3\u{2013}4.6 Mb/s taken off the top of the link and outside the \
             automatic-bitrate loop. The host has its own switch, off by default, and \
             quietly stays on Opus if it can\u{2019}t deliver the rate; the stats overlay \
             names what the session actually got.",
        ));
    }
    advanced.extend([
        described_overridable(
            (rev, set_rev),
            scope,
            "keep_host_audio",
            "Keep host audio playing",
            over.keep_host_audio,
            keep_host_audio_toggle,
            "The host\u{2019}s own speakers or headphones keep playing while you stream \
             \u{2014} both ends hear the same audio. Needs a host on 0.32 or newer.",
        ),
        described_overridable(
            (rev, set_rev),
            scope,
            "echo_cancel",
            "Echo cancellation",
            over.echo_cancel,
            echo_toggle,
            "Keeps the host\u{2019}s audio, playing from this machine\u{2019}s speakers, \
             from being picked up and sent straight back. Turn it off if your microphone \
             already does its own processing.",
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
        over.audio_format || over.keep_host_audio || over.echo_cancel,
    ));
    out
}
