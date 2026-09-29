//! Every setting row the dialog shows, as data: the key it stores, its label and caption, its
//! page, whether it waits behind Show advanced, and which layer it edits. Rows take their text
//! from here, and the catalog test holds this table to `clients/shared/settings-catalog.json`.

use super::tables::{
    resolution_caption, BITRATE_CAPTION, CODEC_CAPTION, MOUSE_MODE_CAPTIONS,
    PRESENT_PRIORITY_CAPTIONS, TOUCH_MODE_CAPTIONS, VIDEO_FIT_CAPTIONS,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    General,
    Display,
    Input,
    Audio,
    Controllers,
}

impl Page {
    pub fn title(self) -> &'static str {
        match self {
            Page::General => "General",
            Page::Display => "Display",
            Page::Input => "Input",
            Page::Audio => "Audio",
            Page::Controllers => "Controllers",
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
    pub title: &'static str,
    pub caption: &'static str,
    pub page: Page,
    pub advanced: bool,
    pub layer: Layer,
}

macro_rules! specs {
    ($($name:ident: $key:literal, $title:literal, $caption:expr, $page:ident, $advanced:literal, $layer:ident;)*) => {
        $(pub const $name: Spec = Spec {
            key: $key,
            title: $title,
            caption: $caption,
            page: Page::$page,
            advanced: $advanced,
            layer: Layer::$layer,
        };)*
        #[cfg(test)]
        pub const SPECS: &[Spec] = &[$($name),*];
    };
}

specs! {
    FULLSCREEN: "fullscreen", "Fullscreen",
        "Always keeps this window fullscreen too. F11 leads back out", General, false, Device;
    FULLSCREEN_ON_STREAM: "fullscreen_on_stream", "Start streams fullscreen",
        "F11, the mouse at the top edge, or L1+R1+Start+Select lead back out",
        General, false, PresetOnly;
    AUTO_WAKE: "auto_wake", "Auto-wake on connect",
        "Sends Wake-on-LAN to an offline saved host and waits for it to boot \u{2014} turn off if \
         hosts behind a VPN look offline when they aren't", General, false, Device;
    START_IN: "start_in", "Start in", "", General, false, Device;
    GAMEPAD_UI: "gamepad_ui_enabled", "Controller-optimized UI",
        "Opens the console, the interface built for a controller", General, false, Device;
    GAMEPAD_UI_MODE: "gamepad_ui_mode", "Show it",
        "Always opens it at launch. Otherwise it opens when a controller connects, and this \
         window returns when the last one disconnects", General, false, Device;
    FOLLOW_OS_THEME: "follow_os_theme", "Follow system theme",
        "Colours track omarchy-theme-set live \u{2014} off keeps Punktfunk's own look",
        General, false, Device;
    OMARCHY_MENU: "omarchy_menu", "Hosts in the Omarchy menu",
        "Super+Space: connect, wake, the console \u{2014} this writes rows to omarchy-menu.jsonc",
        General, false, Device;
    STATS: "stats_verbosity", "Statistics overlay",
        "Compact = fps \u{b7} latency \u{b7} bitrate in one line \u{2014} Ctrl+Alt+Shift+S cycles \
         the tiers live", General, false, Stream;
    SHOW_ADVANCED: "show_advanced", "Show advanced",
        "Adds the settings most players never need to change", General, false, Dialog;
    ADVANCED_STATS: "advanced_stats", "Advanced statistics",
        "Off shows the figures Moonlight's overlay also shows. On shows capture to glass as \
         p50/p95 and every stage between", General, true, Device;
    STATS_POSITION: "hud_placement", "Statistics position",
        "The corner the statistics overlay sits in", General, true, Device;
    STATS_SIZE: "stats_scale_pct", "Statistics size",
        "The overlay's size, on top of your display's scaling", General, true, Device;
    EXIT_HINT: "exit_hint", "Exit hint",
        "Shows how to leave for a few seconds when a stream starts", General, true, Device;

    RESOLUTION: "resolution", "Resolution", resolution_caption(0), Display, false, Stream;
    REFRESH: "refresh_hz", "Refresh rate", "Native follows the monitor the window is on",
        Display, false, Stream;
    BITRATE: "bitrate_kbps", "Bitrate", BITRATE_CAPTION, Display, false, Stream;
    VIDEO_FIT: "video_fit", "Picture fit", VIDEO_FIT_CAPTIONS[0], Display, false, Stream;
    HDR: "hdr_enabled", "10-bit HDR",
        "Advertise 10-bit HDR10 so the host upgrades HDR content \u{2014} shown in HDR where the \
         display supports it, tone-mapped otherwise", Display, false, Stream;
    PRESENT: "present_priority", "Prioritize", PRESENT_PRIORITY_CAPTIONS[0],
        Display, false, Stream;
    SMOOTH_BUFFER: "smooth_buffer", "Smoothness buffer",
        "Each frame held absorbs one refresh of hiccup and adds one of delay",
        Display, true, Stream;
    RENDER_SCALE: "render_scale", "Render scale",
        "Above 1\u{d7} supersamples for sharpness; below is lighter on the host",
        Display, true, Stream;
    CODEC: "codec", "Video codec", CODEC_CAPTION, Display, true, Stream;
    CHROMA: "enable_444", "Full chroma (4:4:4)",
        "Full-colour video: crisp small text and thin lines, at more bandwidth. HEVC only, and \
         only where the host can encode it.", Display, true, Stream;
    TEN_BIT_SDR: "ten_bit_sdr", "10-bit SDR",
        "Smoother gradients without HDR \u{2014} 10-bit encoding precision. Needs an NVIDIA \
         host; HDR takes over when it engages.", Display, true, Stream;
    VSYNC: "vsync", "V-Sync",
        "Tear-free. Turning it off removes the wait for the screen's refresh \u{2014} the lowest \
         possible delay, at the cost of visible tearing. Not every driver offers it; the stats \
         overlay names the mode actually in use", Display, true, Stream;
    VRR: "allow_vrr", "Follow variable refresh",
        "On a VRR/FreeSync/G-Sync screen, let the panel refresh in step with the stream instead \
         of on a fixed cadence. Applies to fullscreen sessions; harmless on a fixed-refresh \
         screen", Display, true, Stream;
    COMPOSITOR: "compositor", "Host compositor",
        "Advisory \u{2014} the host falls back to auto-detect when unavailable",
        Display, true, Stream;
    DECODER: "decoder", "Video decoder",
        "Automatic picks the best hardware decode, then software", Display, true, Device;
    ADAPTER: "adapter", "GPU", "Decodes and presents the stream", Display, true, Device;

    TOUCH: "touch_mode", "Touch input", TOUCH_MODE_CAPTIONS[0], Input, false, Stream;
    MOUSE: "mouse_mode", "Mouse input", MOUSE_MODE_CAPTIONS[0], Input, false, Stream;
    SHORTCUTS: "inhibit_shortcuts", "Capture system shortcuts",
        "Forward Alt+Tab, Super, \u{2026} to the host while input is captured",
        Input, false, Stream;
    INVERT_SCROLL: "invert_scroll", "Invert scroll direction",
        "Reverses the wheel and trackpad scroll direction sent to the host",
        Input, false, Stream;
    QUICK_ACTIONS: "overlay_actions", "Quick actions", "", Input, false, Stream;

    AUDIO_CHANNELS: "audio_channels", "Audio channels",
        "Stereo or surround \u{2014} the host downmixes if its output has fewer",
        Audio, false, Stream;
    SPEAKER: "speaker_device", "Speaker",
        "Host audio plays here \u{2014} System default follows the desktop", Audio, false, Device;
    MIC: "mic_enabled", "Stream microphone",
        "Sends your microphone to the host's virtual mic \u{2014} Ctrl+Alt+Shift+V mutes it \
         mid-stream", Audio, false, Stream;
    MIC_DEVICE: "mic_device", "Microphone", "The input that feeds the host's virtual mic",
        Audio, false, Device;
    AUDIO_FORMAT: "audio_format", "Audio quality",
        "Lossless is uncompressed PCM \u{2014} 2.3\u{2013}4.6 Mb/s off the top of the link, and \
         the host has its own switch", Audio, true, Stream;
    KEEP_HOST_AUDIO: "keep_host_audio", "Keep host audio playing",
        "The host's speakers or headphones keep playing while you stream \u{2014} needs a host \
         on 0.32+", Audio, true, Stream;
    ECHO_CANCEL: "echo_cancel", "Echo cancellation",
        "Keeps the host's audio, playing from this machine's speakers, out of the uplink",
        Audio, true, Stream;

    GAMEPAD: "gamepad", "Controller type",
        "The virtual pad on the host \u{2014} Automatic matches your controller. An X-Box type \
         has no gyroscope, so pick a DualSense-class one if you want motion.",
        Controllers, false, Stream;
    FORWARDING: "gamepad_forwarding", "Forward controllers",
        "Send this device's controllers to the host \u{2014} off if it already has them another \
         way", Controllers, true, Stream;
    FORWARD_PAD: "forward_pad", "Use controller",
        "Every pad is its own player \u{2014} pick one to force single-player",
        Controllers, true, Device;
    SYSTEM_BUTTONS: "system_buttons", "Guide button",
        "Automatic sends it to the host, except where this device reacts to it too",
        Controllers, true, Stream;
    GUIDE_GESTURE: "guide_gesture", "Hold Select for guide",
        "Hold Select alone for the host's guide button \u{2014} a tap still goes through",
        Controllers, true, Stream;
    PAD_HAPTICS: "pad_haptics", "Controller haptics",
        "Play a DualSense's voice-coil haptics on the pad itself \u{2014} wired pads only",
        Controllers, true, Device;
    PAD_SPEAKER: "pad_speaker", "Controller speaker",
        "Play the audio a game sends to the pad's own speaker on the pad, not here",
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
        ("audio_route", "webOS's own audio plane"),
        ("cursor_gestures", "a TV remote's missing second button"),
        ("rumble_on_phone", "a phone's own hardware"),
        ("gyro_on_phone", "a phone's own hardware"),
        ("sc2_capture", "Android and Apple only"),
        ("ds_capture", "Android and webOS only"),
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
            assert_eq!(spec.title, e["label"].as_str().unwrap(), "{}", spec.key);
            let page = spec.page.title().to_lowercase();
            assert_eq!(page, e["category"].as_str().unwrap(), "{}", spec.key);
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
