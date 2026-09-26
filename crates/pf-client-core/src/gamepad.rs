//! App-lifetime SDL3 gamepad service: Settings pad list, a forwarded slot per connected
//! pad (a user pin narrows it to one), and in-session buttons/axes, DualSense touchpad +
//! motion (`0xCC`), rumble, lightbar, DualSense raw effects, and Steam Controller 2 raw
//! passthrough ([`crate::sc2_capture`]). Held state is zeroed on slot close or detach.
//!
//! Idle never opens a device and keeps Valve HIDAPI off ([`set_valve_hidapi`]): the
//! Deck driver kills lizard mode (trackpad-mouse) at *enumeration*. Settings uses
//! ID-based metadata getters. Menu mode ([`GamepadService::set_menu_mode`]) is the
//! exception: the same pads stay open for [`MenuEvent`]s, folded into one sample so any
//! of them navigates; Valve HIDAPI stays off; an attached session supersedes. This
//! thread is the single rumble/HID-output consumer. Menu types live in `menu_nav`.

use crate::menu_nav::{ring_sector, MenuNav, MenuSample};
pub use crate::menu_nav::{MenuDir, MenuEvent, MenuPulse, PadBattery, PadInfo};
use punktfunk_core::client::{ActuatorQuirks, NativeClient};
use punktfunk_core::config::GamepadPref;
use punktfunk_core::input::{gamepad as wire, InputEvent, InputKind};
use punktfunk_core::quic::{HidOutput, RichInput};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// SDL gyro is rad/s and accel is m/s²; the DualSense report wants the wire LSBs
/// ([`wire::MOTION_GYRO_LSB_PER_DEG_S`] / [`wire::MOTION_ACCEL_LSB_PER_G`]).
const GYRO_LSB_PER_RAD_S: f32 =
    wire::MOTION_GYRO_LSB_PER_DEG_S as f32 * 180.0 / std::f32::consts::PI;
const ACCEL_LSB_PER_G: f32 = wire::MOTION_ACCEL_LSB_PER_G as f32;
const G: f32 = 9.80665;

/// L1+R1+Start+Select: leave fullscreen and release capture. Raises the UI escape;
/// the presenter masks forwarding until capture returns. A hold of
/// [`DISCONNECT_HOLD`] disconnects. Not Guide/QAM — those pass through to the host.
const ESCAPE_CHORD: [u32; 4] = [wire::BTN_LB, wire::BTN_RB, wire::BTN_START, wire::BTN_BACK];

/// 1500 ms is long enough to be deliberate over a leave-fullscreen press.
const DISCONNECT_HOLD: Duration = Duration::from_millis(1500);

/// Hold Select alone this long for a synthetic host Guide ([`SelectGesture`]). The
/// physical Guide never reaches the host cleanly where the local shell owns it.
const GUIDE_HOLD: Duration = Duration::from_millis(350);

/// Delay between a held-back Select tap's press and its scheduled release. Core folds
/// per-transition sends into one `GamepadState`; down+up in one window vanish.
const TAP_PRESS: Duration = Duration::from_millis(50);

/// Deck actuator keepalive, declared as [`ActuatorQuirks`] at slot open. The built-in
/// actuator decays inside SDL's ~2 s rumble resend, and an identical `set_rumble` is a
/// no-op, so a steady level pulses unless re-kicked sub-decay; 40 ms matches SDL's
/// Steam-Controller driver. The engine owns timing and 1-LSB jitter.
const DECK_RUMBLE_KEEPALIVE_MS: u16 = 40;

/// Open-pad battery re-read. 15 s is coarser than a percent move, finer than the
/// worker loop; the read is a cached HID report.
const BATTERY_POLL: Duration = Duration::from_secs(15);

/// Open-pad power report. `None` is wired-no-battery, error, unknown, and SDL's `-1`
/// percent — draw no battery, never 0 % (that looks like empty).
fn battery_of(pad: &sdl3::gamepad::Gamepad) -> Option<PadBattery> {
    use sdl3::joystick::PowerLevel;
    let info = pad.power_info();
    let charging = match info.state {
        PowerLevel::OnBattery => false,
        PowerLevel::Charging | PowerLevel::Charged => true,
        PowerLevel::NoBattery | PowerLevel::Error | PowerLevel::Unknown => return None,
    };
    if info.percentage < 0 {
        return None;
    }
    Some(PadBattery {
        percent: info.percentage.min(100) as u8,
        charging,
    })
}

/// Valve HIDAPI on/off. The Deck driver sends `ID_CLEAR_DIGITAL_MAPPINGS` +
/// `TRACKPAD_NONE` at *enumeration* and feeds the lizard-mode watchdog, so the
/// trackpad-mouse dies while the driver merely runs. Enable only in-session (paddles,
/// trackpads, gyro). SDL3 applies live; disable restores lizard mode in seconds.
fn set_valve_hidapi(enabled: bool) {
    let v = if enabled { "1" } else { "0" };
    sdl3::hint::set("SDL_JOYSTICK_HIDAPI_STEAMDECK", v);
    sdl3::hint::set("SDL_JOYSTICK_HIDAPI_STEAM", v);
}

/// Disable Valve HIDAPI **before** `SDL_Init`. Enumeration is part of joystick init:
/// setting the hint afterwards detaches the driver only after it has already cleared
/// lizard mode. [`run`] orders this correctly; the pumped path receives a subsystem
/// after enumeration, so callers must invoke this with the other pre-init hints.
pub fn preinit_disable_valve_hidapi() {
    set_valve_hidapi(false);
}

fn pref_for_type(t: sdl3::gamepad::GamepadType) -> GamepadPref {
    use sdl3::gamepad::GamepadType as T;
    match t {
        T::PS5 => GamepadPref::DualSense,
        T::PS4 => GamepadPref::DualShock4,
        T::XboxOne => GamepadPref::XboxOne,
        // A Joy-Con pair exposes the full Pro surface; a single Joy-Con is half a pad
        // and stays on the Xbox 360 fallback.
        T::NintendoSwitchPro | T::NintendoSwitchJoyconPair => GamepadPref::SwitchPro,
        _ => GamepadPref::Xbox360,
    }
}

/// Kind declared in [`InputKind::GamepadArrival`]: an explicit setting emulates that
/// pad on every slot; `Auto` keeps per-pad detection. Applied per pad, not only in
/// Hello — the host builds each virtual device from arrival. Local feedback still
/// uses the physical kind (the controller in hand, not the host's pretence).
fn declared_kind(setting: GamepadPref, physical: GamepadPref) -> GamepadPref {
    match setting {
        GamepadPref::Auto => physical,
        explicit => explicit,
    }
}

/// Steam Deck probe. `SteamDeck=1` short-circuits; else DMI (Valve + Jupiter/Galileo,
/// readable in the flatpak). Cached — the answer cannot change while we run.
pub fn is_steam_deck() -> bool {
    static DECK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DECK.get_or_init(|| {
        // Valve documents the VALUE: desktop Steam exports `SteamDeck=0`, so a presence
        // check would call every Steam PC a Deck.
        if std::env::var("SteamDeck").is_ok_and(|v| v.trim() == "1") {
            return true;
        }
        let dmi = |f: &str| std::fs::read_to_string(format!("/sys/class/dmi/id/{f}"));
        dmi("board_vendor").is_ok_and(|v| v.trim() == "Valve")
            && dmi("product_name").is_ok_and(|p| matches!(p.trim(), "Jupiter" | "Galileo"))
    })
}

enum Ctl {
    Attach(Arc<NativeClient>),
    Detach,
    Pin(Option<String>),
    KindOverride(GamepadPref),
    Forwarding(bool),
    SystemButtons {
        forward_raw: bool,
        gesture: bool,
    },
    TapButton(u32),
    /// Pad-audio streams to render: bit0 = haptics, bit1 = speaker. Settings half of
    /// the tier-A capability declared at slot open.
    PadAudioPrefs(u8),
    MenuMode(bool),
    MenuRumble(MenuPulse),
    Mask(bool),
    /// In-stream ring is up: first forwarded pad → [`MenuEvent`]s. Pair with
    /// [`Ctl::Mask`] so the same presses never reach the host.
    RingNav(bool),
    /// Whether anything is listening for a Select chord ([`GamepadService::set_chords_live`]).
    ChordsLive(bool),
}

/// What a Select+button chord on a forwarded pad asks the client for.
///
/// [`Ring`](Self::Ring) withholds the press it takes, because A is the dial's own confirm. That
/// is why the client says whether anything is listening ([`GamepadService::set_chords_live`]):
/// a chord nothing receives would eat a face button the game was owed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectChord {
    /// Select+A — open the quick-action dial. The A press is withheld.
    Ring,
    /// Select+X — step the stats overlay's tier. Both presses still reach the game.
    Stats,
}

#[derive(Clone)]
pub struct GamepadService {
    pads: Arc<Mutex<Vec<PadInfo>>>,
    active: Arc<Mutex<Option<PadInfo>>>,
    ctl: Sender<Ctl>,
    escape_rx: async_channel::Receiver<()>,
    disconnect_rx: async_channel::Receiver<()>,
    menu_rx: async_channel::Receiver<MenuEvent>,
    /// A Select chord while streaming — the second button swallowed. Carries the pad's wire
    /// index and what it asked for.
    chord_rx: async_channel::Receiver<(u8, SelectChord)>,
}

impl GamepadService {
    pub fn start() -> GamepadService {
        let pads = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(Mutex::new(None));
        let (ctl, ctl_rx) = std::sync::mpsc::channel();
        let (escape_tx, escape_rx) = async_channel::unbounded();
        let (disconnect_tx, disconnect_rx) = async_channel::unbounded();
        let (menu_tx, menu_rx) = async_channel::unbounded();
        let (chord_tx, chord_rx) = async_channel::unbounded();
        let (p, a) = (pads.clone(), active.clone());
        if let Err(e) = std::thread::Builder::new()
            .name("punktfunk-gamepad".into())
            .spawn(move || {
                if let Err(e) = run(
                    p,
                    a,
                    &ctl_rx,
                    &escape_tx,
                    &disconnect_tx,
                    &menu_tx,
                    &chord_tx,
                ) {
                    tracing::warn!(error = %e, "gamepad service ended — pads disabled");
                }
            })
        {
            tracing::warn!(error = %e, "gamepad service start failed");
        }
        GamepadService {
            pads,
            active,
            ctl,
            escape_rx,
            disconnect_rx,
            menu_rx,
            chord_rx,
        }
    }

    /// Caller-pumped variant: SDL grants one thread the event queue, so the session
    /// binary (video+events on its main thread) cannot use [`start`]. Feed every event
    /// to [`GamepadPump::handle_event`] and [`GamepadPump::tick`] once per loop.
    ///
    /// Valve HIDAPI is held off here too late — `subsystem` means enumeration already
    /// ran. Call [`preinit_disable_valve_hidapi`] with the other pre-`SDL_Init` hints.
    /// This still re-asserts off after an earlier session.
    pub fn pumped(subsystem: sdl3::GamepadSubsystem) -> (GamepadService, GamepadPump) {
        set_valve_hidapi(false);
        let pads = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(Mutex::new(None));
        let (ctl, ctl_rx) = std::sync::mpsc::channel();
        let (escape_tx, escape_rx) = async_channel::unbounded();
        let (disconnect_tx, disconnect_rx) = async_channel::unbounded();
        let (menu_tx, menu_rx) = async_channel::unbounded();
        let (chord_tx, chord_rx) = async_channel::unbounded();
        let worker = Worker::new(
            subsystem,
            pads.clone(),
            active.clone(),
            escape_tx,
            disconnect_tx,
            menu_tx,
            chord_tx,
        );
        (
            GamepadService {
                pads,
                active,
                ctl,
                escape_rx,
                disconnect_rx,
                menu_rx,
                chord_rx,
            },
            GamepadPump { worker, ctl_rx },
        )
    }

    /// Clone of the shared mpmc channel; the stream page spawns a future on it.
    pub fn escape_events(&self) -> async_channel::Receiver<()> {
        self.escape_rx.clone()
    }

    /// Clone of the shared mpmc channel; fires once past [`DISCONNECT_HOLD`].
    pub fn disconnect_events(&self) -> async_channel::Receiver<()> {
        self.disconnect_rx.clone()
    }

    /// Clone of the shared mpmc channel; flowing only while menu mode is on and idle.
    pub fn menu_events(&self) -> async_channel::Receiver<MenuEvent> {
        self.menu_rx.clone()
    }

    /// Select chords on a forwarded pad — both buttons swallowed. One event per chord, carrying
    /// the pad's wire index and what it asked for. Silent until [`Self::set_chords_live`].
    pub fn chord_events(&self) -> async_channel::Receiver<(u8, SelectChord)> {
        self.chord_rx.clone()
    }

    /// Whether this client can act on a Select chord at all.
    ///
    /// Off by default, and off for good on a build with no console UI: the chord swallows the
    /// button pressed with Select, so claiming one nothing receives eats a face button the game
    /// was owed. The client turns it on once it has somewhere to send them.
    pub fn set_chords_live(&self, on: bool) {
        let _ = self.ctl.send(Ctl::ChordsLive(on));
    }

    /// Pair with [`Self::set_masked`] so the same presses never reach the host.
    pub fn set_ring_nav(&self, on: bool) {
        let _ = self.ctl.send(Ctl::RingNav(on));
    }

    /// While on and idle, hold the active pad open for [`MenuEvent`]s. An attached
    /// session supersedes translation.
    pub fn set_menu_mode(&self, on: bool) {
        let _ = self.ctl.send(Ctl::MenuMode(on));
    }

    /// No-op while a session is attached or no pad is open.
    pub fn menu_rumble(&self, pulse: MenuPulse) {
        let _ = self.ctl.send(Ctl::MenuRumble(pulse));
    }

    pub fn pads(&self) -> Vec<PadInfo> {
        self.pads.lock().unwrap().clone()
    }

    pub fn active(&self) -> Option<PadInfo> {
        self.active.lock().unwrap().clone()
    }

    /// Pin by `PadInfo::key` — `None` = automatic. Survives disconnect; re-applies when
    /// a matching controller returns.
    pub fn set_pinned(&self, key: Option<String>) {
        let _ = self.ctl.send(Ctl::Pin(key));
    }

    /// Explicit controller-type for the session about to start (`Auto` = per pad).
    /// Call before [`Self::attach`]: the host honors [`InputKind::GamepadArrival`] over
    /// the Hello default and does not hot-swap a device that already exists.
    pub fn set_kind_override(&self, pref: GamepadPref) {
        let _ = self.ctl.send(Ctl::KindOverride(pref));
    }

    /// Off holds no slot: no arrival and the hidraw node stays free for a passthrough
    /// tool (SDL HIDAPI takes it at open). The escape chord listens only on forwarded
    /// pads; menu navigation is untouched.
    pub fn set_forwarding(&self, on: bool) {
        let _ = self.ctl.send(Ctl::Forwarding(on));
    }

    /// Overlay owns the controller: hold every forwarded pad NEUTRAL. Not
    /// [`set_forwarding`](Self::set_forwarding) — that sends [`GamepadRemove`](InputKind::GamepadRemove)
    /// (the game sees an unplug). Masking keeps slots open, flushes held state so a
    /// stick stops steering, and adopts (does not replay) on the way back.
    ///
    /// SDL's own unfocused-window gate cannot fire on a Deck in Gaming Mode:
    /// gamescope keeps this client focused in its own Xwayland ctx.
    pub fn set_masked(&self, on: bool) {
        let _ = self.ctl.send(Ctl::Mask(on));
    }

    /// `forward_raw` gates physical Guide/QAM onto the wire (off = local shell; on a
    /// Gaming-Mode host, forwarding opens both overlays). `gesture` arms hold-Select
    /// ([`GUIDE_HOLD`]) so the host Guide stays reachable.
    pub fn set_system_buttons(&self, forward_raw: bool, gesture: bool) {
        let _ = self.ctl.send(Ctl::SystemButtons {
            forward_raw,
            gesture,
        });
    }

    /// Synthetic host Guide: down now, up [`TAP_PRESS`] later, on the first forwarded
    /// slot (pad 0 if none). No-op with no session.
    pub fn tap_guide(&self) {
        self.tap_button(wire::BTN_GUIDE);
    }

    /// [`Self::tap_guide`] for any system button — the quick-action ring's route, which
    /// carries the bit its slot stands for.
    pub fn tap_button(&self, bit: u32) {
        let _ = self.ctl.send(Ctl::TapButton(bit));
    }

    /// Like [`Self::tap_guide`] for `MISC1` (Deck `…`). Harmless on pads that map or
    /// drop the misc button.
    pub fn tap_qam(&self) {
        self.tap_button(wire::BTN_MISC1);
    }

    /// Tier-A capability bits declared at slot open (wired DualSense/Edge only; others
    /// 0). Call before [`Self::attach`]. Default is nothing — an embedder that never
    /// calls this keeps the wire bytes unchanged.
    pub fn set_pad_audio_prefs(&self, haptics: bool, speaker: bool) {
        let bits = (haptics as u8) | ((speaker as u8) << 1);
        let _ = self.ctl.send(Ctl::PadAudioPrefs(bits));
    }

    pub fn attach(&self, connector: Arc<NativeClient>) {
        let _ = self.ctl.send(Ctl::Attach(connector));
    }

    pub fn detach(&self) {
        let _ = self.ctl.send(Ctl::Detach);
    }

    /// Physical pad's virtual kind, or the host default if none. Read *before* attach,
    /// when Valve HIDAPI is still off ([`set_valve_hidapi`]) so the Deck's 28DE:1205 is
    /// not enumerable; Steam Input shows only a virtual X360. On a Deck, a virtual pad
    /// (or none) is the built-in controller — resolve to Steam Deck so paddles/gyro land.
    /// A real external controller still wins.
    pub fn auto_pref(&self) -> GamepadPref {
        match self.active() {
            Some(p) if !p.steam_virtual => p.pref,
            _ if is_steam_deck() => GamepadPref::SteamDeck,
            Some(p) => p.pref,
            None => GamepadPref::Auto,
        }
    }
}

/// Caller-pumped half of [`GamepadService::pumped`]: events plus a periodic tick.
pub struct GamepadPump {
    worker: Worker,
    ctl_rx: Receiver<Ctl>,
}

impl GamepadPump {
    pub fn handle_event(&mut self, event: sdl3::event::Event) {
        self.worker.handle_event(event);
    }

    /// Per-wakeup work: ctl drain, chord hold, menu repeat, rumble/HID. ≲30 ms keeps
    /// chord-hold and haptics inside the threaded worker's tolerances.
    pub fn tick(&mut self) {
        let _ = self.worker.drain_ctl(&self.ctl_rx);
        self.worker.gesture_poll();
        self.worker.maybe_fire_disconnect();
        self.worker.menu_poll();
        self.worker.render_feedback();
    }

    /// Close every forwarded slot now. [`GamepadService::detach`] only posts `Ctl::Detach`;
    /// without another [`tick`](Self::tick) the flush never runs, and slots have no `Drop`
    /// that silences them. Closes directly rather than draining ctl: this also runs from
    /// `Drop`, and `drain_ctl` would `unwrap` a poisoned lock during unwind.
    pub fn shutdown(&mut self) {
        self.worker.close_all_slots();
    }
}

/// Last-resort silence: a `?` exit skips an explicit [`shutdown`](GamepadPump::shutdown).
/// Call `shutdown` at the normal exit so the pad goes quiet before a long teardown.
impl Drop for GamepadPump {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Lowest free wire index, or `None` when every slot is taken. Lowest-free keeps indices
/// stable: a disconnect frees only its own index, so a game never sees players shuffle.
fn lowest_free_index(taken: &[u8]) -> Option<u8> {
    (0..punktfunk_core::input::MAX_PADS as u8).find(|i| !taken.contains(i))
}

/// One per-transition event tagged with wire pad index (`flags`). Core folds these into
/// seq'd [`GamepadState`](punktfunk_core::input::InputKind::GamepadState) keyed on that index.
fn send(connector: &NativeClient, kind: InputKind, code: u32, x: i32, pad: u8) {
    let _ = connector.send_input(&InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y: 0,
        flags: pad as u32,
    });
}

fn button_bit(b: sdl3::gamepad::Button) -> Option<u32> {
    use sdl3::gamepad::Button;
    Some(match b {
        Button::South => wire::BTN_A,
        Button::East => wire::BTN_B,
        Button::West => wire::BTN_X,
        Button::North => wire::BTN_Y,
        Button::Back => wire::BTN_BACK,
        Button::Start => wire::BTN_START,
        Button::Guide => wire::BTN_GUIDE,
        Button::LeftStick => wire::BTN_LS_CLICK,
        Button::RightStick => wire::BTN_RS_CLICK,
        Button::LeftShoulder => wire::BTN_LB,
        Button::RightShoulder => wire::BTN_RB,
        Button::DPadUp => wire::BTN_DPAD_UP,
        Button::DPadDown => wire::BTN_DPAD_DOWN,
        Button::DPadLeft => wire::BTN_DPAD_LEFT,
        Button::DPadRight => wire::BTN_DPAD_RIGHT,
        Button::Touchpad => wire::BTN_TOUCHPAD,
        // PADDLE1/2/3/4 = R4/L4/R5/L5 (host `input::gamepad`).
        Button::RightPaddle1 => wire::BTN_PADDLE1,
        Button::LeftPaddle1 => wire::BTN_PADDLE2,
        Button::RightPaddle2 => wire::BTN_PADDLE3,
        Button::LeftPaddle2 => wire::BTN_PADDLE4,
        Button::Misc1 => wire::BTN_MISC1,
        _ => return None,
    })
}

/// The menu-navigation state of one open pad. Read off the handle, not from events — the
/// console polls, so a stick that never moves again still holds its direction.
fn menu_sample(pad: &sdl3::gamepad::Gamepad) -> MenuSample {
    use sdl3::gamepad::{Axis, Button};
    MenuSample {
        buttons: [
            pad.button(Button::South),
            pad.button(Button::East),
            pad.button(Button::West),
            pad.button(Button::North),
            pad.button(Button::LeftShoulder),
            pad.button(Button::RightShoulder),
        ],
        lx: pad.axis(Axis::LeftX),
        ly: pad.axis(Axis::LeftY),
        dpad: [
            pad.button(Button::DPadUp),
            pad.button(Button::DPadDown),
            pad.button(Button::DPadLeft),
            pad.button(Button::DPadRight),
        ],
    }
}

/// Fold every open pad into the one sample [`MenuNav`] steps: buttons and dpad OR'd, stick
/// from whoever is furthest off centre. Two pads pushing at once read as one hand instead of
/// cancelling, and one `MenuNav` keeps one repeat clock — per-pad ones race on the same list.
/// The Skia console already merges this way (`console/mod.rs`); this is the desktop half.
fn merge_samples(samples: &[MenuSample]) -> MenuSample {
    let mut out = MenuSample::default();
    let mut best = -1i32;
    for s in samples {
        for i in 0..out.buttons.len() {
            out.buttons[i] |= s.buttons[i];
        }
        for i in 0..out.dpad.len() {
            out.dpad[i] |= s.dpad[i];
        }
        let mag = i32::from(s.lx).pow(2) + i32::from(s.ly).pow(2);
        if mag > best {
            best = mag;
            out.lx = s.lx;
            out.ly = s.ly;
        }
    }
    out
}

/// This pad is the one in someone's hands right now — a detent buzzes it, not its idle
/// neighbour. The stick test is the engage deadzone, so resting drift never claims the pulse.
fn is_acting(s: &MenuSample) -> bool {
    s.buttons.iter().chain(&s.dpad).any(|&b| b) || ring_sector(s.lx, s.ly, None).is_some()
}

/// SDL sticks are +y = down; the wire (XInput) is +y = up. Triggers 0..32767 → 0..255.
fn axis_value(axis: sdl3::gamepad::Axis, v: i16) -> (u32, i32) {
    use sdl3::gamepad::Axis;
    match axis {
        Axis::LeftX => (wire::AXIS_LS_X, (v as i32).max(-32767)),
        Axis::LeftY => (wire::AXIS_LS_Y, -(v as i32).max(-32767)),
        Axis::RightX => (wire::AXIS_RS_X, (v as i32).max(-32767)),
        Axis::RightY => (wire::AXIS_RS_Y, -(v as i32).max(-32767)),
        Axis::TriggerLeft => (wire::AXIS_LT, (v as i32).clamp(0, 32767) >> 7),
        Axis::TriggerRight => (wire::AXIS_RT, (v as i32).clamp(0, 32767) >> 7),
    }
}

/// Decimal or `0x`-hex (DS5 report bytes are named in hex). `None` on a typo so it
/// falls back to the default rather than to zero.
fn env_u8(key: &str) -> Option<u8> {
    let v = std::env::var(key).ok()?;
    let v = v.trim();
    match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        Some(hex) => u8::from_str_radix(hex, 16).ok(),
        None => v.parse().ok(),
    }
}

/// DualSense effects packet (SDL `DS5EffectsState_t`, 47 bytes). Offsets are the USB
/// output report **minus one**: SDL's payload has no leading report id. Deliberate
/// second copy — `pf-inject` owns the layout and this crate cannot import it.
/// [`ds5_offsets_track_the_usb_report`](ds5_feedback_tests) pins the `−1`.
struct Ds5Feedback;

impl Ds5Feedback {
    const REPORT_ID_LEN: usize = 1;
    /// Audio-control region (`ucHeadphoneVolume`…`ucAudioMuteBits`): USB report byte 5.
    const AUDIO: usize = 5 - Self::REPORT_ID_LEN;
    const RIGHT_TRIGGER: usize = 11 - Self::REPORT_ID_LEN;
    const LEFT_TRIGGER: usize = 22 - Self::REPORT_ID_LEN;
    const PAD_LIGHTS: usize = 44 - Self::REPORT_ID_LEN;
    /// `ucMicLightMode`: USB report byte 9.
    const MIC_LED: usize = 9 - Self::REPORT_ID_LEN;
    const LED_RGB: usize = 45 - Self::REPORT_ID_LEN;
    /// Mode byte plus 10 parameters — same width as `PUNKTFUNK_HID_EFFECT_MAX`.
    const TRIGGER_LEN: usize = punktfunk_core::abi::PUNKTFUNK_HID_EFFECT_MAX as usize;

    fn trigger_packet(which: u8, effect: &[u8]) -> [u8; 47] {
        let mut p = [0u8; 47];
        let (flag, off) = if which == 1 {
            (0x04, Self::RIGHT_TRIGGER)
        } else {
            (0x08, Self::LEFT_TRIGGER)
        };
        p[0] = flag;
        let n = effect.len().min(Self::TRIGGER_LEN);
        p[off..off + n].copy_from_slice(&effect[..n]);
        p
    }

    fn lightbar_packet(r: u8, g: u8, b: u8) -> [u8; 47] {
        let mut p = [0u8; 47];
        p[1] = 0x04; // valid_flag1 lightbar
        p[Self::LED_RGB] = r;
        p[Self::LED_RGB + 1] = g;
        p[Self::LED_RGB + 2] = b;
        p
    }

    fn player_packet(bits: u8) -> [u8; 47] {
        let mut p = [0u8; 47];
        p[1] = 0x10; // valid_flag1 player LEDs
        p[Self::PAD_LIGHTS] = bits & 0x1F;
        p
    }

    fn mic_led_packet(mode: u8) -> [u8; 47] {
        let mut p = [0u8; 47];
        p[1] = 0x01; // valid_flag1 mic-mute LED
        p[Self::MIC_LED] = mode;
        p
    }

    /// All-zero packet: `ucEnableBits1` bits 0/1 stay clear. SDL's rumble path sets both
    /// ("enable rumble emulation" + "disable audio haptics"), which mutes the 0xD1 coils.
    fn audio_haptics_packet() -> [u8; 47] {
        [0u8; 47]
    }

    /// Point channel 1 (shared headphone-R / mono speaker) at the speaker. Power-on is
    /// the jack, so PCM to the speaker pair is silent until `ucAudioEnableBits` bit 5
    /// (`0x20`) is set. Ship `0x20` not `0x30`: bit 4's headphone effect is unmeasured.
    /// Bits 0/1 stay clear (rumble-emulation / disable-audio-haptics mute the coils).
    /// The select persists across USB-audio restart; a later [`HidOutput::AudioCtl`]
    /// still overrides. `PUNKTFUNK_PAD_SPEAKER_PATH` / `_VOLUME` override per run.
    fn speaker_enable_packet(volume: u8, path: u8) -> [u8; 47] {
        let mut p = [0u8; 47];
        // bit5 = ucSpeakerVolume valid, bit7 = audio-control byte valid.
        p[0] = 0x20 | 0x80;
        p[Self::AUDIO + 1] = volume;
        p[Self::AUDIO + 3] = path;
        p
    }

    /// Fold [`HidOutput::AudioCtl`]: `raw` is report `0x02` bytes 5..=10 → offsets 4..=9.
    /// `flags` bits 1..4 become `p[0]` bits 4..7. Bit 0 is not replayed — bits 0/1 stay
    /// clear so audio haptics stay live ([`audio_haptics_packet`]).
    fn audio_ctl_packet(flags: u8, raw: &[u8; 6]) -> [u8; 47] {
        let mut p = [0u8; 47];
        p[0] = (flags & 0x1E) << 3;
        p[Self::AUDIO..Self::AUDIO + 6].copy_from_slice(raw);
        p
    }
}

/// One forwarded controller while a session is attached. Opening grabs the hidraw node
/// (SDL HIDAPI); idle/menu never populates slots.
struct Slot {
    /// SDL instance id (`ControllerDevice*::which`).
    id: u32,
    /// Wire pad index — lowest-free at open, stable for the slot's life.
    index: u8,
    pad: sdl3::gamepad::Gamepad,
    /// Physical kind, captured at open so feedback paths need no `&mut` SDL re-query.
    pref: GamepadPref,
    /// Kind declared in [`InputKind::GamepadArrival`] — the host's pretence, not `pref`.
    declared: GamepadPref,
    last_axis: [i32; 6],
    held_buttons: Vec<u32>,
    /// Host-believed contacts `(surface, finger)`; lifted on close. 0 = legacy pad, 1/2 = Steam.
    held_touches: std::collections::HashSet<(u8, u8)>,
    /// The button a Select chord took while Select was pending: neither press went out, so
    /// the release must not either.
    swallow_btn: Option<u32>,
    /// Per Steam surface (0 = left, 1 = right): last wire coords + finger-down. Clicks have no
    /// position, so the click forward reuses the live contact.
    surface_last: [(i16, i16, bool); 2],
    /// Held Steam-pad clicks: motion frames would otherwise clear the bit host-side.
    held_clicks: [bool; 2],
    last_accel: [i16; 3],
    /// At least one motion sample went out — gates the zero-gyro park in [`Worker::flush_slot`].
    sent_motion: bool,
    /// Log-once: this path runs at the pad's sensor rate.
    motion_unreachable_logged: bool,
    gesture: SelectGesture,
    /// bit0 = haptics, bit1 = speaker. Nonzero only for tier-A; bit0 also suppresses wire rumble
    /// while haptics frames arrive.
    audio_caps: u8,
    rumble_suppressed_logged: bool,
    /// A wire rumble went out since the coils were last armed; SDL's rumble bits mute them.
    coils_muted: bool,
    /// Raw passthrough for a Steam Controller 2 declared as one.
    sc2: Option<crate::sc2_capture::Sc2Capture>,
    /// The host sent raw writes, so its `0x80` reports own the motors and wire rumble is skipped.
    raw_rumble: bool,
}

impl Slot {
    fn new(
        id: u32,
        index: u8,
        pref: GamepadPref,
        declared: GamepadPref,
        pad: sdl3::gamepad::Gamepad,
    ) -> Slot {
        Slot {
            id,
            index,
            pad,
            pref,
            declared,
            last_axis: [i32::MIN; 6],
            held_buttons: Vec::new(),
            held_touches: std::collections::HashSet::new(),
            swallow_btn: None,
            surface_last: [(0, 0, false); 2],
            held_clicks: [false; 2],
            last_accel: [0; 3],
            sent_motion: false,
            motion_unreachable_logged: false,
            gesture: SelectGesture::default(),
            audio_caps: 0,
            rumble_suppressed_logged: false,
            coils_muted: false,
            sc2: None,
            raw_rumble: false,
        }
    }

    /// Two touchpads: `TouchpadEx` surface encoding and pad-click re-route.
    fn is_multi_touchpad(&self) -> bool {
        self.pad.touchpads_count() >= 2
    }
}

/// What a button pressed with Select already held asks for, whether or not the guide gesture
/// is on. Select-first only: A is the ring's own confirm, and X is a face button a game wants.
/// Once the hold became Guide, both belong to whatever that opened on the host.
fn select_chord(held: &[u32], bit: u32, select_as_guide: bool) -> Option<SelectChord> {
    if select_as_guide || !held.contains(&wire::BTN_BACK) {
        return None;
    }
    match bit {
        wire::BTN_A => Some(SelectChord::Ring),
        wire::BTN_X => Some(SelectChord::Stats),
        _ => None,
    }
}

/// Hold-Select→guide ([`GUIDE_HOLD`]). Pure: transitions + clock emit `(bit, down)`
/// pairs. Select alone is pending; another button already down passes through (the
/// escape chord ends in Select). A button while pending makes it a real Select (deferred
/// down first). Past [`GUIDE_HOLD`] it is synthetic Guide until release; earlier it is a
/// tap — press on release, up [`TAP_PRESS`] later (back-to-back folds into nothing).
#[derive(Default)]
struct SelectGesture {
    pending_since: Option<Instant>,
    as_guide: bool,
    release_due: Option<Instant>,
    /// Ring chord: the pending press never went out, so the release is dropped here.
    swallowed: bool,
}

impl SelectGesture {
    /// Returns true when the press is held back. `alone` = no other button held.
    fn on_select_down(&mut self, now: Instant, alone: bool, out: &mut Vec<(u32, bool)>) -> bool {
        // A previous tap's scheduled release is still owed: lift it before the new press.
        if self.release_due.take().is_some() {
            out.push((wire::BTN_BACK, false));
        }
        if alone {
            self.pending_since = Some(now);
            return true;
        }
        false
    }

    /// Ring chord: a pending Select never goes out, so its later release is dropped too.
    fn swallow_for_ring(&mut self) {
        if self.pending_since.take().is_some() {
            self.swallowed = true;
        }
    }

    /// Pending Select is real after all — deferred down goes out before the new button.
    fn on_other_down(&mut self, out: &mut Vec<(u32, bool)>) {
        if self.pending_since.take().is_some() {
            out.push((wire::BTN_BACK, true));
        }
    }

    /// True when the gesture owned this release (caller skips the normal button-up).
    fn on_select_up(&mut self, now: Instant, out: &mut Vec<(u32, bool)>) -> bool {
        if self.swallowed {
            self.swallowed = false;
            return true;
        }
        if self.as_guide {
            self.as_guide = false;
            out.push((wire::BTN_GUIDE, false));
            return true;
        }
        if self.pending_since.take().is_some() {
            out.push((wire::BTN_BACK, true));
            self.release_due = Some(now + TAP_PRESS);
            return true;
        }
        false
    }

    fn poll(&mut self, now: Instant, out: &mut Vec<(u32, bool)>) {
        if let Some(since) = self.pending_since {
            if now.duration_since(since) >= GUIDE_HOLD {
                self.pending_since = None;
                self.as_guide = true;
                out.push((wire::BTN_GUIDE, true));
            }
        }
        if let Some(due) = self.release_due {
            if now >= due {
                self.release_due = None;
                out.push((wire::BTN_BACK, false));
            }
        }
    }

    /// Slot close / disarm: nothing may stay down (or owed) on the wire.
    fn flush(&mut self, out: &mut Vec<(u32, bool)>) {
        self.pending_since = None;
        self.swallowed = false;
        if self.as_guide {
            self.as_guide = false;
            out.push((wire::BTN_GUIDE, false));
        }
        if self.release_due.take().is_some() {
            out.push((wire::BTN_BACK, false));
        }
    }
}

struct Worker {
    subsystem: sdl3::GamepadSubsystem,
    pads_out: Arc<Mutex<Vec<PadInfo>>>,
    active_out: Arc<Mutex<Option<PadInfo>>>,
    /// Open only while a session is attached; opening grabs hardware.
    slots: Vec<Slot>,
    /// Menu pads while menu mode is on and no session; mutually exclusive with `slots`.
    /// EVERY connected pad, not the newest one: a second controller is otherwise dead on
    /// the console, and one left shut keeps the dark lightbar `reset_slot_feedback` gave it.
    menu_open: Vec<(u32, sdl3::gamepad::Gamepad)>,
    /// Menu pad that last had a button or stick engaged — the one a detent pulse belongs in.
    menu_last: Option<u32>,
    /// Menu pad power, `(id, level)`. Cached: [`publish`](Self::publish) runs on every hotplug.
    battery: Option<(u32, PadBattery)>,
    battery_at: Option<Instant>,
    /// Connected ids in connection order (metadata only, no open).
    order: Vec<u32>,
    /// Stable key; unmatched pin is kept (survives disconnect) and falls through to automatic.
    pinned: Option<String>,
    /// Off: [`Self::forwarded_ids`] is empty so a session opens no slot (hidraw stays free).
    forwarding: bool,
    /// Whether a Select chord has anywhere to go. Off until the client says so, because a
    /// chord swallows the button pressed with Select.
    chords_live: bool,
    /// Applied at open to the kind DECLARED to the host, never to [`Slot::pref`].
    kind_override: GamepadPref,
    system_forward: bool,
    guide_gesture: bool,
    /// Owed synthetic-tap releases `(pad, bit, due)` — down went out on receipt.
    synthetic_ups: Vec<(u8, u32, Instant)>,
    /// bit0 = haptics, bit1 = speaker. `0` until declared: tier-A detection then never runs.
    pad_audio_prefs: u8,
    attached: Option<Arc<NativeClient>>,
    escape_tx: async_channel::Sender<()>,
    disconnect_tx: async_channel::Sender<()>,
    /// Escape chord fully held — latched so it fires once.
    chord_armed: bool,
    chord_since: Option<Instant>,
    disconnect_fired: bool,
    menu_mode: bool,
    menu_nav: MenuNav,
    menu_tx: async_channel::Sender<MenuEvent>,
    chord_tx: async_channel::Sender<(u8, SelectChord)>,
    /// Overlay owns input: pads held neutral, slots still OPEN.
    masked: bool,
    /// In-stream ring: first slot → [`MenuEvent`]s even while masked.
    ring_nav: bool,
}

impl Worker {
    fn active_id(&self) -> Option<u32> {
        // Pin matches by stable key (most-recent wins if two share one); unmatched falls
        // through to automatic without being cleared.
        if let Some(key) = &self.pinned {
            if let Some(id) = self
                .order
                .iter()
                .rev()
                .copied()
                .find(|&id| self.pad_info(id).is_some_and(|p| &p.key == key))
            {
                return Some(id);
            }
        }
        // Most recently connected, but never Steam Input's virtual pad while a real one exists.
        self.order
            .iter()
            .rev()
            .copied()
            .find(|&id| self.pad_info(id).is_some_and(|p| !p.steam_virtual))
            .or_else(|| self.order.last().copied())
    }

    /// ID-based metadata — no device open (an open would grab the hardware).
    fn pad_info(&self, id: u32) -> Option<PadInfo> {
        if !self.order.contains(&id) {
            return None;
        }
        let jid = sdl3::sys::joystick::SDL_JoystickID(id);
        let mut pref = pref_for_type(self.subsystem.type_for_id(jid));
        let (vid, pid) = (
            self.subsystem.vendor_for_id(jid).unwrap_or(0),
            self.subsystem.product_for_id(jid).unwrap_or(0),
        );
        // SDL has no Valve pad types; VID/PID picks the matching Deck / Steam Controller kind.
        if vid == 0x28DE && pid == 0x1205 {
            pref = GamepadPref::SteamDeck;
        }
        if vid == 0x28DE && matches!(pid, 0x1102 | 0x1142) {
            pref = GamepadPref::SteamController;
        }
        if let Some(sc2) = crate::sc2_capture::pref_for(vid, pid) {
            pref = sc2;
        }
        // Edge reports as PS5; VID/PID so paddles land on native slots, not the fold/drop policy.
        if vid == 0x054C && pid == 0x0DF2 {
            pref = GamepadPref::DualSenseEdge;
        }
        let name = self
            .subsystem
            .name_for_id(jid)
            .unwrap_or_else(|_| "Controller".into());
        Some(PadInfo {
            key: format!("{vid:04x}:{pid:04x}:{name}"),
            steam_virtual: (vid == 0x28DE && pid == 0x11FF)
                || name.starts_with("Steam Virtual Gamepad"),
            name,
            pref,
            // SDL reports power only for an OPEN device; `publish` fills the one we hold.
            battery: None,
            // `forwarded`/`rumble` are console-screen fields; rumble, like battery, needs OPEN.
            detail: format!("{vid:04X}:{pid:04X}"),
            forwarded: true,
            rumble: false,
        })
    }

    /// Pin: only that pad. Automatic: every real pad, or every virtual one when that is all
    /// Steam Input exposes (Deck game-mode — else gyro/paddles have nowhere to land).
    fn forwarded_ids(&self) -> Vec<u32> {
        if !self.forwarding {
            return Vec::new();
        }
        self.candidate_ids()
    }

    /// [`forwarded_ids`](Self::forwarded_ids) without the forwarding gate — what menu mode
    /// holds open. Console navigation is local, so a user who turned wire forwarding off
    /// still drives the launcher with the pad in their hands.
    fn candidate_ids(&self) -> Vec<u32> {
        if let Some(key) = &self.pinned {
            if let Some(id) = self
                .order
                .iter()
                .rev()
                .copied()
                .find(|&id| self.pad_info(id).is_some_and(|p| &p.key == key))
            {
                return vec![id];
            }
            // Unmatched pin falls through to Automatic; the pin itself is not cleared.
        }
        let real: Vec<u32> = self
            .order
            .iter()
            .copied()
            .filter(|&id| self.pad_info(id).is_some_and(|p| !p.steam_virtual))
            .collect();
        if !real.is_empty() {
            return real;
        }
        // Every pad is virtual: Steam Input is wrapping all of them, so none is a shadow of
        // another and forwarding the lot double-counts nothing. Taking only the newest is
        // what left a Deck with two controllers seeing just one.
        self.order.clone()
    }

    /// The one place that opens (= grabs) hardware. Dropping a handle is `SDL_CloseGamepad`;
    /// on a Deck the firmware watchdog then restores lizard mode.
    fn sync_open(&mut self) {
        if self.attached.is_some() {
            self.menu_open.clear();
            self.menu_last = None;
            self.reconcile_slots();
            return;
        }
        self.close_all_slots();
        let want = if self.menu_mode {
            self.candidate_ids()
        } else {
            Vec::new()
        };
        let before = self.menu_open.len();
        self.menu_open.retain(|(id, _)| want.contains(id));
        let mut changed = self.menu_open.len() != before;
        for id in want {
            if self.menu_open.iter().any(|(open, _)| *open == id) {
                continue;
            }
            match self.subsystem.open(sdl3::sys::joystick::SDL_JoystickID(id)) {
                Ok(pad) => {
                    self.menu_open.push((id, pad));
                    changed = true;
                }
                Err(e) => tracing::warn!(id, error = %e, "gamepad open failed"),
            }
        }
        if changed {
            // Hot-plug under the launcher: adopt held state instead of firing it. No sensors.
            self.menu_nav.reset();
        }
    }

    /// Close unwanted slots (flush first) and open newly-wanted pads into the lowest free
    /// index. A disconnect frees only its own index; others keep theirs.
    fn reconcile_slots(&mut self) {
        let want = self.forwarded_ids();
        let mut i = 0;
        while i < self.slots.len() {
            if want.contains(&self.slots[i].id) {
                i += 1;
            } else {
                self.close_slot_at(i);
            }
        }
        for id in want {
            if self.slots.iter().any(|s| s.id == id) {
                continue;
            }
            self.open_slot(id);
        }
    }

    fn open_slot(&mut self, id: u32) {
        let taken: Vec<u8> = self.slots.iter().map(|s| s.index).collect();
        let Some(index) = lowest_free_index(&taken) else {
            tracing::warn!(
                id,
                max = punktfunk_core::input::MAX_PADS,
                "gamepad slots full — controller not forwarded"
            );
            return;
        };
        let pref = match self.pad_info(id) {
            // Virtual pad in front of the Deck's built-in controls: declare Deck, not X360.
            // The host honors per-pad arrival over the session default ([`Self::auto_pref`]).
            Some(p) if p.steam_virtual && is_steam_deck() => GamepadPref::SteamDeck,
            Some(p) => p.pref,
            None => GamepadPref::Xbox360,
        };
        let declared = declared_kind(self.kind_override, pref);
        match self.subsystem.open(sdl3::sys::joystick::SDL_JoystickID(id)) {
            Ok(pad) => {
                let mut slot = Slot::new(id, index, pref, declared, pad);
                let raw_sc2 =
                    crate::sc2_capture::is_sc2(pref) && crate::sc2_capture::is_sc2(declared);
                // A raw SC2's IMU mode is Steam's to set, through the raw plane.
                if !raw_sc2 {
                    Self::set_slot_sensors(&mut slot, true);
                }
                slot.audio_caps = self.pad_audio_caps_for(id, &slot.pad);
                // Kind before any input so the host builds a matching virtual device. Core
                // re-sends against datagram loss; an older host ignores it.
                if let Some(c) = &self.attached {
                    // Caps first — core ORs them into this arrival (and every re-send). Always
                    // set (0 for non-tier-A): wire indices are reused; leftover bits would stick.
                    c.set_pad_audio_caps(index, slot.audio_caps);
                    send(
                        c,
                        InputKind::GamepadArrival,
                        declared.to_u8() as u32,
                        0,
                        index,
                    );
                    // Always set (defaults for a well-behaved pad): wire indices are reused.
                    let quirks = if pref == GamepadPref::SteamDeck {
                        ActuatorQuirks {
                            keepalive_ms: DECK_RUMBLE_KEEPALIVE_MS,
                            min_pulse_ms: 0,
                            dedup_jitter: true,
                        }
                    } else {
                        ActuatorQuirks::default()
                    };
                    c.set_rumble_quirks(index as u16, quirks);
                    // After the arrival, so the host builds the as-is pad before raw reports.
                    if raw_sc2 {
                        slot.sc2 = slot.pad.path().and_then(|path| {
                            crate::sc2_capture::Sc2Capture::open(
                                &path,
                                c.clone(),
                                index,
                                self.sc2_gate(),
                            )
                        });
                    }
                }
                if slot.audio_caps != 0 {
                    if slot.audio_caps & 0x01 != 0 {
                        // SDL rumble sets ucEnableBits1 0x01|0x02, muting the 0xD1 coils.
                        // Clear those bits at open; render_feedback clears them again when
                        // haptics resume after a rumble. Fails if hid-playstation owns the HID
                        // link — that driver asserts the same bit on every FF update.
                        if let Err(e) = slot.pad.send_effect(&Ds5Feedback::audio_haptics_packet()) {
                            tracing::info!(
                                index,
                                error = %e,
                                "could not re-arm the DualSense's audio-haptics bit (SDL does \
                                 not own this pad's HID link) — haptics still work unless \
                                 something else has rumbled the pad this plug-in"
                            );
                        }
                    }
                    if slot.audio_caps & 0x02 != 0 {
                        // Channel 1 powers up on the headphone jack; see `speaker_enable_packet`.
                        let path = env_u8("PUNKTFUNK_PAD_SPEAKER_PATH").unwrap_or(0x20);
                        let volume = env_u8("PUNKTFUNK_PAD_SPEAKER_VOLUME").unwrap_or(0x7F);
                        if let Err(e) = slot
                            .pad
                            .send_effect(&Ds5Feedback::speaker_enable_packet(volume, path))
                        {
                            tracing::info!(
                                index,
                                error = %e,
                                "could not point the DualSense at its own speaker (SDL does \
                                 not own this pad's HID link) — the pad's speaker may stay \
                                 silent even though the stream reaches it"
                            );
                        }
                    }
                    crate::pad_audio::register_tier_a(index, slot.pad.path());
                    tracing::info!(
                        index,
                        caps = slot.audio_caps,
                        "tier-A DualSense: pad-audio render caps declared"
                    );
                }
                tracing::info!(
                    id,
                    index,
                    pref = ?pref,
                    declared = ?declared,
                    raw = slot.sc2.is_some(),
                    "gamepad forwarding (slot opened)"
                );
                self.slots.push(slot);
            }
            Err(e) => tracing::warn!(id, error = %e, "gamepad open failed"),
        }
    }

    /// Settings prefs for a physical DualSense/Edge (VID:PID, never the declared kind) on
    /// a wired connection; 0 otherwise. Wired from SDL; Unknown falls back to the 4-ch
    /// audio sibling (Bluetooth exposes none).
    fn pad_audio_caps_for(&self, id: u32, pad: &sdl3::gamepad::Gamepad) -> u8 {
        if self.pad_audio_prefs == 0 {
            return 0;
        }
        let jid = sdl3::sys::joystick::SDL_JoystickID(id);
        let vid = self.subsystem.vendor_for_id(jid).unwrap_or(0);
        let pid = self.subsystem.product_for_id(jid).unwrap_or(0);
        if !crate::pad_audio::is_tier_a_ds5(vid, pid, true) {
            return 0;
        }
        use sdl3::joystick::ConnectionState;
        let wired = match pad.connection_state() {
            Ok(ConnectionState::Wired) => true,
            Ok(ConnectionState::Wireless) => false,
            _ => crate::pad_audio::wired_audio_sibling(pad.path().as_deref()),
        };
        if crate::pad_audio::is_tier_a_ds5(vid, pid, wired) {
            self.pad_audio_prefs
        } else {
            0
        }
    }

    /// Flush held wire state and drop the SDL handle. Flush is wire-only, so unplug is safe.
    fn close_slot_at(&mut self, i: usize) {
        // Raw reports stop first: one landing after the remove would outlive the slot.
        self.slots[i].sc2 = None;
        // Silence before the handle drops; do not depend on SDL at close. Errors if already gone.
        let _ = self.slots[i].pad.set_rumble(0, 0, 100);
        Self::reset_slot_feedback(&mut self.slots[i]);
        if let Some(c) = self.attached.clone() {
            Self::flush_slot(&c, &mut self.slots[i]);
            // After the flush so seq is past the zeroing snapshots; host seq-gates resurrection.
            send(&c, InputKind::GamepadRemove, 0, 0, self.slots[i].index);
        }
        let slot = self.slots.remove(i);
        if slot.audio_caps != 0 {
            crate::pad_audio::unregister_tier_a(slot.index);
            crate::pad_audio::clear_haptics_liveness(slot.index);
        }
        tracing::info!(
            id = slot.id,
            index = slot.index,
            "gamepad forwarding stopped (slot closed)"
        );
    }

    /// Neutral the physical pad before the handle closes. Rumble decays; adaptive-trigger
    /// and lightbar are latched in firmware and survive the stream. Best-effort: pad may
    /// already be gone.
    fn reset_slot_feedback(slot: &mut Slot) {
        if matches!(
            slot.pref,
            GamepadPref::DualSense | GamepadPref::DualSenseEdge
        ) {
            // Mode 0x00 = no effect. Both sides, then lightbar dark and player LEDs clear.
            for which in [0u8, 1] {
                let _ = slot
                    .pad
                    .send_effect(&Ds5Feedback::trigger_packet(which, &[0u8; 11]));
            }
            let _ = slot.pad.send_effect(&Ds5Feedback::lightbar_packet(0, 0, 0));
            let _ = slot.pad.send_effect(&Ds5Feedback::player_packet(0));
        } else {
            let _ = slot.pad.set_led(0, 0, 0);
        }
    }

    fn close_all_slots(&mut self) {
        while !self.slots.is_empty() {
            self.close_slot_at(0);
        }
    }

    /// The typed plane's mask and system-button routing, for raw SC2 reports.
    fn sc2_gate(&self) -> crate::sc2_capture::Gate {
        crate::sc2_capture::Gate {
            masked: self.masked,
            system_forward: self.system_forward,
        }
    }

    fn push_sc2_gate(&self) {
        let gate = self.sc2_gate();
        for cap in self.slots.iter().filter_map(|s| s.sc2.as_ref()) {
            cap.set_gate(gate);
        }
    }

    /// Motion sensors stream only while a session wants them (USB/BT bandwidth). Once at open.
    fn set_slot_sensors(slot: &mut Slot, enabled: bool) {
        use sdl3::sensor::SensorType;
        for s in [SensorType::Gyroscope, SensorType::Accelerometer] {
            // SAFETY: an SDL3 query on the gamepad this slot owns and keeps open; it takes a
            // plain sensor-type enum and only reads device state.
            if unsafe { slot.pad.has_sensor(s) } {
                let _ = slot.pad.sensor_set_enabled(s, enabled);
            }
        }
    }

    /// After hotplug or pin change. A pad holding the escape chord may have just unplugged.
    fn refresh_active(&mut self) {
        self.sync_open();
        self.rearm_escape();
        self.publish();
    }

    /// Zero host-held state. Wire events only — safe against an already-removed pad.
    fn flush_slot(c: &NativeClient, slot: &mut Slot) {
        let pad = slot.index;
        // Gesture first: synthetic Guide is not in `held_buttons`; a pending Select was never sent.
        let mut due = Vec::new();
        slot.gesture.flush(&mut due);
        // A chord's pending button-up is host-held state too. Its own `ButtonUp` is what
        // clears it, and masking the pad is exactly what stops that arriving — so it would
        // survive and eat the release of the NEXT real press, leaving that button down.
        slot.swallow_btn = None;
        for (b, down) in due {
            send(c, InputKind::GamepadButton, b, down as i32, pad);
        }
        for b in slot.held_buttons.drain(..) {
            send(c, InputKind::GamepadButton, b, 0, pad);
        }
        for (id, v) in slot.last_axis.iter_mut().enumerate() {
            if *v != 0 && *v != i32::MIN {
                send(c, InputKind::GamepadAxis, id as u32, 0, pad);
            }
            *v = i32::MIN;
        }
        for i in 0..2usize {
            if std::mem::take(&mut slot.held_clicks[i]) {
                let (x, y, _) = slot.surface_last[i];
                let _ = c.send_rich_input(RichInput::TouchpadEx {
                    pad,
                    surface: (i as u8) + 1,
                    finger: 0,
                    touch: false,
                    click: false,
                    x,
                    y,
                    pressure: 0,
                });
            }
        }
        slot.surface_last = [(0, 0, false); 2];
        for (surface, finger) in slot.held_touches.drain() {
            let rich = if surface == 0 {
                RichInput::Touchpad {
                    pad,
                    finger,
                    active: false,
                    x: 0,
                    y: 0,
                }
            } else {
                RichInput::TouchpadEx {
                    pad,
                    surface,
                    finger,
                    touch: false,
                    click: false,
                    x: 0,
                    y: 0,
                    pressure: 0,
                }
            };
            let _ = c.send_rich_input(rich);
        }
        // Gyro is level-triggered host-side; a close mid-rotation leaves the virtual pad turning.
        // Keep accel: gravity does not stop with the session.
        if std::mem::take(&mut slot.sent_motion) {
            let _ = c.send_rich_input(RichInput::Motion {
                pad,
                gyro: [0; 3],
                accel: slot.last_accel,
            });
        }
    }

    /// Overlay mask lifts: adopt buttons into `held_buttons` without a wire press (an A
    /// that picked a QAM row must not fire in the game). Axes are re-sent: the mask
    /// flushed them to zero and SDL only speaks on change, so a still-held stick would
    /// stay dead host-side.
    fn readopt_held(&mut self) {
        use sdl3::gamepad::{Axis, Button};
        const BUTTONS: [Button; 21] = [
            Button::South,
            Button::East,
            Button::West,
            Button::North,
            Button::Back,
            Button::Start,
            Button::Guide,
            Button::LeftStick,
            Button::RightStick,
            Button::LeftShoulder,
            Button::RightShoulder,
            Button::DPadUp,
            Button::DPadDown,
            Button::DPadLeft,
            Button::DPadRight,
            Button::Touchpad,
            Button::RightPaddle1,
            Button::LeftPaddle1,
            Button::RightPaddle2,
            Button::LeftPaddle2,
            Button::Misc1,
        ];
        const AXES: [Axis; 6] = [
            Axis::LeftX,
            Axis::LeftY,
            Axis::RightX,
            Axis::RightY,
            Axis::TriggerLeft,
            Axis::TriggerRight,
        ];
        let system_forward = self.system_forward;
        let attached = self.attached.clone();
        for slot in &mut self.slots {
            slot.held_buttons.clear();
            for b in BUTTONS {
                let Some(bit) = button_bit(b) else {
                    continue;
                };
                if !system_forward && matches!(bit, wire::BTN_GUIDE | wire::BTN_MISC1) {
                    continue;
                }
                if slot.pad.button(b) {
                    slot.held_buttons.push(bit);
                }
            }
            let Some(c) = &attached else {
                continue;
            };
            for a in AXES {
                let (id, v) = axis_value(a, slot.pad.axis(a));
                if slot.last_axis[id as usize] != v {
                    slot.last_axis[id as usize] = v;
                    send(c, InputKind::GamepadAxis, id, v, slot.index);
                }
            }
        }
        self.rearm_escape();
    }

    fn chord_held(&self) -> bool {
        self.slots
            .iter()
            .any(|s| ESCAPE_CHORD.iter().all(|b| s.held_buttons.contains(b)))
    }

    fn maybe_fire_escape(&mut self) {
        if self.chord_armed {
            return;
        }
        if self.chord_held() {
            self.chord_armed = true;
            self.chord_since = Some(Instant::now());
            let _ = self.escape_tx.try_send(());
            tracing::info!(
                "gamepad escape chord (L1+R1+Start+Select) — leaving fullscreen (hold to disconnect)"
            );
        }
    }

    /// Hold threshold and owed tap releases. ~10 ms attached jitter at most.
    fn gesture_poll(&mut self) {
        let Some(c) = self.attached.clone() else {
            self.synthetic_ups.clear();
            return;
        };
        let now = Instant::now();
        self.synthetic_ups.retain(|&(pad, bit, due)| {
            if now >= due {
                send(&c, InputKind::GamepadButton, bit, 0, pad);
                false
            } else {
                true
            }
        });
        if !self.guide_gesture {
            return;
        }
        for slot in &mut self.slots {
            let mut due = Vec::new();
            slot.gesture.poll(now, &mut due);
            for (b, down) in due {
                send(&c, InputKind::GamepadButton, b, down as i32, slot.index);
            }
        }
    }

    /// Polled so the hold completes without new events.
    fn maybe_fire_disconnect(&mut self) {
        if self.disconnect_fired {
            return;
        }
        if let Some(since) = self.chord_since {
            if since.elapsed() >= DISCONNECT_HOLD {
                self.disconnect_fired = true;
                let _ = self.disconnect_tx.try_send(());
                tracing::info!("gamepad escape chord held — disconnecting");
            }
        }
    }

    fn rearm_escape(&mut self) {
        if self.chord_armed && !self.chord_held() {
            self.reset_chord();
        }
    }

    /// Session boundary: hold-to-disconnect ends the session while the chord is still held,
    /// so button-ups arrive after detach and `rearm_escape` never runs. Without this the
    /// latch leaks into the next session (swallows the first chord or fires a stale disconnect).
    fn reset_chord(&mut self) {
        self.chord_armed = false;
        self.chord_since = None;
        self.disconnect_fired = false;
    }

    /// Steam pads: `TouchpadEx` (SDL 0 = left → surface 1, signed). DualSense: legacy unsigned.
    fn forward_touch(
        c: &NativeClient,
        slot: &mut Slot,
        touchpad: u32,
        finger: u8,
        x: f32,
        y: f32,
        active: bool,
    ) {
        let pad = slot.index;
        let multi = slot.is_multi_touchpad();
        let (cx, cy) = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
        let surface = if multi { (touchpad as u8) + 1 } else { 0 };
        let rich = if multi {
            let (wx, wy) = (
                (cx * 65535.0 - 32768.0) as i16,
                (cy * 65535.0 - 32768.0) as i16,
            );
            let i = (surface - 1).min(1) as usize;
            slot.surface_last[i] = (wx, wy, active);
            RichInput::TouchpadEx {
                pad,
                surface,
                finger,
                touch: active,
                // Click is a separate button event; carry held so motion cannot clear it.
                click: slot.held_clicks[i],
                x: wx,
                y: wy,
                pressure: 0,
            }
        } else {
            RichInput::Touchpad {
                pad,
                finger,
                active,
                x: (cx * 65535.0) as u16,
                y: (cy * 65535.0) as u16,
            }
        };
        let _ = c.send_rich_input(rich);
        if active {
            slot.held_touches.insert((surface, finger));
        } else {
            slot.held_touches.remove(&(surface, finger));
        }
    }

    /// Steam pad clicks arrive as buttons (`touchpad` = left, `misc2` = right). Must not
    /// ride the button plane: the host maps `BTN_TOUCHPAD` to the RIGHT pad. DualSense's
    /// single touchpad button stays a wire button.
    fn steam_click_surface(slot: &Slot, button: sdl3::gamepad::Button) -> Option<u8> {
        use sdl3::gamepad::Button;
        if !slot.is_multi_touchpad() {
            return None;
        }
        match button {
            Button::Touchpad => Some(1),
            Button::Misc2 => Some(2),
            _ => None,
        }
    }

    /// Clicks carry no position — reuse the live contact. `touch` stays asserted while
    /// down even if the touch event has not arrived yet.
    fn forward_click(c: &NativeClient, slot: &mut Slot, surface: u8, down: bool) {
        let i = (surface - 1).min(1) as usize;
        slot.held_clicks[i] = down;
        let (x, y, touching) = slot.surface_last[i];
        let _ = c.send_rich_input(RichInput::TouchpadEx {
            pad: slot.index,
            surface,
            finger: 0,
            touch: touching || down,
            click: down,
            x,
            y,
            pressure: 0,
        });
    }

    fn publish(&self) {
        // `pad_info` is open-free; SDL reports power only for an OPEN device. Other pads
        // stay `None` — we cannot know without grabbing hardware that is not ours.
        let with_battery = |id: u32| -> Option<PadInfo> {
            let mut info = self.pad_info(id)?;
            if let Some((bid, b)) = self.battery {
                if bid == id {
                    info.battery = Some(b);
                }
            }
            Some(info)
        };
        let mut list: Vec<PadInfo> = self
            .order
            .iter()
            .copied()
            .filter_map(with_battery)
            .collect();
        list.reverse();
        *self.pads_out.lock().unwrap() = list;
        *self.active_out.lock().unwrap() = self.active_id().and_then(with_battery);
    }

    /// Polled: nothing reports a battery changing. Menu pads only (the ones the console holds).
    fn battery_poll(&mut self) {
        // The UI shows one level, for the pad it calls active; with several open that is the
        // only one worth a poll (`publish` matches it back by id).
        let active = self.active_id();
        let Some((id, pad)) = self
            .menu_open
            .iter()
            .find(|(id, _)| Some(*id) == active)
            .or_else(|| self.menu_open.first())
        else {
            if self.battery.take().is_some() {
                self.publish();
            }
            self.battery_at = None;
            return;
        };
        let now = Instant::now();
        if self
            .battery_at
            .is_some_and(|t| now.duration_since(t) < BATTERY_POLL)
        {
            return;
        }
        self.battery_at = Some(now);
        let fresh = battery_of(pad).map(|b| (*id, b));
        if fresh != self.battery {
            self.battery = fresh;
            self.publish();
        }
    }

    /// False when the app side is gone and the worker should exit.
    fn drain_ctl(&mut self, ctl: &Receiver<Ctl>) -> bool {
        loop {
            match ctl.try_recv() {
                Ok(Ctl::Attach(c)) => {
                    self.attached = Some(c);
                    self.reset_chord();

                    // Valve HIDAPI only in-session. Not with forwarding off: enumeration
                    // kills the Deck trackpad-mouse and grabs hardware a passthrough needs.
                    if self.forwarding {
                        set_valve_hidapi(true);
                    }
                    self.sync_open();
                }
                Ok(Ctl::Detach) => {
                    self.close_all_slots();
                    self.attached = None;
                    self.reset_chord();
                    self.sync_open();
                    set_valve_hidapi(false);
                    if self.menu_mode {
                        // Adopt still-held buttons so the escape chord cannot ghost-fire the menu.
                        self.menu_nav.reset();
                    }
                }
                Ok(Ctl::Pin(key)) => {
                    self.pinned = key;
                    self.refresh_active();
                }
                Ok(Ctl::KindOverride(pref)) => self.kind_override = pref,
                Ok(Ctl::ChordsLive(on)) => self.chords_live = on,
                Ok(Ctl::SystemButtons {
                    forward_raw,
                    gesture,
                }) => {
                    self.system_forward = forward_raw;
                    self.push_sc2_gate();
                    if self.guide_gesture == gesture {
                        continue;
                    }
                    self.guide_gesture = gesture;
                    // Mid-session flip can strand a synthetic Guide or owed tap; lift now.
                    if let Some(c) = self.attached.clone() {
                        for slot in &mut self.slots {
                            let mut due = Vec::new();
                            slot.gesture.flush(&mut due);
                            for (b, down) in due {
                                send(&c, InputKind::GamepadButton, b, down as i32, slot.index);
                            }
                        }
                    }
                }
                Ok(Ctl::TapButton(bit)) => {
                    // Down on the first forwarded index (pad 0 if none). Up TAP_PRESS later.
                    if let Some(c) = self.attached.clone() {
                        let pad = self.slots.first().map_or(0, |s| s.index);
                        send(&c, InputKind::GamepadButton, bit, 1, pad);
                        self.synthetic_ups
                            .push((pad, bit, Instant::now() + TAP_PRESS));
                    }
                }
                Ok(Ctl::Mask(on)) => {
                    if self.masked == on {
                        continue;
                    }
                    self.masked = on;
                    self.push_sc2_gate();
                    if on {
                        // Neutral now, slots stay open — the host must not see an unplug.
                        if let Some(c) = self.attached.clone() {
                            for slot in &mut self.slots {
                                Self::flush_slot(&c, slot);
                            }
                        }
                        self.reset_chord();
                    } else {
                        self.readopt_held();
                        self.menu_nav.reset();
                    }
                    tracing::info!(masked = on, "overlay input mask");
                }
                Ok(Ctl::Forwarding(on)) => {
                    if self.forwarding == on {
                        continue;
                    }
                    self.forwarding = on;
                    self.reset_chord();

                    // ON: enable Valve HIDAPI before `sync_open` or a Deck pad opens under
                    // its old identity. OFF: disable after, so no slot outlives the driver.
                    let attached = self.attached.is_some();
                    if on && attached {
                        set_valve_hidapi(true);
                    }
                    self.sync_open();
                    if !on && attached {
                        set_valve_hidapi(false);
                    }
                }
                Ok(Ctl::PadAudioPrefs(bits)) => self.pad_audio_prefs = bits & 0x03,
                Ok(Ctl::MenuMode(on)) => {
                    self.menu_mode = on;
                    if on {
                        self.menu_nav.reset();
                    }
                    self.sync_open();
                }
                Ok(Ctl::RingNav(on)) => {
                    self.ring_nav = on;
                    self.menu_nav.reset();
                }
                Ok(Ctl::MenuRumble(pulse)) => {
                    if self.attached.is_none() {
                        // The pad that last acted, not the newest: with two on the console the
                        // detent belongs in the hands that moved the cursor.
                        let i = self
                            .menu_open
                            .iter()
                            .position(|(id, _)| Some(*id) == self.menu_last)
                            .unwrap_or(0);
                        if let Some((_, pad)) = self.menu_open.get_mut(i) {
                            let (low, high, ms) = match pulse {
                                MenuPulse::Move => (0, 0x3000, 25),
                                MenuPulse::Confirm => (0x5000, 0x5000, 60),
                                MenuPulse::Boundary => (0x6000, 0, 60),
                            };
                            let _ = pad.set_rumble(low, high, ms);
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn handle_event(&mut self, event: sdl3::event::Event) {
        use sdl3::event::Event;
        // Overlay owns the pad: drop input transitions (flushed neutral when the mask went
        // on). Add/remove still count — losing them would leave the slot table stale.
        if self.masked
            && matches!(
                event,
                Event::ControllerButtonDown { .. }
                    | Event::ControllerButtonUp { .. }
                    | Event::ControllerAxisMotion { .. }
                    | Event::ControllerTouchpadDown { .. }
                    | Event::ControllerTouchpadMotion { .. }
                    | Event::ControllerTouchpadUp { .. }
                    | Event::ControllerSensorUpdated { .. }
            )
        {
            return;
        }
        match event {
            Event::ControllerDeviceAdded { which, .. } => {
                if !self.order.contains(&which) {
                    self.order.push(which);
                    if let Some(p) = self.pad_info(which) {
                        tracing::info!(
                            name = p.name,
                            key = p.key,
                            pref = ?p.pref,
                            steam_virtual = p.steam_virtual,
                            "gamepad attached"
                        );
                    }
                    self.refresh_active();
                }
            }
            Event::ControllerDeviceRemoved { which, .. } => {
                if self.order.contains(&which) {
                    self.order.retain(|&id| id != which);
                    tracing::info!("gamepad detached");
                    self.refresh_active();
                }
            }
            Event::ControllerButtonDown { which, button, .. } => {
                let Some(c) = self.attached.clone() else {
                    return;
                };
                let Some(slot) = self.slots.iter_mut().find(|s| s.id == which) else {
                    return;
                };
                if let Some(surface) = Self::steam_click_surface(slot, button) {
                    Self::forward_click(&c, slot, surface, true);
                    return;
                }
                if let Some(bit) = button_bit(button) {
                    if !self.system_forward && matches!(bit, wire::BTN_GUIDE | wire::BTN_MISC1) {
                        return;
                    }
                    // Claimed only where the client can act on it: the ring withholds the press
                    // it takes, and eating a face button nothing receives is worse than the
                    // chord not working.
                    let chord = self
                        .chords_live
                        .then(|| select_chord(&slot.held_buttons, bit, slot.gesture.as_guide))
                        .flatten();
                    if let Some(chord) = chord {
                        let _ = self.chord_tx.try_send((slot.index, chord));
                        // A is the ring's own confirm, so the host must not also see it — a
                        // pending Select goes with it, and one already on the wire is lifted by
                        // the ring's mask flush. Stats changes nothing on the host, so that
                        // press carries on to the game, as it does on the Apple clients.
                        if chord == SelectChord::Ring {
                            slot.gesture.swallow_for_ring();
                            slot.swallow_btn = Some(bit);
                            slot.held_buttons.push(bit);
                            return;
                        }
                    }
                    let mut due = Vec::new();
                    let held_back = if !self.guide_gesture {
                        false
                    } else if bit == wire::BTN_BACK {
                        let alone = slot.held_buttons.is_empty();
                        slot.gesture.on_select_down(Instant::now(), alone, &mut due)
                    } else {
                        slot.gesture.on_other_down(&mut due);
                        false
                    };
                    for (b, down) in due {
                        send(&c, InputKind::GamepadButton, b, down as i32, slot.index);
                    }
                    // Chord bookkeeping sees the physical press even when the gesture holds it back.
                    slot.held_buttons.push(bit);
                    if !held_back {
                        send(&c, InputKind::GamepadButton, bit, 1, slot.index);
                    }
                    self.maybe_fire_escape();
                }
            }
            Event::ControllerButtonUp { which, button, .. } => {
                let Some(c) = self.attached.clone() else {
                    return;
                };
                let Some(slot) = self.slots.iter_mut().find(|s| s.id == which) else {
                    return;
                };
                if let Some(surface) = Self::steam_click_surface(slot, button) {
                    Self::forward_click(&c, slot, surface, false);
                    return;
                }
                if let Some(bit) = button_bit(button) {
                    if !self.system_forward && matches!(bit, wire::BTN_GUIDE | wire::BTN_MISC1) {
                        return;
                    }
                    slot.held_buttons.retain(|&b| b != bit);
                    if slot.swallow_btn == Some(bit) {
                        slot.swallow_btn = None;
                        return;
                    }
                    let mut due = Vec::new();
                    let owned = self.guide_gesture
                        && bit == wire::BTN_BACK
                        && slot.gesture.on_select_up(Instant::now(), &mut due);
                    for (b, down) in due {
                        send(&c, InputKind::GamepadButton, b, down as i32, slot.index);
                    }
                    if !owned {
                        send(&c, InputKind::GamepadButton, bit, 0, slot.index);
                    }
                    self.rearm_escape();
                }
            }
            Event::ControllerAxisMotion {
                which, axis, value, ..
            } => {
                let Some(c) = self.attached.clone() else {
                    return;
                };
                let Some(slot) = self.slots.iter_mut().find(|s| s.id == which) else {
                    return;
                };
                let (id, v) = axis_value(axis, value);
                if slot.last_axis[id as usize] != v {
                    slot.last_axis[id as usize] = v;
                    send(&c, InputKind::GamepadAxis, id, v, slot.index);
                }
            }
            Event::ControllerTouchpadDown {
                which,
                touchpad,
                finger,
                x,
                y,
                ..
            }
            | Event::ControllerTouchpadMotion {
                which,
                touchpad,
                finger,
                x,
                y,
                ..
            } => {
                let Some(c) = self.attached.clone() else {
                    return;
                };
                let Some(slot) = self.slots.iter_mut().find(|s| s.id == which) else {
                    return;
                };
                Self::forward_touch(&c, slot, touchpad as u32, finger as u8, x, y, true);
            }
            Event::ControllerTouchpadUp {
                which,
                touchpad,
                finger,
                x,
                y,
                ..
            } => {
                let Some(c) = self.attached.clone() else {
                    return;
                };
                let Some(slot) = self.slots.iter_mut().find(|s| s.id == which) else {
                    return;
                };
                Self::forward_touch(&c, slot, touchpad as u32, finger as u8, x, y, false);
            }
            Event::ControllerSensorUpdated {
                which,
                sensor,
                data,
                ..
            } => {
                let Some(c) = self.attached.clone() else {
                    return;
                };
                let Some(slot) = self.slots.iter_mut().find(|s| s.id == which) else {
                    return;
                };
                use sdl3::sensor::SensorType;
                match sensor {
                    SensorType::Accelerometer => {
                        for (i, v) in data.iter().enumerate() {
                            slot.last_accel[i] =
                                (v / G * ACCEL_LSB_PER_G).clamp(-32768.0, 32767.0) as i16;
                        }
                    }
                    SensorType::Gyroscope => {
                        // Per-pad declaration, not the session echo: under Auto the Hello
                        // carries pad 0's kind while pad 1 may still have a gyro.
                        if !punktfunk_core::config::pad_motion_reaches(
                            slot.declared,
                            c.requested_gamepad,
                            c.resolved_gamepad,
                        ) {
                            if !slot.motion_unreachable_logged {
                                slot.motion_unreachable_logged = true;
                                tracing::warn!(
                                    pad = slot.index,
                                    declared = ?slot.declared,
                                    resolved = ?c.resolved_gamepad,
                                    "this controller has a gyro but the host built it a backend \
                                     without one — motion will not reach the game; pick a \
                                     DualSense-class controller type to get it"
                                );
                            }
                            return;
                        }
                        let mut gyro = [0i16; 3];
                        for (i, v) in data.iter().enumerate() {
                            gyro[i] = (v * GYRO_LSB_PER_RAD_S).clamp(-32768.0, 32767.0) as i16;
                        }
                        slot.sent_motion = true;
                        let _ = c.send_rich_input(RichInput::Motion {
                            pad: slot.index,
                            gyro,
                            accel: slot.last_accel,
                        });
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn menu_poll(&mut self) {
        // Ring: every forwarded pad (masked off the wire) — the ring opens from whichever pad
        // pressed Select+A, which is not always slot 0. Else skip if overlay-masked so the
        // same stick cannot scroll Steam's UI and ours.
        let pads: Vec<(u32, &sdl3::gamepad::Gamepad)> = if self.ring_nav {
            self.slots.iter().map(|s| (s.id, &s.pad)).collect()
        } else if !self.menu_mode || self.attached.is_some() || self.masked {
            return;
        } else {
            self.menu_open.iter().map(|(id, p)| (*id, p)).collect()
        };
        if pads.is_empty() {
            return;
        }
        let samples: Vec<MenuSample> = pads.iter().map(|(_, p)| menu_sample(p)).collect();
        // Latest connected wins a tie, matching the active-pad rule everywhere else.
        if let Some(i) = samples.iter().rposition(is_acting) {
            self.menu_last = Some(pads[i].0);
        }
        let merged = merge_samples(&samples);
        let mut out = Vec::new();
        self.menu_nav.poll(&merged, Instant::now(), &mut out);
        for e in out {
            let _ = self.menu_tx.try_send(e);
        }
    }

    /// Apply one engine command verbatim. `backstop_ms` is the SDL duration — a hardware
    /// net under a stalled worker; the engine emits explicit zeros at every policy stop.
    fn issue_rumble(slot: &mut Slot, low: u16, high: u16, backstop_ms: u32) {
        let dur_ms: u32 = if (low, high) == (0, 0) {
            100
        } else {
            // No local floor: actuator floors live in `ActuatorQuirks::min_pulse_ms`.
            backstop_ms
        };
        match slot.pad.set_rumble(low, high, dur_ms) {
            Err(e) => {
                tracing::warn!(pad = slot.index, low, high, error = %e, "rumble: SDL set_rumble failed")
            }
            Ok(()) => tracing::trace!(pad = slot.index, low, high, "rumble: rendered"),
        }
    }

    /// Single consumer of rumble + HID output. Engine commands are already effective;
    /// this worker applies them verbatim. A raw SC2 hands its motors to the host's raw writes.
    /// A tier-A pad's coils go to whichever of wire rumble and haptics audio is arriving.
    fn render_feedback(&mut self) {
        let Some(connector) = self.attached.clone() else {
            return;
        };
        // Haptics resumed after a rumble muted the coils: arm them again.
        for slot in &mut self.slots {
            if slot.coils_muted && crate::pad_audio::haptics_live(slot.index) {
                slot.coils_muted = false;
                if let Err(e) = slot.pad.send_effect(&Ds5Feedback::audio_haptics_packet()) {
                    tracing::debug!(pad = slot.index, error = %e, "audio-haptics re-arm");
                }
            }
        }
        while let Ok(cmd) = connector.next_rumble_command(Duration::ZERO) {
            if let Some(slot) = self.slots.iter_mut().find(|s| s.index as u16 == cmd.pad) {
                if slot.raw_rumble {
                    continue;
                }
                // SDL rumble sets ucEnableBits1 0x01|0x02, muting the 0xD1 coils. A title that
                // renders no haptics audio sends no frames, so it keeps its rumble.
                let haptics = slot.audio_caps & 0x01 != 0;
                if haptics && crate::pad_audio::haptics_live(slot.index) {
                    if !slot.rumble_suppressed_logged {
                        slot.rumble_suppressed_logged = true;
                        tracing::info!(
                            pad = slot.index,
                            "wire rumble suppressed — the pad-audio haptics stream carries feedback"
                        );
                    }
                    continue;
                }
                slot.coils_muted |= haptics;
                Self::issue_rumble(slot, cmd.low, cmd.high, cmd.backstop_ms);
            }
        }
        while let Ok(hid) = connector.next_hidout(Duration::ZERO) {
            let idx = hidout_pad(&hid);
            let Some(slot) = self.slots.iter_mut().find(|s| s.index == idx) else {
                continue;
            };
            let is_ds = matches!(
                slot.pref,
                GamepadPref::DualSense | GamepadPref::DualSenseEdge
            );
            match hid {
                HidOutput::Led { r, g, b, .. } if is_ds => {
                    let _ = slot.pad.send_effect(&Ds5Feedback::lightbar_packet(r, g, b));
                }
                HidOutput::Led { r, g, b, .. } => {
                    let _ = slot.pad.set_led(r, g, b);
                }
                HidOutput::PlayerLeds { bits, .. } if is_ds => {
                    let _ = slot.pad.send_effect(&Ds5Feedback::player_packet(bits));
                }
                HidOutput::PlayerLeds { bits, .. } => {
                    let _ = set_player_leds(&slot.pad, bits);
                }
                HidOutput::MicLed { mode, .. } if is_ds => {
                    let _ = slot.pad.send_effect(&Ds5Feedback::mic_led_packet(mode));
                }
                HidOutput::Trigger {
                    which, ref effect, ..
                } if is_ds => {
                    let _ = slot
                        .pad
                        .send_effect(&Ds5Feedback::trigger_packet(which, effect));
                }
                // Only with a live tier-A renderer: replaying volumes at a silent pad
                // would mute/blast the next session's start state.
                HidOutput::AudioCtl { flags, raw, .. } if is_ds && slot.audio_caps != 0 => {
                    let _ = slot
                        .pad
                        .send_effect(&Ds5Feedback::audio_ctl_packet(flags, &raw));
                }
                HidOutput::HidRaw { kind, data, .. } => {
                    if slot.sc2.is_none() {
                        continue;
                    }
                    if !slot.raw_rumble {
                        // The host drives the motors raw from here on; drop SDL's held level.
                        slot.raw_rumble = true;
                        let _ = slot.pad.set_rumble(0, 0, 100);
                    }
                    if let Some(cap) = &slot.sc2 {
                        cap.write(kind, data);
                    }
                }
                HidOutput::Trigger { .. }
                | HidOutput::TrackpadHaptic { .. }
                | HidOutput::AudioCtl { .. }
                | HidOutput::MicLed { .. } => {}
            }
        }
    }
}

/// Wire bitmask (low 5) → SDL player index. Every convention here spells "player N" as
/// N lit LEDs; 0-based, so player 1 is index 0. No lit LED is *no* player, not player 0.
fn player_index_from_bits(bits: u8) -> Option<u16> {
    match (bits & 0x1F).count_ones() {
        0 => None,
        n => Some((n - 1) as u16),
    }
}

fn set_player_leds(pad: &sdl3::gamepad::Gamepad, bits: u8) -> Result<(), sdl3::Error> {
    match player_index_from_bits(bits) {
        None => pad.unset_player_index(),
        Some(i) => pad.set_player_index(i),
    }
}

fn hidout_pad(h: &HidOutput) -> u8 {
    match h {
        HidOutput::Led { pad, .. }
        | HidOutput::PlayerLeds { pad, .. }
        | HidOutput::Trigger { pad, .. }
        | HidOutput::TrackpadHaptic { pad, .. }
        | HidOutput::HidRaw { pad, .. }
        | HidOutput::MicLed { pad, .. } => *pad,
        // AudioCtl's pad is the plane's only u16; decode already rejects ≥ MAX_PADS.
        HidOutput::AudioCtl { pad, .. } => *pad as u8,
    }
}

impl Worker {
    fn new(
        subsystem: sdl3::GamepadSubsystem,
        pads_out: Arc<Mutex<Vec<PadInfo>>>,
        active_out: Arc<Mutex<Option<PadInfo>>>,
        escape_tx: async_channel::Sender<()>,
        disconnect_tx: async_channel::Sender<()>,
        menu_tx: async_channel::Sender<MenuEvent>,
        chord_tx: async_channel::Sender<(u8, SelectChord)>,
    ) -> Worker {
        Worker {
            subsystem,
            pads_out,
            active_out,
            slots: Vec::new(),
            menu_open: Vec::new(),
            menu_last: None,
            battery: None,
            battery_at: None,
            order: Vec::new(),
            pinned: None,
            forwarding: true,
            chords_live: false,
            kind_override: GamepadPref::Auto,
            system_forward: true,
            guide_gesture: false,
            synthetic_ups: Vec::new(),
            pad_audio_prefs: 0,
            attached: None,
            escape_tx,
            disconnect_tx,
            chord_armed: false,
            chord_since: None,
            disconnect_fired: false,
            menu_mode: false,
            menu_nav: MenuNav::new(),
            menu_tx,
            chord_tx,
            masked: false,
            ring_nav: false,
        }
    }
}

fn run(
    pads_out: Arc<Mutex<Vec<PadInfo>>>,
    active_out: Arc<Mutex<Option<PadInfo>>>,
    ctl: &Receiver<Ctl>,
    escape_tx: &async_channel::Sender<()>,
    disconnect_tx: &async_channel::Sender<()>,
    menu_tx: &async_channel::Sender<MenuEvent>,
    chord_tx: &async_channel::Sender<(u8, SelectChord)>,
) -> Result<(), String> {
    // Off-main-thread, no video: keep SDL away from signals; poll pads on this thread.
    sdl3::hint::set("SDL_NO_SIGNAL_HANDLERS", "1");
    sdl3::hint::set("SDL_JOYSTICK_THREAD", "1");
    // SDL defaults the Deck HIDAPI on; mere enumeration kills lizard mode.
    set_valve_hidapi(false);
    let sdl = sdl3::init().map_err(|e| e.to_string())?;
    let subsystem = sdl.gamepad().map_err(|e| e.to_string())?;
    let mut pump = sdl.event_pump().map_err(|e| e.to_string())?;

    let mut w = Worker::new(
        subsystem,
        pads_out,
        active_out,
        escape_tx.clone(),
        disconnect_tx.clone(),
        menu_tx.clone(),
        chord_tx.clone(),
    );

    loop {
        if !w.drain_ctl(ctl) {
            return Ok(());
        }

        // Wait, don't sleep+poll. 10 ms attached/menu bounds chord-hold and haptic
        // jitter (DISCONNECT_HOLD is 1500 ms). Idle wakes at 30 ms for hotplug + ctl.
        let timeout = Duration::from_millis(if w.attached.is_some() || w.menu_mode {
            10
        } else {
            30
        });
        if let Some(event) = pump.wait_event_timeout(timeout) {
            w.handle_event(event);
            while let Some(event) = pump.poll_event() {
                w.handle_event(event);
            }
        }

        w.gesture_poll();
        w.maybe_fire_disconnect();

        w.menu_poll();
        w.battery_poll();
        w.render_feedback();
    }
}

#[cfg(test)]
mod menu_merge_tests {
    use super::*;
    use crate::menu_nav::MENU_DEADZONE;

    fn held(dpad_right: bool, lx: i16) -> MenuSample {
        MenuSample {
            dpad: [false, false, false, dpad_right],
            lx,
            ..MenuSample::default()
        }
    }

    #[test]
    fn either_pad_drives_the_menu() {
        let idle = MenuSample::default();
        let pressing = MenuSample {
            buttons: [true, false, false, false, false, false],
            ..MenuSample::default()
        };
        // Player 2's A must survive the fold: the bug was that only one pad was ever
        // polled, so a second controller confirmed nothing until a session attached.
        assert!(merge_samples(&[idle, pressing]).buttons[0]);
        assert!(merge_samples(&[pressing, idle]).buttons[0]);
        assert!(!merge_samples(&[idle, idle]).buttons[0]);
    }

    #[test]
    fn the_furthest_stick_wins_so_idle_drift_cannot_cancel_it() {
        let pushed = held(false, 30000);
        let drifting = held(false, -300);
        assert_eq!(merge_samples(&[drifting, pushed]).lx, 30000);
        assert_eq!(merge_samples(&[pushed, drifting]).lx, 30000);
    }

    #[test]
    fn acting_needs_a_press_or_a_stick_past_the_deadzone() {
        assert!(!is_acting(&MenuSample::default()));
        assert!(!is_acting(&held(false, MENU_DEADZONE as i16 - 1)));
        assert!(is_acting(&held(false, MENU_DEADZONE as i16 + 1)));
        assert!(is_acting(&held(true, 0)));
    }
}

#[cfg(test)]
mod select_gesture_tests {
    use super::*;

    #[test]
    fn a_while_select_is_pending_swallows_both() {
        let mut g = SelectGesture::default();
        let mut out = Vec::new();
        let t = Instant::now();
        assert!(g.on_select_down(t, true, &mut out));
        g.swallow_for_ring();
        assert!(out.is_empty(), "the pending press stays swallowed: {out:?}");
        assert!(
            g.on_select_up(t, &mut out),
            "the release is owned, not forwarded"
        );
        assert!(out.is_empty(), "…and emits nothing: {out:?}");
    }

    #[test]
    fn select_then_a_or_x_is_a_chord_without_the_guide_gesture() {
        let held = |b: u32| select_chord(&[wire::BTN_BACK], b, false);
        assert_eq!(held(wire::BTN_A), Some(SelectChord::Ring));
        assert_eq!(held(wire::BTN_X), Some(SelectChord::Stats));
        // Every other face button is the game's, held Select or not.
        assert_eq!(held(wire::BTN_B), None);
        assert_eq!(held(wire::BTN_Y), None);
        assert_eq!(
            select_chord(&[wire::BTN_LB, wire::BTN_BACK], wire::BTN_A, false),
            Some(SelectChord::Ring),
            "other buttons held alongside do not disqualify it"
        );
        assert_eq!(select_chord(&[], wire::BTN_A, false), None, "A alone");
        assert_eq!(
            select_chord(&[wire::BTN_A], wire::BTN_BACK, false),
            None,
            "A first"
        );
        assert_eq!(
            select_chord(&[wire::BTN_BACK], wire::BTN_X, true),
            None,
            "Select became Guide"
        );
    }

    #[test]
    fn tap_delivers_press_then_scheduled_release() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out), "not held back");
        assert!(out.is_empty(), "a held-back press sends nothing yet");
        let up = t + Duration::from_millis(120);
        assert!(g.on_select_up(up, &mut out));
        assert_eq!(out, vec![(wire::BTN_BACK, true)]);
        out.clear();
        g.poll(up + TAP_PRESS - Duration::from_millis(1), &mut out);
        assert!(out.is_empty(), "release went out early");
        g.poll(up + TAP_PRESS, &mut out);
        assert_eq!(out, vec![(wire::BTN_BACK, false)]);
    }

    #[test]
    fn hold_becomes_guide_down_until_release() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out));
        g.poll(t + GUIDE_HOLD - Duration::from_millis(1), &mut out);
        assert!(out.is_empty(), "guide fired inside the threshold");
        g.poll(t + GUIDE_HOLD, &mut out);
        assert_eq!(out, vec![(wire::BTN_GUIDE, true)]);
        out.clear();
        g.poll(t + GUIDE_HOLD * 4, &mut out);
        assert!(out.is_empty());
        assert!(g.on_select_up(t + GUIDE_HOLD * 5, &mut out));
        assert_eq!(out, vec![(wire::BTN_GUIDE, false)]);
    }

    #[test]
    fn second_button_makes_pending_select_real() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out));
        g.on_other_down(&mut out);
        assert_eq!(out, vec![(wire::BTN_BACK, true)]);
        out.clear();
        assert!(!g.on_select_up(t + Duration::from_millis(200), &mut out));
        assert!(out.is_empty());
        g.poll(t + GUIDE_HOLD * 2, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn select_inside_a_combo_passes_through() {
        let mut g = SelectGesture::default();
        let mut out = Vec::new();
        assert!(!g.on_select_down(Instant::now(), false, &mut out));
        assert!(out.is_empty());
    }

    #[test]
    fn quick_repress_lifts_owed_release_first() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out));
        assert!(g.on_select_up(t + Duration::from_millis(80), &mut out));
        out.clear();
        assert!(g.on_select_down(t + Duration::from_millis(100), true, &mut out));
        assert_eq!(out, vec![(wire::BTN_BACK, false)]);
    }

    #[test]
    fn flush_lifts_synthetic_guide_and_owed_release() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out));
        g.poll(t + GUIDE_HOLD, &mut out);
        out.clear();
        g.flush(&mut out);
        assert_eq!(out, vec![(wire::BTN_GUIDE, false)]);
        out.clear();
        assert!(g.on_select_down(t, true, &mut out));
        assert!(g.on_select_up(t + Duration::from_millis(80), &mut out));
        out.clear();
        g.flush(&mut out);
        assert_eq!(out, vec![(wire::BTN_BACK, false)]);
        out.clear();
        assert!(g.on_select_down(t, true, &mut out));
        g.flush(&mut out);
        assert!(out.is_empty(), "a never-sent pending Select ghosted a send");
    }
}

#[cfg(test)]
mod slot_tests {
    use super::*;

    #[test]
    fn lowest_free_index_fills_gaps_and_bounds() {
        assert_eq!(lowest_free_index(&[]), Some(0));
        assert_eq!(lowest_free_index(&[0]), Some(1));
        assert_eq!(lowest_free_index(&[0, 1, 2]), Some(3));
        // A freed middle index is reused before growing — pad 0 and pad 2 stay put.
        assert_eq!(lowest_free_index(&[0, 2]), Some(1));
        assert_eq!(lowest_free_index(&[2, 0]), Some(1));
        let all: Vec<u8> = (0..punktfunk_core::input::MAX_PADS as u8).collect();
        assert_eq!(lowest_free_index(&all), None);
        let mut but_seven = all.clone();
        but_seven.retain(|&i| i != 7);
        assert_eq!(lowest_free_index(&but_seven), Some(7));
    }

    #[test]
    fn an_explicit_setting_is_what_every_pad_declares() {
        assert_eq!(
            declared_kind(GamepadPref::DualShock4, GamepadPref::DualSense),
            GamepadPref::DualShock4
        );
        for physical in [
            GamepadPref::DualSense,
            GamepadPref::Xbox360,
            GamepadPref::SwitchPro,
            GamepadPref::SteamDeck,
        ] {
            assert_eq!(
                declared_kind(GamepadPref::Xbox360, physical),
                GamepadPref::Xbox360
            );
        }
        assert_eq!(
            declared_kind(GamepadPref::Auto, GamepadPref::DualSense),
            GamepadPref::DualSense
        );
        assert_eq!(
            declared_kind(GamepadPref::Auto, GamepadPref::SteamDeck),
            GamepadPref::SteamDeck
        );
    }

    #[test]
    fn hidout_pad_reads_every_variant() {
        assert_eq!(
            hidout_pad(&HidOutput::Led {
                pad: 3,
                r: 1,
                g: 2,
                b: 3
            }),
            3
        );
        assert_eq!(hidout_pad(&HidOutput::PlayerLeds { pad: 5, bits: 1 }), 5);
        assert_eq!(
            hidout_pad(&HidOutput::Trigger {
                pad: 2,
                which: 0,
                effect: vec![1, 2, 3]
            }),
            2
        );
        assert_eq!(
            hidout_pad(&HidOutput::TrackpadHaptic {
                pad: 4,
                side: 0,
                amplitude: 1,
                period: 2,
                count: 3
            }),
            4
        );
        assert_eq!(
            hidout_pad(&HidOutput::HidRaw {
                pad: 6,
                kind: 0,
                data: vec![0x80, 0, 0]
            }),
            6
        );
        assert_eq!(
            hidout_pad(&HidOutput::AudioCtl {
                pad: 7,
                flags: 0,
                raw: [0; 6]
            }),
            7
        );
    }

    /// Bits 0/1 of `ucEnableBits1` stay clear: asserting either mutes the 0xD1 coils.
    #[test]
    fn speaker_enable_sets_volume_and_path_without_touching_the_haptics_bits() {
        let p = Ds5Feedback::speaker_enable_packet(0x7F, 0x20);
        assert_eq!(
            p[0] & 0x03,
            0,
            "rumble-emulation / disable-audio-haptics must stay clear"
        );
        assert_eq!(
            p[0],
            0x20 | 0x80,
            "speaker-volume + audio-control validity bits"
        );
        assert_eq!(p[5], 0x7F);
        assert_eq!(p[7], 0x20);
        for (i, b) in p.iter().enumerate() {
            if !matches!(i, 0 | 5 | 7) {
                assert_eq!(*b, 0, "byte {i} should be untouched");
            }
        }
    }

    /// A typo falls back to the default rather than silently meaning zero.
    #[test]
    fn env_u8_reads_hex_and_decimal() {
        assert_eq!(env_u8("PF_TEST_ABSENT_KEY_XYZ"), None);
        for (s, want) in [
            ("0x20", Some(0x20)),
            ("0X7f", Some(0x7F)),
            ("32", Some(32u8)),
        ] {
            let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                Some(hex) => u8::from_str_radix(hex, 16).ok(),
                None => s.parse().ok(),
            };
            assert_eq!(parsed, want, "{s}");
        }
    }

    /// Raw bytes 5..=10 land at offsets 4..=9; bits 0/1 of `p[0]` stay clear.
    #[test]
    fn audio_ctl_folds_report_bytes_into_effect_offsets() {
        let raw = [0x50, 0x60, 0x70, 0x05, 0x11, 0x22];
        let p = Ds5Feedback::audio_ctl_packet(0b1_0111, &raw);
        assert_eq!(&p[4..10], &raw, "report bytes 5..=10 → struct 4..=9");
        assert_eq!(p[0], 0b1011_0000);
        assert_eq!(
            p[0] & 0x03,
            0,
            "haptics-select must NOT replay into p[0] bits 0/1"
        );
        assert!(p[1..4].iter().all(|&b| b == 0));
        assert!(p[10..].iter().all(|&b| b == 0));
        let p = Ds5Feedback::audio_ctl_packet(0b0_0001, &raw);
        assert_eq!(p[0], 0);
        assert_eq!(&p[4..10], &raw);
        assert_eq!(Ds5Feedback::audio_haptics_packet(), [0u8; 47]);
    }
}

#[cfg(test)]
mod ds5_feedback_tests {
    use super::*;

    /// SDL payload offsets are USB report offsets minus the leading report id.
    #[test]
    fn ds5_offsets_track_the_usb_report() {
        for (usb, payload) in [
            (11usize, Ds5Feedback::RIGHT_TRIGGER),
            (22, Ds5Feedback::LEFT_TRIGGER),
            (44, Ds5Feedback::PAD_LIGHTS),
            (45, Ds5Feedback::LED_RGB),
            (9, Ds5Feedback::MIC_LED),
        ] {
            assert_eq!(payload, usb - 1, "payload offset for USB byte {usb}");
        }
        assert_eq!(Ds5Feedback::TRIGGER_LEN, 11);
    }

    #[test]
    fn lightbar_sets_only_its_enable_bit_and_its_three_bytes() {
        let p = Ds5Feedback::lightbar_packet(0x11, 0x22, 0x33);
        assert_eq!(p.len(), 47);
        assert_eq!(p[1], 0x04, "valid_flag1 lightbar bit");
        assert_eq!(p[0], 0, "must not claim any valid_flag0 field");
        assert_eq!(
            (
                p[Ds5Feedback::LED_RGB],
                p[Ds5Feedback::LED_RGB + 1],
                p[Ds5Feedback::LED_RGB + 2]
            ),
            (0x11, 0x22, 0x33)
        );
        let touched = [
            1,
            Ds5Feedback::LED_RGB,
            Ds5Feedback::LED_RGB + 1,
            Ds5Feedback::LED_RGB + 2,
        ];
        assert!(p
            .iter()
            .enumerate()
            .all(|(i, &b)| touched.contains(&i) || b == 0));
    }

    #[test]
    fn player_leds_are_masked_to_five_bits() {
        let p = Ds5Feedback::player_packet(0xFF);
        assert_eq!(p[1], 0x10, "valid_flag1 player-indicator bit");
        assert_eq!(
            p[Ds5Feedback::PAD_LIGHTS],
            0x1F,
            "high bits are not ours to set"
        );
        let p = Ds5Feedback::player_packet(0b0000_0101);
        assert_eq!(p[Ds5Feedback::PAD_LIGHTS], 0b0000_0101);
    }

    /// SDL's `ucEnableBits2` 0x01 enables `ucMicLightMode`; nothing else is claimed.
    #[test]
    fn mic_led_sets_only_its_enable_bit_and_mode() {
        let p = Ds5Feedback::mic_led_packet(2);
        assert_eq!(p[1], 0x01, "valid_flag1 mic-mute LED bit");
        assert_eq!(p[0], 0, "must not claim any valid_flag0 field");
        assert_eq!(p[Ds5Feedback::MIC_LED], 2);
        assert_eq!(p.iter().filter(|&&b| b != 0).count(), 2);
    }

    /// which 1 = R2, which 0 = L2; the RIGHT block sits first in the report.
    #[test]
    fn trigger_which_selects_the_right_flag_and_offset() {
        let eff: Vec<u8> = (1..=11).collect();

        let r = Ds5Feedback::trigger_packet(1, &eff);
        assert_eq!(r[0], 0x04, "valid_flag0 R2 bit");
        assert_eq!(
            &r[Ds5Feedback::RIGHT_TRIGGER..Ds5Feedback::RIGHT_TRIGGER + 11],
            &eff[..]
        );
        assert_eq!(
            r[Ds5Feedback::LEFT_TRIGGER],
            0,
            "the other trigger is untouched"
        );

        let l = Ds5Feedback::trigger_packet(0, &eff);
        assert_eq!(l[0], 0x08, "valid_flag0 L2 bit");
        assert_eq!(
            &l[Ds5Feedback::LEFT_TRIGGER..Ds5Feedback::LEFT_TRIGGER + 11],
            &eff[..]
        );
        assert_eq!(l[Ds5Feedback::RIGHT_TRIGGER], 0);
    }

    #[test]
    fn an_oversized_effect_is_clamped_rather_than_overflowing_into_the_next_field() {
        let long = vec![0xAAu8; 40];
        let p = Ds5Feedback::trigger_packet(1, &long);
        assert_eq!(p.len(), 47);
        assert_eq!(p[Ds5Feedback::RIGHT_TRIGGER + 10], 0xAA);
        assert_eq!(p[Ds5Feedback::RIGHT_TRIGGER + 11], 0);
        assert_eq!(p[Ds5Feedback::LEFT_TRIGGER], 0);
    }

    #[test]
    fn a_short_effect_leaves_the_rest_of_the_block_zeroed() {
        let p = Ds5Feedback::trigger_packet(0, &[0x02, 0x99]);
        assert_eq!(p[Ds5Feedback::LEFT_TRIGGER], 0x02);
        assert_eq!(p[Ds5Feedback::LEFT_TRIGGER + 1], 0x99);
        assert!(
            p[Ds5Feedback::LEFT_TRIGGER + 2..Ds5Feedback::LEFT_TRIGGER + 11]
                .iter()
                .all(|&b| b == 0)
        );
    }

    /// Empty effect is mode 0x00 = release. The enable bit must still be set, or the pad
    /// keeps the latched effect.
    #[test]
    fn an_empty_effect_is_a_release_not_a_no_op() {
        let p = Ds5Feedback::trigger_packet(1, &[]);
        assert_eq!(p[0], 0x04);
        assert!(
            p[Ds5Feedback::RIGHT_TRIGGER..Ds5Feedback::RIGHT_TRIGGER + 11]
                .iter()
                .all(|&b| b == 0)
        );
    }
}

#[cfg(test)]
mod reset_packet_tests {
    use super::*;

    /// Wrong enable flag or a non-zero mode byte leaves the effect latched.
    #[test]
    fn reset_packets_release_the_triggers_and_darken_the_lights() {
        let l = Ds5Feedback::trigger_packet(0, &[0u8; 11]);
        assert_eq!(l[0], 0x08, "left-trigger enable bit");
        assert!(
            l[Ds5Feedback::LEFT_TRIGGER..Ds5Feedback::LEFT_TRIGGER + 11]
                .iter()
                .all(|&b| b == 0),
            "an all-zero block is mode 0x00 = no effect"
        );
        let r = Ds5Feedback::trigger_packet(1, &[0u8; 11]);
        assert_eq!(r[0], 0x04, "right-trigger enable bit");
        assert!(
            r[Ds5Feedback::RIGHT_TRIGGER..Ds5Feedback::RIGHT_TRIGGER + 11]
                .iter()
                .all(|&b| b == 0)
        );

        let bar = Ds5Feedback::lightbar_packet(0, 0, 0);
        assert_eq!(bar[1], 0x04, "lightbar enable bit");
        assert_eq!(
            &bar[Ds5Feedback::LED_RGB..Ds5Feedback::LED_RGB + 3],
            &[0, 0, 0]
        );

        let pl = Ds5Feedback::player_packet(0);
        assert_eq!(pl[1], 0x10, "player-LED enable bit");
        assert_eq!(pl[Ds5Feedback::PAD_LIGHTS], 0);
    }
}

#[cfg(test)]
mod player_led_tests {
    use super::*;

    /// DualSense patterns are non-contiguous; Switch/XInput is a run of low bits. Count is N.
    #[test]
    fn player_index_counts_lit_leds_for_both_conventions() {
        assert_eq!(player_index_from_bits(0x04), Some(0));
        assert_eq!(player_index_from_bits(0x0A), Some(1));
        assert_eq!(player_index_from_bits(0x15), Some(2));
        assert_eq!(player_index_from_bits(0x1B), Some(3));
        assert_eq!(player_index_from_bits(0x1F), Some(4));

        assert_eq!(player_index_from_bits(0x01), Some(0));
        assert_eq!(player_index_from_bits(0x03), Some(1));
        assert_eq!(player_index_from_bits(0x07), Some(2));
        assert_eq!(player_index_from_bits(0x0F), Some(3));
    }

    /// No lit LED is "no player", not player 0.
    #[test]
    fn no_lit_led_is_no_player() {
        assert_eq!(player_index_from_bits(0x00), None);
        assert_eq!(player_index_from_bits(0xE0), None);
    }

    #[test]
    fn high_bits_are_masked_off_before_counting() {
        assert_eq!(player_index_from_bits(0xFF), Some(4));
        assert_eq!(player_index_from_bits(0xE4), Some(0));
    }
}
