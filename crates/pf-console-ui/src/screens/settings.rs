//! Console settings: the couch-facing subset of the shared Settings store.
//!
//! One row per setting, grouped by [`TABS`]. Left/right steps the focused value
//! (clamped); A cycles wrapping; L1/R1 change section; B closes. Every change
//! writes the store immediately so desktop shells round-trip the same file.
//! Each tab remembers its cursor. Presets is built from the catalog at render
//! time — the console never creates or edits presets.
//!
//! Tab names match `clients/shared/console-vectors.json`. Platform split:
//! [`row_on`]. Availability this frame: [`row_applies`].

use crate::glyphs::{Hint, HintKey};
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{Ctx, Outbox, Screen};
use crate::theme::{fg, Fonts, W};
use crate::widgets::{
    permits, Charset, KeyMsg, Keyboard, ListMsg, MenuList, RowSpec, TabStrip, TAB_STRIP_H,
};
use pf_client_core::audio_format::{AUDIO_FORMATS, AUDIO_FORMAT_OPUS};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use pf_client_core::presets::SettingsOverlay;
use pf_client_core::start;
use pf_client_core::trust::{MouseMode, StatsVerbosity, TouchMode};
use skia_safe::{Canvas, Rect};

/// Dispatch key for adjust/activate. The pad list under "Use controller" can
/// churn between frames, so an index would act on the wrong row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowId {
    /// Index into [`SettingsScreen::presets`]. Activate opens pin-to-hosts;
    /// the console never edits a preset.
    Preset(usize),
    NoPresets,
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
    /// `trust::Settings::library_collections`. Couch path besides the shelf's Y.
    LibraryCollections,
    /// `trust::Settings::start_in`. The value line names where a launch will land.
    StartIn,
    // Android-only. Values live in `trust::Settings::extra` under `android.*`
    // so the typed struct stays shared; [`row_on`] keeps them off desktop.
    /// Slice-progressive decode plus DSCP. Android-only.
    LowLatency,
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
    /// Which path decodes the stream's audio on a TV — see `WEBOS_AUDIO_ROUTES`. webOS only:
    /// the offload route is that client's NDL audio plane, which no other platform has.
    AudioRoute,
    /// Long-press the remote's OK to send a right click. webOS only — it exists because a
    /// Magic Remote has no second button.
    CursorGestures,
    /// Action row: opens the in-process controllers screen.
    Controllers,
    /// Action row: asks the host to open the platform licences screen.
    Licenses,
}

/// `Settings::extra` keys the Android rows share with Kotlin (`ConsoleJson.settings`).
mod android_keys {
    pub const LOW_LATENCY: &str = "android.low_latency";
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

/// The `extra` key behind [`RowId::ReduceUiResolution`] on this platform. Each TV client
/// persists its own — the names are what Kotlin (`ConsoleJson`) and the webOS store share.
fn reduce_ui_key(platform: crate::platform::Platform) -> &'static str {
    use crate::platform::Platform;
    match platform {
        Platform::WebOS => webos_keys::REDUCE_UI_RES,
        _ => android_keys::REDUCE_UI_RES,
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

// Tab names match Apple/Android (`console-vectors.json`). Presets is empty here:
// its rows come from the catalog. Device pickers stay on the desktop dialogs.
const TABS: [(&str, &[RowId]); 7] = [
    (
        "Stream",
        &[
            RowId::Aspect,
            RowId::Resolution,
            RowId::Refresh,
            RowId::RenderScale,
            RowId::VideoFit,
            RowId::Bitrate,
            RowId::Compositor,
        ],
    ),
    (
        "Video",
        &[
            RowId::Codec,
            RowId::Decoder,
            RowId::LowLatency,
            RowId::Hdr,
            RowId::Chroma444,
            RowId::TenBitSdr,
            RowId::PresentPriority,
            RowId::SmoothBuffer,
            RowId::Vsync,
            RowId::AllowVrr,
        ],
    ),
    (
        "Audio",
        &[
            RowId::Audio,
            RowId::AudioRoute,
            RowId::AudioFormat,
            RowId::KeepHostAudio,
            RowId::Mic,
            RowId::EchoCancel,
        ],
    ),
    (
        "Controller",
        &[
            RowId::PadForward,
            RowId::Pad,
            RowId::PadType,
            RowId::SystemButtons,
            RowId::GuideGesture,
            RowId::PadHaptics,
            RowId::PadSpeaker,
            RowId::PhoneRumble,
            RowId::PhoneGyro,
            RowId::Sc2Passthrough,
            RowId::DsCapture,
            RowId::Controllers,
        ],
    ),
    (
        "Input",
        &[
            RowId::Touch,
            RowId::Mouse,
            RowId::CursorGestures,
            RowId::InvertScroll,
            RowId::Shortcuts,
            RowId::QuickActions,
        ],
    ),
    (
        "Interface",
        &[
            RowId::FollowOsTheme,
            RowId::Palette,
            RowId::ReduceMotion,
            RowId::ReduceUiResolution,
            RowId::LibraryView,
            RowId::LibraryCollections,
            RowId::StartIn,
            RowId::Stats,
            RowId::AdvancedStats,
            RowId::Fullscreen,
            RowId::AutoWake,
            RowId::GamepadUi,
            RowId::GamepadUiMode,
            RowId::Licenses,
        ],
    ),
    ("Presets", &[]),
];

/// Trailing Presets tab — catalog-built, not [`TABS`] rows.
const PRESETS_TAB: usize = TABS.len() - 1;

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

/// Android's Native (safe area) resolution. The flag only counts on a native size.
fn safe_area(s: &pf_client_core::trust::Settings, platform: crate::platform::Platform) -> bool {
    platform == crate::platform::Platform::Android
        && s.width == 0
        && !s.match_window
        && extra_bool(s, android_keys::SAFE_AREA_MODE, false)
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
        | crate::platform::Platform::Web => CUSTOM_MAX_MBPS * 1_000,
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
/// no AV1 (never presented a picture) and no PyroWave (no Vulkan presentation).
fn codecs(platform: crate::platform::Platform) -> &'static [(&'static str, &'static str)] {
    match platform {
        crate::platform::Platform::WebOS => &CODECS[..3],
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
/// Shared `system_buttons` key. Auto sends to the host except in Gaming Mode,
/// where Steam on this device would open a second overlay on the same press.
const SYSTEM_BUTTONS: [(&str, &str); 3] = [
    ("auto", "Automatic"),
    ("forward", "Send to host"),
    ("local", "This device"),
];
const GUIDE_GESTURE: [(&str, &str); 3] = [("auto", "Automatic"), ("on", "On"), ("off", "Off")];

pub(crate) struct SettingsScreen {
    list: MenuList,
    strip: TabStrip,
    tab: usize,
    /// Per-tab cursor so a detour does not reset the one you left.
    tab_cursors: [usize; TABS.len()],
    /// `(id, name)` loaded once. The console cannot create presets, so this is
    /// stable for the screen's lifetime.
    presets: Vec<(String, String)>,
    /// Each preset's overrides by id, loaded with `presets`: the rows say when a
    /// host's bound preset outranks the global value they show.
    overrides: std::collections::HashMap<String, SettingsOverlay>,
    /// D-pad focus on the strip. TV remotes have no shoulders and no Tab key.
    strip_focus: bool,
    /// Typed Mbps while Y has the bitrate field open. Y, not A, so A still cycles.
    custom_bitrate: Option<String>,
    /// Tray keyboard. Unused on Deck: Steam's keyboard types (same as add-host).
    keyboard: Keyboard,
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
            overrides: Default::default(),
            strip_focus: false,
            custom_bitrate: None,
            keyboard: Keyboard::new(),
        }
    }

    /// True while the typed field is open; the run loop keeps SDL text input started.
    pub(crate) fn editing(&self) -> bool {
        self.custom_bitrate.is_some()
    }

    /// SDL text. Digits only; four chars is 2000 Mbps, the ceiling.
    pub(crate) fn text_input(&mut self, text: &str) {
        for ch in text.chars() {
            self.type_char(ch);
        }
    }

    fn type_char(&mut self, ch: char) -> bool {
        let Some(buf) = self.custom_bitrate.as_mut() else {
            return false;
        };
        if !permits(Charset::Digits, ch) || buf.chars().count() >= 4 {
            return false;
        }
        buf.push(ch);
        true
    }

    fn backspace(&mut self) -> bool {
        self.custom_bitrate.as_mut().and_then(String::pop).is_some()
    }

    pub(crate) fn edit_key(&mut self, key: crate::input::Key, ctx: &mut Ctx) -> bool {
        use crate::input::Key as K;
        if self.custom_bitrate.is_none() {
            return false;
        }
        match key {
            K::Backspace => {
                self.backspace();
                true
            }
            K::Return | K::Escape => {
                self.commit_custom(ctx);
                true
            }
            _ => false,
        }
    }

    /// Close the field. Empty or `0` is an abandoned edit, not Automatic (the first rung).
    fn commit_custom(&mut self, ctx: &mut Ctx) {
        let Some(text) = self.custom_bitrate.take() else {
            return;
        };
        let Ok(mbps) = text.parse::<u32>() else {
            return;
        };
        if mbps == 0 {
            return;
        }
        // Rebase first: another writer may have stored the file while the keyboard was up.
        *ctx.settings = ctx.store.load();
        let ceiling_mbps = bitrate_ceiling_kbps(ctx.platform) / 1_000;
        ctx.settings.bitrate_kbps = mbps.min(ceiling_mbps) * 1000;
        ctx.store.save(ctx.settings);
    }

    fn custom_menu(&mut self, ev: MenuEvent, ctx: &mut Ctx) -> Option<MenuPulse> {
        if ctx.deck {
            // Steam types via `text_input`; the pad only commits.
            return match ev {
                MenuEvent::Back | MenuEvent::Confirm => {
                    self.commit_custom(ctx);
                    Some(MenuPulse::Confirm)
                }
                _ => None,
            };
        }
        let (msg, pulse) = self.keyboard.menu(ev);
        match msg {
            KeyMsg::Type(c) => {
                if self.type_char(c) {
                    Some(MenuPulse::Move)
                } else {
                    Some(MenuPulse::Boundary)
                }
            }
            KeyMsg::Backspace => {
                if self.backspace() {
                    Some(MenuPulse::Move)
                } else {
                    Some(MenuPulse::Boundary)
                }
            }
            KeyMsg::Done => {
                self.commit_custom(ctx);
                Some(MenuPulse::Confirm)
            }
            KeyMsg::None => pulse,
        }
    }

    /// Filtered by [`row_on`] / [`row_applies`]. Presets comes from the catalog.
    fn row_ids(&self, ctx: &Ctx) -> Vec<RowId> {
        if self.tab != PRESETS_TAB {
            return TABS[self.tab]
                .1
                .iter()
                .copied()
                .filter(|id| row_on(*id, ctx.platform) && row_applies(*id, ctx))
                .collect();
        }
        if self.presets.is_empty() {
            vec![RowId::NoPresets]
        } else {
            (0..self.presets.len()).map(RowId::Preset).collect()
        }
    }

    /// Pull the cursor back. The smoothness buffer (and other writers) can shrink the list
    /// between frames.
    fn clamp_cursor(&mut self, len: usize) {
        if self.list.cursor >= len {
            self.list.jump_to(len.saturating_sub(1));
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
        if let PointerKind::PanStart { .. } | PointerKind::Pan { .. } | PointerKind::Fling { .. } =
            p.kind
        {
            return self.custom_bitrate.is_none() && self.list.pan(p);
        }
        if self.custom_bitrate.is_some() && !ctx.deck {
            if !self.keyboard.covers(p) {
                if p.press() {
                    self.commit_custom(ctx);
                    return true;
                }
                return false;
            }
            let (msg, _) = self.keyboard.pointer(p);
            match msg {
                KeyMsg::Type(c) => {
                    self.type_char(c);
                }
                KeyMsg::Backspace => {
                    self.backspace();
                }
                KeyMsg::Done => self.commit_custom(ctx),
                KeyMsg::None => {}
            }
            return true;
        }
        if let Some(tab) = self.strip.pointer(p) {
            self.show_tab(tab, ctx);
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
        if self.custom_bitrate.is_some() {
            return self.custom_menu(ev, ctx);
        }
        if self.strip_focus {
            // D-pad tab path for remotes with no shoulders and no Tab key.
            return match ev {
                MenuEvent::Back => {
                    fx.pop();
                    None
                }
                MenuEvent::Move(MenuDir::Left) | MenuEvent::JumpBack => self.switch_tab(-1, ctx),
                MenuEvent::Move(MenuDir::Right) | MenuEvent::JumpForward => self.switch_tab(1, ctx),
                MenuEvent::Move(MenuDir::Down) | MenuEvent::Confirm => {
                    self.strip_focus = false;
                    Some(MenuPulse::Move)
                }
                MenuEvent::Move(MenuDir::Up) => Some(MenuPulse::Boundary),
                _ => None,
            };
        }
        match ev {
            MenuEvent::Back => {
                fx.pop();
                return None;
            }
            MenuEvent::JumpBack => return self.switch_tab(-1, ctx),
            MenuEvent::JumpForward => return self.switch_tab(1, ctx),
            // Up from row 0 focuses the strip, not a boundary.
            MenuEvent::Move(MenuDir::Up) if self.list.cursor == 0 => {
                self.strip_focus = true;
                return Some(MenuPulse::Move);
            }
            _ => {}
        }
        let ids = self.row_ids(ctx);
        self.clamp_cursor(ids.len());
        // Y opens the typed bitrate. Skip under PyroWave: the row is inert (`row_spec`).
        if ev == MenuEvent::Secondary {
            return if ids.get(self.list.cursor) == Some(&RowId::Bitrate)
                && ctx.settings.codec != "pyrowave"
            {
                self.custom_bitrate = Some(String::new());
                Some(MenuPulse::Confirm)
            } else {
                None
            };
        }
        let (msg, pulse) = self.list.menu(ev, ids.len());
        self.apply_row(msg, pulse, &ids, ctx, fx)
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
                        fx.push(Screen::PinHosts(super::pin_hosts::PinHostsScreen::new(
                            id, name,
                        )));
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            RowId::NoPresets => {
                return match msg {
                    ListMsg::Adjust(_) | ListMsg::Activate => Some(MenuPulse::Boundary),
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
            // In-process Skia screen; grant dialogs still go to the host.
            RowId::Controllers => {
                return match msg {
                    ListMsg::Activate => {
                        fx.push(Screen::Controllers(
                            super::controllers::ControllersScreen::new(),
                        ));
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            // Platform screen: A asks the host to open it; nothing here edits.
            RowId::Licenses => {
                return match msg {
                    ListMsg::Activate => {
                        fx.cmds.push(crate::model::ConsoleCmd::OpenPlatformScreen {
                            id: crate::platform::PlatformScreen::Licenses.id().to_string(),
                        });
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            _ => {}
        }
        // Whole-file writer: rebase before mutate or another writer's store is reverted.
        // Cursor moves must not touch the disk.
        if matches!(msg, ListMsg::Adjust(_) | ListMsg::Activate) {
            *ctx.settings = ctx.store.load();
        }
        match msg {
            ListMsg::Adjust(delta) => {
                let changed = adjust(focused, delta, false, ctx);
                if changed {
                    ctx.store.save(ctx.settings);
                    Some(MenuPulse::Move)
                } else {
                    Some(MenuPulse::Boundary)
                }
            }
            ListMsg::Activate => {
                if adjust(focused, 1, true, ctx) {
                    ctx.store.save(ctx.settings);
                }
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
        let row = row_spec(
            *self.row_ids(ctx).get(self.list.cursor)?,
            ctx,
            &self.presets,
            &self.overrides,
        );
        Some(match row.value {
            Some(value) => format!("{}, {}", row.label, value),
            None => row.label,
        })
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if self.custom_bitrate.is_some() {
            if ctx.deck {
                return vec![
                    Hint::new(HintKey::Key("STEAM + X"), "Keyboard"),
                    Hint::new(HintKey::Confirm, "Done"),
                    Hint::new(HintKey::Back, "Done"),
                ];
            }
            return vec![
                Hint::new(HintKey::Confirm, "Type"),
                Hint::new(HintKey::Tertiary, "Delete"),
                Hint::new(HintKey::Back, "Done"),
            ];
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
                Hint::new(HintKey::Confirm, "Pin to hosts…"),
                Hint::new(HintKey::Back, "Done"),
            ],
            Some(RowId::NoPresets) | None => vec![Hint::new(HintKey::Back, "Done")],
            Some(RowId::Controllers | RowId::Licenses) => vec![
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
        // Strip on top, explainer under the list; rows get the band between.
        let detail_h = 34.0 * k;
        let strip_h = TAB_STRIP_H * k;
        let labels: Vec<&str> = TABS.iter().map(|(name, _)| *name).collect();
        self.strip.render(
            canvas,
            Rect::from_ltrb(rect.left, rect.top, rect.right, rect.top + strip_h as f32),
            &labels,
            self.tab,
            self.strip_focus,
            fonts,
            k,
            dt,
        );
        let seat = self
            .keyboard
            .seat(self.custom_bitrate.is_some() && !ctx.deck, dt);
        let tray_h = if seat > 0.0 {
            (Keyboard::tray_height() + 12.0) * k * seat
        } else {
            0.0
        };
        let list_rect = Rect::from_ltrb(
            rect.left,
            rect.top + strip_h as f32,
            rect.right,
            rect.bottom - detail_h as f32 - tray_h as f32,
        );
        let ids = self.row_ids(ctx);
        self.clamp_cursor(ids.len());
        let mut rows: Vec<RowSpec> = ids
            .iter()
            .map(|id| row_spec(*id, ctx, &self.presets, &self.overrides))
            .collect();
        // Field-open: the Bitrate row shows the typed digits and the caret.
        if let (Some(text), Some(i)) = (
            self.custom_bitrate.as_ref(),
            ids.iter().position(|id| *id == RowId::Bitrate),
        ) {
            rows[i].value = Some(if text.is_empty() {
                "Mbps".into()
            } else {
                format!("{text} Mbps")
            });
            rows[i].value_dim = text.is_empty();
            rows[i].caret = true;
        }
        self.list.render(
            canvas,
            list_rect,
            &rows,
            fonts,
            k,
            dt,
            // No row focus ring while the tray or the strip holds it.
            self.custom_bitrate.is_none() && !self.strip_focus,
        );
        let detail = ids
            .get(self.list.cursor)
            .copied()
            .map_or("", |id| detail(id, ctx));
        fonts.centered(
            canvas,
            detail,
            W::Regular,
            13.0 * k,
            fg(0.55),
            f64::from(rect.left) + f64::from(rect.width()) / 2.0,
            f64::from(rect.bottom) - detail_h - tray_h + 6.0 * k,
            f64::from(rect.width()) * 0.8,
        );
        if seat > 0.0 {
            self.keyboard.render(
                canvas,
                fonts,
                f64::from(rect.width()),
                f64::from(rect.bottom),
                seat,
                k,
            );
        }
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
    use Platform::{Android, Desktop, WebOS};
    let on: &[Platform] = match id {
        // Phone sensors and the Steam Controller 2 dongle: hardware a TV does not have.
        RowId::PhoneRumble | RowId::PhoneGyro | RowId::Sc2Passthrough => &[Android],
        // The weak-GPU row. On Android it also shrinks the surface; webOS's compositor
        // already hands a 1080p buffer, so there it is the cheaper backdrop alone.
        RowId::ReduceUiResolution => &[Android, WebOS],
        // A MediaCodec decoder flag; nothing else has the knob.
        RowId::LowLatency => &[Android],
        // The clients whose presenters place the picture through `video_fit`.
        RowId::VideoFit => &[Desktop, Android],
        // Offered wherever there is a second UI to fall back to: Android's touch home,
        // webOS's cursor shell. `row_applies` still needs `fallback_ui` from the host.
        RowId::GamepadUi | RowId::GamepadUiMode => &[Android, WebOS],
        // A pad list and a licences screen: both real on a TV.
        RowId::Controllers | RowId::Licenses => &[Android, WebOS],
        // DualSense capture — the pad reaches webOS over Bluetooth HID, not hidraw, so the
        // concept is real there too (punktfunk-webos docs/NOTES.md).
        RowId::DsCapture => &[Android, WebOS],
        // Which pad is player 1 — a question only a client that can narrow forwarding to one pad
        // has to answer. Android's router, webOS's slot table and the browser's Gamepad API give
        // every controller its own wire slot, so there is nothing to pick.
        RowId::Pad => &[Desktop],
        // That client's own audio plane and its remote's missing second button.
        RowId::AudioRoute | RowId::CursorGestures => &[WebOS],
        // Main10 at BT.709 asks nothing of the panel, and MediaCodec and NDL both decode it from
        // the SPS.
        RowId::TenBitSdr => &[Desktop, Android, WebOS],
        // Decoder choice, chroma and the window-manager knobs: the TV decodes through NDL and has
        // no window manager, so none of these is a control it could obey. The browser is out for
        // the same shape of reason — WebCodecs picks the decoder, a page binds no system chord,
        // and fullscreen needs a gesture. VRR is desktop-only because Android pins a fixed mode
        // on purpose (`trust::Settings::allow_vrr`).
        RowId::Decoder
        | RowId::Chroma444
        | RowId::Vsync
        | RowId::AllowVrr
        | RowId::Fullscreen
        | RowId::Shortcuts => &[Desktop],
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
    match id {
        RowId::SmoothBuffer => ctx.settings.present_priority == "smooth",
        // Needs `fallback_ui`; otherwise off strands the user with no UI.
        RowId::GamepadUi => ctx.fallback_ui,
        // Hidden unless fallback_ui and the switch above is on. Sits below that
        // switch so the cursor is never on a row that vanishes. An Android TV
        // ignores the value (`GamepadUi.kt`: the tv term alone satisfies the OR);
        // webOS obeys it — a Magic Remote with no pad is why its cursor UI exists.
        RowId::GamepadUiMode => ctx.fallback_ui && extra_bool(ctx.settings, GAMEPAD_UI_KEY, true),
        // `os_theme::available()`, not platform: a new publisher needs no edit here.
        RowId::FollowOsTheme => crate::os_theme::available(),
        // Hidden while follow_os_theme; sits below the switch that drops it.
        RowId::Palette => !(ctx.settings.follow_os_theme && crate::os_theme::available()),
        _ => true,
    }
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
    if let Some((host, preset, overlay)) = preset_override(id, ctx, presets, overrides) {
        let mut resolved = overlay.apply(ctx.settings);
        let under = Ctx {
            hosts: ctx.hosts,
            library: ctx.library,
            settings: &mut resolved,
            store: ctx.store,
            platform: ctx.platform,
            screen: None,
            pads: ctx.pads,
            deck: ctx.deck,
            fallback_ui: ctx.fallback_ui,
            pyrowave_ok: ctx.pyrowave_ok,
            av1_ok: ctx.av1_ok,
            device_name: ctx.device_name,
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
fn overrides_row(id: RowId, o: &SettingsOverlay) -> bool {
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
        RowId::NoPresets => {
            return RowSpec::action("No presets yet", false);
        }
        RowId::Controllers => return RowSpec::action("Connected controllers", true),
        RowId::Licenses => return RowSpec::action("Open-source licences", true),
        RowId::QuickActions => {
            let mut r = RowSpec::action("Quick actions", true);
            r.header = Some("Quick actions");
            return r;
        }
        _ => {}
    }
    let s = &ctx.settings;
    // Dim under a parent switch (relationship stays visible). Smoothness buffer
    // is dropped instead — see [`row_applies`].
    let enabled = match id {
        RowId::EchoCancel => s.mic_enabled,
        // PyroWave ignores stored bitrate (session sends 0). Dim; keep the value.
        RowId::Bitrate => s.codec != "pyrowave",
        // Session still drops lossless unless `audio_channels == 2` (before the wire).
        // A live row under surround would change nothing. Delete this arm when that
        // filter learns the frame ladder — not before.
        RowId::AudioFormat => s.audio_channels == 2,
        RowId::Pad
        | RowId::PadType
        | RowId::SystemButtons
        | RowId::GuideGesture
        | RowId::PadHaptics
        | RowId::PadSpeaker => s.gamepad_forwarding,
        _ => true,
    };
    let (header, label, value): (Option<&'static str>, &str, String) = match id {
        RowId::Resolution => (
            None,
            "Resolution",
            if s.match_window {
                "Match window".into()
            } else if safe_area(s, ctx.platform) {
                "Native (safe area)".into()
            } else if s.width == 0 {
                "Native".into()
            } else {
                format!("{} × {}", s.width, s.height)
            },
        ),
        RowId::Aspect => {
            let fams = families(ctx.screen);
            (
                None,
                "Aspect ratio",
                fams[family(s, &fams, ctx.platform)].label.into(),
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
            "Compositor",
            label_for(&COMPOSITORS, &s.compositor).into(),
        ),
        RowId::Codec => (
            None,
            "Video codec",
            if s.codec == "pyrowave" && !ctx.pyrowave_ok {
                "PyroWave (unsupported)".into()
            } else if s.codec == "av1" && !ctx.av1_ok {
                "AV1 (unsupported)".into()
            } else {
                label_for(codecs(ctx.platform), &s.codec).into()
            },
        ),
        // Migrate before lookup or a legacy store (`vulkan`/`vaapi`) shows "—".
        RowId::Decoder => (
            None,
            "Decoder",
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
            Some("Presentation"),
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
        RowId::Mic => (None, "Microphone", on_off(s.mic_enabled).into()),
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
            "Steam / guide button",
            label_for(&SYSTEM_BUTTONS, &s.system_buttons).into(),
        ),
        RowId::GuideGesture => (
            None,
            "Hold Select for guide",
            label_for(&GUIDE_GESTURE, &s.guide_gesture).into(),
        ),
        RowId::PadHaptics => (None, "Controller haptics", on_off(s.pad_haptics).into()),
        RowId::PadSpeaker => (
            None,
            "Controller speaker",
            on_off(pad_speaker_on(&s.pad_speaker)).into(),
        ),
        RowId::Touch => (None, "Touch mode", s.touch_mode().label().into()),
        RowId::Mouse => (None, "Mouse mode", s.mouse_mode().label().into()),
        RowId::InvertScroll => (None, "Invert scroll", on_off(s.invert_scroll).into()),
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
        RowId::Palette => (
            None,
            "Background",
            crate::library::palette(&s.ui_palette).name.into(),
        ),
        // Label is the reduction, so On means it is in effect.
        RowId::ReduceMotion => (None, "Reduce motion", on_off(s.reduce_motion).into()),
        RowId::ReduceUiResolution => (
            None,
            "Reduce interface resolution",
            on_off(reduce_ui_res(s, ctx.platform, ctx.fallback_ui)).into(),
        ),
        RowId::LibraryView => (
            None,
            "Library view",
            crate::library::LibraryView::parse(&s.library_view)
                .label()
                .into(),
        ),
        RowId::LibraryCollections => (
            None,
            "Start in collections",
            on_off(s.library_collections).into(),
        ),
        RowId::StartIn => (None, "Start in", start_in_value(ctx)),
        RowId::Stats => (
            None,
            "Statistics overlay",
            s.stats_verbosity().label().into(),
        ),
        RowId::AdvancedStats => (None, "Advanced statistics", on_off(s.advanced_stats).into()),
        RowId::Fullscreen => (
            None,
            "Start streams fullscreen",
            on_off(s.fullscreen_on_stream).into(),
        ),
        RowId::AutoWake => (None, "Wake hosts automatically", on_off(s.auto_wake).into()),
        RowId::LowLatency => (
            Some("Decoding"),
            "Low-latency mode",
            on_off(extra_bool(s, android_keys::LOW_LATENCY, true)).into(),
        ),
        RowId::PhoneRumble => (
            Some("This device"),
            "Rumble on this phone",
            on_off(extra_bool(s, android_keys::PHONE_RUMBLE, false)).into(),
        ),
        RowId::PhoneGyro => (
            None,
            "Gyro from this phone",
            on_off(extra_bool(s, android_keys::PHONE_GYRO, false)).into(),
        ),
        RowId::Sc2Passthrough => (
            Some("Passthrough"),
            "Steam Controller 2",
            on_off(extra_bool(s, android_keys::SC2, true)).into(),
        ),
        RowId::DsCapture => (
            None,
            "DualSense over USB",
            on_off(extra_bool(s, android_keys::DS_CAPTURE, true)).into(),
        ),
        RowId::AudioRoute => (
            None,
            "Audio processing",
            label_for(
                &WEBOS_AUDIO_ROUTES,
                extra_str(s, webos_keys::AUDIO_ROUTE, "software"),
            )
            .into(),
        ),
        RowId::CursorGestures => (
            None,
            "Long press to right-click",
            on_off(extra_bool(s, webos_keys::CURSOR_GESTURES, false)).into(),
        ),
        RowId::GamepadUi => (
            None,
            "Controller-optimized UI",
            on_off(extra_bool(s, GAMEPAD_UI_KEY, true)).into(),
        ),
        RowId::GamepadUiMode => (
            None,
            // Not "Controller UI": that collides with the row above.
            "Show it",
            label_for(
                &GAMEPAD_UI_MODES,
                extra_str(s, GAMEPAD_UI_MODE_KEY, "connected"),
            )
            .into(),
        ),
        RowId::Preset(_)
        | RowId::NoPresets
        | RowId::Controllers
        | RowId::Licenses
        | RowId::QuickActions => {
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

/// One-line explainer. Platform so Android is not taught desktop-only chords.
pub fn detail(id: RowId, ctx: &Ctx) -> &'static str {
    use crate::platform::Platform;
    let platform = ctx.platform;
    match id {
        RowId::Resolution => {
            "The host creates a virtual display at exactly this size — no scaling. \
             Match window follows this window, including mid-stream resizes."
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
        RowId::Codec if ctx.settings.codec == "pyrowave" && !ctx.pyrowave_ok => {
            "This device can't decode PyroWave — it needs a Vulkan 1.3 GPU, which most TV \
             boxes don't have. The session streams HEVC instead."
        }
        RowId::Codec if ctx.settings.codec == "av1" && !ctx.av1_ok => {
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
            Platform::Android | Platform::WebOS | Platform::Web => {
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
            "The colour family this backdrop drifts through — it changes as you step, so \
             pick by looking. Appearance only; nothing about a stream depends on it."
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
        RowId::LibraryCollections => {
            "Opening a host's library goes straight to its collections — platforms and \
             stores as tiles — instead of the whole shelf. A library with only one \
             collection opens on the shelf as usual."
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
            Platform::Android | Platform::WebOS | Platform::Web => {
                "How much the overlay shows: Compact (one line) → Normal → Detailed."
            }
        },
        RowId::AdvancedStats => {
            "Off shows the figures Moonlight's overlay also shows. On shows capture to glass as \
             p50/p95 and every stage between. Every number: docs.punktfunk.unom.io/docs/stats"
        }
        RowId::Fullscreen => "Streams open fullscreen instead of windowed.",
        RowId::AutoWake => {
            "Send Wake-on-LAN to a sleeping host before connecting. Turn off for hosts \
             reached over a VPN, where the wake wait only adds delay."
        }
        RowId::LowLatency => {
            "Feeds the decoder slice by slice as frames arrive and marks the media sockets \
             for priority. Off if a decoder shows artefacts under it."
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
            // `row_on` offers this to Android and webOS only; Desktop and Web are here for
            // exhaustiveness, never to be read.
            Platform::Desktop | Platform::Android | Platform::Web => {
                "Front the app with this console instead of the touch interface. Off returns \
                 to the touch home immediately — switch it back on there."
            }
            Platform::WebOS => {
                "Front the app with this console instead of the cursor UI. Off returns to \
                 the cursor UI immediately — switch it back on there."
            }
        },
        RowId::GamepadUiMode => match platform {
            Platform::Desktop | Platform::Android | Platform::Web => {
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
        RowId::Controllers => "Connected controllers, their grants and a rumble/haptics test.",
        RowId::Licenses => "The open-source licences this app ships under.",
        RowId::Preset(_) => {
            "Pin this preset to a host and it appears as its own card — one press \
             connects with these settings. Presets are created and edited in the \
             Punktfunk desktop app."
        }
        RowId::NoPresets => {
            "Presets bundle stream settings for different uses (a low-latency one, a \
             quality one…). Create them in the Punktfunk desktop app, then pin them \
             here as one-press connect cards."
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
    let platform = ctx.platform;
    let fams = families(ctx.screen);
    let s = &mut *ctx.settings;
    match id {
        RowId::Resolution => {
            // Native, Native (safe area) on Android, Match window, then the current
            // family's sizes. The policies before the sizes all clear w/h.
            let sizes = &fams[family(s, &fams, platform)].sizes;
            let android = platform == crate::platform::Platform::Android;
            let matching = if android { 2 } else { 1 };
            let cur = if s.match_window {
                Some(matching)
            } else if safe_area(s, platform) {
                Some(1)
            } else if s.width == 0 {
                Some(0)
            } else {
                sizes
                    .iter()
                    .position(|&wh| wh == (s.width, s.height))
                    .map(|i| i + matching + 1)
            };
            step_option(cur, sizes.len() + matching + 1, delta, wrap).map(|i| {
                s.match_window = i == matching;
                if android {
                    set_extra_bool(s, android_keys::SAFE_AREA_MODE, i == 1);
                }
                (s.width, s.height) = if i <= matching {
                    (0, 0)
                } else {
                    sizes[i - matching - 1]
                };
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
            // Inert under PyroWave (host pins the rate; see `row_spec`).
            if s.codec == "pyrowave" {
                return false;
            }
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
        // Inert under surround (this client's request filter; see `row_spec`).
        RowId::AudioFormat => {
            if s.audio_channels == 2 {
                step_str(AUDIO_FORMATS, &mut s.audio_format, delta, wrap)
            } else {
                None
            }
        }
        RowId::KeepHostAudio => toggle(&mut s.keep_host_audio, delta, wrap),
        RowId::Mic => toggle(&mut s.mic_enabled, delta, wrap),
        RowId::EchoCancel => {
            if s.mic_enabled {
                toggle(&mut s.echo_cancel, delta, wrap)
            } else {
                None
            }
        }
        RowId::PadForward => toggle(&mut s.gamepad_forwarding, delta, wrap),
        RowId::Pad => {
            if !s.gamepad_forwarding {
                return false;
            }
            // Automatic first, then connected pads by stable key.
            let keys: Vec<String> = std::iter::once(String::new())
                .chain(ctx.pads.iter().map(|p| p.key.clone()))
                .collect();
            let cur = keys.iter().position(|c| *c == s.forward_pad);
            step_option(cur, keys.len(), delta, wrap).map(|i| s.forward_pad = keys[i].clone())
        }
        RowId::PadType => {
            if !s.gamepad_forwarding {
                return false;
            }
            step_str(&PAD_TYPES, &mut s.gamepad, delta, wrap)
        }
        RowId::SystemButtons => {
            if !s.gamepad_forwarding {
                return false;
            }
            step_str(&SYSTEM_BUTTONS, &mut s.system_buttons, delta, wrap)
        }
        RowId::GuideGesture => {
            if !s.gamepad_forwarding {
                return false;
            }
            step_str(&GUIDE_GESTURE, &mut s.guide_gesture, delta, wrap)
        }
        RowId::PadHaptics => {
            if !s.gamepad_forwarding {
                return false;
            }
            toggle(&mut s.pad_haptics, delta, wrap)
        }
        RowId::PadSpeaker => {
            if !s.gamepad_forwarding {
                return false;
            }
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
        RowId::Palette => {
            let all = &crate::library::PALETTES;
            let cur = all.iter().position(|p| p.id == s.ui_palette);
            step_option(cur, all.len(), delta, wrap).map(|i| s.ui_palette = all[i].id.to_string())
        }
        RowId::ReduceMotion => toggle(&mut s.reduce_motion, delta, wrap),
        RowId::ReduceUiResolution => toggle_extra(
            s,
            reduce_ui_key(platform),
            reduce_ui_default(platform, ctx.fallback_ui),
            delta,
            wrap,
        ),
        RowId::LibraryView => {
            let all = &crate::library::LibraryView::ALL;
            let cur = crate::library::LibraryView::parse(&s.library_view);
            let at = all.iter().position(|v| *v == cur);
            step_option(at, all.len(), delta, wrap).map(|i| s.library_view = all[i].id().into())
        }
        RowId::LibraryCollections => toggle(&mut s.library_collections, delta, wrap),
        RowId::StartIn => {
            let all = &start::StartIn::ALL;
            let at = all
                .iter()
                .position(|v| *v == start::StartIn::parse(&s.start_in));
            step_option(at, all.len(), delta, wrap).map(|i| s.start_in = all[i].as_str().into())
        }
        RowId::Fullscreen => toggle(&mut s.fullscreen_on_stream, delta, wrap),
        RowId::AutoWake => toggle(&mut s.auto_wake, delta, wrap),
        RowId::LowLatency => toggle_extra(s, android_keys::LOW_LATENCY, true, delta, wrap),
        RowId::PhoneRumble => toggle_extra(s, android_keys::PHONE_RUMBLE, false, delta, wrap),
        RowId::PhoneGyro => toggle_extra(s, android_keys::PHONE_GYRO, false, delta, wrap),
        RowId::Sc2Passthrough => toggle_extra(s, android_keys::SC2, true, delta, wrap),
        RowId::DsCapture => toggle_extra(s, android_keys::DS_CAPTURE, true, delta, wrap),
        RowId::CursorGestures => toggle_extra(s, webos_keys::CURSOR_GESTURES, false, delta, wrap),
        RowId::AudioRoute => {
            let mut v = extra_str(s, webos_keys::AUDIO_ROUTE, "software").to_string();
            step_str(&WEBOS_AUDIO_ROUTES, &mut v, delta, wrap).map(|()| {
                s.extra.insert(
                    webos_keys::AUDIO_ROUTE.to_string(),
                    serde_json::Value::String(v),
                );
            })
        }
        RowId::GamepadUi => toggle_extra(s, GAMEPAD_UI_KEY, true, delta, wrap),
        RowId::GamepadUiMode => {
            let mut v = extra_str(s, GAMEPAD_UI_MODE_KEY, "connected").to_string();
            step_str(&GAMEPAD_UI_MODES, &mut v, delta, wrap).map(|()| {
                s.extra.insert(
                    GAMEPAD_UI_MODE_KEY.to_string(),
                    serde_json::Value::String(v),
                );
            })
        }
        // Navigation rows: handled in `apply_row` before the settings path.
        RowId::Preset(_)
        | RowId::NoPresets
        | RowId::Controllers
        | RowId::Licenses
        | RowId::QuickActions => None,
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
pub(crate) mod tests {
    use super::*;
    use pf_client_core::trust::Settings;

    /// The row shows the global; a host's bound preset outranks it at launch, and the
    /// row has to say so or "Automatic" streams as DualSense with nothing explaining it.
    #[test]
    fn a_bound_preset_marks_the_row_it_overrides() {
        let (mut settings, pads) = ctx_parts();
        settings.gamepad = "auto".into();
        let library = crate::library::LibraryShared::default();
        let desk = crate::model::HostRow {
            key: "bb".into(),
            id: None,
            name: "Desk".into(),
            addr: "10.0.0.7".into(),
            port: 9777,
            fp_hex: "bb".into(),
            paired: true,
            saved: true,
            online: true,
            mgmt_port: 47990,
            can_wake: false,
            clipboard_sync: false,
            last_used: None,
            os: String::new(),
            actions: Vec::new(),
            pin: None,
            bound_preset: Some(crate::model::PresetChip {
                id: "p1".into(),
                name: "Living room".into(),
                accent: None,
                bitrate_kbps: None,
            }),
            running: String::new(),
            game_presets: Default::default(),
        };
        let hosts = [desk];
        let ctx = Ctx {
            hosts: &hosts,
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let presets = vec![("p1".to_string(), "Living room".to_string())];
        let overrides = std::collections::HashMap::from([(
            "p1".to_string(),
            SettingsOverlay {
                gamepad: Some("dualsense".into()),
                ..Default::default()
            },
        )]);
        let spec = row_spec(RowId::PadType, &ctx, &presets, &overrides);
        assert!(spec.dot, "the overridden row carries the dot");
        assert_eq!(
            spec.value.as_deref(),
            Some("Automatic"),
            "the row keeps the global, which is what the console edits"
        );
        assert_eq!(
            spec.note.as_deref(),
            Some("Preset \u{201c}Living room\u{201d} on Desk: DualSense")
        );
        let spec = row_spec(RowId::Codec, &ctx, &presets, &overrides);
        assert!(
            !spec.dot && spec.note.is_none(),
            "a row the preset leaves alone carries no marker"
        );
    }

    /// Tab names vs `console-vectors.json`. Input is `desktop_only` in the vectors:
    /// omitting it would fail this test; a seven-name list would fail the mobile clients.
    #[test]
    fn tab_names_match_the_shared_vectors() {
        let raw = include_str!("../../../../clients/shared/console-vectors.json");
        let file: serde_json::Value =
            serde_json::from_str(raw).expect("console-vectors.json must parse");
        let want: Vec<&str> = file["tabs"]
            .as_array()
            .expect("tabs")
            .iter()
            .map(|t| t["name"].as_str().expect("tab name"))
            .collect();
        let got: Vec<&str> = TABS.iter().map(|(name, _)| *name).collect();
        assert_eq!(got, want, "the desktop console's tab names and order");
    }

    /// Every refresh rate the desktop shells can persist must have an index here.
    /// `step_option` answers a missing index with 0, and `REFRESH[0]` is Automatic, so a rate
    /// this table lacks is silently discarded the first time the user nudges the row. On Linux
    /// both desktop shells write the same client-gtk-settings.json this screen reads, so 144,
    /// 165 and 240 arrive here whether or not this table offers them.
    #[test]
    fn refresh_table_covers_every_rate_the_desktop_shells_write() {
        // clients/linux/src/ui_settings.rs and clients/windows/src/app/settings.rs.
        for hz in [0u32, 30, 60, 90, 120, 144, 165, 240] {
            assert!(
                REFRESH.contains(&hz),
                "{hz} Hz is offered by the desktop shells but has no index in REFRESH"
            );
        }
        // The mechanism this pins: no index means index 0, which is Automatic.
        assert_eq!(step_option(None, REFRESH.len(), 1, false), Some(0));
        assert_eq!(REFRESH[0], 0, "index 0 must stay Automatic");
    }

    fn ctx_parts() -> (Settings, Vec<pf_client_core::menu_nav::PadInfo>) {
        (Settings::default(), Vec::new())
    }

    /// Throwaway config dir: the screens read the preset catalog and the known hosts
    /// straight off it, and a test must not see the developer's own. Settings go through
    /// `store::file_store`, which in tests is per-thread and in memory.
    ///
    /// Redirects `HOME` on unix, `APPDATA` on Windows (`trust::config_dir`).
    /// One `OnceLock` for the binary — a second copy races `set_var`.
    pub(crate) fn fake_home() {
        use std::sync::OnceLock;
        static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
        HOME.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("pf-settings-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let var = if cfg!(windows) { "APPDATA" } else { "HOME" };
            // SAFETY: runs at most once, inside `get_or_init` — concurrent `fake_home`
            // callers block until it returns, and nothing else in this binary mutates
            // the env var.
            unsafe { std::env::set_var(var, &dir) };
            dir
        });
    }

    /// Draw once so hit-testing reads real strip/list geometry.
    fn rendered(screen: &mut SettingsScreen) -> f64 {
        let fonts = crate::theme::build_fonts().unwrap();
        let (w, h) = (1280i32, 800i32);
        let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let k = f64::from(h) / 800.0;
        screen.render(
            surface.canvas(),
            Rect::from_ltrb(0.0, 64.0, w as f32, h as f32 - 86.0),
            k,
            1.0 / 60.0,
            &fonts,
            &mut ctx,
        );
        k
    }

    fn press(r: Rect) -> Pointer {
        Pointer {
            x: f64::from(r.center_x()),
            y: f64::from(r.center_y()),
            kind: crate::pointer::PointerKind::Press,
        }
    }

    fn with_ctx(f: impl FnOnce(&mut Ctx)) {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        f(&mut ctx);
    }

    /// Default-on where the GPU is the weak part; a stored value always wins over
    /// the platform default, in both directions.
    #[test]
    fn reduce_ui_res_defaults_on_for_tvs_and_stays_revertible() {
        use crate::platform::Platform;
        let s = Settings::default();
        assert!(reduce_ui_res(&s, Platform::WebOS, true));
        assert!(reduce_ui_res(&s, Platform::WebOS, false));
        assert!(reduce_ui_res(&s, Platform::Android, false));
        assert!(!reduce_ui_res(&s, Platform::Android, true));
        assert!(!reduce_ui_res(&s, Platform::Desktop, false));

        let mut off = s.clone();
        off.extra.insert(
            "webos.reduce_ui_resolution".into(),
            serde_json::Value::Bool(false),
        );
        assert!(!reduce_ui_res(&off, Platform::WebOS, true));
        let mut on = s;
        on.extra.insert(
            "android.reduce_ui_resolution".into(),
            serde_json::Value::Bool(true),
        );
        assert!(reduce_ui_res(&on, Platform::Android, true));
    }

    /// Each TV client owns its key: stepping the row on webOS writes `webos.*`,
    /// the same place that client's store and the shell's backdrop read.
    #[test]
    fn the_row_writes_the_platforms_own_key() {
        with_ctx(|ctx| {
            ctx.platform = crate::platform::Platform::WebOS;
            assert!(adjust(RowId::ReduceUiResolution, -1, false, ctx));
            assert_eq!(
                ctx.settings
                    .extra
                    .get("webos.reduce_ui_resolution")
                    .and_then(|v| v.as_bool()),
                Some(false)
            );
            assert!(!ctx
                .settings
                .extra
                .contains_key("android.reduce_ui_resolution"));

            // A phone defaults off, so stepping right writes an explicit On.
            ctx.platform = crate::platform::Platform::Android;
            ctx.fallback_ui = true;
            assert!(adjust(RowId::ReduceUiResolution, 1, false, ctx));
            assert_eq!(
                ctx.settings
                    .extra
                    .get("android.reduce_ui_resolution")
                    .and_then(|v| v.as_bool()),
                Some(true)
            );
        });
    }

    #[test]
    fn a_press_on_a_pill_selects_that_tab() {
        let mut s = SettingsScreen::with_presets(Vec::new());
        rendered(&mut s);
        assert_eq!(s.tab, 0);
        for target in [3, 1, TABS.len() - 1, 0] {
            let pill = s.strip.pill(target).expect("the strip drew every pill");
            with_ctx(|ctx| {
                let mut fx = Outbox::default();
                assert!(s.pointer(press(pill), ctx, &mut fx), "the pill took it");
            });
            assert_eq!(s.tab, target, "pressing pill {target} selects it");
            // Selecting a tab re-lays the strip; re-render so the next pick is current.
            rendered(&mut s);
        }
    }

    #[test]
    fn a_pressed_tab_restores_that_tabs_cursor() {
        let mut s = SettingsScreen::with_presets(Vec::new());
        rendered(&mut s);
        s.list.cursor = 2;
        let second = s.strip.pill(1).unwrap();
        with_ctx(|ctx| {
            let mut fx = Outbox::default();
            s.pointer(press(second), ctx, &mut fx);
        });
        assert_eq!(s.list.cursor, 0, "a fresh tab starts at its own top");
        rendered(&mut s);
        let first = s.strip.pill(0).unwrap();
        with_ctx(|ctx| {
            let mut fx = Outbox::default();
            s.pointer(press(first), ctx, &mut fx);
        });
        assert_eq!(s.list.cursor, 2, "coming back lands where it was left");
    }

    #[test]
    fn a_press_on_a_row_focuses_and_cycles_it() {
        fake_home();
        let mut s = SettingsScreen::with_presets(Vec::new());
        rendered(&mut s);
        let first = s.list.row_rect(0).expect("the list drew its rows");
        let (mut settings, pads) = ctx_parts();
        crate::store::file_store().save(&settings); // `apply_row` rebases on the store
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        assert_eq!(s.row_ids(&ctx)[0], RowId::Aspect);
        let mut fx = Outbox::default();
        assert_eq!(ctx.settings.width, 0);
        assert!(s.pointer(press(first), &mut ctx, &mut fx));
        assert_eq!(s.list.cursor, 0, "the pressed row takes focus");
        assert_eq!(
            (ctx.settings.width, ctx.settings.height),
            (1920, 1200),
            "one press both focuses the row and cycles its value"
        );
    }

    #[test]
    fn a_press_on_empty_space_is_not_consumed() {
        let mut s = SettingsScreen::with_presets(Vec::new());
        rendered(&mut s);
        with_ctx(|ctx| {
            let mut fx = Outbox::default();
            let p = Pointer {
                x: 4.0,
                y: 780.0,
                kind: crate::pointer::PointerKind::Press,
            };
            assert!(!s.pointer(p, ctx, &mut fx));
        });
    }

    /// Speaker row: stored `"mix"` reads Off; a step writes only `"pad"` / `"off"`.
    #[test]
    fn controller_audio_rows_follow_forwarding_and_speak_the_gtk_dialect() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        assert!(ctx.settings.pad_haptics);
        assert_eq!(ctx.settings.pad_speaker, "pad");
        assert!(adjust(RowId::PadHaptics, 1, true, &mut ctx));
        assert!(!ctx.settings.pad_haptics);
        assert!(adjust(RowId::PadSpeaker, 1, true, &mut ctx));
        assert_eq!(ctx.settings.pad_speaker, "off");
        assert!(adjust(RowId::PadSpeaker, 1, true, &mut ctx));
        assert_eq!(ctx.settings.pad_speaker, "pad");
        ctx.settings.pad_speaker = "mix".into();
        assert!(adjust(RowId::PadSpeaker, 1, true, &mut ctx));
        assert_eq!(ctx.settings.pad_speaker, "pad");
        ctx.settings.gamepad_forwarding = false;
        assert!(!adjust(RowId::PadHaptics, 1, true, &mut ctx));
        assert!(!adjust(RowId::PadSpeaker, 1, true, &mut ctx));
    }

    #[test]
    fn adjust_clamps_and_activate_wraps() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        // Native (index 0): left refuses; right is Match window, then sizes.
        assert!(!adjust(RowId::Resolution, -1, false, &mut ctx));
        assert!(adjust(RowId::Resolution, 1, false, &mut ctx));
        assert!(ctx.settings.match_window, "Native → Match window");
        assert_eq!((ctx.settings.width, ctx.settings.height), (0, 0));
        assert!(adjust(RowId::Resolution, 1, false, &mut ctx));
        assert!(
            !ctx.settings.match_window,
            "explicit size clears the policy"
        );
        assert_eq!((ctx.settings.width, ctx.settings.height), (1280, 720));
        assert!(adjust(RowId::Resolution, -1, false, &mut ctx));
        assert!(ctx.settings.match_window);
        assert!(adjust(RowId::Resolution, -1, false, &mut ctx));
        assert!(!ctx.settings.match_window);
        assert_eq!(ctx.settings.width, 0, "back to Native");
        (ctx.settings.width, ctx.settings.height) = (5120, 2880);
        assert!(adjust(RowId::Resolution, 1, true, &mut ctx));
        assert_eq!(ctx.settings.width, 0, "wrapped to Native");
        assert!(!ctx.settings.match_window);
    }

    /// Android's safe-area mode is its own slot after Native: shown by name, and a nudge
    /// moves off it by one step instead of snapping to Native.
    #[test]
    fn android_resolution_row_carries_the_safe_area_mode() {
        let (mut settings, pads) = ctx_parts();
        settings
            .extra
            .insert(android_keys::SAFE_AREA_MODE.into(), true.into());
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Android,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: true,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let value = |ctx: &Ctx| row_spec(RowId::Resolution, ctx, &[], &Default::default()).value;
        let safe = |ctx: &Ctx| extra_bool(ctx.settings, android_keys::SAFE_AREA_MODE, false);
        assert_eq!(value(&ctx).as_deref(), Some("Native (safe area)"));
        assert!(adjust(RowId::Resolution, 1, false, &mut ctx));
        assert!(ctx.settings.match_window, "safe area → Match window");
        assert!(!safe(&ctx));
        assert!(adjust(RowId::Resolution, -1, false, &mut ctx));
        assert!(
            safe(&ctx) && !ctx.settings.match_window,
            "back to safe area"
        );
        assert!(adjust(RowId::Resolution, -1, false, &mut ctx));
        assert_eq!((ctx.settings.width, safe(&ctx)), (0, false), "Native");
        assert_eq!(value(&ctx).as_deref(), Some("Native"));
        set_extra_bool(ctx.settings, android_keys::SAFE_AREA_MODE, true);
        (ctx.settings.width, ctx.settings.height) = (1280, 720);
        assert_eq!(value(&ctx).as_deref(), Some("1280 × 720"), "a size wins");
    }

    /// The Aspect row moves between families at the nearest height; the
    /// Resolution row then steps inside that family only.
    #[test]
    fn aspect_row_switches_family_at_nearest_height() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let size = |ctx: &Ctx| (ctx.settings.width, ctx.settings.height);
        assert_eq!(
            row_spec(RowId::Aspect, &ctx, &[], &Default::default())
                .value
                .as_deref(),
            Some("16:9"),
            "Native lists 16:9"
        );
        assert!(!adjust(RowId::Aspect, -1, false, &mut ctx), "16:9 is first");
        assert!(adjust(RowId::Aspect, 1, false, &mut ctx));
        assert_eq!(size(&ctx), (1920, 1200), "Native → 16:10 nearest 1080");
        (ctx.settings.width, ctx.settings.height) = (2560, 1600);
        assert!(adjust(RowId::Aspect, 1, false, &mut ctx));
        assert_eq!(size(&ctx), (3840, 1600), "21:9 nearest 1600");
        assert!(adjust(RowId::Resolution, 1, false, &mut ctx));
        assert_eq!(size(&ctx), (5120, 2160), "steps inside 21:9");
        assert!(
            !adjust(RowId::Resolution, 1, false, &mut ctx),
            "clamps at the family's end"
        );
        ctx.settings.match_window = true;
        (ctx.settings.width, ctx.settings.height) = (0, 0);
        assert!(adjust(RowId::Aspect, -1, true, &mut ctx));
        assert_eq!(size(&ctx), (1600, 1200), "wrapped to 4:3");
        assert!(!ctx.settings.match_window, "a size clears the policy");
    }

    /// A phone leads the Aspect row with its own screen and safe area, and Native
    /// (safe area) reads as the latter.
    #[test]
    fn a_phone_leads_the_aspect_row_with_its_own_shapes() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Android,
            screen: Some(crate::shell::DeviceScreen {
                full: (3216, 1440),
                safe: (3088, 1440),
            }),
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let aspect = |ctx: &Ctx| {
            row_spec(RowId::Aspect, ctx, &[], &Default::default())
                .value
                .unwrap_or_default()
        };
        assert_eq!(aspect(&ctx), "Screen", "Native lists the screen");
        set_extra_bool(ctx.settings, android_keys::SAFE_AREA_MODE, true);
        assert_eq!(aspect(&ctx), "Safe area");
        assert!(adjust(RowId::Aspect, -1, false, &mut ctx));
        assert_eq!(
            (ctx.settings.width, ctx.settings.height),
            (2412, 1080),
            "Screen nearest 1080"
        );
        assert!(adjust(RowId::Resolution, 1, false, &mut ctx));
        assert_eq!((ctx.settings.width, ctx.settings.height), (3216, 1440));
        assert!(adjust(RowId::Aspect, 1, false, &mut ctx));
        assert_eq!(aspect(&ctx), "Safe area");
        assert!(adjust(RowId::Aspect, 1, false, &mut ctx));
        assert_eq!(aspect(&ctx), "16:9");
    }

    #[test]
    fn toggles_read_left_off_right_on() {
        let (mut settings, pads) = ctx_parts();
        settings.mic_enabled = false;
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        assert!(
            !adjust(RowId::Mic, -1, false, &mut ctx),
            "already off = thud"
        );
        assert!(adjust(RowId::Mic, 1, false, &mut ctx));
        assert!(ctx.settings.mic_enabled);
        assert!(adjust(RowId::Mic, 1, true, &mut ctx), "A always flips");
        assert!(!ctx.settings.mic_enabled);
    }

    #[test]
    fn echo_cancellation_follows_the_microphone() {
        let (mut settings, pads) = ctx_parts();
        settings.mic_enabled = false;
        assert!(settings.echo_cancel, "it ships on");
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        assert!(!row_spec(RowId::EchoCancel, &ctx, &[], &Default::default()).enabled);
        assert!(
            !adjust(RowId::EchoCancel, -1, false, &mut ctx),
            "mic off = thud"
        );
        assert!(!adjust(RowId::EchoCancel, 1, true, &mut ctx), "A too");
        assert!(ctx.settings.echo_cancel, "and nothing was written");

        ctx.settings.mic_enabled = true;
        assert!(row_spec(RowId::EchoCancel, &ctx, &[], &Default::default()).enabled);
        assert!(adjust(RowId::EchoCancel, -1, false, &mut ctx));
        assert!(!ctx.settings.echo_cancel);
        assert!(adjust(RowId::EchoCancel, 1, true, &mut ctx));
        assert!(ctx.settings.echo_cancel);
    }

    /// The TV's codec row wraps from H.264 back to Automatic: no AV1, no PyroWave.
    #[test]
    fn webos_offers_only_the_codecs_ndl_decodes() {
        let (mut settings, pads) = ctx_parts();
        settings.codec = "h264".into();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::WebOS,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: true,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        assert!(adjust(RowId::Codec, 1, true, &mut ctx));
        assert_eq!(ctx.settings.codec, "auto");
        ctx.platform = crate::platform::Platform::Desktop;
        ctx.settings.codec = "h264".into();
        assert!(adjust(RowId::Codec, 1, true, &mut ctx));
        assert_eq!(ctx.settings.codec, "av1");
    }

    /// A GPU without the codec's compute set says so on the row, in both places a user
    /// reads: the value and the line under the list. The other codecs are untouched.
    #[test]
    fn pyrowave_reads_unsupported_where_the_gpu_cannot_decode_it() {
        let (mut settings, pads) = ctx_parts();
        settings.codec = "pyrowave".into();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Android,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: false,
            av1_ok: false,
            device_name: "t",
            t: 0.0,
        };
        let value = row_spec(RowId::Codec, &ctx, &[], &Default::default())
            .value
            .unwrap();
        assert!(value.contains("unsupported"), "value said {value}");
        assert!(detail(RowId::Codec, &ctx).contains("can't decode PyroWave"));

        ctx.settings.codec = "hevc".into();
        assert_eq!(
            row_spec(RowId::Codec, &ctx, &[], &Default::default())
                .value
                .unwrap(),
            "HEVC"
        );
        assert!(!detail(RowId::Codec, &ctx).contains("PyroWave"));

        ctx.settings.codec = "pyrowave".into();
        ctx.pyrowave_ok = true;
        assert_eq!(
            row_spec(RowId::Codec, &ctx, &[], &Default::default())
                .value
                .unwrap(),
            "PyroWave (wired LAN)"
        );
    }

    /// A device with no hardware AV1 decoder never advertises AV1, so the row that still
    /// says "AV1" is the bug (#1138): the value and the line under it both say it lost.
    #[test]
    fn av1_reads_unsupported_without_a_hardware_decoder() {
        let (mut settings, pads) = ctx_parts();
        settings.codec = "av1".into();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: false,
            device_name: "t",
            t: 0.0,
        };
        let value = row_spec(RowId::Codec, &ctx, &[], &Default::default())
            .value
            .unwrap();
        assert_eq!(value, "AV1 (unsupported)");
        assert!(detail(RowId::Codec, &ctx).contains("no hardware AV1 decoder"));

        // The other codecs keep the plain row and the plain line.
        ctx.settings.codec = "hevc".into();
        assert_eq!(
            row_spec(RowId::Codec, &ctx, &[], &Default::default())
                .value
                .unwrap(),
            "HEVC"
        );
        assert!(!detail(RowId::Codec, &ctx).contains("AV1"));

        ctx.settings.codec = "av1".into();
        ctx.av1_ok = true;
        assert_eq!(
            row_spec(RowId::Codec, &ctx, &[], &Default::default())
                .value
                .unwrap(),
            "AV1"
        );
    }

    #[test]
    fn bitrate_dims_under_pyrowave() {
        let (mut settings, pads) = ctx_parts();
        settings.codec = "pyrowave".into();
        settings.bitrate_kbps = 80_000;
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        assert!(!row_spec(RowId::Bitrate, &ctx, &[], &Default::default()).enabled);
        assert!(
            !adjust(RowId::Bitrate, 1, false, &mut ctx),
            "pyrowave = thud"
        );
        assert!(!adjust(RowId::Bitrate, 1, true, &mut ctx), "A too");
        assert_eq!(ctx.settings.bitrate_kbps, 80_000, "the stored rate is kept");

        ctx.settings.codec = "hevc".into();
        assert!(row_spec(RowId::Bitrate, &ctx, &[], &Default::default()).enabled);
        assert!(adjust(RowId::Bitrate, 1, false, &mut ctx));
    }

    #[test]
    fn smoothness_buffer_is_offered_only_under_smoothness() {
        let (mut settings, pads) = ctx_parts();
        assert_eq!(settings.present_priority, "latency", "the shipped default");
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let mut s = SettingsScreen::with_presets(Vec::new());
        s.tab = TABS
            .iter()
            .position(|(name, _)| *name == "Video")
            .expect("the Video tab");

        let video = s.row_ids(&ctx);
        assert!(
            !video.contains(&RowId::SmoothBuffer),
            "latency hides the buffer row: {video:?}"
        );
        assert!(video.contains(&RowId::PresentPriority), "the intent stays");
        assert!(
            !adjust(RowId::SmoothBuffer, 1, false, &mut ctx),
            "latency intent = thud"
        );
        assert_eq!(ctx.settings.smooth_buffer, 0, "and nothing was written");

        assert!(adjust(RowId::PresentPriority, 1, false, &mut ctx));
        assert_eq!(ctx.settings.present_priority, "smooth");
        let video = s.row_ids(&ctx);
        let intent = video
            .iter()
            .position(|id| *id == RowId::PresentPriority)
            .expect("the intent row");
        assert_eq!(
            video.get(intent + 1),
            Some(&RowId::SmoothBuffer),
            "the row that comes and goes sits BELOW the row that decides it, so the cursor \
             never has anything move out from under it"
        );
        assert!(adjust(RowId::SmoothBuffer, 1, false, &mut ctx));
        assert_eq!(ctx.settings.smooth_buffer, 1);

        s.list.cursor = intent;
        assert!(adjust(RowId::PresentPriority, -1, false, &mut ctx));
        assert_eq!(ctx.settings.present_priority, "latency");
        let video = s.row_ids(&ctx);
        assert!(!video.contains(&RowId::SmoothBuffer));
        assert_eq!(
            video.get(s.list.cursor),
            Some(&RowId::PresentPriority),
            "the cursor is still on the row the user was stepping"
        );
    }

    /// Cursor past a list that shrank: pull back, do not index. Another writer can
    /// flip presentation intent while this screen is open.
    #[test]
    fn a_shrinking_list_pulls_the_cursor_back() {
        // Seat the STORE with the shrunken list: `apply_row` rebases on it.
        fake_home();
        let (mut settings, pads) = ctx_parts();
        settings.present_priority = "latency".into();
        crate::store::file_store().save(&settings);
        settings.present_priority = "smooth".into();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let mut s = SettingsScreen::with_presets(Vec::new());
        s.tab = TABS
            .iter()
            .position(|(name, _)| *name == "Video")
            .expect("the Video tab");
        s.list.cursor = s.row_ids(&ctx).len() - 1;
        let parked = s.list.cursor;
        ctx.settings.present_priority = "latency".into();
        let mut fx = Outbox::default();
        let pulse = s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(pulse.is_some(), "the press was routed, not dropped");
        assert!(s.list.cursor < parked, "the cursor came back onto the list");
        assert!(fx.nav.is_none());
    }

    #[test]
    fn touch_mode_steps_and_wraps() {
        let (mut settings, pads) = ctx_parts();
        assert_eq!(settings.touch_mode, "trackpad");
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        assert!(
            !adjust(RowId::Touch, -1, false, &mut ctx),
            "already first = thud"
        );
        assert!(adjust(RowId::Touch, 1, false, &mut ctx));
        assert_eq!(ctx.settings.touch_mode, "pointer");
        assert!(adjust(RowId::Touch, 1, false, &mut ctx));
        assert_eq!(ctx.settings.touch_mode, "touch");
        assert!(!adjust(RowId::Touch, 1, false, &mut ctx), "last = thud");
        assert!(adjust(RowId::Touch, 1, true, &mut ctx));
        assert_eq!(ctx.settings.touch_mode, "trackpad");
    }

    #[test]
    fn mouse_mode_steps_and_wraps() {
        let (mut settings, pads) = ctx_parts();
        assert_eq!(settings.mouse_mode, "capture");
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        assert!(
            !adjust(RowId::Mouse, -1, false, &mut ctx),
            "already first = thud"
        );
        assert!(adjust(RowId::Mouse, 1, false, &mut ctx));
        assert_eq!(ctx.settings.mouse_mode, "desktop");
        assert!(!adjust(RowId::Mouse, 1, false, &mut ctx), "last = thud");
        assert!(adjust(RowId::Mouse, 1, true, &mut ctx));
        assert_eq!(ctx.settings.mouse_mode, "capture");
    }

    /// Off-ladder must not snap to Automatic (index 0). Step to the neighbour.
    #[test]
    fn an_off_ladder_rate_steps_to_its_neighbour() {
        let (mut settings, pads) = ctx_parts();
        settings.bitrate_kbps = 12_345;
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        assert!(adjust(RowId::Bitrate, 1, false, &mut ctx));
        assert_eq!(ctx.settings.bitrate_kbps, 15_000, "the rung above");
        ctx.settings.bitrate_kbps = 12_345;
        assert!(adjust(RowId::Bitrate, -1, false, &mut ctx));
        assert_eq!(ctx.settings.bitrate_kbps, 12_000, "the rung below");
        ctx.settings.bitrate_kbps = 2_000_000;
        assert!(!adjust(RowId::Bitrate, 1, false, &mut ctx), "the ceiling");
        ctx.settings.bitrate_kbps = 5_000;
        assert!(adjust(RowId::Bitrate, -1, false, &mut ctx));
        assert_eq!(ctx.settings.bitrate_kbps, 4_000);
    }

    #[test]
    fn a_typed_bitrate_is_stored_and_clamped() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        // Snapshot, not the file store: this test saves.
        let store = crate::store::SnapshotStore::new(settings.clone(), Vec::new());
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: &store,
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let mut s = SettingsScreen::with_presets(Vec::new());
        let mut fx = Outbox::default();
        let ids = s.row_ids(&ctx);
        s.list.cursor = ids
            .iter()
            .position(|id| *id == RowId::Bitrate)
            .expect("the bitrate row");
        s.menu(MenuEvent::Secondary, &mut ctx, &mut fx);
        assert!(s.editing(), "Y opens the field");
        s.text_input("13x7"); // digits only: 'x' is refused
        assert!(s.edit_key(crate::input::Key::Return, &mut ctx));
        assert!(!s.editing(), "Return closes it");
        assert_eq!(ctx.settings.bitrate_kbps, 137_000);

        s.menu(MenuEvent::Secondary, &mut ctx, &mut fx);
        s.text_input("99999"); // four digits; the fifth is refused
        assert!(s.edit_key(crate::input::Key::Return, &mut ctx));
        assert_eq!(
            ctx.settings.bitrate_kbps, 2_000_000,
            "clamped to the ceiling"
        );

        s.menu(MenuEvent::Secondary, &mut ctx, &mut fx);
        assert!(s.edit_key(crate::input::Key::Return, &mut ctx));
        assert_eq!(ctx.settings.bitrate_kbps, 2_000_000, "left alone");

        s.list.cursor = 0;
        s.menu(MenuEvent::Secondary, &mut ctx, &mut fx);
        assert!(!s.editing());
    }

    #[test]
    fn rates_read_in_the_biggest_round_unit() {
        assert_eq!(bitrate_label(20_000), "20 Mbps");
        assert_eq!(bitrate_label(12_500), "12.5 Mbps");
        assert_eq!(bitrate_label(1_000_000), "1 Gbps");
        assert_eq!(bitrate_label(1_500_000), "1.5 Gbps");
        assert_eq!(bitrate_label(2_000_000), "2 Gbps");
    }

    #[test]
    fn preset_rows_navigate_instead_of_editing() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut pinned = crate::model::HostRow {
            key: "aa\0p1".into(),
            id: None,
            name: "Tower".into(),
            addr: "10.0.0.9".into(),
            port: 9777,
            fp_hex: "aa".into(),
            paired: true,
            saved: true,
            online: true,
            mgmt_port: 47990,
            can_wake: false,
            clipboard_sync: false,
            last_used: None,
            os: String::new(),
            actions: Vec::new(),
            pin: Some(crate::model::PresetChip {
                id: "p1".into(),
                name: "Work".into(),
                accent: None,
                bitrate_kbps: None,
            }),
            bound_preset: None,
            running: String::new(),
            game_presets: Default::default(),
        };
        let hosts = [pinned.clone(), {
            pinned.key = "aa".into();
            pinned.pin = None;
            pinned
        }];
        let mut ctx = Ctx {
            hosts: &hosts,
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let mut s = SettingsScreen::with_presets(vec![
            ("p1".into(), "Work".into()),
            ("p2".into(), "Game".into()),
        ]);
        s.tab = PRESETS_TAB;
        let ids = s.row_ids(&ctx);
        assert_eq!(ids, vec![RowId::Preset(0), RowId::Preset(1)]);

        let spec = row_spec(RowId::Preset(0), &ctx, &s.presets, &s.overrides);
        assert_eq!(spec.header, None, "the tab pill names the section");
        assert_eq!(spec.label, "Work");
        assert_eq!(spec.value.as_deref(), Some("Pinned to 1 host"));
        let spec = row_spec(RowId::Preset(1), &ctx, &s.presets, &s.overrides);
        assert_eq!(spec.value.as_deref(), Some("Not pinned"));

        s.list.cursor = 0;
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(
            matches!(fx.nav, Some(crate::screens::Nav::Push(b))
                if matches!(*b, Screen::PinHosts(ref p) if p.preset_name() == "Work")),
            "A on a preset row opens its pin screen"
        );

        let mut fx = Outbox::default();
        let pulse = s.menu(
            MenuEvent::Move(pf_client_core::menu_nav::MenuDir::Right),
            &mut ctx,
            &mut fx,
        );
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
        assert!(fx.nav.is_none() && fx.cmds.is_empty());
    }

    #[test]
    fn empty_catalog_shows_the_placeholder() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let mut s = SettingsScreen::with_presets(Vec::new());
        s.tab = PRESETS_TAB;
        let ids = s.row_ids(&ctx);
        assert_eq!(ids, vec![RowId::NoPresets]);
        let spec = row_spec(RowId::NoPresets, &ctx, &s.presets, &s.overrides);
        assert!(!spec.enabled);

        s.list.cursor = ids.len() - 1;
        let mut fx = Outbox::default();
        let pulse = s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
        assert!(fx.nav.is_none());
    }

    #[test]
    fn the_quick_actions_row_opens_the_editor_and_steps_nothing() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let row = row_spec(RowId::QuickActions, &ctx, &[], &Default::default());
        assert!(row.value.is_none(), "an action row");
        assert_eq!(row.label, "Quick actions");
        assert!(!adjust(RowId::QuickActions, 1, false, &mut ctx));
        assert!(
            ctx.settings.overlay_actions.is_empty(),
            "the row itself never writes the blob"
        );
        assert!(TABS
            .iter()
            .any(|(tab, rows)| *tab == "Input" && rows.contains(&RowId::QuickActions)));
    }

    #[test]
    fn platform_row_split_hides_only_the_other_platforms_concepts() {
        use crate::platform::Platform;
        let all: Vec<RowId> = TABS
            .iter()
            .flat_map(|(_, rows)| rows.iter().copied())
            .collect();
        let off_desktop: Vec<RowId> = all
            .iter()
            .copied()
            .filter(|id| !row_on(*id, Platform::Desktop))
            .collect();
        assert_eq!(
            off_desktop,
            vec![
                RowId::LowLatency,
                RowId::AudioRoute,
                RowId::PhoneRumble,
                RowId::PhoneGyro,
                RowId::Sc2Passthrough,
                RowId::DsCapture,
                RowId::Controllers,
                RowId::CursorGestures,
                RowId::ReduceUiResolution,
                RowId::GamepadUi,
                RowId::GamepadUiMode,
                RowId::Licenses,
            ]
        );
        let off_android: Vec<RowId> = all
            .iter()
            .copied()
            .filter(|id| !row_on(*id, Platform::Android))
            .collect();
        assert_eq!(
            off_android,
            vec![
                RowId::Decoder,
                RowId::Chroma444,
                // TenBitSdr is NOT here: MediaCodec decodes Main10 from the SPS and the depth
                // asks nothing of the panel, so Android obeys it.
                RowId::Vsync,
                RowId::AllowVrr,
                RowId::AudioRoute,
                // Every controller already gets its own wire slot, so player 1 is not a choice.
                RowId::Pad,
                RowId::CursorGestures,
                RowId::Shortcuts,
                RowId::Fullscreen,
            ]
        );
        // Every row reaches at least one platform: a row listed in a tab and offered nowhere
        // is dead weight the tab still spends a line on.
        assert!(all
            .iter()
            .all(|id| Platform::ALL.iter().any(|p| row_on(*id, *p))));
    }

    /// The other direction, and the one that bites: a platform missing from every list
    /// offers no row at all, so all six tabs draw empty instead of one control going
    /// missing. `Web` shipped that way.
    #[test]
    fn every_platform_offers_rows() {
        use crate::platform::Platform;
        for p in Platform::ALL {
            // Exhaustive on purpose: a new variant must be weighed here and added to `ALL`.
            match p {
                Platform::Desktop | Platform::Android | Platform::WebOS | Platform::Web => {}
            }
            let n = TABS
                .iter()
                .flat_map(|(_, rows)| rows.iter())
                .filter(|id| row_on(**id, p))
                .count();
            assert!(n > 0, "{p:?} offers no settings rows at all");
        }
    }

    #[test]
    fn android_rows_live_in_extra() {
        with_ctx(|ctx| {
            ctx.platform = crate::platform::Platform::Android;
            let before = ctx.settings.clone();
            assert!(extra_bool(ctx.settings, android_keys::LOW_LATENCY, true));
            assert!(adjust(RowId::LowLatency, 1, true, ctx));
            assert!(!extra_bool(ctx.settings, android_keys::LOW_LATENCY, true));
            assert!(adjust(RowId::GamepadUiMode, 1, true, ctx));
            assert_eq!(
                extra_str(ctx.settings, GAMEPAD_UI_MODE_KEY, "connected"),
                "always"
            );
            assert!(extra_bool(ctx.settings, GAMEPAD_UI_KEY, true));
            assert!(adjust(RowId::GamepadUi, 1, true, ctx));
            assert!(!extra_bool(ctx.settings, GAMEPAD_UI_KEY, true));
            let mut after = ctx.settings.clone();
            after.extra = before.extra.clone();
            assert_eq!(after, before);
        });
    }

    #[test]
    fn console_off_switch_needs_a_fallback_ui() {
        with_ctx(|ctx| {
            ctx.platform = crate::platform::Platform::Android;
            assert!(
                !row_applies(RowId::GamepadUi, ctx),
                "a TV offers no off switch"
            );
            assert!(!row_applies(RowId::GamepadUiMode, ctx));
            ctx.fallback_ui = true;
            assert!(row_applies(RowId::GamepadUi, ctx));
            assert!(row_applies(RowId::GamepadUiMode, ctx));
            set_extra_bool(ctx.settings, GAMEPAD_UI_KEY, false);
            assert!(row_applies(RowId::GamepadUi, ctx));
            assert!(
                !row_applies(RowId::GamepadUiMode, ctx),
                "the mode row decides nothing while the switch above it is off"
            );
        });
    }

    /// webOS bounds its own slider at 200 Mbps and clamps the document to it, so the shell
    /// must not offer more there — and must not take anything away from the others.
    #[test]
    fn the_bitrate_ceiling_is_webos_only() {
        use crate::platform::Platform;
        assert_eq!(bitrate_ceiling_kbps(Platform::WebOS), 200_000);
        assert_eq!(
            bitrate_ceiling_kbps(Platform::Desktop),
            *BITRATES.last().expect("rungs"),
            "the ladder's own top still stands off the TV"
        );
        assert_eq!(
            bitrate_ceiling_kbps(Platform::Android),
            bitrate_ceiling_kbps(Platform::Desktop)
        );

        // Stepping up from the rung below the cap lands ON it and goes no further.
        with_ctx(|ctx| {
            ctx.platform = Platform::WebOS;
            ctx.settings.bitrate_kbps = 150_000;
            assert!(adjust(RowId::Bitrate, 1, false, ctx));
            assert_eq!(ctx.settings.bitrate_kbps, 200_000);
            assert!(
                !adjust(RowId::Bitrate, 1, false, ctx),
                "200 Mbps is the last rung a TV may reach"
            );
            assert_eq!(ctx.settings.bitrate_kbps, 200_000);
            // Down still works, so the cap is a ceiling and not a trap.
            assert!(adjust(RowId::Bitrate, -1, false, ctx));
            assert_eq!(ctx.settings.bitrate_kbps, 150_000);
        });

        // The same step on a desktop keeps climbing.
        with_ctx(|ctx| {
            ctx.settings.bitrate_kbps = 200_000;
            assert!(adjust(RowId::Bitrate, 1, false, ctx));
            assert_eq!(ctx.settings.bitrate_kbps, 250_000);
        });
    }

    #[test]
    fn every_row_has_exactly_one_tab() {
        let mut seen: Vec<RowId> = Vec::new();
        for (_, rows) in &TABS {
            for id in *rows {
                assert!(!seen.contains(id), "{id:?} is in two tabs");
                seen.push(*id);
            }
        }
        assert_eq!(seen.len(), 55, "{seen:?}");
        assert!(seen.contains(&RowId::StartIn));
        assert!(seen.contains(&RowId::AdvancedStats));
        assert!(seen.contains(&RowId::FollowOsTheme));
        assert!(seen.contains(&RowId::Palette));
        assert!(seen.contains(&RowId::ReduceMotion));
        assert!(seen.contains(&RowId::ReduceUiResolution));
        assert!(seen.contains(&RowId::AudioFormat));
        assert!(TABS[PRESETS_TAB].1.is_empty());
        assert_eq!(TABS[PRESETS_TAB].0, "Presets");
    }

    /// Only test that touches the process-wide `os_theme` slot. A sibling races
    /// under libtest. Leaves the slot cleared.
    #[test]
    fn the_follow_system_row_exists_only_where_a_theme_is_published() {
        with_ctx(|ctx| {
            assert!(
                !row_applies(RowId::FollowOsTheme, ctx),
                "no publisher, no row"
            );
            assert!(row_applies(RowId::Palette, ctx));

            let t = crate::os_theme::OsTheme {
                light: false,
                background: (0.02, 0.04, 0.12),
                foreground: (1.0, 0.81, 0.68),
                accent: (0.49, 0.51, 0.85),
            };
            crate::os_theme::set_os_theme(Some(t));
            let rev = crate::os_theme::os_theme().0;
            crate::os_theme::set_os_theme(Some(t));
            assert_eq!(
                crate::os_theme::os_theme().0,
                rev,
                "an unchanged publish is free"
            );

            assert!(row_applies(RowId::FollowOsTheme, ctx));
            assert!(
                !row_applies(RowId::Palette, ctx),
                "ruled by the system theme"
            );

            ctx.settings.follow_os_theme = false;
            assert!(row_applies(RowId::Palette, ctx));

            crate::os_theme::set_os_theme(None);
            assert!(!row_applies(RowId::FollowOsTheme, ctx));
        });
    }

    /// Off by default: this key decides where a deep link lands, so an install
    /// that never opens this screen must keep the shelf it has.
    #[test]
    fn the_collections_entry_sits_with_the_library_view_and_ships_off() {
        let (mut settings, pads) = ctx_parts();
        assert!(!settings.library_collections, "off by default");
        let interface = TABS
            .iter()
            .find(|(name, _)| *name == "Interface")
            .expect("the Interface tab")
            .1;
        let view = interface
            .iter()
            .position(|id| *id == RowId::LibraryView)
            .expect("the library view row");
        assert_eq!(interface.get(view + 1), Some(&RowId::LibraryCollections));

        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        assert!(
            !adjust(RowId::LibraryCollections, -1, false, &mut ctx),
            "already off = thud"
        );
        assert!(adjust(RowId::LibraryCollections, 1, false, &mut ctx));
        assert!(ctx.settings.library_collections);
        assert_eq!(
            row_spec(RowId::LibraryCollections, &ctx, &[], &Default::default())
                .value
                .as_deref(),
            Some("On"),
            "the row says what the key holds"
        );
        assert!(
            !adjust(RowId::LibraryCollections, 1, false, &mut ctx),
            "on = thud"
        );
        assert!(
            adjust(RowId::LibraryCollections, 1, true, &mut ctx),
            "A flips it back"
        );
        assert!(!ctx.settings.library_collections);
    }

    #[test]
    fn shoulders_cycle_tabs_and_keep_each_cursor() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let mut s = SettingsScreen::with_presets(Vec::new());
        let mut fx = Outbox::default();
        assert_eq!(s.tab, 0);
        s.list.cursor = 3; // Stream / Bitrate
        s.menu(MenuEvent::JumpForward, &mut ctx, &mut fx);
        assert_eq!(s.tab, 1);
        assert_eq!(s.list.cursor, 0, "a fresh tab starts at its first row");
        s.list.cursor = 2; // Video / 10-bit HDR
        s.menu(MenuEvent::JumpBack, &mut ctx, &mut fx);
        assert_eq!((s.tab, s.list.cursor), (0, 3), "Stream kept its place");
        s.menu(MenuEvent::JumpBack, &mut ctx, &mut fx);
        assert_eq!(s.tab, PRESETS_TAB);
        assert_eq!(s.list.cursor, 0);
        s.menu(MenuEvent::JumpForward, &mut ctx, &mut fx);
        assert_eq!(s.tab, 0);
        assert!(fx.nav.is_none() && fx.cmds.is_empty());
    }

    /// TV remotes have no shoulders and no Tab key: Up from row 0 focuses the strip.
    #[test]
    fn dpad_alone_reaches_every_tab() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let mut s = SettingsScreen::with_presets(Vec::new());
        let mut fx = Outbox::default();
        assert_eq!(s.list.cursor, 0);
        s.menu(MenuEvent::Move(MenuDir::Up), &mut ctx, &mut fx);
        assert!(s.strip_focus, "Up from the top row lands on the strip");
        s.menu(MenuEvent::Move(MenuDir::Right), &mut ctx, &mut fx);
        assert_eq!(s.tab, 1);
        assert!(s.strip_focus, "switching keeps the strip focused");
        s.menu(MenuEvent::Move(MenuDir::Left), &mut ctx, &mut fx);
        s.menu(MenuEvent::Move(MenuDir::Left), &mut ctx, &mut fx);
        assert_eq!(s.tab, PRESETS_TAB, "the strip wraps like the shoulders do");
        s.menu(MenuEvent::Move(MenuDir::Down), &mut ctx, &mut fx);
        assert!(!s.strip_focus, "Down drops back into the list");
        s.menu(MenuEvent::Move(MenuDir::Down), &mut ctx, &mut fx);
        assert!(!s.strip_focus);
        assert!(fx.nav.is_none() && fx.cmds.is_empty());
    }

    /// Assert against `audio_format` constants so a spelling change there reds this
    /// instead of writing a key nobody reads. Dim under surround: see [`row_spec`].
    #[test]
    fn audio_format_ships_off_and_follows_the_channel_count() {
        use pf_client_core::audio_format::{AUDIO_FORMAT_LOSSLESS_48, AUDIO_FORMAT_LOSSLESS_96};
        let (mut settings, pads) = ctx_parts();
        assert_eq!(settings.audio_format, AUDIO_FORMAT_OPUS, "off by default");
        assert_eq!(settings.audio_channels, 2, "…and the gate starts open");
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let mut s = SettingsScreen::with_presets(Vec::new());
        s.tab = TABS
            .iter()
            .position(|(name, _)| *name == "Audio")
            .expect("the Audio tab");
        let audio = s.row_ids(&ctx);
        let channels = audio
            .iter()
            .position(|id| *id == RowId::Audio)
            .expect("the channels row");
        assert_eq!(
            audio.get(channels + 1),
            Some(&RowId::AudioFormat),
            "the row sits directly under the one that dims it, like every other pair here"
        );

        assert!(
            !adjust(RowId::AudioFormat, -1, false, &mut ctx),
            "already Opus = thud"
        );
        assert!(adjust(RowId::AudioFormat, 1, false, &mut ctx));
        assert_eq!(ctx.settings.audio_format, AUDIO_FORMAT_LOSSLESS_48);
        assert!(adjust(RowId::AudioFormat, 1, false, &mut ctx));
        assert_eq!(ctx.settings.audio_format, AUDIO_FORMAT_LOSSLESS_96);
        assert!(
            !adjust(RowId::AudioFormat, 1, false, &mut ctx),
            "last = thud"
        );
        assert!(adjust(RowId::AudioFormat, 1, true, &mut ctx));
        assert_eq!(ctx.settings.audio_format, AUDIO_FORMAT_OPUS);

        ctx.settings.audio_format = AUDIO_FORMAT_LOSSLESS_48.into();
        ctx.settings.audio_channels = 6;
        assert!(!row_spec(RowId::AudioFormat, &ctx, &[], &Default::default()).enabled);
        assert!(
            !adjust(RowId::AudioFormat, 1, false, &mut ctx),
            "surround = thud"
        );
        assert!(!adjust(RowId::AudioFormat, 1, true, &mut ctx), "A too");
        assert_eq!(
            ctx.settings.audio_format, AUDIO_FORMAT_LOSSLESS_48,
            "and nothing was written — the stored preference survives the gate"
        );
        assert!(s.row_ids(&ctx).contains(&RowId::AudioFormat));
        ctx.settings.audio_channels = 2;
        assert!(row_spec(RowId::AudioFormat, &ctx, &[], &Default::default()).enabled);

        ctx.settings.audio_format = AUDIO_FORMAT_OPUS.into();
        let opus = row_spec(RowId::AudioFormat, &ctx, &[], &Default::default()).value;
        assert!(opus.is_some());
        ctx.settings.audio_format = "lossless192".into();
        assert_eq!(
            row_spec(RowId::AudioFormat, &ctx, &[], &Default::default()).value,
            opus
        );
    }

    #[test]
    fn palette_row_steps_the_shared_key() {
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        assert_eq!(ctx.settings.ui_palette, "violet", "the brand default ships");
        assert_eq!(
            row_spec(RowId::Palette, &ctx, &[], &Default::default())
                .value
                .as_deref(),
            Some("Violet")
        );
        assert!(
            !adjust(RowId::Palette, -1, false, &mut ctx),
            "already the first = thud"
        );
        assert!(adjust(RowId::Palette, 1, false, &mut ctx));
        assert_eq!(ctx.settings.ui_palette, crate::library::PALETTES[1].id);
        ctx.settings.ui_palette = crate::library::PALETTES
            .last()
            .expect("non-empty")
            .id
            .to_string();
        assert!(adjust(RowId::Palette, 1, true, &mut ctx));
        assert_eq!(ctx.settings.ui_palette, "violet");
        ctx.settings.ui_palette = "chartreuse".into();
        assert_eq!(
            row_spec(RowId::Palette, &ctx, &[], &Default::default())
                .value
                .as_deref(),
            Some("Violet"),
            "an unknown palette reads as the default it actually draws"
        );
    }

    /// The value names where a launch will land, not what the key holds: with no
    /// default host every setting resolves to the list, and the row must say so.
    #[test]
    fn the_start_in_row_cycles_and_names_the_host() {
        use pf_client_core::trust::{KnownHost, KnownHosts};

        let store = std::sync::Arc::new(crate::store::SnapshotStore::new(
            Settings::default(),
            Vec::new(),
        ));
        let (mut settings, pads) = ctx_parts();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: store.as_ref(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let value = |ctx: &Ctx| {
            row_spec(RowId::StartIn, ctx, &[], &Default::default())
                .value
                .unwrap()
        };

        // The fresh default is the list by choice, so the row reads plainly, not as a fallback.
        assert_eq!(value(&ctx), "Host list");
        assert!(adjust(RowId::StartIn, 1, false, &mut ctx));
        assert_eq!(ctx.settings.start_in, "library");
        assert_eq!(value(&ctx), "Host list (no default host)");
        store.set_known_hosts(KnownHosts {
            hosts: vec![KnownHost {
                name: "Desk".into(),
                addr: "10.0.0.5".into(),
                fp_hex: "aa".repeat(32),
                paired: true,
                ..Default::default()
            }],
        });
        assert_eq!(
            value(&ctx),
            "Library \u{b7} Desk",
            "one paired host derives"
        );

        assert!(adjust(RowId::StartIn, 1, false, &mut ctx));
        assert_eq!(ctx.settings.start_in, "stream");
        assert_eq!(value(&ctx), "Stream \u{b7} Desk");
        assert!(
            !adjust(RowId::StartIn, 1, false, &mut ctx),
            "the last value = thud"
        );
        assert!(adjust(RowId::StartIn, 1, true, &mut ctx));
        assert_eq!(ctx.settings.start_in, "hosts");
        assert_eq!(
            value(&ctx),
            "Host list",
            "a resolved default host does not dress up the list"
        );
    }
}
