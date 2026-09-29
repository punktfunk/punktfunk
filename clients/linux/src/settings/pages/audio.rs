//! Audio: channels, this device's speaker and microphone, the uplink.

use super::Build;
use crate::settings::field::Field;
use crate::settings::spec::{self, Page, Spec};
use crate::settings::tables::*;
use crate::trust::Settings;
use adw::prelude::*;
use pf_client_core::audio::AudioDevice;
use std::rc::Rc;

pub fn audio(b: &mut Build) {
    let mut p = b.page(Page::Audio, "audio-volume-high-symbolic");
    let g = p.group("", "Applies from the next session.");
    let channels = b.choice(
        &spec::AUDIO_CHANNELS,
        &["Stereo", "5.1 Surround", "7.1 Surround"],
    );
    // Lossless is stereo-only: a lossless surround frame does not fit one QUIC datagram at the
    // default MTU (design/hi-res-audio.md §4.2). Greyed, so the reason stays beside its cause.
    let labels: Vec<&str> = AUDIO_FORMATS.iter().map(|(_, l)| *l).collect();
    let format = b.choice(&spec::AUDIO_FORMAT, &labels);
    {
        let w = format.widget().clone();
        channels.connect_changed(move |i| w.set_sensitive(i == 0));
    }
    b.put(
        &mut p,
        Some(&g),
        Field::choice(&spec::AUDIO_CHANNELS, &channels, index::surround, |s, i| {
            s.audio_channels = match i {
                1 => 6,
                2 => 8,
                _ => 2,
            }
        }),
    );
    let speaker = device(
        b,
        &spec::SPEAKER,
        &b.probes.speakers,
        |s| &s.speaker_device,
        |s, v| s.speaker_device = v,
    );
    if let Some(f) = speaker {
        b.put(&mut p, Some(&g), f);
    }

    // The microphone picker and the echo canceller follow the mic switch.
    let (field, mic) = Field::switch(&spec::MIC, |s| s.mic_enabled, |s, v| s.mic_enabled = v);
    b.put(&mut p, Some(&g), field);
    let (echo_field, echo) = Field::switch(
        &spec::ECHO_CANCEL,
        |s| s.echo_cancel,
        |s, v| s.echo_cancel = v,
    );
    let mut follows_mic: Vec<gtk::Widget> = vec![echo.upcast()];
    let mic_device = device(
        b,
        &spec::MIC_DEVICE,
        &b.probes.mics,
        |s| &s.mic_device,
        |s, v| s.mic_device = v,
    );
    if let Some(f) = mic_device {
        follows_mic.push(f.row.clone().upcast());
        b.put(&mut p, Some(&g), f);
    }
    for w in &follows_mic {
        w.set_sensitive(false);
    }
    mic.connect_active_notify(move |m| {
        for w in &follows_mic {
            w.set_sensitive(m.is_active());
        }
    });

    b.put(
        &mut p,
        None,
        Field::choice(&spec::AUDIO_FORMAT, &format, index::audio_format, |s, i| {
            s.audio_format = at(AUDIO_FORMATS, i).0.to_string()
        }),
    );
    let (field, _) = Field::switch(
        &spec::KEEP_HOST_AUDIO,
        |s| s.keep_host_audio,
        |s, v| s.keep_host_audio = v,
    );
    b.put(&mut p, None, field);
    b.put(&mut p, None, echo_field);
    b.finish(p);
}

/// An endpoint picker: labels are descriptions, the stored value the node name. `None` when
/// the probe found nothing; a saved device that is gone keeps an entry to move off.
fn device(
    b: &Build,
    spec: &'static Spec,
    devs: &[AudioDevice],
    get: fn(&Settings) -> &String,
    set: fn(&mut Settings, String),
) -> Option<Field> {
    let saved = get(b.seed);
    let mut names = vec!["System default".to_string()];
    let mut keys = vec![String::new()];
    for d in devs {
        names.push(d.description.clone());
        keys.push(d.name.clone());
    }
    if !saved.is_empty() && !keys.contains(saved) {
        names.push(format!("{saved} (not detected)"));
        keys.push(saved.clone());
    }
    if keys.len() < 2 {
        return None;
    }
    let row = b.choice(spec, &names.iter().map(String::as_str).collect::<Vec<_>>());
    let keys = Rc::new(keys);
    let k = keys.clone();
    Some(Field::choice(
        spec,
        &row,
        move |s| k.iter().position(|k| k == get(s)).unwrap_or(0) as u32,
        move |s, i| set(s, at(&keys, i).clone()),
    ))
}
