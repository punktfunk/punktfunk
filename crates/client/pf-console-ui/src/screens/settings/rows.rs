//! The settings rows' engine: ids, sections, platform gate, spec and step. The Settings
//! screen ([`super::SettingsScreen`]) and the preset editor lay these rows out;
//! [`crate::settings_rows`] re-exports them for a shell that lays them out its own way.

use crate::screens::{home, Ctx};
use crate::widgets::RowSpec;
use pf_client_core::audio_format::{AUDIO_FORMATS, AUDIO_FORMAT_OPUS};
use pf_client_core::display_targets::{host_screen_choices, HostScreenChoice, Targets};
use pf_client_core::presets::SettingsOverlay;
use pf_client_core::settings::Label;
use pf_client_core::start;
use pf_client_core::trust::{HudCorner, MouseMode, Settings, StatsVerbosity, TouchMode};
use pf_l10n::{client as m, common, console};
use std::borrow::Cow;

/// Dispatch key for adjust/activate. The pad list under "Use controller" can
/// churn between frames, so an index would act on the wrong row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowId {
    /// Index into [`SettingsScreen::presets`]. Activate opens the preset's menu.
    Preset(usize),
    /// Last on the Presets tab: name a new preset, then edit it.
    NewPreset,
    Resolution,
    /// The family of sizes the Resolution row steps through.
    Aspect,
    Refresh,
    RenderScale,
    /// `trust::Settings::video_fit`: bars, crop or stretch when the stream's shape differs.
    VideoFit,
    /// `trust::Settings::screens`: this monitor, or a host screen on each monitor.
    Screens,
    /// A preset's `host_screen`: the role or monitor its connects ask for. The preset editor's
    /// row alone; no tab lists it, as a host's default is on its card.
    HostScreen,
    /// How a Vision Pro bends the picture: flat, or a curved monitor's radius.
    /// Device-local, under [`device_keys::SCREEN`].
    Screen,
    Bitrate,
    /// `trust::Settings::pyrowave_bpp`, shown as the rate it needs. In Bitrate's place while
    /// the codec is PyroWave.
    PyroWaveQuality,
    Compositor,
    Codec,
    Decoder,
    Hdr,
    Chroma444,
    /// `VIDEO_CAP_10BIT` without HDR. Desktop-only: Android takes depth from the panel.
    TenBitSdr,
    PresentPriority,
    SmoothBuffer,
    Vsync,
    AllowVrr,
    Audio,
    /// Cross-client `audio_format` key. Gated on stereo — see [`row_spec`].
    AudioFormat,
    /// `CLIENT_CAP_KEEP_HOST_AUDIO`. Advertised on every platform, including Android.
    KeepHostAudio,
    Mic,
    EchoCancel,
    /// Off, a Vision Pro plays the game's mix straight to the ears instead of from the
    /// window. Device-local, under [`device_keys::SPATIAL_AUDIO`].
    SpatialAudio,
    PadForward,
    Pad,
    PadType,
    /// `trust::Settings::pad_rumble`. Off, the client drops the host's rumble and the
    /// console's own pulses.
    PadRumble,
    SystemButtons,
    /// `trust::Settings::pad_haptics`. Negotiated: needs a capable host and a wired DS5.
    PadHaptics,
    /// `trust::Settings::pad_speaker`. On (`"pad"`) / Off only — stored `"mix"`
    /// renders as off, so offering it would be a no-op control.
    PadSpeaker,
    Touch,
    Mouse,
    InvertScroll,
    /// `trust::Settings::inhibit_shortcuts`: the system's own key chords reach the host.
    InhibitShortcuts,
    /// `trust::Settings::overlay_actions`. Action row: opens [`super::ring_editor::RingEditorScreen`].
    QuickActions,
    /// `trust::Settings::shortcuts`. Action row: opens [`super::shortcuts::ShortcutsScreen`].
    ShortcutsPage,
    Stats,
    /// `trust::Settings::advanced_stats`: which vocabulary the overlay speaks. Device-wide.
    AdvancedStats,
    Fullscreen,
    /// The Mac's three-way picker over `fullscreen_on_stream` and [`FULLSCREEN_ALWAYS_KEY`]. The
    /// tab shows it in place of [`RowId::Fullscreen`]; a preset still edits that toggle.
    FullscreenMode,
    AutoWake,
    /// `trust::Settings::follow_os_theme`. Shown only while the embedder publishes
    /// a theme; [`RowId::Palette`] hides while it is on.
    FollowOsTheme,
    Palette,
    ReduceMotion,
    /// The TV clients' low-cost interface mode: Android caps its surface at 1080p,
    /// both draw the backdrop small and slow. The TVs default it on.
    ReduceUiResolution,
    /// Same `library_view` key the library bar writes.
    LibraryView,
    /// `trust::Settings::start_in`. The value line names where a launch will land.
    StartIn,
    // Android-only. Values live in `trust::Settings::extra` under `android.*`
    // so the typed struct stays shared; [`row_on`] keeps them off desktop.
    /// Slice-progressive decode plus DSCP. Android-only.
    LowLatency,
    /// `trust::Settings::keep_awake`. Off, the stream holds the screen awake only while the
    /// player's input is recent.
    KeepAwake,
    /// A dual-screen handheld's lower panel carries the companion panel or the picture;
    /// off leaves a second screen to the system. Android-only.
    SecondScreen,
    PhoneRumble,
    PhoneGyro,
    /// Raw BLE/USB capture instead of the OS pad.
    Sc2Passthrough,
    /// Raw USB: touchpad, motion, adaptive triggers.
    DsCapture,
    /// `Settings.gamepadUiEnabled`. Only with [`Ctx::fallback_ui`] — otherwise
    /// off strands the user with no UI.
    GamepadUi,
    GamepadUiMode,
    /// A session stays up while the app is in the background. Phones, tablets and TVs.
    BackgroundKeepAlive,
    /// How long a backgrounded session lasts. Under [`RowId::BackgroundKeepAlive`].
    BackgroundTimeout,
    /// The statistics overlay's corner. Apple draws that overlay itself.
    StatsPosition,
    /// The host row's order ([`super::home::arrange`]).
    HostSort,
    /// Bands in the host row: none, by preset, or by online status.
    HostGrouping,
    /// Which path decodes the stream's audio on a TV — see `WEBOS_AUDIO_ROUTES`. webOS only:
    /// the offload route is that client's NDL audio plane, which no other platform has.
    AudioRoute,
    /// Long-press the remote's OK to send a right click. webOS only — it exists because a
    /// Magic Remote has no second button.
    CursorGestures,
    /// Action row: jumps to the Controllers tab.
    Controllers,
    /// Action row: asks the host to open the platform licences screen.
    Licenses,
    /// Action row: the Games tab's sections, in [`super::library::CustomizeScreen`].
    LibrarySections,
    /// This build's version. Nothing to change.
    Version,
    /// `trust::Settings::show_advanced`: whether the tabs list their [`advanced`] rows.
    ShowAdvanced,
    /// `trust::Settings::stats_scale_pct`: the statistics panel's size.
    StatsSize,
    /// `trust::Settings::exit_hint`: the one-line exit hint at stream start.
    ExitHint,
    /// Action row, last on a tab while its advanced rows are hidden and some differ from a fresh
    /// install: says how many, and shows them.
    AdvancedChanged,
    /// Action row, last on General: every setting back to a fresh install's, asked first.
    ResetSettings,
}

/// Rows the tabs list only under Show advanced: their default is right for nearly everyone,
/// picking a value takes knowing how streaming works, and no first stream needs them.
pub fn advanced(id: RowId) -> bool {
    matches!(
        id,
        RowId::SmoothBuffer
            | RowId::RenderScale
            | RowId::Chroma444
            | RowId::TenBitSdr
            | RowId::Vsync
            | RowId::AllowVrr
            | RowId::Compositor
            | RowId::Decoder
            | RowId::LowLatency
            | RowId::KeepAwake
            | RowId::AudioFormat
            | RowId::KeepHostAudio
            | RowId::EchoCancel
            | RowId::AudioRoute
            | RowId::PadForward
            | RowId::Pad
            | RowId::SystemButtons
            | RowId::PadHaptics
            | RowId::PadSpeaker
            | RowId::Sc2Passthrough
            | RowId::DsCapture
            | RowId::AdvancedStats
            | RowId::StatsPosition
            | RowId::StatsSize
            | RowId::ExitHint
            | RowId::ReduceUiResolution
    )
}

/// `Settings::extra` keys for the rows about the device in your hand, not the host. The
/// `android.` prefix is where they were first written and stays for the stores that hold it;
/// the Apple client reads the same keys from its own document.
pub(super) mod device_keys {
    pub const LOW_LATENCY: &str = "android.low_latency";
    pub const SECOND_SCREEN: &str = "android.second_screen";
    pub const PHONE_RUMBLE: &str = "android.rumble_on_phone";
    pub const PHONE_GYRO: &str = "android.gyro_on_phone";
    pub const SC2: &str = "android.sc2_capture";
    pub const DS_CAPTURE: &str = "android.ds_capture";
    pub const REDUCE_UI_RES: &str = "android.reduce_ui_resolution";
    /// With `width`/`height` 0: Native narrowed to clear the cutout. Kotlin's `-2`
    /// sentinel, which an unsigned size cannot carry.
    pub const SAFE_AREA_MODE: &str = "android.safe_area_mode";
    /// The Vision Pro's own rows, under the prefix that client writes.
    pub const SPATIAL_AUDIO: &str = "visionos.spatial_audio";
    pub const SCREEN: &str = "visionos.screen";
}

/// The `Settings::extra` keys the webOS rows share with that client (`services::store::shared`),
/// which namespaces everything only it models.
mod webos_keys {
    pub const AUDIO_ROUTE: &str = "webos.audio_route";
    pub const CURSOR_GESTURES: &str = "webos.cursor_gestures";
    pub const REDUCE_UI_RES: &str = "webos.reduce_ui_resolution";
}

/// Stored `webos.audio_route` values, spelled as that client's enum serializes.
const WEBOS_AUDIO_ROUTES: [(&str, Label); 2] = [
    ("software", m::player_audio_route_software),
    ("ndlopus", m::player_audio_route_ndlopus),
];

/// The console-vs-fallback pair, unprefixed: webOS carries the same two keys (its
/// fallback is the cursor UI, not a touch home), so they name a concept rather than
/// a platform. Kotlin reads and writes them under these names too.
pub(super) const GAMEPAD_UI_KEY: &str = "gamepad_ui_enabled";
pub(super) const GAMEPAD_UI_MODE_KEY: &str = "gamepad_ui_mode";

/// The Vision Pro's `visionos.screen` values, as its own Settings names them.
const SCREENS: [(&str, Label); 8] = [
    ("flat", m::player_screen_flat),
    ("4000r", || "4000R"),
    ("3000r", || "3000R"),
    ("2300r", || "2300R"),
    ("1800r", || "1800R"),
    ("1500r", || "1500R"),
    ("1000r", || "1000R"),
    ("800r", || "800R"),
];
/// Stored [`GAMEPAD_UI_MODE_KEY`] values (`GamepadUi.kt`).
const GAMEPAD_UI_MODES: [(&str, Label); 2] = [
    ("connected", m::player_gamepad_ui_mode_connected),
    ("always", m::player_gamepad_ui_mode_always),
];

/// Apple's `fullscreenAlways`: the Mac window opens fullscreen and stays so between streams.
/// Device-only, so no preset carries it.
pub(super) const FULLSCREEN_ALWAYS_KEY: &str = "fullscreen_always";
const FULLSCREEN_MODES: [Label; 3] = [
    m::player_fullscreen_off,
    m::player_fullscreen_stream,
    m::player_fullscreen_always,
];

/// Index into [`FULLSCREEN_MODES`].
pub(super) fn fullscreen_mode(s: &pf_client_core::trust::Settings) -> usize {
    if extra_bool(s, FULLSCREEN_ALWAYS_KEY, false) {
        2
    } else {
        usize::from(s.fullscreen_on_stream)
    }
}

/// The background-session pair, the names Android's and Apple's stores share.
const BACKGROUND_KEEP_ALIVE_KEY: &str = "background_keep_alive";
const BACKGROUND_TIMEOUT_KEY: &str = "background_timeout_minutes";
/// Minutes, as the two touch UIs offer them. Stored as a number.
const BACKGROUND_TIMEOUTS: [u64; 4] = [1, 5, 10, 30];
const BACKGROUND_TIMEOUT_DEFAULT: u64 = 10;

/// The corner each platform's stats overlay sits in until the player picks one.
pub(crate) fn own_stats_corner(platform: crate::platform::Platform) -> HudCorner {
    use crate::platform::Platform;
    match platform {
        Platform::Desktop | Platform::Android | Platform::Ps5 | Platform::Switch => {
            HudCorner::TopLeft
        }
        Platform::Apple | Platform::WebOS | Platform::Web | Platform::Tizen => HudCorner::TopRight,
    }
}

/// `"pad"` is the only live value; `"mix"` renders as off. Local copy because
/// `pad_audio` is `cfg(linux|windows)` and Android still sends the setting.
fn pad_speaker_on(mode: &str) -> bool {
    mode == "pad"
}

pub(super) fn extra_bool(s: &pf_client_core::trust::Settings, key: &str, default: bool) -> bool {
    s.extra
        .get(key)
        .and_then(|v| v.as_bool())
        .unwrap_or(default)
}

pub(super) fn set_extra_bool(s: &mut pf_client_core::trust::Settings, key: &str, value: bool) {
    s.extra
        .insert(key.to_string(), serde_json::Value::Bool(value));
}

pub(super) fn extra_str<'a>(
    s: &'a pf_client_core::trust::Settings,
    key: &str,
    default: &'a str,
) -> &'a str {
    s.extra.get(key).and_then(|v| v.as_str()).unwrap_or(default)
}

/// Apple's store may still hold `profile`, the old name for grouping by preset.
pub(super) fn host_grouping(s: &pf_client_core::trust::Settings) -> &str {
    match extra_str(s, home::HOST_GROUPING_KEY, "none") {
        "profile" => "preset",
        g => g,
    }
}

pub(super) fn background_timeout(s: &pf_client_core::trust::Settings) -> u64 {
    (s.extra.get(BACKGROUND_TIMEOUT_KEY))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(BACKGROUND_TIMEOUT_DEFAULT)
}

fn toggle_extra(
    s: &mut pf_client_core::trust::Settings,
    key: &str,
    default: bool,
    delta: i32,
    wrap: bool,
) -> Option<()> {
    let mut v = extra_bool(s, key, default);
    toggle(&mut v, delta, wrap)?;
    set_extra_bool(s, key, v);
    Some(())
}

/// A row whose whole value is one stored entry: a `Settings::extra` key with what an unwritten
/// key reads as (and for a choice the values it steps through), or a typed bool field.
/// Reading and stepping share the entry.
#[derive(Clone, Copy)]
pub(super) enum Extra {
    Bool(&'static str, bool),
    Choice(&'static str, &'static str, &'static [(&'static str, Label)]),
    /// A typed bool: read through the first fn, stepped through the second, one field.
    Flag(fn(&Settings) -> bool, fn(&mut Settings) -> &mut bool),
}

impl Extra {
    fn label(self, s: &pf_client_core::trust::Settings) -> String {
        match self {
            Extra::Bool(key, default) => on_off(extra_bool(s, key, default)).into(),
            Extra::Flag(get, _) => on_off(get(s)).into(),
            Extra::Choice(key, default, options) => {
                label_for(options, extra_str(s, key, default)).into()
            }
        }
    }

    fn step(self, s: &mut pf_client_core::trust::Settings, delta: i32, wrap: bool) -> Option<()> {
        match self {
            Extra::Bool(key, default) => toggle_extra(s, key, default, delta, wrap),
            Extra::Flag(_, field) => toggle(field(s), delta, wrap),
            Extra::Choice(key, default, options) => {
                let mut v = extra_str(s, key, default).to_string();
                step_str(options, &mut v, delta, wrap).map(|()| {
                    s.extra
                        .insert(key.to_string(), serde_json::Value::String(v));
                })
            }
        }
    }
}

impl RowId {
    /// The one stored entry behind this row, when that entry is the row's whole value.
    pub(super) fn extra(self) -> Option<Extra> {
        Some(match self {
            RowId::Hdr => Extra::Flag(|s| s.hdr_enabled, |s| &mut s.hdr_enabled),
            RowId::Chroma444 => Extra::Flag(|s| s.enable_444, |s| &mut s.enable_444),
            RowId::TenBitSdr => Extra::Flag(|s| s.ten_bit_sdr, |s| &mut s.ten_bit_sdr),
            RowId::Vsync => Extra::Flag(|s| s.vsync, |s| &mut s.vsync),
            RowId::AllowVrr => Extra::Flag(|s| s.allow_vrr, |s| &mut s.allow_vrr),
            RowId::KeepHostAudio => Extra::Flag(|s| s.keep_host_audio, |s| &mut s.keep_host_audio),
            RowId::Mic => Extra::Flag(|s| s.mic_enabled, |s| &mut s.mic_enabled),
            RowId::EchoCancel => Extra::Flag(|s| s.echo_cancel, |s| &mut s.echo_cancel),
            RowId::PadForward => {
                Extra::Flag(|s| s.gamepad_forwarding, |s| &mut s.gamepad_forwarding)
            }
            RowId::PadRumble => Extra::Flag(|s| s.pad_rumble, |s| &mut s.pad_rumble),
            RowId::PadHaptics => Extra::Flag(|s| s.pad_haptics, |s| &mut s.pad_haptics),
            RowId::InvertScroll => Extra::Flag(|s| s.invert_scroll, |s| &mut s.invert_scroll),
            RowId::InhibitShortcuts => {
                Extra::Flag(|s| s.inhibit_shortcuts, |s| &mut s.inhibit_shortcuts)
            }
            RowId::AdvancedStats => Extra::Flag(|s| s.advanced_stats, |s| &mut s.advanced_stats),
            RowId::FollowOsTheme => Extra::Flag(|s| s.follow_os_theme, |s| &mut s.follow_os_theme),
            RowId::ReduceMotion => Extra::Flag(|s| s.reduce_motion, |s| &mut s.reduce_motion),
            RowId::Fullscreen => {
                Extra::Flag(|s| s.fullscreen_on_stream, |s| &mut s.fullscreen_on_stream)
            }
            RowId::AutoWake => Extra::Flag(|s| s.auto_wake, |s| &mut s.auto_wake),
            RowId::ExitHint => Extra::Flag(|s| s.exit_hint, |s| &mut s.exit_hint),
            RowId::KeepAwake => Extra::Flag(|s| s.keep_awake, |s| &mut s.keep_awake),
            RowId::ShowAdvanced => Extra::Flag(|s| s.show_advanced, |s| &mut s.show_advanced),
            RowId::LowLatency => Extra::Bool(device_keys::LOW_LATENCY, true),
            RowId::SecondScreen => Extra::Bool(device_keys::SECOND_SCREEN, true),
            RowId::PhoneRumble => Extra::Bool(device_keys::PHONE_RUMBLE, false),
            RowId::PhoneGyro => Extra::Bool(device_keys::PHONE_GYRO, false),
            RowId::Sc2Passthrough => Extra::Bool(device_keys::SC2, true),
            RowId::DsCapture => Extra::Bool(device_keys::DS_CAPTURE, true),
            RowId::SpatialAudio => Extra::Bool(device_keys::SPATIAL_AUDIO, true),
            RowId::CursorGestures => Extra::Bool(webos_keys::CURSOR_GESTURES, false),
            RowId::GamepadUi => Extra::Bool(GAMEPAD_UI_KEY, true),
            RowId::BackgroundKeepAlive => Extra::Bool(BACKGROUND_KEEP_ALIVE_KEY, false),
            RowId::AudioRoute => {
                Extra::Choice(webos_keys::AUDIO_ROUTE, "software", &WEBOS_AUDIO_ROUTES)
            }
            RowId::HostSort => Extra::Choice(home::HOST_SORT_KEY, "added", &home::HOST_SORTS),
            RowId::Screen => Extra::Choice(device_keys::SCREEN, "flat", &SCREENS),
            RowId::GamepadUiMode => {
                Extra::Choice(GAMEPAD_UI_MODE_KEY, "connected", &GAMEPAD_UI_MODES)
            }
            _ => return None,
        })
    }

    /// A switch row's stored value, its default when unwritten.
    fn extra_on(self, s: &pf_client_core::trust::Settings) -> bool {
        matches!(self.extra(), Some(Extra::Bool(key, default)) if extra_bool(s, key, default))
    }
}

/// The `extra` key behind [`RowId::ReduceUiResolution`] on this platform. Each TV client
/// persists its own — the names are what Kotlin (`ConsoleJson`) and the webOS store share.
fn reduce_ui_key(platform: crate::platform::Platform) -> &'static str {
    use crate::platform::Platform;
    match platform {
        Platform::WebOS => webos_keys::REDUCE_UI_RES,
        _ => device_keys::REDUCE_UI_RES,
    }
}

/// The value an unwritten [`reduce_ui_key`] resolves to: on for the TV shells. A webOS or
/// Samsung set always is one, and an Android host without the touch fallback is the box
/// plugged into a panel far faster than its GPU. Off elsewhere, where the GPU is not the
/// bottleneck.
fn reduce_ui_default(platform: crate::platform::Platform, fallback_ui: bool) -> bool {
    use crate::platform::Platform;
    matches!(platform, Platform::WebOS | Platform::Tizen)
        || (platform == Platform::Android && !fallback_ui)
}

/// The resolved "Reduce interface resolution" flag. The settings rows read it; the shell's
/// backdrop pass reads the same answer — one switch carries both savings.
pub(crate) fn reduce_ui_res(
    s: &pf_client_core::trust::Settings,
    platform: crate::platform::Platform,
    fallback_ui: bool,
) -> bool {
    extra_bool(
        s,
        reduce_ui_key(platform),
        reduce_ui_default(platform, fallback_ui),
    )
}

/// A section of the Settings tab. The first five are the catalogue's categories.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Section {
    General,
    Display,
    Audio,
    Input,
    Controllers,
    Presets,
    About,
}

impl Section {
    /// The section's name on its tab and over its rows.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Section::General => m::prefs_category_general(),
            Section::Display => m::prefs_category_display(),
            Section::Audio => m::prefs_category_audio(),
            Section::Input => m::prefs_category_input(),
            Section::Controllers => m::prefs_category_controllers(),
            Section::Presets => console::console_prefs_category_presets(),
            Section::About => m::prefs_category_about(),
        }
    }

    /// What a screen reader says when the section takes focus.
    pub(super) fn spoken(self) -> &'static str {
        match self {
            Section::General => console::console_prefs_category_general_a11y(),
            Section::Display => console::console_prefs_category_display_a11y(),
            Section::Audio => console::console_prefs_category_audio_a11y(),
            Section::Input => console::console_prefs_category_input_a11y(),
            Section::Controllers => console::console_prefs_category_controllers_a11y(),
            Section::Presets => console::console_prefs_category_presets_a11y(),
            Section::About => console::console_prefs_category_about_a11y(),
        }
    }
}

// The sections, the rows a player touches most first and the [`advanced`] ones last. A child
// row sits right under the switch it dims or drops with. Presets is empty here: its rows come
// from the catalog.
pub(super) const TABS: [(Section, &[RowId]); 7] = [
    (
        Section::General,
        &[
            RowId::StartIn,
            RowId::AutoWake,
            RowId::FullscreenMode,
            RowId::Fullscreen,
            RowId::BackgroundKeepAlive,
            RowId::BackgroundTimeout,
            RowId::Stats,
            RowId::GamepadUi,
            RowId::GamepadUiMode,
            RowId::FollowOsTheme,
            RowId::Palette,
            RowId::ReduceMotion,
            RowId::LibraryView,
            RowId::LibrarySections,
            RowId::HostSort,
            RowId::HostGrouping,
            RowId::ShowAdvanced,
            RowId::AdvancedStats,
            RowId::StatsPosition,
            RowId::StatsSize,
            RowId::ExitHint,
            RowId::ReduceUiResolution,
            RowId::ResetSettings,
        ],
    ),
    (
        Section::Display,
        &[
            RowId::Aspect,
            RowId::Resolution,
            RowId::Refresh,
            RowId::Codec,
            RowId::Bitrate,
            RowId::PyroWaveQuality,
            RowId::VideoFit,
            RowId::Screens,
            RowId::Screen,
            RowId::SecondScreen,
            RowId::Hdr,
            RowId::PresentPriority,
            RowId::SmoothBuffer,
            RowId::RenderScale,
            RowId::Chroma444,
            RowId::TenBitSdr,
            RowId::Vsync,
            RowId::AllowVrr,
            RowId::Compositor,
            RowId::Decoder,
            RowId::LowLatency,
            RowId::KeepAwake,
        ],
    ),
    (
        Section::Audio,
        &[
            RowId::Audio,
            RowId::Mic,
            RowId::SpatialAudio,
            RowId::AudioFormat,
            RowId::KeepHostAudio,
            RowId::EchoCancel,
            RowId::AudioRoute,
        ],
    ),
    (
        Section::Input,
        &[
            RowId::Touch,
            RowId::Mouse,
            RowId::InvertScroll,
            RowId::InhibitShortcuts,
            RowId::QuickActions,
            RowId::ShortcutsPage,
            RowId::CursorGestures,
        ],
    ),
    (
        Section::Controllers,
        &[
            RowId::Controllers,
            RowId::PadType,
            RowId::PadRumble,
            RowId::PhoneRumble,
            RowId::PhoneGyro,
            RowId::PadForward,
            RowId::Pad,
            RowId::SystemButtons,
            RowId::PadHaptics,
            RowId::PadSpeaker,
            RowId::Sc2Passthrough,
            RowId::DsCapture,
        ],
    ),
    (Section::Presets, &[]),
    (Section::About, &[RowId::Version, RowId::Licenses]),
];

/// The Presets section — catalog-built, not [`TABS`] rows.
pub(super) const PRESETS_TAB: usize = 5;

/// Strip length for the shell's raster walk. `cfg(test)`: a shipping build
/// would warn it dead, and this crate treats warnings as errors.
#[cfg(test)]
pub(crate) const TAB_COUNT: usize = TABS.len();

/// Sizes by family; the Resolution row lists one family at a time.
use punktfunk_core::resolutions::{family_of, nearest_in, Family, FamilyId, ASPECTS};

/// The Aspect row's entries: this device's screen and safe area first when no
/// standard shape has them, then the standard families.
pub(super) fn families(screen: Option<crate::shell::DeviceScreen>) -> Vec<Family> {
    punktfunk_core::resolutions::families(screen.map(|s| s.full), screen.map(|s| s.safe))
}

/// What the Aspect row shows for a family: the device's own two by name, a ratio as itself.
fn family_label(f: &Family) -> &'static str {
    match f.id {
        FamilyId::Screen => m::player_resolution_aspect_screen(),
        FamilyId::SafeArea => m::player_resolution_aspect_safe_area(),
        FamilyId::Aspect(i) => ASPECTS[i].label,
    }
}

/// The entry the Resolution row lists: the stored size's shape. Native (safe
/// area) and Native read as the device's own entries where it has them;
/// otherwise, and for a shape none has, the first entry.
pub(super) fn family(
    s: &pf_client_core::trust::Settings,
    fams: &[Family],
    platform: crate::platform::Platform,
) -> usize {
    let own = |id| fams.iter().position(|f| f.id == id);
    if s.width == 0 && !s.match_window {
        let native = if safe_area(s, platform) {
            own(FamilyId::SafeArea).or_else(|| own(FamilyId::Screen))
        } else {
            own(FamilyId::Screen)
        };
        return native.unwrap_or(0);
    }
    family_of(fams, s.width, s.height).unwrap_or(0)
}

/// What a Native stream asks for here. Unknown, a handheld's panel or 1920×1080 at 60 Hz,
/// as the desktop presenter assumes for a display that reports no mode.
fn native_mode(device: &crate::screens::Device) -> punktfunk_core::Mode {
    device.native_mode.unwrap_or_else(|| {
        let (width, height) = device.screen.map_or((1920, 1080), |s| s.full);
        punktfunk_core::Mode {
            width,
            height,
            refresh_hz: 60,
        }
    })
}

/// This device's link toward its default route, looked up at most once a second: the rows
/// are rebuilt every frame.
fn local_link() -> punktfunk_core::transport::LinkFacts {
    #[cfg(not(target_family = "wasm"))]
    {
        use std::time::{Duration, Instant};
        type Seen = Option<(Instant, punktfunk_core::transport::LinkFacts)>;
        static LAST: std::sync::Mutex<Seen> = std::sync::Mutex::new(None);
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        match *last {
            Some((at, facts)) if at.elapsed() < Duration::from_secs(1) => facts,
            _ => {
                let facts = punktfunk_core::transport::ifinfo::local_link_facts(None);
                *last = Some((Instant::now(), facts));
                facts
            }
        }
    }
    #[cfg(target_family = "wasm")]
    punktfunk_core::transport::LinkFacts::default()
}

/// A window the stream can follow (Match window): the desktops, the browser, and every Apple
/// device but the TV.
fn has_window(device: &crate::screens::Device) -> bool {
    use crate::platform::Platform;
    match device.platform {
        Platform::Desktop | Platform::Web | Platform::Tizen => true,
        Platform::Apple => !device.tv,
        Platform::Android | Platform::WebOS | Platform::Ps5 | Platform::Switch => false,
    }
}

/// A stored size the Resolution row's list does not hold: typed here or on another client.
fn custom_size(
    s: &pf_client_core::trust::Settings,
    fams: &[Family],
    platform: crate::platform::Platform,
) -> bool {
    s.width != 0
        && !s.match_window
        && !fams[family(s, fams, platform)]
            .sizes
            .contains(&(s.width, s.height))
}

/// Android's Native (safe area) resolution. The flag only counts on a native size.
fn safe_area(s: &pf_client_core::trust::Settings, platform: crate::platform::Platform) -> bool {
    platform == crate::platform::Platform::Android
        && s.width == 0
        && !s.match_window
        && extra_bool(s, device_keys::SAFE_AREA_MODE, false)
}
/// `0` = the panel's native refresh, resolved at connect. Must cover every value the desktop
/// shells can write: on Linux both write the same client-gtk-settings.json, so a box that set
/// 144 Hz there opens this screen holding it. A value missing from this table has no index, and
/// `step_option` answers a missing index with 0 — one nudge on the row would silently snap the
/// setting to Automatic. Keep in step with clients/linux/src/ui_settings.rs and
/// clients/windows/src/app/settings.rs.
pub(super) const REFRESH: [u32; 8] = [0, 30, 60, 90, 120, 144, 165, 240];
/// Render-scale multipliers; `1.0` = Native.
use punktfunk_core::render_scale::PRESETS as RENDER_SCALES;
use punktfunk_core::video_fit::VideoFit;
/// Left/right rungs in kbps: 5 Mbps apart to 50, 10 to 100, where most links settle;
/// ceiling 2 Gbps. Off-ladder values go through the typed field rather than a longer ladder.
pub(super) const BITRATES: [u32; 34] = [
    0, 1_000, 2_000, 3_000, 4_000, 5_000, 6_000, 8_000, 10_000, 12_000, 15_000, 20_000, 25_000,
    30_000, 35_000, 40_000, 45_000, 50_000, 60_000, 70_000, 80_000, 90_000, 100_000, 125_000,
    150_000, 200_000, 250_000, 300_000, 400_000, 500_000, 750_000, 1_000_000, 1_500_000, 2_000_000,
];
/// Typed-field ceiling in Mbps — the ladder's top. Host range is 500 kbps–8 Gbps.
const CUSTOM_MAX_MBPS: u32 = 2_000;
/// PyroWave quality's rungs, hundredths of a bit per pixel: `BPP_FLOOR` to `BPP_MAX` by 0.1.
const PYROWAVE_BPP_X100: [u16; 16] = [
    50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160, 170, 180, 190, 200,
];

/// The highest fixed bitrate this client may be set to, in kbps.
///
/// webOS is the one platform with a real ceiling: the TV client bounds its own slider at
/// 200 Mbps and clamps the document to it. Everywhere else the ladder's own top stands.
pub(crate) fn bitrate_ceiling_kbps(platform: crate::platform::Platform) -> u32 {
    match platform {
        crate::platform::Platform::WebOS => 200_000,
        crate::platform::Platform::Desktop
        | crate::platform::Platform::Android
        | crate::platform::Platform::Web
        | crate::platform::Platform::Apple
        | crate::platform::Platform::Tizen
        | crate::platform::Platform::Ps5
        | crate::platform::Platform::Switch => CUSTOM_MAX_MBPS * 1_000,
    }
}

/// How many ladder rungs this platform may reach — everything up to its ceiling.
fn bitrate_rungs(platform: crate::platform::Platform) -> usize {
    let ceiling = bitrate_ceiling_kbps(platform);
    BITRATES
        .iter()
        .position(|b| *b > ceiling)
        .unwrap_or(BITRATES.len())
}
const COMPOSITORS: [(&str, Label); 8] = [
    ("auto", m::player_compositor_auto),
    ("kwin", || "KWin (KDE Plasma)"),
    ("mutter", || "Mutter (GNOME)"),
    ("hyprland", || "Hyprland"),
    ("niri", || "niri"),
    ("umbriel", || "Umbriel"),
    ("wlroots", || "wlroots (Sway, River)"),
    ("gamescope", || "gamescope"),
];
const CODECS: [(&str, Label); 5] = [
    ("auto", m::player_codec_auto),
    ("hevc", || "HEVC"),
    ("h264", || "H.264"),
    ("av1", || "AV1"),
    // 100–400 Mbps class, 8-bit SDR. Host must support it; else HEVC.
    ("pyrowave", m::player_codec_pyrowave),
];

/// The console titles': the PS5's Videodec2 and the Switch's NVDEC decode H.264 and HEVC, the
/// GPU PyroWave, and nothing decodes AV1.
const CODECS_PS5: [(&str, Label); 4] = [CODECS[0], CODECS[1], CODECS[2], CODECS[4]];

/// The codecs this platform decodes. The TV's NDL pipeline takes H.264, HEVC and AV1, but no
/// PyroWave (no Vulkan presentation). A Samsung set takes H.264 and HEVC: its WebCodecs says
/// yes to AV1 without ever having been measured on one.
pub(super) fn codecs(platform: crate::platform::Platform) -> &'static [(&'static str, Label)] {
    match platform {
        crate::platform::Platform::WebOS => &CODECS[..4],
        crate::platform::Platform::Tizen => &CODECS[..3],
        crate::platform::Platform::Ps5 | crate::platform::Platform::Switch => &CODECS_PS5,
        _ => &CODECS,
    }
}
// Per-OS hardware rungs. Windows has no VAAPI (`Decoder::new` has no branch).
// Stored values are `native-*`; `migrate_decoder_pref` rewrites a legacy store
// on read, but until the user re-picks it will not match a preset here.
#[cfg(not(windows))]
const DECODERS: [(&str, Label); 4] = [
    ("auto", m::player_decoder_auto),
    ("native-vulkan", || "Vulkan Video"),
    ("native-vaapi", || "VAAPI"),
    ("software", m::player_decoder_software),
];
#[cfg(windows)]
const DECODERS: [(&str, Label); 4] = [
    ("auto", m::player_decoder_auto),
    ("native-vulkan", || "Vulkan Video"),
    ("native-d3d11va", || "Direct3D 11"),
    ("software", m::player_decoder_software),
];
const AUDIO: [(u8, Label); 3] = [
    (2, m::player_audio_channels_2),
    (6, m::player_audio_channels_6),
    (8, m::player_audio_channels_8),
];
/// Shared `present_priority` key — one preset reads the same on every client.
const VIDEO_FITS: [(&str, Label); 3] = [
    ("fit", m::player_video_fit_fit),
    ("crop", m::player_video_fit_crop),
    ("stretch", m::player_video_fit_stretch),
];
/// Shared `screens` key; anything but `all` reads as this monitor.
const MONITORS: [(&str, Label); 2] = [
    ("this", m::player_screens_this),
    ("all", m::player_screens_all),
];
const PRESENT_PRIORITIES: [(&str, Label); 2] = [
    ("latency", m::player_present_priority_latency),
    ("smooth", m::player_present_priority_smooth),
];
/// Depth in frames. `0` = Automatic, which resolves to 2.
const SMOOTH_BUFFERS: [u8; 4] = [0, 1, 2, 3];
pub(super) const PAD_TYPES: [(&str, Label); 7] = [
    ("auto", m::player_gamepad_auto),
    ("xbox360", || "Xbox 360"),
    ("xboxone", || "Xbox One"),
    ("dualsense", || "DualSense"),
    ("dualshock4", || "DualShock 4"),
    ("steamdeck", || "Steam Deck"),
    ("steamcontroller2", || "Steam Controller 2"),
];
/// The pad types `platform` can ask a host for. The TV, PS5 and Switch clients map Xbox 360,
/// Steam Deck and Steam Controller 2 to Automatic, so their rows do not offer them.
fn pad_types(platform: crate::platform::Platform) -> Vec<(&'static str, Label)> {
    use crate::platform::Platform;
    PAD_TYPES
        .iter()
        .copied()
        .filter(|(v, _)| {
            !matches!(platform, Platform::WebOS | Platform::Ps5 | Platform::Switch)
                || !matches!(*v, "xbox360" | "steamdeck" | "steamcontroller2")
        })
        .collect()
}
/// Shared `system_buttons` key. Auto sends to the host except in Gaming Mode,
/// where Steam on this device would open a second overlay on the same press.
const SYSTEM_BUTTONS: [(&str, Label); 3] = [
    ("auto", m::player_system_buttons_auto),
    ("forward", m::player_system_buttons_forward),
    ("local", m::player_system_buttons_local),
];

/// Whether this platform has the row at all.
///
/// [`TABS`] is the union so a setting sits under the same word on every client.
/// A concept the platform does not have is absent, never a no-op control.
pub fn row_on(id: RowId, platform: crate::platform::Platform) -> bool {
    use crate::platform::Platform;
    // Rows name the platforms that OFFER them. "Everything except the other one's rows" is
    // well defined for two platforms and ambiguous for three: a new host would inherit every
    // row nobody weighed it against. Unlisted here means universal, as it always did.
    use Platform::{Android, Apple, Desktop, Ps5, Switch, WebOS};
    let on: &[Platform] = match id {
        // Phone sensors: hardware a TV does not have. Apple keeps them for the iPhone and
        // iPad (`rumbleOnDevice`, `gyroFromDevice`); an Apple TV has no sensor to report.
        RowId::PhoneRumble | RowId::PhoneGyro => &[Android, Apple],
        // Apple asks before it captures a Steam Controller 2 (`sc2Capture`); Android captures
        // one whenever it is present.
        RowId::Sc2Passthrough => &[Apple],
        // A room to place the sound and the picture in: the Vision Pro alone (`row_applies`).
        RowId::SpatialAudio | RowId::Screen => &[Apple],
        // The weak-GPU row. On Android it also shrinks the surface; webOS's compositor
        // already hands a 1080p buffer, so there it is the cheaper backdrop alone.
        RowId::ReduceUiResolution => &[Android, WebOS],
        // A MediaCodec decoder flag; nothing else has the knob.
        RowId::LowLatency => &[Android],
        // The clients whose streams read the switch before holding the screen awake.
        RowId::KeepAwake => &[Desktop, Android, Apple],
        // Only Android has a second screen to use (a handheld's lower panel, a half-open fold).
        RowId::SecondScreen => &[Android],
        // The clients whose presenters place the picture through `video_fit`.
        RowId::VideoFit => &[Desktop, Android, Apple],
        // The clients that open a window per monitor; Apple's is the Mac (`row_applies`).
        RowId::Screens => &[Desktop, Apple],
        // Every client's dial sends a preset's screen wish.
        RowId::HostScreen => &Platform::ALL,
        // The clients whose dial sends `pyrowave_bpp`.
        RowId::PyroWaveQuality => &[Desktop, Android, Apple, Ps5, Switch],
        // Offered wherever there is a second UI to fall back to: Android's touch home,
        // webOS's cursor shell. `row_applies` still needs `fallback_ui` from the host.
        RowId::GamepadUi | RowId::GamepadUiMode => &[Android, WebOS, Apple],
        // A pad list: real on a TV, and on Apple its Controllers screen.
        RowId::Controllers => &[Android, WebOS, Apple],
        // Apps a phone or TV can put in the background; `row_applies` drops the Mac.
        RowId::BackgroundKeepAlive | RowId::BackgroundTimeout => &[Android, Apple],
        // The clients whose overlays place and size the statistics by these keys, and draw the
        // exit hint.
        RowId::StatsPosition | RowId::StatsSize | RowId::ExitHint => {
            &[Desktop, Android, Apple, WebOS]
        }
        // The webOS session never reads these; its TV builds its own session from a few keys.
        RowId::RenderScale
        | RowId::AudioFormat
        | RowId::KeepHostAudio
        | RowId::Mic
        | RowId::EchoCancel
        | RowId::PadForward
        | RowId::SystemButtons
        | RowId::Touch => &[Desktop, Android, Apple, Platform::Web, Platform::Tizen],
        // DualSense voice coils and speaker: no Apple or browser client plays them.
        RowId::PadHaptics | RowId::PadSpeaker => &[Desktop, Android, WebOS],
        // The clients whose rumble paths read the switch.
        RowId::PadRumble => &[Desktop, Android, Apple],
        // Every client binds something mid-stream: keys, chords, or a remote's colour buttons.
        RowId::ShortcutsPage => &Platform::ALL,
        // Every client ships third-party code. The browser build has no bundle to list.
        RowId::Licenses => &[Desktop, Android, WebOS, Apple, Ps5, Switch],
        // DualSense capture — the pad reaches webOS over Bluetooth HID, not hidraw, so the
        // concept is real there too (punktfunk-webos docs/NOTES.md).
        RowId::DsCapture => &[Android, WebOS],
        // Apple reads `UIAccessibility.isReduceMotionEnabled` and follows it, so the shell has
        // nothing to ask. The others carry a row because they cannot see the OS switch.
        RowId::ReduceMotion => &[
            Desktop,
            Android,
            WebOS,
            Platform::Web,
            Platform::Tizen,
            Ps5,
            Switch,
        ],
        // Which pad is player 1 — a question only a client that can narrow forwarding to one pad
        // has to answer. Android's router, webOS's slot table and the browser's Gamepad API give
        // every controller its own wire slot, so there is nothing to pick.
        RowId::Pad => &[Desktop],
        // That client's own audio plane and its remote's missing second button.
        RowId::AudioRoute | RowId::CursorGestures => &[WebOS],
        RowId::FullscreenMode => &[Apple],
        // Where the console owns the settings file. A host app keeps its own store and reset.
        RowId::ResetSettings => &[Desktop],
        // Main10 at BT.709 asks nothing of the panel, and MediaCodec and NDL both decode it from
        // the SPS.
        RowId::TenBitSdr => &[Desktop, Android, WebOS, Apple, Ps5, Switch],
        // The PS5 and Switch titles read no mouse.
        RowId::Mouse | RowId::InvertScroll => &[
            Desktop,
            Android,
            WebOS,
            Platform::Web,
            Apple,
            Platform::Tizen,
        ],
        // VideoToolbox and the Metal wavelet path follow the codec, so Apple picks no decoder
        // either. The TV decodes through NDL and the browser through WebCodecs.
        RowId::Decoder => &[Desktop],
        // Chroma and the window-manager knobs: the TV has no window manager and the browser
        // binds no system chord, while Android pins a fixed mode on purpose (`allow_vrr`).
        RowId::Chroma444
        | RowId::Vsync
        | RowId::AllowVrr
        | RowId::Fullscreen
        | RowId::InhibitShortcuts => &[Desktop, Apple],
        _ => &Platform::ALL,
    };
    on.contains(&platform)
}

/// The codec setting is PyroWave on a device that decodes it and dials its quality: the
/// session's rate is the quality's, not Bitrate's.
pub(super) fn pyrowave_rate(ctx: &Ctx) -> bool {
    ctx.settings.codec == "pyrowave"
        && ctx.device.pyrowave_ok
        && row_on(RowId::PyroWaveQuality, ctx.device.platform)
}

/// Offered this frame, as opposed to offered-but-inert.
///
/// Echo cancel and pad rows dim under a parent switch so the relationship stays
/// visible. Smoothness buffer is a knob on one of two intents — under Lowest
/// latency the quantity does not exist, so the row is dropped. It sits directly
/// below the intent row so the cursor is never on a row that vanishes. PyroWave quality
/// comes and goes with the codec; Bitrate stays, as Automatic or Fixed under PyroWave.
pub fn row_applies(id: RowId, ctx: &Ctx) -> bool {
    let apple = ctx.device.platform == crate::platform::Platform::Apple;
    match id {
        // The Apple app reads these on the Mac alone, as its own settings offer them.
        RowId::Vsync | RowId::InhibitShortcuts | RowId::Mouse | RowId::Screens if apple => {
            is_mac(ctx)
        }
        // An Apple TV has no touchscreen, microphone, scroll wheel or variable refresh.
        RowId::Touch | RowId::Mic | RowId::EchoCancel | RowId::InvertScroll | RowId::AllowVrr
            if apple =>
        {
            !ctx.device.tv
        }
        RowId::SmoothBuffer => ctx.settings.present_priority == "smooth",
        RowId::PyroWaveQuality => pyrowave_rate(ctx),
        // Needs `fallback_ui`; otherwise off strands the user with no UI.
        RowId::GamepadUi => ctx.device.fallback_ui,
        // Hidden unless fallback_ui and the switch above is on. Sits below that
        // switch so the cursor is never on a row that vanishes. An Android TV
        // ignores the value (`GamepadUi.kt`: the tv term alone satisfies the OR);
        // webOS obeys it — a Magic Remote with no pad is why its cursor UI exists.
        RowId::GamepadUiMode => ctx.device.fallback_ui && RowId::GamepadUi.extra_on(ctx.settings),
        // The phone's own motor, gyro and SC2 dongle, and its second screen: only a handheld
        // sends its screen (`ConsoleOptions::screen`), so a TV or a Mac never offers them.
        RowId::PhoneRumble | RowId::PhoneGyro | RowId::Sc2Passthrough | RowId::SecondScreen => {
            ctx.device.screen.is_some()
        }
        // A Mac window has no background session; Android and the other Apple devices do.
        RowId::BackgroundKeepAlive => !is_mac(ctx),
        RowId::SpatialAudio | RowId::Screen => ctx.device.spatial,
        RowId::BackgroundTimeout => {
            !is_mac(ctx) && RowId::BackgroundKeepAlive.extra_on(ctx.settings)
        }
        RowId::FullscreenMode => is_mac(ctx),
        RowId::StatsPosition => {
            ctx.settings.stats_verbosity() != pf_client_core::trust::StatsVerbosity::Off
        }
        // The OS answered, and the console follows it: no second switch.
        RowId::ReduceMotion => crate::os_theme::os_reduce_motion().is_none(),
        // `os_theme::available()`, not platform: a new publisher needs no edit here.
        RowId::FollowOsTheme => crate::os_theme::available(),
        // Hidden while follow_os_theme; sits below the switch that drops it.
        RowId::Palette => !(ctx.settings.follow_os_theme && crate::os_theme::available()),
        _ => true,
    }
}

/// The settings a fresh install starts with on this platform: the shared defaults, and the
/// few this platform's own app starts elsewhere.
fn fresh_settings(device: &crate::screens::Device) -> pf_client_core::trust::Settings {
    use crate::platform::Platform;
    let mut s = pf_client_core::trust::Settings::default();
    match device.platform {
        Platform::Android => {
            s.pad_speaker = "off".into();
            s.mouse_mode = "desktop".into();
        }
        Platform::Apple => {
            s.vsync = false;
            set_extra_bool(&mut s, device_keys::SC2, false);
        }
        Platform::Desktop
        | Platform::WebOS
        | Platform::Web
        | Platform::Tizen
        | Platform::Ps5
        | Platform::Switch => {}
    }
    s
}

/// The rows of `ids` that show something other than a fresh install would: what
/// [`RowId::AdvancedChanged`] counts. Compared by the value drawn, so every row kind works alike.
pub fn changed(ids: &[RowId], ctx: &Ctx) -> Vec<RowId> {
    let mut fresh = fresh_settings(ctx.device);
    let under = Ctx {
        hosts: ctx.hosts,
        library: ctx.library,
        settings: &mut fresh,
        store: ctx.store,
        pads: ctx.pads,
        device: ctx.device,
        t: ctx.t,
        profiles: ctx.profiles,
        ui: ctx.ui,
    };
    ids.iter()
        .copied()
        .filter(|id| match id {
            // Its value is a rate the mode rows move too: compare the setting itself.
            RowId::PyroWaveQuality => {
                under.settings.pyrowave_bpp_x100() != ctx.settings.pyrowave_bpp_x100()
            }
            _ => row_spec_base(*id, &under, &[]).value != row_spec_base(*id, ctx, &[]).value,
        })
        .collect()
}

/// This tab's advanced rows, offered here and now, that hold a changed value.
pub(super) fn changed_advanced(tab: usize, ctx: &Ctx) -> Vec<RowId> {
    let offered: Vec<RowId> = TABS[tab]
        .1
        .iter()
        .copied()
        .filter(|id| advanced(*id) && row_on(*id, ctx.device.platform) && row_applies(*id, ctx))
        .collect();
    changed(&offered, ctx)
}

/// This device is a Mac.
pub(super) fn is_mac(ctx: &Ctx) -> bool {
    ctx.device.is_mac()
}

/// Where a launch will actually land, named. Not the stored value: with no default host
/// every setting resolves to the list, and the row says so rather than promising a shelf.
fn start_in_value(ctx: &Ctx) -> String {
    let known = ctx.store.known_hosts();
    let want = start::StartIn::parse(&ctx.settings.start_in);
    match start::default_host(ctx.settings, &known) {
        _ if want == start::StartIn::Hosts => m::player_start_in_hosts().into(),
        Some(i) if want == start::StartIn::Library => {
            m::player_start_in_library_on(&known.hosts[i].name)
        }
        Some(i) => m::player_start_in_stream_on(&known.hosts[i].name),
        None => m::player_start_in_hosts_no_default().into(),
    }
}

/// The row as drawn, keyed by its [`RowId`]. A row a known host's bound preset overrides
/// carries the dot and a note naming preset, host and the value that host streams at; a row
/// no preset touches but a game in the open library sets names host, game and value. The
/// row keeps showing the global, which is what the console edits.
pub fn row_spec(
    id: RowId,
    ctx: &Ctx,
    presets: &[(String, String)],
    overrides: &std::collections::HashMap<String, SettingsOverlay>,
) -> RowSpec {
    let mut spec = row_spec_base(id, ctx, presets);
    spec.id = Some(format!("{id:?}"));
    spec.icon = Some(row_icon(id));
    if let Some((host, preset, overlay)) = preset_override(id, ctx, presets, overrides) {
        let mut resolved = overlay.apply(ctx.settings);
        let under = Ctx {
            hosts: ctx.hosts,
            library: ctx.library,
            settings: &mut resolved,
            store: ctx.store,
            pads: ctx.pads,
            device: ctx.device,
            t: ctx.t,
            profiles: ctx.profiles,
            ui: ctx.ui,
        };
        spec.dot = true;
        // A page row (Shortcuts, Quick actions) has no value to name: the dot alone.
        if let Some(value) = row_spec_base(id, &under, presets).value {
            spec.note = Some(console::console_prefs_override_note(preset, host, value));
        }
    } else if let Some((host, game, value)) = host_override(id, ctx) {
        spec.dot = true;
        spec.note = Some(console::console_prefs_host_note(host, game, value));
    }
    spec
}

/// The open library's first title whose host override sets `id`: `(host, title, value)`.
/// Aspect stays the device's; the override sets lines.
fn host_override(id: RowId, ctx: &Ctx) -> Option<(String, String, String)> {
    let field = preset_field(id).filter(|_| id != RowId::Aspect)?;
    let (fp, (title, value)) = ctx.library.find_game(|g| {
        let value = pf_client_core::library::override_value(g.overrides.as_ref()?, field)?;
        Some((g.title.clone(), value))
    })?;
    let host = ctx.hosts.iter().find(|h| h.fp_hex == fp)?;
    Some((host.name.clone(), title, value))
}

/// The row's Lucide mark.
pub(super) fn row_icon(id: RowId) -> &'static str {
    match id {
        RowId::Aspect | RowId::RenderScale | RowId::Fullscreen | RowId::FullscreenMode => {
            "maximize"
        }
        RowId::Resolution | RowId::ReduceUiResolution => "monitor",
        RowId::Refresh | RowId::Vsync | RowId::AllowVrr => "refresh-cw",
        RowId::Bitrate
        | RowId::PyroWaveQuality
        | RowId::PadRumble
        | RowId::PadHaptics
        | RowId::PhoneRumble => "activity",
        RowId::VideoFit => "square",
        RowId::Compositor => "panel-right",
        RowId::Codec => "film",
        RowId::Hdr | RowId::KeepAwake => "sun",
        RowId::PresentPriority | RowId::SmoothBuffer | RowId::LowLatency => "clock",
        RowId::Decoder | RowId::AudioRoute => "cpu",
        RowId::Chroma444 | RowId::TenBitSdr => "eye",
        RowId::Audio
        | RowId::AudioFormat
        | RowId::KeepHostAudio
        | RowId::PadSpeaker
        | RowId::SpatialAudio => "volume-2",
        RowId::Mic => "mic",
        RowId::EchoCancel => "mic-off",
        RowId::PadForward
        | RowId::Pad
        | RowId::PadType
        | RowId::Sc2Passthrough
        | RowId::DsCapture
        | RowId::Controllers => "gamepad-2",
        RowId::SystemButtons => "house",
        RowId::PhoneGyro => "rotate-cw",
        RowId::SecondScreen | RowId::Screen | RowId::Screens | RowId::HostScreen => "monitor",
        RowId::Touch | RowId::CursorGestures => "pointer",
        RowId::Mouse => "mouse",
        RowId::QuickActions => "ellipsis",
        RowId::InvertScroll => "undo-2",
        RowId::InhibitShortcuts | RowId::ShortcutsPage => "keyboard",
        RowId::FollowOsTheme => "moon",
        RowId::Palette => "palette",
        RowId::LibrarySections => "grip-vertical",
        RowId::LibraryView => "menu",
        RowId::StartIn => "play",
        RowId::GamepadUi | RowId::GamepadUiMode => "tv",
        RowId::Stats | RowId::AdvancedStats => "chart-column",
        RowId::StatsPosition => "panel-right",
        RowId::HostSort => "grip-vertical",
        RowId::HostGrouping => "square",
        RowId::BackgroundKeepAlive => "moon",
        RowId::BackgroundTimeout => "clock",
        RowId::ReduceMotion => "eye",
        RowId::AutoWake => "power",
        RowId::Version | RowId::Licenses => "info",
        RowId::ShowAdvanced | RowId::AdvancedChanged => "wrench",
        RowId::StatsSize => "chart-column",
        RowId::ExitHint => "log-out",
        RowId::ResetSettings => "undo-2",
        RowId::Preset(_) | RowId::NewPreset => "settings",
    }
}

/// The first known host whose bound preset, or one bound to a title, overrides `id`:
/// `(host, preset name, overlay)`. Pinned shortcut rows are skipped; the host's own row
/// carries its binding.
fn preset_override<'a>(
    id: RowId,
    ctx: &'a Ctx,
    presets: &'a [(String, String)],
    overrides: &'a std::collections::HashMap<String, SettingsOverlay>,
) -> Option<(&'a str, &'a str, &'a SettingsOverlay)> {
    ctx.hosts.iter().filter(|h| h.pin.is_none()).find_map(|h| {
        h.bound_preset
            .iter()
            .map(|c| c.id.as_str())
            .chain(h.game_presets.values().map(String::as_str))
            .find_map(|pid| {
                let overlay = overrides.get(pid).filter(|o| overrides_row(id, o))?;
                let name = presets.iter().find(|(id, _)| id == pid)?.1.as_str();
                Some((h.name.as_str(), name, overlay))
            })
    })
}

/// The overlay field a preset stores for this row, by its serialised name
/// ([`SettingsOverlay::clear`]); `None` for a row no preset carries.
pub(crate) fn preset_field(id: RowId) -> Option<&'static str> {
    Some(match id {
        RowId::Resolution | RowId::Aspect => "resolution",
        RowId::Refresh => "refresh_hz",
        RowId::RenderScale => "render_scale",
        RowId::VideoFit => "video_fit",
        RowId::Screens => "screens",
        RowId::HostScreen => "host_screen",
        RowId::Bitrate => "bitrate_kbps",
        RowId::PyroWaveQuality => "pyrowave_bpp",
        RowId::Compositor => "compositor",
        RowId::Codec => "codec",
        RowId::Hdr => "hdr_enabled",
        RowId::Chroma444 => "enable_444",
        RowId::TenBitSdr => "ten_bit_sdr",
        RowId::PresentPriority => "present_priority",
        RowId::SmoothBuffer => "smooth_buffer",
        RowId::Vsync => "vsync",
        RowId::AllowVrr => "allow_vrr",
        RowId::Audio => "audio_channels",
        RowId::AudioFormat => "audio_format",
        RowId::KeepHostAudio => "keep_host_audio",
        RowId::Mic => "mic_enabled",
        RowId::EchoCancel => "echo_cancel",
        RowId::PadForward => "gamepad_forwarding",
        RowId::PadType => "gamepad",
        RowId::SystemButtons => "system_buttons",
        RowId::Touch => "touch_mode",
        RowId::Mouse => "mouse_mode",
        RowId::InvertScroll => "invert_scroll",
        RowId::InhibitShortcuts => "inhibit_shortcuts",
        RowId::QuickActions => "overlay_actions",
        RowId::ShortcutsPage => "shortcuts",
        RowId::Stats => "stats_verbosity",
        RowId::Fullscreen => "fullscreen_on_stream",
        _ => return None,
    })
}

/// The Host screen row's choices at `s`'s value, over each host the console shows that has
/// answered for its screens. A pinned card repeats its host, so it is skipped.
fn host_screens(hosts: &[crate::model::HostRow], s: &Settings) -> Vec<HostScreenChoice> {
    let known: Vec<(&str, &Targets)> = (hosts.iter().filter(|h| h.pin.is_none()))
        .filter_map(|h| Some((h.name.as_str(), h.screens.as_ref()?)))
        .collect();
    host_screen_choices(&known, Some(&s.host_screen))
}

/// The rows a preset can hold on this platform this frame, each with its section. Host screen,
/// on no tab, follows Screens.
pub(crate) fn preset_rows(ctx: &Ctx) -> Vec<(Section, RowId)> {
    (TABS.iter())
        .flat_map(|(tab, rows)| rows.iter().map(move |id| (*tab, *id)))
        .flat_map(|(tab, id)| {
            let host_screen = (id == RowId::Screens).then_some((tab, RowId::HostScreen));
            std::iter::once((tab, id)).chain(host_screen)
        })
        .filter(|(_, id)| preset_field(*id).is_some())
        .filter(|(_, id)| row_on(*id, ctx.device.platform) && row_applies(*id, ctx))
        .collect()
}

/// Whether an overlay pins the value this row shows.
pub(crate) fn overrides_row(id: RowId, o: &SettingsOverlay) -> bool {
    preset_field(id).is_some_and(|f| o.overrides(f))
}

fn row_spec_base(id: RowId, ctx: &Ctx, presets: &[(String, String)]) -> RowSpec {
    // Pin count from live host rows, matching the carousel.
    match id {
        RowId::Preset(i) => {
            let (pid, name) = &presets[i];
            let pins = ctx
                .hosts
                .iter()
                .filter(|h| h.pin.as_ref().is_some_and(|p| &p.id == pid))
                .count();
            return RowSpec {
                header: None,
                label: name.clone(),
                value: Some(match pins {
                    0 => console::console_preset_not_pinned().into(),
                    n => console::console_preset_pinned(n as u64),
                }),
                value_dim: pins == 0,
                caret: false,
                adjustable: false,
                enabled: true,
                ..RowSpec::default()
            };
        }
        RowId::NewPreset => {
            return RowSpec::action(m::preset_new(), true);
        }
        RowId::Controllers => return RowSpec::action(console::console_prefs_controllers(), true),
        // The label is the tab's count; `SettingsScreen::spec` writes it in.
        RowId::AdvancedChanged => return RowSpec::action(String::new(), true),
        RowId::Licenses => return RowSpec::action(m::about_licenses(), true),
        RowId::ResetSettings => {
            return RowSpec {
                danger: true,
                ..RowSpec::action(m::prefs_reset_all(), true)
            };
        }
        RowId::QuickActions => return RowSpec::action(m::player_overlay_actions(), true),
        RowId::ShortcutsPage => return RowSpec::action(m::player_shortcuts(), true),
        // Opens the cards: the value names the pick, no ‹ › to step it.
        RowId::Palette => {
            return RowSpec {
                label: m::player_ui_palette().into(),
                value: Some((crate::palette::palette(&ctx.settings.ui_palette).name)().into()),
                ..RowSpec::default()
            };
        }
        RowId::LibrarySections => {
            let on = crate::library::sections(&ctx.settings.library_sections)
                .iter()
                .filter(|(_, on)| *on)
                .count();
            let all = crate::library::Section::ALL.len();
            return RowSpec {
                label: m::player_library_sections().into(),
                value: Some(match on {
                    n if n == all => m::player_library_sections_all().into(),
                    n => m::player_library_sections_some(n as u64, all),
                }),
                ..RowSpec::default()
            };
        }
        RowId::Version => {
            return RowSpec {
                label: m::about_version().into(),
                value: Some(ctx.device.version.clone()),
                ..RowSpec::default()
            };
        }
        // The rate the quality needs at the picture the host encodes; the link's warning under it.
        RowId::PyroWaveQuality => {
            let (rate, warning) = ctx
                .settings
                .pyrowave_quality_lines(native_mode(ctx.device), local_link());
            let spec = RowSpec::choice(m::player_pyrowave_bpp(), rate);
            return match warning {
                Some(w) => spec.with_note(w),
                None => spec,
            };
        }
        _ => {}
    }
    let s = &ctx.settings;
    let enabled = row_enabled(id, s);
    let extra = || id.extra().expect("an extra row").label(s);
    let (header, label, value): (Option<&'static str>, &str, String) = match id {
        RowId::Resolution => (
            None,
            m::player_resolution(),
            if s.match_window && has_window(ctx.device) {
                m::player_resolution_match_window().into()
            } else if safe_area(s, ctx.device.platform) {
                m::player_resolution_native_safe_area().into()
            } else if s.width == 0 {
                m::player_resolution_native().into()
            } else if custom_size(s, &families(ctx.device.screen), ctx.device.platform) {
                m::player_resolution_custom_size(s.width, s.height)
            } else {
                format!("{} × {}", s.width, s.height)
            },
        ),
        RowId::Aspect => {
            let fams = families(ctx.device.screen);
            (
                None,
                m::player_resolution_aspect(),
                family_label(&fams[family(s, &fams, ctx.device.platform)]).into(),
            )
        }
        RowId::Refresh => (
            None,
            m::player_refresh_hz(),
            if s.refresh_hz == 0 {
                m::player_refresh_hz_native().into()
            } else {
                common::common_hz(s.refresh_hz)
            },
        ),
        RowId::RenderScale => {
            // In hundredths, so a float-stored 0.67 does not read as 0.6700000167.
            let x = f64::from(punktfunk_core::render_scale::percent(s.render_scale)) / 100.0;
            let value = if x == 1.0 {
                m::player_render_scale_native().into()
            } else if x > 1.0 {
                m::player_render_scale_supersample(x)
            } else {
                format!("{x}×")
            };
            (None, m::player_render_scale(), value)
        }
        RowId::VideoFit => (
            None,
            m::player_video_fit(),
            label_for(&VIDEO_FITS, VideoFit::from_name(&s.video_fit).name()).into(),
        ),
        RowId::Screens => (
            None,
            m::player_screens(),
            MONITORS[usize::from(s.all_screens())].1().into(),
        ),
        RowId::HostScreen => (
            None,
            m::preset_host_screen(),
            (host_screens(ctx.hosts, s).into_iter())
                .find(|c| c.checked)
                .map(|c| c.label)
                .unwrap_or_default(),
        ),
        RowId::Bitrate => (
            None,
            m::player_bitrate_kbps(),
            if s.bitrate_kbps == 0 {
                m::player_bitrate_kbps_automatic().into()
            } else if pyrowave_rate(ctx) {
                m::player_bitrate_kbps_fixed().into()
            } else {
                bitrate_label(s.bitrate_kbps)
            },
        ),
        RowId::Compositor => (
            None,
            m::player_compositor(),
            label_for(&COMPOSITORS, &s.compositor).into(),
        ),
        RowId::Codec => (
            None,
            m::player_codec(),
            if s.codec == "pyrowave" && !ctx.device.pyrowave_ok {
                m::player_codec_pyrowave_unsupported().into()
            } else if s.codec == "av1" && !ctx.device.av1_ok {
                m::player_codec_av1_unsupported().into()
            } else {
                label_for(codecs(ctx.device.platform), &s.codec).into()
            },
        ),
        // Migrate before lookup or a legacy store (`vulkan`/`vaapi`) shows "—".
        RowId::Decoder => (
            None,
            m::player_decoder(),
            label_for(
                &DECODERS,
                &pf_client_core::decoder_pref::migrate_decoder_pref(&s.decoder),
            )
            .into(),
        ),
        RowId::Hdr => (None, m::player_hdr_enabled(), extra()),
        RowId::Chroma444 => (None, m::player_enable_444(), extra()),
        RowId::TenBitSdr => (None, m::player_ten_bit_sdr(), extra()),
        RowId::PresentPriority => (
            None,
            m::player_present_priority(),
            label_for(&PRESENT_PRIORITIES, &s.present_priority).into(),
        ),
        RowId::SmoothBuffer => (
            None,
            m::player_smooth_buffer(),
            match s.smooth_buffer {
                n if n != 0 && SMOOTH_BUFFERS.contains(&n) => {
                    m::player_smooth_buffer_frames(u64::from(n))
                }
                _ => m::player_smooth_buffer_automatic().into(),
            },
        ),
        RowId::Vsync => (None, "V-Sync", extra()),
        RowId::AllowVrr => (None, m::player_allow_vrr(), extra()),
        RowId::Audio => (
            None,
            m::player_audio_channels(),
            AUDIO
                .iter()
                .find(|(v, _)| *v == s.audio_channels)
                .map_or(m::player_audio_channels_2(), |(_, l)| l())
                .into(),
        ),
        RowId::AudioFormat => (
            None,
            m::player_audio_format(),
            audio_format_label(&s.audio_format).into(),
        ),
        RowId::KeepHostAudio => (None, m::player_keep_host_audio(), extra()),
        RowId::Mic => (None, m::player_mic_enabled(), extra()),
        RowId::EchoCancel => (None, m::player_echo_cancel(), extra()),
        RowId::SpatialAudio => (None, m::player_spatial_audio(), extra()),
        RowId::PadForward => (None, m::player_gamepad_forwarding(), extra()),
        RowId::Pad => (
            None,
            m::player_forward_pad(),
            if s.forward_pad.is_empty() {
                m::player_forward_pad_auto().into()
            } else {
                ctx.pads
                    .iter()
                    .find(|p| p.key == s.forward_pad)
                    .map_or_else(|| m::player_forward_pad_saved().into(), |p| p.name.clone())
            },
        ),
        RowId::PadType => (
            None,
            m::player_gamepad(),
            label_for(&PAD_TYPES, &s.gamepad).into(),
        ),
        RowId::SystemButtons => (
            None,
            m::player_system_buttons(),
            label_for(&SYSTEM_BUTTONS, &s.system_buttons).into(),
        ),
        RowId::PadRumble => (None, m::player_pad_rumble(), extra()),
        RowId::PadHaptics => (None, m::player_pad_haptics(), extra()),
        RowId::PadSpeaker => (
            None,
            m::player_pad_speaker(),
            on_off(pad_speaker_on(&s.pad_speaker)).into(),
        ),
        RowId::Touch => (None, m::player_touch_mode(), s.touch_mode().label().into()),
        RowId::Mouse => (None, m::player_mouse_mode(), s.mouse_mode().label().into()),
        RowId::InvertScroll => (None, m::player_invert_scroll(), extra()),
        RowId::InhibitShortcuts => (None, m::player_inhibit_shortcuts(), extra()),
        RowId::FollowOsTheme => (None, m::player_follow_os_theme(), extra()),
        // Label is the reduction, so On means it is in effect.
        RowId::ReduceMotion => (None, m::player_reduce_motion(), extra()),
        RowId::ReduceUiResolution => (
            None,
            m::player_reduce_ui_resolution(),
            on_off(reduce_ui_res(
                s,
                ctx.device.platform,
                ctx.device.fallback_ui,
            ))
            .into(),
        ),
        RowId::LibraryView => (
            None,
            m::player_library_view(),
            crate::library::LibraryView::parse(&s.library_view)
                .label()
                .into(),
        ),
        RowId::StartIn => (None, m::player_start_in(), start_in_value(ctx)),
        RowId::Stats => (
            None,
            m::player_stats_verbosity(),
            s.stats_verbosity().label().into(),
        ),
        RowId::AdvancedStats => (None, m::player_advanced_stats(), extra()),
        RowId::ShowAdvanced => (None, m::player_show_advanced(), extra()),
        RowId::StatsSize => (
            None,
            m::player_stats_scale_pct(),
            common::common_percent(
                (punktfunk_core::hud::stats_scale(s.stats_scale_pct) * 100.0).round(),
            ),
        ),
        RowId::ExitHint => (None, m::player_exit_hint(), extra()),
        RowId::HostSort => (None, m::player_host_sort(), extra()),
        RowId::Screen => (None, m::player_screen(), extra()),
        RowId::HostGrouping => (
            None,
            m::player_host_grouping(),
            label_for(&home::HOST_GROUPINGS, host_grouping(s)).into(),
        ),
        RowId::StatsPosition => (
            None,
            m::player_hud_placement(),
            s.hud_corner(own_stats_corner(ctx.device.platform))
                .label()
                .into(),
        ),
        RowId::BackgroundKeepAlive => (None, m::player_background_keep_alive(), extra()),
        RowId::BackgroundTimeout => (
            None,
            m::player_background_timeout_minutes(),
            match background_timeout(s) {
                n if BACKGROUND_TIMEOUTS.contains(&n) => {
                    m::player_background_timeout_minutes_count(n)
                }
                _ => "\u{2014}".into(),
            },
        ),
        RowId::Fullscreen => (None, m::player_fullscreen_on_stream(), extra()),
        RowId::FullscreenMode => (
            None,
            m::player_fullscreen(),
            FULLSCREEN_MODES[fullscreen_mode(s)]().into(),
        ),
        RowId::AutoWake => (None, m::player_auto_wake(), extra()),
        RowId::LowLatency => (None, m::player_low_latency(), extra()),
        RowId::KeepAwake => (None, m::player_keep_awake(), extra()),
        RowId::SecondScreen => (None, m::player_second_screen(), extra()),
        RowId::PhoneRumble => (
            Some(console::console_prefs_header_this_device()),
            m::player_rumble_on_phone(),
            extra(),
        ),
        RowId::PhoneGyro => (None, m::player_gyro_on_phone(), extra()),
        RowId::Sc2Passthrough => (None, m::player_sc2_capture(), extra()),
        RowId::DsCapture => (None, m::player_ds_capture(), extra()),
        RowId::AudioRoute => (None, m::player_audio_route(), extra()),
        RowId::CursorGestures => (None, m::player_cursor_gestures(), extra()),
        RowId::GamepadUi => (None, m::player_gamepad_ui_enabled(), extra()),
        RowId::GamepadUiMode => (
            None,
            // Not "Controller UI": that collides with the row above.
            m::player_gamepad_ui_mode(),
            extra(),
        ),
        RowId::Preset(_)
        | RowId::NewPreset
        | RowId::Controllers
        | RowId::AdvancedChanged
        | RowId::Licenses
        | RowId::ResetSettings
        | RowId::QuickActions
        | RowId::ShortcutsPage
        | RowId::LibrarySections
        | RowId::Palette
        | RowId::PyroWaveQuality
        | RowId::Version => {
            unreachable!("returned above")
        }
    };
    RowSpec {
        header,
        label: label.into(),
        value: Some(value),
        value_dim: !enabled,
        caret: false,
        adjustable: enabled,
        enabled,
        ..RowSpec::default()
    }
}

/// Off under a parent switch: the row dims and does not step, so the relationship stays
/// visible. Smoothness buffer is dropped instead — see [`row_applies`].
fn row_enabled(id: RowId, s: &pf_client_core::trust::Settings) -> bool {
    match id {
        RowId::EchoCancel => s.mic_enabled,
        // Session still drops lossless unless `audio_channels == 2` (before the wire).
        // A live row under surround would change nothing. Delete this arm when that
        // filter learns the frame ladder — not before.
        RowId::AudioFormat => s.audio_channels == 2,
        RowId::Pad
        | RowId::PadType
        | RowId::PadRumble
        | RowId::SystemButtons
        | RowId::PadHaptics
        | RowId::PadSpeaker => s.gamepad_forwarding,
        _ => true,
    }
}

/// One-line explainer. Platform so Android is not taught desktop-only chords. A button or
/// chord is data filled into its own sentence, after the hint.
pub fn detail(id: RowId, ctx: &Ctx) -> Cow<'static, str> {
    use crate::platform::Platform;
    let platform = ctx.device.platform;
    let two = |hint: &str, more: String| -> Cow<'static, str> { format!("{hint} {more}").into() };
    match id {
        RowId::Resolution => m::player_resolution_hint().into(),
        RowId::Aspect => m::player_resolution_aspect_hint().into(),
        RowId::Refresh => m::player_refresh_hz_hint().into(),
        RowId::RenderScale => m::player_render_scale_hint().into(),
        RowId::VideoFit => m::player_video_fit_hint().into(),
        RowId::Screens => m::player_screens_hint().into(),
        // The picked role's hint; none for the host card's pick or a monitor.
        RowId::HostScreen => (host_screens(ctx.hosts, ctx.settings).into_iter())
            .find(|c| c.checked)
            .and_then(|c| c.hint)
            .unwrap_or_default()
            .into(),
        RowId::PyroWaveQuality => m::player_pyrowave_bpp_hint().into(),
        RowId::Bitrate if pyrowave_rate(ctx) => m::player_bitrate_kbps_pyrowave_hint().into(),
        RowId::Bitrate => m::player_bitrate_kbps_hint().into(),
        RowId::Compositor => m::player_compositor_hint().into(),
        RowId::Codec if ctx.settings.codec == "pyrowave" && !ctx.device.pyrowave_ok => {
            m::player_codec_pyrowave_unsupported_hint().into()
        }
        RowId::Codec if ctx.settings.codec == "av1" && !ctx.device.av1_ok => {
            m::player_codec_av1_unsupported_hint().into()
        }
        RowId::Codec => m::player_codec_hint().into(),
        RowId::Decoder => m::player_decoder_hint().into(),
        RowId::Hdr => m::player_hdr_enabled_hint().into(),
        RowId::Chroma444 => m::player_enable_444_hint().into(),
        RowId::TenBitSdr => m::player_ten_bit_sdr_hint().into(),
        RowId::PresentPriority => m::player_present_priority_hint().into(),
        RowId::SmoothBuffer => m::player_smooth_buffer_hint().into(),
        RowId::Vsync => m::player_vsync_hint().into(),
        RowId::AllowVrr => m::player_allow_vrr_hint().into(),
        RowId::Audio => m::player_audio_channels_hint().into(),
        RowId::AudioFormat => m::player_audio_format_hint().into(),
        RowId::KeepHostAudio => m::player_keep_host_audio_hint().into(),
        RowId::Mic => two(
            m::player_mic_enabled_hint(),
            m::player_mic_enabled_keys_hint("Ctrl+Alt+Shift+V"),
        ),
        RowId::EchoCancel => m::player_echo_cancel_hint().into(),
        RowId::Screen => m::player_screen_hint().into(),
        RowId::SpatialAudio => m::player_spatial_audio_hint().into(),
        RowId::PadForward => m::player_gamepad_forwarding_hint().into(),
        RowId::Pad => m::player_forward_pad_hint().into(),
        RowId::PadType => m::player_gamepad_hint().into(),
        RowId::SystemButtons => m::player_system_buttons_hint().into(),
        RowId::PadRumble => m::player_pad_rumble_hint().into(),
        RowId::PadHaptics => m::player_pad_haptics_hint().into(),
        RowId::PadSpeaker => m::player_pad_speaker_hint().into(),
        RowId::Touch => m::player_touch_mode_hint().into(),
        RowId::Mouse => match platform {
            Platform::Desktop => two(
                m::player_mouse_mode_hint(),
                m::player_mouse_mode_keys_hint("Ctrl+Alt+Shift+M"),
            ),
            // No live chord to name: none of these hosts binds one.
            Platform::Android
            | Platform::WebOS
            | Platform::Web
            | Platform::Apple
            | Platform::Tizen
            | Platform::Ps5
            | Platform::Switch => m::player_mouse_mode_hint().into(),
        },
        RowId::InvertScroll => m::player_invert_scroll_hint().into(),
        RowId::QuickActions => console::console_player_overlay_actions_hint("Select+A").into(),
        RowId::ShortcutsPage => m::player_shortcuts_hint().into(),
        RowId::InhibitShortcuts => {
            m::player_inhibit_shortcuts_hint("Alt+Tab, Super, \u{2026}").into()
        }
        RowId::FollowOsTheme => m::player_follow_os_theme_hint().into(),
        RowId::Palette => m::player_ui_palette_hint().into(),
        RowId::ReduceMotion => m::player_reduce_motion_hint().into(),
        RowId::ReduceUiResolution => match platform {
            Platform::WebOS => console::console_reduce_ui_resolution_webos_hint().into(),
            _ => m::player_reduce_ui_resolution_hint().into(),
        },
        RowId::LibraryView => m::player_library_view_hint().into(),
        RowId::StartIn => m::player_start_in_hint().into(),
        RowId::Stats => match platform {
            Platform::Desktop => two(
                m::player_stats_verbosity_hint(),
                m::player_stats_verbosity_cycle_hint("Ctrl+Alt+Shift+S"),
            ),
            Platform::Android
            | Platform::WebOS
            | Platform::Web
            | Platform::Apple
            | Platform::Tizen
            | Platform::Ps5
            | Platform::Switch => m::player_stats_verbosity_hint().into(),
        },
        RowId::AdvancedStats => two(
            m::player_advanced_stats_hint(),
            m::player_advanced_stats_docs_hint("docs.punktfunk.unom.io/docs/stats"),
        ),
        RowId::Fullscreen => m::player_fullscreen_on_stream_hint().into(),
        RowId::FullscreenMode => match fullscreen_mode(ctx.settings) {
            0 => m::player_fullscreen_off_hint().into(),
            1 => m::player_fullscreen_stream_hint().into(),
            _ => m::player_fullscreen_always_hint().into(),
        },
        RowId::StatsPosition => m::player_hud_placement_hint().into(),
        RowId::StatsSize => m::player_stats_scale_pct_hint().into(),
        RowId::ExitHint => m::player_exit_hint_hint().into(),
        RowId::ShowAdvanced => m::player_show_advanced_hint().into(),
        RowId::AdvancedChanged => console::console_prefs_advanced_changed_hint("A").into(),
        RowId::HostSort => m::player_host_sort_hint().into(),
        RowId::HostGrouping => m::player_host_grouping_hint().into(),
        RowId::BackgroundKeepAlive => m::player_background_keep_alive_hint().into(),
        RowId::BackgroundTimeout if ctx.device.tv => {
            m::player_background_timeout_minutes_tv_hint().into()
        }
        RowId::BackgroundTimeout => m::player_background_timeout_minutes_hint().into(),
        RowId::AutoWake => m::player_auto_wake_hint().into(),
        RowId::LowLatency => m::player_low_latency_hint().into(),
        RowId::KeepAwake => m::player_keep_awake_hint().into(),
        RowId::SecondScreen => m::player_second_screen_hint().into(),
        RowId::PhoneRumble => m::player_rumble_on_phone_hint().into(),
        RowId::PhoneGyro => m::player_gyro_on_phone_hint().into(),
        RowId::Sc2Passthrough => m::player_sc2_capture_hint().into(),
        RowId::DsCapture => m::player_ds_capture_hint().into(),
        RowId::AudioRoute => m::player_audio_route_hint().into(),
        RowId::CursorGestures => m::player_cursor_gestures_hint("OK").into(),
        // The other UI is named, not called "the other UI": the sentence has to tell
        // the reader where "off" lands, and that is a different place per client.
        RowId::GamepadUi => match platform {
            // `row_on` offers this to Android, webOS and Apple; Desktop, Web, Tizen and PS5 are
            // here for exhaustiveness, never to be read.
            Platform::Desktop
            | Platform::Android
            | Platform::Web
            | Platform::Apple
            | Platform::Tizen
            | Platform::Ps5
            | Platform::Switch => console::console_gamepad_ui_enabled_hint().into(),
            Platform::WebOS => console::console_gamepad_ui_enabled_webos_hint().into(),
        },
        RowId::GamepadUiMode => match platform {
            Platform::Desktop
            | Platform::Android
            | Platform::Web
            | Platform::Apple
            | Platform::Tizen
            | Platform::Ps5
            | Platform::Switch => console::console_gamepad_ui_mode_hint().into(),
            Platform::WebOS => console::console_gamepad_ui_mode_webos_hint().into(),
        },
        RowId::Controllers => console::console_prefs_controllers_hint().into(),
        RowId::Licenses => m::about_licenses_hint().into(),
        RowId::ResetSettings => m::prefs_reset_all_hint().into(),
        RowId::LibrarySections => m::player_library_sections_hint().into(),
        RowId::Version => console::console_about_version_hint().into(),
        RowId::Preset(_) => console::console_preset_row_hint().into(),
        RowId::NewPreset => console::console_preset_new_hint().into(),
    }
}

/// Mbit/s below 1 Gbit/s, Gbit/s above. Decimal only when rounding would collide
/// (12.5 Mbit/s, 1.5 Gbit/s). Off-ladder rates are real (typed field, desktop spinner).
pub(super) fn bitrate_label(kbps: u32) -> String {
    let number = |v: f64| {
        if (v - v.round()).abs() < 0.05 {
            format!("{}", v.round())
        } else {
            format!("{v:.1}")
        }
    };
    let mbps = f64::from(kbps) / 1000.0;
    if kbps >= 1_000_000 {
        common::common_gbits(number(mbps / 1000.0))
    } else {
        common::common_mbits(number(mbps))
    }
}

fn on_off(v: bool) -> &'static str {
    if v {
        common::common_on()
    } else {
        common::common_off()
    }
}

fn label_for(options: &[(&str, Label)], value: &str) -> &'static str {
    options
        .iter()
        .find(|(v, _)| *v == value)
        .map_or("—", |(_, l)| l())
}

/// Label for a stored `audio_format`. Unknown → Opus (what the session runs), not
/// [`label_for`]'s "—": a shared catalog can carry a newer client's rung.
fn audio_format_label(value: &str) -> &'static str {
    AUDIO_FORMATS
        .iter()
        .find(|(v, _)| *v == value)
        .or_else(|| AUDIO_FORMATS.iter().find(|(v, _)| *v == AUDIO_FORMAT_OPUS))
        .map_or("", |(_, l)| l())
}

/// Step (`wrap=false`, clamp; `None` = boundary) or cycle (`wrap=true`).
/// Toggles: left = off, right = on. A no-op is a boundary.
pub fn adjust(id: RowId, delta: i32, wrap: bool, ctx: &mut Ctx) -> bool {
    if !row_enabled(id, ctx.settings) {
        return false;
    }
    let platform = ctx.device.platform;
    let fams = families(ctx.device.screen);
    let window = has_window(ctx.device);
    let pyrowave = pyrowave_rate(ctx);
    let hosts = ctx.hosts;
    let s = &mut *ctx.settings;
    match id {
        RowId::Resolution => {
            // Native, Native (safe area) on Android, Match window where there is a window, the
            // current family's sizes, then a typed size while one is stored. The policies before
            // the sizes all clear w/h; stepping onto the typed size is a no-op, so it only reads.
            let sizes = &fams[family(s, &fams, platform)].sizes;
            let android = platform == crate::platform::Platform::Android;
            let match_i = 1 + usize::from(android);
            let first = match_i + usize::from(window);
            let custom = custom_size(s, &fams, platform);
            let len = sizes.len() + first + usize::from(custom);
            let cur = if s.match_window && window {
                Some(match_i)
            } else if safe_area(s, platform) {
                Some(1)
            } else if s.width == 0 {
                Some(0)
            } else if custom {
                Some(len - 1)
            } else {
                sizes
                    .iter()
                    .position(|&wh| wh == (s.width, s.height))
                    .map(|i| i + first)
            };
            let stepped = step_option(cur, len, delta, wrap).filter(|i| !(custom && *i == len - 1));
            stepped.map(|i| {
                s.match_window = window && i == match_i;
                if android {
                    set_extra_bool(s, device_keys::SAFE_AREA_MODE, i == 1);
                }
                (s.width, s.height) = if i < first { (0, 0) } else { sizes[i - first] };
            })
        }
        RowId::Aspect => {
            // Steps from the entry shown, so Native (listed under its own entry)
            // moves on to the next shape rather than restating the one it reads as.
            step_option(Some(family(s, &fams, platform)), fams.len(), delta, wrap).map(|i| {
                s.match_window = false;
                (s.width, s.height) = nearest_in(&fams[i], s.height);
            })
        }
        RowId::Refresh => {
            let cur = REFRESH.iter().position(|r| *r == s.refresh_hz);
            step_option(cur, REFRESH.len(), delta, wrap).map(|i| s.refresh_hz = REFRESH[i])
        }
        RowId::RenderScale => {
            // A typed scale sits between two stops: step to the one the thumb is heading for.
            let stepped = match punktfunk_core::render_scale::preset_index(s.render_scale) {
                cur @ Some(_) => step_option(cur, RENDER_SCALES.len(), delta, wrap),
                None if delta < 0 => RENDER_SCALES.iter().rposition(|v| *v < s.render_scale),
                None => RENDER_SCALES.iter().position(|v| *v > s.render_scale),
            };
            stepped.map(|i| s.render_scale = RENDER_SCALES[i])
        }
        RowId::VideoFit => {
            let cur = VIDEO_FITS
                .iter()
                .position(|(v, _)| *v == VideoFit::from_name(&s.video_fit).name());
            step_option(cur, VIDEO_FITS.len(), delta, wrap)
                .map(|i| s.video_fit = VIDEO_FITS[i].0.to_string())
        }
        RowId::Screens => {
            let cur = Some(usize::from(s.all_screens()));
            step_option(cur, MONITORS.len(), delta, wrap)
                .map(|i| s.screens = MONITORS[i].0.to_string())
        }
        // Empty is the host card's pick, which the preset editor stores as no pin.
        RowId::HostScreen => {
            let choices = host_screens(hosts, s);
            let cur = choices.iter().position(|c| c.checked);
            step_option(cur, choices.len(), delta, wrap)
                .map(|i| s.host_screen = choices[i].value.clone().unwrap_or_default())
        }
        // Automatic or Fixed: the quality row stands in for the rate.
        RowId::Bitrate if pyrowave => {
            let rates = [0, pf_client_core::settings::PYROWAVE_FIXED_KBPS];
            step_option(Some(usize::from(s.bitrate_kbps != 0)), 2, delta, wrap)
                .map(|i| s.bitrate_kbps = rates[i])
        }
        RowId::Bitrate => {
            // Off-ladder must not snap to Automatic (index 0). Step to the neighbour
            // the thumb is heading for.
            // Only the rungs this platform may reach — see [`bitrate_rungs`]. A value stored
            // above them (set on another client, or in a shared preset) still steps DOWN
            // from where it is rather than being silently rewritten here.
            let rungs = &BITRATES[..bitrate_rungs(platform)];
            let stepped = match rungs.iter().position(|b| *b == s.bitrate_kbps) {
                Some(i) => step_option(Some(i), rungs.len(), delta, wrap),
                None if delta < 0 => rungs.iter().rposition(|b| *b < s.bitrate_kbps),
                // Above the ceiling: wrap goes to Automatic; clamp thuds.
                None => rungs.iter().position(|b| *b > s.bitrate_kbps).or(if wrap {
                    Some(0)
                } else {
                    None
                }),
            };
            stepped.map(|i| s.bitrate_kbps = rungs[i])
        }
        // A value another client set between rungs steps to the neighbour in that direction.
        RowId::PyroWaveQuality => {
            let x100 = s.pyrowave_bpp_x100();
            let stepped = match PYROWAVE_BPP_X100.iter().position(|&r| r == x100) {
                Some(i) => step_option(Some(i), PYROWAVE_BPP_X100.len(), delta, wrap),
                None if delta < 0 => PYROWAVE_BPP_X100.iter().rposition(|&r| r < x100),
                None => PYROWAVE_BPP_X100.iter().position(|&r| r > x100),
            };
            stepped.map(|i| s.pyrowave_bpp = f64::from(PYROWAVE_BPP_X100[i]) / 100.0)
        }
        RowId::Compositor => step_str(&COMPOSITORS, &mut s.compositor, delta, wrap),
        RowId::Codec => step_str(codecs(platform), &mut s.codec, delta, wrap),
        RowId::Decoder => {
            // Migrate first or a legacy value jumps to first/last instead of its neighbour.
            s.decoder = pf_client_core::decoder_pref::migrate_decoder_pref(&s.decoder);
            step_str(&DECODERS, &mut s.decoder, delta, wrap)
        }
        RowId::PresentPriority => {
            let cur = PRESENT_PRIORITIES
                .iter()
                .position(|(v, _)| *v == s.present_priority);
            step_option(cur, PRESENT_PRIORITIES.len(), delta, wrap)
                .map(|i| s.present_priority = PRESENT_PRIORITIES[i].0.to_string())
        }
        // Not offered under latency ([`row_applies`]). Reachable only if another
        // writer flipped intent between list-build and this keypress: thud, don't store.
        RowId::SmoothBuffer => {
            if s.present_priority == "smooth" {
                let cur = SMOOTH_BUFFERS.iter().position(|v| *v == s.smooth_buffer);
                step_option(cur, SMOOTH_BUFFERS.len(), delta, wrap)
                    .map(|i| s.smooth_buffer = SMOOTH_BUFFERS[i])
            } else {
                None
            }
        }
        RowId::Audio => {
            let cur = AUDIO.iter().position(|(v, _)| *v == s.audio_channels);
            step_option(cur, AUDIO.len(), delta, wrap).map(|i| s.audio_channels = AUDIO[i].0)
        }
        RowId::AudioFormat => step_str(AUDIO_FORMATS, &mut s.audio_format, delta, wrap),
        RowId::Pad => {
            // Automatic first, then connected pads by stable key.
            let keys: Vec<String> = std::iter::once(String::new())
                .chain(ctx.pads.iter().map(|p| p.key.clone()))
                .collect();
            let cur = keys.iter().position(|c| *c == s.forward_pad);
            step_option(cur, keys.len(), delta, wrap).map(|i| s.forward_pad = keys[i].clone())
        }
        RowId::PadType => step_str(&pad_types(platform), &mut s.gamepad, delta, wrap),
        RowId::SystemButtons => step_str(&SYSTEM_BUTTONS, &mut s.system_buttons, delta, wrap),
        RowId::PadSpeaker => {
            // `"mix"` reads Off; a step writes only `"pad"` / `"off"`.
            let mut on = pad_speaker_on(&s.pad_speaker);
            toggle(&mut on, delta, wrap)
                .map(|()| s.pad_speaker = if on { "pad" } else { "off" }.to_string())
        }
        RowId::Touch => {
            let cur = TouchMode::ALL.iter().position(|m| *m == s.touch_mode());
            step_option(cur, TouchMode::ALL.len(), delta, wrap)
                .map(|i| s.touch_mode = TouchMode::ALL[i].as_name().to_string())
        }
        RowId::Mouse => {
            let cur = MouseMode::ALL.iter().position(|m| *m == s.mouse_mode());
            step_option(cur, MouseMode::ALL.len(), delta, wrap)
                .map(|i| s.mouse_mode = MouseMode::ALL[i].as_name().to_string())
        }
        RowId::Stats => {
            let cur = StatsVerbosity::ALL
                .iter()
                .position(|v| *v == s.stats_verbosity());
            step_option(cur, StatsVerbosity::ALL.len(), delta, wrap)
                .map(|i| s.set_stats_verbosity(StatsVerbosity::ALL[i]))
        }
        RowId::ReduceUiResolution => toggle_extra(
            s,
            reduce_ui_key(platform),
            reduce_ui_default(platform, ctx.device.fallback_ui),
            delta,
            wrap,
        ),
        RowId::LibraryView => {
            let all = &crate::library::LibraryView::ALL;
            let cur = crate::library::LibraryView::parse(&s.library_view);
            let at = all.iter().position(|v| *v == cur);
            step_option(at, all.len(), delta, wrap).map(|i| s.library_view = all[i].id().into())
        }
        RowId::StartIn => {
            let all = &start::StartIn::ALL;
            let at = all
                .iter()
                .position(|v| *v == start::StartIn::parse(&s.start_in));
            step_option(at, all.len(), delta, wrap).map(|i| s.start_in = all[i].as_str().into())
        }
        RowId::FullscreenMode => step_option(Some(fullscreen_mode(s)), 3, delta, wrap).map(|i| {
            set_extra_bool(s, FULLSCREEN_ALWAYS_KEY, i == 2);
            if i < 2 {
                s.fullscreen_on_stream = i == 1;
            }
        }),
        RowId::LowLatency
        | RowId::KeepAwake
        | RowId::Hdr
        | RowId::Chroma444
        | RowId::TenBitSdr
        | RowId::Vsync
        | RowId::AllowVrr
        | RowId::KeepHostAudio
        | RowId::Mic
        | RowId::EchoCancel
        | RowId::SpatialAudio
        | RowId::PadForward
        | RowId::PadRumble
        | RowId::PadHaptics
        | RowId::InvertScroll
        | RowId::InhibitShortcuts
        | RowId::AdvancedStats
        | RowId::FollowOsTheme
        | RowId::ReduceMotion
        | RowId::Fullscreen
        | RowId::AutoWake
        | RowId::ExitHint
        | RowId::ShowAdvanced
        | RowId::SecondScreen
        | RowId::PhoneRumble
        | RowId::PhoneGyro
        | RowId::Sc2Passthrough
        | RowId::DsCapture
        | RowId::CursorGestures
        | RowId::AudioRoute
        | RowId::GamepadUi
        | RowId::BackgroundKeepAlive
        | RowId::HostSort
        | RowId::Screen
        | RowId::GamepadUiMode => id.extra().and_then(|e| e.step(s, delta, wrap)),
        RowId::StatsPosition => {
            let at = HudCorner::ALL
                .iter()
                .position(|c| *c == s.hud_corner(own_stats_corner(platform)));
            step_option(at, HudCorner::ALL.len(), delta, wrap)
                .map(|i| s.hud_placement = HudCorner::ALL[i].as_name().into())
        }
        RowId::StatsSize => {
            use punktfunk_core::hud::{stats_scale, STATS_SCALE_PCTS};
            // Stepped from the size in effect, so an off-list stored value moves to its neighbour.
            let pct = (stats_scale(s.stats_scale_pct) * 100.0).round() as u16;
            let at = STATS_SCALE_PCTS.iter().position(|p| *p == pct);
            step_option(at, STATS_SCALE_PCTS.len(), delta, wrap)
                .map(|i| s.stats_scale_pct = STATS_SCALE_PCTS[i])
        }
        RowId::BackgroundTimeout => {
            let cur = (BACKGROUND_TIMEOUTS.iter()).position(|m| *m == background_timeout(s));
            step_option(cur, BACKGROUND_TIMEOUTS.len(), delta, wrap).map(|i| {
                s.extra.insert(
                    BACKGROUND_TIMEOUT_KEY.to_string(),
                    BACKGROUND_TIMEOUTS[i].into(),
                );
            })
        }
        RowId::HostGrouping => {
            let mut v = host_grouping(s).to_string();
            step_str(&home::HOST_GROUPINGS, &mut v, delta, wrap).map(|()| {
                s.extra.insert(
                    home::HOST_GROUPING_KEY.to_string(),
                    serde_json::Value::String(v),
                );
            })
        }
        // Navigation rows: handled in `apply_row` before the settings path.
        RowId::Preset(_)
        | RowId::NewPreset
        | RowId::Controllers
        | RowId::AdvancedChanged
        | RowId::ShortcutsPage
        | RowId::Licenses
        | RowId::ResetSettings
        | RowId::QuickActions
        | RowId::LibrarySections
        | RowId::Palette
        | RowId::Version => None,
    }
    .is_some()
}

/// Clamp when adjusting, wrap when cycling. Unknown current value snaps to first.
pub(super) fn step_option(
    current: Option<usize>,
    len: usize,
    delta: i32,
    wrap: bool,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let Some(cur) = current else { return Some(0) };
    let target = cur as i32 + delta;
    if wrap {
        Some(target.rem_euclid(len as i32) as usize)
    } else if target < 0 || target >= len as i32 {
        None
    } else {
        Some(target as usize)
    }
}

fn step_str<L>(options: &[(&str, L)], value: &mut String, delta: i32, wrap: bool) -> Option<()> {
    let cur = options.iter().position(|(v, _)| v == value);
    step_option(cur, options.len(), delta, wrap).map(|i| *value = options[i].0.to_string())
}

pub(super) fn toggle(value: &mut bool, delta: i32, wrap: bool) -> Option<()> {
    let target = if wrap { !*value } else { delta > 0 };
    if *value == target {
        None
    } else {
        *value = target;
        Some(())
    }
}

/// The stored value of each entry of a list row, in the order the row steps through them; the
/// catalogue test pins these. `None` for a row that is not a list.
#[cfg(test)]
pub(super) fn stored_options(
    id: RowId,
    platform: crate::platform::Platform,
) -> Option<Vec<String>> {
    fn ids<L>(table: &[(&str, L)]) -> Vec<String> {
        table.iter().map(|(v, _)| v.to_string()).collect()
    }
    Some(match id {
        RowId::StartIn => (start::StartIn::ALL.iter())
            .map(|v| v.as_str().into())
            .collect(),
        RowId::Stats => (StatsVerbosity::ALL.iter())
            .map(|v| format!("{v:?}").to_lowercase())
            .collect(),
        RowId::GamepadUiMode => ids(&GAMEPAD_UI_MODES),
        RowId::LibraryView => (crate::library::LibraryView::ALL.iter())
            .map(|v| v.id().into())
            .collect(),
        RowId::HostSort => ids(&home::HOST_SORTS),
        RowId::HostGrouping => ids(&home::HOST_GROUPINGS),
        RowId::StatsPosition => HudCorner::ALL.iter().map(|c| c.as_name().into()).collect(),
        RowId::StatsSize => (punktfunk_core::hud::STATS_SCALE_PCTS.iter())
            .map(|p| p.to_string())
            .collect(),
        RowId::BackgroundTimeout => BACKGROUND_TIMEOUTS.iter().map(|m| m.to_string()).collect(),
        RowId::Refresh => REFRESH.iter().map(|hz| hz.to_string()).collect(),
        RowId::RenderScale => RENDER_SCALES.iter().map(|x| x.to_string()).collect(),
        RowId::VideoFit => ids(&VIDEO_FITS),
        RowId::Screens => ids(&MONITORS),
        RowId::PresentPriority => ids(&PRESENT_PRIORITIES),
        RowId::SmoothBuffer => SMOOTH_BUFFERS.iter().map(|n| n.to_string()).collect(),
        RowId::Codec => ids(codecs(platform)),
        RowId::Decoder => ids(&DECODERS),
        RowId::Compositor => ids(&COMPOSITORS),
        RowId::Screen => ids(&SCREENS),
        RowId::Audio => AUDIO.iter().map(|(n, _)| n.to_string()).collect(),
        RowId::AudioFormat => ids(AUDIO_FORMATS),
        RowId::AudioRoute => ids(&WEBOS_AUDIO_ROUTES),
        RowId::Touch => TouchMode::ALL.iter().map(|m| m.as_name().into()).collect(),
        RowId::Mouse => MouseMode::ALL.iter().map(|m| m.as_name().into()).collect(),
        RowId::PadType => ids(&pad_types(platform)),
        RowId::SystemButtons => ids(&SYSTEM_BUTTONS),
        _ => return None,
    })
}

/// What a fresh install on `device` stores for a row, spelled as the catalogue spells a
/// default: `true` or `false` for a switch, the digits of a number. `None` for a row with no
/// one value.
#[cfg(test)]
pub(super) fn stored_default(id: RowId, device: &crate::screens::Device) -> Option<String> {
    use serde_json::Value;
    let s = fresh_settings(device);
    if let Some(extra) = id.extra() {
        return Some(match extra {
            Extra::Bool(key, default) => extra_bool(&s, key, default).to_string(),
            Extra::Flag(get, _) => get(&s).to_string(),
            Extra::Choice(key, default, _) => extra_str(&s, key, default).to_string(),
        });
    }
    let json = serde_json::to_value(&s).ok()?;
    let field = |key: &str| match &json[key] {
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => n.as_f64().map(|n| n.to_string()),
        Value::String(t) => Some(t.clone()),
        _ => None,
    };
    match id {
        RowId::Refresh => field("refresh_hz"),
        RowId::Bitrate => field("bitrate_kbps"),
        RowId::PyroWaveQuality => field("pyrowave_bpp"),
        RowId::RenderScale => field("render_scale"),
        RowId::VideoFit => field("video_fit"),
        RowId::Screens => field("screens"),
        RowId::Compositor => field("compositor"),
        RowId::Codec => field("codec"),
        RowId::Decoder => field("decoder"),
        RowId::PresentPriority => field("present_priority"),
        RowId::SmoothBuffer => field("smooth_buffer"),
        RowId::Audio => field("audio_channels"),
        RowId::AudioFormat => field("audio_format"),
        RowId::PadType => field("gamepad"),
        RowId::SystemButtons => field("system_buttons"),
        RowId::Touch => field("touch_mode"),
        RowId::Mouse => field("mouse_mode"),
        RowId::Palette => field("ui_palette"),
        RowId::PadSpeaker => Some(pad_speaker_on(&s.pad_speaker).to_string()),
        RowId::Stats => Some(format!("{:?}", s.stats_verbosity()).to_lowercase()),
        RowId::StartIn => Some(start::StartIn::parse(&s.start_in).as_str().into()),
        RowId::LibraryView => Some(
            crate::library::LibraryView::parse(&s.library_view)
                .id()
                .into(),
        ),
        RowId::HostGrouping => Some(host_grouping(&s).into()),
        RowId::BackgroundTimeout => Some(background_timeout(&s).to_string()),
        RowId::StatsSize => {
            let pct = punktfunk_core::hud::stats_scale(s.stats_scale_pct) * 100.0;
            Some(pct.round().to_string())
        }
        RowId::StatsPosition => Some(
            (s.hud_corner(own_stats_corner(device.platform)))
                .as_name()
                .into(),
        ),
        RowId::ReduceUiResolution => {
            Some(reduce_ui_res(&s, device.platform, device.fallback_ui).to_string())
        }
        _ => None,
    }
}
