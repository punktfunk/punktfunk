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
//!
//! `worker` owns SDL and the forwarded slots; `ds5` builds the DualSense effects packets;
//! `select_gesture` is the hold-Select state machine.

mod ds5;
mod select_gesture;
mod worker;

pub use crate::menu_nav::{MenuDir, MenuEvent, MenuPulse, PadBattery, PadInfo};
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::GamepadPref;
use punktfunk_core::input::gamepad as wire;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use worker::{run, Worker};

/// 1500 ms is long enough to be deliberate over a leave-fullscreen press.
const DISCONNECT_HOLD: Duration = Duration::from_millis(1500);

/// Hold Select alone this long for a synthetic host Guide
/// ([`SelectGesture`](select_gesture::SelectGesture)). The physical Guide never reaches
/// the host cleanly where the local shell owns it.
const GUIDE_HOLD: Duration = Duration::from_millis(350);

/// Delay between a held-back Select tap's press and its scheduled release. Core folds
/// per-transition sends into one `GamepadState`; down+up in one window vanish.
const TAP_PRESS: Duration = Duration::from_millis(50);

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
    /// Off drops the host's rumble ([`GamepadService::set_rumble`]).
    Rumble(bool),
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
    /// The last [`Self::set_kind_override`], as [`GamepadPref::to_u8`].
    kind: Arc<AtomicU8>,
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
            kind: Arc::default(),
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
                kind: Arc::default(),
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

    /// Explicit controller-type (`Auto` = per pad). The host builds a pad from its
    /// [`InputKind::GamepadArrival`](punktfunk_core::input::InputKind::GamepadArrival) and
    /// never swaps a built one, so a change mid-session re-plugs each forwarded pad whose
    /// declared kind moves.
    pub fn set_kind_override(&self, pref: GamepadPref) {
        self.kind.store(pref.to_u8(), Ordering::Relaxed);
        let _ = self.ctl.send(Ctl::KindOverride(pref));
    }

    /// The controller-type [`Self::set_kind_override`] last asked for.
    pub fn kind_override(&self) -> GamepadPref {
        GamepadPref::from_u8(self.kind.load(Ordering::Relaxed))
    }

    /// Off holds no slot: no arrival and the hidraw node stays free for a passthrough
    /// tool (SDL HIDAPI takes it at open). The escape chord listens only on forwarded
    /// pads; menu navigation is untouched.
    pub fn set_forwarding(&self, on: bool) {
        let _ = self.ctl.send(Ctl::Forwarding(on));
    }

    /// Overlay owns the controller: hold every forwarded pad NEUTRAL. Not
    /// [`set_forwarding`](Self::set_forwarding) — that sends
    /// [`GamepadRemove`](punktfunk_core::input::InputKind::GamepadRemove) (the game sees an
    /// unplug). Masking keeps slots open, flushes held state so a
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

    /// Tier-A capability bits declared at slot open (DualSense/Edge only; others
    /// 0). Call before [`Self::attach`]. Default is nothing — an embedder that never
    /// calls this keeps the wire bytes unchanged.
    pub fn set_pad_audio_prefs(&self, haptics: bool, speaker: bool) {
        let bits = (haptics as u8) | ((speaker as u8) << 1);
        let _ = self.ctl.send(Ctl::PadAudioPrefs(bits));
    }

    /// Off, the host's rumble never reaches a pad. Call before [`Self::attach`]; the
    /// default is on.
    pub fn set_rumble(&self, on: bool) {
        let _ = self.ctl.send(Ctl::Rumble(on));
    }

    pub fn attach(&self, connector: Arc<NativeClient>) {
        let _ = self.ctl.send(Ctl::Attach(connector));
    }

    pub fn detach(&self) {
        let _ = self.ctl.send(Ctl::Detach);
    }

    /// The active pad's kind, or the host default if none. Read *before* attach, when a
    /// Deck's built-in controls are still Steam Input's pad, which already reads as a Deck.
    /// A Deck with no pad at all is still a Deck, so paddles and gyro land.
    pub fn auto_pref(&self) -> GamepadPref {
        match self.active() {
            Some(p) => p.pref,
            None if is_steam_deck() => GamepadPref::SteamDeck,
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
        self.worker.battery_poll();
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
