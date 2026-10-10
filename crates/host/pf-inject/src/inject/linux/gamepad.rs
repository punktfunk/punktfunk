//! Virtual gamepads via `/dev/uinput`, cloning the kernel `xpad` identity so SDL/Steam/Proton
//! match their built-in mapping with no extra config. One [`VirtualPad`] per attached client
//! controller; [`GamepadManager`] applies decoded
//! [`GamepadFrame`](punktfunk_core::input::GamepadFrame)s.
//!
//! Rumble is the reverse path on the same fd: the game uploads FF effects
//! (`EV_UINPUT`/`UI_FF_UPLOAD` → `UI_BEGIN/END_FF_UPLOAD`) and plays them with `EV_FF`.
//! [`GamepadManager::pump_rumble`] must run every tick — a game's `EVIOCSFF` BLOCKS until
//! we answer `UI_END_FF_UPLOAD`. Mixdown is `(low, high)` for the host to send back.
//!
//! The uinput ABI, the device and the FF upload protocol live in [`crate::uinput_abi`]; the
//! mixdown is this file's own. On a seat the device is the supervisor's relay
//! ([`crate::pad_broker`]), built from [`build_pad`] there.

use crate::pad_broker::PadKind;
use crate::pad_slots::PadSlots;
use crate::uinput_abi::{
    AbsInfo, FfNotice, InputId, UinputDevice, EV_ABS, EV_FF, EV_KEY, EV_SYN, FF_GAIN, FF_RUMBLE,
    SYN_REPORT, UI_SET_EVBIT, UI_SET_FFBIT, UI_SET_KEYBIT,
};
use anyhow::Result;
use punktfunk_core::input::{gamepad, GamepadFrame, MAX_PADS};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_RZ: u16 = 0x05;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;

const BTN_SOUTH: u16 = 0x130; // A
const BTN_EAST: u16 = 0x131; // B
const BTN_NORTH: u16 = 0x133; // X
const BTN_WEST: u16 = 0x134; // Y
const BTN_TL: u16 = 0x136;
const BTN_TR: u16 = 0x137;
const BTN_SELECT: u16 = 0x13a;
const BTN_START: u16 = 0x13b;
const BTN_MODE: u16 = 0x13c;
const BTN_THUMBL: u16 = 0x13d;
const BTN_THUMBR: u16 = 0x13e;
// xpad Elite paddles: SDL reads HAPPY5/6 as the right pair and HAPPY7/8 as the left pair.
const BTN_TRIGGER_HAPPY5: u16 = 0x2c4;
const BTN_TRIGGER_HAPPY6: u16 = 0x2c5;
const BTN_TRIGGER_HAPPY7: u16 = 0x2c6;
const BTN_TRIGGER_HAPPY8: u16 = 0x2c7;

/// `(GameStream button bit, evdev key code)`. D-pad is HAT axes, not keys.
const BUTTON_MAP: [(u32, u16); 15] = [
    (gamepad::BTN_A, BTN_SOUTH),
    (gamepad::BTN_B, BTN_EAST),
    (gamepad::BTN_X, BTN_NORTH),
    (gamepad::BTN_Y, BTN_WEST),
    (gamepad::BTN_LB, BTN_TL),
    (gamepad::BTN_RB, BTN_TR),
    (gamepad::BTN_BACK, BTN_SELECT),
    (gamepad::BTN_START, BTN_START),
    (gamepad::BTN_GUIDE, BTN_MODE),
    (gamepad::BTN_LS_CLICK, BTN_THUMBL),
    (gamepad::BTN_RS_CLICK, BTN_THUMBR),
    // Wire PADDLE1/2/3/4 = R4/L4/R5/L5.
    (gamepad::BTN_PADDLE1, BTN_TRIGGER_HAPPY5),
    (gamepad::BTN_PADDLE2, BTN_TRIGGER_HAPPY7),
    (gamepad::BTN_PADDLE3, BTN_TRIGGER_HAPPY6),
    (gamepad::BTN_PADDLE4, BTN_TRIGGER_HAPPY8),
];

/// USB identity the virtual pad presents. SDL/Steam/Proton key the mapping off
/// `bustype/vendor/product/version` (+ name); games pick glyphs from it. Axis/button
/// layout is XInput either way — One/Series only changes glyphs. Impulse-trigger rumble
/// is not in evdev `FF_RUMBLE`.
#[derive(Clone, Copy)]
pub struct PadIdentity {
    /// What a seat asks the broker for.
    kind: PadKind,
    vendor: u16,
    product: u16,
    version: u16,
    name: &'static [u8],
    log: &'static str,
}

impl PadIdentity {
    /// Kernel `xpad` table entry `045e:028e`. SDL/Steam map it with no extra config.
    pub const fn xbox360() -> PadIdentity {
        PadIdentity {
            kind: PadKind::Xbox360,
            vendor: 0x045e,
            product: 0x028e,
            version: 0x0110,
            name: b"Microsoft X-Box 360 pad",
            log: "X-Box 360 pad",
        }
    }

    /// Kernel `xpad` table entry `045e:02ea`. One/Series glyphs; XInput-identical otherwise.
    pub const fn xbox_one() -> PadIdentity {
        PadIdentity {
            kind: PadKind::XboxOne,
            vendor: 0x045e,
            product: 0x02ea,
            version: 0x0408,
            name: b"Microsoft X-Box One S pad",
            log: "X-Box One S pad",
        }
    }

    /// Kernel `xpad` table entry `045e:0b00`. SDL's database has no USB row for it, so SDL's
    /// evdev mapping names `BTN_TRIGGER_HAPPY5-8` as the paddles; the 360 and One S rows do not.
    pub const fn elite2() -> PadIdentity {
        PadIdentity {
            kind: PadKind::XboxElite2,
            vendor: 0x045e,
            product: 0x0b00,
            version: 0x0511,
            name: b"Microsoft X-Box One Elite 2 pad",
            log: "X-Box One Elite 2 pad",
        }
    }

    /// The identity a broker request names: the supervisor builds from this table, never from
    /// anything the seat sent.
    pub(crate) const fn of(kind: PadKind) -> PadIdentity {
        match kind {
            PadKind::Xbox360 => PadIdentity::xbox360(),
            PadKind::XboxOne => PadIdentity::xbox_one(),
            PadKind::XboxElite2 => PadIdentity::elite2(),
        }
    }

    pub(crate) fn log(&self) -> &'static str {
        self.log
    }
}

/// The uinput pad `identity` describes, with `phys` stamped on it when the supervisor builds
/// one for a seat. The rumble plane is on: `ff_effects_max` must be > 0 or FF uploads are never
/// delivered.
pub(crate) fn build_pad(identity: PadIdentity, phys: Option<&str>) -> Result<UinputDevice> {
    let dev = UinputDevice::open()?;
    dev.set_bits(UI_SET_EVBIT, "UI_SET_EVBIT", &[EV_KEY, EV_ABS, EV_FF])?;
    dev.set_bits(
        UI_SET_KEYBIT,
        "UI_SET_KEYBIT",
        &BUTTON_MAP.map(|(_, key)| key),
    )?;
    dev.set_bits(UI_SET_FFBIT, "UI_SET_FFBIT", &[FF_RUMBLE, FF_GAIN])?;

    let stick = AbsInfo {
        minimum: -32768,
        maximum: 32767,
        fuzz: 16,
        flat: 128,
        ..Default::default()
    };
    let trigger = AbsInfo {
        minimum: 0,
        maximum: 255,
        ..Default::default()
    };
    let hat = AbsInfo {
        minimum: -1,
        maximum: 1,
        ..Default::default()
    };
    for (code, info) in [
        (ABS_X, stick),
        (ABS_Y, stick),
        (ABS_RX, stick),
        (ABS_RY, stick),
        (ABS_Z, trigger),
        (ABS_RZ, trigger),
        (ABS_HAT0X, hat),
        (ABS_HAT0Y, hat),
    ] {
        dev.abs(code, info)?;
    }
    if let Some(phys) = phys {
        dev.set_phys(phys)?;
    }
    let id = InputId {
        bustype: 0x0003, // BUS_USB
        vendor: identity.vendor,
        product: identity.product,
        version: identity.version,
    };
    dev.create(id, identity.name, 16)?;
    Ok(dev)
}

impl Default for PadIdentity {
    fn default() -> PadIdentity {
        PadIdentity::xbox360()
    }
}

/// Played-effect window: `replay.delay` of silence, then `replay.length` of rumble.
#[derive(Clone, Copy)]
struct Playback {
    /// `play + replay.delay`. Armed but silent until then.
    starts: Instant,
    /// When it stops, or `None` for replay length 0 (until explicitly stopped).
    ends: Option<Instant>,
}

struct Effect {
    strong: u16,
    weak: u16,
    playing: Option<Playback>,
    replay_ms: u16,
    /// Silence after play. `replay.length` runs from the end of this delay, so the delay
    /// shifts the window instead of eating into it.
    delay_ms: u16,
}

impl Effect {
    /// Silent for `replay.delay`, then `replay.length` of rumble (length 0 = until stopped).
    /// Length is measured from the end of the delay, not from the play command.
    fn window(&self, at: Instant) -> Playback {
        let starts = at + Duration::from_millis(self.delay_ms as u64);
        Playback {
            starts,
            ends: (self.replay_ms > 0)
                .then(|| starts + Duration::from_millis(self.replay_ms as u64)),
        }
    }
}

/// Game-side FF table and mixdown (finite-replay expiry + abandoned infinite force-off).
/// Split from [`VirtualPad`] so the policy is testable without a uinput fd.
struct FfState {
    effects: HashMap<i16, Effect>,
    gain: u32,
    /// Last `(low, high)` reported, to dedup.
    last_mix: (u16, u16),
    /// Last upload/erase/play/stop/gain. An infinite-replay effect still playing past the
    /// idle window against this was abandoned — kernel auto-erase only runs on fd close.
    /// Finite effects keep their declared deadline. SDL re-plays held rumble every ~2 s.
    last_activity: Instant,
}

impl FfState {
    fn new() -> FfState {
        FfState {
            effects: HashMap::new(),
            gain: 0xFFFF,
            last_mix: (0, 0),
            last_activity: Instant::now(),
        }
    }

    fn note_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Fold one thing the game did into the table. Every notice is activity.
    fn apply(&mut self, notice: FfNotice) {
        self.note_activity();
        match notice {
            FfNotice::Upload {
                id,
                strong,
                weak,
                replay_ms,
                delay_ms,
            } => {
                let slot = self.effects.entry(id).or_insert(Effect {
                    strong: 0,
                    weak: 0,
                    playing: None,
                    replay_ms: 0,
                    delay_ms: 0,
                });
                slot.strong = strong;
                slot.weak = weak;
                slot.replay_ms = replay_ms;
                slot.delay_ms = delay_ms;
            }
            FfNotice::Erase { id } => {
                self.effects.remove(&id);
            }
            FfNotice::Gain(gain) => self.gain = gain.min(0xFFFF),
            FfNotice::Play { id, on } => {
                if let Some(e) = self.effects.get_mut(&id) {
                    e.playing = on.then(|| e.window(Instant::now()));
                }
            }
        }
    }

    /// `Some` only when mixed `(low, high)` changed since last call.
    fn mix(&mut self, now: Instant, idle: Option<Duration>) -> Option<(u16, u16)> {
        let quiet_since = |t: Instant| idle.is_some_and(|d| now.duration_since(t) >= d);
        let plane_stale = quiet_since(self.last_activity);
        let (mut strong, mut weak) = (0u32, 0u32);
        for e in self.effects.values_mut() {
            let Some(p) = e.playing else { continue };
            // Still inside `replay.delay`: armed, silent, not a candidate for expiry or the
            // abandoned-effect force-off — it has not had its turn yet.
            if now < p.starts {
                continue;
            }
            match p.ends {
                Some(d) if now >= d => e.playing = None,
                // Infinite-replay, no FF traffic for `idle`. Kernel auto-erase only runs on
                // fd close. Require audible-for-`idle` too: play is last_activity, so a delay
                // longer than idle would die on its first contributing tick.
                None if plane_stale && quiet_since(p.starts) => {
                    tracing::info!(
                        strong = e.strong,
                        weak = e.weak,
                        "rumble: stale infinite FF effect (game stopped driving the pad) — forcing off"
                    );
                    e.playing = None;
                }
                _ => {
                    strong = strong.saturating_add(e.strong as u32);
                    weak = weak.saturating_add(e.weak as u32);
                }
            }
        }
        // Linux FF: strong = low-frequency (big) motor, weak = high-frequency motor.
        let low = ((strong.min(0xFFFF) * self.gain) >> 16) as u16;
        let high = ((weak.min(0xFFFF) * self.gain) >> 16) as u16;
        (self.last_mix != (low, high)).then(|| {
            self.last_mix = (low, high);
            (low, high)
        })
    }
}

pub struct VirtualPad {
    dev: UinputDevice,
    ff: FfState,
}

impl VirtualPad {
    /// On a seat the supervisor builds the device and this holds its relay; the box's own host
    /// opens `/dev/uinput` itself.
    pub fn create(index: usize, identity: PadIdentity) -> Result<VirtualPad> {
        let dev = if pf_paths::seat::is_seat_host() {
            let relay = crate::pad_broker::request(identity.kind, index as u8)?;
            tracing::info!(
                index,
                pad = identity.log,
                "virtual gamepad created (seat broker relay)"
            );
            UinputDevice::relayed(relay)?
        } else {
            let dev = build_pad(identity, None)?;
            tracing::info!(
                index,
                pad = identity.log,
                "virtual gamepad created (uinput)"
            );
            dev
        };
        Ok(VirtualPad {
            dev,
            ff: FfState::new(),
        })
    }

    /// `false` once the supervisor dropped this pad's relay.
    pub(crate) fn alive(&self) -> bool {
        self.dev.alive()
    }

    pub fn apply(&mut self, f: &GamepadFrame) {
        // Absolute state every frame, not XOR edges: `emit` is best-effort, so a dropped
        // edge would stick until that button toggles again. Kernel input drops an EV_KEY
        // that already matches device state (BTN_* does not autorepeat).
        for (bit, key) in BUTTON_MAP {
            self.dev.emit(EV_KEY, key, ((f.buttons & bit) != 0) as i32);
        }

        // Moonlight: +Y = up; evdev: +Y = down → negate (i32 math avoids -(-32768) overflow).
        self.dev.emit(EV_ABS, ABS_X, f.ls_x as i32);
        self.dev.emit(EV_ABS, ABS_Y, -(f.ls_y as i32));
        self.dev.emit(EV_ABS, ABS_RX, f.rs_x as i32);
        self.dev.emit(EV_ABS, ABS_RY, -(f.rs_y as i32));
        self.dev.emit(EV_ABS, ABS_Z, f.left_trigger as i32);
        self.dev.emit(EV_ABS, ABS_RZ, f.right_trigger as i32);
        let hat_x = ((f.buttons & gamepad::BTN_DPAD_RIGHT != 0) as i32)
            - ((f.buttons & gamepad::BTN_DPAD_LEFT != 0) as i32);
        let hat_y = ((f.buttons & gamepad::BTN_DPAD_DOWN != 0) as i32)
            - ((f.buttons & gamepad::BTN_DPAD_UP != 0) as i32);
        self.dev.emit(EV_ABS, ABS_HAT0X, hat_x);
        self.dev.emit(EV_ABS, ABS_HAT0Y, hat_y);
        self.dev.emit(EV_SYN, SYN_REPORT, 0);
    }

    /// Drain the FF plane into the table. `Some` when mixed `(low, high)` changed.
    fn pump_ff(&mut self) -> Option<(u16, u16)> {
        while let Some(notice) = self.dev.next_ff() {
            self.ff.apply(notice);
        }
        self.ff
            .mix(Instant::now(), crate::uhid_manager::rumble_idle_timeout())
    }
}

/// Evdev holds last-known state kernel-side, so this rides [`PadSlots`] with no extra
/// vec or heartbeat.
pub struct GamepadManager {
    slots: PadSlots<VirtualPad>,
    /// Shared by every pad in the session.
    identity: PadIdentity,
}

impl Default for GamepadManager {
    fn default() -> GamepadManager {
        GamepadManager::new()
    }
}

impl GamepadManager {
    pub fn new() -> GamepadManager {
        GamepadManager::with_identity(PadIdentity::xbox360())
    }

    pub fn with_identity(identity: PadIdentity) -> GamepadManager {
        GamepadManager {
            slots: PadSlots::new(identity.log, "gamepad", ""),
            identity,
        }
    }

    /// Show this session's pads to one seat alone
    /// ([`PadSlots::expose_in`](crate::pad_slots::PadSlots::expose_in)).
    pub fn expose_in(&mut self, dir: Option<std::path::PathBuf>) {
        self.slots.expose_in(dir);
    }

    pub fn handle(&mut self, ev: &punktfunk_core::input::GamepadEvent) {
        use punktfunk_core::input::GamepadEvent;
        match ev {
            GamepadEvent::Arrival { index, kind, .. } => {
                tracing::info!(index, kind, "controller arrival ({})", self.slots.label());
                self.ensure(*index as usize);
            }
            GamepadEvent::State(f) => {
                let idx = f.index as usize;
                if idx >= MAX_PADS {
                    return;
                }
                // Drop any allocated pad whose mask bit cleared. No per-index sibling
                // state to reset — the pads mix rumble internally.
                self.slots.sweep(f.active_mask);
                if f.active_mask & (1 << idx) == 0 {
                    return; // this event WAS the unplug
                }
                self.ensure(idx);
                let lost_relay = self.slots.get_mut(idx).is_some_and(|pad| {
                    pad.apply(f);
                    !pad.alive()
                });
                // The supervisor restarted: its next answer is a new pad, made by `ensure`.
                if lost_relay {
                    tracing::warn!(
                        index = idx,
                        "virtual gamepad lost its relay — making it again"
                    );
                    self.slots.remove(idx);
                }
            }
        }
    }

    fn ensure(&mut self, idx: usize) {
        let identity = self.identity;
        // `VirtualPad::create` logs its own success line (it knows the identity + transport).
        self.slots
            .ensure(idx, |i| VirtualPad::create(i as usize, identity));
    }

    /// Service every pad's FF protocol. `send(index, low, high, left_trigger, right_trigger)`
    /// runs when mixed rumble changed. Call every tick: games block in `EVIOCSFF` until answered.
    /// Trigger levels are always 0: `FF_RUMBLE` is `{strong, weak}` with no third field.
    pub fn pump_rumble(&mut self, mut send: impl FnMut(u16, u16, u16, u16, u16)) {
        // Reap an unplug whose removal frame only armed the grace — that frame is sent once,
        // so without this the uinput node outlives the controller. The swept mask is unused:
        // this manager has no per-index sibling state.
        self.slots.reap();
        for (i, pad) in self.slots.iter_mut() {
            if let Some((low, high)) = pad.pump_ff() {
                send(i as u16, low, high, 0, 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uapi;
    use crate::uinput_abi::input_event;
    use std::io::Write;
    use std::os::fd::AsFd;
    use std::time::Duration;

    /// Every key the generic pad emits is the row `gamepad-button-vectors.json` gives its
    /// bit, so the web console names a press the way an evdev dump reads it.
    #[test]
    fn button_map_matches_the_shared_vectors() {
        let raw =
            include_str!("../../../../../core/punktfunk-core/testdata/gamepad-button-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("vector file parses");
        let keyed: Vec<(u32, u16)> = file["buttons"]
            .as_array()
            .expect("buttons array")
            .iter()
            .filter_map(|r| Some((r["bit"].as_u64()? as u32, r["code"].as_u64()? as u16)))
            .collect();
        assert_eq!(keyed.len(), BUTTON_MAP.len());
        for pair in BUTTON_MAP {
            assert!(keyed.contains(&pair), "{pair:x?}");
        }
    }

    /// The evdev node for `name` that also advertises `FF`. A match without it is a
    /// sibling node effects cannot be written to.
    fn find_ff_node(name: &str) -> Option<String> {
        let s = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
        let mut cur = String::new();
        let mut node = None;
        for line in s.lines() {
            if let Some(n) = line.strip_prefix("N: Name=") {
                cur = n.trim_matches('"').to_string();
            } else if let Some(h) = line.strip_prefix("H: Handlers=") {
                if cur.contains(name) {
                    node = h
                        .split_whitespace()
                        .find(|t| t.starts_with("event"))
                        .map(|ev| format!("/dev/input/{ev}"));
                }
            } else if line.starts_with("B: FF=")
                && cur.contains(name)
                && node.is_some()
                && !line.trim_end().ends_with("FF=0")
            {
                return node;
            }
        }
        None
    }

    /// Poll for the node udev is still publishing, up to `timeout`.
    fn wait_ff_node(name: &str, timeout: Duration) -> Option<String> {
        let start = Instant::now();
        loop {
            if let Some(node) = find_ff_node(name) {
                return Some(node);
            }
            if start.elapsed() >= timeout {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Upload + play an `FF_RUMBLE`. Returns the OPEN fd (close erases the process's effects)
    /// and the kernel-assigned id. `EVIOCSFF` BLOCKS until the uinput owner answers
    /// `UI_FF_UPLOAD` — the caller must not be the thread running [`VirtualPad::pump_ff`].
    fn evdev_rumble(node: &str, strong: u16, weak: u16) -> std::io::Result<(std::fs::File, i16)> {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(node)?;
        let mut eff = [0u8; 48]; // struct ff_effect; union (rumble magnitudes) at offset 16
        eff[0..2].copy_from_slice(&FF_RUMBLE.to_ne_bytes());
        eff[2..4].copy_from_slice(&(-1i16).to_ne_bytes()); // id: kernel assigns
        eff[10..12].copy_from_slice(&5000u16.to_ne_bytes()); // replay.length ms
        eff[16..18].copy_from_slice(&strong.to_ne_bytes());
        eff[18..20].copy_from_slice(&weak.to_ne_bytes());
        // EVIOCSFF = _IOW('E', 0x80, struct ff_effect)
        let req: libc::c_ulong = (1 << 30) | (48 << 16) | (0x45 << 8) | 0x80;
        uapi::ioctl_with(f.as_fd(), req, &mut eff)?;
        let id = i16::from_ne_bytes([eff[2], eff[3]]);
        f.write_all(&input_event(EV_FF, id as u16, 1))?; // play
        Ok((f, id))
    }

    #[test]
    #[ignore = "creates a real /dev/uinput device; needs the input group"]
    fn ff_upload_reaches_pump_and_stops_on_erase() {
        let mut pad = VirtualPad::create(0, PadIdentity::xbox360()).expect("create uinput pad");
        let node = wait_ff_node("Microsoft X-Box 360 pad", Duration::from_millis(700))
            .expect("no force-feedback X-Box 360 evdev node");
        let game = std::thread::spawn(move || {
            let r = evdev_rumble(&node, 0xC000, 0x4000);
            std::thread::sleep(Duration::from_millis(1200)); // hold the effect, then erase
            r.expect("EVIOCSFF/play (fd held meanwhile)");
        });
        let start = Instant::now();
        let mut seen = Vec::new();
        while start.elapsed() < Duration::from_millis(2500) {
            if let Some(mix) = pad.pump_ff() {
                seen.push(mix);
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        game.join().unwrap();
        // Requested magnitudes scaled by the 0xFFFF default gain (>> 16).
        assert!(
            seen.contains(&(0xBFFF, 0x3FFF)),
            "evdev FF rumble never surfaced through pump_ff: {seen:?}"
        );
        assert_eq!(
            seen.last(),
            Some(&(0, 0)),
            "erase-on-close never produced a stop mix: {seen:?}"
        );
    }
}

#[cfg(test)]
mod ff_state_tests {
    use super::*;

    /// The default idle window the shared hatch resolves to when the env is unset.
    const IDLE: Option<Duration> = Some(Duration::from_millis(2500));

    /// `gain` is 0xFFFF (not a true 1.0 multiplier), so a magnitude loses 1 LSB in the mixdown.
    fn scaled(v: u16) -> u16 {
        ((v as u32 * 0xFFFF) >> 16) as u16
    }

    fn ff_with(effect: Effect) -> FfState {
        let mut ff = FfState::new();
        ff.effects.insert(0, effect);
        ff
    }

    /// Playing from `at`, no delay, until explicitly stopped.
    fn playing(at: Instant) -> Option<Playback> {
        Some(Playback {
            starts: at,
            ends: None,
        })
    }

    fn playing_for(at: Instant, len: Duration) -> Option<Playback> {
        Some(Playback {
            starts: at,
            ends: Some(at + len),
        })
    }

    #[test]
    fn abandoned_infinite_effect_is_forced_off_after_idle_window() {
        let now = Instant::now();
        let mut ff = ff_with(Effect {
            strong: 0x8000,
            weak: 0,
            // Playing since before the window: abandoned means audible AND unattended.
            playing: playing(now - Duration::from_millis(2600)),
            replay_ms: 0,
            delay_ms: 0,
        });
        assert_eq!(ff.mix(now, IDLE), Some((scaled(0x8000), 0)));
        assert_eq!(ff.mix(now, IDLE), None); // unchanged level; still playing
                                             // FF plane quiet past the idle window: cut, exactly once.
        ff.last_activity = now - Duration::from_millis(2600);
        assert_eq!(ff.mix(now, IDLE), Some((0, 0)));
        assert_eq!(ff.mix(now, IDLE), None); // already off — no repeat
    }

    #[test]
    fn finite_effect_honors_its_replay_deadline_not_the_idle_window() {
        let now = Instant::now();
        let mut ff = ff_with(Effect {
            strong: 0x4000,
            weak: 0,
            playing: playing_for(now, Duration::from_secs(10)),
            replay_ms: 10_000,
            delay_ms: 0,
        });
        // FF plane long stale, but a finite replay is the contract — keep playing.
        ff.last_activity = now - Duration::from_secs(60);
        assert_eq!(ff.mix(now, IDLE), Some((scaled(0x4000), 0)));
        // Expires at its own deadline, not the idle window.
        assert_eq!(ff.mix(now + Duration::from_secs(11), IDLE), Some((0, 0)));
    }

    #[test]
    fn replay_after_cut_rearms_the_effect() {
        let now = Instant::now();
        let mut ff = ff_with(Effect {
            strong: 0x8000,
            weak: 0,
            playing: playing(now - Duration::from_millis(3000)),
            replay_ms: 0,
            delay_ms: 0,
        });
        assert_eq!(ff.mix(now, IDLE), Some((scaled(0x8000), 0)));
        ff.last_activity = now - Duration::from_millis(3000);
        assert_eq!(ff.mix(now, IDLE), Some((0, 0)));
        ff.last_activity = now;
        ff.effects.get_mut(&0).unwrap().playing = playing(now);
        assert_eq!(ff.mix(now, IDLE), Some((scaled(0x8000), 0)));
    }

    #[test]
    fn replay_delay_holds_the_effect_off_then_gives_it_its_full_length() {
        let now = Instant::now();
        let starts = now + Duration::from_millis(500);
        let mut ff = ff_with(Effect {
            strong: 0x8000,
            weak: 0,
            playing: Some(Playback {
                starts,
                ends: Some(starts + Duration::from_secs(1)),
            }),
            replay_ms: 1000,
            delay_ms: 500,
        });
        // Inside the delay: armed but silent.
        assert_eq!(ff.mix(now, IDLE), None);
        assert_eq!(ff.mix(now + Duration::from_millis(499), IDLE), None);
        // Delay elapsed: it plays.
        assert_eq!(
            ff.mix(now + Duration::from_millis(501), IDLE),
            Some((scaled(0x8000), 0))
        );
        // Still playing at 1400 ms — full second FROM the delay, not from play.
        assert_eq!(ff.mix(now + Duration::from_millis(1400), IDLE), None);
        // Ends at delay + length, not at length.
        assert_eq!(
            ff.mix(now + Duration::from_millis(1600), IDLE),
            Some((0, 0))
        );
    }

    /// Playback window from the uploaded fields. Separate from `mix` because the `EV_FF`
    /// handler that calls [`Effect::window`] needs a live uinput fd; a mix-only test would
    /// pass with the delay ignored.
    #[test]
    fn window_offsets_the_whole_playback_by_replay_delay() {
        let at = Instant::now();

        let delayed = Effect {
            strong: 0,
            weak: 0,
            playing: None,
            replay_ms: 1000,
            delay_ms: 500,
        };
        let w = delayed.window(at);
        assert_eq!(
            w.starts,
            at + Duration::from_millis(500),
            "delay defers the start"
        );
        assert_eq!(
            w.ends,
            Some(at + Duration::from_millis(1500)),
            "length runs from the END of the delay, so the effect keeps its full second"
        );

        let plain = Effect {
            strong: 0,
            weak: 0,
            playing: None,
            replay_ms: 1000,
            delay_ms: 0,
        };
        let w = plain.window(at);
        assert_eq!(w.starts, at);
        assert_eq!(w.ends, Some(at + Duration::from_millis(1000)));

        // Length 0 = until stopped, but the delay still applies.
        let infinite = Effect {
            strong: 0,
            weak: 0,
            playing: None,
            replay_ms: 0,
            delay_ms: 250,
        };
        let w = infinite.window(at);
        assert_eq!(w.starts, at + Duration::from_millis(250));
        assert_eq!(w.ends, None);
    }

    /// A delayed effect is not "abandoned" while it is still waiting: it has not had its
    /// turn, and the idle window can be shorter than a legitimate delay.
    #[test]
    fn a_waiting_effect_is_not_cut_by_the_idle_watchdog() {
        let now = Instant::now();
        let starts = now + Duration::from_secs(5);
        let mut ff = ff_with(Effect {
            strong: 0x8000,
            weak: 0,
            playing: Some(Playback { starts, ends: None }),
            replay_ms: 0,
            delay_ms: 5000,
        });
        ff.last_activity = now - Duration::from_secs(60); // long stale
        assert_eq!(ff.mix(now, IDLE), None); // silent, but not cut
                                             // Plays once the delay elapses.
        assert_eq!(
            ff.mix(now + Duration::from_millis(5001), IDLE),
            Some((scaled(0x8000), 0))
        );
    }

    #[test]
    fn disabled_watchdog_never_cuts() {
        let now = Instant::now();
        let mut ff = ff_with(Effect {
            strong: 0x8000,
            weak: 0,
            playing: playing(now),
            replay_ms: 0,
            delay_ms: 0,
        });
        ff.last_activity = now - Duration::from_secs(600);
        assert_eq!(ff.mix(now, None), Some((scaled(0x8000), 0)));
    }
}
