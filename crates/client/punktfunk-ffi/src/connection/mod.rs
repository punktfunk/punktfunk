//! `quic` client connector: the connection handle and the wire vocabulary its calls
//! share. Header symbols sit under `PUNKTFUNK_FEATURE_QUIC`.

// Declaration order is header order, as in `lib.rs`; blank lines keep rustfmt from sorting.
mod connect;

mod planes;

mod pad;

mod video;

mod input;

mod host;

mod clipboard;

mod control;

pub use clipboard::*;
pub use connect::*;
pub use control::*;
#[cfg(feature = "quic")]
pub use host::*;
#[cfg(feature = "quic")]
pub use input::*;
pub use pad::*;
#[cfg(feature = "quic")]
pub use planes::*;
pub use video::*;

#[cfg(feature = "quic")]
use crate::*;

/// Live `punktfunk/1` connection (QUIC control + UDP data, pumped on internal threads).
/// One puller thread per plane; never two threads on the same plane.
#[cfg(feature = "quic")]
pub struct PunktfunkConnection {
    inner: punktfunk_core::client::NativeClient,
    /// Last `next_au` payload. Pointer valid until the next video pull.
    last: std::sync::Mutex<Option<punktfunk_core::session::Frame>>,
    /// Last `next_audio` payload. Independent of the video slot.
    last_audio: std::sync::Mutex<Option<punktfunk_core::client::AudioPacket>>,
    /// In-core PCM decode. Returned pointer valid until the next PCM call.
    audio_pcm: std::sync::Mutex<AudioPcmState>,
    /// Last clipboard payload. Pointer valid until the next `next_clipboard`.
    last_clip: std::sync::Mutex<Option<Vec<u8>>>,
    /// Last cursor RGBA. Pointer valid until the next cursor-shape call.
    last_cursor_shape: std::sync::Mutex<Option<punktfunk_core::quic::CursorShape>>,
    /// Stats overlay window as last drained; `hud_text` formats it at any tier.
    hud_snap: std::sync::Mutex<punktfunk_core::hud::StatsSnapshot>,
}

// Plane pullers and the demo host's callers use these handles from several threads at once.
#[cfg(feature = "quic")]
const _: fn() = || {
    fn shared<T: Send + Sync>() {}
    shared::<PunktfunkConnection>();
    shared::<PunktfunkDemoHost>();
};

/// `PunktfunkHidOutput::kind`: lightbar RGB (`r`/`g`/`b` valid).
pub const PUNKTFUNK_HIDOUT_LED: u8 = 1;
/// `PunktfunkHidOutput::kind`: player-indicator LEDs (`player_bits` valid, low 5 bits).
pub const PUNKTFUNK_HIDOUT_PLAYER_LEDS: u8 = 2;
/// `PunktfunkHidOutput::kind`: one adaptive-trigger effect (`which` + `effect`/`effect_len` valid).
pub const PUNKTFUNK_HIDOUT_TRIGGER: u8 = 3;
/// Trackpad haptic pulse. `which` = side (0 right, 1 left); `effect[0..6]` =
/// amplitude/period/count as LE `u16`, `effect_len = 6`. Drop if no coils.
pub const PUNKTFUNK_HIDOUT_TRACKPAD_HAPTIC: u8 = 4;
/// DS5 audio-control region. Samples arrive via [`punktfunk_connection_next_pad_audio`].
/// `which` = flags; `effect[0..6]` = report bytes 5..=10; `effect_len = 6`. Change-only.
pub const PUNKTFUNK_HIDOUT_AUDIO_CTL: u8 = 5;
/// Raw hidraw report to replay (`HidRaw`). `hid_kind` + `raw`/`raw_len` valid.
/// Only `PUNKTFUNK_GAMEPAD_STEAMCONTROLLER2` emits these; others drop.
pub const PUNKTFUNK_HIDOUT_HID_RAW: u8 = 6;
/// `PunktfunkHidOutput::kind`: DualSense microphone-mute LED. `which` = mode (0 off, 1 on,
/// 2 pulse).
pub const PUNKTFUNK_HIDOUT_MIC_LED: u8 = 7;
/// Capacity of `PunktfunkHidOutput::effect` (DualSense trigger parameter block).
pub const PUNKTFUNK_HID_EFFECT_MAX: u8 = 11;
// The wire clamps a trigger effect to this length; `effect` must hold it whole.
const _: () =
    assert!(PUNKTFUNK_HID_EFFECT_MAX as usize == punktfunk_core::quic::TRIGGER_EFFECT_MAX);

/// HID-output feedback from the host virtual pad ([`punktfunk_connection_next_hidout`]).
/// `kind` selects which fields to replay on the physical controller.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkHidOutput {
    /// One of `PUNKTFUNK_HIDOUT_*`.
    pub kind: u8,
    /// Gamepad index.
    pub pad: u8,
    /// LED: lightbar red.
    pub r: u8,
    /// LED: lightbar green.
    pub g: u8,
    /// LED: lightbar blue.
    pub b: u8,
    /// PlayerLeds: lit player indicators (low 5 bits).
    pub player_bits: u8,
    /// Trigger: 0 = L2, 1 = R2.
    pub which: u8,
    /// Trigger: number of valid bytes in `effect` (≤ `PUNKTFUNK_HID_EFFECT_MAX`).
    pub effect_len: u8,
    /// DualSense trigger parameter block. Length is [`PUNKTFUNK_HID_EFFECT_MAX`], not a second `11`.
    pub effect: [u8; PUNKTFUNK_HID_EFFECT_MAX as usize],
    /// HidRaw channel: `PUNKTFUNK_HID_RAW_OUTPUT` or `PUNKTFUNK_HID_RAW_FEATURE`.
    pub hid_kind: u8,
    /// HidRaw: number of valid bytes in `raw` (≤ `PUNKTFUNK_HID_REPORT_MAX`).
    pub raw_len: u8,
    /// HidRaw report, id byte first. Feature frames may be zero-padded; sized off `HID_REPORT_MAX`.
    pub raw: [u8; punktfunk_core::quic::HID_REPORT_MAX],
}

#[cfg(feature = "quic")]
impl PunktfunkHidOutput {
    /// Map every [`HidOutput`](punktfunk_core::quic::HidOutput) variant. `HidRaw` uses `raw`/`hid_kind`.
    fn from_hid(h: &punktfunk_core::quic::HidOutput) -> PunktfunkHidOutput {
        use punktfunk_core::quic::HidOutput;
        let mut out = PunktfunkHidOutput {
            kind: 0,
            pad: 0,
            r: 0,
            g: 0,
            b: 0,
            player_bits: 0,
            which: 0,
            effect_len: 0,
            effect: [0u8; 11],
            hid_kind: 0,
            raw_len: 0,
            raw: [0u8; punktfunk_core::quic::HID_REPORT_MAX],
        };
        match h {
            HidOutput::Led { pad, r, g, b } => {
                out.kind = PUNKTFUNK_HIDOUT_LED;
                out.pad = *pad;
                out.r = *r;
                out.g = *g;
                out.b = *b;
            }
            HidOutput::PlayerLeds { pad, bits } => {
                out.kind = PUNKTFUNK_HIDOUT_PLAYER_LEDS;
                out.pad = *pad;
                out.player_bits = *bits;
            }
            HidOutput::Trigger { pad, which, effect } => {
                out.kind = PUNKTFUNK_HIDOUT_TRIGGER;
                out.pad = *pad;
                out.which = *which;
                let n = effect.len().min(out.effect.len());
                out.effect[..n].copy_from_slice(&effect[..n]);
                out.effect_len = n as u8;
            }
            HidOutput::TrackpadHaptic {
                pad,
                side,
                amplitude,
                period,
                count,
            } => {
                // No size guard: pack into `which` + `effect[0..6]` LE, `effect_len = 6`.
                out.kind = PUNKTFUNK_HIDOUT_TRACKPAD_HAPTIC;
                out.pad = *pad;
                out.which = *side;
                out.effect[0..2].copy_from_slice(&amplitude.to_le_bytes());
                out.effect[2..4].copy_from_slice(&period.to_le_bytes());
                out.effect[4..6].copy_from_slice(&count.to_le_bytes());
                out.effect_len = 6;
            }
            HidOutput::HidRaw { pad, kind, data } => {
                out.kind = PUNKTFUNK_HIDOUT_HID_RAW;
                out.pad = *pad;
                out.hid_kind = *kind;
                // `decode` already bounds; clamp so a local oversize cannot overrun `raw`.
                let n = data.len().min(out.raw.len());
                out.raw[..n].copy_from_slice(&data[..n]);
                out.raw_len = n as u8;
            }
            HidOutput::AudioCtl { pad, flags, raw } => {
                // Same pack as TrackpadHaptic. `pad as u8` is lossless: decode rejects ≥ MAX_PADS.
                out.kind = PUNKTFUNK_HIDOUT_AUDIO_CTL;
                out.pad = *pad as u8;
                out.which = *flags;
                out.effect[0..6].copy_from_slice(raw);
                out.effect_len = 6;
            }
            HidOutput::MicLed { pad, mode } => {
                out.kind = PUNKTFUNK_HIDOUT_MIC_LED;
                out.pad = *pad;
                out.which = *mode;
            }
        }
        out
    }
}

/// `PunktfunkRichInput::kind`: a touchpad contact (`finger`/`active`/`x`/`y` valid).
pub const PUNKTFUNK_RICH_TOUCHPAD: u8 = 1;
/// `PunktfunkRichInput::kind`: a motion sample (`gyro`/`accel` valid).
pub const PUNKTFUNK_RICH_MOTION: u8 = 2;
/// `RichInput::TouchpadEx` on the wire: surface (0 single / 1 Steam-left / 2
/// Steam-right) plus click + pressure. C send path is size-prefixed
/// `PunktfunkRichInputEx` via `punktfunk_connection_send_rich_input2`.
pub const PUNKTFUNK_RICH_TOUCHPAD_EX: u8 = 3;
/// `RichInput::HidReport` on the wire (`[0xCC][0x04][pad][len][data…]`): raw HID
/// input for the host as-is pad (`PUNKTFUNK_GAMEPAD_STEAMCONTROLLER2`). C clients
/// send via [`punktfunk_connection_send_hid_report`], never by building the datagram.
pub const PUNKTFUNK_RICH_HID_REPORT: u8 = 4;

/// [`punktfunk_connection_set_sc2_gate`] bit: the client's overlay owns the pad. Its raw state
/// goes out neutral, and a button held now stays off the wire until it is released.
pub const PUNKTFUNK_SC2_GATE_MASKED: u32 = 1;
/// [`punktfunk_connection_set_sc2_gate`] bit: Steam and QAM stay with the client.
pub const PUNKTFUNK_SC2_GATE_SYSTEM_LOCAL: u32 = 2;
/// [`punktfunk_connection_set_sc2_gate`] bit: the client opens its ring on Select then A, so
/// that chord stays off the wire.
pub const PUNKTFUNK_SC2_GATE_CHORDS: u32 = 4;

/// One rich client→host input for the host virtual DualSense
/// ([`punktfunk_connection_send_rich_input`]): touchpad contact or motion sample.
/// Set `kind` and the matching fields; the others are ignored.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkRichInput {
    /// One of `PUNKTFUNK_RICH_*`.
    pub kind: u8,
    /// Gamepad index.
    pub pad: u8,
    /// Touchpad: contact id (0 or 1).
    pub finger: u8,
    /// Touchpad: 1 = finger down, 0 = lifted.
    pub active: u8,
    /// Touchpad: normalized x, 0..=65535 across the touchpad.
    pub x: u16,
    /// Touchpad: normalized y, 0..=65535 across the touchpad.
    pub y: u16,
    /// Motion: gyro (pitch, yaw, roll), raw signed-16.
    pub gyro: [i16; 3],
    /// Motion: accelerometer (x, y, z), raw signed-16.
    pub accel: [i16; 3],
}

#[cfg(feature = "quic")]
impl PunktfunkRichInput {
    fn to_rich(self) -> Option<punktfunk_core::quic::RichInput> {
        use punktfunk_core::quic::RichInput;
        match self.kind {
            PUNKTFUNK_RICH_TOUCHPAD => Some(RichInput::Touchpad {
                pad: self.pad,
                finger: self.finger,
                active: self.active != 0,
                x: self.x,
                y: self.y,
            }),
            PUNKTFUNK_RICH_MOTION => Some(RichInput::Motion {
                pad: self.pad,
                gyro: self.gyro,
                accel: self.accel,
            }),
            _ => None,
        }
    }
}

/// Superset of [`PunktfunkRichInput`] for `TouchpadEx` (second pad, click, signed
/// coords, pressure). Set `struct_size = sizeof(PunktfunkRichInputEx)`.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkRichInputEx {
    /// Must equal `sizeof(PunktfunkRichInputEx)`.
    pub struct_size: u32,
    /// One of `PUNKTFUNK_RICH_*` (`TOUCHPAD` / `MOTION` / `TOUCHPAD_EX`).
    pub kind: u8,
    /// Gamepad index.
    pub pad: u8,
    /// Touchpad/TouchpadEx: contact id.
    pub finger: u8,
    /// Touchpad/TouchpadEx: 1 = finger down / touching, 0 = lifted.
    pub active: u8,
    /// TouchpadEx: surface — 0 = single/DualSense, 1 = Steam left pad, 2 = Steam right pad.
    pub surface: u8,
    /// TouchpadEx: 1 = the pad is physically clicked, distinct from a touch contact.
    pub click: u8,
    /// Reserved for alignment; set to 0.
    pub _reserved: [u8; 2],
    /// TouchpadEx: x, **signed**, centred at 0 (Steam report convention). For a
    /// `TOUCHPAD` kind through this struct, store the unsigned `0..=65535` bits.
    pub x: i16,
    /// TouchpadEx: y, signed, centred at 0.
    pub y: i16,
    /// TouchpadEx: contact pressure (`0` if the surface has no force sensor).
    pub pressure: u16,
    /// Motion: gyro (pitch, yaw, roll), raw signed-16.
    pub gyro: [i16; 3],
    /// Motion: accelerometer (x, y, z), raw signed-16.
    pub accel: [i16; 3],
}

#[cfg(feature = "quic")]
impl PunktfunkRichInputEx {
    fn to_rich(self) -> Option<punktfunk_core::quic::RichInput> {
        use punktfunk_core::quic::RichInput;
        match self.kind {
            PUNKTFUNK_RICH_TOUCHPAD_EX => Some(RichInput::TouchpadEx {
                pad: self.pad,
                surface: self.surface,
                finger: self.finger,
                touch: self.active != 0,
                click: self.click != 0,
                x: self.x,
                y: self.y,
                pressure: self.pressure,
            }),
            PUNKTFUNK_RICH_MOTION => Some(RichInput::Motion {
                pad: self.pad,
                gyro: self.gyro,
                accel: self.accel,
            }),
            PUNKTFUNK_RICH_TOUCHPAD => Some(RichInput::Touchpad {
                pad: self.pad,
                finger: self.finger,
                active: self.active != 0,
                x: self.x as u16,
                y: self.y as u16,
            }),
            _ => None,
        }
    }
}

/// [`PunktfunkPenSample::state`] bit: the pen hovers in range (implied by `TOUCHING`).
pub const PUNKTFUNK_PEN_IN_RANGE: u8 = 0x01;
/// [`PunktfunkPenSample::state`] bit: the tip is in contact.
pub const PUNKTFUNK_PEN_TOUCHING: u8 = 0x02;
/// [`PunktfunkPenSample::state`] bit: primary barrel button (or squeeze mapping) held.
pub const PUNKTFUNK_PEN_BARREL1: u8 = 0x04;
/// [`PunktfunkPenSample::state`] bit: secondary barrel button (or double-tap mapping) held.
pub const PUNKTFUNK_PEN_BARREL2: u8 = 0x08;
/// [`PunktfunkPenSample::tool`]: the pen tip.
pub const PUNKTFUNK_PEN_TOOL_PEN: u8 = 0;
/// [`PunktfunkPenSample::tool`]: the eraser. Client-side mode — no hardware eraser
/// end; squeeze/double-tap mapping usually drives this.
pub const PUNKTFUNK_PEN_TOOL_ERASER: u8 = 1;
/// Most samples one [`punktfunk_connection_send_pen`] call accepts (one wire batch).
pub const PUNKTFUNK_PEN_BATCH_MAX: u32 = 8;
/// [`PunktfunkPenSample::tilt_deg`] sentinel: no tilt reading.
pub const PUNKTFUNK_PEN_TILT_UNKNOWN: u8 = 0xFF;
/// Longest profile id, in bytes, that [`PunktfunkConnectOpts::profile_id`] sends and
/// [`punktfunk_connection_profile`] writes (before the NUL).
pub const PUNKTFUNK_PROFILE_ID_MAX: usize = 64;
/// [`PunktfunkPenSample::azimuth_deg`] / `roll_deg` sentinel: no reading.
pub const PUNKTFUNK_PEN_ANGLE_UNKNOWN: u16 = 0xFFFF;
/// [`PunktfunkPenSample::distance`] sentinel: no hover-distance reading.
pub const PUNKTFUNK_PEN_DISTANCE_UNKNOWN: u16 = 0xFFFF;

/// Full stylus state at one instant ([`punktfunk_connection_send_pen`];
/// `design/pen-tablet-input.md`). Fill every field (`*_UNKNOWN` if missing); the
/// host diffs samples. `x`/`y` are `0.0..=1.0` in video-frame space.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkPenSample {
    /// Normalized `0.0..=1.0` across the video frame. Must be finite.
    pub x: f32,
    /// Normalized `0.0..=1.0` across the video frame. Must be finite.
    pub y: f32,
    /// Tip force, `0..=65535` full scale (`0` while hovering).
    pub pressure: u16,
    /// Hover distance `0..=65534` (0 = at the hover floor), or `PUNKTFUNK_PEN_DISTANCE_UNKNOWN`.
    pub distance: u16,
    /// Tilt azimuth, degrees `0..=359` clockwise from north, or `PUNKTFUNK_PEN_ANGLE_UNKNOWN`.
    pub azimuth_deg: u16,
    /// Barrel roll (Apple Pencil Pro `rollAngle`), degrees `0..=359`, or
    /// `PUNKTFUNK_PEN_ANGLE_UNKNOWN`.
    pub roll_deg: u16,
    /// µs since the previous sample in the same call (`0` for the first) — coalesced
    /// capture spacing.
    pub dt_us: u16,
    /// Bitfield of `PUNKTFUNK_PEN_*` state bits. Unknown bits are rejected (`InvalidArg`).
    pub state: u8,
    /// `PUNKTFUNK_PEN_TOOL_PEN` or `PUNKTFUNK_PEN_TOOL_ERASER`.
    pub tool: u8,
    /// Tilt from the surface normal, degrees `0..=90`, or `PUNKTFUNK_PEN_TILT_UNKNOWN`.
    pub tilt_deg: u8,
    /// Set to 0.
    pub _reserved: [u8; 3],
}

#[cfg(feature = "quic")]
impl PunktfunkPenSample {
    /// `None` = invalid field (non-finite coordinate, unknown state bit, unknown tool).
    /// Embedder input is validated strictly, unlike the loss-tolerant wire decode.
    fn to_sample(self) -> Option<punktfunk_core::quic::PenSample> {
        use punktfunk_core::quic as q;
        let known = q::PEN_IN_RANGE | q::PEN_TOUCHING | q::PEN_BARREL1 | q::PEN_BARREL2;
        if !self.x.is_finite() || !self.y.is_finite() || self.state & !known != 0 {
            return None;
        }
        let tool = match self.tool {
            PUNKTFUNK_PEN_TOOL_PEN => q::PenTool::Pen,
            PUNKTFUNK_PEN_TOOL_ERASER => q::PenTool::Eraser,
            _ => return None,
        };
        Some(q::PenSample {
            state: self.state,
            tool,
            x: self.x,
            y: self.y,
            pressure: self.pressure,
            distance: self.distance,
            tilt_deg: self.tilt_deg,
            azimuth_deg: self.azimuth_deg,
            roll_deg: self.roll_deg,
            dt_us: self.dt_us,
        })
    }
}

/// Compositor preference for [`punktfunk_connect_ex`]. `AUTO` lets the host pick.
/// A concrete value is honored only if that backend is available now; else auto-detect.
/// Resolved choice is on `Welcome`.
pub const PUNKTFUNK_COMPOSITOR_AUTO: u32 = 0;
/// KWin / KDE Plasma.
pub const PUNKTFUNK_COMPOSITOR_KWIN: u32 = 1;
/// wlroots (Sway / River). Older clients sent it for Hyprland too; the host still honors that.
pub const PUNKTFUNK_COMPOSITOR_WLROOTS: u32 = 2;
/// Mutter / GNOME.
pub const PUNKTFUNK_COMPOSITOR_MUTTER: u32 = 3;
/// gamescope (spawned nested).
pub const PUNKTFUNK_COMPOSITOR_GAMESCOPE: u32 = 4;
/// Hyprland.
pub const PUNKTFUNK_COMPOSITOR_HYPRLAND: u32 = 5;
/// A Windows host's virtual display. Only ever resolved, never requested.
pub const PUNKTFUNK_COMPOSITOR_WINDOWS: u32 = 6;

/// Gamepad-backend preference for [`punktfunk_connect_ex2`]: which virtual pad
/// the host creates. Precedence: client choice > `PUNKTFUNK_GAMEPAD` env > X-Box 360.
/// `AUTO` (or unrecognized) = host decides. Resolved via [`punktfunk_connection_gamepad`].
pub const PUNKTFUNK_GAMEPAD_AUTO: u32 = 0;
/// uinput X-Box 360 pad (default — every game speaks XInput).
pub const PUNKTFUNK_GAMEPAD_XBOX360: u32 = 1;
/// UHID DualSense (`hid-playstation`): adaptive triggers, lightbar, touchpad, motion.
/// Feedback on [`punktfunk_connection_next_hidout`]. Linux UHID / Windows UMDF; else X-Box 360.
pub const PUNKTFUNK_GAMEPAD_DUALSENSE: u32 = 2;
/// X-Box One/Series identity on the 360 backend (glyphs). No impulse-trigger rumble:
/// evdev `FF_RUMBLE` has two magnitudes. Windows HID Xbox can; see `next_rumble_cmd2`.
pub const PUNKTFUNK_GAMEPAD_XBOXONE: u32 = 3;
/// UHID DualShock 4 (`hid-playstation`): lightbar, touchpad, motion, rumble.
/// Rich-input + HID-output planes, no adaptive triggers / player LEDs / mute.
/// Linux UHID / Windows UMDF; else X-Box 360.
pub const PUNKTFUNK_GAMEPAD_DUALSHOCK4: u32 = 4;
/// UHID classic Steam Controller (`hid-steam`): one stick + dual trackpads + two
/// grip paddles. Linux only; else Xbox 360.
pub const PUNKTFUNK_GAMEPAD_STEAMCONTROLLER: u32 = 5;
/// Steam Deck controller: four back grips, both trackpads, IMU. Steam Input
/// re-grabs with native glyphs when Steam runs on the host. Linux/Windows; else X-Box 360.
pub const PUNKTFUNK_GAMEPAD_STEAMDECK: u32 = 6;
/// DualSense Edge: DualSense plus two back buttons + two Fn buttons, so client
/// paddles land on native slots. Linux UHID / Windows UMDF; else X-Box 360.
pub const PUNKTFUNK_GAMEPAD_DUALSENSEEDGE: u32 = 7;
/// Nintendo Switch Pro: Nintendo glyphs + positional layout, gyro/accel, HD rumble.
/// Linux UHID `hid-nintendo` only; else X-Box 360.
pub const PUNKTFUNK_GAMEPAD_SWITCHPRO: u32 = 8;
/// Steam Controller 2 as-is passthrough. Linux UHID; else X-Box 360.
pub const PUNKTFUNK_GAMEPAD_STEAMCONTROLLER2: u32 = 9;
/// Steam Controller Puck dongle: native seven-interface topology, four slots.
/// For capture clients that own the physical Puck; wired/BLE SC2 stays `STEAMCONTROLLER2`.
pub const PUNKTFUNK_GAMEPAD_STEAMCONTROLLER2_PUCK: u32 = 10;
/// Xbox Elite identity. Linux hosts carry the paddles; Windows hosts drop them.
pub const PUNKTFUNK_GAMEPAD_XBOXELITE: u32 = 11;
/// 8BitDo Ultimate 2 Wireless (`2DC8:6012`): L4/R4, back paddles, gyro.
pub const PUNKTFUNK_GAMEPAD_8BITDO_ULTIMATE2: u32 = 12;
/// 8BitDo Pro 2 (`2DC8:6003`): back paddles, gyro.
pub const PUNKTFUNK_GAMEPAD_8BITDO_PRO2: u32 = 13;
/// 8BitDo Pro 3 (`2DC8:6009`): L4/R4, back paddles, gyro.
pub const PUNKTFUNK_GAMEPAD_8BITDO_PRO3: u32 = 14;
/// Wireless HORIPAD for Steam (`0F0D:01AB`): rear buttons, gyro.
pub const PUNKTFUNK_GAMEPAD_HORIPAD_STEAM: u32 = 15;
/// Joy-Con pair (`057E:2006` + `2007`): SL/SR as paddles, gyro.
pub const PUNKTFUNK_GAMEPAD_JOYCON_PAIR: u32 = 16;
/// Switch 2 Pro Controller (`057E:2069`): GL/GR, gyro. Linux hosts only (usbip).
pub const PUNKTFUNK_GAMEPAD_SWITCH2_PRO: u32 = 17;
/// Switch 2 GameCube controller (`057E:2073`): analog triggers. Linux hosts only (usbip).
pub const PUNKTFUNK_GAMEPAD_SWITCH2_GAMECUBE: u32 = 18;

/// Extended `InputEvent` gamepad button bits: four back grips (Steam L4/L5/R4/R5 ≙
/// Xbox-Elite P1–P4) + misc/capture, in Moonlight's `buttonFlags2 << 16` namespace.
/// Mirror `input::gamepad::BTN_PADDLE1..4` / `BTN_MISC1`.
pub const PUNKTFUNK_GAMEPAD_BTN_PADDLE1: u32 = 0x0001_0000;
pub const PUNKTFUNK_GAMEPAD_BTN_PADDLE2: u32 = 0x0002_0000;
pub const PUNKTFUNK_GAMEPAD_BTN_PADDLE3: u32 = 0x0004_0000;
pub const PUNKTFUNK_GAMEPAD_BTN_PADDLE4: u32 = 0x0008_0000;
pub const PUNKTFUNK_GAMEPAD_BTN_MISC1: u32 = 0x0020_0000;

/// Connect to a `punktfunk/1` host at `width`x`height`@`refresh_hz`. Blocks up to
/// `timeout_ms`. NULL on failure. Same as [`punktfunk_connect_ex`] with
/// `compositor = PUNKTFUNK_COMPOSITOR_AUTO`.
///
/// Video-capability bit for [`punktfunk_connect_ex5`]: the client can decode
/// 10-bit (Main10) HEVC.
pub const PUNKTFUNK_VIDEO_CAP_10BIT: u8 = 0x01;
/// Video-capability bit: the client can present BT.2020 PQ HDR10 (implies 10-bit).
pub const PUNKTFUNK_VIDEO_CAP_HDR: u8 = 0x02;
/// Video-capability bit: the client can decode full-chroma 4:4:4 HEVC. The host
/// emits 4:4:4 only when this is set, the host opted in, the codec is HEVC, and
/// the GPU supports it — else 4:2:0. Read [`punktfunk_connection_chroma_format`].
pub const PUNKTFUNK_VIDEO_CAP_444: u8 = 0x04;

/// Codec bit for [`punktfunk_connect_ex7`] and [`punktfunk_connection_codec`]: H.264 / AVC.
pub const PUNKTFUNK_CODEC_H264: u8 = 0x01;
/// Codec bit: H.265 / HEVC — the default codec.
pub const PUNKTFUNK_CODEC_HEVC: u8 = 0x02;
/// Codec bit: AV1.
pub const PUNKTFUNK_CODEC_AV1: u8 = 0x04;
/// PyroWave. Never auto-selected; pass it as `preferred_codec` (`design/pyrowave-codec-plan.md`).
pub const PUNKTFUNK_CODEC_PYROWAVE: u8 = 0x08;

/// Host-capability bit: the host applies gamepad-state snapshots (a capable client
/// sends full-state snapshots instead of per-transition events).
pub const PUNKTFUNK_HOST_CAP_GAMEPAD_STATE: u8 = 0x01;
/// Host-capability bit: the host supports the shared clipboard; a client may offer the toggle.
pub const PUNKTFUNK_HOST_CAP_CLIPBOARD: u8 = 0x02;
/// Host injects stylus. Without it [`punktfunk_connection_send_pen`] is `Unsupported`.
/// (`design/pen-tablet-input.md`.)
pub const PUNKTFUNK_HOST_CAP_PEN: u8 = 0x10;
/// Host-capability bit: per-gamepad audio (DualSense voice-coil + speaker) on
/// the 0xD1 plane toward pads declared via [`punktfunk_connection_set_pad_audio_caps`].
/// Set only when the client asked via [`PUNKTFUNK_CLIENT_CAP_PAD_AUDIO`].
pub const PUNKTFUNK_HOST_CAP_PAD_AUDIO: u8 = 0x40;
/// Session is on lossless `0xD3`, not Opus. Distinguishes 48 kHz/16-bit PCM from
/// 48 kHz Opus when draining [`punktfunk_connection_next_audio`]. PCM decode path
/// does not need it; still read [`punktfunk_connection_audio_sample_rate`].
pub const PUNKTFUNK_HOST_CAP_AUDIO_HIRES: u8 = 0x80;

/// Host-capability bit in [`punktfunk_connection_host_caps2`] (second byte): the
/// host injector puts wire touch contacts on its desktop. Without the bit, fall
/// back to a cursor model — the host drops every contact silently.
pub const PUNKTFUNK_HOST_CAP2_TOUCH: u8 = 0x02;

/// Pad-audio `kind` ([`punktfunk_connection_next_pad_audio`]): BACK channel pair —
/// DualSense voice-coil haptics, 5 ms Opus frames.
pub const PUNKTFUNK_PAD_AUDIO_KIND_HAPTICS: u8 = 0;
/// Pad-audio `kind`: FRONT channel pair — the controller's built-in speaker, 10 ms Opus frames.
pub const PUNKTFUNK_PAD_AUDIO_KIND_SPEAKER: u8 = 1;

/// [`punktfunk_connection_set_pad_audio_caps`] bit: the pad renders the HAPTICS stream
/// (DualSense voice coils).
pub const PUNKTFUNK_PAD_AUDIO_CAP_HAPTICS: u8 = 0x01;
/// [`punktfunk_connection_set_pad_audio_caps`] bit: the pad renders the SPEAKER stream.
pub const PUNKTFUNK_PAD_AUDIO_CAP_SPEAKER: u8 = 0x02;

// ABI cap bits must match the wire constants.
#[cfg(feature = "quic")]
const _: () = {
    assert!(PUNKTFUNK_VIDEO_CAP_10BIT == punktfunk_core::quic::VIDEO_CAP_10BIT);
    assert!(PUNKTFUNK_VIDEO_CAP_HDR == punktfunk_core::quic::VIDEO_CAP_HDR);
    assert!(PUNKTFUNK_VIDEO_CAP_444 == punktfunk_core::quic::VIDEO_CAP_444);
    assert!(PUNKTFUNK_CODEC_H264 == punktfunk_core::quic::CODEC_H264);
    assert!(PUNKTFUNK_CODEC_HEVC == punktfunk_core::quic::CODEC_HEVC);
    assert!(PUNKTFUNK_CODEC_AV1 == punktfunk_core::quic::CODEC_AV1);
    assert!(PUNKTFUNK_CODEC_PYROWAVE == punktfunk_core::quic::CODEC_PYROWAVE);
    assert!(PUNKTFUNK_HOST_CAP_GAMEPAD_STATE == punktfunk_core::quic::HOST_CAP_GAMEPAD_STATE);
    assert!(PUNKTFUNK_HOST_CAP_CLIPBOARD == punktfunk_core::quic::HOST_CAP_CLIPBOARD);
    assert!(PUNKTFUNK_HOST_CAP_PEN == punktfunk_core::quic::HOST_CAP_PEN);
    assert!(PUNKTFUNK_HOST_CAP_PAD_AUDIO == punktfunk_core::quic::HOST_CAP_PAD_AUDIO);
    assert!(PUNKTFUNK_HOST_CAP_AUDIO_HIRES == punktfunk_core::quic::HOST_CAP_AUDIO_HIRES);
    assert!(PUNKTFUNK_HOST_CAP2_TOUCH == punktfunk_core::quic::HOST_CAP2_TOUCH);
    assert!(PUNKTFUNK_CLIENT_CAP_PAD_AUDIO == punktfunk_core::quic::CLIENT_CAP_PAD_AUDIO);
    assert!(PUNKTFUNK_CLIENT_CAP_AUDIO_HIRES == punktfunk_core::quic::CLIENT_CAP_AUDIO_HIRES);
    assert!(
        PUNKTFUNK_CLIENT_CAP_KEEP_HOST_AUDIO == punktfunk_core::quic::CLIENT_CAP_KEEP_HOST_AUDIO
    );
    assert!(PUNKTFUNK_PAD_AUDIO_KIND_HAPTICS == punktfunk_core::quic::PAD_AUDIO_KIND_HAPTICS);
    assert!(PUNKTFUNK_PAD_AUDIO_KIND_SPEAKER == punktfunk_core::quic::PAD_AUDIO_KIND_SPEAKER);
    // Setter cap bits are arrival flags 8/9 shifted down.
    assert!(
        (PUNKTFUNK_PAD_AUDIO_CAP_HAPTICS as u32) << 8
            == punktfunk_core::input::ARRIVAL_FLAG_PAD_AUDIO_HAPTICS
    );
    assert!(
        (PUNKTFUNK_PAD_AUDIO_CAP_SPEAKER as u32) << 8
            == punktfunk_core::input::ARRIVAL_FLAG_PAD_AUDIO_SPEAKER
    );
    assert!(PUNKTFUNK_PEN_IN_RANGE == punktfunk_core::quic::PEN_IN_RANGE);
    assert!(PUNKTFUNK_PEN_TOUCHING == punktfunk_core::quic::PEN_TOUCHING);
    assert!(PUNKTFUNK_PEN_BARREL1 == punktfunk_core::quic::PEN_BARREL1);
    assert!(PUNKTFUNK_PEN_BARREL2 == punktfunk_core::quic::PEN_BARREL2);
    assert!(PUNKTFUNK_PEN_BATCH_MAX as usize == punktfunk_core::quic::PEN_BATCH_MAX);
    assert!(PUNKTFUNK_PEN_TILT_UNKNOWN == punktfunk_core::quic::PEN_TILT_UNKNOWN);
    assert!(PUNKTFUNK_PEN_ANGLE_UNKNOWN == punktfunk_core::quic::PEN_ANGLE_UNKNOWN);
    assert!(PUNKTFUNK_PEN_DISTANCE_UNKNOWN == punktfunk_core::quic::PEN_DISTANCE_UNKNOWN);
    assert!(PUNKTFUNK_SC2_GATE_MASKED == punktfunk_core::client::SC2_GATE_MASKED);
    assert!(PUNKTFUNK_SC2_GATE_SYSTEM_LOCAL == punktfunk_core::client::SC2_GATE_SYSTEM_LOCAL);
    assert!(PUNKTFUNK_SC2_GATE_CHORDS == punktfunk_core::client::SC2_GATE_CHORDS);
};

// ABI gamepad constants must match the wire enum.
const _: () = {
    use punktfunk_core::config::GamepadPref;
    use punktfunk_core::input::gamepad as g;
    assert!(PUNKTFUNK_GAMEPAD_AUTO == GamepadPref::Auto.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_XBOX360 == GamepadPref::Xbox360.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_DUALSENSE == GamepadPref::DualSense.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_XBOXONE == GamepadPref::XboxOne.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_DUALSHOCK4 == GamepadPref::DualShock4.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_STEAMCONTROLLER == GamepadPref::SteamController.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_STEAMDECK == GamepadPref::SteamDeck.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_DUALSENSEEDGE == GamepadPref::DualSenseEdge.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_SWITCHPRO == GamepadPref::SwitchPro.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_STEAMCONTROLLER2 == GamepadPref::SteamController2.to_u8() as u32);
    assert!(
        PUNKTFUNK_GAMEPAD_STEAMCONTROLLER2_PUCK == GamepadPref::SteamController2Puck.to_u8() as u32
    );
    assert!(PUNKTFUNK_GAMEPAD_XBOXELITE == GamepadPref::XboxElite.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_8BITDO_ULTIMATE2 == GamepadPref::EightBitDoUltimate2.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_8BITDO_PRO2 == GamepadPref::EightBitDoPro2.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_8BITDO_PRO3 == GamepadPref::EightBitDoPro3.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_HORIPAD_STEAM == GamepadPref::HoripadSteam.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_JOYCON_PAIR == GamepadPref::JoyConPair.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_SWITCH2_PRO == GamepadPref::Switch2Pro.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_SWITCH2_GAMECUBE == GamepadPref::Switch2GameCube.to_u8() as u32);
    // Extended button bits mirror the wire `input::gamepad` constants.
    assert!(PUNKTFUNK_GAMEPAD_BTN_PADDLE1 == g::BTN_PADDLE1);
    assert!(PUNKTFUNK_GAMEPAD_BTN_PADDLE2 == g::BTN_PADDLE2);
    assert!(PUNKTFUNK_GAMEPAD_BTN_PADDLE3 == g::BTN_PADDLE3);
    assert!(PUNKTFUNK_GAMEPAD_BTN_PADDLE4 == g::BTN_PADDLE4);
    assert!(PUNKTFUNK_GAMEPAD_BTN_MISC1 == g::BTN_MISC1);
};

// No `struct_size`: growing these corrupts old callers. Additive kinds must not
// grow them; a deliberate widen needs an [`punktfunk_core::ABI_VERSION`] bump. RichInput
// is frozen at 20. HidOutput is 19 + 2 + `HID_REPORT_MAX`.
#[cfg(feature = "quic")]
const _: () = {
    assert!(core::mem::size_of::<PunktfunkRichInput>() == 20);
    assert!(
        core::mem::size_of::<PunktfunkHidOutput>() == 19 + 2 + punktfunk_core::quic::HID_REPORT_MAX
    );
};

#[cfg(all(test, feature = "quic"))]
mod tests {
    use super::*;

    /// AudioCtl packs as kind 5, `which` = flags, `effect[0..6]`, `effect_len = 6`.
    #[test]
    fn hidout_abi_maps_audio_ctl() {
        let out = PunktfunkHidOutput::from_hid(&punktfunk_core::quic::HidOutput::AudioCtl {
            pad: 3,
            flags: 0x17,
            raw: [0x50, 0x60, 0x70, 0x05, 0, 0],
        });
        assert_eq!(out.kind, PUNKTFUNK_HIDOUT_AUDIO_CTL);
        assert_eq!(out.pad, 3);
        assert_eq!(out.which, 0x17);
        assert_eq!(out.effect_len, 6);
        assert_eq!(out.effect[..6], [0x50, 0x60, 0x70, 0x05, 0, 0]);
        assert_eq!(out.effect[6..], [0; 5]);
        assert_eq!(out.raw_len, 0);
    }

    /// MicLed maps to kind 7 with the mode in `which`.
    #[test]
    fn hidout_abi_maps_mic_led() {
        let out = PunktfunkHidOutput::from_hid(&punktfunk_core::quic::HidOutput::MicLed {
            pad: 2,
            mode: 2,
        });
        assert_eq!(out.kind, PUNKTFUNK_HIDOUT_MIC_LED);
        assert_eq!(out.pad, 2);
        assert_eq!(out.which, 2);
        assert_eq!(out.effect_len, 0);
    }

    /// HidRaw maps to kind 6 + `hid_kind`/`raw`/`raw_len`, not a skip.
    #[test]
    fn hidout_abi_maps_hid_raw() {
        // OUTPUT report (id 0x80), host-trimmed to its declared 10 bytes.
        let rumble: Vec<u8> = vec![0x80, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let out = PunktfunkHidOutput::from_hid(&punktfunk_core::quic::HidOutput::HidRaw {
            pad: 2,
            kind: punktfunk_core::quic::HID_RAW_OUTPUT,
            data: rumble.clone(),
        });
        assert_eq!(out.kind, PUNKTFUNK_HIDOUT_HID_RAW);
        assert_eq!(out.pad, 2);
        assert_eq!(out.hid_kind, punktfunk_core::quic::HID_RAW_OUTPUT);
        assert_eq!(out.raw_len, 10);
        assert_eq!(out.raw[..10], rumble[..]);
        assert_eq!(
            out.raw[10..],
            [0; punktfunk_core::quic::HID_REPORT_MAX - 10]
        );
        // The other fields stay zero — `kind` alone says which ones are meaningful.
        assert_eq!(out.effect_len, 0);

        // A FEATURE frame arrives whole (zero-padded) and must round-trip whole;
        // anything longer clamps instead of overrunning.
        let mut lizard = vec![0u8; punktfunk_core::quic::HID_REPORT_MAX + 8];
        lizard[..6].copy_from_slice(&[0x01, 0x87, 0x03, 0x09, 0x00, 0x00]);
        let out = PunktfunkHidOutput::from_hid(&punktfunk_core::quic::HidOutput::HidRaw {
            pad: 0,
            kind: punktfunk_core::quic::HID_RAW_FEATURE,
            data: lizard.clone(),
        });
        assert_eq!(out.hid_kind, punktfunk_core::quic::HID_RAW_FEATURE);
        assert_eq!(out.raw_len as usize, punktfunk_core::quic::HID_REPORT_MAX);
        assert_eq!(out.raw[..], lizard[..punktfunk_core::quic::HID_REPORT_MAX]);
    }
}
