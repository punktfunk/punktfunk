//! The values each picker stores, its labels and captions, and where a stored value sits in
//! its list. Seeding a row and resetting one read the same [`index`] functions.

use crate::trust::Settings;
pub use pf_client_core::session::AUDIO_FORMATS;
use pf_client_core::settings::{MouseMode, TouchMode};
use pf_client_core::trust::{HudCorner, StatsVerbosity};
use pf_l10n::client;
use punktfunk_core::hud::{stats_scale, STATS_SCALE_PCTS};

/// A message, read when a row is built, so a table holds what to say and not the words.
pub type Text = fn() -> &'static str;

/// The words of a table of messages.
pub fn texts(table: &[Text]) -> Vec<&'static str> {
    table.iter().map(|t| t()).collect()
}

/// Sizes by family; the Resolution row lists one family at a time behind the Aspect row.
/// `(0, 0)` = the native size of the monitor the window is on, resolved at connect.
pub use punktfunk_core::resolutions::{aspect_of, nearest, ASPECTS};
/// `0` = the monitor's native refresh, resolved at connect.
pub const REFRESH: &[u32] = &[0, 30, 60, 90, 120, 144, 165, 240];
/// Render-scale multipliers. `1.0` = Native; applied at connect and each match-window resize.
pub use punktfunk_core::render_scale::PRESETS as RENDER_SCALES;

/// Where each picker sits for a given settings snapshot. Factored out because two places need
/// exactly this: seeding the dialog, and putting ONE row back to the inherited value when its
/// override is reset — and those two must agree, or a reset would land on a different option
/// than reopening the dialog would show.
pub mod index {
    use super::*;

    /// The family the Resolution row lists: the stored size's shape, or 16:9 for Native.
    pub fn aspect(s: &Settings) -> u32 {
        aspect_of(s.width, s.height).unwrap_or(0) as u32
    }

    pub fn resolution(s: &Settings) -> u32 {
        // 0 = Native, 1 = the virtual "Match window" entry, 2.. = the family's sizes, and
        // Custom last for a size no family lists.
        if s.match_window {
            return 1;
        }
        if s.width == 0 {
            return 0;
        }
        let family = aspect(s) as usize;
        ASPECTS[family]
            .sizes
            .iter()
            .position(|&(w, h)| w == s.width && h == s.height)
            .map_or(custom(family), |i| i as u32 + 2)
    }

    /// The Resolution row's Custom entry for a family: after Native, Match window and its sizes.
    pub fn custom(family: usize) -> u32 {
        ASPECTS[family].sizes.len() as u32 + 2
    }

    pub fn refresh(s: &Settings) -> u32 {
        REFRESH.iter().position(|&r| r == s.refresh_hz).unwrap_or(0) as u32
    }

    /// The Render scale row's Custom entry: after the stops.
    pub const CUSTOM_SCALE: u32 = RENDER_SCALES.len() as u32;

    /// A stop's entry, or Custom for a typed scale.
    pub fn render_scale(s: &Settings) -> u32 {
        punktfunk_core::render_scale::preset_index(s.render_scale)
            .map_or(CUSTOM_SCALE, |i| i as u32)
    }

    pub fn codec(s: &Settings) -> u32 {
        CODECS.iter().position(|&c| c == s.codec).unwrap_or(0) as u32
    }

    pub fn compositor(s: &Settings) -> u32 {
        COMPOSITORS
            .iter()
            .position(|&c| c == s.compositor)
            .unwrap_or(0) as u32
    }

    pub fn stats(s: &Settings) -> u32 {
        StatsVerbosity::ALL
            .iter()
            .position(|v| *v == s.stats_verbosity())
            .unwrap_or(0) as u32
    }

    pub fn touch(s: &Settings) -> u32 {
        TOUCH_MODES
            .iter()
            .position(|&t| t == s.touch_mode)
            .unwrap_or(0) as u32
    }

    pub fn mouse(s: &Settings) -> u32 {
        MOUSE_MODES
            .iter()
            .position(|&m| m == s.mouse_mode)
            .unwrap_or(0) as u32
    }

    pub fn surround(s: &Settings) -> u32 {
        match s.audio_channels {
            6 => 1,
            8 => 2,
            _ => 0,
        }
    }

    /// An unknown stored value (a newer client's row, via a shared preset) reads as row 0 —
    /// Opus — which is also what the session resolves it to, so the row and the wire agree.
    pub fn audio_format(s: &Settings) -> u32 {
        AUDIO_FORMATS
            .iter()
            .position(|(v, _)| *v == s.audio_format)
            .unwrap_or(0) as u32
    }

    pub fn gamepad(s: &Settings) -> u32 {
        GAMEPADS.iter().position(|&g| g == s.gamepad).unwrap_or(0) as u32
    }

    pub fn system_buttons(s: &Settings) -> u32 {
        SYSTEM_BUTTONS
            .iter()
            .position(|&v| v == s.system_buttons)
            .unwrap_or(0) as u32
    }

    pub fn video_fit(s: &Settings) -> u32 {
        // Unknown values (a newer client's mode) read as Fit, as the presenter does.
        VIDEO_FITS
            .iter()
            .position(|&v| v == s.video_fit)
            .unwrap_or(0) as u32
    }

    pub fn screens(s: &Settings) -> u32 {
        u32::from(s.all_screens())
    }

    pub fn present_priority(s: &Settings) -> u32 {
        // Unknown values (a newer client's intent) read as the default, exactly as
        // `PresentPriority::resolve` treats them.
        PRESENT_PRIORITIES
            .iter()
            .position(|&p| p == s.present_priority)
            .unwrap_or(0) as u32
    }

    pub fn smooth_buffer(s: &Settings) -> u32 {
        // The index IS the stored value: 0 = Automatic, 1..3 = frames.
        u32::from(s.smooth_buffer).min(SMOOTH_BUFFER_STEPS - 1)
    }

    /// The stats corner: the stored one, else the desktop overlay's top left.
    pub fn stats_position(s: &Settings) -> u32 {
        let corner = s.hud_corner(HudCorner::TopLeft);
        HudCorner::ALL
            .iter()
            .position(|c| *c == corner)
            .unwrap_or(0) as u32
    }

    /// The stats size in effect, on the offered steps; an off-step value reads as 100 %.
    pub fn stats_size(s: &Settings) -> u32 {
        let pct = (stats_scale(s.stats_scale_pct) * 100.0).round() as u16;
        (STATS_SCALE_PCTS.iter().position(|p| *p == pct))
            .or_else(|| STATS_SCALE_PCTS.iter().position(|p| *p == 100))
            .unwrap_or(0) as u32
    }
}

/// A table's entry at a row's index, the last one past the end.
pub fn at<T>(table: &[T], i: u32) -> &T {
    &table[(i as usize).min(table.len() - 1)]
}

/// A compact label for a render-scale multiplier: "Native" / "1.5×" / "2× (supersample)".
pub fn render_scale_label(scale: f64) -> String {
    if scale == 1.0 {
        client::player_render_scale_native().to_string()
    } else {
        // Just the multiplier: the row's caption already says what above and below 1× mean,
        // and "2× (supersample)" is long enough that the value ellipsizes to "2× (su…" the
        // moment the row grows another suffix.
        format!("{scale}×")
    }
}
pub const GAMEPADS: &[&str] = &[
    "auto",
    "xbox360",
    "dualsense",
    "xboxone",
    "dualshock4",
    "steamdeck",
    "steamcontroller2",
];
/// System-button routing values (persisted under the cross-client `system_buttons` key):
/// where the guide (Xbox/PS/Steam) and quick-access presses land while streaming. Auto =
/// the host, except under Gaming Mode where the local Steam UI reacts to the same press.
pub const SYSTEM_BUTTONS: &[&str] = &["auto", "forward", "local"];
pub const SYSTEM_BUTTON_LABELS: &[Text] = &[
    client::player_system_buttons_auto,
    client::player_system_buttons_forward,
    client::player_system_buttons_local,
];
pub const COMPOSITORS: &[&str] = &[
    "auto",
    "kwin",
    "mutter",
    "hyprland",
    "niri",
    "umbriel",
    "wlroots",
    "gamescope",
];
/// Codec setting values (persisted) paired with their display labels below. PyroWave is
/// preference-only by design (`Settings::preferred_codec`) — the ladder falls back to
/// HEVC when either side can't do it.
pub const CODECS: &[&str] = &["auto", "hevc", "h264", "av1", "pyrowave"];
pub const CODEC_LABELS: &[Text] = &[
    client::player_codec_auto,
    || "HEVC",
    || "H.264",
    || "AV1",
    client::player_codec_pyrowave,
];
// Stored decoder-preference values. `native-*` since M10 — the bare "vulkan"/"vaapi"
// named libavcodec's rungs, which are deleted; a store still holding them is migrated on
// read (`pf_client_core::video::migrate_decoder_pref`) and simply matches no entry here
// until the user re-picks. The labels below are unchanged and still true.
pub const DECODERS: &[&str] = &["auto", "native-vulkan", "native-vaapi", "software"];
/// Touch-input model values (persisted) paired with their display labels below — the
/// cross-client set (Android/Apple). Only meaningful on a touchscreen (Deck/tablet).
pub const TOUCH_MODES: &[&str] = &["trackpad", "pointer", "touch", "off"];
pub fn touch_mode_labels() -> Vec<&'static str> {
    TOUCH_MODES
        .iter()
        .map(|name| TouchMode::from_name(name).label())
        .collect()
}
/// The SELECTED touch mode explained — the caption swaps with the choice (the Apple
/// revamp's dynamic-caption idiom) instead of narrating every mode at once.
/// Combo-row captions must stay ONE line (~66 chars at the default dialog width): a
/// wrapped subtitle's natural width crushes the selected-value label into an ellipsis.
pub const TOUCH_MODE_CAPTIONS: &[Text] = &[
    client::player_touch_mode_trackpad_hint,
    client::player_touch_mode_pointer_hint,
    client::player_touch_mode_touch_hint,
    client::player_touch_mode_off_hint,
];
/// `video_fit` values + labels + one-line captions, index-aligned.
pub const VIDEO_FITS: &[&str] = &["fit", "crop", "stretch"];
pub const VIDEO_FIT_LABELS: &[Text] = &[
    client::player_video_fit_fit,
    client::player_video_fit_crop,
    client::player_video_fit_stretch,
];
pub const VIDEO_FIT_CAPTIONS: &[Text] = &[
    client::player_video_fit_fit_hint,
    client::player_video_fit_crop_hint,
    client::player_video_fit_stretch_hint,
];
/// `screens` values + labels + one-line captions, index-aligned.
pub const SCREENS: &[&str] = &["this", "all"];
pub const SCREENS_LABELS: &[Text] = &[client::player_screens_this, client::player_screens_all];
pub const SCREENS_CAPTIONS: &[Text] = &[
    client::player_screens_this_hint,
    client::player_screens_all_hint,
];
/// Presentation-intent values (persisted under the `present_priority` key the Apple and
/// Android clients share) + labels + dynamic captions. Captions stay ONE line, like the
/// touch/mouse rows.
pub const PRESENT_PRIORITIES: &[&str] = &["latency", "smooth"];
pub const PRESENT_PRIORITY_LABELS: &[Text] = &[
    client::player_present_priority_latency,
    client::player_present_priority_smooth,
];
pub const PRESENT_PRIORITY_CAPTIONS: &[Text] = &[
    client::player_present_priority_latency_hint,
    client::player_present_priority_smooth_hint,
];
/// Smoothness buffer depth, in frames — the index IS the stored `smooth_buffer` value
/// (0 = Automatic, which resolves to 2). No millisecond hints: the cost is one refresh
/// per frame, and the session's refresh isn't known here when the mode is Native.
pub const SMOOTH_BUFFER_STEPS: u32 = 4;

/// The Smoothness buffer row's entries: Automatic, then 1 to 3 frames.
pub fn smooth_buffer_labels() -> Vec<String> {
    (0..SMOOTH_BUFFER_STEPS)
        .map(|frames| match frames {
            0 => client::player_smooth_buffer_automatic().to_string(),
            n => client::player_smooth_buffer_frames(u64::from(n)),
        })
        .collect()
}

/// Physical-mouse model values (persisted) + labels + dynamic captions — same idiom as
/// the touch rows. Ctrl+Alt+Shift+M flips the model live in-stream.
pub const MOUSE_MODES: &[&str] = &["capture", "desktop"];
pub fn mouse_mode_labels() -> Vec<&'static str> {
    MOUSE_MODES
        .iter()
        .map(|name| MouseMode::from_name(name).label())
        .collect()
}
pub const MOUSE_MODE_CAPTIONS: &[Text] = &[
    client::player_mouse_mode_capture_hint,
    client::player_mouse_mode_desktop_hint,
];

/// The SELECTED resolution choice explained (row index: 0 = Native, 1 = Match window,
/// 2.. = explicit sizes) — one line each, see the caption-width note on
/// [`TOUCH_MODE_CAPTIONS`].
pub fn resolution_caption(i: u32) -> &'static str {
    match i {
        0 => client::player_resolution_native_hint(),
        1 => client::player_resolution_match_window_hint(),
        _ => client::player_resolution_hint(),
    }
}

/// The Resolution row's entries for one family: the D1 tri-state's Native and Match window,
/// that family's sizes, then Custom, which shows the Width and Height rows.
pub fn resolution_names(family: usize) -> Vec<String> {
    [
        client::player_resolution_native().to_string(),
        client::player_resolution_match_window().to_string(),
    ]
    .into_iter()
    .chain(
        ASPECTS[family]
            .sizes
            .iter()
            .map(|&(w, h)| format!("{w} × {h}")),
    )
    .chain([client::player_resolution_custom().to_string()])
    .collect()
}

/// The SELECTED codec explained: the PyroWave entry is the one that needs its trade-off
/// spelled out.
pub fn codec_caption(i: u32) -> &'static str {
    if CODECS.get(i as usize) == Some(&"pyrowave") {
        client::player_codec_pyrowave_hint()
    } else {
        client::player_codec_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored value a table does not list seeds its row at a fallback rung, so only the
    /// user moving that row may write it. A size is the exception: one no family lists
    /// seeds Custom, which shows it.
    ///
    /// No display needed: these are the pure index helpers the rows are seeded from.
    #[test]
    fn off_ladder_values_seed_a_fallback_rung() {
        // A size typed on another client: 3:2 by shape, listed by no family.
        let custom = Settings {
            width: 1500,
            height: 1000,
            ..Default::default()
        };
        assert_eq!(index::aspect(&custom), 4, "lists the 3:2 family");
        assert_eq!(
            index::resolution(&custom),
            index::custom(4),
            "seeds Custom, which holds 1500x1000"
        );
        assert_eq!(
            resolution_names(4).last().map(String::as_str),
            Some("Custom\u{2026}")
        );
        assert!(
            !ASPECTS
                .iter()
                .any(|a| a.sizes.contains(&(custom.width, custom.height))),
            "the premise: no family can show it"
        );
        let native = Settings::default();
        assert_eq!(
            index::resolution(&native),
            0,
            "Native stays the first entry"
        );
        // The Steam Deck's panel is the first 16:10 size: family 1, row 2 (after Native and
        // Match window), so it round-trips.
        let deck = Settings {
            width: 1280,
            height: 800,
            ..Default::default()
        };
        assert_eq!((index::aspect(&deck), index::resolution(&deck)), (1, 2));

        // A refresh rung the console's table lacks round-trips here, so it must be written.
        let fast = Settings {
            refresh_hz: 240,
            ..Default::default()
        };
        assert!(REFRESH.contains(&fast.refresh_hz));
        assert_eq!(REFRESH[index::refresh(&fast) as usize], 240);

        // One this table lacks seeds rung 0 and must NOT be written back.
        let odd = Settings {
            refresh_hz: 75,
            ..Default::default()
        };
        assert!(!REFRESH.contains(&odd.refresh_hz));
        assert_eq!(index::refresh(&odd), 0);

        // A scale that is no stop seeds Custom, past the last stop, so the typed value
        // shows as typed. Native's stop is not index 0, so "unmoved" cannot be spelled as
        // "index 0" for this row.
        let odd_scale = Settings {
            render_scale: 0.63,
            ..Default::default()
        };
        assert_eq!(index::render_scale(&odd_scale), index::CUSTOM_SCALE);
        let native = index::render_scale(&Settings::default());
        assert!(RENDER_SCALES[native as usize] == 1.0 && native != 0);
    }
}
