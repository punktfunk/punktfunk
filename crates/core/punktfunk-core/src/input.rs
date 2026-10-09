//! Client → host input events, plus the GameStream decoded-frame vocabulary injectors share.
//!
//! Input rides the same datagram plane as video, tagged [`INPUT_MAGIC`] so a session
//! demultiplexes by the first byte. Every native event is a fixed [`INPUT_WIRE_LEN`]-byte
//! little-endian [`InputEvent`] (`#[repr(C)]` as `PunktfunkInputEvent`). Variant field
//! packing lives on [`InputKind`]. Capability-gated tags (`GamepadState`/`Remove`/`Arrival`,
//! `TextInput`) are ignored by hosts that never advertised the matching `HOST_CAP_*`.
//!
//! Motion units and the rest-pose accel are pinned by `pf-inject`'s `motion_contract` test
//! against [`gamepad::MOTION_GYRO_LSB_PER_DEG_S`] / [`gamepad::MOTION_NEUTRAL_ACCEL`].

/// Wire tag: input datagram vs video packet.
pub const INPUT_MAGIC: u8 = 0xC8;

/// Serialized [`InputEvent`] size (tag + fields). The C struct is larger (`_pad`).
pub const INPUT_WIRE_LEN: usize = 1 + 1 + 4 + 4 + 4 + 4;

/// Normalized scroll vocabulary ([`InputKind::Scroll`]) plus the client-side
/// quantizer and the single outbound legacy/inversion seam.
pub mod scroll;

/// `#[repr(u8)]` so the C ABI sees a byte tag.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    KeyDown = 0,
    KeyUp = 1,
    /// Relative motion: `x`/`y` carry `dx`/`dy`.
    MouseMove = 2,
    /// Absolute: `x`/`y` pixels, `flags` = `(width << 16) | height` (same as
    /// [`TouchDown`](Self::TouchDown)). Injectors drop the event when flags is 0.
    MouseMoveAbs = 3,
    MouseButtonDown = 4,
    MouseButtonUp = 5,
    /// `x` carries the signed delta in 120-per-detent units, `code` the axis (0 = vertical,
    /// 1 = horizontal), `flags` optionally [`SCROLL_FLAG_PRECISE`]. Sub-120 magnitudes are
    /// legal: the unit is a fixed-point detent, not a click count.
    MouseScroll = 6,
    /// `code` = button bit ([`gamepad`] `BTN_*`), `x` ≠ 0 = pressed, `flags` = pad index.
    GamepadButton = 7,
    /// `code` = [`gamepad`] `AXIS_*`, `x` = value, `flags` = pad. Sticks i16 with **+y =
    /// up** (unlike mouse); triggers 0..255.
    GamepadAxis = 8,
    /// `code` = touch id (reusable after [`TouchUp`](Self::TouchUp)), `x`/`y` pixels,
    /// `flags` = `(width << 16) | height` — same absolute mapping as [`MouseMoveAbs`](Self::MouseMoveAbs).
    TouchDown = 9,
    /// Same field meaning as [`TouchDown`](Self::TouchDown).
    TouchMove = 10,
    /// Only `code` (the touch id) is used.
    TouchUp = 11,
    /// Full pad in one event ([`GamepadSnapshot`]). A dropped transition corrupts
    /// accumulated host state until the next change; a snapshot heals on the next send
    /// and `seq` drops reorders. Sent only when the host advertised
    /// [`HOST_CAP_GAMEPAD_STATE`](crate::quic::HOST_CAP_GAMEPAD_STATE); older hosts keep
    /// the per-transition events.
    GamepadState = 12,
    /// Pad unplugged. `flags` = [`encode_gamepad_remove`] (`seq << 24 | pad`, shared
    /// seq space with [`GamepadSnapshot`]) so a snapshot reordered past the removal
    /// cannot re-create the pad. Sent only to a
    /// [`HOST_CAP_GAMEPAD_STATE`](crate::quic::HOST_CAP_GAMEPAD_STATE) host; older
    /// hosts ignore the tag and the pad lingers until session end.
    GamepadRemove = 13,
    /// Kind this pad presents (`code` = [`GamepadPref`](crate::config::GamepadPref)
    /// wire byte) so a session can mix types. `flags` = pad in the low byte; bits 8/9
    /// are [`ARRIVAL_FLAG_PAD_AUDIO_HAPTICS`]/[`ARRIVAL_FLAG_PAD_AUDIO_SPEAKER`] and
    /// ride only toward a [`HOST_CAP_PAD_AUDIO`](crate::quic::HOST_CAP_PAD_AUDIO) host
    /// (an older host reads the whole word as the index). Decode with
    /// [`decode_gamepad_arrival`]. Idempotent, no seq. A pad that never arrives
    /// uses the handshake default; older hosts ignore the unknown tag.
    GamepadArrival = 14,
    /// One Unicode scalar of committed text (`code` = the scalar; other fields 0).
    /// Layout-independent VK events cannot express IME commits, so a capable client
    /// sends characters verbatim. Sent only when the host advertised
    /// [`HOST_CAP_TEXT_INPUT`](crate::quic::HOST_CAP_TEXT_INPUT); older hosts ignore
    /// the tag and clients keep best-effort VK synthesis.
    TextInput = 15,
    /// Normalized scroll ([`scroll::ScrollEvent`]): `code` = axis (0 = vertical,
    /// 1 = horizontal), `x` = signed Q24.8 delta in the source's unit, `y` = 0,
    /// `flags` = source in the low byte, phase in bits 8–15. Sent only when the
    /// host advertised `HOST_CAP2_SCROLL`; the client's outbound seam converts
    /// to [`MouseScroll`](Self::MouseScroll) for older hosts.
    Scroll = 16,
}

/// Pack [`InputKind::GamepadRemove`] `flags` (`seq << 24 | pad`) — same layout as
/// [`GamepadSnapshot::to_event`], so a removal seq-gates against snapshots.
pub fn encode_gamepad_remove(pad: u8, seq: u8) -> u32 {
    ((seq as u32) << 24) | (pad as u32)
}

/// Unpack [`InputKind::GamepadRemove`] `flags` into `(pad, seq)`.
pub fn decode_gamepad_remove(flags: u32) -> (u8, u8) {
    (flags as u8, (flags >> 24) as u8)
}

/// [`InputKind::MouseScroll`] `flags` bit: the delta was MEASURED off a precise surface (a
/// trackpad, a Magic Mouse, a touchscreen pan), not counted off a notched wheel. `x` stays in
/// 120-per-detent units; the bit says that number is a distance expressed in detents, not a
/// tally of clicks.
///
/// The two need opposite injection. A detent is a STEP — the app scrolls its own ~3 lines per
/// click. A precise delta is a DISTANCE — the content must travel as far as the finger did.
/// Injecting the latter as a wheel multiplies it by that step, which is why a 10 px flick moves
/// a page three lines. Honouring hosts emit a continuous finger-source axis at
/// [`PRECISE_PX_PER_DETENT`] and no discrete steps; a host that predates the bit ignores it and
/// keeps the old behaviour, so nothing needs negotiating.
pub const SCROLL_FLAG_PRECISE: u32 = 1;

/// Pixels one detent of a [`SCROLL_FLAG_PRECISE`] delta is worth — the inverse of the
/// 0.1-units-per-point factor SDL and the Apple clients use to put measured pixels INTO
/// 120-space. Undoing it exactly is what makes the content travel as far as the finger; any
/// other value re-introduces a scale factor.
pub const PRECISE_PX_PER_DETENT: f64 = 10.0;

/// [`InputKind::GamepadArrival`] `flags` bit 8: this pad renders haptics
/// ([`PAD_AUDIO_KIND_HAPTICS`](crate::quic::PAD_AUDIO_KIND_HAPTICS)). Sent only toward
/// a [`HOST_CAP_PAD_AUDIO`](crate::quic::HOST_CAP_PAD_AUDIO) host — an older host
/// reads the whole `flags` word as the index, so a high bit would drop the declaration.
pub const ARRIVAL_FLAG_PAD_AUDIO_HAPTICS: u32 = 1 << 8;
/// [`InputKind::GamepadArrival`] `flags` bit 9: this pad renders speaker
/// ([`PAD_AUDIO_KIND_SPEAKER`](crate::quic::PAD_AUDIO_KIND_SPEAKER)). Same wire
/// discipline as [`ARRIVAL_FLAG_PAD_AUDIO_HAPTICS`].
pub const ARRIVAL_FLAG_PAD_AUDIO_SPEAKER: u32 = 1 << 9;

/// Pack [`InputKind::GamepadArrival`] `flags`: pad in the low byte, `audio_caps`
/// (bit0 = haptics, bit1 = speaker) as bits 8/9. `audio_caps = 0` is byte-identical
/// to the pre-pad-audio wire.
pub fn encode_gamepad_arrival(pad: u8, audio_caps: u8) -> u32 {
    (pad as u32) | (((audio_caps & 0x03) as u32) << 8)
}

/// Unpack [`InputKind::GamepadArrival`] `flags` into `(pad, audio_caps)`. Mask the
/// index (`flags & 0xFF`); taking the whole word turns a capability bit into a
/// phantom pad. `audio_caps` is bits 8/9. An old-format word yields `audio_caps = 0`.
pub fn decode_gamepad_arrival(flags: u32) -> (u8, u8) {
    (flags as u8, ((flags >> 8) & 0x03) as u8)
}

/// Every stored key name outside the computed `a`-`z`, `0`-`9` and `f1`-`f24` ranges.
const KEY_NAMES: &[(&str, u8)] = &[
    ("ctrl", 0x11),
    ("control", 0x11),
    ("shift", 0x10),
    ("alt", 0x12),
    ("option", 0x12),
    ("win", 0x5B),
    ("cmd", 0x5B),
    ("super", 0x5B),
    ("meta", 0x5B),
    ("escape", 0x1B),
    ("esc", 0x1B),
    ("tab", 0x09),
    ("enter", 0x0D),
    ("return", 0x0D),
    ("space", 0x20),
    ("backspace", 0x08),
    ("delete", 0x2E),
    ("del", 0x2E),
    ("insert", 0x2D),
    ("home", 0x24),
    ("end", 0x23),
    ("pageup", 0x21),
    ("pagedown", 0x22),
    ("up", 0x26),
    ("down", 0x28),
    ("left", 0x25),
    ("right", 0x27),
    ("printscreen", 0x2C),
    ("pause", 0x13),
    ("capslock", 0x14),
];

/// Windows VK for a stored key name. The wire is VKs; a ring preset stores names, so one
/// preset works on every client. `None` means this build does not know the name — the
/// shortcut does not fire. The Kotlin and Swift `keyVk` twins
/// replay `testdata/key-vk-vectors.json`, which `key_vk_vectors_are_checked_in` regenerates.
pub fn key_vk(name: &str) -> Option<u8> {
    let n = name.trim().to_ascii_lowercase();
    if let Some(&(_, vk)) = KEY_NAMES.iter().find(|(k, _)| *k == n) {
        return Some(vk);
    }
    match n.as_bytes() {
        [c @ b'a'..=b'z'] => Some(0x41 + (c - b'a')),
        [c @ b'0'..=b'9'] => Some(0x30 + (c - b'0')),
        [b'f', rest @ ..] if !rest.is_empty() => n[1..]
            .parse::<u8>()
            .ok()
            .filter(|f| (1..=24).contains(f))
            .map(|f| 0x70 + f - 1),
        _ => None,
    }
}

/// Linux evdev key code → US-positional Windows VK on the wire; the host maps it back with
/// `vk_to_evdev`, so this is that table inverted. GTK's hardware keycode is evdev + 8.
/// `None` is a key the wire contract does not cover (media keys): drop it, do not guess.
/// The Kotlin `Keymap.evdevToVk` twin replays `testdata/evdev-vk-vectors.json`, which
/// `evdev_vk_vectors_are_checked_in` regenerates.
pub fn evdev_to_vk(evdev: u16) -> Option<u8> {
    Some(match evdev {
        14 => 0x08,  // KEY_BACKSPACE -> VK_BACK
        15 => 0x09,  // KEY_TAB       -> VK_TAB
        28 => 0x0D,  // KEY_ENTER     -> VK_RETURN
        119 => 0x13, // KEY_PAUSE     -> VK_PAUSE
        58 => 0x14,  // KEY_CAPSLOCK  -> VK_CAPITAL
        1 => 0x1B,   // KEY_ESC       -> VK_ESCAPE
        57 => 0x20,  // KEY_SPACE     -> VK_SPACE
        104 => 0x21, // KEY_PAGEUP    -> VK_PRIOR
        109 => 0x22, // KEY_PAGEDOWN  -> VK_NEXT
        107 => 0x23, // KEY_END       -> VK_END
        102 => 0x24, // KEY_HOME      -> VK_HOME
        105 => 0x25, // KEY_LEFT      -> VK_LEFT
        103 => 0x26, // KEY_UP        -> VK_UP
        106 => 0x27, // KEY_RIGHT     -> VK_RIGHT
        108 => 0x28, // KEY_DOWN      -> VK_DOWN
        99 => 0x2C,  // KEY_SYSRQ     -> VK_SNAPSHOT
        110 => 0x2D, // KEY_INSERT    -> VK_INSERT
        111 => 0x2E, // KEY_DELETE    -> VK_DELETE

        // KEY_1..KEY_9 are 2..10; KEY_0 is 11.
        11 => 0x30,
        2 => 0x31,
        3 => 0x32,
        4 => 0x33,
        5 => 0x34,
        6 => 0x35,
        7 => 0x36,
        8 => 0x37,
        9 => 0x38,
        10 => 0x39,

        // Evdev letters are QWERTY-row order; arms are VK order (A = 0x41).
        30 => 0x41, // A
        48 => 0x42, // B
        46 => 0x43, // C
        32 => 0x44, // D
        18 => 0x45, // E
        33 => 0x46, // F
        34 => 0x47, // G
        35 => 0x48, // H
        23 => 0x49, // I
        36 => 0x4A, // J
        37 => 0x4B, // K
        38 => 0x4C, // L
        50 => 0x4D, // M
        49 => 0x4E, // N
        24 => 0x4F, // O
        25 => 0x50, // P
        16 => 0x51, // Q
        19 => 0x52, // R
        31 => 0x53, // S
        20 => 0x54, // T
        22 => 0x55, // U
        47 => 0x56, // V
        17 => 0x57, // W
        45 => 0x58, // X
        21 => 0x59, // Y
        44 => 0x5A, // Z

        125 => 0x5B, // KEY_LEFTMETA  -> VK_LWIN
        126 => 0x5C, // KEY_RIGHTMETA -> VK_RWIN
        127 => 0x5D, // KEY_COMPOSE   -> VK_APPS

        82 => 0x60, // KP0
        79 => 0x61,
        80 => 0x62,
        81 => 0x63,
        75 => 0x64,
        76 => 0x65,
        77 => 0x66,
        71 => 0x67,
        72 => 0x68,
        73 => 0x69, // KP9
        55 => 0x6A, // KEY_KPASTERISK -> VK_MULTIPLY
        78 => 0x6B, // KEY_KPPLUS     -> VK_ADD
        96 => 0x6C, // KEY_KPENTER    -> VK_SEPARATOR
        74 => 0x6D, // KEY_KPMINUS    -> VK_SUBTRACT
        83 => 0x6E, // KEY_KPDOT      -> VK_DECIMAL
        98 => 0x6F, // KEY_KPSLASH    -> VK_DIVIDE

        59 => 0x70, // F1
        60 => 0x71,
        61 => 0x72,
        62 => 0x73,
        63 => 0x74,
        64 => 0x75,
        65 => 0x76,
        66 => 0x77,
        67 => 0x78,
        68 => 0x79, // F10
        87 => 0x7A, // F11
        88 => 0x7B, // F12

        69 => 0x90, // KEY_NUMLOCK    -> VK_NUMLOCK
        70 => 0x91, // KEY_SCROLLLOCK -> VK_SCROLL

        // Specific L/R VKs. The host maps generic VK_SHIFT/CONTROL/MENU onto these too.
        42 => 0xA0,  // KEY_LEFTSHIFT  -> VK_LSHIFT
        54 => 0xA1,  // KEY_RIGHTSHIFT -> VK_RSHIFT
        29 => 0xA2,  // KEY_LEFTCTRL   -> VK_LCONTROL
        97 => 0xA3,  // KEY_RIGHTCTRL  -> VK_RCONTROL
        56 => 0xA4,  // KEY_LEFTALT    -> VK_LMENU
        100 => 0xA5, // KEY_RIGHTALT   -> VK_RMENU

        // OEM punctuation at US-layout positions.
        39 => 0xBA, // KEY_SEMICOLON  -> VK_OEM_1
        13 => 0xBB, // KEY_EQUAL      -> VK_OEM_PLUS
        51 => 0xBC, // KEY_COMMA      -> VK_OEM_COMMA
        12 => 0xBD, // KEY_MINUS      -> VK_OEM_MINUS
        52 => 0xBE, // KEY_DOT        -> VK_OEM_PERIOD
        53 => 0xBF, // KEY_SLASH      -> VK_OEM_2
        41 => 0xC0, // KEY_GRAVE      -> VK_OEM_3
        26 => 0xDB, // KEY_LEFTBRACE  -> VK_OEM_4
        43 => 0xDC, // KEY_BACKSLASH  -> VK_OEM_5
        27 => 0xDD, // KEY_RIGHTBRACE -> VK_OEM_6
        40 => 0xDE, // KEY_APOSTROPHE -> VK_OEM_7
        86 => 0xE2, // KEY_102ND      -> VK_OEM_102

        // IME keys — the codes `vk_to_evdev` lists under the same heading.
        122 => 0x15, // KEY_HANGEUL          -> VK_HANGUL
        123 => 0x19, // KEY_HANJA            -> VK_HANJA
        92 => 0x1C,  // KEY_HENKAN           -> VK_CONVERT
        94 => 0x1D,  // KEY_MUHENKAN         -> VK_NONCONVERT
        93 => 0xF2,  // KEY_KATAKANAHIRAGANA -> VK_DBE_HIRAGANA
        85 => 0xF3,  // KEY_ZENKAKUHANKAKU   -> VK_DBE_SBCSCHAR
        89 => 0xC1,  // KEY_RO               -> VK_ABNT_C1 (JIS ろ, ABNT2 /?)
        121 => 0xC2, // KEY_KPCOMMA          -> VK_ABNT_C2
        124 => 0xE1, // KEY_YEN              -> VK_OEM_AX

        _ => return None,
    })
}

/// Gamepad wire contract for [`InputKind::GamepadButton`]/[`InputKind::GamepadAxis`].
///
/// GameStream/XInput end to end: buttons reuse GameStream `buttonFlags` bit positions,
/// sticks −32768..32767 with **+y = up**, triggers 0..255.
pub mod gamepad {
    pub const BTN_DPAD_UP: u32 = 0x0001;
    pub const BTN_DPAD_DOWN: u32 = 0x0002;
    pub const BTN_DPAD_LEFT: u32 = 0x0004;
    pub const BTN_DPAD_RIGHT: u32 = 0x0008;
    pub const BTN_START: u32 = 0x0010;
    pub const BTN_BACK: u32 = 0x0020;
    pub const BTN_LS_CLICK: u32 = 0x0040;
    pub const BTN_RS_CLICK: u32 = 0x0080;
    pub const BTN_LB: u32 = 0x0100;
    pub const BTN_RB: u32 = 0x0200;
    pub const BTN_GUIDE: u32 = 0x0400;
    pub const BTN_A: u32 = 0x1000;
    pub const BTN_B: u32 = 0x2000;
    pub const BTN_X: u32 = 0x4000;
    pub const BTN_Y: u32 = 0x8000;
    // Moonlight `buttonFlags2 << 16` (see `gamestream/gamepad.rs`) so both planes share
    // one host injector map. Steam Deck L4/L5/R4/R5 reuse the four Elite paddle slots.
    /// Back grip R4 — SDL `RightPaddle1` / GameStream `PADDLE1`.
    pub const BTN_PADDLE1: u32 = 0x0001_0000;
    /// Back grip L4 — SDL `LeftPaddle1` / GameStream `PADDLE2`.
    pub const BTN_PADDLE2: u32 = 0x0002_0000;
    /// Back grip R5 — SDL `RightPaddle2` / GameStream `PADDLE3`.
    pub const BTN_PADDLE3: u32 = 0x0004_0000;
    /// Back grip L5 — SDL `LeftPaddle2` / GameStream `PADDLE4`.
    pub const BTN_PADDLE4: u32 = 0x0008_0000;
    /// DualSense touchpad click. Moonlight `buttonFlags2 << 16` so GameStream clients
    /// land on the same bit. Only the DualSense backend has this button.
    pub const BTN_TOUCHPAD: u32 = 0x10_0000;
    /// Misc / capture — Deck `…`/quick-access, Share/Capture / GameStream `MISC`.
    pub const BTN_MISC1: u32 = 0x0020_0000;

    pub const AXIS_LS_X: u32 = 0;
    pub const AXIS_LS_Y: u32 = 1;
    pub const AXIS_RS_X: u32 = 2;
    pub const AXIS_RS_Y: u32 = 3;
    /// Triggers: value range 0..255.
    pub const AXIS_LT: u32 = 4;
    pub const AXIS_RT: u32 = 5;

    /// Gyro scale: DualSense raw `i16` LSBs per °/s, carried by `RichInput::Motion`.
    /// Saturates at `i16::MAX / 20` ≈ ±1638 °/s (a real DualSense is ±2000). Every
    /// capture path scales *into* these units and every host backend *from* them;
    /// `pf-inject`'s `motion_contract` pins the calibration blobs against this number.
    /// Lifting the clip is a wire-v2 change, not a quiet re-tune.
    pub const MOTION_GYRO_LSB_PER_DEG_S: i32 = 20;
    /// Accel scale: DualSense raw `i16` LSBs per g. Saturates at ±3.28 g (device ±4 g).
    /// Same pin as [`MOTION_GYRO_LSB_PER_DEG_S`].
    pub const MOTION_ACCEL_LSB_PER_G: i32 = 10_000;

    /// Rest pose: 1 g along up (index 1), zeros on the other two. `[0, 0, 0]` is
    /// free-fall, not "no sample". Backends that use different units rescale this
    /// like any other sample (`steam_remap::motion_wire_to_deck`).
    pub const MOTION_NEUTRAL_ACCEL: [i16; 3] = [0, MOTION_ACCEL_LSB_PER_G as i16, 0];
}

impl InputKind {
    pub fn from_u8(v: u8) -> Option<InputKind> {
        use InputKind::*;
        Some(match v {
            0 => KeyDown,
            1 => KeyUp,
            2 => MouseMove,
            3 => MouseMoveAbs,
            4 => MouseButtonDown,
            5 => MouseButtonUp,
            6 => MouseScroll,
            7 => GamepadButton,
            8 => GamepadAxis,
            9 => TouchDown,
            10 => TouchMove,
            11 => TouchUp,
            12 => GamepadState,
            13 => GamepadRemove,
            14 => GamepadArrival,
            15 => TextInput,
            16 => Scroll,
            _ => return None,
        })
    }
}

/// Wire pad index 0..15. Shared by the client's snapshot fold and the host's per-pad
/// accumulators.
pub const MAX_PADS: usize = 16;

/// What controller mouse does with a pad. `Touchpad` keeps the pad in the game and moves the
/// pointer with its touchpads; `Full` makes the whole pad a mouse.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PadMouseMode {
    #[default]
    Off = 0,
    Touchpad = 1,
    Full = 2,
}

impl PadMouseMode {
    pub fn from_u8(v: u8) -> Option<PadMouseMode> {
        match v {
            0 => Some(PadMouseMode::Off),
            1 => Some(PadMouseMode::Touchpad),
            2 => Some(PadMouseMode::Full),
            _ => None,
        }
    }

    /// The dial's next step: off, touchpad, full, off.
    pub fn next(self) -> PadMouseMode {
        match self {
            PadMouseMode::Off => PadMouseMode::Touchpad,
            PadMouseMode::Touchpad => PadMouseMode::Full,
            PadMouseMode::Full => PadMouseMode::Off,
        }
    }
}

/// One pad's complete state packed into a single [`InputKind::GamepadState`] event
/// (the 18-byte layout, nothing appended):
///
/// - `code`  = `buttons` ([`gamepad`] `BTN_*` bitmask, extended bits included)
/// - `x`     = `ls_x << 16 | ls_y` (two i16 halves, **+y = up**)
/// - `y`     = `rs_x << 16 | rs_y`
/// - `flags` = `seq << 24 | left_trigger << 16 | right_trigger << 8 | pad`
///
/// `seq` is a per-pad wrapping u8. The host applies a snapshot only when `seq` is
/// newer (wrapping i8 compare). The wrap window (128 sends) dwarfs any real reorder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GamepadSnapshot {
    /// Pad index 0..[`MAX_PADS`].
    pub pad: u8,
    /// Wrapping send counter; host applies only if [`Self::seq_newer`].
    pub seq: u8,
    pub buttons: u32,
    /// Triggers 0..255 (the [`gamepad::AXIS_LT`]/[`gamepad::AXIS_RT`] convention).
    pub left_trigger: u8,
    pub right_trigger: u8,
    /// Sticks −32768..32767, **+y = up**.
    pub ls_x: i16,
    pub ls_y: i16,
    pub rs_x: i16,
    pub rs_y: i16,
}

impl GamepadSnapshot {
    pub fn to_event(&self) -> InputEvent {
        InputEvent {
            kind: InputKind::GamepadState,
            _pad: [0; 3],
            code: self.buttons,
            x: ((self.ls_x as u16 as i32) << 16) | (self.ls_y as u16 as i32),
            y: ((self.rs_x as u16 as i32) << 16) | (self.rs_y as u16 as i32),
            flags: ((self.seq as u32) << 24)
                | ((self.left_trigger as u32) << 16)
                | ((self.right_trigger as u32) << 8)
                | (self.pad as u32),
        }
    }

    pub fn from_event(ev: &InputEvent) -> Option<GamepadSnapshot> {
        if ev.kind != InputKind::GamepadState {
            return None;
        }
        Some(GamepadSnapshot {
            pad: ev.flags as u8,
            seq: (ev.flags >> 24) as u8,
            buttons: ev.code,
            left_trigger: (ev.flags >> 16) as u8,
            right_trigger: (ev.flags >> 8) as u8,
            ls_x: (ev.x >> 16) as i16,
            ls_y: ev.x as i16,
            rs_x: (ev.y >> 16) as i16,
            rs_y: ev.y as i16,
        })
    }

    /// Fold one [`GamepadButton`](InputKind::GamepadButton) /
    /// [`GamepadAxis`](InputKind::GamepadAxis) into this snapshot (`seq`/`pad` untouched).
    /// `false` = not foldable / unknown axis (snapshot unchanged).
    pub fn fold(&mut self, ev: &InputEvent) -> bool {
        match ev.kind {
            InputKind::GamepadButton => {
                if ev.x != 0 {
                    self.buttons |= ev.code;
                } else {
                    self.buttons &= !ev.code;
                }
                true
            }
            InputKind::GamepadAxis => {
                let stick = ev.x.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                let trigger = ev.x.clamp(0, 255) as u8;
                match ev.code {
                    gamepad::AXIS_LS_X => self.ls_x = stick,
                    gamepad::AXIS_LS_Y => self.ls_y = stick,
                    gamepad::AXIS_RS_X => self.rs_x = stick,
                    gamepad::AXIS_RS_Y => self.rs_y = stick,
                    gamepad::AXIS_LT => self.left_trigger = trigger,
                    gamepad::AXIS_RT => self.right_trigger = trigger,
                    _ => return false,
                }
                true
            }
            _ => false,
        }
    }

    /// The [`GamepadFrame`] the injectors apply for wire pad `index`. The index is a
    /// parameter so a host accumulator can keep `pad`/`seq` zero and compare states.
    pub fn to_frame(&self, index: u8, active_mask: u16) -> GamepadFrame {
        GamepadFrame {
            index: i16::from(index),
            active_mask,
            buttons: self.buttons,
            left_trigger: self.left_trigger,
            right_trigger: self.right_trigger,
            ls_x: self.ls_x,
            ls_y: self.ls_y,
            rs_x: self.rs_x,
            rs_y: self.rs_y,
        }
    }

    /// True when `seq` supersedes `last` (wrapping u8, forward window of 127).
    /// `None` (nothing applied yet) always accepts.
    pub fn seq_newer(seq: u8, last: Option<u8>) -> bool {
        match last {
            None => true,
            Some(l) => (seq.wrapping_sub(l) as i8) > 0,
        }
    }
}

/// `#[repr(C)]` as `PunktfunkInputEvent`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputEvent {
    pub kind: InputKind,
    pub _pad: [u8; 3],
    /// keycode / button id / axis id, depending on `kind`.
    pub code: u32,
    /// x / dx / abs-x / axis-value / scroll-delta, depending on `kind`.
    pub x: i32,
    /// y / dy / abs-y, depending on `kind`.
    pub y: i32,
    /// modifier bitmask or gamepad index.
    pub flags: u32,
}

impl InputEvent {
    /// Serialize: [`INPUT_MAGIC`] + little-endian fields.
    pub fn encode(&self) -> [u8; INPUT_WIRE_LEN] {
        let mut b = [0u8; INPUT_WIRE_LEN];
        b[0] = INPUT_MAGIC;
        b[1] = self.kind as u8;
        b[2..6].copy_from_slice(&self.code.to_le_bytes());
        b[6..10].copy_from_slice(&self.x.to_le_bytes());
        b[10..14].copy_from_slice(&self.y.to_le_bytes());
        b[14..18].copy_from_slice(&self.flags.to_le_bytes());
        b
    }

    pub fn decode(buf: &[u8]) -> Option<InputEvent> {
        if buf.len() < INPUT_WIRE_LEN || buf[0] != INPUT_MAGIC {
            return None;
        }
        let kind = InputKind::from_u8(buf[1])?;
        let ev = InputEvent {
            kind,
            _pad: [0; 3],
            code: u32::from_le_bytes(buf[2..6].try_into().unwrap()),
            x: i32::from_le_bytes(buf[6..10].try_into().unwrap()),
            y: i32::from_le_bytes(buf[10..14].try_into().unwrap()),
            flags: u32::from_le_bytes(buf[14..18].try_into().unwrap()),
        };
        // A normalized scroll event is only well-formed when its body is.
        if kind == InputKind::Scroll && scroll::ScrollEvent::from_event(&ev).is_none() {
            return None;
        }
        Some(ev)
    }
}

/// One decoded GameStream (Moonlight-plane) controller event. The host decode path
/// produces these; `pf-inject` consumes them — so the type lives here, below both
/// planes. `buttons` uses the same [`gamepad`] `BTN_*` layout as [`GamepadSnapshot`]
/// (GameStream `buttonFlags | buttonFlags2 << 16` is bit-identical).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GamepadEvent {
    /// Full state of one controller plus the attached-controller mask.
    State(GamepadFrame),
    /// Sunshine arrival metadata; precedes the first [`Self::State`] for that pad.
    Arrival {
        index: u8,
        /// 0 unknown, 1 xbox, 2 ps, 3 nintendo.
        kind: u8,
        /// LI_CCAP_* bits (0x02 = rumble).
        capabilities: u16,
        /// Pad-audio render caps from a native-plane arrival's `flags` bits 8/9
        /// (see [`decode_gamepad_arrival`]). Not a GameStream LI_CCAP bit — that
        /// lives in `capabilities`. GameStream always sets 0.
        audio_caps: u8,
    },
}

/// One controller's inputs on the GameStream/Moonlight plane (sticks −32768..32767
/// with +Y up, triggers 0..255, buttons = `buttonFlags | buttonFlags2 << 16`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GamepadFrame {
    pub index: i16,
    /// Bit n set = controller n attached; a clear bit for an allocated pad means unplug.
    pub active_mask: u16,
    pub buttons: u32,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub ls_x: i16,
    pub ls_y: i16,
    pub rs_x: i16,
    pub rs_y: i16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_wire_roundtrip() {
        let e = InputEvent {
            kind: InputKind::MouseMove,
            _pad: [0; 3],
            code: 0,
            x: -12,
            y: 34,
            flags: 0xABCD,
        };
        assert_eq!(InputEvent::decode(&e.encode()), Some(e));
        assert!(InputEvent::decode(&[0u8; INPUT_WIRE_LEN]).is_none());
    }

    #[test]
    fn touch_kinds_roundtrip() {
        for kind in [
            InputKind::TouchDown,
            InputKind::TouchMove,
            InputKind::TouchUp,
        ] {
            assert_eq!(InputKind::from_u8(kind as u8), Some(kind));
            let e = InputEvent {
                kind,
                _pad: [0; 3],
                code: 2,
                x: 640,
                y: 360,
                flags: (1280u32 << 16) | 720,
            };
            assert_eq!(InputEvent::decode(&e.encode()), Some(e));
        }
        // 17 is one past the last valid kind.
        assert_eq!(InputKind::from_u8(13), Some(InputKind::GamepadRemove));
        assert_eq!(InputKind::from_u8(14), Some(InputKind::GamepadArrival));
        assert_eq!(InputKind::from_u8(15), Some(InputKind::TextInput));
        assert_eq!(InputKind::from_u8(16), Some(InputKind::Scroll));
        assert_eq!(InputKind::from_u8(17), None);
    }

    #[test]
    fn text_input_roundtrip() {
        for cp in ['a' as u32, 'ß' as u32, '語' as u32, 0x1F600] {
            let e = InputEvent {
                kind: InputKind::TextInput,
                _pad: [0; 3],
                code: cp,
                x: 0,
                y: 0,
                flags: 0,
            };
            assert_eq!(InputEvent::decode(&e.encode()), Some(e));
        }
    }

    #[test]
    fn gamepad_remove_flags_roundtrip() {
        for (pad, seq) in [(0u8, 0u8), (3, 200), (15, 255), (7, 1)] {
            let flags = encode_gamepad_remove(pad, seq);
            assert_eq!(decode_gamepad_remove(flags), (pad, seq));
        }
        // Snapshot pack uses the same low-byte pad / high-byte seq as a removal.
        let snap = GamepadSnapshot {
            pad: 9,
            seq: 123,
            ..Default::default()
        };
        let (pad, seq) = decode_gamepad_remove(snap.to_event().flags);
        assert_eq!((pad, seq), (9, 123));
    }

    #[test]
    fn gamepad_arrival_flags_roundtrip() {
        for (pad, caps) in [(0u8, 0u8), (3, 0b01), (15, 0b10), (7, 0b11)] {
            let flags = encode_gamepad_arrival(pad, caps);
            assert_eq!(decode_gamepad_arrival(flags), (pad, caps));
            assert_eq!(flags & 0xFF, pad as u32);
        }
        assert_eq!(
            encode_gamepad_arrival(2, 0b11),
            2 | ARRIVAL_FLAG_PAD_AUDIO_HAPTICS | ARRIVAL_FLAG_PAD_AUDIO_SPEAKER
        );
        // `audio_caps = 0` is byte-identical to the plain index.
        assert_eq!(encode_gamepad_arrival(5, 0), 5);
        assert_eq!(decode_gamepad_arrival(5), (5, 0));
        // Undefined high bits (a future extension) do not leak into index or caps.
        assert_eq!(
            decode_gamepad_arrival(0xFFFF_0000 | (0b01 << 8) | 9),
            (9, 1)
        );
        // encode masks unknown caps bits so they cannot land in the index space.
        assert_eq!(encode_gamepad_arrival(1, 0xFF), 1 | (0b11 << 8));
    }

    #[test]
    fn gamepad_snapshot_roundtrip() {
        let s = GamepadSnapshot {
            pad: 3,
            seq: 200,
            buttons: gamepad::BTN_A | gamepad::BTN_PADDLE4 | gamepad::BTN_MISC1,
            left_trigger: 255,
            right_trigger: 1,
            ls_x: -32768,
            ls_y: 32767,
            rs_x: -1,
            rs_y: 12345,
        };
        let ev = s.to_event();
        assert_eq!(ev.kind, InputKind::GamepadState);
        let dec = InputEvent::decode(&ev.encode()).unwrap();
        assert_eq!(GamepadSnapshot::from_event(&dec), Some(s));
        let axis = InputEvent {
            kind: InputKind::GamepadAxis,
            _pad: [0; 3],
            code: gamepad::AXIS_LT,
            x: 255,
            y: 0,
            flags: 0,
        };
        assert_eq!(GamepadSnapshot::from_event(&axis), None);
    }

    #[test]
    fn gamepad_snapshot_fold() {
        let mut s = GamepadSnapshot::default();
        let ev = |kind: InputKind, code: u32, x: i32| InputEvent {
            kind,
            _pad: [0; 3],
            code,
            x,
            y: 0,
            flags: 0,
        };
        assert!(s.fold(&ev(InputKind::GamepadButton, gamepad::BTN_A, 1)));
        assert!(s.fold(&ev(InputKind::GamepadButton, gamepad::BTN_RB, 1)));
        assert_eq!(s.buttons, gamepad::BTN_A | gamepad::BTN_RB);
        assert!(s.fold(&ev(InputKind::GamepadButton, gamepad::BTN_A, 0)));
        assert_eq!(s.buttons, gamepad::BTN_RB);
        assert!(s.fold(&ev(InputKind::GamepadAxis, gamepad::AXIS_LT, 300)));
        assert_eq!(s.left_trigger, 255);
        assert!(s.fold(&ev(InputKind::GamepadAxis, gamepad::AXIS_LS_Y, -40000)));
        assert_eq!(s.ls_y, i16::MIN);
        assert!(!s.fold(&ev(InputKind::GamepadAxis, 99, 1)));
        assert!(!s.fold(&ev(InputKind::KeyDown, 30, 1)));

        let f = s.to_frame(2, 0b0100);
        assert_eq!((f.index, f.active_mask), (2, 0b0100));
        assert_eq!(
            (f.buttons, f.left_trigger, f.ls_y),
            (gamepad::BTN_RB, 255, i16::MIN)
        );
    }

    #[test]
    fn gamepad_snapshot_seq_gate() {
        assert!(GamepadSnapshot::seq_newer(0, None));
        assert!(GamepadSnapshot::seq_newer(6, Some(5)));
        assert!(!GamepadSnapshot::seq_newer(5, Some(5)));
        assert!(!GamepadSnapshot::seq_newer(4, Some(5)));
        assert!(GamepadSnapshot::seq_newer(2, Some(250)));
        assert!(!GamepadSnapshot::seq_newer(250, Some(2)));
        // Distance 128 is stale: wrapping i8 of 128 is -128, and `> 0` excludes it.
        assert!(!GamepadSnapshot::seq_newer(133, Some(5)));
    }

    #[test]
    fn key_names_map_to_windows_vks() {
        assert_eq!(key_vk("ctrl"), Some(0x11));
        assert_eq!(key_vk(" Meta "), Some(0x5B));
        assert_eq!(key_vk("F4"), Some(0x73));
        assert_eq!(key_vk("z"), Some(0x5A));
        assert_eq!(key_vk("f25"), None);
        assert_eq!(key_vk(""), None);
    }

    /// `key_vk` over every table name (plain, upper-cased, whitespace-padded), every printable
    /// ASCII character, the `f` edge cases and a few unknown names, one case per line.
    fn key_vk_vectors() -> String {
        let mut names: Vec<String> = Vec::new();
        for (k, _) in KEY_NAMES {
            names.extend([k.to_string(), k.to_ascii_uppercase(), format!(" \t{k}\r\n")]);
        }
        names.extend((0x21u8..=0x7E).map(|b| char::from(b).to_string()));
        names.extend((0..=25).map(|f| format!("f{f}")));
        let odd = [
            "F12", " f4\n", "f01", "f001", "f+1", "f-1", "f256", "ff", "f1a", "f 1",
        ];
        let unknown = [
            "", " ", "\n", "hyper", "ctrl+c", "numpad0", "lctrl", "escape2",
        ];
        names.extend(odd.into_iter().chain(unknown).map(String::from));

        let about =
            "Generated from punktfunk_core::input::key_vk by key_vk_vectors_are_checked_in \
            (UPDATE_VECTORS=1 rewrites it). The Kotlin and Swift keyVk tests replay every case.";
        let mut out = format!("{{\n  \"$comment\": \"{about}\",\n  \"cases\": [\n");
        for (i, name) in names.iter().enumerate() {
            let vk = key_vk(name).map_or("null".to_string(), |v| v.to_string());
            let comma = if i + 1 < names.len() { "," } else { "" };
            let name = serde_json::to_string(name).unwrap();
            out += &format!("    {{\"name\": {name}, \"vk\": {vk}}}{comma}\n");
        }
        out + "  ]\n}\n"
    }

    #[test]
    fn key_vk_vectors_are_checked_in() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/key-vk-vectors.json");
        let fresh = key_vk_vectors();
        if std::env::var_os("UPDATE_VECTORS").is_some() {
            std::fs::write(path, &fresh).unwrap();
        }
        let on_disk = std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            on_disk == fresh,
            "{path} is stale: rerun with UPDATE_VECTORS=1"
        );
    }

    /// Inverse of host `inject::vk_to_evdev`. Generic-modifier VKs collapse onto the
    /// left-hand evdev codes and are omitted here.
    #[test]
    fn roundtrips_through_the_host_table() {
        let host_pairs: &[(u8, u16)] = &[
            (0x08, 14),
            (0x09, 15),
            (0x0D, 28),
            (0x13, 119),
            (0x14, 58),
            (0x1B, 1),
            (0x20, 57),
            (0x21, 104),
            (0x22, 109),
            (0x23, 107),
            (0x24, 102),
            (0x25, 105),
            (0x26, 103),
            (0x27, 106),
            (0x28, 108),
            (0x2C, 99),
            (0x2D, 110),
            (0x2E, 111),
            (0x30, 11),
            (0x31, 2),
            (0x39, 10),
            (0x41, 30),
            (0x5A, 44),
            (0x5B, 125),
            (0x60, 82),
            (0x69, 73),
            (0x70, 59),
            (0x7B, 88),
            (0x90, 69),
            (0xA0, 42),
            (0xA5, 100),
            (0xBA, 39),
            (0xE2, 86),
            (0x15, 122),
            (0x19, 123),
            (0x1C, 92),
            (0x1D, 94),
            (0xF2, 93),
            (0xF3, 85),
            (0xC1, 89),
            (0xC2, 121),
            (0xE1, 124),
        ];
        for &(vk, evdev) in host_pairs {
            assert_eq!(evdev_to_vk(evdev), Some(vk), "evdev {evdev}");
        }
        assert_eq!(evdev_to_vk(113), None); // KEY_MUTE — not in the wire contract
    }

    /// `evdev_to_vk` over every evdev code below 256, one case per line.
    fn evdev_vk_vectors() -> String {
        let about = "Generated from punktfunk_core::input::evdev_to_vk by \
            evdev_vk_vectors_are_checked_in (UPDATE_VECTORS=1 rewrites it). The Kotlin \
            Keymap.evdevToVk test replays every case.";
        let mut out = format!("{{\n  \"$comment\": \"{about}\",\n  \"cases\": [\n");
        for evdev in 0..=255u16 {
            let vk = evdev_to_vk(evdev).map_or("null".to_string(), |v| v.to_string());
            let comma = if evdev < 255 { "," } else { "" };
            out += &format!("    {{\"evdev\": {evdev}, \"vk\": {vk}}}{comma}\n");
        }
        out + "  ]\n}\n"
    }

    #[test]
    fn evdev_vk_vectors_are_checked_in() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/evdev-vk-vectors.json"
        );
        let fresh = evdev_vk_vectors();
        if std::env::var_os("UPDATE_VECTORS").is_some() {
            std::fs::write(path, &fresh).unwrap();
        }
        let on_disk = std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            on_disk == fresh,
            "{path} is stale: rerun with UPDATE_VECTORS=1"
        );
    }

    /// `testdata/gamepad-button-vectors.json` names every wire button with its bit; the web
    /// Controllers page and pf-inject read the same rows.
    #[test]
    fn gamepad_button_vectors_match_the_wire() {
        use gamepad::*;
        let wire = [
            ("DPAD_UP", BTN_DPAD_UP),
            ("DPAD_DOWN", BTN_DPAD_DOWN),
            ("DPAD_LEFT", BTN_DPAD_LEFT),
            ("DPAD_RIGHT", BTN_DPAD_RIGHT),
            ("START", BTN_START),
            ("BACK", BTN_BACK),
            ("LS_CLICK", BTN_LS_CLICK),
            ("RS_CLICK", BTN_RS_CLICK),
            ("LB", BTN_LB),
            ("RB", BTN_RB),
            ("GUIDE", BTN_GUIDE),
            ("A", BTN_A),
            ("B", BTN_B),
            ("X", BTN_X),
            ("Y", BTN_Y),
            ("PADDLE1", BTN_PADDLE1),
            ("PADDLE2", BTN_PADDLE2),
            ("PADDLE3", BTN_PADDLE3),
            ("PADDLE4", BTN_PADDLE4),
            ("TOUCHPAD", BTN_TOUCHPAD),
            ("MISC1", BTN_MISC1),
        ];
        let raw = include_str!("../testdata/gamepad-button-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("vector file parses");
        let rows = file["buttons"].as_array().expect("buttons array");
        let mut got: Vec<(&str, u32)> = rows
            .iter()
            .map(|r| {
                (
                    r["name"].as_str().unwrap(),
                    r["bit"].as_u64().unwrap() as u32,
                )
            })
            .collect();
        got.sort_unstable();
        let mut want = wire.to_vec();
        want.sort_unstable();
        assert_eq!(got, want);
    }
}
