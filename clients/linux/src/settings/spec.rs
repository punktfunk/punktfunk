//! Every setting row the dialog shows, as data: the key it stores, its label and caption, its
//! page, whether it waits behind Show advanced, and which layer it edits. Rows take their text
//! from here, and the catalog test holds this table to `clients/shared/settings-catalog.json`.

use super::tables::{
    resolution_caption, MOUSE_MODE_CAPTIONS, PRESENT_PRIORITY_CAPTIONS, SCREENS_CAPTIONS,
    TOUCH_MODE_CAPTIONS, VIDEO_FIT_CAPTIONS,
};
use pf_l10n::{client, desktop};
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    General,
    Display,
    Input,
    Audio,
    Controllers,
}

impl Page {
    /// The category the settings catalogue files the page under, and the page's name in the
    /// dialog. Not for display: see [`Page::title`].
    pub fn id(self) -> &'static str {
        match self {
            Page::General => "general",
            Page::Display => "display",
            Page::Input => "input",
            Page::Audio => "audio",
            Page::Controllers => "controllers",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Page::General => client::prefs_category_general(),
            Page::Display => client::prefs_category_display(),
            Page::Input => client::prefs_category_input(),
            Page::Audio => client::prefs_category_audio(),
            Page::Controllers => client::prefs_category_controllers(),
        }
    }
}

/// Which layer a row edits, and so in which scope it shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    /// A stream setting: the defaults, or in preset scope that preset's override.
    Stream,
    /// This device's. Shown in the defaults scope only.
    Device,
    /// This device's, shown in both scopes: it changes the dialog, not a stream.
    Dialog,
    /// A stream setting whose defaults-scope row is a device variant (the fullscreen switch).
    PresetOnly,
}

impl Layer {
    /// Whether a preset can override the row.
    pub fn presetable(self) -> bool {
        matches!(self, Layer::Stream | Layer::PresetOnly)
    }

    /// Whether the row shows in this scope.
    pub fn shown(self, preset_scope: bool) -> bool {
        match self {
            Layer::Stream | Layer::Dialog => true,
            Layer::Device => !preset_scope,
            Layer::PresetOnly => preset_scope,
        }
    }
}

#[derive(Debug)]
pub struct Spec {
    /// The catalog key, and the name a preset pins it under.
    pub key: &'static str,
    title: fn() -> &'static str,
    caption: fn() -> Cow<'static, str>,
    pub page: Page,
    pub advanced: bool,
    pub layer: Layer,
}

impl Spec {
    pub fn title(&self) -> &'static str {
        (self.title)()
    }

    pub fn caption(&self) -> Cow<'static, str> {
        (self.caption)()
    }
}

/// A row's title and caption are read each time the row is built, so a message such as
/// `client::player_stats_scale_pct()` reads in the language `pf_l10n` settled on. A caption
/// may be a message or a few joined, so it is a `Cow`.
macro_rules! specs {
    ($($name:ident: $key:literal, $title:expr, $caption:expr, $page:ident, $advanced:literal, $layer:ident;)*) => {
        $(pub const $name: Spec = Spec {
            key: $key,
            title: || $title,
            caption: || Cow::from($caption),
            page: Page::$page,
            advanced: $advanced,
            layer: Layer::$layer,
        };)*
        #[cfg(test)]
        pub const SPECS: &[Spec] = &[$($name),*];
    };
}

specs! {
    FULLSCREEN: "fullscreen", client::player_fullscreen(),
        format!("{} {}", desktop::desktop_fullscreen_hint(),
            desktop::desktop_fullscreen_exit_hint("F11")), General, false, Device;
    FULLSCREEN_ON_STREAM: "fullscreen_on_stream", client::player_fullscreen_on_stream(),
        desktop::desktop_fullscreen_on_stream_exit_hint("F11", "L1+R1+Start+Select"),
        General, false, PresetOnly;
    AUTO_WAKE: "auto_wake", client::player_auto_wake(), client::player_auto_wake_hint(),
        General, false, Device;
    START_IN: "start_in", client::player_start_in(), "", General, false, Device;
    GAMEPAD_UI: "gamepad_ui_enabled", client::player_gamepad_ui_enabled(),
        client::player_gamepad_ui_enabled_hint(), General, false, Device;
    GAMEPAD_UI_MODE: "gamepad_ui_mode", client::player_gamepad_ui_mode(),
        client::player_gamepad_ui_mode_hint(), General, false, Device;
    FOLLOW_OS_THEME: "follow_os_theme", client::player_follow_os_theme(),
        client::player_follow_os_theme_hint(), General, false, Device;
    OMARCHY_MENU: "omarchy_menu", desktop::desktop_omarchy_menu(),
        desktop::desktop_omarchy_menu_hint("Super+Space"), General, false, Device;
    STATS: "stats_verbosity", client::player_stats_verbosity(),
        format!("{} {}", client::player_stats_verbosity_hint(),
            client::player_stats_verbosity_cycle_hint("Ctrl+Alt+Shift+S")),
        General, false, Stream;
    SHOW_ADVANCED: "show_advanced", client::player_show_advanced(),
        client::player_show_advanced_hint(), General, false, Dialog;
    ADVANCED_STATS: "advanced_stats", client::player_advanced_stats(),
        client::player_advanced_stats_hint(), General, true, Device;
    STATS_POSITION: "hud_placement", client::player_hud_placement(),
        client::player_hud_placement_hint(), General, true, Device;
    STATS_SIZE: "stats_scale_pct", client::player_stats_scale_pct(),
        client::player_stats_scale_pct_hint(), General, true, Device;
    EXIT_HINT: "exit_hint", client::player_exit_hint(), client::player_exit_hint_hint(),
        General, true, Device;

    RESOLUTION: "resolution", client::player_resolution(), resolution_caption(0),
        Display, false, Stream;
    REFRESH: "refresh_hz", client::player_refresh_hz(), client::player_refresh_hz_hint(),
        Display, false, Stream;
    BITRATE: "bitrate_kbps", client::player_bitrate_kbps(),
        desktop::desktop_bitrate_kbps_hint(), Display, false, Stream;
    VIDEO_FIT: "video_fit", client::player_video_fit(), VIDEO_FIT_CAPTIONS[0](),
        Display, false, Stream;
    SCREENS: "screens", client::player_screens(), SCREENS_CAPTIONS[0](), Display, false, Stream;
    HDR: "hdr_enabled", client::player_hdr_enabled(), client::player_hdr_enabled_hint(),
        Display, false, Stream;
    PRESENT: "present_priority", client::player_present_priority(),
        PRESENT_PRIORITY_CAPTIONS[0](), Display, false, Stream;
    SMOOTH_BUFFER: "smooth_buffer", client::player_smooth_buffer(),
        client::player_smooth_buffer_hint(), Display, true, Stream;
    RENDER_SCALE: "render_scale", client::player_render_scale(),
        client::player_render_scale_hint(), Display, true, Stream;
    CODEC: "codec", client::player_codec(), client::player_codec_hint(), Display, false, Stream;
    PYROWAVE_QUALITY: "pyrowave_bpp", client::player_pyrowave_bpp(), "", Display, false, Stream;
    CHROMA: "enable_444", client::player_enable_444(), client::player_enable_444_hint(),
        Display, true, Stream;
    TEN_BIT_SDR: "ten_bit_sdr", client::player_ten_bit_sdr(), client::player_ten_bit_sdr_hint(),
        Display, true, Stream;
    VSYNC: "vsync", "V-Sync", client::player_vsync_hint(), Display, true, Stream;
    VRR: "allow_vrr", client::player_allow_vrr(), client::player_allow_vrr_hint(),
        Display, true, Stream;
    COMPOSITOR: "compositor", client::player_compositor(), client::player_compositor_hint(),
        Display, true, Stream;
    DECODER: "decoder", client::player_decoder(), client::player_decoder_hint(),
        Display, true, Device;
    ADAPTER: "adapter", desktop::desktop_adapter(), desktop::desktop_adapter_hint(),
        Display, true, Device;
    KEEP_AWAKE: "keep_awake", client::player_keep_awake(), client::player_keep_awake_hint(),
        Display, true, Device;

    TOUCH: "touch_mode", client::player_touch_mode(), TOUCH_MODE_CAPTIONS[0](),
        Input, false, Stream;
    MOUSE: "mouse_mode", client::player_mouse_mode(), MOUSE_MODE_CAPTIONS[0](),
        Input, false, Stream;
    SHORTCUTS: "inhibit_shortcuts", client::player_inhibit_shortcuts(),
        client::player_inhibit_shortcuts_hint("Alt+Tab, Super, \u{2026}"), Input, false, Stream;
    INVERT_SCROLL: "invert_scroll", client::player_invert_scroll(),
        client::player_invert_scroll_hint(), Input, false, Stream;
    QUICK_ACTIONS: "overlay_actions", client::player_overlay_actions(), "", Input, false, Stream;

    AUDIO_CHANNELS: "audio_channels", client::player_audio_channels(),
        client::player_audio_channels_hint(), Audio, false, Stream;
    SPEAKER: "speaker_device", client::player_speaker_device(),
        client::player_speaker_device_hint(), Audio, false, Device;
    MIC: "mic_enabled", client::player_mic_enabled(),
        format!("{} {}", client::player_mic_enabled_hint(),
            client::player_mic_enabled_keys_hint("Ctrl+Alt+Shift+V")), Audio, false, Stream;
    MIC_DEVICE: "mic_device", client::player_mic_device(), client::player_mic_device_hint(),
        Audio, false, Device;
    AUDIO_FORMAT: "audio_format", client::player_audio_format(),
        client::player_audio_format_hint(), Audio, true, Stream;
    KEEP_HOST_AUDIO: "keep_host_audio", client::player_keep_host_audio(),
        client::player_keep_host_audio_hint(), Audio, true, Stream;
    ECHO_CANCEL: "echo_cancel", client::player_echo_cancel(), client::player_echo_cancel_hint(),
        Audio, true, Stream;

    GAMEPAD: "gamepad", client::player_gamepad(), client::player_gamepad_hint(),
        Controllers, false, Stream;
    PAD_RUMBLE: "pad_rumble", client::player_pad_rumble(), client::player_pad_rumble_hint(),
        Controllers, false, Device;
    FORWARDING: "gamepad_forwarding", client::player_gamepad_forwarding(),
        client::player_gamepad_forwarding_hint(), Controllers, true, Stream;
    FORWARD_PAD: "forward_pad", client::player_forward_pad(), client::player_forward_pad_hint(),
        Controllers, true, Device;
    SYSTEM_BUTTONS: "system_buttons", client::player_system_buttons(),
        client::player_system_buttons_hint(), Controllers, true, Stream;
    GUIDE_GESTURE: "guide_gesture", client::player_guide_gesture(),
        client::player_guide_gesture_hint("Select"), Controllers, true, Stream;
    PAD_HAPTICS: "pad_haptics", client::player_pad_haptics(), client::player_pad_haptics_hint(),
        Controllers, true, Device;
    PAD_SPEAKER: "pad_speaker", client::player_pad_speaker(), client::player_pad_speaker_hint(),
        Controllers, true, Device;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catalog keys this client has no row for, and why.
    const ABSENT: &[(&str, &str)] = &[
        (
            "background_keep_alive",
            "a phone or TV app in the background",
        ),
        (
            "background_timeout_minutes",
            "a phone or TV app in the background",
        ),
        ("ui_palette", "the console's own look"),
        ("reduce_motion", "the console's own look"),
        ("reduce_ui_resolution", "a weak GPU's console"),
        ("library_view", "the console's library"),
        ("library_sections", "Customize on the Library header"),
        ("host_sort", "Sort on the Hosts header"),
        ("host_grouping", "Group on the Hosts header"),
        ("low_latency", "a MediaCodec decoder flag"),
        ("second_screen", "an Android handheld's second screen"),
        ("audio_route", "webOS's own audio plane"),
        ("cursor_gestures", "a TV remote's missing second button"),
        ("rumble_on_phone", "a phone's own hardware"),
        ("gyro_on_phone", "a phone's own hardware"),
        ("sc2_capture", "Apple only"),
        ("ds_capture", "Android and webOS only"),
        ("spatial_audio", "a Vision Pro's room"),
        ("screen", "a Vision Pro's room"),
    ];

    /// Rows with no catalog entry: this client's own variants and device pickers.
    const LOCAL: &[&str] = &[
        "fullscreen",
        "omarchy_menu",
        "adapter",
        "speaker_device",
        "mic_device",
    ];

    /// Each row that edits a shared setting carries the catalog's label, category and tier,
    /// and every catalog entry has a row here or a reason it does not.
    #[test]
    fn rows_match_the_settings_catalog() {
        let raw = include_str!("../../../shared/settings-catalog.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("the catalog parses");
        let entries = file["settings"].as_array().expect("settings");
        for spec in SPECS {
            let Some(e) = entries.iter().find(|e| e["key"] == spec.key) else {
                assert!(
                    LOCAL.contains(&spec.key),
                    "{} is not in the catalog",
                    spec.key
                );
                continue;
            };
            assert_eq!(spec.title(), e["label"].as_str().unwrap(), "{}", spec.key);
            assert_eq!(
                spec.page.id(),
                e["category"].as_str().unwrap(),
                "{}",
                spec.key
            );
            assert_eq!(
                spec.advanced,
                e["advanced"].as_bool().unwrap(),
                "{}",
                spec.key
            );
        }
        for e in entries {
            let key = e["key"].as_str().unwrap();
            let bound = SPECS.iter().any(|s| s.key == key);
            let absent = ABSENT.iter().any(|(k, _)| *k == key);
            assert!(
                bound != absent,
                "{key}: a row or a reason, not both or neither"
            );
        }
    }

    /// A preset pins exactly the rows it can show.
    #[test]
    fn presetable_rows_are_ones_a_preset_can_pin() {
        let from = crate::trust::Settings::default();
        for spec in SPECS {
            let mut o = pf_client_core::presets::SettingsOverlay::default();
            assert_eq!(
                spec.layer.presetable(),
                o.pin(spec.key, &from),
                "{}",
                spec.key
            );
        }
    }
}
