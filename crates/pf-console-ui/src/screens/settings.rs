//! Console settings: the couch-facing subset of the shared Settings store.
//!
//! One row per setting, in the sections of [`TABS`], named in a strip of tabs over the
//! list. Up from the first row, or B from any, reaches the sections; Left/Right there
//! switch them, Down returns. Left/right steps the focused value (clamped); A cycles
//! wrapping; L1/R1 change section; B on the sections closes. Every change writes the
//! store immediately so desktop shells round-trip the same file.
//! Each section remembers its cursor. Presets lists the catalog and ends on New preset;
//! a preset's own screens ([`super::preset`]) edit it through the host.
//!
//! Section names match `settings_sections` in `clients/shared/console-vectors.json`.
//! Platform split: [`row_on`]. Availability this frame: [`row_applies`].

use crate::glyphs::{Hint, HintKey};
use crate::pointer::Pointer;
use crate::screens::{home, Ctx, Outbox, Screen};
use crate::theme::Fonts;
use crate::widgets::{
    column, entry_hints, field_key, permits, type_text, Charset, Entry, Keyboard, ListMsg,
    MenuList, RowSpec, TabStrip, TAB_STRIP_H,
};
use pf_client_core::audio_format::{AUDIO_FORMATS, AUDIO_FORMAT_OPUS};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use pf_client_core::presets::SettingsOverlay;
use pf_client_core::start;
use pf_client_core::trust::{HudCorner, MouseMode, StatsVerbosity, TouchMode};
use skia_safe::{Canvas, Rect};

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
    Bitrate,
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
    PadForward,
    Pad,
    PadType,
    /// `trust::Settings::pad_rumble`. Off, the client drops the host's rumble and the
    /// console's own pulses.
    PadRumble,
    SystemButtons,
    GuideGesture,
    /// `trust::Settings::pad_haptics`. Negotiated: needs a capable host and a wired DS5.
    PadHaptics,
    /// `trust::Settings::pad_speaker`. On (`"pad"`) / Off only — stored `"mix"`
    /// renders as off, so offering it would be a no-op control.
    PadSpeaker,
    Touch,
    Mouse,
    InvertScroll,
    Shortcuts,
    /// `trust::Settings::overlay_actions`. Action row: opens [`super::ring_editor::RingEditorScreen`].
    QuickActions,
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
    /// Action row: the stream's keys, chords and gestures, read-only.
    StreamControls,
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
}

/// Rows the tabs list only under Show advanced: their default is right for nearly everyone,
/// picking a value takes knowing how streaming works, and no first stream needs them.
pub fn advanced(id: RowId) -> bool {
    matches!(
        id,
        RowId::SmoothBuffer
            | RowId::RenderScale
            | RowId::Codec
            | RowId::Chroma444
            | RowId::TenBitSdr
            | RowId::Vsync
            | RowId::AllowVrr
            | RowId::Compositor
            | RowId::Decoder
            | RowId::LowLatency
            | RowId::AudioFormat
            | RowId::KeepHostAudio
            | RowId::EchoCancel
            | RowId::AudioRoute
            | RowId::PadForward
            | RowId::Pad
            | RowId::SystemButtons
            | RowId::GuideGesture
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
mod device_keys {
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
}

/// The `Settings::extra` keys the webOS rows share with that client (`services::store::shared`),
/// which namespaces everything only it models.
mod webos_keys {
    pub const AUDIO_ROUTE: &str = "webos.audio_route";
    pub const CURSOR_GESTURES: &str = "webos.cursor_gestures";
    pub const REDUCE_UI_RES: &str = "webos.reduce_ui_resolution";
}

/// Stored `webos.audio_route` values, spelled as that client's enum serializes.
const WEBOS_AUDIO_ROUTES: [(&str, &str); 2] =
    [("software", "Software (SDL)"), ("ndlopus", "Offload (NDL)")];

/// The console-vs-fallback pair, unprefixed: webOS carries the same two keys (its
/// fallback is the cursor UI, not a touch home), so they name a concept rather than
/// a platform. Kotlin reads and writes them under these names too.
const GAMEPAD_UI_KEY: &str = "gamepad_ui_enabled";
const GAMEPAD_UI_MODE_KEY: &str = "gamepad_ui_mode";

/// Stored [`GAMEPAD_UI_MODE_KEY`] values (`GamepadUi.kt`).
const GAMEPAD_UI_MODES: [(&str, &str); 2] =
    [("connected", "With a controller"), ("always", "Always")];

/// Apple's `fullscreenAlways`: the Mac window opens fullscreen and stays so between streams.
/// Device-only, so no preset carries it.
const FULLSCREEN_ALWAYS_KEY: &str = "fullscreen_always";
const FULLSCREEN_MODES: [&str; 3] = ["Off", "While streaming", "Always"];

/// Index into [`FULLSCREEN_MODES`].
fn fullscreen_mode(s: &pf_client_core::trust::Settings) -> usize {
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
const BACKGROUND_TIMEOUTS: [(&str, &str); 4] = [
    ("1", "1 minute"),
    ("5", "5 minutes"),
    ("10", "10 minutes"),
    ("30", "30 minutes"),
];
const BACKGROUND_TIMEOUT_DEFAULT: u64 = 10;

/// The corner each platform's stats overlay sits in until the player picks one.
pub(crate) fn own_stats_corner(platform: crate::platform::Platform) -> HudCorner {
    use crate::platform::Platform;
    match platform {
        Platform::Desktop | Platform::Android => HudCorner::TopLeft,
        Platform::Apple | Platform::WebOS | Platform::Web | Platform::Tizen => HudCorner::TopRight,
    }
}

/// `"pad"` is the only live value; `"mix"` renders as off. Local copy because
/// `pad_audio` is `cfg(linux|windows)` and Android still sends the setting.
fn pad_speaker_on(mode: &str) -> bool {
    mode == "pad"
}

fn extra_bool(s: &pf_client_core::trust::Settings, key: &str, default: bool) -> bool {
    s.extra
        .get(key)
        .and_then(|v| v.as_bool())
        .unwrap_or(default)
}

fn set_extra_bool(s: &mut pf_client_core::trust::Settings, key: &str, value: bool) {
    s.extra
        .insert(key.to_string(), serde_json::Value::Bool(value));
}

fn extra_str<'a>(s: &'a pf_client_core::trust::Settings, key: &str, default: &'a str) -> &'a str {
    s.extra.get(key).and_then(|v| v.as_str()).unwrap_or(default)
}

/// Apple's store may still hold `profile`, the old name for grouping by preset.
fn host_grouping(s: &pf_client_core::trust::Settings) -> &str {
    match extra_str(s, home::HOST_GROUPING_KEY, "none") {
        "profile" => "preset",
        g => g,
    }
}

fn background_timeout(s: &pf_client_core::trust::Settings) -> u64 {
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

/// A row whose whole value lives in `Settings::extra`: its key, what an unwritten key reads
/// as, and for a choice the values it steps through. Reading and stepping share the entry.
#[derive(Clone, Copy)]
enum Extra {
    Bool(&'static str, bool),
    Choice(
        &'static str,
        &'static str,
        &'static [(&'static str, &'static str)],
    ),
}

impl Extra {
    fn label(self, s: &pf_client_core::trust::Settings) -> String {
        match self {
            Extra::Bool(key, default) => on_off(extra_bool(s, key, default)).into(),
            Extra::Choice(key, default, options) => {
                label_for(options, extra_str(s, key, default)).into()
            }
        }
    }

    fn step(self, s: &mut pf_client_core::trust::Settings, delta: i32, wrap: bool) -> Option<()> {
        match self {
            Extra::Bool(key, default) => toggle_extra(s, key, default, delta, wrap),
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
    /// The `extra` entry behind this row, when that entry is the row's whole value.
    fn extra(self) -> Option<Extra> {
        Some(match self {
            RowId::LowLatency => Extra::Bool(device_keys::LOW_LATENCY, true),
            RowId::SecondScreen => Extra::Bool(device_keys::SECOND_SCREEN, true),
            RowId::PhoneRumble => Extra::Bool(device_keys::PHONE_RUMBLE, false),
            RowId::PhoneGyro => Extra::Bool(device_keys::PHONE_GYRO, false),
            RowId::Sc2Passthrough => Extra::Bool(device_keys::SC2, true),
            RowId::DsCapture => Extra::Bool(device_keys::DS_CAPTURE, true),
            RowId::CursorGestures => Extra::Bool(webos_keys::CURSOR_GESTURES, false),
            RowId::GamepadUi => Extra::Bool(GAMEPAD_UI_KEY, true),
            RowId::BackgroundKeepAlive => Extra::Bool(BACKGROUND_KEEP_ALIVE_KEY, false),
            RowId::AudioRoute => {
                Extra::Choice(webos_keys::AUDIO_ROUTE, "software", &WEBOS_AUDIO_ROUTES)
            }
            RowId::HostSort => Extra::Choice(home::HOST_SORT_KEY, "added", &home::HOST_SORTS),
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

/// The value an unwritten [`reduce_ui_key`] resolves to: on for the TV shells — a webOS set
/// always is one, and an Android host without the touch fallback is the box plugged into a
/// panel far faster than its GPU. Off elsewhere, where the GPU is not the bottleneck.
fn reduce_ui_default(platform: crate::platform::Platform, fallback_ui: bool) -> bool {
    use crate::platform::Platform;
    platform == Platform::WebOS || (platform == Platform::Android && !fallback_ui)
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

/// The explainer band under the rows, design units.
const DETAIL_H: f64 = crate::widgets::FOOT_DETAIL_H;

// The sections, the rows a player touches most first and the [`advanced`] ones last. A child
// row sits right under the switch it dims or drops with. Presets is empty here: its rows come
// from the catalog.
const TABS: [(&str, &[RowId]); 7] = [
    (
        "General",
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
        ],
    ),
    (
        "Display",
        &[
            RowId::Aspect,
            RowId::Resolution,
            RowId::Refresh,
            RowId::Bitrate,
            RowId::VideoFit,
            RowId::SecondScreen,
            RowId::Hdr,
            RowId::PresentPriority,
            RowId::SmoothBuffer,
            RowId::RenderScale,
            RowId::Codec,
            RowId::Chroma444,
            RowId::TenBitSdr,
            RowId::Vsync,
            RowId::AllowVrr,
            RowId::Compositor,
            RowId::Decoder,
            RowId::LowLatency,
        ],
    ),
    (
        "Audio",
        &[
            RowId::Audio,
            RowId::Mic,
            RowId::AudioFormat,
            RowId::KeepHostAudio,
            RowId::EchoCancel,
            RowId::AudioRoute,
        ],
    ),
    (
        "Input",
        &[
            RowId::Touch,
            RowId::Mouse,
            RowId::InvertScroll,
            RowId::Shortcuts,
            RowId::QuickActions,
            RowId::CursorGestures,
        ],
    ),
    (
        "Controllers",
        &[
            RowId::Controllers,
            RowId::PadType,
            RowId::PadRumble,
            RowId::PhoneRumble,
            RowId::PhoneGyro,
            RowId::PadForward,
            RowId::Pad,
            RowId::SystemButtons,
            RowId::GuideGesture,
            RowId::PadHaptics,
            RowId::PadSpeaker,
            RowId::Sc2Passthrough,
            RowId::DsCapture,
        ],
    ),
    ("Presets", &[]),
    (
        "About",
        &[RowId::Version, RowId::StreamControls, RowId::Licenses],
    ),
];

/// The Presets section — catalog-built, not [`TABS`] rows.
const PRESETS_TAB: usize = 5;

/// Strip length for the shell's raster walk. `cfg(test)`: a shipping build
/// would warn it dead, and this crate treats warnings as errors.
#[cfg(test)]
pub(crate) const TAB_COUNT: usize = TABS.len();

/// Sizes by family; the Resolution row lists one family at a time.
use punktfunk_core::resolutions::{family_of, nearest_in, Family, SAFE_AREA_LABEL, SCREEN_LABEL};

/// The Aspect row's entries: this device's screen and safe area first when no
/// standard shape has them, then the standard families.
fn families(screen: Option<crate::shell::DeviceScreen>) -> Vec<Family> {
    punktfunk_core::resolutions::families(screen.map(|s| s.full), screen.map(|s| s.safe))
}

/// The entry the Resolution row lists: the stored size's shape. Native (safe
/// area) and Native read as the device's own entries where it has them;
/// otherwise, and for a shape none has, the first entry.
fn family(
    s: &pf_client_core::trust::Settings,
    fams: &[Family],
    platform: crate::platform::Platform,
) -> usize {
    let own = |label| fams.iter().position(|f| f.label == label);
    if s.width == 0 && !s.match_window {
        let native = if safe_area(s, platform) {
            own(SAFE_AREA_LABEL).or_else(|| own(SCREEN_LABEL))
        } else {
            own(SCREEN_LABEL)
        };
        return native.unwrap_or(0);
    }
    family_of(fams, s.width, s.height).unwrap_or(0)
}

/// A window the stream can follow (Match window): the desktops, the browser, and every Apple
/// device but the TV.
fn has_window(device: &crate::screens::Device) -> bool {
    use crate::platform::Platform;
    match device.platform {
        Platform::Desktop | Platform::Web | Platform::Tizen => true,
        Platform::Apple => !device.tv,
        Platform::Android | Platform::WebOS => false,
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
const REFRESH: [u32; 8] = [0, 30, 60, 90, 120, 144, 165, 240];
/// Render-scale multipliers; `1.0` = Native.
use punktfunk_core::render_scale::PRESETS as RENDER_SCALES;
use punktfunk_core::video_fit::VideoFit;
/// Left/right rungs in kbps. Denser below ~20 Mbps; ceiling 2 Gbps. Off-ladder
/// values go through the Y field rather than a longer ladder.
const BITRATES: [u32; 30] = [
    0, 1_000, 2_000, 3_000, 4_000, 5_000, 6_000, 8_000, 10_000, 12_000, 15_000, 20_000, 25_000,
    30_000, 40_000, 50_000, 60_000, 80_000, 100_000, 125_000, 150_000, 200_000, 250_000, 300_000,
    400_000, 500_000, 750_000, 1_000_000, 1_500_000, 2_000_000,
];
/// Typed-field ceiling in Mbps — the ladder's top. Host range is 500 kbps–8 Gbps.
const CUSTOM_MAX_MBPS: u32 = 2_000;

/// The highest fixed bitrate this client may be set to, in kbps.
///
/// webOS is the one platform with a real ceiling: the TV client bounds its own slider at
/// 200 Mbps and clamps the document to it, so a shell offering more would write a number the
/// classic menus take straight back off again. Everywhere else the ladder's own top stands.
pub(crate) fn bitrate_ceiling_kbps(platform: crate::platform::Platform) -> u32 {
    match platform {
        crate::platform::Platform::WebOS => 200_000,
        crate::platform::Platform::Desktop
        | crate::platform::Platform::Android
        | crate::platform::Platform::Web
        | crate::platform::Platform::Apple
        | crate::platform::Platform::Tizen => CUSTOM_MAX_MBPS * 1_000,
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
const COMPOSITORS: [(&str, &str); 6] = [
    ("auto", "Automatic"),
    ("kwin", "KWin"),
    ("mutter", "Mutter"),
    ("hyprland", "Hyprland"),
    ("wlroots", "wlroots"),
    ("gamescope", "gamescope"),
];
const CODECS: [(&str, &str); 5] = [
    ("auto", "Automatic"),
    ("hevc", "HEVC"),
    ("h264", "H.264"),
    ("av1", "AV1"),
    // 100–400 Mbps class, 8-bit SDR. Host must support it; else HEVC.
    ("pyrowave", "PyroWave (wired LAN)"),
];

/// The codecs this platform decodes. The TV's NDL pipeline takes H.264 and HEVC only:
/// no AV1 (never presented a picture) and no PyroWave (no Vulkan presentation). A Samsung set
/// is the same pair: its WebCodecs says yes to AV1 without ever having been measured on one.
fn codecs(platform: crate::platform::Platform) -> &'static [(&'static str, &'static str)] {
    match platform {
        crate::platform::Platform::WebOS | crate::platform::Platform::Tizen => &CODECS[..3],
        _ => &CODECS,
    }
}
// Per-OS hardware rungs. Windows has no VAAPI (`Decoder::new` has no branch).
// Stored values are `native-*`; `migrate_decoder_pref` rewrites a legacy store
// on read, but until the user re-picks it will not match a preset here.
#[cfg(not(windows))]
const DECODERS: [(&str, &str); 4] = [
    ("auto", "Automatic"),
    ("native-vulkan", "Vulkan Video"),
    ("native-vaapi", "VAAPI"),
    ("software", "Software"),
];
#[cfg(windows)]
const DECODERS: [(&str, &str); 4] = [
    ("auto", "Automatic"),
    ("native-vulkan", "Vulkan Video"),
    ("native-d3d11va", "Direct3D 11"),
    ("software", "Software"),
];
const AUDIO: [(u8, &str); 3] = [(2, "Stereo"), (6, "5.1"), (8, "7.1")];
/// Shared `present_priority` key — one preset reads the same on every client.
const VIDEO_FITS: [(&str, &str); 3] = [
    ("fit", "Fit"),
    ("crop", "Crop to fill"),
    ("stretch", "Stretch to fill"),
];
const PRESENT_PRIORITIES: [(&str, &str); 2] =
    [("latency", "Lowest latency"), ("smooth", "Smoothness")];
/// Depth in frames. `0` = Automatic, which resolves to 2.
const SMOOTH_BUFFERS: [(u8, &str); 4] = [
    (0, "Automatic"),
    (1, "1 frame"),
    (2, "2 frames"),
    (3, "3 frames"),
];
const PAD_TYPES: [(&str, &str); 7] = [
    ("auto", "Automatic"),
    ("xbox360", "Xbox 360"),
    ("xboxone", "Xbox One"),
    ("dualsense", "DualSense"),
    ("dualshock4", "DualShock 4"),
    ("steamdeck", "Steam Deck"),
    ("steamcontroller2", "Steam Controller 2"),
];
/// The pad types `platform` can ask a host for. The TV client maps Xbox 360, Steam Deck and
/// Steam Controller 2 to Automatic, so its row does not offer them.
fn pad_types(platform: crate::platform::Platform) -> Vec<(&'static str, &'static str)> {
    PAD_TYPES
        .iter()
        .copied()
        .filter(|(v, _)| {
            platform != crate::platform::Platform::WebOS
                || !matches!(*v, "xbox360" | "steamdeck" | "steamcontroller2")
        })
        .collect()
}
/// Shared `system_buttons` key. Auto sends to the host except in Gaming Mode,
/// where Steam on this device would open a second overlay on the same press.
const SYSTEM_BUTTONS: [(&str, &str); 3] = [
    ("auto", "Automatic"),
    ("forward", "Send to host"),
    ("local", "This device"),
];
const GUIDE_GESTURE: [(&str, &str); 3] = [("auto", "Automatic"), ("on", "On"), ("off", "Off")];

/// What the open typed field sets. Y opens it on Bitrate, or on Resolution for a width and then
/// a height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Typing {
    Bitrate,
    Width,
    /// The width typed a step earlier.
    Height(u32),
}

pub(crate) struct SettingsScreen {
    pub(super) list: MenuList,
    strip: TabStrip,
    tab: usize,
    /// Per-tab cursor so a detour does not reset the one you left.
    tab_cursors: [usize; TABS.len()],
    /// `(id, name)`, re-read at most twice a second while the Presets tab is up: a preset
    /// saved from its own screens lands through the host.
    presets: Vec<(String, String)>,
    presets_at: f64,
    /// Each preset's overrides by id, loaded with `presets`: the rows say when a
    /// host's bound preset outranks the global value they show.
    overrides: std::collections::HashMap<String, SettingsOverlay>,
    /// D-pad focus on the section strip. TV remotes have no shoulders and no Tab key.
    strip_focus: bool,
    /// The typed field Y opened, and its digits. Y, not A, so A still cycles.
    typing: Option<(Typing, String)>,
    /// Tray keyboard. Unused on Deck: Steam's keyboard types (same as add-host).
    keyboard: Keyboard,
    /// How far the keyboard tray is up, 0..1, as the last frame left it.
    seat: f64,
}

impl SettingsScreen {
    pub(crate) fn new(store: &dyn crate::store::SettingsStore) -> SettingsScreen {
        let mut s = Self::with_presets(store.presets());
        s.overrides = store.preset_overrides();
        s
    }

    fn with_presets(presets: Vec<(String, String)>) -> SettingsScreen {
        SettingsScreen {
            list: MenuList::new(),
            strip: TabStrip::new(),
            tab: 0,
            tab_cursors: [0; TABS.len()],
            presets,
            presets_at: 0.0,
            overrides: Default::default(),
            strip_focus: false,
            typing: None,
            keyboard: Keyboard::new(),
            seat: 0.0,
        }
    }

    /// True while the typed field is open; the run loop keeps SDL text input started.
    pub(crate) fn editing(&self) -> bool {
        self.typing.is_some()
    }

    pub(crate) fn edit_field(&self) -> Option<crate::screens::EditField> {
        let (typing, text) = self.typing.as_ref()?;
        let label = match typing {
            Typing::Bitrate => "Bitrate in Mbps",
            Typing::Width => "Width in pixels",
            Typing::Height(_) => "Height in pixels",
        };
        crate::screens::EditField::new(label, text, true)
    }

    /// Digits only; four is 2000 Mbps and 8192 px, the ceilings.
    fn admits(text: &str, ch: char) -> bool {
        permits(Charset::Digits, ch) && text.chars().count() < 4
    }

    /// SDL text into the open field.
    pub(crate) fn text_input(&mut self, typed: &str) {
        if let Some((_, text)) = self.typing.as_mut() {
            type_text(text, typed, Self::admits);
        }
    }

    /// Every way out of the field commits it: the typed number is the setting.
    pub(crate) fn edit_key(&mut self, key: crate::input::Key, ctx: &mut Ctx) -> bool {
        let Some((_, text)) = self.typing.as_mut() else {
            return false;
        };
        let Some(entry) = field_key(key, text) else {
            return false;
        };
        if entry != Entry::Stay {
            self.commit_field(ctx);
        }
        true
    }

    /// Close the field, or move from the width to the height. Empty or `0` abandons the edit:
    /// it is not Automatic or Native, the rows' first entries.
    fn commit_field(&mut self, ctx: &mut Ctx) {
        let Some((typing, text)) = self.typing.take() else {
            return;
        };
        let Some(n) = text.parse::<u32>().ok().filter(|n| *n > 0) else {
            return;
        };
        match typing {
            Typing::Bitrate => {
                ctx.write(|c| {
                    let ceiling_mbps = bitrate_ceiling_kbps(c.device.platform) / 1_000;
                    c.settings.bitrate_kbps = n.min(ceiling_mbps) * 1000;
                    true
                });
            }
            Typing::Width => self.typing = Some((Typing::Height(n), String::new())),
            Typing::Height(w) => {
                ctx.write(|c| {
                    let s = &mut *c.settings;
                    (s.width, s.height) = punktfunk_core::resolutions::custom(w, n, &s.codec);
                    s.match_window = false;
                    if c.device.platform == crate::platform::Platform::Android {
                        set_extra_bool(s, device_keys::SAFE_AREA_MODE, false);
                    }
                    true
                });
            }
        }
    }

    fn field_menu(&mut self, ev: MenuEvent, ctx: &mut Ctx) -> Option<MenuPulse> {
        let (_, text) = self.typing.as_mut()?;
        let (entry, pulse) = self
            .keyboard
            .edit_menu(ev, ctx.device.deck, text, Self::admits);
        if entry != Entry::Stay {
            self.commit_field(ctx);
        }
        pulse
    }

    /// The catalog as the store has it now, on the Presets tab.
    fn sync_presets(&mut self, ctx: &Ctx) {
        if self.tab != PRESETS_TAB || (ctx.t - self.presets_at).abs() < 0.5 {
            return;
        }
        self.presets_at = ctx.t;
        let presets = ctx.store.presets();
        if presets != self.presets {
            self.presets = presets;
            self.overrides = ctx.store.preset_overrides();
            self.list.cursor = self.list.cursor.min(self.presets.len());
        }
    }

    /// Filtered by [`row_on`] / [`row_applies`], and [`advanced`] rows by Show advanced: a tab
    /// hiding changed ones ends on [`RowId::AdvancedChanged`]. Presets comes from the catalog.
    fn row_ids(&self, ctx: &Ctx) -> Vec<RowId> {
        if self.tab != PRESETS_TAB {
            let offered = TABS[self.tab]
                .1
                .iter()
                .copied()
                .filter(|id| row_on(*id, ctx.device.platform) && row_applies(*id, ctx))
                // The Mac's picker stands in for the toggle, which presets keep.
                .filter(|id| !(*id == RowId::Fullscreen && is_mac(ctx)));
            if ctx.settings.show_advanced {
                return offered.collect();
            }
            let mut rows: Vec<RowId> = offered.filter(|id| !advanced(*id)).collect();
            if !changed_advanced(self.tab, ctx).is_empty() {
                rows.push(RowId::AdvancedChanged);
            }
            return rows;
        }
        if self.presets.is_empty() {
            vec![RowId::NewPreset]
        } else {
            (0..self.presets.len())
                .map(RowId::Preset)
                .chain([RowId::NewPreset])
                .collect()
        }
    }

    /// [`row_spec`], with the facts a row cannot know alone: how many hidden rows changed, and
    /// the Advanced heading over the first advanced row the tab shows.
    fn spec(&self, id: RowId, ids: &[RowId], ctx: &Ctx) -> RowSpec {
        let mut spec = row_spec(id, ctx, &self.presets, &self.overrides);
        if id == RowId::AdvancedChanged {
            spec.label = match changed_advanced(self.tab, ctx).len() {
                1 => "1 advanced setting changed".into(),
                n => format!("{n} advanced settings changed"),
            };
        } else if advanced(id) && ids.iter().find(|r| advanced(**r)) == Some(&id) {
            spec.header = Some("Advanced");
        }
        spec
    }

    /// Pull the cursor back. The smoothness buffer (and other writers) can shrink the list
    /// between frames.
    fn clamp_cursor(&mut self, len: usize) {
        if self.list.cursor >= len {
            self.list.jump_to(len.saturating_sub(1));
        }
    }

    #[cfg(test)]
    pub(crate) fn strip_focus_for_test(&self) -> bool {
        self.strip_focus
    }

    /// The section strip above the rows and the explainer band under them: the shell's
    /// trays run in this far so the rows bleed under both on one ramp. The keyboard
    /// lifts the bottom reach away, or the tray would slab the keys.
    pub(crate) fn pinned(&self, k: f64) -> (f32, f32) {
        let bottom = DETAIL_H * k * (1.0 - self.seat.min(1.0));
        ((TAB_STRIP_H * k) as f32, bottom as f32)
    }

    fn tray_h(&self, k: f64) -> f64 {
        if self.seat > 0.0 {
            (Keyboard::tray_height() + 12.0) * k * self.seat
        } else {
            0.0
        }
    }

    /// The rows' band: under the section strip, above the explainer and the keyboard.
    fn list_rect(&self, rect: Rect, k: f64) -> Rect {
        Rect::from_ltrb(
            rect.left,
            rect.top + (TAB_STRIP_H * k) as f32,
            rect.right,
            rect.bottom - (DETAIL_H * k + self.tray_h(k)) as f32,
        )
    }

    /// Down from the shell's tabs lands on the section strip, not the rows under it.
    pub(crate) fn enter_from_top(&mut self) {
        self.strip_focus = true;
    }

    /// OK went down on the focused row: it dips before the release acts.
    pub(crate) fn press(&mut self) {
        if self.strip_focus {
            self.strip.press();
        } else if self.typing.is_none() {
            self.list.dip();
        }
    }

    #[cfg(test)]
    pub(crate) fn tab_for_test(&self) -> usize {
        self.tab
    }

    /// Last-drawn row rect — hit tests press real coordinates.
    #[cfg(test)]
    pub(crate) fn row_rect_for_test(&self, i: usize) -> Option<Rect> {
        self.list.row_rect(i)
    }

    fn switch_tab(&mut self, delta: i32, ctx: &Ctx) -> Option<MenuPulse> {
        let n = TABS.len() as i32;
        self.show_tab((self.tab as i32 + delta).rem_euclid(n) as usize, ctx)
    }

    /// Park the outgoing tab's cursor, then jump. Pointer pills name a tab outright.
    fn show_tab(&mut self, tab: usize, ctx: &Ctx) -> Option<MenuPulse> {
        if tab >= TABS.len() {
            return None;
        }
        self.tab_cursors[self.tab] = self.list.cursor;
        self.tab = tab;
        // Remembered cursor can outlive a shorter tab (Presets catalog, smoothness buffer).
        let len = self.row_ids(ctx).len();
        self.list
            .jump_to(self.tab_cursors[self.tab].min(len.saturating_sub(1)));
        Some(MenuPulse::Move)
    }

    /// Strip first: pills sit above the list, so a press there is never a row.
    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        if let Some((_, text)) = self.typing.as_mut().filter(|_| !ctx.device.deck) {
            let Some(entry) = self.keyboard.edit_pointer(p, text, Self::admits) else {
                return false;
            };
            if entry != Entry::Stay {
                self.commit_field(ctx);
            }
            return true;
        }
        if let Some(tab) = self.strip.pointer(p) {
            if p.press() {
                self.show_tab(tab, ctx);
            }
            return true;
        }
        // A press on the rows takes D-pad focus back from the strip.
        if p.press() {
            self.strip_focus = false;
        }
        let ids = self.row_ids(ctx);
        self.clamp_cursor(ids.len());
        let (msg, pulse) = self.list.pointer(p, ids.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.apply_row(msg, pulse, &ids, ctx, fx);
        true
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        if self.typing.is_some() {
            return self.field_menu(ev, ctx);
        }
        if self.strip_focus {
            return self.sections_menu(ev, ctx, fx);
        }
        match ev {
            MenuEvent::JumpBack => return self.switch_tab(-1, ctx),
            MenuEvent::JumpForward => return self.switch_tab(1, ctx),
            // Back and Up from row 0 focus the sections, not a boundary.
            MenuEvent::Back => {
                self.strip_focus = true;
                return Some(MenuPulse::Move);
            }
            MenuEvent::Move(MenuDir::Up) if self.list.cursor == 0 => {
                self.strip_focus = true;
                return Some(MenuPulse::Move);
            }
            _ => {}
        }
        let ids = self.row_ids(ctx);
        self.clamp_cursor(ids.len());
        // Y opens the typed bitrate or size. Not the bitrate under PyroWave: the row is inert.
        if ev == MenuEvent::Secondary {
            let typing = match ids.get(self.list.cursor) {
                Some(RowId::Bitrate) if ctx.settings.codec != "pyrowave" => Typing::Bitrate,
                Some(RowId::Resolution) => Typing::Width,
                _ => return None,
            };
            self.typing = Some((typing, String::new()));
            return Some(MenuPulse::Confirm);
        }
        let (msg, pulse) = self.list.menu(ev, ids.len());
        self.apply_row(msg, pulse, &ids, ctx, fx)
    }

    /// The D-pad on the section strip: Left/Right walk it, wrapping; Down returns to the
    /// rows; Up is the shell's tab strip.
    fn sections_menu(&mut self, ev: MenuEvent, ctx: &Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        match ev {
            MenuEvent::Back => {
                fx.pop();
                None
            }
            MenuEvent::JumpBack => self.switch_tab(-1, ctx),
            MenuEvent::JumpForward => self.switch_tab(1, ctx),
            MenuEvent::Confirm => {
                self.strip_focus = false;
                Some(MenuPulse::Move)
            }
            MenuEvent::Move(MenuDir::Down) => {
                self.strip_focus = false;
                Some(MenuPulse::Move)
            }
            MenuEvent::Move(MenuDir::Left) => self.switch_tab(-1, ctx),
            MenuEvent::Move(MenuDir::Right) => self.switch_tab(1, ctx),
            MenuEvent::Move(_) => Some(MenuPulse::Boundary),
            _ => None,
        }
    }

    /// Shared by pad and pointer so a click and an A press cannot drift apart.
    fn apply_row(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        ids: &[RowId],
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        // List shrank between clamp and here: drop the keypress, do not panic.
        let Some(&focused) = ids.get(self.list.cursor) else {
            return pulse;
        };
        // Presets navigate; they must not hit the settings save path.
        match focused {
            RowId::Preset(i) => {
                return match msg {
                    ListMsg::Activate => {
                        let (id, name) = self.presets[i].clone();
                        fx.push(Screen::PresetMenu(super::preset::PresetMenu::new(id, name)));
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            RowId::NewPreset => {
                return match msg {
                    ListMsg::Activate => {
                        fx.push(Screen::PresetName(super::preset::PresetName::new()));
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            RowId::Version => {
                return match msg {
                    ListMsg::Adjust(_) | ListMsg::Activate => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            RowId::LibrarySections => {
                return match msg {
                    ListMsg::Activate => {
                        fx.push(Screen::Customize(super::library::CustomizeScreen::new()));
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            RowId::Palette => {
                return match msg {
                    ListMsg::Activate => {
                        fx.push(Screen::Palette(super::palette::PaletteScreen::new(
                            &ctx.settings.ui_palette,
                        )));
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            // Action row: adjust is a boundary.
            RowId::QuickActions => {
                return match msg {
                    ListMsg::Activate => {
                        fx.push(Screen::RingEditor(Box::new(
                            super::ring_editor::RingEditorScreen::new(ctx),
                        )));
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            RowId::Controllers => {
                return match msg {
                    ListMsg::Activate => {
                        fx.tab = Some(crate::shell::Tab::Players);
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            // Shows the advanced rows and lands on the first changed one.
            RowId::AdvancedChanged => {
                return match msg {
                    ListMsg::Activate => {
                        let first = changed_advanced(self.tab, ctx).first().copied();
                        ctx.write(|c| {
                            c.settings.show_advanced = true;
                            true
                        });
                        let ids = self.row_ids(ctx);
                        if let Some(i) = first.and_then(|id| ids.iter().position(|r| *r == id)) {
                            self.list.jump_to(i);
                        }
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            RowId::StreamControls => {
                return match msg {
                    ListMsg::Activate => {
                        if let Some(screen) = super::licenses::LicensesScreen::controls(ctx) {
                            fx.push(Screen::Licenses(screen));
                        }
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            // The console draws the licences with the host's sections.
            RowId::Licenses => {
                return match msg {
                    ListMsg::Activate => {
                        let screen = super::licenses::LicensesScreen::new(fx);
                        fx.push(Screen::Licenses(screen));
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            _ => {}
        }
        // Cursor moves must not touch the disk.
        match msg {
            ListMsg::Adjust(delta) => {
                if ctx.write(|c| adjust(focused, delta, false, c)) {
                    Some(MenuPulse::Move)
                } else {
                    Some(MenuPulse::Boundary)
                }
            }
            ListMsg::Activate => {
                ctx.write(|c| adjust(focused, 1, true, c));
                pulse
            }
            ListMsg::None => pulse,
        }
    }

    /// What a screen reader speaks: the section while the strip holds focus, otherwise the
    /// focused row's label and the value drawn beside it.
    pub(crate) fn announcement(&self, ctx: &Ctx) -> Option<String> {
        if self.strip_focus {
            return Some(format!("{} section", TABS[self.tab].0));
        }
        let ids = self.row_ids(ctx);
        let row = self.spec(*ids.get(self.list.cursor)?, &ids, ctx);
        Some(match row.value {
            Some(value) => format!("{}, {}", row.label, value),
            None => row.label,
        })
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if let Some((typing, _)) = &self.typing {
            let done = if *typing == Typing::Width {
                "Next"
            } else {
                "Done"
            };
            return entry_hints(ctx.device.deck, done);
        }
        // Strip-focused: hints describe the D-pad, not the rows.
        if self.strip_focus {
            return vec![
                Hint::new(HintKey::Adjust, "Section"),
                Hint::new(HintKey::Confirm, "Rows"),
                Hint::new(HintKey::Back, "Done"),
            ];
        }
        let ids = self.row_ids(ctx);
        let mut hints = vec![Hint::new(HintKey::Shoulders, "Section")];
        hints.extend(match ids.get(self.list.cursor) {
            Some(RowId::Preset(_)) => vec![
                Hint::new(HintKey::Confirm, "Options\u{2026}"),
                Hint::new(HintKey::Back, "Done"),
            ],
            Some(RowId::NewPreset) => vec![
                Hint::new(HintKey::Confirm, "Create\u{2026}"),
                Hint::new(HintKey::Back, "Done"),
            ],
            Some(RowId::Version) | None => {
                vec![Hint::new(HintKey::Back, "Done")]
            }
            Some(
                RowId::Controllers
                | RowId::StreamControls
                | RowId::Licenses
                | RowId::LibrarySections
                | RowId::Palette
                | RowId::AdvancedChanged,
            ) => vec![
                Hint::new(HintKey::Confirm, "Open"),
                Hint::new(HintKey::Back, "Done"),
            ],
            // Inert under PyroWave (`row_spec`): no Adjust hint.
            Some(RowId::Bitrate) if ctx.settings.codec == "pyrowave" => {
                vec![Hint::new(HintKey::Back, "Done")]
            }
            Some(RowId::Bitrate) => vec![
                Hint::new(HintKey::Adjust, "Adjust"),
                Hint::new(HintKey::Secondary, "Type a rate"),
                Hint::new(HintKey::Back, "Done"),
            ],
            Some(RowId::Resolution) => vec![
                Hint::new(HintKey::Adjust, "Adjust"),
                Hint::new(HintKey::Secondary, "Type a size"),
                Hint::new(HintKey::Back, "Done"),
            ],
            Some(_) => vec![
                Hint::new(HintKey::Adjust, "Adjust"),
                Hint::new(HintKey::Confirm, "Change"),
                Hint::new(HintKey::Back, "Done"),
            ],
        });
        hints
    }

    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        self.seat = self
            .keyboard
            .seat(self.typing.is_some() && !ctx.device.deck, dt);
        self.sync_presets(ctx);
        let list_rect = self.list_rect(rect, k);
        let ids = self.row_ids(ctx);
        self.clamp_cursor(ids.len());
        let mut rows: Vec<RowSpec> = ids.iter().map(|id| self.spec(*id, &ids, ctx)).collect();
        // Field-open: the row being typed shows the digits so far and the caret.
        if let Some((typing, text)) = self.typing.as_ref() {
            let (row, value) = match typing {
                Typing::Bitrate if text.is_empty() => (RowId::Bitrate, "Mbps".into()),
                Typing::Bitrate => (RowId::Bitrate, format!("{text} Mbps")),
                Typing::Width if text.is_empty() => (RowId::Resolution, "Width".into()),
                Typing::Width => (RowId::Resolution, format!("{text} × \u{2026}")),
                Typing::Height(w) if text.is_empty() => {
                    (RowId::Resolution, format!("{w} × height"))
                }
                Typing::Height(w) => (RowId::Resolution, format!("{w} × {text}")),
            };
            if let Some(i) = ids.iter().position(|id| *id == row) {
                rows[i].value = Some(value);
                rows[i].value_dim = text.is_empty();
                rows[i].caret = true;
            }
        }
        // Rows run on under the section strip and the explainer, on the shell's trays;
        // with the keyboard up they stay in their band, or a tray would slab the keys.
        self.list.bleed = self.seat == 0.0;
        self.list.render(
            canvas,
            list_rect,
            &rows,
            fonts,
            k,
            dt,
            // No row focus ring while the tray or the strip holds it.
            self.typing.is_none() && !self.strip_focus,
        );
        if self.seat > 0.0 {
            self.keyboard.render(
                canvas,
                fonts,
                f64::from(rect.width()),
                f64::from(rect.bottom),
                self.seat,
                k,
            );
        }
    }

    /// The section tabs on the margin, under the shell's tabs, and the explainer on the
    /// rows' inner column. The shell draws these over its trays, after [`Self::render`].
    pub(crate) fn render_pinned(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &Ctx,
    ) {
        let list_rect = self.list_rect(rect, k);
        let col = column(list_rect, k);
        let inner = f64::from(col.left) + 16.0 * k;
        let labels: Vec<&str> = TABS.iter().map(|(name, _)| *name).collect();
        self.strip.render(
            canvas,
            Rect::from_ltrb(rect.left, rect.top, rect.right, list_rect.top),
            &labels,
            self.tab,
            self.strip_focus,
            fonts,
            k,
            dt,
        );
        let ids = self.row_ids(ctx);
        let focused = ids.get(self.list.cursor).copied();
        let detail = focused.map_or("", |id| detail(id, ctx));
        // The explainer under the list, led by the row's mark; above the keyboard when up.
        crate::widgets::Foot {
            detail: Some(detail),
            mark: focused.map(row_icon),
            ..Default::default()
        }
        .paint(
            canvas,
            fonts,
            Rect::from_ltrb(rect.left, list_rect.bottom, rect.right, rect.bottom),
            (inner, f64::from(col.right)),
            k,
        );
    }
}

/// Whether this platform has the row at all.
///
/// [`TABS`] is the union so a setting sits under the same word on every client.
/// A concept the platform does not have is absent, never a no-op control.
pub fn row_on(id: RowId, platform: crate::platform::Platform) -> bool {
    use crate::platform::Platform;
    // Rows name the platforms that OFFER them. "Everything except the other one's rows" is
    // well defined for two platforms and ambiguous for three: a new host would inherit every
    // row nobody weighed it against. Unlisted here means universal, as it always did.
    use Platform::{Android, Apple, Desktop, WebOS};
    let on: &[Platform] = match id {
        // Phone sensors: hardware a TV does not have. Apple keeps them for the iPhone and
        // iPad (`rumbleOnDevice`, `gyroFromDevice`); an Apple TV has no sensor to report.
        RowId::PhoneRumble | RowId::PhoneGyro => &[Android, Apple],
        // Apple asks before it captures a Steam Controller 2 (`sc2Capture`); Android captures
        // one whenever it is present.
        RowId::Sc2Passthrough => &[Apple],
        // The weak-GPU row. On Android it also shrinks the surface; webOS's compositor
        // already hands a 1080p buffer, so there it is the cheaper backdrop alone.
        RowId::ReduceUiResolution => &[Android, WebOS],
        // A MediaCodec decoder flag; nothing else has the knob.
        RowId::LowLatency => &[Android],
        // Only Android has a second screen to use (a handheld's lower panel, a half-open fold).
        RowId::SecondScreen => &[Android],
        // The clients whose presenters place the picture through `video_fit`.
        RowId::VideoFit => &[Desktop, Android, Apple],
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
        | RowId::GuideGesture
        | RowId::Touch => &[Desktop, Android, Apple, Platform::Web, Platform::Tizen],
        // DualSense voice coils and speaker: no Apple or browser client plays them.
        RowId::PadHaptics | RowId::PadSpeaker => &[Desktop, Android, WebOS],
        // The clients whose rumble paths read the switch.
        RowId::PadRumble => &[Desktop, Android, Apple],
        // Every client binds something mid-stream: keys, chords, or a remote's colour buttons.
        RowId::StreamControls => &[
            Desktop,
            Android,
            WebOS,
            Platform::Web,
            Apple,
            Platform::Tizen,
        ],
        // Every client ships third-party code. The browser build has no bundle to list.
        RowId::Licenses => &[Desktop, Android, WebOS, Apple],
        // DualSense capture — the pad reaches webOS over Bluetooth HID, not hidraw, so the
        // concept is real there too (punktfunk-webos docs/NOTES.md).
        RowId::DsCapture => &[Android, WebOS],
        // Apple reads `UIAccessibility.isReduceMotionEnabled` and follows it, so the shell has
        // nothing to ask. The others carry a row because they cannot see the OS switch.
        RowId::ReduceMotion => &[Desktop, Android, WebOS, Platform::Web, Platform::Tizen],
        // Which pad is player 1 — a question only a client that can narrow forwarding to one pad
        // has to answer. Android's router, webOS's slot table and the browser's Gamepad API give
        // every controller its own wire slot, so there is nothing to pick.
        RowId::Pad => &[Desktop],
        // That client's own audio plane and its remote's missing second button.
        RowId::AudioRoute | RowId::CursorGestures => &[WebOS],
        RowId::FullscreenMode => &[Apple],
        // Main10 at BT.709 asks nothing of the panel, and MediaCodec and NDL both decode it from
        // the SPS.
        RowId::TenBitSdr => &[Desktop, Android, WebOS, Apple],
        // VideoToolbox and the Metal wavelet path follow the codec, so Apple picks no decoder
        // either. The TV decodes through NDL and the browser through WebCodecs.
        RowId::Decoder => &[Desktop],
        // Chroma and the window-manager knobs: the TV has no window manager and the browser
        // binds no system chord, while Android pins a fixed mode on purpose (`allow_vrr`).
        RowId::Chroma444
        | RowId::Vsync
        | RowId::AllowVrr
        | RowId::Fullscreen
        | RowId::Shortcuts => &[Desktop, Apple],
        _ => &Platform::ALL,
    };
    on.contains(&platform)
}

/// Offered this frame, as opposed to offered-but-inert.
///
/// Echo cancel and pad rows dim under a parent switch so the relationship stays
/// visible. Smoothness buffer is a knob on one of two intents — under Lowest
/// latency the quantity does not exist, so the row is dropped. It sits directly
/// below the intent row so the cursor is never on a row that vanishes.
pub fn row_applies(id: RowId, ctx: &Ctx) -> bool {
    let apple = ctx.device.platform == crate::platform::Platform::Apple;
    match id {
        // The Apple app reads these on the Mac alone, as its own settings offer them.
        RowId::Vsync | RowId::Shortcuts | RowId::Mouse if apple => is_mac(ctx),
        // An Apple TV has no touchscreen, microphone, scroll wheel or variable refresh.
        RowId::Touch | RowId::Mic | RowId::EchoCancel | RowId::InvertScroll | RowId::AllowVrr
            if apple =>
        {
            !ctx.device.tv
        }
        RowId::SmoothBuffer => ctx.settings.present_priority == "smooth",
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
        Platform::Desktop | Platform::WebOS | Platform::Web | Platform::Tizen => {}
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
    };
    ids.iter()
        .copied()
        .filter(|id| row_spec_base(*id, &under, &[]).value != row_spec_base(*id, ctx, &[]).value)
        .collect()
}

/// This tab's advanced rows, offered here and now, that hold a changed value.
fn changed_advanced(tab: usize, ctx: &Ctx) -> Vec<RowId> {
    let offered: Vec<RowId> = TABS[tab]
        .1
        .iter()
        .copied()
        .filter(|id| advanced(*id) && row_on(*id, ctx.device.platform) && row_applies(*id, ctx))
        .collect();
    changed(&offered, ctx)
}

/// Apple with no handheld screen and no TV.
fn is_mac(ctx: &Ctx) -> bool {
    ctx.device.platform == crate::platform::Platform::Apple
        && ctx.device.screen.is_none()
        && !ctx.device.tv
}

/// Where a launch will actually land, named. Not the stored value: with no default host
/// every setting resolves to the list, and the row says so rather than promising a shelf.
fn start_in_value(ctx: &Ctx) -> String {
    let known = ctx.store.known_hosts();
    let want = start::StartIn::parse(&ctx.settings.start_in);
    match start::default_host(ctx.settings, &known) {
        _ if want == start::StartIn::Hosts => "Host list".into(),
        Some(i) => format!("{} · {}", want.label(), known.hosts[i].name),
        None => "Host list (no default host)".into(),
    }
}

/// The row as drawn. A row a known host's bound preset overrides carries the dot and a
/// note naming preset, host and the value that host streams at: the row keeps showing
/// the global, which is what the console edits.
pub fn row_spec(
    id: RowId,
    ctx: &Ctx,
    presets: &[(String, String)],
    overrides: &std::collections::HashMap<String, SettingsOverlay>,
) -> RowSpec {
    let mut spec = row_spec_base(id, ctx, presets);
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
        };
        let value = row_spec_base(id, &under, presets).value.unwrap_or_default();
        spec.dot = true;
        spec.note = Some(format!(
            "Preset \u{201c}{preset}\u{201d} on {host}: {value}"
        ));
    }
    spec
}

/// The row's Lucide mark.
fn row_icon(id: RowId) -> &'static str {
    match id {
        RowId::Aspect | RowId::RenderScale | RowId::Fullscreen | RowId::FullscreenMode => {
            "maximize"
        }
        RowId::Resolution | RowId::ReduceUiResolution => "monitor",
        RowId::Refresh | RowId::Vsync | RowId::AllowVrr => "refresh-cw",
        RowId::Bitrate | RowId::PadRumble | RowId::PadHaptics | RowId::PhoneRumble => "activity",
        RowId::VideoFit => "square",
        RowId::Compositor => "panel-right",
        RowId::Codec => "film",
        RowId::Hdr => "sun",
        RowId::PresentPriority | RowId::SmoothBuffer | RowId::LowLatency => "clock",
        RowId::Decoder | RowId::AudioRoute => "cpu",
        RowId::Chroma444 | RowId::TenBitSdr => "eye",
        RowId::Audio | RowId::AudioFormat | RowId::KeepHostAudio | RowId::PadSpeaker => "volume-2",
        RowId::Mic => "mic",
        RowId::EchoCancel => "mic-off",
        RowId::PadForward
        | RowId::Pad
        | RowId::PadType
        | RowId::Sc2Passthrough
        | RowId::DsCapture
        | RowId::Controllers => "gamepad-2",
        RowId::SystemButtons | RowId::GuideGesture => "house",
        RowId::PhoneGyro => "rotate-cw",
        RowId::SecondScreen => "monitor",
        RowId::Touch | RowId::CursorGestures => "pointer",
        RowId::Mouse => "mouse",
        RowId::QuickActions => "ellipsis",
        RowId::InvertScroll => "undo-2",
        RowId::Shortcuts => "keyboard",
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
        RowId::StreamControls => "keyboard",
        RowId::ShowAdvanced | RowId::AdvancedChanged => "wrench",
        RowId::StatsSize => "chart-column",
        RowId::ExitHint => "log-out",
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

/// Whether an overlay pins the value this row shows.
/// The overlay field a preset stores for this row, by its serialised name
/// ([`SettingsOverlay::clear`]); `None` for a row no preset carries. Mirrors [`overrides_row`].
pub(crate) fn preset_field(id: RowId) -> Option<&'static str> {
    Some(match id {
        RowId::Resolution | RowId::Aspect => "resolution",
        RowId::Refresh => "refresh_hz",
        RowId::RenderScale => "render_scale",
        RowId::VideoFit => "video_fit",
        RowId::Bitrate => "bitrate_kbps",
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
        RowId::GuideGesture => "guide_gesture",
        RowId::Touch => "touch_mode",
        RowId::Mouse => "mouse_mode",
        RowId::InvertScroll => "invert_scroll",
        RowId::Shortcuts => "inhibit_shortcuts",
        RowId::Stats => "stats_verbosity",
        RowId::Fullscreen => "fullscreen_on_stream",
        _ => return None,
    })
}

/// The rows a preset can hold on this platform this frame, each with its section's name.
pub(crate) fn preset_rows(ctx: &Ctx) -> Vec<(&'static str, RowId)> {
    (TABS.iter())
        .flat_map(|(tab, rows)| rows.iter().map(move |id| (*tab, *id)))
        .filter(|(_, id)| preset_field(*id).is_some())
        .filter(|(_, id)| row_on(*id, ctx.device.platform) && row_applies(*id, ctx))
        .collect()
}

pub(crate) fn overrides_row(id: RowId, o: &SettingsOverlay) -> bool {
    match id {
        RowId::Resolution | RowId::Aspect => {
            o.width.is_some() || o.height.is_some() || o.match_window.is_some()
        }
        RowId::Refresh => o.refresh_hz.is_some(),
        RowId::RenderScale => o.render_scale.is_some(),
        RowId::VideoFit => o.video_fit.is_some(),
        RowId::Bitrate => o.bitrate_kbps.is_some(),
        RowId::Compositor => o.compositor.is_some(),
        RowId::Codec => o.codec.is_some(),
        RowId::Hdr => o.hdr_enabled.is_some(),
        RowId::Chroma444 => o.enable_444.is_some(),
        RowId::TenBitSdr => o.ten_bit_sdr.is_some(),
        RowId::PresentPriority => o.present_priority.is_some(),
        RowId::SmoothBuffer => o.smooth_buffer.is_some(),
        RowId::Vsync => o.vsync.is_some(),
        RowId::AllowVrr => o.allow_vrr.is_some(),
        RowId::Audio => o.audio_channels.is_some(),
        RowId::AudioFormat => o.audio_format.is_some(),
        RowId::KeepHostAudio => o.keep_host_audio.is_some(),
        RowId::Mic => o.mic_enabled.is_some(),
        RowId::EchoCancel => o.echo_cancel.is_some(),
        RowId::PadForward => o.gamepad_forwarding.is_some(),
        RowId::PadType => o.gamepad.is_some(),
        RowId::SystemButtons => o.system_buttons.is_some(),
        RowId::GuideGesture => o.guide_gesture.is_some(),
        RowId::Touch => o.touch_mode.is_some(),
        RowId::Mouse => o.mouse_mode.is_some(),
        RowId::InvertScroll => o.invert_scroll.is_some(),
        RowId::Shortcuts => o.inhibit_shortcuts.is_some(),
        RowId::Stats => o.stats_verbosity.is_some(),
        RowId::Fullscreen => o.fullscreen_on_stream.is_some(),
        _ => false,
    }
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
                    0 => "Not pinned".into(),
                    1 => "Pinned to 1 host".into(),
                    n => format!("Pinned to {n} hosts"),
                }),
                value_dim: pins == 0,
                caret: false,
                adjustable: false,
                enabled: true,
                ..RowSpec::default()
            };
        }
        RowId::NewPreset => {
            return RowSpec::action("New preset\u{2026}", true);
        }
        RowId::Controllers => return RowSpec::action("Controllers", true),
        // The count is the tab's; `SettingsScreen::spec` writes it in.
        RowId::AdvancedChanged => return RowSpec::action("Advanced settings changed", true),
        RowId::StreamControls => return RowSpec::action("Stream controls", true),
        RowId::Licenses => return RowSpec::action("Open-source licences", true),
        RowId::QuickActions => return RowSpec::action("Quick actions", true),
        // Opens the cards: the value names the pick, no ‹ › to step it.
        RowId::Palette => {
            return RowSpec {
                label: "Background".into(),
                value: Some(
                    crate::palette::palette(&ctx.settings.ui_palette)
                        .name
                        .into(),
                ),
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
                label: "Library sections".into(),
                value: Some(match on {
                    n if n == all => "All shown".into(),
                    n => format!("{n} of {all} shown"),
                }),
                ..RowSpec::default()
            };
        }
        RowId::Version => {
            return RowSpec {
                label: "Version".into(),
                value: Some(ctx.device.version.clone()),
                ..RowSpec::default()
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
            "Resolution",
            if s.match_window && has_window(ctx.device) {
                "Match window".into()
            } else if safe_area(s, ctx.device.platform) {
                "Native (safe area)".into()
            } else if s.width == 0 {
                "Native".into()
            } else if custom_size(s, &families(ctx.device.screen), ctx.device.platform) {
                format!("Custom ({} × {})", s.width, s.height)
            } else {
                format!("{} × {}", s.width, s.height)
            },
        ),
        RowId::Aspect => {
            let fams = families(ctx.device.screen);
            (
                None,
                "Aspect ratio",
                fams[family(s, &fams, ctx.device.platform)].label.into(),
            )
        }
        RowId::Refresh => (
            None,
            "Refresh rate",
            if s.refresh_hz == 0 {
                "Native".into()
            } else {
                format!("{} Hz", s.refresh_hz)
            },
        ),
        RowId::RenderScale => (
            None,
            "Render scale",
            if s.render_scale == 1.0 {
                "Native".into()
            } else if s.render_scale > 1.0 {
                format!("{}× (supersample)", s.render_scale)
            } else {
                format!("{}×", s.render_scale)
            },
        ),
        RowId::VideoFit => (
            None,
            "Picture fit",
            label_for(&VIDEO_FITS, VideoFit::from_name(&s.video_fit).name()).into(),
        ),
        RowId::Bitrate => (
            None,
            "Bitrate",
            if s.bitrate_kbps == 0 {
                "Automatic".into()
            } else {
                bitrate_label(s.bitrate_kbps)
            },
        ),
        RowId::Compositor => (
            None,
            "Host compositor",
            label_for(&COMPOSITORS, &s.compositor).into(),
        ),
        RowId::Codec => (
            None,
            "Video codec",
            if s.codec == "pyrowave" && !ctx.device.pyrowave_ok {
                "PyroWave (unsupported)".into()
            } else if s.codec == "av1" && !ctx.device.av1_ok {
                "AV1 (unsupported)".into()
            } else {
                label_for(codecs(ctx.device.platform), &s.codec).into()
            },
        ),
        // Migrate before lookup or a legacy store (`vulkan`/`vaapi`) shows "—".
        RowId::Decoder => (
            None,
            "Video decoder",
            label_for(
                &DECODERS,
                &pf_client_core::decoder_pref::migrate_decoder_pref(&s.decoder),
            )
            .into(),
        ),
        RowId::Hdr => (None, "10-bit HDR", on_off(s.hdr_enabled).into()),
        RowId::Chroma444 => (None, "Full chroma (4:4:4)", on_off(s.enable_444).into()),
        RowId::TenBitSdr => (None, "10-bit SDR", on_off(s.ten_bit_sdr).into()),
        RowId::PresentPriority => (
            None,
            "Prioritize",
            label_for(&PRESENT_PRIORITIES, &s.present_priority).into(),
        ),
        RowId::SmoothBuffer => (
            None,
            "Smoothness buffer",
            SMOOTH_BUFFERS
                .iter()
                .find(|(v, _)| *v == s.smooth_buffer)
                .map_or("Automatic", |(_, l)| l)
                .into(),
        ),
        RowId::Vsync => (None, "V-Sync", on_off(s.vsync).into()),
        RowId::AllowVrr => (None, "Follow variable refresh", on_off(s.allow_vrr).into()),
        RowId::Audio => (
            None,
            "Audio channels",
            AUDIO
                .iter()
                .find(|(v, _)| *v == s.audio_channels)
                .map_or("Stereo", |(_, l)| l)
                .into(),
        ),
        RowId::AudioFormat => (
            None,
            "Audio quality",
            audio_format_label(&s.audio_format).into(),
        ),
        RowId::KeepHostAudio => (
            None,
            "Keep host audio playing",
            on_off(s.keep_host_audio).into(),
        ),
        RowId::Mic => (None, "Stream microphone", on_off(s.mic_enabled).into()),
        RowId::EchoCancel => (None, "Echo cancellation", on_off(s.echo_cancel).into()),
        RowId::PadForward => (
            None,
            "Forward controllers",
            on_off(s.gamepad_forwarding).into(),
        ),
        RowId::Pad => (
            None,
            "Use controller",
            if s.forward_pad.is_empty() {
                "Automatic".into()
            } else {
                ctx.pads
                    .iter()
                    .find(|p| p.key == s.forward_pad)
                    .map_or_else(|| "Saved (disconnected)".to_string(), |p| p.name.clone())
            },
        ),
        RowId::PadType => (
            None,
            "Controller type",
            label_for(&PAD_TYPES, &s.gamepad).into(),
        ),
        RowId::SystemButtons => (
            None,
            "Guide button",
            label_for(&SYSTEM_BUTTONS, &s.system_buttons).into(),
        ),
        RowId::GuideGesture => (
            None,
            "Hold Select for guide",
            label_for(&GUIDE_GESTURE, &s.guide_gesture).into(),
        ),
        RowId::PadRumble => (None, "Controller rumble", on_off(s.pad_rumble).into()),
        RowId::PadHaptics => (None, "Controller haptics", on_off(s.pad_haptics).into()),
        RowId::PadSpeaker => (
            None,
            "Controller speaker",
            on_off(pad_speaker_on(&s.pad_speaker)).into(),
        ),
        RowId::Touch => (None, "Touch input", s.touch_mode().label().into()),
        RowId::Mouse => (None, "Mouse input", s.mouse_mode().label().into()),
        RowId::InvertScroll => (
            None,
            "Invert scroll direction",
            on_off(s.invert_scroll).into(),
        ),
        RowId::Shortcuts => (
            None,
            "Capture system shortcuts",
            on_off(s.inhibit_shortcuts).into(),
        ),
        RowId::FollowOsTheme => (
            None,
            "Follow system theme",
            on_off(s.follow_os_theme).into(),
        ),
        // Label is the reduction, so On means it is in effect.
        RowId::ReduceMotion => (None, "Reduce motion", on_off(s.reduce_motion).into()),
        RowId::ReduceUiResolution => (
            None,
            "Reduce interface resolution",
            on_off(reduce_ui_res(
                s,
                ctx.device.platform,
                ctx.device.fallback_ui,
            ))
            .into(),
        ),
        RowId::LibraryView => (
            None,
            "Library view",
            crate::library::LibraryView::parse(&s.library_view)
                .label()
                .into(),
        ),
        RowId::StartIn => (None, "Start in", start_in_value(ctx)),
        RowId::Stats => (
            None,
            "Statistics overlay",
            s.stats_verbosity().label().into(),
        ),
        RowId::AdvancedStats => (None, "Advanced statistics", on_off(s.advanced_stats).into()),
        RowId::ShowAdvanced => (None, "Show advanced", on_off(s.show_advanced).into()),
        RowId::StatsSize => (
            None,
            "Statistics size",
            format!(
                "{} %",
                (punktfunk_core::hud::stats_scale(s.stats_scale_pct) * 100.0).round()
            ),
        ),
        RowId::ExitHint => (None, "Exit hint", on_off(s.exit_hint).into()),
        RowId::HostSort => (None, "Host order", extra()),
        RowId::HostGrouping => (
            None,
            "Group hosts by",
            label_for(&home::HOST_GROUPINGS, host_grouping(s)).into(),
        ),
        RowId::StatsPosition => (
            None,
            "Statistics position",
            s.hud_corner(own_stats_corner(ctx.device.platform))
                .label()
                .into(),
        ),
        RowId::BackgroundKeepAlive => (None, "Keep streaming in background", extra()),
        RowId::BackgroundTimeout => (
            None,
            "Disconnect after",
            label_for(&BACKGROUND_TIMEOUTS, &background_timeout(s).to_string()).into(),
        ),
        RowId::Fullscreen => (
            None,
            "Start streams fullscreen",
            on_off(s.fullscreen_on_stream).into(),
        ),
        RowId::FullscreenMode => (
            None,
            "Fullscreen",
            FULLSCREEN_MODES[fullscreen_mode(s)].into(),
        ),
        RowId::AutoWake => (None, "Auto-wake on connect", on_off(s.auto_wake).into()),
        RowId::LowLatency => (None, "Low-latency mode", extra()),
        RowId::SecondScreen => (None, "Second screen", extra()),
        RowId::PhoneRumble => (Some("This device"), "Rumble on this phone", extra()),
        RowId::PhoneGyro => (None, "Gyro from this phone", extra()),
        RowId::Sc2Passthrough => (None, "Steam Controller 2 passthrough", extra()),
        RowId::DsCapture => (None, "DualSense over USB", extra()),
        RowId::AudioRoute => (None, "Audio processing", extra()),
        RowId::CursorGestures => (None, "Long press to right-click", extra()),
        RowId::GamepadUi => (None, "Controller-optimized UI", extra()),
        RowId::GamepadUiMode => (
            None,
            // Not "Controller UI": that collides with the row above.
            "Show it",
            extra(),
        ),
        RowId::Preset(_)
        | RowId::NewPreset
        | RowId::Controllers
        | RowId::AdvancedChanged
        | RowId::StreamControls
        | RowId::Licenses
        | RowId::QuickActions
        | RowId::LibrarySections
        | RowId::Palette
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
        // PyroWave ignores stored bitrate (session sends 0). Dim; keep the value.
        RowId::Bitrate => s.codec != "pyrowave",
        // Session still drops lossless unless `audio_channels == 2` (before the wire).
        // A live row under surround would change nothing. Delete this arm when that
        // filter learns the frame ladder — not before.
        RowId::AudioFormat => s.audio_channels == 2,
        RowId::Pad
        | RowId::PadType
        | RowId::PadRumble
        | RowId::SystemButtons
        | RowId::GuideGesture
        | RowId::PadHaptics
        | RowId::PadSpeaker => s.gamepad_forwarding,
        _ => true,
    }
}

/// One-line explainer. Platform so Android is not taught desktop-only chords.
pub fn detail(id: RowId, ctx: &Ctx) -> &'static str {
    use crate::platform::Platform;
    let platform = ctx.device.platform;
    match id {
        RowId::Resolution => {
            "The host creates a virtual display at exactly this size — no scaling. \
             Y types any size."
        }
        RowId::Aspect => {
            "Which shapes the Resolution row offers. Picking one moves to its size \
             nearest the current height."
        }
        RowId::Refresh => "Native follows the display this window is on.",
        RowId::RenderScale => {
            "The host renders larger or smaller than the stream mode and this window \
             resamples — above 1× supersamples, below saves bandwidth."
        }
        RowId::VideoFit => {
            "When the stream's shape differs from this window. Fit shows the whole picture \
             with black bars, Crop to fill cuts the edges off, Stretch to fill distorts it."
        }
        RowId::Bitrate if ctx.settings.codec == "pyrowave" => {
            "PyroWave sets its own rate from the stream mode (all-intra) — a fixed bitrate \
             doesn't apply. Pick another codec to use this setting."
        }
        RowId::Bitrate => {
            "Automatic uses the host's default (20 Mbps). Y types an exact rate, up to 2 Gbps."
        }
        RowId::Compositor => {
            "Which compositor drives the virtual output — honored only if available on the host."
        }
        RowId::Codec if ctx.settings.codec == "pyrowave" && !ctx.device.pyrowave_ok => {
            "This device can't decode PyroWave — it needs a Vulkan 1.3 GPU, which most TV \
             boxes don't have. The session streams HEVC instead."
        }
        RowId::Codec if ctx.settings.codec == "av1" && !ctx.device.av1_ok => {
            "This device has no hardware AV1 decoder, so the client never asks for AV1 — \
             the session streams HEVC instead."
        }
        RowId::Codec => "A preference — the host falls back if it can't encode this one.",
        RowId::Decoder => "Automatic picks the best hardware decoder for this GPU, then software.",
        RowId::Hdr => {
            "HDR10 — engages when the host sends HDR content and this display supports it."
        }
        RowId::Chroma444 => {
            "Full-colour video: crisp small text and thin lines, at more bandwidth. \
             Needs an NVIDIA host (NVENC) or the PyroWave codec — other encoders \
             stream 4:2:0 and the session falls back silently."
        }
        RowId::TenBitSdr => {
            "Smoother gradients without HDR — the picture is encoded at 10-bit \
             precision. Needs an NVIDIA or AMD host, or Intel on Linux; HDR takes over \
             when it engages."
        }
        RowId::PresentPriority => {
            "Lowest latency shows each frame the moment the display can take it — a \
             network hiccup becomes an occasional repeated or skipped frame. Smoothness \
             buffers a little to even those out."
        }
        RowId::SmoothBuffer => {
            "Frames held back before showing. Each one absorbs about a refresh of network \
             hiccup and adds a refresh of delay. Automatic holds two."
        }
        RowId::Vsync => {
            "Tear-free. Off removes the wait for the screen's refresh — the lowest \
             possible delay, at the cost of visible tearing. Not every driver offers it; \
             the stats overlay names the mode actually in use."
        }
        RowId::AllowVrr => {
            "On a VRR screen, let the panel refresh in step with the stream instead of on \
             a fixed cadence. Applies to fullscreen sessions; harmless on a fixed screen."
        }
        RowId::Audio => "The speaker layout requested from the host.",
        RowId::AudioFormat => {
            "Bit-exact PCM instead of Opus — 2.3 Mb/s at 48 kHz, 4.6 at 96, off the top of the \
             link. The host has its own switch and stays on Opus if it can't deliver the rate; \
             the stats overlay names what the session got. Stereo only."
        }
        RowId::KeepHostAudio => {
            "The host's own speakers or headphones keep playing while you stream. \
             Both ends hear the same audio; needs a host on 0.32 or newer."
        }
        RowId::Mic => {
            "Send this device's microphone to the host's virtual mic. \
             Ctrl+Alt+Shift+V mutes and unmutes it while streaming."
        }
        RowId::EchoCancel => {
            "Stops the host's audio, playing from this device's speakers, being picked up \
             and sent back. Turn it off if your microphone already runs its own processing."
        }
        RowId::PadForward => {
            "Send controllers connected to this device to the host. Turn it off when your \
             controller already reaches the host another way — USB passthrough such as \
             VirtualHere, or a pad plugged into the host — so games don't see two of them."
        }
        RowId::Pad => "Which pad is forwarded to the host, as player 1.",
        RowId::PadType => {
            "The virtual pad the host creates — Automatic matches this controller. A host's \
             preset can pin another; the row says so."
        }
        RowId::SystemButtons => {
            "Where the guide (Xbox/PS/Steam) and quick-access presses go. Automatic \
             sends them to the host except in Gaming Mode, where Steam on this device \
             reacts to the same press and both overlays would open at once."
        }
        RowId::GuideGesture => {
            "Hold Select on its own to press the host's guide button — keep holding for \
             the host's quick-access menu. Automatic arms it only where the real button \
             can't reach the host. A Select tap still goes through, slightly delayed."
        }
        RowId::PadRumble => {
            "Off, controllers don't vibrate from the stream or in the menus, whatever the game sends."
        }
        RowId::PadHaptics => {
            "Play a DualSense's fine-grained haptics on the pad itself instead of plain \
             rumble. Negotiated — it changes nothing without a capable host and a wired pad."
        }
        RowId::PadSpeaker => {
            "Play the audio a game sends to the controller's own speaker on the pad, \
             not through this device's output."
        }
        RowId::Touch => {
            "How the touchscreen drives the host: Trackpad (relative cursor), \
             Direct pointer (cursor jumps to your finger), or Touch passthrough (raw contacts)."
        }
        RowId::Mouse => match platform {
            Platform::Desktop => {
                "How a physical mouse drives the host: Capture locks the pointer (relative, \
                 for games), Desktop leaves it free and sends absolute positions. \
                 Ctrl+Alt+Shift+M switches live while streaming."
            }
            // No live chord to name: none of these hosts binds one.
            Platform::Android
            | Platform::WebOS
            | Platform::Web
            | Platform::Apple
            | Platform::Tizen => {
                "How a physical mouse drives the host: Capture locks the pointer (relative, \
                 for games), Desktop leaves it free and sends absolute positions."
            }
        },
        RowId::InvertScroll => "Reverses the wheel and trackpad scroll direction sent to the host.",
        RowId::QuickActions => {
            "The ring a two-finger twist or Select+A opens in a stream: what its six buttons \
             hold, and the shortcut chords they can send. Edited on the ring itself."
        }
        RowId::Shortcuts => {
            "Alt+Tab, Super and friends reach the host while input is captured. \
             Off, they act on this device instead."
        }
        RowId::FollowOsTheme => {
            "The console wears your desktop's theme — background, text and accent — and \
             follows a theme switch live. Off, the Background row below picks the look."
        }
        RowId::Palette => {
            "The colour family this backdrop drifts through. Appearance only; nothing \
             about a stream depends on it."
        }
        RowId::ReduceMotion => {
            "Freezes the backdrop and replaces the console's slides and pops with plain \
             fades. Also the gentler choice on an OLED, where a still field can sit for \
             hours."
        }
        RowId::ReduceUiResolution => match platform {
            Platform::WebOS => {
                "Draws the animated backdrop small and steps it slower, so the console \
                 stays smooth on this TV's graphics chip. Nothing about a stream changes \
                 — this is the interface only."
            }
            _ => {
                "Draws the menus at 1080p — the backdrop cheaper, too — and lets the \
                 display scale them up. Text goes a little softer; the console gets much \
                 smoother on a 4K TV or projector, whose graphics chip is far slower than \
                 the panel in front of it. Nothing about a stream changes — this is the \
                 interface only."
            }
        },
        RowId::LibraryView => {
            "Shelf shows one cover at a time, big. Grid shows about eighteen at once — \
             for when you already know what you are looking for. The library's own bar \
             switches it while you browse, along with the sort."
        }
        RowId::StartIn => {
            "Where this app opens. Library lands on your host's shelf, Stream goes \
             straight to its desktop, and Back leaves either one on the host list. \
             With one paired host that host is the default; with several, pick one \
             from its Options menu."
        }
        RowId::Stats => match platform {
            Platform::Desktop => {
                "How much the overlay shows: Compact (one line) → Normal → Detailed. \
                 Ctrl+Alt+Shift+S cycles it live while streaming."
            }
            Platform::Android
            | Platform::WebOS
            | Platform::Web
            | Platform::Apple
            | Platform::Tizen => {
                "How much the overlay shows: Compact (one line) → Normal → Detailed."
            }
        },
        RowId::AdvancedStats => {
            "Off shows the figures Moonlight's overlay also shows. On shows capture to glass as \
             p50/p95 and every stage between. Every number: docs.punktfunk.unom.io/docs/stats"
        }
        RowId::Fullscreen => "Streams open fullscreen instead of windowed.",
        RowId::FullscreenMode => match fullscreen_mode(ctx.settings) {
            0 => "Streams stay in a window.",
            1 => "Streams go fullscreen. The host list returns to a window.",
            _ => "Punktfunk opens fullscreen and stays fullscreen between streams.",
        },
        RowId::StatsPosition => "Which corner the statistics overlay sits in.",
        RowId::StatsSize => "The size of the statistics overlay, on top of your display's scaling.",
        RowId::ExitHint => "Shows how to leave for a few seconds when a stream starts.",
        RowId::ShowAdvanced => "Adds the settings most players never need to change.",
        RowId::AdvancedChanged => "Some hidden settings differ from a fresh install. A shows them.",
        RowId::HostSort => {
            "The order of the host row: as you added them, by name, or most \
             recently connected first."
        }
        RowId::HostGrouping => {
            "Split the host row into bands, each named over its first \
             card: by the preset a card connects with, or online before offline."
        }
        RowId::BackgroundKeepAlive => {
            "Audio and the connection stay live when you switch away; video pauses."
        }
        RowId::BackgroundTimeout if ctx.device.tv => {
            "Ends a session left in the background after this long."
        }
        RowId::BackgroundTimeout => "Ends a backgrounded session so it can't run down the battery.",
        RowId::AutoWake => {
            "Send Wake-on-LAN to a sleeping host before connecting. Turn off for hosts \
             reached over a VPN, where the wake wait only adds delay."
        }
        RowId::LowLatency => {
            "Feeds the decoder slice by slice as frames arrive and marks the media sockets \
             for priority. Off if a decoder shows artefacts under it."
        }
        RowId::SecondScreen => {
            "A smaller second screen — a dual-screen handheld's lower panel, a foldable half \
             open — shows the companion panel, or the picture with Screens. Off leaves it to \
             the system: for a phone on a TV."
        }
        RowId::PhoneRumble => {
            "Also play controller 1's rumble on this phone's own motor — for a clip-on pad \
             with no motor. Costs battery."
        }
        RowId::PhoneGyro => {
            "Send this phone's motion as controller 1's gyro when that pad has none of its \
             own — the on-screen pad included. Only games that read gyro notice."
        }
        RowId::Sc2Passthrough => {
            "Capture the Steam Controller 2 directly (touchpads, gyro, paddles) instead of \
             the generic pad Android shows. Needs the Bluetooth or USB grant."
        }
        RowId::DsCapture => {
            "Capture a wired DualSense directly (touchpad, motion, adaptive triggers). \
             Needs the USB grant when the pad is plugged in."
        }
        // The other UI is named, not called "the other UI": the sentence has to tell
        // the reader where "off" lands, and that is a different place per client.
        RowId::AudioRoute => {
            "Where the stream's audio is decoded. Offload hands the TV's own plane the Opus \
             stream and spends no CPU on it — stereo only, and not every set plays it."
        }
        RowId::CursorGestures => {
            "Hold OK to send a right click, for a remote with no second button. Off, OK stays \
             the plain immediate left click."
        }
        RowId::GamepadUi => match platform {
            // `row_on` offers this to Android, webOS and Apple; Desktop, Web and Tizen are
            // here for exhaustiveness, never to be read.
            Platform::Desktop
            | Platform::Android
            | Platform::Web
            | Platform::Apple
            | Platform::Tizen => {
                "Front the app with this console instead of the touch interface. Off returns \
                 to the touch home immediately — switch it back on there."
            }
            Platform::WebOS => {
                "Front the app with this console instead of the cursor UI. Off returns to \
                 the cursor UI immediately — switch it back on there."
            }
        },
        RowId::GamepadUiMode => match platform {
            Platform::Desktop
            | Platform::Android
            | Platform::Web
            | Platform::Apple
            | Platform::Tizen => {
                "When this console fronts the app: whenever a controller is attached, or \
                 always — for a device that lives docked to a TV. The switch above turns it \
                 off altogether."
            }
            Platform::WebOS => {
                "When this console fronts the app: whenever a controller is connected, or \
                 always. Without one the remote gets the cursor UI, which it points at. \
                 The switch above turns it off altogether."
            }
        },
        RowId::Controllers => "The controllers connected here, their grants and a rumble test.",
        RowId::StreamControls => {
            "The keys, controller chords and gestures that work while you stream."
        }
        RowId::Licenses => "The open-source licences this app ships under.",
        RowId::LibrarySections => "Which sections the Games tab shows, and in what order.",
        RowId::Version => "This console's build.",
        RowId::Preset(_) => {
            "Its settings, its name, and the hosts it is pinned to: pinned, it appears as \
             its own card, and one press connects with these settings."
        }
        RowId::NewPreset => {
            "A preset bundles stream settings for one use, a low-latency one or a quality \
             one, over the settings here. Pin it to a host as a one-press card."
        }
    }
}

/// Mbps below 1 Gbps, Gbps above. Decimal only when rounding would collide
/// (12.5 Mbps, 1.5 Gbps). Off-ladder rates are real (typed field, desktop spinner).
fn bitrate_label(kbps: u32) -> String {
    let unit = |v: f64, suffix: &str| {
        if (v - v.round()).abs() < 0.05 {
            format!("{} {suffix}", v.round())
        } else {
            format!("{v:.1} {suffix}")
        }
    };
    let mbps = f64::from(kbps) / 1000.0;
    if kbps >= 1_000_000 {
        unit(mbps / 1000.0, "Gbps")
    } else {
        unit(mbps, "Mbps")
    }
}

fn on_off(v: bool) -> &'static str {
    if v {
        "On"
    } else {
        "Off"
    }
}

fn label_for<'a>(options: &'a [(&str, &'a str)], value: &str) -> &'a str {
    options
        .iter()
        .find(|(v, _)| *v == value)
        .map_or("—", |(_, l)| l)
}

/// Label for a stored `audio_format`. Unknown → Opus (what the session runs), not
/// [`label_for`]'s "—": a shared catalog can carry a newer client's rung.
fn audio_format_label(value: &str) -> &'static str {
    AUDIO_FORMATS
        .iter()
        .find(|(v, _)| *v == value)
        .or_else(|| AUDIO_FORMATS.iter().find(|(v, _)| *v == AUDIO_FORMAT_OPUS))
        .map_or("", |(_, l)| *l)
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
            // Writers store these literals; a hand-edited oddball snaps to the first step.
            let cur = RENDER_SCALES.iter().position(|v| *v == s.render_scale);
            step_option(cur, RENDER_SCALES.len(), delta, wrap)
                .map(|i| s.render_scale = RENDER_SCALES[i])
        }
        RowId::VideoFit => {
            let cur = VIDEO_FITS
                .iter()
                .position(|(v, _)| *v == VideoFit::from_name(&s.video_fit).name());
            step_option(cur, VIDEO_FITS.len(), delta, wrap)
                .map(|i| s.video_fit = VIDEO_FITS[i].0.to_string())
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
        RowId::Compositor => step_str(&COMPOSITORS, &mut s.compositor, delta, wrap),
        RowId::Codec => step_str(codecs(platform), &mut s.codec, delta, wrap),
        RowId::Decoder => {
            // Migrate first or a legacy value jumps to first/last instead of its neighbour.
            s.decoder = pf_client_core::decoder_pref::migrate_decoder_pref(&s.decoder);
            step_str(&DECODERS, &mut s.decoder, delta, wrap)
        }
        RowId::Hdr => toggle(&mut s.hdr_enabled, delta, wrap),
        RowId::Chroma444 => toggle(&mut s.enable_444, delta, wrap),
        RowId::TenBitSdr => toggle(&mut s.ten_bit_sdr, delta, wrap),
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
                let cur = SMOOTH_BUFFERS
                    .iter()
                    .position(|(v, _)| *v == s.smooth_buffer);
                step_option(cur, SMOOTH_BUFFERS.len(), delta, wrap)
                    .map(|i| s.smooth_buffer = SMOOTH_BUFFERS[i].0)
            } else {
                None
            }
        }
        RowId::Vsync => toggle(&mut s.vsync, delta, wrap),
        RowId::AllowVrr => toggle(&mut s.allow_vrr, delta, wrap),
        RowId::Audio => {
            let cur = AUDIO.iter().position(|(v, _)| *v == s.audio_channels);
            step_option(cur, AUDIO.len(), delta, wrap).map(|i| s.audio_channels = AUDIO[i].0)
        }
        RowId::AudioFormat => step_str(AUDIO_FORMATS, &mut s.audio_format, delta, wrap),
        RowId::KeepHostAudio => toggle(&mut s.keep_host_audio, delta, wrap),
        RowId::Mic => toggle(&mut s.mic_enabled, delta, wrap),
        RowId::EchoCancel => toggle(&mut s.echo_cancel, delta, wrap),
        RowId::PadForward => toggle(&mut s.gamepad_forwarding, delta, wrap),
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
        RowId::GuideGesture => step_str(&GUIDE_GESTURE, &mut s.guide_gesture, delta, wrap),
        RowId::PadRumble => toggle(&mut s.pad_rumble, delta, wrap),
        RowId::PadHaptics => toggle(&mut s.pad_haptics, delta, wrap),
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
        RowId::InvertScroll => toggle(&mut s.invert_scroll, delta, wrap),
        RowId::Shortcuts => toggle(&mut s.inhibit_shortcuts, delta, wrap),
        RowId::Stats => {
            let cur = StatsVerbosity::ALL
                .iter()
                .position(|v| *v == s.stats_verbosity());
            step_option(cur, StatsVerbosity::ALL.len(), delta, wrap)
                .map(|i| s.set_stats_verbosity(StatsVerbosity::ALL[i]))
        }
        RowId::AdvancedStats => toggle(&mut s.advanced_stats, delta, wrap),
        RowId::FollowOsTheme => toggle(&mut s.follow_os_theme, delta, wrap),
        RowId::ReduceMotion => toggle(&mut s.reduce_motion, delta, wrap),
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
        RowId::Fullscreen => toggle(&mut s.fullscreen_on_stream, delta, wrap),
        RowId::FullscreenMode => step_option(Some(fullscreen_mode(s)), 3, delta, wrap).map(|i| {
            set_extra_bool(s, FULLSCREEN_ALWAYS_KEY, i == 2);
            if i < 2 {
                s.fullscreen_on_stream = i == 1;
            }
        }),
        RowId::AutoWake => toggle(&mut s.auto_wake, delta, wrap),
        RowId::LowLatency
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
        RowId::ExitHint => toggle(&mut s.exit_hint, delta, wrap),
        RowId::ShowAdvanced => toggle(&mut s.show_advanced, delta, wrap),
        RowId::BackgroundTimeout => {
            let mut v = background_timeout(s).to_string();
            step_str(&BACKGROUND_TIMEOUTS, &mut v, delta, wrap).map(|()| {
                let minutes: u64 = v.parse().unwrap_or(BACKGROUND_TIMEOUT_DEFAULT);
                s.extra
                    .insert(BACKGROUND_TIMEOUT_KEY.to_string(), minutes.into());
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
        | RowId::StreamControls
        | RowId::Licenses
        | RowId::QuickActions
        | RowId::LibrarySections
        | RowId::Palette
        | RowId::Version => None,
    }
    .is_some()
}

/// Clamp when adjusting, wrap when cycling. Unknown current value snaps to first.
///
/// `pub(super)` so the library view/sort bar shares the boundary thud.
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

fn step_str(options: &[(&str, &str)], value: &mut String, delta: i32, wrap: bool) -> Option<()> {
    let cur = options.iter().position(|(v, _)| v == value);
    step_option(cur, options.len(), delta, wrap).map(|i| *value = options[i].0.to_string())
}

fn toggle(value: &mut bool, delta: i32, wrap: bool) -> Option<()> {
    let target = if wrap { !*value } else { delta > 0 };
    if *value == target {
        None
    } else {
        *value = target;
        Some(())
    }
}

// `pub(crate)` so shell tests outside `screens` share `fake_home`.
#[cfg(test)]
pub(crate) mod tests;
