//! Every setting row the pages show, as data: the key it stores under and the label it carries.
//! The pages take both from here, so a row cannot show a label this table does not hold, and
//! the catalog test holds the table to `clients/shared/settings-catalog.json`. A row's page and
//! tier are stated here for that test; its place is laid out by hand in its page.

/// One row of a settings page.
pub(super) struct Row {
    /// The stored key, and the name a preset pins it under.
    pub(super) key: &'static str,
    label: fn() -> &'static str,
    /// The sidebar page it sits on, by its tag.
    #[cfg(test)]
    page: &'static str,
    /// Whether it waits behind Show advanced.
    #[cfg(test)]
    advanced: bool,
}

impl Row {
    pub(super) fn label(&self) -> &'static str {
        (self.label)()
    }
}

/// A row's label is read each time the page renders, so a message such as
/// `pf_l10n::client::player_stats_scale_pct()` reads in the language `pf_l10n` settled on.
macro_rules! rows {
    ($($name:ident: $key:literal, $label:expr, $page:literal, $advanced:literal;)*) => {
        $(pub(super) const $name: Row = Row {
            key: $key,
            label: || $label,
            #[cfg(test)]
            page: $page,
            #[cfg(test)]
            advanced: $advanced,
        };)*
        #[cfg(test)]
        const ROWS: &[Row] = &[$($name),*];
    };
}

// Name: key, label, the sidebar page it sits on, whether it waits behind Show advanced.
rows! {
    FULLSCREEN_ON_STREAM: "fullscreen_on_stream", "Start streams fullscreen", "general", false;
    AUTO_WAKE: "auto_wake", "Auto-wake on connect", "general", false;
    START_IN: "start_in", "Start in", "general", false;
    GAMEPAD_UI: "gamepad_ui_enabled", "Controller-optimized UI", "general", false;
    GAMEPAD_UI_MODE: "gamepad_ui_mode", "Show it", "general", false;
    STATS: "stats_verbosity", "Statistics overlay", "general", false;
    SHOW_ADVANCED: "show_advanced", "Show advanced", "general", false;
    ADVANCED_STATS: "advanced_stats", "Advanced statistics", "general", true;
    STATS_POSITION: "hud_placement", "Statistics position", "general", true;
    STATS_SIZE: "stats_scale_pct", pf_l10n::client::player_stats_scale_pct(), "general", true;
    EXIT_HINT: "exit_hint", "Exit hint", "general", true;

    RESOLUTION: "resolution", "Resolution", "display", false;
    REFRESH: "refresh_hz", "Refresh rate", "display", false;
    BITRATE: "bitrate_kbps", "Bitrate", "display", false;
    VIDEO_FIT: "video_fit", "Picture fit", "display", false;
    SCREENS: "screens", "Screens", "display", false;
    HDR: "hdr_enabled", "10-bit HDR", "display", false;
    PRESENT: "present_priority", "Prioritize", "display", false;
    SMOOTH_BUFFER: "smooth_buffer", "Smoothness buffer", "display", true;
    RENDER_SCALE: "render_scale", "Render scale", "display", true;
    CODEC: "codec", "Video codec", "display", false;
    PYROWAVE_QUALITY: "pyrowave_bpp", "PyroWave quality", "display", false;
    CHROMA: "enable_444", "Full chroma (4:4:4)", "display", true;
    TEN_BIT_SDR: "ten_bit_sdr", "10-bit SDR", "display", true;
    VSYNC: "vsync", "V-Sync", "display", true;
    VRR: "allow_vrr", "Follow variable refresh", "display", true;
    COMPOSITOR: "compositor", "Host compositor", "display", true;
    KEEP_AWAKE: "keep_awake", "Keep screen awake", "display", true;
    DECODER: "decoder", "Video decoder", "display", true;
    ADAPTER: "adapter", "GPU", "display", true;

    TOUCH: "touch_mode", "Touch input", "input", false;
    MOUSE: "mouse_mode", "Mouse input", "input", false;
    SHORTCUTS: "inhibit_shortcuts", "Capture system shortcuts", "input", false;
    INVERT_SCROLL: "invert_scroll", "Invert scroll direction", "input", false;
    QUICK_ACTIONS: "overlay_actions", "Quick actions", "quick_actions", false;

    AUDIO_CHANNELS: "audio_channels", "Audio channels", "audio", false;
    SPEAKER: "speaker_device", "Speaker", "audio", false;
    MIC: "mic_enabled", "Stream microphone", "audio", false;
    MIC_DEVICE: "mic_device", "Microphone", "audio", false;
    AUDIO_FORMAT: "audio_format", "Audio quality", "audio", true;
    KEEP_HOST_AUDIO: "keep_host_audio", "Keep host audio playing", "audio", true;
    ECHO_CANCEL: "echo_cancel", "Echo cancellation", "audio", true;

    GAMEPAD: "gamepad", "Controller type", "controllers", false;
    PAD_RUMBLE: "pad_rumble", "Controller rumble", "controllers", false;
    FORWARDING: "gamepad_forwarding", "Forward controllers", "controllers", true;
    FORWARD_PAD: "forward_pad", "Use controller", "controllers", true;
    SYSTEM_BUTTONS: "system_buttons", "Guide button", "controllers", true;
    GUIDE_GESTURE: "guide_gesture", "Hold Select for guide", "controllers", true;
    PAD_HAPTICS: "pad_haptics", "Controller haptics", "controllers", true;
    PAD_SPEAKER: "pad_speaker", "Controller speaker", "controllers", true;
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
        ("follow_os_theme", "the console's own look"),
        ("ui_palette", "the console's own look"),
        ("reduce_motion", "the console's own look"),
        ("reduce_ui_resolution", "a weak GPU's console"),
        ("library_view", "the console's library"),
        ("library_sections", "the console's library"),
        ("host_sort", "Sort on the Hosts header"),
        ("host_grouping", "Group on the Hosts header"),
        ("second_screen", "an Android handheld's second screen"),
        ("low_latency", "a MediaCodec decoder flag"),
        ("audio_route", "webOS's own audio plane"),
        ("cursor_gestures", "a TV remote's missing second button"),
        ("rumble_on_phone", "a phone's own hardware"),
        ("gyro_on_phone", "a phone's own hardware"),
        ("sc2_capture", "Apple only"),
        ("ds_capture", "Android and webOS only"),
        ("spatial_audio", "a Vision Pro's room"),
        ("screen", "a Vision Pro's room"),
    ];

    /// Rows with no catalog entry: this device's own pickers.
    const LOCAL: &[&str] = &["adapter", "speaker_device", "mic_device"];

    /// Rows on a sidebar page the catalog does not name, with that page: Quick actions has
    /// its own.
    const OWN_PAGE: &[(&str, &str)] = &[("overlay_actions", "quick_actions")];

    /// Each row that edits a shared setting carries the catalog's label, category and tier,
    /// save a page [`OWN_PAGE`] names, and every catalog entry has a row here or a reason it
    /// does not.
    #[test]
    fn rows_match_the_settings_catalog() {
        let raw = include_str!("../../../../shared/settings-catalog.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("the catalog parses");
        let entries = file["settings"].as_array().expect("settings");
        for row in ROWS {
            let Some(e) = entries.iter().find(|e| e["key"] == row.key) else {
                assert!(
                    LOCAL.contains(&row.key),
                    "{} is not in the catalog",
                    row.key
                );
                continue;
            };
            assert_eq!(row.label(), e["label"].as_str().unwrap(), "{}", row.key);
            let category = e["category"].as_str().unwrap();
            assert_eq!(
                row.page != category,
                OWN_PAGE.contains(&(row.key, row.page)),
                "{}: page {}, the catalog's {category}",
                row.key,
                row.page
            );
            assert_eq!(
                row.advanced,
                e["advanced"].as_bool().unwrap(),
                "{}",
                row.key
            );
        }
        for e in entries {
            let key = e["key"].as_str().unwrap();
            let bound = ROWS.iter().any(|r| r.key == key);
            let absent = ABSENT.iter().any(|(k, _)| *k == key);
            assert!(
                bound != absent,
                "{key}: a row or a reason, not both or neither"
            );
        }
    }
}
