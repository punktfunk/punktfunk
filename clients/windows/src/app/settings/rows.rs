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
    FULLSCREEN_ON_STREAM: "fullscreen_on_stream", pf_l10n::client::player_fullscreen_on_stream(), "general", false;
    AUTO_WAKE: "auto_wake", pf_l10n::client::player_auto_wake(), "general", false;
    START_IN: "start_in", pf_l10n::client::player_start_in(), "general", false;
    GAMEPAD_UI: "gamepad_ui_enabled", pf_l10n::client::player_gamepad_ui_enabled(), "general", false;
    GAMEPAD_UI_MODE: "gamepad_ui_mode", pf_l10n::client::player_gamepad_ui_mode(), "general", false;
    STATS: "stats_verbosity", pf_l10n::client::player_stats_verbosity(), "general", false;
    SHOW_ADVANCED: "show_advanced", pf_l10n::client::player_show_advanced(), "general", false;
    ADVANCED_STATS: "advanced_stats", pf_l10n::client::player_advanced_stats(), "general", true;
    STATS_POSITION: "hud_placement", pf_l10n::client::player_hud_placement(), "general", true;
    STATS_SIZE: "stats_scale_pct", pf_l10n::client::player_stats_scale_pct(), "general", true;
    EXIT_HINT: "exit_hint", pf_l10n::client::player_exit_hint(), "general", true;

    RESOLUTION: "resolution", pf_l10n::client::player_resolution(), "display", false;
    REFRESH: "refresh_hz", pf_l10n::client::player_refresh_hz(), "display", false;
    BITRATE: "bitrate_kbps", pf_l10n::client::player_bitrate_kbps(), "display", false;
    VIDEO_FIT: "video_fit", pf_l10n::client::player_video_fit(), "display", false;
    SCREENS: "screens", pf_l10n::client::player_screens(), "display", false;
    HDR: "hdr_enabled", pf_l10n::client::player_hdr_enabled(), "display", false;
    PRESENT: "present_priority", pf_l10n::client::player_present_priority(), "display", false;
    SMOOTH_BUFFER: "smooth_buffer", pf_l10n::client::player_smooth_buffer(), "display", true;
    RENDER_SCALE: "render_scale", pf_l10n::client::player_render_scale(), "display", true;
    CODEC: "codec", pf_l10n::client::player_codec(), "display", false;
    PYROWAVE_QUALITY: "pyrowave_bpp", pf_l10n::client::player_pyrowave_bpp(), "display", false;
    CHROMA: "enable_444", pf_l10n::client::player_enable_444(), "display", true;
    TEN_BIT_SDR: "ten_bit_sdr", pf_l10n::client::player_ten_bit_sdr(), "display", true;
    VSYNC: "vsync", "V-Sync", "display", true;
    VRR: "allow_vrr", pf_l10n::client::player_allow_vrr(), "display", true;
    COMPOSITOR: "compositor", pf_l10n::client::player_compositor(), "display", true;
    KEEP_AWAKE: "keep_awake", pf_l10n::client::player_keep_awake(), "display", true;
    DECODER: "decoder", pf_l10n::client::player_decoder(), "display", true;
    ADAPTER: "adapter", pf_l10n::desktop::desktop_adapter(), "display", true;

    TOUCH: "touch_mode", pf_l10n::client::player_touch_mode(), "input", false;
    MOUSE: "mouse_mode", pf_l10n::client::player_mouse_mode(), "input", false;
    SHORTCUTS: "inhibit_shortcuts", pf_l10n::client::player_inhibit_shortcuts(), "input", false;
    INVERT_SCROLL: "invert_scroll", pf_l10n::client::player_invert_scroll(), "input", false;
    QUICK_ACTIONS: "overlay_actions", pf_l10n::client::player_overlay_actions(), "quick_actions", false;

    AUDIO_CHANNELS: "audio_channels", pf_l10n::client::player_audio_channels(), "audio", false;
    SPEAKER: "speaker_device", pf_l10n::client::player_speaker_device(), "audio", false;
    MIC: "mic_enabled", pf_l10n::client::player_mic_enabled(), "audio", false;
    MIC_DEVICE: "mic_device", pf_l10n::client::player_mic_device(), "audio", false;
    AUDIO_FORMAT: "audio_format", pf_l10n::client::player_audio_format(), "audio", true;
    KEEP_HOST_AUDIO: "keep_host_audio", pf_l10n::client::player_keep_host_audio(), "audio", true;
    ECHO_CANCEL: "echo_cancel", pf_l10n::client::player_echo_cancel(), "audio", true;

    GAMEPAD: "gamepad", pf_l10n::client::player_gamepad(), "controllers", false;
    PAD_RUMBLE: "pad_rumble", pf_l10n::client::player_pad_rumble(), "controllers", false;
    FORWARDING: "gamepad_forwarding", pf_l10n::client::player_gamepad_forwarding(), "controllers", true;
    FORWARD_PAD: "forward_pad", pf_l10n::client::player_forward_pad(), "controllers", true;
    SYSTEM_BUTTONS: "system_buttons", pf_l10n::client::player_system_buttons(), "controllers", true;
    GUIDE_GESTURE: "guide_gesture", pf_l10n::client::player_guide_gesture(), "controllers", true;
    PAD_HAPTICS: "pad_haptics", pf_l10n::client::player_pad_haptics(), "controllers", true;
    PAD_SPEAKER: "pad_speaker", pf_l10n::client::player_pad_speaker(), "controllers", true;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catalog entries naming `desktop` that this client has no row for, and why: the console
    /// the desktop session draws has them.
    const CONSOLE_ONLY: &[(&str, &str)] = &[
        ("follow_os_theme", "the console's own look"),
        ("ui_palette", "the console's own look"),
        ("reduce_motion", "the console's own look"),
        ("library_view", "the console's library"),
        ("library_sections", "the console's library"),
        ("host_sort", "Sort on the Hosts header"),
        ("host_grouping", "Group on the Hosts header"),
    ];

    /// Rows with no catalog entry: this device's own pickers.
    const LOCAL: &[&str] = &["adapter", "speaker_device", "mic_device"];

    /// Rows on a sidebar page the catalog does not name, with that page: Quick actions has
    /// its own.
    const OWN_PAGE: &[(&str, &str)] = &[("overlay_actions", "quick_actions")];

    /// Each row that edits a shared setting is the catalog's: the `player_<key>` message for
    /// its label (V-Sync, a brand, has none), its category and tier, save a page [`OWN_PAGE`]
    /// names. A catalog entry naming `desktop` has a row here or a reason it does not, and an
    /// entry that leaves `desktop` out has no row.
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
            match pf_l10n::get(&format!("player_{}", row.key)) {
                Some(m) => assert_eq!(row.label(), m.text(), "{}", row.key),
                None => assert_eq!((row.key, row.label()), ("vsync", "V-Sync")),
            }
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
            assert!(
                e["on"].as_array().unwrap().iter().any(|p| p == "desktop"),
                "{} has a row here, and the catalog leaves desktop out",
                row.key
            );
        }
        for e in entries {
            let key = e["key"].as_str().unwrap();
            let desktop = e["on"].as_array().unwrap().iter().any(|p| p == "desktop");
            let bound = ROWS.iter().any(|r| r.key == key);
            let console_only = CONSOLE_ONLY.iter().any(|(k, _)| *k == key);
            assert_eq!(
                bound,
                desktop && !console_only,
                "{key}: a row, or a reason, exactly when the catalog names desktop"
            );
        }
    }

    /// The pages' tables, by the catalog key they hold: the page's source and the const.
    const TABLES: &[(&str, &str, &str)] = &[
        ("refresh_hz", include_str!("display.rs"), "REFRESH"),
        ("decoder", include_str!("display.rs"), "DECODERS"),
        ("codec", include_str!("display.rs"), "CODECS"),
        ("video_fit", include_str!("display.rs"), "VIDEO_FITS"),
        ("screens", include_str!("display.rs"), "SCREENS"),
        (
            "present_priority",
            include_str!("display.rs"),
            "PRESENT_PRIORITIES",
        ),
        (
            "smooth_buffer",
            include_str!("display.rs"),
            "SMOOTH_BUFFERS",
        ),
        ("compositor", include_str!("display.rs"), "COMPOSITORS"),
        ("gamepad", include_str!("controllers.rs"), "GAMEPADS"),
        (
            "system_buttons",
            include_str!("controllers.rs"),
            "SYSTEM_BUTTONS",
        ),
        (
            "guide_gesture",
            include_str!("controllers.rs"),
            "GUIDE_GESTURES",
        ),
        ("audio_channels", include_str!("audio.rs"), "AUDIO_CHANNELS"),
    ];

    /// Lists the pages draw from the shared core (`StartIn`, `StatsVerbosity`, `HudCorner`,
    /// `TouchMode`, the render-scale stops, the audio formats) or build inline: the console
    /// and the Linux dialog pin the core's, so there is no table here to read.
    const NOT_HERE: &[&str] = &[
        "start_in",
        "stats_verbosity",
        "hud_placement",
        "stats_scale_pct",
        "render_scale",
        "touch_mode",
        "mouse_mode",
        "audio_format",
        "gamepad_ui_mode",
    ];

    /// Pickers that list fewer options than the catalog, the ones they list in its order.
    const SUBSET: &[(&str, &[&str])] = &[(
        "decoder",
        &["auto", "native-vulkan", "native-d3d11va", "software"],
    )];

    /// Pickers that list the catalog's options in another order: open rows in
    /// docs/settings.md.
    const OTHER_ORDER: &[(&str, &[&str])] = &[(
        "gamepad",
        &[
            "auto",
            "xbox360",
            "dualsense",
            "xboxone",
            "dualshock4",
            "steamdeck",
            "steamcontroller2",
        ],
    )];

    /// The stored values of `const name` in `source`: the first literal of each tuple, or the
    /// numbers of a plain array.
    fn stored(source: &str, name: &str) -> Vec<String> {
        let at = source
            .find(&format!("const {name}:"))
            .unwrap_or_else(|| panic!("no const {name}"));
        let body = &source[at..];
        let body = &body[body.find("= ").unwrap() + 2..];
        let body = &body[..body.find("];").unwrap()];
        if body.contains('(') {
            body.lines()
                .filter_map(|l| l.trim().strip_prefix('('))
                .map(|l| {
                    let first = l.split(',').next().unwrap().trim();
                    first.trim_matches('"').to_string()
                })
                .collect()
        } else {
            let list = &body[body.find('[').unwrap() + 1..];
            list.split(',').map(|n| n.trim().to_string()).collect()
        }
    }

    /// Each picker the pages build from a table of their own lists the catalog's options in
    /// its order, or the subset or other order this client names.
    #[test]
    fn options_match_the_settings_catalog() {
        let raw = include_str!("../../../../shared/settings-catalog.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("the catalog parses");
        let entries = file["settings"].as_array().expect("settings");
        for (key, source, name) in TABLES {
            let e = entries.iter().find(|e| e["key"] == *key).expect(key);
            let want: Vec<String> = e["options"]
                .as_array()
                .unwrap_or_else(|| panic!("{key} has no options"))
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            let got = stored(source, name);
            let subset = SUBSET.iter().find(|(k, _)| k == key);
            let other = OTHER_ORDER.iter().find(|(k, _)| k == key);
            if let Some((_, listed)) = subset {
                assert_eq!(got, *listed, "{key}");
                let mut rest = want.iter();
                assert!(
                    listed.len() < want.len() && listed.iter().all(|s| rest.any(|w| w == s)),
                    "{key}: the pages' options are not the catalog's, in order"
                );
            } else if let Some((_, listed)) = other {
                assert_eq!(got, *listed, "{key}");
                let (mut a, mut b) = (got.clone(), want.clone());
                a.sort();
                b.sort();
                assert!(a == b && got != want, "{key}: not the catalog's, reordered");
            } else {
                assert_eq!(got, want, "{key}: options");
            }
        }
        for e in entries {
            let key = e["key"].as_str().unwrap();
            let desktop = e["on"].as_array().unwrap().iter().any(|p| p == "desktop");
            let list = e.get("options").is_some();
            if desktop && list && ROWS.iter().any(|r| r.key == key) {
                let read = TABLES.iter().any(|(k, _, _)| *k == key);
                assert!(
                    read != NOT_HERE.contains(&key),
                    "{key}: a table to read, or a reason there is none"
                );
            }
        }
    }
}
