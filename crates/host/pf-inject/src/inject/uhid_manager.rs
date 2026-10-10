//! Virtual-pad manager ([`UhidManager`]) shared by the Linux UHID/usbip pads and every Windows
//! UMDF pad, XUSB included. Event routing, frame merge, rich-input, silence heartbeat, and the
//! rumble + hidout-dedup feedback pump live here; a backend supplies only its per-controller
//! pieces via [`PadProto`].
//!
//! The Linux uinput pad writes frames through [`PadSlots`] itself: evdev holds the last state,
//! so it needs no state vec, heartbeat, or rich plane. Rumble plane:
//! `design/trigger-rumble-plane.md`.

use crate::hidout_dedup::HidoutDedup;
use crate::pad_slots::PadSlots;
use anyhow::Result;
use punktfunk_core::input::{GamepadEvent, GamepadFrame, MAX_PADS};
use punktfunk_core::quic::{HidOutput, RichInput};
use std::time::{Duration, Instant};

/// One poll of a pad's driver/kernel channel. `rumble` is the 0xCA plane; `hidout` is 0xCD
/// (lightbar / LEDs / adaptive triggers), both deduped before forward.
#[derive(Default)]
pub struct PadFeedback {
    /// `(low, high, left_trigger, right_trigger)` if this poll saw a rumble report.
    /// Range is `0..=0xFFFF`; consumers narrow with `>> 8` (0xFF00 and 0xFFFF both become 255).
    /// Trailing pair is Xbox impulse-trigger motors on the 0xCA v3 tail
    /// (`design/trigger-rumble-plane.md`). Only the Windows HID Xbox pad ever sets them: its
    /// `0x03` report has those fields. Every other protocol reports `(low, high, 0, 0)` —
    /// DualSense trigger actuators are adaptive (0xCD), not motors.
    pub rumble: Option<(u16, u16, u16, u16)>,
    pub hidout: Vec<HidOutput>,
    /// `Some(true)` iff this poll saw a vibration-asserting output report (including explicit
    /// zero). `None` = backend does not track activity, so [`UhidManager::pump`] never force-offs.
    /// Keyed on the rumble plane, not general output, so an LED/trigger stream cannot keep an
    /// abandoned rumble alive.
    pub rumble_drove: Option<bool>,
    /// Driver-channel drain overflowed this poll; downstream feedback state is unknown.
    /// [`UhidManager::pump`] force-stops rumble and re-arms hidout dedup. Windows ring drains
    /// only; Linux kernel channels drain losslessly.
    pub resync: bool,
}

/// Per-pad report state (`DsState`, `SteamState`). `Copy` so the manager can snapshot into
/// [`PadProto::write_state`] without holding the pad borrow. The state's own model, shared by
/// every transport and OS that presents it.
pub trait PadState: Copy {
    /// A fresh pad: centred, nothing pressed, at rest.
    fn neutral() -> Self;

    /// Zero this state's angular velocity; keep acceleration (gravity is persistent). Returns
    /// whether anything changed, so a pad already at rest costs no write.
    ///
    /// Motion is level-triggered: [`PadProto::merge_frame`] preserves the last sample and the
    /// heartbeat re-emits it, so a client that stops sending Motion leaves a constant rotation.
    /// Idle watchdog, driven from [`MOTION_IDLE_TIMEOUT`]. A state with no motion plane leaves
    /// this a no-op.
    fn neutralize_gyro(&mut self) -> bool {
        false
    }

    /// Reset rich-plane fields (touch + motion) to a fresh pad, leaving buttons, sticks, and
    /// feedback cursors. Used on [`Sweep::reclaimed`](crate::pad_slots::Sweep::reclaimed): a
    /// different controller inherits a live virtual pad without `reset_pad`.
    fn clear_rich(&mut self) {}
}

impl PadState for crate::dualsense_proto::DsState {
    fn neutral() -> Self {
        Self::neutral()
    }
    fn neutralize_gyro(&mut self) -> bool {
        Self::neutralize_gyro(self)
    }
    fn clear_rich(&mut self) {
        Self::clear_rich(self)
    }
}

impl PadState for crate::steam_proto::SteamState {
    fn neutral() -> Self {
        Self::neutral()
    }
    fn neutralize_gyro(&mut self) -> bool {
        Self::neutralize_gyro(self)
    }
    fn clear_rich(&mut self) {
        Self::clear_rich(self)
    }
}

impl PadState for crate::switch_proto::SwitchState {
    fn neutral() -> Self {
        Self::neutral()
    }
    fn neutralize_gyro(&mut self) -> bool {
        Self::neutralize_gyro(self)
    }
    fn clear_rich(&mut self) {
        Self::clear_rich(self)
    }
}

impl PadState for crate::switch2_proto::Switch2State {
    fn neutral() -> Self {
        Self::neutral()
    }
    fn neutralize_gyro(&mut self) -> bool {
        Self::neutralize_gyro(self)
    }
    fn clear_rich(&mut self) {
        Self::clear_rich(self)
    }
}

impl PadState for crate::eightbitdo_proto::EightBitDoState {
    fn neutral() -> Self {
        Self::neutral()
    }
    fn neutralize_gyro(&mut self) -> bool {
        Self::neutralize_gyro(self)
    }
    fn clear_rich(&mut self) {
        Self::clear_rich(self)
    }
}

impl PadState for crate::hori_proto::HoriState {
    fn neutral() -> Self {
        Self::neutral()
    }
    fn neutralize_gyro(&mut self) -> bool {
        Self::neutralize_gyro(self)
    }
    fn clear_rich(&mut self) {
        Self::clear_rich(self)
    }
}

/// Motion lives inside the opaque raw report, so there is nothing to neutralize or clear.
impl PadState for crate::triton_proto::TritonState {
    fn neutral() -> Self {
        Self::neutral()
    }
}

/// No rich plane on an Xbox pad.
impl PadState for crate::xbox_proto::XboxState {
    fn neutral() -> Self {
        Self::default()
    }
}

/// Per-controller half of a stateful virtual-pad backend: transport open, GameStream/rich-input
/// mappers, state write, and feedback poll. `&mut self` lets a backend carry configuration
/// (Steam-paddle remap, pad identity); most implementations are otherwise stateless.
pub trait PadProto {
    type Pad;
    type State: PadState;

    const LABEL: &'static str;
    const DEVICE: &'static str;
    /// Suffix for the create-failure line — empty on Linux, the driver-install hint on Windows.
    const CREATE_HINT: &'static str;
    /// Fixed report rate for a pad whose consumers assume one: with no IMU clock in the report,
    /// SDL stamps each sample one period apart. `Some`: input only updates state and
    /// [`UhidManager::heartbeat`] writes on deadlines. `None`: every frame writes.
    fn report_period(&self) -> Option<Duration> {
        None
    }

    /// Backend logs success; the manager logs create-gate failures.
    fn open(&mut self, idx: u8) -> Result<Self::Pad>;
    /// Fold one button/stick frame into a new state, preserving from `prev` every field that
    /// arrives on the rich plane (touch / motion). Paddle remap applies here too.
    fn merge_frame(&self, prev: &Self::State, f: &GamepadFrame) -> Self::State;
    fn apply_rich(&self, st: &mut Self::State, rich: RichInput);
    /// Write the full state to the pad (best-effort; the next frame or heartbeat re-syncs).
    fn write_state(&self, pad: &mut Self::Pad, st: &Self::State);
    /// Poll the pad's driver/kernel channel: answer any pending handshake and return the
    /// feedback it carried. `idx` is the wire pad index (DualSense GET_REPORT replies need it).
    fn service(&self, pad: &mut Self::Pad, idx: u8) -> PadFeedback;
    /// Heartbeat write NOW regardless of the silence gap (Steam gamepad-mode-entry pulse).
    fn force_heartbeat(&self, _pad: &Self::Pad) -> bool {
        false
    }
}

/// All virtual pads of one stateful backend. Method surface (`new` / `handle` / `apply_rich` /
/// `pump` / `heartbeat`) matches the session input thread, so each backend re-exports as
/// `pub type … = UhidManager<…Proto>`.
pub struct UhidManager<B: PadProto> {
    backend: B,
    slots: PadSlots<B::Pad>,
    state: Vec<B::State>,
    /// Last rumble forwarded per pad. All four levels: dedup on the handle pair alone would
    /// swallow a trigger-only change and the pad would never rumble.
    last_rumble: Vec<(u16, u16, u16, u16)>,
    /// Last rich feedback forwarded per pad, so a rumble-only report does not re-send LEDs.
    hidout_dedup: Vec<HidoutDedup>,
    last_write: Vec<Instant>,
    /// When the game last drove this pad's rumble plane. A non-zero `last_rumble` older than
    /// [`RUMBLE_IDLE_TIMEOUT`] against this is an abandoned residual — see [`pump`](Self::pump).
    last_active: Vec<Instant>,
    /// Last `RichInput::Motion` per pad. `None` before the first sample and after neutralize, so
    /// a pad with no motion feed costs nothing per tick.
    last_motion: Vec<Option<Instant>>,
    /// Next write of a [`PadProto::report_period`] pad, µs after `epoch`; `None` until its first
    /// heartbeat.
    next_due: Vec<Option<u64>>,
    epoch: Instant,
    overflow_warn: Vec<OverflowWarn>,
}

/// Per-poll ring-overflow WARN limiter. A >2 kHz writer against an 8-slot ring overflows every
/// ~4 ms poll; unlimited that is ~230 WARN/s. One line per [`Self::PERIOD`] with a `suppressed`
/// count keeps the signal.
#[derive(Default, Clone)]
struct OverflowWarn {
    last: Option<Instant>,
    suppressed: u32,
}

impl OverflowWarn {
    const PERIOD: Duration = Duration::from_secs(1);

    fn note(&mut self, now: Instant, backend: &'static str, index: usize) {
        if self
            .last
            .is_none_or(|t| now.duration_since(t) >= Self::PERIOD)
        {
            tracing::warn!(
                backend,
                index,
                suppressed = self.suppressed,
                "output-report ring overflow — resyncing feedback state (repeats coalesced, 1 \
                 line/s; `suppressed` = swallowed since the previous line)"
            );
            self.last = Some(now);
            self.suppressed = 0;
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
        }
    }
}

/// How long a latched non-zero rumble may sit without the game driving the RUMBLE plane before
/// it is forced off. DualSense/DS4/Deck motors are level-triggered — they run until an output
/// report zeros them — and the host resend loop renews a latched level every ~120 ms. Keyed on
/// [`PadFeedback::rumble_drove`], not general output, so an LED/trigger stream cannot keep an
/// abandoned rumble alive.
///
/// INVARIANT: stay above SDL's ~2 s resend (`SDL_RUMBLE_RESEND_MS`). SDL-class writers re-assert
/// a held level on that cadence because real firmware decays; that re-assert keeps a
/// legitimately-held rumble alive here. Shared with the uinput FF mixer via [`rumble_idle_timeout`].
///
/// KNOWN COST: `ff-memless` sends one report at start and one at stop, so a finite effect
/// longer than this window is cut in half. The uinput path can exempt that because evdev FF
/// hands it `replay.length`; nothing equivalent reaches this layer. Do not widen or disable
/// without evidence; `PUNKTFUNK_RUMBLE_IDLE_MS` exists for that experiment.
const RUMBLE_IDLE_TIMEOUT: Duration = Duration::from_millis(2500);

/// How long a pad's motion feed may go quiet before angular velocity is zeroed — see
/// [`PadState::neutralize_gyro`]. 100 ms ≈ 25 missed samples of a 250 Hz feed (client capture
/// floors ~4 ms); a still controller sends a zero sample, it does not stop sending.
const MOTION_IDLE_TIMEOUT: Duration = Duration::from_millis(100);

/// Abandoned-rumble force-off window. `PUNKTFUNK_RUMBLE_IDLE_MS` overrides
/// [`RUMBLE_IDLE_TIMEOUT`]; `0` disables. Non-zero values are floored at 2100 ms, just above
/// SDL's ~2 s resend, so the hatch cannot cut a legitimately-held rumble. Shared by
/// [`UhidManager`] and the Linux uinput FF mixer.
pub(crate) fn rumble_idle_timeout() -> Option<Duration> {
    static VAL: std::sync::OnceLock<Option<Duration>> = std::sync::OnceLock::new();
    *VAL.get_or_init(|| match std::env::var("PUNKTFUNK_RUMBLE_IDLE_MS") {
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(ms) => Some(Duration::from_millis(ms.max(2100))),
            Err(_) => Some(RUMBLE_IDLE_TIMEOUT),
        },
        Err(_) => Some(RUMBLE_IDLE_TIMEOUT),
    })
}

impl<B: PadProto + Default> UhidManager<B> {
    pub fn new() -> UhidManager<B> {
        UhidManager::with_backend(B::default())
    }
}

impl<B: PadProto + Default> Default for UhidManager<B> {
    fn default() -> UhidManager<B> {
        UhidManager::new()
    }
}

/// The per-tick half of [`UhidManager`] as an object, so a host keeps its managers of different
/// backends in one list that both pump and heartbeat walk.
pub trait UhidTick {
    fn pump(
        &mut self,
        rumble: &mut dyn FnMut(u16, u16, u16, u16, u16),
        hidout: &mut dyn FnMut(HidOutput),
    );
    fn heartbeat(&mut self, max_gap: Duration);
}

impl<B: PadProto> UhidTick for UhidManager<B> {
    fn pump(
        &mut self,
        rumble: &mut dyn FnMut(u16, u16, u16, u16, u16),
        hidout: &mut dyn FnMut(HidOutput),
    ) {
        UhidManager::pump(self, rumble, hidout);
    }

    fn heartbeat(&mut self, max_gap: Duration) {
        UhidManager::heartbeat(self, max_gap);
    }
}

impl<B: PadProto> UhidManager<B> {
    pub fn with_backend(backend: B) -> UhidManager<B> {
        let state = vec![B::State::neutral(); MAX_PADS];
        UhidManager {
            backend,
            slots: PadSlots::new(B::LABEL, B::DEVICE, B::CREATE_HINT),
            state,
            last_rumble: vec![(0, 0, 0, 0); MAX_PADS],
            hidout_dedup: vec![HidoutDedup::default(); MAX_PADS],
            last_write: vec![Instant::now(); MAX_PADS],
            last_active: vec![Instant::now(); MAX_PADS],
            last_motion: vec![None; MAX_PADS],
            next_due: vec![None; MAX_PADS],
            epoch: Instant::now(),
            overflow_warn: vec![OverflowWarn::default(); MAX_PADS],
        }
    }

    /// Show this session's pads to one seat alone
    /// ([`PadSlots::expose_in`](crate::pad_slots::PadSlots::expose_in)).
    pub fn expose_in(&mut self, dir: Option<std::path::PathBuf>) {
        self.slots.expose_in(dir);
    }

    /// Bring-up harnesses only ([`PadSlots::live`](crate::pad_slots::PadSlots::live)): a create
    /// failure leaves the slot empty and only logs, so a harness that still pushes frames
    /// drives nothing while another process may own that index. A session has no use for this
    /// — zero is a normal `active_mask` state.
    pub fn live_pads(&self) -> usize {
        self.slots.live()
    }

    pub fn handle(&mut self, ev: &GamepadEvent) {
        match ev {
            GamepadEvent::Arrival { index, kind, .. } => {
                tracing::info!(index, kind, "controller arrival ({})", B::LABEL);
                self.ensure(*index as usize);
            }
            GamepadEvent::State(f) => {
                let idx = f.index as usize;
                if idx >= MAX_PADS {
                    return;
                }
                // Unplug: arm grace for any pad whose mask bit cleared. The drop lands on a later
                // `pump` tick — this frame is the only one the producer sends.
                let sweep = self.slots.sweep(f.active_mask);
                self.reset_swept(sweep.dropped);
                self.clear_reclaimed_rich(sweep.reclaimed);
                if f.active_mask & (1 << idx) == 0 {
                    return; // this event WAS the unplug
                }
                self.ensure(idx);
                self.state[idx] = self.backend.merge_frame(&self.state[idx], f);
                self.write_unpaced(idx);
            }
        }
    }

    /// Never creates a pad; dropped if the pad is not present. `rich` is addressed in
    /// OS-slot space, like every other index here — the caller translates.
    pub fn apply_rich(&mut self, rich: RichInput) {
        let idx = rich.pad() as usize;
        if idx >= MAX_PADS || self.slots.get(idx).is_none() {
            return;
        }
        if matches!(rich, RichInput::Motion { .. }) {
            self.last_motion[idx] = Some(Instant::now());
        }
        self.backend.apply_rich(&mut self.state[idx], rich);
        self.write_unpaced(idx);
    }

    /// Re-emit each live pad's current report if silent for `max_gap` (or the backend forces a
    /// write). UHID/UMDF treat a multi-second input silence as unplug; a held stick produces no
    /// wire events. Re-sending is idempotent: a stale-but-correct frame, never a phantom input.
    /// A [`PadProto::report_period`] pad writes here only, on its deadlines.
    pub fn heartbeat(&mut self, max_gap: Duration) {
        let now = Instant::now();
        for i in 0..MAX_PADS {
            let Some(pad) = self.slots.get(i) else {
                continue;
            };
            let forced = self.backend.force_heartbeat(pad);
            // A stopped motion feed must not keep re-emitting its last angular velocity: heartbeat
            // re-sends the current report forever, and a real sensor clock grows dt each time —
            // the shape of phantom rotation. Zero once and stop watching until the feed returns.
            let mut neutralized = false;
            if self.last_motion[i].is_some_and(|t| now.duration_since(t) >= MOTION_IDLE_TIMEOUT) {
                self.last_motion[i] = None;
                neutralized = self.state[i].neutralize_gyro();
            }
            if let Some(period) = self.backend.report_period() {
                // The driver's schedule: deadlines, restarted rather than burst when a tick is late.
                let now_us = now.duration_since(self.epoch).as_micros() as u64;
                let due = *self.next_due[i].get_or_insert(now_us);
                let period_us = period.as_micros() as u64;
                if let Some(next) = pf_driver_proto::gamepad::serve_due(now_us, due, period_us) {
                    self.write(i);
                    self.next_due[i] = Some(next);
                }
            } else if neutralized || forced || now.duration_since(self.last_write[i]) >= max_gap {
                self.write(i);
            }
        }
    }

    /// Service every pad: answer pending driver/kernel handshakes and route game feedback.
    /// `rumble` is `(index, low, high, left_trigger, right_trigger)` only when the motor level
    /// changes (0xCA; trigger pair non-zero only on Windows HID Xbox). `hidout` is each 0xCD
    /// event that is not an exact repeat. Call frequently — init handshakes block until answered.
    pub fn pump(
        &mut self,
        mut rumble: impl FnMut(u16, u16, u16, u16, u16),
        mut hidout: impl FnMut(HidOutput),
    ) {
        let now = Instant::now();
        // Finish any unplug whose removal frame only armed the grace. The producer emits that
        // frame once, so without this a detached pad is never destroyed. Run before the loop so
        // a reaped index is gone for `get_mut` here and for `heartbeat`'s `get` later this tick.
        let swept = self.slots.reap();
        self.reset_swept(swept);
        for i in 0..MAX_PADS {
            let Some(pad) = self.slots.get_mut(i) else {
                continue;
            };
            let fb = self.backend.service(pad, i as u8);
            if fb.resync {
                // Output-report ring overflowed: feedback state is unknown beyond what the drain
                // salvaged. Silence the pad first (an unsaved plane must not stay latched; salvage
                // re-applies below) and re-arm hidout dedup so the next LED/trigger re-forwards.
                self.overflow_warn[i].note(now, B::LABEL, i);
                if self.last_rumble[i] != (0, 0, 0, 0) {
                    self.last_rumble[i] = (0, 0, 0, 0);
                    rumble(i as u16, 0, 0, 0, 0);
                }
                self.hidout_dedup[i] = HidoutDedup::default();
            }
            // Refresh activity when the game drove the rumble plane this poll (even at an
            // unchanged level). LED/trigger-only traffic does not. `None` = backend does not
            // track activity: treated as always-active, so the force-off below never fires.
            if fb.rumble_drove != Some(false) {
                self.last_active[i] = now;
            }
            if let Some(r) = fb.rumble {
                if self.last_rumble[i] != r {
                    self.last_rumble[i] = r;
                    rumble(i as u16, r.0, r.1, r.2, r.3);
                }
            } else if self.last_rumble[i] != (0, 0, 0, 0)
                && rumble_idle_timeout()
                    .is_some_and(|t| now.duration_since(self.last_active[i]) >= t)
            {
                // Latched non-zero rumble, game has not driven the rumble plane for the idle
                // window. Force off so the host resend loop stops forwarding it. See
                // RUMBLE_IDLE_TIMEOUT.
                tracing::info!(
                    backend = B::LABEL,
                    index = i,
                    prev_low = self.last_rumble[i].0,
                    prev_high = self.last_rumble[i].1,
                    prev_lt = self.last_rumble[i].2,
                    prev_rt = self.last_rumble[i].3,
                    "rumble: stale residual (game stopped driving the rumble plane) — forcing off"
                );
                self.last_rumble[i] = (0, 0, 0, 0);
                rumble(i as u16, 0, 0, 0, 0);
            }
            for h in fb.hidout {
                if self.hidout_dedup[i].should_forward(&h, now) {
                    hidout(h);
                }
            }
            // Re-assert latched rich state on a slow cadence. Deduping a datagram plane means a
            // dropped update is never re-derived: the game keeps sending the same value and
            // dedup eats every copy, leaving the pad on the previous trigger effect.
            for h in self.hidout_dedup[i].renewals(i as u8, now) {
                hidout(h);
            }
        }
    }

    /// Input-driven write. A paced pad's next deadline carries the new state instead.
    fn write_unpaced(&mut self, idx: usize) {
        if self.backend.report_period().is_none() {
            self.write(idx);
        }
    }

    /// Resets the heartbeat clock on every write so an actively-used pad emits no extra reports.
    fn write(&mut self, idx: usize) {
        let st = self.state[idx];
        if let Some(pad) = self.slots.get_mut(idx) {
            self.backend.write_state(pad, &st);
        }
        self.last_write[idx] = Instant::now();
    }

    /// Gate-checked create. A fresh pad starts from neutral state and re-armed dedups.
    fn ensure(&mut self, idx: usize) {
        let backend = &mut self.backend;
        if self.slots.ensure(idx, |i| backend.open(i)) {
            self.reset_pad(idx);
        }
    }

    /// Reset sibling state of every index a sweep or reap just dropped. Both unplug halves land
    /// here so a pump-tick teardown clears the same as a state-frame one — in particular
    /// `hidout_dedup`, which has no watchdog and would swallow an identical re-assert after replug.
    fn reset_swept(&mut self, swept: u16) {
        for i in 0..MAX_PADS {
            if swept & (1 << i) != 0 {
                self.reset_pad(i);
            }
        }
    }

    /// Clear rich-plane state of every slot reclaimed inside the grace window (see
    /// [`Sweep::reclaimed`](crate::pad_slots::Sweep::reclaimed)). Rumble and hidout dedup survive
    /// a removal; buttons/sticks arrive on the frame that re-set the mask bit.
    fn clear_reclaimed_rich(&mut self, reclaimed: u16) {
        for i in 0..MAX_PADS {
            if reclaimed & (1 << i) != 0 {
                self.state[i].clear_rich();
                self.last_motion[i] = None;
            }
        }
    }

    /// Reset one pad's sibling state (create and unplug) so the first frame/feedback after a
    /// (re)connect starts from scratch and is always forwarded.
    fn reset_pad(&mut self, idx: usize) {
        self.state[idx] = B::State::neutral();
        self.last_rumble[idx] = (0, 0, 0, 0);
        self.hidout_dedup[idx].clear();
        self.last_write[idx] = Instant::now();
        self.last_active[idx] = Instant::now();
        self.last_motion[idx] = None;
        self.next_due[idx] = None;
    }

    /// Backdate every pad's motion clock past [`MOTION_IDLE_TIMEOUT`] so the next
    /// [`heartbeat`](Self::heartbeat) can neutralize a stale gyro without a wall-clock sleep.
    /// Test-only; same hatch as `PadSlots::expire_grace`.
    #[cfg(test)]
    fn expire_motion(&mut self) {
        for t in self.last_motion.iter_mut().flatten() {
            *t -= MOTION_IDLE_TIMEOUT;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Scripted mock: `open` fails while `fail_opens > 0`; `service` replays canned feedback.
    #[derive(Default)]
    struct MockProto {
        fail_opens: RefCell<u32>,
        feedback: RefCell<Vec<PadFeedback>>,
        force_hb: bool,
    }

    #[derive(Clone, Copy, Default, PartialEq, Debug)]
    struct MockState {
        buttons: u32,
        /// Stands in for rich-plane fields (touch/motion): set by `apply_rich`, must survive
        /// `merge_frame`.
        rich_marker: u16,
        /// Stands in for angular velocity — zeroed by the idle-motion watchdog.
        gyro: i16,
        /// Stands in for acceleration, which the watchdog must not zero (gravity is persistent).
        accel: i16,
    }

    impl PadState for MockState {
        fn neutral() -> MockState {
            MockState::default()
        }
        fn neutralize_gyro(&mut self) -> bool {
            let changed = self.gyro != 0;
            self.gyro = 0;
            changed
        }
        fn clear_rich(&mut self) {
            self.rich_marker = 0;
            self.gyro = 0;
            self.accel = 0;
        }
    }

    #[derive(Default)]
    struct MockPad {
        writes: RefCell<Vec<MockState>>,
    }

    impl PadProto for MockProto {
        type Pad = MockPad;
        type State = MockState;
        const LABEL: &'static str = "Mock";
        const DEVICE: &'static str = "mock pad";
        const CREATE_HINT: &'static str = "";

        fn open(&mut self, _idx: u8) -> Result<MockPad> {
            let mut fails = self.fail_opens.borrow_mut();
            if *fails > 0 {
                *fails -= 1;
                anyhow::bail!("scripted open failure");
            }
            Ok(MockPad::default())
        }
        fn merge_frame(&self, prev: &MockState, f: &GamepadFrame) -> MockState {
            MockState {
                buttons: f.buttons,
                // Preserve-rich-fields contract: a stale motion sample lives forever without a
                // watchdog.
                rich_marker: prev.rich_marker,
                gyro: prev.gyro,
                accel: prev.accel,
            }
        }
        fn apply_rich(&self, st: &mut MockState, rich: RichInput) {
            match rich {
                RichInput::Touchpad { x, .. } => st.rich_marker = x,
                RichInput::Motion { gyro, accel, .. } => {
                    st.gyro = gyro[0];
                    st.accel = accel[2];
                }
                _ => {}
            }
        }
        fn write_state(&self, pad: &mut MockPad, st: &MockState) {
            pad.writes.borrow_mut().push(*st);
        }
        fn service(&self, _pad: &mut MockPad, _idx: u8) -> PadFeedback {
            let mut fb = self.feedback.borrow_mut();
            if fb.is_empty() {
                PadFeedback::default()
            } else {
                fb.remove(0)
            }
        }
        fn force_heartbeat(&self, _pad: &MockPad) -> bool {
            self.force_hb
        }
    }

    fn frame(idx: i16, mask: u16, buttons: u32) -> GamepadEvent {
        GamepadEvent::State(GamepadFrame {
            index: idx,
            active_mask: mask,
            buttons,
            ..Default::default()
        })
    }

    fn touch(pad: u8, x: u16) -> RichInput {
        RichInput::Touchpad {
            pad,
            finger: 0,
            active: true,
            x,
            y: 0,
        }
    }

    fn motion(pad: u8, gyro_x: i16, accel_z: i16) -> RichInput {
        RichInput::Motion {
            pad,
            gyro: [gyro_x, 0, 0],
            accel: [0, 0, accel_z],
        }
    }

    fn mgr() -> UhidManager<MockProto> {
        UhidManager::new()
    }

    /// [`MockProto`] with a fixed report period; an hour so only the first deadline is due.
    #[derive(Default)]
    struct PacedProto(MockProto);

    impl PadProto for PacedProto {
        type Pad = MockPad;
        type State = MockState;
        const LABEL: &'static str = "Paced";
        const DEVICE: &'static str = "paced pad";
        const CREATE_HINT: &'static str = "";

        fn open(&mut self, idx: u8) -> Result<MockPad> {
            self.0.open(idx)
        }
        fn merge_frame(&self, prev: &MockState, f: &GamepadFrame) -> MockState {
            self.0.merge_frame(prev, f)
        }
        fn apply_rich(&self, st: &mut MockState, rich: RichInput) {
            self.0.apply_rich(st, rich)
        }
        fn write_state(&self, pad: &mut MockPad, st: &MockState) {
            self.0.write_state(pad, st)
        }
        fn service(&self, pad: &mut MockPad, idx: u8) -> PadFeedback {
            self.0.service(pad, idx)
        }
        fn report_period(&self) -> Option<Duration> {
            Some(Duration::from_secs(3600))
        }
    }

    /// Input only moves a paced pad's state; the heartbeat writes it on a deadline, and a
    /// `max_gap` of zero does not bypass the period.
    #[test]
    fn a_paced_pad_writes_on_its_deadlines_only() {
        let mut m = UhidManager::<PacedProto>::new();
        m.handle(&frame(0, 0b1, 0x10));
        m.apply_rich(motion(0, 900, 10_000));
        let writes = |m: &UhidManager<PacedProto>| m.slots.get(0).unwrap().writes.borrow().clone();
        assert!(writes(&m).is_empty(), "input wrote around the pacing");

        m.heartbeat(Duration::ZERO);
        let w = writes(&m);
        assert_eq!(w.len(), 1, "the first heartbeat is the first deadline");
        assert_eq!((w[0].buttons, w[0].gyro), (0x10, 900));

        m.heartbeat(Duration::ZERO);
        assert_eq!(writes(&m).len(), 1, "a write landed before its deadline");
    }

    /// A motion feed that stops must not leave the pad rotating: past [`MOTION_IDLE_TIMEOUT`]
    /// the heartbeat zeroes angular velocity, keeps acceleration, and writes. `max_gap` is huge
    /// so the only write is the neutralize.
    #[test]
    fn a_stalled_motion_feed_has_its_gyro_neutralized() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0));
        m.apply_rich(motion(0, 900, 10_000));
        assert_eq!(m.state[0].gyro, 900);

        m.heartbeat(Duration::from_secs(3600));
        assert_eq!(m.state[0].gyro, 900, "neutralized inside the idle window");

        m.expire_motion();
        m.heartbeat(Duration::from_secs(3600));
        assert_eq!(
            m.state[0].gyro, 0,
            "stale angular velocity outlived the watchdog"
        );
        assert_eq!(
            m.state[0].accel, 10_000,
            "gravity must survive the neutralize"
        );
        let pad = m.slots.get(0).unwrap();
        let writes = pad.writes.borrow();
        assert_eq!(
            writes.last().unwrap().gyro,
            0,
            "the neutralized state never reached the pad"
        );
    }

    /// A pad already at rest must not manufacture a write on every tick.
    #[test]
    fn neutralizing_an_already_still_pad_writes_nothing() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0));
        m.apply_rich(motion(0, 0, 10_000));
        let before = m.slots.get(0).unwrap().writes.borrow().len();
        m.expire_motion();
        m.heartbeat(Duration::from_secs(3600));
        m.heartbeat(Duration::from_secs(3600));
        assert_eq!(m.slots.get(0).unwrap().writes.borrow().len(), before);
    }

    /// A controller that takes over a live pad inside replug grace skips create, so it must not
    /// inherit the previous finger or rotation — a pad with no gyro has no sample to correct it.
    #[test]
    fn a_grace_reclaim_clears_the_previous_pads_rich_state() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0));
        m.apply_rich(touch(0, 4242));
        m.apply_rich(motion(0, 900, 10_000));

        m.handle(&frame(0, 0b0, 0)); // the unplug frame: arms the grace, drops nothing
        assert!(m.slots.get(0).is_some(), "the grace must not drop it here");
        m.handle(&frame(0, 0b1, 0)); // back inside the grace — same pad, new owner

        assert_eq!(m.state[0].rich_marker, 0, "inherited the last pad's touch");
        assert_eq!(m.state[0].gyro, 0, "inherited the last pad's rotation");
    }

    /// Re-claim keys off the grace clock, not presence — a steady mask must never wipe touch
    /// and motion on every state frame.
    #[test]
    fn a_steady_mask_never_clears_rich_state() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0));
        m.apply_rich(touch(0, 4242));
        for _ in 0..5 {
            m.handle(&frame(0, 0b1, 0));
        }
        assert_eq!(m.state[0].rich_marker, 4242);
    }

    #[test]
    fn arrival_eager_creates_the_pad() {
        let mut m = mgr();
        m.handle(&GamepadEvent::Arrival {
            index: 2,
            kind: 1,
            capabilities: 0,
            audio_caps: 0,
        });
        assert!(m.slots.get(2).is_some());
    }

    #[test]
    fn button_frame_preserves_rich_fields_and_writes_merged_state() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0));
        m.apply_rich(touch(0, 777));
        m.handle(&frame(0, 0b1, 0xA));
        let pad = m.slots.get(0).unwrap();
        let writes = pad.writes.borrow();
        let last = writes.last().unwrap();
        assert_eq!(last.buttons, 0xA);
        assert_eq!(last.rich_marker, 777);
    }

    #[test]
    fn one_removal_frame_plus_a_pump_tick_completes_the_unplug() {
        // The producer emits the cleared-mask frame once (`native/input.rs` guards on the bit
        // still being set), so teardown has to finish on the periodic pump.
        let mut m = mgr();
        m.handle(&frame(1, 0b10, 0));
        assert!(m.slots.get(1).is_some());
        m.handle(&frame(1, 0b00, 0));
        assert!(m.slots.get(1).is_some(), "inside the grace — not yet swept");
        // A tick inside the grace must not flap the devnode (`pad_slots::SWEEP_GRACE`).
        m.pump(|_, _, _, _, _| {}, |_| {});
        assert!(
            m.slots.get(1).is_some(),
            "a tick inside the grace dropped it"
        );
        m.slots.expire_grace();
        m.pump(|_, _, _, _, _| {}, |_| {});
        assert!(
            m.slots.get(1).is_none(),
            "the pump tick never completed the unplug"
        );
        // A further cleared-mask frame must not resurrect it (the arm branch early-returns).
        m.handle(&frame(1, 0b00, 0));
        assert!(
            m.slots.get(1).is_none(),
            "a cleared-mask frame recreated the pad"
        );
    }

    #[test]
    fn rich_event_for_an_absent_pad_is_dropped_and_never_creates() {
        let mut m = mgr();
        m.apply_rich(touch(3, 42));
        assert!(m.slots.get(3).is_none());
        m.handle(&frame(3, 0b1000, 0));
        assert_eq!(m.state[3].rich_marker, 0);
    }

    #[test]
    fn create_failure_backs_off_then_state_still_tracks() {
        let mut m = mgr();
        *m.backend.fail_opens.borrow_mut() = 1;
        m.handle(&frame(0, 0b1, 0x1));
        assert!(m.slots.get(0).is_none());
        assert_eq!(m.state[0].buttons, 0x1);
        m.handle(&frame(0, 0b1, 0x3));
        assert!(m.slots.get(0).is_none());
        assert_eq!(m.state[0].buttons, 0x3);
    }

    #[test]
    fn rumble_dedup_forwards_changes_only_and_rearms_on_recreate() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0));
        let collect = |m: &mut UhidManager<MockProto>| {
            let out = RefCell::new(Vec::new());
            m.pump(
                |i, lo, hi, lt, rt| out.borrow_mut().push((i, lo, hi, lt, rt)),
                |_| {},
            );
            out.into_inner()
        };
        let rumble = |r| PadFeedback {
            rumble: Some(r),
            hidout: Vec::new(),
            rumble_drove: Some(true),
            resync: false,
        };
        *m.backend.feedback.borrow_mut() = vec![
            rumble((100, 0, 0, 0)),
            rumble((100, 0, 0, 0)),
            rumble((7, 7, 0, 0)),
        ];
        assert_eq!(collect(&mut m), vec![(0, 100, 0, 0, 0)]);
        assert_eq!(collect(&mut m), vec![]);
        assert_eq!(collect(&mut m), vec![(0, 7, 7, 0, 0)]);
        // Unplug + recreate re-arms the dedup. Unplug completes on a pump tick, not a second
        // frame — that is all production ever sends.
        m.handle(&frame(0, 0b0, 0)); // the one removal frame — arms the grace
        m.slots.expire_grace();
        assert_eq!(collect(&mut m), vec![]); // this tick reaps; nothing queued to forward
        assert!(
            m.slots.get(0).is_none(),
            "the pump tick completed the unplug"
        );
        m.handle(&frame(0, 0b1, 0));
        *m.backend.feedback.borrow_mut() = vec![rumble((7, 7, 0, 0))];
        assert_eq!(collect(&mut m), vec![(0, 7, 7, 0, 0)]);
    }

    /// Dedup compares all four levels. Handle-pair-only would swallow a trigger-only change —
    /// the normal shape of impulse-trigger content — and the pad would never rumble.
    #[test]
    fn a_trigger_only_change_is_forwarded_not_deduped_away() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0));
        let collect = |m: &mut UhidManager<MockProto>| {
            let out = RefCell::new(Vec::new());
            m.pump(
                |i, lo, hi, lt, rt| out.borrow_mut().push((i, lo, hi, lt, rt)),
                |_| {},
            );
            out.into_inner()
        };
        let rumble = |r| PadFeedback {
            rumble: Some(r),
            hidout: Vec::new(),
            rumble_drove: Some(true),
            resync: false,
        };
        *m.backend.feedback.borrow_mut() = vec![
            rumble((0, 0, 0x8000, 0)),
            rumble((0, 0, 0x8000, 0)),
            rumble((0, 0, 0x8000, 0x4000)),
            rumble((0, 0, 0, 0)),
        ];
        assert_eq!(collect(&mut m), vec![(0, 0, 0, 0x8000, 0)]);
        assert_eq!(collect(&mut m), vec![], "exact repeat still dedups");
        assert_eq!(collect(&mut m), vec![(0, 0, 0, 0x8000, 0x4000)]);
        assert_eq!(collect(&mut m), vec![(0, 0, 0, 0, 0)], "the stop forwards");
    }

    #[test]
    fn abandoned_rumble_is_forced_off_after_idle_timeout() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0));
        let collect = |m: &mut UhidManager<MockProto>| {
            let out = RefCell::new(Vec::new());
            m.pump(
                |i, lo, hi, lt, rt| out.borrow_mut().push((i, lo, hi, lt, rt)),
                |_| {},
            );
            out.into_inner()
        };
        *m.backend.feedback.borrow_mut() = vec![PadFeedback {
            rumble: Some((200, 0, 0, 0)),
            hidout: Vec::new(),
            rumble_drove: Some(true),
            resync: false,
        }];
        assert_eq!(collect(&mut m), vec![(0, 200, 0, 0, 0)]);

        // Game stops driving the rumble plane (no report, or LED/trigger-only) and never sent a
        // stop. Before the idle window elapses the latched level stays asserting.
        let idle = || PadFeedback {
            rumble: None,
            hidout: Vec::new(),
            rumble_drove: Some(false),
            resync: false,
        };
        *m.backend.feedback.borrow_mut() = vec![idle()];
        assert_eq!(collect(&mut m), vec![]);

        // Past the timeout: residual is forced off once, then stays off (no repeated zero).
        m.last_active[0] = Instant::now() - (RUMBLE_IDLE_TIMEOUT + Duration::from_millis(50));
        *m.backend.feedback.borrow_mut() = vec![idle(), idle()];
        assert_eq!(collect(&mut m), vec![(0, 0, 0, 0, 0)]);
        assert_eq!(collect(&mut m), vec![]); // already zero — no repeat
    }

    #[test]
    fn asserted_rumble_survives_idle_timeout_while_game_drives() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0));
        let collect = |m: &mut UhidManager<MockProto>| {
            let out = RefCell::new(Vec::new());
            m.pump(
                |i, lo, hi, lt, rt| out.borrow_mut().push((i, lo, hi, lt, rt)),
                |_| {},
            );
            out.into_inner()
        };
        *m.backend.feedback.borrow_mut() = vec![PadFeedback {
            rumble: Some((200, 0, 0, 0)),
            hidout: Vec::new(),
            rumble_drove: Some(true),
            resync: false,
        }];
        assert_eq!(collect(&mut m), vec![(0, 200, 0, 0, 0)]);

        // A poll where the game drove the rumble plane refreshes activity, so a held rumble is
        // not cut even with a stale clock. Unchanged level dedups; `rumble_drove: Some(true)`
        // with no rumble is also honored.
        m.last_active[0] = Instant::now() - (RUMBLE_IDLE_TIMEOUT + Duration::from_millis(50));
        *m.backend.feedback.borrow_mut() = vec![PadFeedback {
            rumble: Some((200, 0, 0, 0)),
            hidout: Vec::new(),
            rumble_drove: Some(true),
            resync: false,
        }];
        assert_eq!(collect(&mut m), vec![]); // unchanged level dedups, clock refreshed
        m.last_active[0] = Instant::now() - (RUMBLE_IDLE_TIMEOUT + Duration::from_millis(50));
        *m.backend.feedback.borrow_mut() = vec![PadFeedback {
            rumble: None,
            hidout: Vec::new(),
            rumble_drove: Some(true),
            resync: false,
        }];
        assert_eq!(collect(&mut m), vec![]);
    }

    #[test]
    fn hidout_dedup_drops_exact_repeats() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0));
        let led = |r| HidOutput::Led {
            pad: 0,
            r,
            g: 0,
            b: 0,
        };
        *m.backend.feedback.borrow_mut() = vec![PadFeedback {
            rumble: None,
            hidout: vec![led(10), led(10), led(20)],
            rumble_drove: Some(true),
            resync: false,
        }];
        let out = RefCell::new(0u32);
        m.pump(
            |_, _, _, _, _| {},
            |_| {
                *out.borrow_mut() += 1;
            },
        );
        assert_eq!(out.into_inner(), 2); // 10 forwarded once, 20 forwarded; the repeat dropped
    }

    #[test]
    fn heartbeat_reemits_silent_pads_and_honors_force() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0x5));
        let writes = |m: &UhidManager<MockProto>| m.slots.get(0).unwrap().writes.borrow().len();
        let after_frame = writes(&m);
        // A pad written just now is not re-emitted under a huge gap.
        m.heartbeat(Duration::from_secs(3600));
        assert_eq!(writes(&m), after_frame);
        // A zero gap counts it as silent and re-emits the current state.
        m.heartbeat(Duration::ZERO);
        assert_eq!(writes(&m), after_frame + 1);
        assert_eq!(
            m.slots
                .get(0)
                .unwrap()
                .writes
                .borrow()
                .last()
                .unwrap()
                .buttons,
            0x5
        );
        // Backend force flag overrides the gap (Steam mode-entry pulse).
        m.backend.force_hb = true;
        m.heartbeat(Duration::from_secs(3600));
        assert_eq!(writes(&m), after_frame + 2);
    }
    /// Overflow WARN limiter: a storm overflowing every ~4 ms poll must emit one line per
    /// [`OverflowWarn::PERIOD`] and carry the swallowed count.
    #[test]
    fn overflow_warn_coalesces_within_the_period() {
        let mut w = OverflowWarn::default();
        let t0 = Instant::now();
        w.note(t0, "Mock", 0);
        assert_eq!((w.last, w.suppressed), (Some(t0), 0));
        for _ in 0..250 {
            w.note(t0 + Duration::from_millis(4), "Mock", 0);
        }
        assert_eq!((w.last, w.suppressed), (Some(t0), 250), "storm coalesced");
        let t1 = t0 + OverflowWarn::PERIOD;
        w.note(t1, "Mock", 0);
        assert_eq!((w.last, w.suppressed), (Some(t1), 0));
    }

    /// A ring-overflow resync must silence a latched rumble once and re-arm hidout dedup, so the
    /// next asserted state re-forwards even when it equals the pre-overflow state.
    #[test]
    fn resync_forces_stop_and_rearms_dedup() {
        let mut m = mgr();
        m.handle(&frame(0, 0b1, 0));
        let led = |r| HidOutput::Led {
            pad: 0,
            r,
            g: 0,
            b: 0,
        };
        let collect = |m: &mut UhidManager<MockProto>| {
            let rumbles = RefCell::new(Vec::new());
            let hidouts = RefCell::new(0u32);
            m.pump(
                |i, lo, hi, lt, rt| rumbles.borrow_mut().push((i, lo, hi, lt, rt)),
                |_| *hidouts.borrow_mut() += 1,
            );
            (rumbles.into_inner(), hidouts.into_inner())
        };

        *m.backend.feedback.borrow_mut() = vec![PadFeedback {
            rumble: Some((100, 0, 0, 0)),
            hidout: vec![led(10)],
            rumble_drove: Some(true),
            resync: false,
        }];
        assert_eq!(collect(&mut m), (vec![(0, 100, 0, 0, 0)], 1));

        // Overflow, no reports survived: forced stop, exactly once.
        *m.backend.feedback.borrow_mut() = vec![PadFeedback {
            rumble: None,
            hidout: Vec::new(),
            rumble_drove: Some(false),
            resync: true,
        }];
        assert_eq!(collect(&mut m), (vec![(0, 0, 0, 0, 0)], 0));

        // Same rumble + LED must re-forward: forced stop reset `last_rumble`, dedup was re-armed.
        *m.backend.feedback.borrow_mut() = vec![PadFeedback {
            rumble: Some((100, 0, 0, 0)),
            hidout: vec![led(10)],
            rumble_drove: Some(true),
            resync: false,
        }];
        assert_eq!(collect(&mut m), (vec![(0, 100, 0, 0, 0)], 1));

        // Resync with nothing latched forwards no spurious stop.
        *m.backend.feedback.borrow_mut() = vec![
            PadFeedback {
                rumble: Some((0, 0, 0, 0)),
                hidout: Vec::new(),
                rumble_drove: Some(true),
                resync: false,
            },
            PadFeedback {
                rumble: None,
                hidout: Vec::new(),
                rumble_drove: Some(false),
                resync: true,
            },
        ];
        assert_eq!(collect(&mut m), (vec![(0, 0, 0, 0, 0)], 0)); // the explicit stop
        assert_eq!(collect(&mut m), (vec![], 0)); // resync at zero — silent
    }
}
