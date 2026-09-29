//! App settings: the persisted [`Settings`] model, its typed views, and the preset
//! resolver every front-end and the session go through. `trust` re-exports all of it.

use crate::presets::{PresetsFile, Resolution, StreamPreset};
use crate::trust::{config_dir, load_json_or_default, write_atomic, KnownHosts};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Overlay tier and corner. Live in the core so every client formats with the same enums.
pub use punktfunk_core::hud::{HudCorner, StatsVerbosity};

/// How a touchscreen drives the host (Android `TouchMode`, Apple `TouchInputMode`).
/// Stored stringly in [`Settings::touch_mode`]; parsed with [`TouchMode::from_name`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TouchMode {
    /// Relative cursor (touchpad): stays put on down, moves by delta, tap to click.
    /// Default — a cursor works on a screen the host is not sized for.
    Trackpad,
    /// Direct pointing: the cursor jumps to the finger and follows it (absolute).
    Pointer,
    /// Multi-touch passthrough: each finger is a host contact, no gesture interpretation.
    Touch,
    /// Fingers reach the host as nothing. Client gestures (ring twist, stats tap) still
    /// run, so a miss beside the on-screen pad cannot move the cursor.
    Off,
}

impl TouchMode {
    pub const ALL: [TouchMode; 4] = [
        TouchMode::Trackpad,
        TouchMode::Pointer,
        TouchMode::Touch,
        TouchMode::Off,
    ];

    /// Persisted name; unknown / unset → `Trackpad`.
    pub fn from_name(s: &str) -> TouchMode {
        match s {
            "pointer" => TouchMode::Pointer,
            "touch" => TouchMode::Touch,
            "off" => TouchMode::Off,
            _ => TouchMode::Trackpad,
        }
    }

    pub fn as_name(self) -> &'static str {
        match self {
            TouchMode::Trackpad => "trackpad",
            TouchMode::Pointer => "pointer",
            TouchMode::Touch => "touch",
            TouchMode::Off => "off",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TouchMode::Trackpad => "Trackpad",
            TouchMode::Pointer => "Direct pointer",
            TouchMode::Touch => "Touch passthrough",
            TouchMode::Off => "Off",
        }
    }
}

/// How a physical mouse drives the host (design/remote-desktop-sweep.md). Stored
/// stringly in [`Settings::mouse_mode`]; parsed with [`MouseMode::from_name`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseMode {
    /// Pointer lock (relative deltas, hidden cursor). Default: the only cursor is the host's.
    Capture,
    /// Uncaptured absolute pointer through the letterbox. Needs an injector with
    /// absolute support (not gamescope).
    Desktop,
}

impl MouseMode {
    pub const ALL: [MouseMode; 2] = [MouseMode::Capture, MouseMode::Desktop];

    /// Persisted name; unknown / unset → `Capture`.
    pub fn from_name(s: &str) -> MouseMode {
        match s {
            "desktop" => MouseMode::Desktop,
            _ => MouseMode::Capture,
        }
    }

    pub fn as_name(self) -> &'static str {
        match self {
            MouseMode::Capture => "capture",
            MouseMode::Desktop => "desktop",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MouseMode::Capture => "Capture (games)",
            MouseMode::Desktop => "Desktop (absolute)",
        }
    }
}

/// Presentation intent (design/desktop-presentation-rebuild.md). Stored as
/// [`Settings::present_priority`] + [`Settings::smooth_buffer`]; resolved with
/// [`PresentPriority::resolve`]: anything but `"smooth"` is latency; a buffer
/// outside 1..=3 (including 0 = Automatic) becomes 2.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PresentPriority {
    /// Present the moment the display can take it. Default.
    Latency,
    /// Buffer 1–3 frames of jitter, at that many frames of added display latency.
    Smooth { buffer: u8 },
}

impl PresentPriority {
    /// Shared resolution rule — pure, so every embedder agrees on a foreign preset.
    pub fn resolve(name: &str, buffer: u8) -> PresentPriority {
        if name == "smooth" {
            PresentPriority::Smooth {
                buffer: if (1..=3).contains(&buffer) { buffer } else { 2 },
            }
        } else {
            PresentPriority::Latency
        }
    }

    /// Frames the smoothing store holds; `0` = newest-wins (the latency intent).
    pub fn fifo_capacity(self) -> u8 {
        match self {
            PresentPriority::Latency => 0,
            PresentPriority::Smooth { buffer } => buffer,
        }
    }
}

/// App settings, persisted as JSON. Stringly-typed prefs so the file stays readable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Stream mode; `0` = native size/refresh of the window's monitor, resolved at connect.
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    /// Requested encoder bitrate (kbps); 0 = host default.
    pub bitrate_kbps: u32,
    /// Host render/encode at `mode × render_scale`; presenter downscales. `> 1`
    /// supersamples; `< 1` under-renders; `1.0` = native. Clamped even, codec max.
    pub render_scale: f64,
    /// How a frame whose aspect differs from the window fills it: `"fit"` (default, bars),
    /// `"crop"` or `"stretch"`. Parsed with `punktfunk_core::video_fit::VideoFit::from_name`;
    /// unknown reads as fit.
    #[serde(default = "default_video_fit")]
    pub video_fit: String,
    pub gamepad: String,
    /// Forward this device's controllers. Default on.
    ///
    /// Off: the client never opens the pad (SDL HIDAPI takes hidraw). Needed when a
    /// USB passthrough or a pad plugged into the host already owns it — otherwise the
    /// host sees two controllers. See [`crate::gamepad::GamepadService::set_forwarding`].
    #[serde(default = "default_true")]
    pub gamepad_forwarding: bool,
    /// `vid:pid:name` (`PadInfo::key`) forwarded as pad 0; empty = most recently connected.
    ///
    /// Per device, and NOT the same value as Apple's `gamepadID`, which is
    /// `vendorName|productCategory`: GameController exposes no vid/pid, and iOS and tvOS have no
    /// IOKit to get one from. The two grammars cannot be exchanged — do not "unify" them by
    /// copying a value across.
    pub forward_pad: String,
    /// Guide / QAM while streaming: `"auto"` (default), `"forward"`, or `"local"`.
    /// Auto forwards everywhere except Gaming Mode, where the local Steam UI also
    /// reacts — forwarding there opens both overlays. Resolved in
    /// [`Settings::system_buttons_forward`].
    #[serde(default = "default_auto")]
    pub system_buttons: String,
    /// Hold-Select ≥ ~350 ms sends the host the guide button (down for the hold).
    /// `"auto"` / `"on"` / `"off"`; auto = on only where raw guide cannot reach the
    /// host cleanly (Gaming Mode). A Select tap is delayed up to the threshold.
    #[serde(default = "default_auto")]
    pub guide_gesture: String,
    /// Host compositor backend to request (advisory; the host falls back if unavailable).
    pub compositor: String,
    /// [`TouchMode`] name: `"trackpad"` (default), `"pointer"`, `"touch"`, or `"off"`.
    /// `default` so older stores load as trackpad.
    #[serde(default = "default_touch_mode")]
    pub touch_mode: String,
    /// [`MouseMode`] name: `"capture"` (default) or `"desktop"`. `default` so older
    /// stores load as capture.
    #[serde(default = "default_mouse_mode")]
    pub mouse_mode: String,
    /// Send system chords (Alt+Tab, Super) to the host while input is captured.
    /// Off leaves them with the local shell. Applies in both mouse models.
    pub inhibit_shortcuts: bool,
    pub mic_enabled: bool,
    /// Platform echo cancellation (PipeWire echo-cancelled source; WASAPI Communications
    /// category). Default on — without it a laptop speaker looping host audio is heard
    /// by the mic. `PUNKTFUNK_NO_AEC=1` overrides off. Only while `mic_enabled`.
    #[serde(default = "default_true")]
    pub echo_cancel: bool,
    /// Requested channels: 2 (stereo), 6 (5.1), 8 (7.1). Host clamps; decoder follows.
    pub audio_channels: u8,
    /// Cross-client `audio_format`: Opus (default), lossless 48, or lossless 96
    /// (`crate::audio_format::AUDIO_FORMATS`).
    ///
    /// Off by default: lossless takes 2.3–4.6 Mbps outside the ABR video budget, vs
    /// ~256 kbps Opus. A request, never a fact — the host may still answer Opus.
    /// Stereo-only: a lossless surround frame does not fit one QUIC datagram
    /// (`design/hi-res-audio.md`). A `String` so an unrecognized value resolves to
    /// Opus rather than ending a session.
    #[serde(default = "default_audio_format")]
    pub audio_format: String,
    /// Ask the host to leave its own audio devices alone (`CLIENT_CAP_KEEP_HOST_AUDIO`).
    /// Off (default): the host parks playback on a silent endpoint. Best-effort; older
    /// hosts ignore it.
    #[serde(default)]
    pub keep_host_audio: bool,
    /// Preferred video codec: `"auto"` (host decides), `"hevc"`, `"h264"`, or `"av1"`.
    /// Soft preference — the host honors it when it can, else falls back.
    #[serde(default = "default_codec")]
    pub codec: String,
    /// Decoder preference: `"auto"` (vendor-ordered native ladder), `"native-vulkan"`,
    /// `"native-vaapi"`, `"native-d3d11va"`, or `"software"`.
    ///
    /// A stored value is not validated. Pre-native spellings `"vulkan"`/`"vaapi"`/`"d3d11va"`
    /// map in `video::migrate_decoder_pref` at warn; the store is not rewritten, so a
    /// downgrade still works. `PUNKTFUNK_DECODER` overrides (see `video::Decoder::new`).
    pub decoder: String,
    /// Decode/present GPU marketing name; empty = automatic. Maps to `PUNKTFUNK_VK_ADAPTER`.
    #[serde(default)]
    pub adapter: String,
    /// Ask for 4:4:4 (`quic::VIDEO_CAP_444`). Default off: bandwidth and encode
    /// headroom; per-preset because a desktop wants it and a game usually does not.
    #[serde(default)]
    pub enable_444: bool,
    /// Advertise 10-bit + HDR10. Off means never send HDR. Default true: Linux stores
    /// never carried this and always advertised.
    #[serde(default = "default_true")]
    pub hdr_enabled: bool,
    /// Advertise 10-bit without HDR (`VIDEO_CAP_10BIT`): SDR desktop at Main10.
    /// Subsumed by `hdr_enabled`. `default` so older stores load off.
    ///
    /// Unlike `hdr_enabled` this asks nothing of the panel, so no client gates it on a display
    /// probe.
    #[serde(default)]
    pub ten_bit_sdr: bool,
    /// `"latency"` (default) or `"smooth"`. Unknown reads as latency so a future
    /// value degrades safely.
    #[serde(default = "default_present_priority")]
    pub present_priority: String,
    /// Smoothness buffer in frames: `0` = Automatic (resolves to 2), else 1–3.
    /// Only under `present_priority = "smooth"`. One frame ≈ one refresh of jitter
    /// and one refresh of display latency.
    #[serde(default)]
    pub smooth_buffer: u8,
    /// Tear-free presentation (default on = MAILBOX, FIFO fallback). Off asks
    /// IMMEDIATE. Shared `vsync` key; macOS defaults false — sync-off means
    /// something different per platform.
    #[serde(default = "default_true")]
    pub vsync: bool,
    /// Let a VRR display follow the stream cadence when fullscreen. Inert on
    /// fixed-refresh (measured from on-glass timestamps). Default on.
    ///
    /// Desktop only, and deliberately absent on Android: that client pins a fixed display mode
    /// and declares its surface FIXED_SOURCE, because OEM refresh governors ignore the advisory
    /// hint. A setting there would have to fight that, not merely gate it.
    #[serde(default = "default_true")]
    pub allow_vrr: bool,
    /// Legacy on/off for the stats overlay — kept in sync with `stats_verbosity`
    /// so pre-tier binaries reading the same file keep working. `alias`: older
    /// WinUI shells persisted this as `show_hud`.
    #[serde(alias = "show_hud")]
    pub show_stats: bool,
    /// Stats overlay tier. `None` = a pre-tier store; resolve through
    /// [`Settings::stats_verbosity`], which falls back to `show_stats`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats_verbosity: Option<StatsVerbosity>,
    /// Overlay vocabulary: off = the Standard figures Moonlight also shows, on = the Advanced
    /// capture→glass view. Device-wide; a preset never carries it.
    #[serde(default)]
    pub advanced_stats: bool,
    /// Stats overlay corner, a [`HudCorner`] name; `""` = this client's own corner. Device-wide.
    #[serde(default)]
    pub hud_placement: String,
    /// Stats overlay size in percent, on top of the display scale; resolve with
    /// [`punktfunk_core::hud::stats_scale`]. The stats panel only. Device-wide.
    #[serde(default = "default_stats_scale_pct")]
    pub stats_scale_pct: u16,
    /// Show how to leave for a few seconds when a stream starts. Device-wide.
    #[serde(default = "default_true")]
    pub exit_hint: bool,
    /// Settings screens show their advanced rows. Device-wide; hiding a row keeps its value.
    #[serde(default)]
    pub show_advanced: bool,
    /// Enter fullscreen when a stream starts. `--fullscreen` (Gaming Mode) ignores this.
    pub fullscreen_on_stream: bool,
    /// Gamepad-UI backdrop palette (`"violet"` default). Presentation only — never
    /// part of a settings preset. Unknown name → default (a newer client may have
    /// shipped one this binary does not know).
    #[serde(default = "default_ui_palette")]
    pub ui_palette: String,
    /// Follow the desktop theme where the platform exposes one (Omarchy on Linux).
    /// While on, [`ui_palette`](Self::ui_palette) stays stored but does not draw.
    /// Default on: following the desk is the integration; the switch is the way out.
    #[serde(default = "default_true")]
    pub follow_os_theme: bool,
    /// Freeze decorative motion. Presentation only, like [`ui_palette`](Self::ui_palette).
    /// No portable OS "reduce motion" via SDL; also the OLED-friendly mode.
    #[serde(default)]
    pub reduce_motion: bool,
    /// Library order within a group: `""`/unknown = host order, `"title"`,
    /// `"platform"`, or `"store"`. Presentation only; unknown → default shelf.
    #[serde(default)]
    pub library_sort: String,
    /// Library arrangement: `"shelf"` (default, and unknown values) or `"grid"`.
    #[serde(default)]
    pub library_view: String,
    /// The Games tab's sections, in order: ids comma-separated, a leading `-` on a section
    /// switched off (`desktops,recent,-favorites,launchers,collections,games`). `""` = every section, on,
    /// in that order. The Apple app's `librarySections` spelling.
    #[serde(default)]
    pub library_sections: String,
    /// Where a bare launch opens: `"hosts"` (default), `"library"`, or `"stream"`.
    /// `""`/unknown = hosts, the `library_view` convention. Resolve through
    /// [`crate::start::start_screen`] — no default host degrades every value to the list.
    #[serde(default)]
    pub start_in: String,
    /// The host a bare launch opens on, a [`KnownHost::id`](crate::trust::KnownHost::id).
    /// `None` = derive it: the sole paired record, else none. A dangling id falls through
    /// to that rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_host: Option<String>,
    /// Wake-on-LAN before connecting and wait for boot. Default on. Off for VPN
    /// hosts, where broadcast never reaches and the wait only adds delay.
    #[serde(default = "default_true")]
    pub auto_wake: bool,
    /// Reverse wheel/trackpad scroll sent to the host. Default off = host matches this machine.
    #[serde(default)]
    pub invert_scroll: bool,
    /// In-stream quick-action ring JSON ([`crate::overlay_actions::OverlayConfig::parse`]).
    /// Empty = platform default. One opaque field: each cross-client setting is ~six edits.
    #[serde(default)]
    pub overlay_actions: String,
    /// Playback endpoint (PipeWire `node.name` / WASAPI `IMMDevice` id); empty = OS default.
    /// Maps to `PUNKTFUNK_AUDIO_SINK`. A gone pick falls back to default.
    #[serde(default)]
    pub speaker_device: String,
    /// Capture endpoint; same semantics as `speaker_device` (`PUNKTFUNK_AUDIO_SOURCE`).
    #[serde(default)]
    pub mic_device: String,
    /// DualSense voice-coil haptics (0xD1 kind 0) on a wired pad's audio device.
    /// Gates `CLIENT_CAP_PAD_AUDIO`; wire rumble is suppressed while the stream is
    /// live (see `gamepad.rs`). Default on: no-op without a capable host and a wired DS5.
    #[serde(default = "default_true")]
    pub pad_haptics: bool,
    /// DualSense speaker stream (0xD1 kind 1): `"pad"` (default), `"mix"` (renders
    /// as `"off"` today; see `pad_audio::speaker_active`), or `"off"`.
    #[serde(default = "default_pad_speaker")]
    pub pad_speaker: String,
    /// Stream mode follows the session window (design/midstream-resolution-resize.md).
    /// Overrides `width`/`height` while on; fullscreen degenerates to the display's
    /// native mode. Default off until per-backend validation is green.
    pub match_window: bool,
    /// Last logical window size under `match_window`, so the next launch's first
    /// connect already matches. `0` = never stored → 1280×720.
    pub last_window_w: u32,
    pub last_window_h: u32,
    /// Keys this build does not model, carried through load→save so an older writer
    /// does not drop a newer client's fields. Empty map serializes to nothing.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn default_codec() -> String {
    "auto".into()
}

/// [`Settings::extra`] keys every client stores under these names. The console, Apple and
/// Android read them there too, so they stay out of the typed struct.
pub const FULLSCREEN_ALWAYS_KEY: &str = "fullscreen_always";
pub const GAMEPAD_UI_KEY: &str = "gamepad_ui_enabled";
pub const GAMEPAD_UI_MODE_KEY: &str = "gamepad_ui_mode";

/// When the controller-optimized UI (the console) takes over the desktop layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GamepadUi {
    Off,
    /// While a controller is attached — the default.
    WithController,
    /// Pad or no pad.
    Always,
}

/// Opus plane — the one an older client's store must load as. Named from `session`
/// so the default and the menu's first row cannot be two different strings.
fn default_audio_format() -> String {
    crate::audio_format::AUDIO_FORMAT_OPUS.into()
}

fn default_auto() -> String {
    "auto".into()
}

fn default_video_fit() -> String {
    "fit".into()
}

fn default_touch_mode() -> String {
    "trackpad".into()
}

fn default_mouse_mode() -> String {
    "capture".into()
}

fn default_present_priority() -> String {
    "latency".into()
}

fn default_true() -> bool {
    true
}

fn default_ui_palette() -> String {
    "violet".into()
}

fn default_pad_speaker() -> String {
    "pad".into()
}

fn default_stats_scale_pct() -> u16 {
    100
}

impl Settings {
    /// Overlay tier, resolving pre-tier stores: `show_stats = false` → Off, else Normal.
    pub fn stats_verbosity(&self) -> StatsVerbosity {
        self.stats_verbosity.unwrap_or(if self.show_stats {
            StatsVerbosity::Normal
        } else {
            StatsVerbosity::Off
        })
    }

    /// The window opens fullscreen and every stream goes fullscreen, whatever a preset
    /// says. Device-only: no preset carries it.
    pub fn fullscreen_always(&self) -> bool {
        self.extra_bool(FULLSCREEN_ALWAYS_KEY, false)
    }

    pub fn set_fullscreen_always(&mut self, on: bool) {
        self.extra
            .insert(FULLSCREEN_ALWAYS_KEY.into(), serde_json::Value::Bool(on));
    }

    /// `gamepad_ui_enabled` (default on) with `gamepad_ui_mode` (`"always"`, else with a
    /// controller). Device-only, like [`fullscreen_always`](Self::fullscreen_always).
    pub fn gamepad_ui(&self) -> GamepadUi {
        if !self.extra_bool(GAMEPAD_UI_KEY, true) {
            GamepadUi::Off
        } else if self.gamepad_ui_always() {
            GamepadUi::Always
        } else {
            GamepadUi::WithController
        }
    }

    /// The stored mode, kept while the switch is off so turning it back on restores it.
    pub fn gamepad_ui_always(&self) -> bool {
        self.extra.get(GAMEPAD_UI_MODE_KEY).and_then(|v| v.as_str()) == Some("always")
    }

    pub fn set_gamepad_ui_enabled(&mut self, on: bool) {
        self.extra
            .insert(GAMEPAD_UI_KEY.into(), serde_json::Value::Bool(on));
    }

    pub fn set_gamepad_ui_always(&mut self, always: bool) {
        let mode = if always { "always" } else { "connected" };
        self.extra
            .insert(GAMEPAD_UI_MODE_KEY.into(), serde_json::Value::from(mode));
    }

    fn extra_bool(&self, key: &str, default: bool) -> bool {
        self.extra
            .get(key)
            .and_then(|v| v.as_bool())
            .unwrap_or(default)
    }

    /// Set the tier, keeping the legacy `show_stats` bool coherent for pre-tier readers.
    pub fn set_stats_verbosity(&mut self, v: StatsVerbosity) {
        self.stats_verbosity = Some(v);
        self.show_stats = v != StatsVerbosity::Off;
    }

    /// The stats corner: the stored one, else `own`, the corner this client draws in by default.
    pub fn hud_corner(&self, own: HudCorner) -> HudCorner {
        HudCorner::from_name(&self.hud_placement).unwrap_or(own)
    }

    pub fn touch_mode(&self) -> TouchMode {
        TouchMode::from_name(&self.touch_mode)
    }

    pub fn mouse_mode(&self) -> MouseMode {
        MouseMode::from_name(&self.mouse_mode)
    }

    pub fn present_priority(&self) -> PresentPriority {
        PresentPriority::resolve(&self.present_priority, self.smooth_buffer)
    }

    /// Whether raw system-button presses (guide + QAM) go to the host.
    /// `game_mode`: auto keeps them local under gamescope (Steam UI reacts too).
    pub fn system_buttons_forward(&self, game_mode: bool) -> bool {
        match self.system_buttons.as_str() {
            "forward" => true,
            "local" => false,
            _ => !game_mode,
        }
    }

    /// Whether the hold-Select guide gesture is armed.
    /// Auto = on only under Gaming Mode, the sole controller route once raw presses stay local.
    pub fn guide_gesture_enabled(&self, game_mode: bool) -> bool {
        match self.guide_gesture.as_str() {
            "on" => true,
            "off" => false,
            _ => game_mode,
        }
    }

    /// The `codec` setting as a `quic::CODEC_*` preference bit (`0` = auto).
    #[cfg(not(target_family = "wasm"))]
    pub fn preferred_codec(&self) -> u8 {
        match self.codec.as_str() {
            "h264" | "avc" => punktfunk_core::quic::CODEC_H264,
            "hevc" | "h265" => punktfunk_core::quic::CODEC_HEVC,
            "av1" => punktfunk_core::quic::CODEC_AV1,
            // Wired-LAN wavelet: preference-only (`resolve_codec` never auto-picks it).
            // Harmless if the bit is not advertised — the ladder falls back to HEVC.
            "pyrowave" => punktfunk_core::quic::CODEC_PYROWAVE,
            _ => 0,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            width: 0,
            height: 0,
            refresh_hz: 0,
            bitrate_kbps: 0,
            render_scale: 1.0,
            video_fit: default_video_fit(),
            gamepad: "auto".into(),
            gamepad_forwarding: true,
            forward_pad: String::new(),
            system_buttons: "auto".into(),
            guide_gesture: "auto".into(),
            compositor: "auto".into(),
            touch_mode: "trackpad".into(),
            mouse_mode: "capture".into(),
            inhibit_shortcuts: true,
            mic_enabled: false,
            echo_cancel: true,
            audio_channels: 2,
            audio_format: default_audio_format(),
            keep_host_audio: false,
            codec: "auto".into(),
            decoder: "auto".into(),
            adapter: String::new(),
            enable_444: false,
            hdr_enabled: true,
            ten_bit_sdr: false,
            present_priority: "latency".into(),
            smooth_buffer: 0,
            vsync: true,
            allow_vrr: true,
            show_stats: true,
            stats_verbosity: None,
            advanced_stats: false,
            hud_placement: String::new(),
            stats_scale_pct: default_stats_scale_pct(),
            exit_hint: true,
            show_advanced: false,
            fullscreen_on_stream: true,
            ui_palette: default_ui_palette(),
            follow_os_theme: true,
            reduce_motion: false,
            library_sort: String::new(),
            library_view: String::new(),
            library_sections: String::new(),
            start_in: String::new(),
            default_host: None,
            auto_wake: true,
            invert_scroll: false,
            overlay_actions: String::new(),
            speaker_device: String::new(),
            mic_device: String::new(),
            pad_haptics: true,
            pad_speaker: "pad".into(),
            match_window: false,
            last_window_w: 0,
            last_window_h: 0,
            extra: BTreeMap::new(),
        }
    }
}

impl Settings {
    fn path() -> Result<PathBuf> {
        // GTK settings file on Linux, WinUI on Windows. Desktop shells and the session
        // console write it; a plain `--connect` stream only reads.
        #[cfg(windows)]
        return Ok(config_dir()?.join("client-windows-settings.json"));
        #[cfg(not(windows))]
        Ok(config_dir()?.join("client-gtk-settings.json"))
    }

    pub fn load() -> Settings {
        Self::path()
            .map(|p| load_json_or_default(&p))
            .unwrap_or_default()
    }

    /// Fire-and-forget (a failed write must never take a stream down), but temp+rename:
    /// five whole-file writers, and a torn file loads as `Default` — silent reset.
    pub fn save(&self) {
        let Ok(p) = Self::path() else { return };
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = write_atomic(&p, s.as_bytes());
        }
    }
}

/// Settings resolver every front-end and the session go through
/// (design/client-settings-profiles.md):
///
/// ```text
/// effective = overlay(preset).apply(global)
/// preset   = one-off override  ??  title binding  ??  host binding  ??  none
/// ```
///
/// `one_off` is Connect-with / `--preset` / `preset=`; `Some("")` forces globals
/// on a bound host and never rebinds. Unknown one-off → defaults (not a binding).
/// `launch` is the library title id, `None` for the desktop. The host is the record
/// [`KnownHosts::resolve`] names for the pin being dialled, as for the per-host clipboard:
/// a dual-boot box's address also names the other OS, with its own binding.
pub fn effective_settings(
    fp_hex: Option<&str>,
    addr: &str,
    port: u16,
    one_off: Option<&str>,
    launch: Option<&str>,
) -> (Settings, Option<StreamPreset>) {
    let base = Settings::load();
    let catalog = PresetsFile::load();
    let known = KnownHosts::load();
    let host = known.resolve(fp_hex, addr, port);
    let bound = host.and_then(|h| h.preset_id.clone());
    let per_game = launch
        .and_then(|game| host.and_then(|h| h.preset_for_game(game)))
        .map(str::to_string);

    match resolve_preset(&catalog, bound.as_deref(), per_game.as_deref(), one_off) {
        Some(p) => (p.overrides.apply(&base), Some(p)),
        None => (base, None),
    }
}

/// Preset half of [`effective_settings`], split so the precedence rules are testable
/// without touching the config directory: one-off ?? title ?? host ?? none.
///
/// A title binding beats the host's default because it is the more specific answer to
/// the same question — the host default is what a title with no opinion inherits. Public
/// for the clients that keep their own document (webOS) and must launch with this order.
pub fn resolve_preset(
    catalog: &PresetsFile,
    bound: Option<&str>,
    per_game: Option<&str>,
    one_off: Option<&str>,
) -> Option<StreamPreset> {
    match one_off {
        // `--preset ""` forces defaults on a bound host.
        Some("") => None,
        Some(reference) => match catalog.resolve(reference) {
            (Some(p), _) => Some(p.clone()),
            (_, res) => {
                tracing::warn!(
                    preset = %reference,
                    ambiguous = res == Resolution::Ambiguous,
                    "no such settings preset — streaming with the default settings"
                );
                None
            }
        },
        // Bindings are ids, never names — a rename must not hijack one. A dangling id
        // falls through to the next-most-general answer, the way a dangling pin just
        // disappears: the title's deleted preset leaves the host's default standing.
        None => {
            let find = |id: &str| catalog.find_by_id(id).cloned();
            per_game.and_then(find).or_else(|| bound.and_then(find))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pre-touch-mode store loads as `trackpad`; names round-trip through the enum.
    #[test]
    fn settings_touch_mode_defaults_trackpad() {
        let old = r#"{"width":1280,"height":720,"gamepad":"auto","compositor":"auto"}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.touch_mode, "trackpad");
        assert_eq!(s.touch_mode(), TouchMode::Trackpad);
        // Unknown name falls back to trackpad.
        assert_eq!(TouchMode::from_name("pointer"), TouchMode::Pointer);
        assert_eq!(TouchMode::from_name("touch"), TouchMode::Touch);
        assert_eq!(TouchMode::from_name("off"), TouchMode::Off);
        assert_eq!(TouchMode::from_name("bogus"), TouchMode::Trackpad);
        for m in TouchMode::ALL {
            assert_eq!(TouchMode::from_name(m.as_name()), m);
        }
    }

    /// A pre-presentation store loads latency / Automatic / tear-free / VRR.
    /// Anything but `"smooth"` is latency; a buffer outside 1..=3 becomes 2.
    #[test]
    fn settings_presentation_defaults_and_resolution() {
        let old = r#"{"width":1280,"height":720,"gamepad":"auto","compositor":"auto"}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.present_priority, "latency");
        assert_eq!(s.smooth_buffer, 0);
        assert!(s.vsync);
        assert!(s.allow_vrr);
        assert_eq!(s.present_priority(), PresentPriority::Latency);

        assert_eq!(
            PresentPriority::resolve("smooth", 0),
            PresentPriority::Smooth { buffer: 2 },
            "Automatic resolves to 2"
        );
        assert_eq!(
            PresentPriority::resolve("smooth", 3),
            PresentPriority::Smooth { buffer: 3 }
        );
        assert_eq!(
            PresentPriority::resolve("smooth", 9),
            PresentPriority::Smooth { buffer: 2 },
            "out-of-range pins to the Automatic resolution"
        );
        assert_eq!(
            PresentPriority::resolve("balanced-from-the-future", 2),
            PresentPriority::Latency,
            "unknown intents degrade to latency"
        );
        assert_eq!(PresentPriority::Latency.fifo_capacity(), 0);
        assert_eq!(PresentPriority::Smooth { buffer: 3 }.fifo_capacity(), 3);
    }

    /// A pre-`forward_pad` store loads with the pin on automatic.
    #[test]
    fn settings_forward_pad_defaults_empty() {
        let old = r#"{"width":1280,"height":720,"refresh_hz":60,"bitrate_kbps":0,
            "gamepad":"auto","compositor":"auto","inhibit_shortcuts":true,"mic_enabled":true}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.forward_pad, "");
        let round: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(round.forward_pad, "");
    }

    /// Older WinUI shell files still load: `show_hud` aliases onto `show_stats`,
    /// dropped `engine` is ignored, missing fields default.
    #[test]
    fn settings_reads_winui_shell_shape() {
        let shell = r#"{
            "width": 2560, "height": 1440, "refresh_hz": 120, "bitrate_kbps": 20000,
            "gamepad": "dualsense", "compositor": "auto",
            "inhibit_shortcuts": true, "mic_enabled": true, "audio_channels": 6,
            "hdr_enabled": true, "decoder": "hardware", "codec": "av1",
            "adapter": "NVIDIA GeForce RTX 4080", "show_hud": false, "engine": "builtin"
        }"#;
        let s: Settings = serde_json::from_str(shell).unwrap();
        assert_eq!((s.width, s.height, s.refresh_hz), (2560, 1440, 120));
        assert_eq!(s.bitrate_kbps, 20000);
        assert_eq!(s.audio_channels, 6);
        assert!(s.mic_enabled);
        assert_eq!(s.decoder, "hardware");
        assert_eq!(s.preferred_codec(), punktfunk_core::quic::CODEC_AV1);
        let mut pw = s.clone();
        pw.codec = "pyrowave".into();
        assert_eq!(pw.preferred_codec(), punktfunk_core::quic::CODEC_PYROWAVE);
        assert_eq!(s.adapter, "NVIDIA GeForce RTX 4080");
        assert!(s.hdr_enabled);
        assert!(!s.show_stats);
        assert_eq!(s.forward_pad, "");
        assert!(s.fullscreen_on_stream);
        // Echo cancellation post-dates every stored file: it must load on.
        assert!(s.echo_cancel);
    }

    /// The shared device keys read from `extra` with the other clients' defaults, and
    /// write back under the same names.
    #[test]
    fn shared_device_keys_live_in_extra() {
        let mut s = Settings::default();
        assert!(!s.fullscreen_always());
        assert_eq!(s.gamepad_ui(), GamepadUi::WithController);

        let stored: Settings = serde_json::from_str(
            r#"{"fullscreen_always":true,"gamepad_ui_enabled":true,"gamepad_ui_mode":"always"}"#,
        )
        .unwrap();
        assert!(stored.fullscreen_always());
        assert_eq!(stored.gamepad_ui(), GamepadUi::Always);

        s.set_gamepad_ui_always(true);
        s.set_gamepad_ui_enabled(false);
        assert_eq!(s.gamepad_ui(), GamepadUi::Off);
        assert!(s.gamepad_ui_always(), "off keeps the mode");
        s.set_fullscreen_always(true);
        let text = serde_json::to_string(&s).unwrap();
        for key in [FULLSCREEN_ALWAYS_KEY, GAMEPAD_UI_KEY, GAMEPAD_UI_MODE_KEY] {
            assert!(text.contains(&format!("\"{key}\"")), "{key} is written");
        }
        // A mode this build does not know reads as with a controller.
        s.set_gamepad_ui_enabled(true);
        s.extra.insert(GAMEPAD_UI_MODE_KEY.into(), "later".into());
        assert_eq!(s.gamepad_ui(), GamepadUi::WithController);
    }

    /// Unknown keys survive load→save. An empty flatten map adds nothing, so files
    /// without extras do not churn.
    #[test]
    fn settings_unknown_keys_survive_round_trip() {
        let newer = r#"{"width":1920,"height":1080,"frob_mode":"fancy","frob_level":3}"#;
        let s: Settings = serde_json::from_str(newer).unwrap();
        assert_eq!((s.width, s.height), (1920, 1080));
        assert_eq!(
            s.extra.get("frob_mode").and_then(|v| v.as_str()),
            Some("fancy")
        );
        let out = serde_json::to_string(&s).unwrap();
        assert!(out.contains(r#""frob_mode":"fancy""#), "{out}");
        assert!(out.contains(r#""frob_level":3"#), "{out}");
        // No unknown keys → no artifact of the passthrough field.
        let plain = serde_json::to_string(&Settings::default()).unwrap();
        assert!(!plain.contains("extra"), "{plain}");
        assert!(!plain.contains("frob"), "{plain}");
    }

    /// A retired key (`library_enabled`) must not fail the load, and must survive the
    /// next whole-file write so a downgrade still reads it.
    #[test]
    fn settings_retired_library_key_loads_and_survives() {
        let stored = r#"{"width":1920,"height":1080,"library_enabled":false}"#;
        let s: Settings = serde_json::from_str(stored).unwrap();
        assert_eq!((s.width, s.height), (1920, 1080));
        assert_eq!(
            s.extra.get("library_enabled").and_then(|v| v.as_bool()),
            Some(false)
        );
        let out = serde_json::to_string(&s).unwrap();
        assert!(out.contains(r#""library_enabled":false"#), "{out}");
    }

    /// An older store loads the overlay and settings-screen keys at their defaults, and a corner
    /// the console or Apple already wrote lands in the field, not in `extra`.
    #[test]
    fn overlay_keys_default_and_read_the_stored_corner() {
        let old: Settings = serde_json::from_str(r#"{"width":1920,"height":1080}"#).unwrap();
        assert_eq!(old.hud_placement, "");
        assert_eq!(old.hud_corner(HudCorner::TopLeft), HudCorner::TopLeft);
        assert_eq!(old.stats_scale_pct, 100);
        assert!(old.exit_hint);
        assert!(!old.show_advanced);

        let placed: Settings =
            serde_json::from_str(r#"{"hud_placement":"bottomTrailing"}"#).unwrap();
        assert_eq!(
            placed.hud_corner(HudCorner::TopLeft),
            HudCorner::BottomRight
        );
        assert!(placed.extra.is_empty(), "{:?}", placed.extra);
    }

    /// Pre-tier store falls back to `show_stats`; setting a tier keeps the legacy bool in sync.
    #[test]
    fn stats_verbosity_migrates_and_round_trips() {
        let mut s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.stats_verbosity(), StatsVerbosity::Normal);
        let off: Settings = serde_json::from_str(r#"{"show_stats":false}"#).unwrap();
        assert_eq!(off.stats_verbosity(), StatsVerbosity::Off);

        s.set_stats_verbosity(StatsVerbosity::Compact);
        assert!(s.show_stats);
        s.set_stats_verbosity(StatsVerbosity::Off);
        assert!(!s.show_stats);

        s.set_stats_verbosity(StatsVerbosity::Detailed);
        let round: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(round.stats_verbosity(), StatsVerbosity::Detailed);
        // Lowercase so the file stays readable.
        assert!(serde_json::to_string(&s).unwrap().contains("\"detailed\""));
    }

    /// One-off beats binding; `""` forces defaults; unknown one-off falls back to
    /// defaults, not the host's preset.
    #[test]
    fn preset_resolution_precedence() {
        use crate::presets::{PresetsFile, StreamPreset};
        let catalog = PresetsFile {
            version: 1,
            presets: vec![
                StreamPreset {
                    id: "aaaaaaaaaaaa".into(),
                    name: "Game".into(),
                    ..StreamPreset::new("")
                },
                StreamPreset {
                    id: "bbbbbbbbbbbb".into(),
                    name: "Work".into(),
                    ..StreamPreset::new("")
                },
                StreamPreset {
                    id: "cccccccccccc".into(),
                    name: "work".into(),
                    ..StreamPreset::new("")
                },
            ],
        };
        let name_of = |p: Option<StreamPreset>| p.map(|p| p.name);

        assert_eq!(resolve_preset(&catalog, None, None, None), None);
        assert_eq!(
            name_of(resolve_preset(&catalog, Some("aaaaaaaaaaaa"), None, None)),
            Some("Game".into())
        );
        assert_eq!(
            name_of(resolve_preset(
                &catalog,
                Some("aaaaaaaaaaaa"),
                None,
                Some("bbbbbbbbbbbb")
            )),
            Some("Work".into())
        );
        assert_eq!(
            name_of(resolve_preset(&catalog, None, None, Some("GAME"))),
            Some("Game".into())
        );
        assert_eq!(
            resolve_preset(&catalog, Some("aaaaaaaaaaaa"), None, Some("")),
            None
        );
        assert_eq!(
            resolve_preset(&catalog, Some("deleted00000"), None, None),
            None
        );
        assert_eq!(
            resolve_preset(&catalog, Some("aaaaaaaaaaaa"), None, Some("nope")),
            None
        );
        assert_eq!(
            resolve_preset(&catalog, Some("aaaaaaaaaaaa"), None, Some("work")),
            None
        );
        // Binding is by id only — a preset named like the bound id must not hijack it.
        assert_eq!(resolve_preset(&catalog, Some("Game"), None, None), None);
    }

    /// A title's binding sits between the one-off and the host's default: more specific
    /// than the host, still overridable for a single launch. A dangling one falls through
    /// to the host rather than to the defaults.
    #[test]
    fn a_title_binding_outranks_the_host_and_yields_to_a_one_off() {
        use crate::presets::{PresetsFile, StreamPreset};
        let catalog = PresetsFile {
            version: 1,
            presets: vec![
                StreamPreset {
                    id: "aaaaaaaaaaaa".into(),
                    name: "Game".into(),
                    ..StreamPreset::new("")
                },
                StreamPreset {
                    id: "bbbbbbbbbbbb".into(),
                    name: "Work".into(),
                    ..StreamPreset::new("")
                },
            ],
        };
        let name_of = |p: Option<StreamPreset>| p.map(|p| p.name);
        let (host, game) = (Some("aaaaaaaaaaaa"), Some("bbbbbbbbbbbb"));

        assert_eq!(
            name_of(resolve_preset(&catalog, host, game, None)),
            Some("Work".into())
        );
        // No binding on the title: the host's default is what it inherits.
        assert_eq!(
            name_of(resolve_preset(&catalog, host, None, None)),
            Some("Game".into())
        );
        // A pinned card's one-off still wins over both.
        assert_eq!(
            name_of(resolve_preset(&catalog, host, game, Some("Game"))),
            Some("Game".into())
        );
        // `--preset ""` forces the globals past a title binding too.
        assert_eq!(resolve_preset(&catalog, host, game, Some("")), None);
        // Deleted title preset: the host's default stands, not the defaults.
        assert_eq!(
            name_of(resolve_preset(&catalog, host, Some("deleted00000"), None)),
            Some("Game".into())
        );
        assert_eq!(
            resolve_preset(&catalog, None, Some("deleted00000"), None),
            None
        );
    }
}
