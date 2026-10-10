//! Controller mouse: pads the embedder flags drive the host's pointer
//! (`design/controller-mouse-mode.md`). [`PadMouseMode::Touchpad`] keeps a pad in the game and
//! moves the pointer with its touchpads ([`super::pad_touch`]); [`PadMouseMode::Full`] turns the
//! whole pad into a mouse and a few keys.
//!
//! The translator is pure — pad state in, `InputEvent`s out — so the input task owns every send.
//! Buttons are levels, not edges: each fold recomputes the outputs a pad wants held and emits only
//! the difference, so two sources on one output (A and RT on the left button) cannot double-press.
//! Buttons already held when a pad enters stay ignored until they release, so the B that closed
//! the dial never lands as Escape. Stick speed and touchpad travel scale with the stream height:
//! the same hand-feel on a 1080p and a 4K desktop.

use super::pad_touch::{Contact, Touchpads};
use crate::input::gamepad::*;
use crate::input::scroll::{ScrollEvent, ScrollPhase, ScrollSource, SCROLL_SCALE};
use crate::input::{GamepadSnapshot, InputEvent, InputKind, PadMouseMode, MAX_PADS};
use crate::quic::{GRANT_KEYBOARD, GRANT_POINTER};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

/// Stick travel below this fraction does nothing (the 7000/32767 a daily user tuned).
const DEADZONE: f64 = 0.2;
/// Full deflection: 1.25 stream heights per second ≈ 1350 px/s at 1080p.
const POINTER_HEIGHTS_PER_S: f64 = 1.25;
const SCROLL_HEIGHTS_PER_S: f64 = 0.3;
const TRIGGER_ON: u8 = 96;
const TRIGGER_OFF: u8 = 64;
/// Pointer cadence while a stick is deflected. A resting pad sends nothing.
pub(crate) const TICK: Duration = Duration::from_millis(4);
/// A stalled task must not fling the pointer across the desktop on its next tick.
const MAX_DT: f64 = 0.05;
const FALLBACK_HEIGHT: u32 = 1080;

/// Triggers as virtual button bits, above every wire `BTN_*`.
const TRIGGER_L: u32 = 1 << 30;
const TRIGGER_R: u32 = 1 << 31;

pub(super) const MOUSE_LEFT: u32 = 1;
const MOUSE_MIDDLE: u32 = 2;
pub(super) const MOUSE_RIGHT: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Out {
    Mouse(u32),
    Key(u8),
}

/// Source bits → output, one entry per output bit. Back and paddles are left to the client; the
/// touchpad click belongs to [`super::pad_touch`].
const MAP: [(u32, Out); 14] = [
    (BTN_A | TRIGGER_R, Out::Mouse(MOUSE_LEFT)),
    (BTN_X | TRIGGER_L, Out::Mouse(MOUSE_RIGHT)),
    (BTN_Y, Out::Mouse(MOUSE_MIDDLE)),
    (BTN_B, Out::Key(0x1B)),
    (BTN_DPAD_UP, Out::Key(0x26)),
    (BTN_DPAD_DOWN, Out::Key(0x28)),
    (BTN_DPAD_LEFT, Out::Key(0x25)),
    (BTN_DPAD_RIGHT, Out::Key(0x27)),
    (BTN_START, Out::Key(0x0D)),
    (BTN_LB, Out::Key(0x11)),
    (BTN_RB, Out::Key(0x12)),
    (BTN_LS_CLICK, Out::Key(0x10)),
    (BTN_RS_CLICK, Out::Key(0x20)),
    (BTN_GUIDE, Out::Key(0x5B)),
];

/// The embedder's request, shared between `NativeClient` and the input task. A pad sits in at
/// most one mask.
#[derive(Default)]
pub(crate) struct PadMouseShared {
    /// Pads in [`PadMouseMode::Full`].
    requested: AtomicU16,
    /// Pads in [`PadMouseMode::Touchpad`].
    touchpad: AtomicU16,
    /// Pads the host holds: declared or driven, not yet removed. Written by the input task.
    live: AtomicU16,
    pub(crate) changed: tokio::sync::Notify,
}

impl PadMouseShared {
    pub(crate) fn live(&self) -> u16 {
        self.live.load(Ordering::Relaxed)
    }

    pub(crate) fn set_live(&self, mask: u16) {
        self.live.store(mask, Ordering::Relaxed);
    }

    /// Exactly the pads in `mask` become full mice; the others leave full mode.
    pub(crate) fn request(&self, mask: u16) {
        self.requested.store(mask, Ordering::Relaxed);
        self.touchpad.fetch_and(!mask, Ordering::Relaxed);
        self.changed.notify_one();
    }

    /// Put every pad in `target` in `mode`; the other pads keep theirs.
    pub(crate) fn set_mode(&self, target: u16, mode: PadMouseMode) {
        let (full, touchpad) = match mode {
            PadMouseMode::Off => (0, 0),
            PadMouseMode::Touchpad => (0, target),
            PadMouseMode::Full => (target, 0),
        };
        self.requested.fetch_and(!target, Ordering::Relaxed);
        self.requested.fetch_or(full, Ordering::Relaxed);
        self.touchpad.fetch_and(!target, Ordering::Relaxed);
        self.touchpad.fetch_or(touchpad, Ordering::Relaxed);
        self.changed.notify_one();
    }

    /// The mode every pad in `target` shares under `grants`; a mixed set reads as off.
    pub(crate) fn mode(&self, target: u16, grants: u32) -> PadMouseMode {
        if target == 0 {
            PadMouseMode::Off
        } else if self.active(grants) & target == target {
            PadMouseMode::Full
        } else if self.touchpad_active(grants) & target == target {
            PadMouseMode::Touchpad
        } else {
            PadMouseMode::Off
        }
    }

    pub(crate) fn requested(&self) -> u16 {
        self.requested.load(Ordering::Relaxed)
    }

    /// Full-mouse pads under `grants`: none without the pointer grant.
    pub(crate) fn active(&self, grants: u32) -> u16 {
        if grants & GRANT_POINTER != 0 {
            self.requested()
        } else {
            0
        }
    }

    /// Touchpad-mouse pads under `grants`: none without the pointer grant.
    pub(crate) fn touchpad_active(&self, grants: u32) -> u16 {
        if grants & GRANT_POINTER != 0 {
            self.touchpad.load(Ordering::Relaxed)
        } else {
            0
        }
    }

    pub(crate) fn clear(&self, pad: usize) {
        self.requested.fetch_and(!(1 << pad), Ordering::Relaxed);
        self.touchpad.fetch_and(!(1 << pad), Ordering::Relaxed);
    }

    pub(crate) fn clear_all(&self) {
        self.requested.store(0, Ordering::Relaxed);
        self.touchpad.store(0, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Default)]
struct Pad {
    mode: PadMouseMode,
    snap: GamepadSnapshot,
    /// Sources held at enter, dropped bit by bit as they release.
    ignore: u32,
    lt: bool,
    rt: bool,
    /// `MAP` entries down on the wire.
    out: u16,
    /// Sub-unit carry: pointer x, y; scroll x, y.
    rem: [f64; 4],
    /// Scroll axes mid-gesture; a neutral stick owes an End.
    scroll_active: [bool; 2],
    touchpads: Touchpads,
}

impl Pad {
    fn sources(&mut self) -> u32 {
        self.lt = hysteresis(self.lt, self.snap.left_trigger);
        self.rt = hysteresis(self.rt, self.snap.right_trigger);
        self.snap.buttons
            | if self.lt { TRIGGER_L } else { 0 }
            | if self.rt { TRIGGER_R } else { 0 }
    }
}

fn hysteresis(on: bool, v: u8) -> bool {
    if on {
        v >= TRIGGER_OFF
    } else {
        v >= TRIGGER_ON
    }
}

/// Stick → unit vector scaled by the squared travel past the deadzone. `+y` stays up.
fn curve(x: i16, y: i16) -> (f64, f64) {
    let (fx, fy) = (f64::from(x) / 32767.0, f64::from(y) / 32767.0);
    let mag = fx.hypot(fy);
    if mag <= DEADZONE {
        return (0.0, 0.0);
    }
    let t = ((mag - DEADZONE) / (1.0 - DEADZONE)).min(1.0);
    let s = t * t / mag;
    (fx * s, fy * s)
}

pub(super) fn event(kind: InputKind, code: u32, x: i32, y: i32, flags: u32) -> InputEvent {
    InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y,
        flags,
    }
}

pub(super) fn scroll_event(
    source: ScrollSource,
    axis: u32,
    delta: i32,
    phase: ScrollPhase,
) -> InputEvent {
    ScrollEvent {
        source,
        phase,
        axis,
        delta,
    }
    .to_event()
}

fn press(out: Out, down: bool) -> InputEvent {
    match (out, down) {
        (Out::Mouse(b), true) => event(InputKind::MouseButtonDown, b, 0, 0, 0),
        (Out::Mouse(b), false) => event(InputKind::MouseButtonUp, b, 0, 0, 0),
        (Out::Key(vk), true) => event(InputKind::KeyDown, u32::from(vk), 0, 0, 0),
        (Out::Key(vk), false) => event(InputKind::KeyUp, u32::from(vk), 0, 0, 0),
    }
}

fn granted(out: Out, grants: u32) -> bool {
    match out {
        Out::Mouse(_) => grants & GRANT_POINTER != 0,
        Out::Key(_) => grants & GRANT_KEYBOARD != 0,
    }
}

/// Whole units out of `*rem + v`, the fraction carried.
pub(super) fn take(rem: &mut f64, v: f64) -> i32 {
    *rem += v;
    let whole = rem.trunc();
    *rem -= whole;
    whole as i32
}

pub(super) fn stream_height(height: u32) -> f64 {
    f64::from(if height == 0 { FALLBACK_HEIGHT } else { height })
}

#[derive(Default)]
pub(crate) struct PadMouse {
    pads: [Pad; MAX_PADS],
}

impl PadMouse {
    pub(crate) fn mode(&self, pad: usize) -> PadMouseMode {
        self.pads.get(pad).map_or(PadMouseMode::Off, |p| p.mode)
    }

    /// The whole pad is a mouse: its buttons and sticks fold here instead of the host snapshot.
    pub(crate) fn is_on(&self, pad: usize) -> bool {
        self.mode(pad) == PadMouseMode::Full
    }

    /// Start translating `pad` in `mode` from its current state. Held buttons stay ignored until
    /// released.
    pub(crate) fn enter(&mut self, pad: usize, mode: PadMouseMode, snap: GamepadSnapshot) {
        let mut p = Pad {
            mode,
            snap,
            ..Pad::default()
        };
        p.ignore = p.sources();
        self.pads[pad] = p;
    }

    /// Release every output `pad` holds and stop translating it.
    pub(crate) fn leave(&mut self, pad: usize) -> Vec<InputEvent> {
        let mut p = std::mem::take(&mut self.pads[pad]);
        let mut evs: Vec<_> = (0..2u32)
            .filter(|&axis| p.scroll_active[axis as usize])
            .map(|axis| scroll_event(ScrollSource::Controller, axis, 0, ScrollPhase::Cancel))
            .collect();
        evs.extend(
            (0..MAP.len())
                .filter(|i| p.out & 1 << i != 0)
                .map(|i| press(MAP[i].1, false)),
        );
        evs.extend(p.touchpads.release());
        evs
    }

    /// Fold one button/axis event of a full-mouse pad and emit the output edges it causes.
    pub(crate) fn fold(&mut self, pad: usize, ev: &InputEvent, grants: u32) -> Vec<InputEvent> {
        let p = &mut self.pads[pad];
        if p.mode != PadMouseMode::Full || !p.snap.fold(ev) {
            return Vec::new();
        }
        let sources = p.sources();
        p.ignore &= sources;
        let live = sources & !p.ignore;
        let mut want = 0u16;
        for (i, &(src, out)) in MAP.iter().enumerate() {
            if live & src != 0 && granted(out, grants) {
                want |= 1 << i;
            }
        }
        let changed = want ^ p.out;
        p.out = want;
        let ups = (0..MAP.len()).filter(|i| changed & 1 << i != 0 && want & 1 << i == 0);
        let downs = (0..MAP.len()).filter(|i| want & changed & 1 << i != 0);
        ups.map(|i| press(MAP[i].1, false))
            .chain(downs.map(|i| press(MAP[i].1, true)))
            .collect()
    }

    /// A touchpad contact of a pad in either mode.
    pub(crate) fn contact(&mut self, c: Contact, height: u32, grants: u32) -> Vec<InputEvent> {
        match self.pads.get_mut(usize::from(c.pad)) {
            Some(p) if p.mode != PadMouseMode::Off => p.touchpads.contact(c, height, grants),
            _ => Vec::new(),
        }
    }

    /// `BTN_TOUCHPAD` of a pad in either mode: the click never reaches the host pad.
    pub(crate) fn touchpad_click(
        &mut self,
        pad: usize,
        down: bool,
        grants: u32,
    ) -> Vec<InputEvent> {
        match self.pads.get_mut(pad) {
            Some(p) if p.mode != PadMouseMode::Off => {
                p.touchpads.button_click(down, grants).into_iter().collect()
            }
            _ => Vec::new(),
        }
    }

    /// True while a full-mouse pad has stick motion or an open scroll gesture.
    pub(crate) fn moving(&self) -> bool {
        self.pads.iter().any(|p| {
            p.mode == PadMouseMode::Full
                && (curve(p.snap.ls_x, p.snap.ls_y) != (0.0, 0.0)
                    || curve(p.snap.rs_x, p.snap.rs_y) != (0.0, 0.0)
                    || p.scroll_active.iter().any(|&active| active))
        })
    }

    /// Pointer motion and normalized scroll for `dt_s` of stick deflection.
    pub(crate) fn tick(&mut self, dt_s: f64, height: u32, grants: u32) -> Vec<InputEvent> {
        let mut evs = Vec::new();
        if grants & GRANT_POINTER == 0 {
            return evs;
        }
        let dt = dt_s.clamp(0.0, MAX_DT);
        let px = POINTER_HEIGHTS_PER_S * stream_height(height) * dt;
        let units = SCROLL_HEIGHTS_PER_S * f64::from(FALLBACK_HEIGHT) * SCROLL_SCALE * dt;
        for p in self
            .pads
            .iter_mut()
            .filter(|p| p.mode == PadMouseMode::Full)
        {
            let (cx, cy) = curve(p.snap.ls_x, p.snap.ls_y);
            let dx = take(&mut p.rem[0], cx * px);
            let dy = take(&mut p.rem[1], -cy * px);
            if dx != 0 || dy != 0 {
                evs.push(event(InputKind::MouseMove, 0, dx, dy, 0));
            }
            let (sx, sy) = curve(p.snap.rs_x, p.snap.rs_y);
            for (axis, velocity, remainder) in [(0usize, sy, 3usize), (1, sx, 2)] {
                if velocity == 0.0 {
                    p.rem[remainder] = 0.0;
                    if std::mem::take(&mut p.scroll_active[axis]) {
                        evs.push(scroll_event(
                            ScrollSource::Controller,
                            axis as u32,
                            0,
                            ScrollPhase::End,
                        ));
                    }
                    continue;
                }
                let delta = take(&mut p.rem[remainder], velocity * units);
                if delta != 0 {
                    let phase = if std::mem::replace(&mut p.scroll_active[axis], true) {
                        ScrollPhase::Update
                    } else {
                        ScrollPhase::Begin
                    };
                    evs.push(scroll_event(
                        ScrollSource::Controller,
                        axis as u32,
                        delta,
                        phase,
                    ));
                }
            }
        }
        evs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::GRANT_ALL;

    fn button(bit: u32, down: bool) -> InputEvent {
        event(InputKind::GamepadButton, bit, down as i32, 0, 0)
    }

    fn axis(code: u32, v: i32) -> InputEvent {
        event(InputKind::GamepadAxis, code, v, 0, 0)
    }

    fn kinds(evs: &[InputEvent]) -> Vec<(InputKind, u32)> {
        evs.iter().map(|e| (e.kind, e.code)).collect()
    }

    fn entered() -> PadMouse {
        let mut m = PadMouse::default();
        m.enter(0, PadMouseMode::Full, GamepadSnapshot::default());
        m
    }

    #[test]
    fn a_and_right_trigger_share_the_left_button() {
        let mut m = entered();
        assert_eq!(
            kinds(&m.fold(0, &button(BTN_A, true), GRANT_ALL)),
            [(InputKind::MouseButtonDown, 1)]
        );
        assert!(
            m.fold(0, &axis(AXIS_RT, 255), GRANT_ALL).is_empty(),
            "already down"
        );
        assert!(
            m.fold(0, &button(BTN_A, false), GRANT_ALL).is_empty(),
            "RT still holds it"
        );
        assert_eq!(
            kinds(&m.fold(0, &axis(AXIS_RT, 0), GRANT_ALL)),
            [(InputKind::MouseButtonUp, 1)]
        );
    }

    #[test]
    fn layout_rows() {
        let rows = [
            (BTN_X, InputKind::MouseButtonDown, MOUSE_RIGHT),
            (BTN_Y, InputKind::MouseButtonDown, MOUSE_MIDDLE),
            (BTN_B, InputKind::KeyDown, 0x1B),
            (BTN_START, InputKind::KeyDown, 0x0D),
            (BTN_DPAD_LEFT, InputKind::KeyDown, 0x25),
            (BTN_LB, InputKind::KeyDown, 0x11),
            (BTN_GUIDE, InputKind::KeyDown, 0x5B),
        ];
        for (bit, kind, code) in rows {
            let mut m = entered();
            assert_eq!(
                kinds(&m.fold(0, &button(bit, true), GRANT_ALL)),
                [(kind, code)],
                "{bit:#x}"
            );
        }
        let mut m = entered();
        assert!(
            m.fold(0, &button(BTN_BACK, true), GRANT_ALL).is_empty(),
            "Back belongs to the dial"
        );
    }

    #[test]
    fn trigger_hysteresis_holds_between_thresholds() {
        let mut m = entered();
        assert!(
            m.fold(0, &axis(AXIS_LT, 80), GRANT_ALL).is_empty(),
            "below on"
        );
        assert_eq!(m.fold(0, &axis(AXIS_LT, 100), GRANT_ALL).len(), 1);
        assert!(
            m.fold(0, &axis(AXIS_LT, 70), GRANT_ALL).is_empty(),
            "above off"
        );
        assert_eq!(
            kinds(&m.fold(0, &axis(AXIS_LT, 10), GRANT_ALL)),
            [(InputKind::MouseButtonUp, 3)]
        );
    }

    #[test]
    fn a_button_held_at_enter_fires_only_after_a_fresh_press() {
        let mut m = PadMouse::default();
        m.enter(
            0,
            PadMouseMode::Full,
            GamepadSnapshot {
                buttons: BTN_B,
                ..Default::default()
            },
        );
        assert!(m.fold(0, &axis(AXIS_LS_X, 0), GRANT_ALL).is_empty());
        assert!(
            m.fold(0, &button(BTN_B, false), GRANT_ALL).is_empty(),
            "no Escape up for a press never sent"
        );
        assert_eq!(
            kinds(&m.fold(0, &button(BTN_B, true), GRANT_ALL)),
            [(InputKind::KeyDown, 0x1B)]
        );
    }

    #[test]
    fn leave_releases_everything_held() {
        let mut m = entered();
        m.fold(0, &button(BTN_A, true), GRANT_ALL);
        m.fold(0, &button(BTN_LB, true), GRANT_ALL);
        let mut released = kinds(&m.leave(0));
        released.sort_by_key(|&(_, c)| c);
        assert_eq!(
            released,
            [(InputKind::MouseButtonUp, 1), (InputKind::KeyUp, 0x11)]
        );
        assert!(!m.is_on(0));
        assert!(
            m.fold(0, &button(BTN_A, false), GRANT_ALL).is_empty(),
            "off pads translate nothing"
        );
    }

    #[test]
    fn keys_need_the_keyboard_grant() {
        let mut m = entered();
        assert!(m.fold(0, &button(BTN_B, true), GRANT_POINTER).is_empty());
        assert_eq!(m.fold(0, &button(BTN_A, true), GRANT_POINTER).len(), 1);
        m.fold(0, &axis(AXIS_LS_X, 32767), GRANT_ALL);
        assert!(
            m.tick(0.01, 1080, GRANT_KEYBOARD).is_empty(),
            "no pointer grant, no motion"
        );
    }

    #[test]
    fn full_deflection_moves_one_and_a_quarter_heights_per_second() {
        let mut m = entered();
        m.fold(0, &axis(AXIS_LS_X, 32767), GRANT_ALL);
        m.fold(0, &axis(AXIS_LS_Y, 0), GRANT_ALL);
        let mut dx = 0;
        for _ in 0..250 {
            for ev in m.tick(0.004, 1080, GRANT_ALL) {
                assert_eq!(ev.kind, InputKind::MouseMove);
                dx += ev.x;
            }
        }
        assert!((1349..=1350).contains(&dx), "{dx} px in 1 s");
        let mut m4k = entered();
        m4k.fold(0, &axis(AXIS_LS_Y, 32767), GRANT_ALL);
        let dy: i32 = m4k.tick(0.04, 2160, GRANT_ALL).iter().map(|e| e.y).sum();
        assert_eq!(dy, -108, "stick up is screen up, twice as fast at 2160p");
    }

    #[test]
    fn deadzone_and_curve() {
        assert_eq!(curve(6000, 0), (0.0, 0.0));
        let (half, _) = curve(19660, 0);
        assert!(
            (half - 0.25).abs() < 0.01,
            "60% travel → (0.4/0.8)² = 0.25, got {half}"
        );
        let mut m = entered();
        m.fold(0, &axis(AXIS_LS_X, 3000), GRANT_ALL);
        assert!(!m.moving());
        assert!(m.tick(1.0, 1080, GRANT_ALL).is_empty());
    }

    #[test]
    fn right_stick_scrolls_normalized_both_axes() {
        let mut m = entered();
        m.fold(0, &axis(AXIS_RS_Y, 32767), GRANT_ALL);
        m.fold(0, &axis(AXIS_RS_X, -32767), GRANT_ALL);
        assert!(m.moving());
        let evs = m.tick(0.01, 1000, GRANT_ALL);
        let vertical = ScrollEvent::from_event(&evs[0]).unwrap();
        let horizontal = ScrollEvent::from_event(&evs[1]).unwrap();
        assert_eq!(
            (vertical.axis, vertical.delta, vertical.phase),
            (0, 586, ScrollPhase::Begin)
        );
        assert_eq!(
            (horizontal.axis, horizontal.delta, horizontal.phase),
            (1, -586, ScrollPhase::Begin)
        );
        m.fold(0, &axis(AXIS_RS_Y, 0), GRANT_ALL);
        m.fold(0, &axis(AXIS_RS_X, 0), GRANT_ALL);
        let ended = m.tick(0.01, 1000, GRANT_ALL);
        assert_eq!(ended.len(), 2);
        assert!(ended
            .iter()
            .all(|event| ScrollEvent::from_event(event).unwrap().phase == ScrollPhase::End));
    }

    #[test]
    fn leaving_cancels_open_scroll_axes() {
        let mut m = entered();
        m.fold(0, &axis(AXIS_RS_Y, 32767), GRANT_ALL);
        assert_eq!(
            ScrollEvent::from_event(&m.tick(0.01, 1080, GRANT_ALL)[0])
                .unwrap()
                .phase,
            ScrollPhase::Begin
        );
        let left = m.leave(0);
        assert_eq!(left.len(), 1);
        assert_eq!(
            ScrollEvent::from_event(&left[0]).unwrap().phase,
            ScrollPhase::Cancel
        );
    }

    #[test]
    fn touch_reaches_only_a_pad_in_a_mouse_mode() {
        let lead = |x: f64| Contact {
            pad: 0,
            surface: 0,
            finger: 0,
            touch: true,
            click: None,
            x,
            y: 0.5,
            raw: false,
        };
        let mut m = PadMouse::default();
        m.contact(lead(0.1), 1080, GRANT_ALL);
        assert!(m.contact(lead(0.9), 1080, GRANT_ALL).is_empty(), "off");
        assert!(m.touchpad_click(0, true, GRANT_ALL).is_empty(), "off");
        m.enter(0, PadMouseMode::Touchpad, GamepadSnapshot::default());
        m.contact(lead(0.1), 1080, GRANT_ALL);
        assert_eq!(
            m.contact(lead(0.2), 1080, GRANT_ALL)[0].kind,
            InputKind::MouseMove
        );
        assert!(
            m.fold(0, &button(BTN_A, true), GRANT_ALL).is_empty(),
            "a touchpad-mode pad keeps its buttons in the game"
        );
        assert_eq!(
            kinds(&m.touchpad_click(0, true, GRANT_ALL)),
            [(InputKind::MouseButtonDown, MOUSE_LEFT)]
        );
        assert_eq!(
            kinds(&m.leave(0)),
            [(InputKind::MouseButtonUp, MOUSE_LEFT)],
            "leaving lifts the click"
        );
    }

    #[test]
    fn a_long_stall_is_clamped() {
        let mut m = entered();
        m.fold(0, &axis(AXIS_LS_X, 32767), GRANT_ALL);
        let dx: i32 = m.tick(5.0, 1080, GRANT_ALL).iter().map(|e| e.x).sum();
        assert_eq!(dx, 67, "50 ms at 1350 px/s");
    }

    #[test]
    fn shared_mask_follows_the_pointer_grant() {
        let s = PadMouseShared::default();
        s.request(0b101);
        assert_eq!(s.active(GRANT_ALL), 0b101);
        assert_eq!(s.active(GRANT_KEYBOARD), 0);
        s.clear(2);
        assert_eq!(s.requested(), 0b001);
        assert_eq!(s.mode(0b001, GRANT_ALL), PadMouseMode::Full);
        assert_eq!(s.mode(0b001, GRANT_KEYBOARD), PadMouseMode::Off);

        s.set_mode(0b011, PadMouseMode::Touchpad);
        assert_eq!((s.requested(), s.touchpad_active(GRANT_ALL)), (0, 0b011));
        assert_eq!(s.mode(0b011, GRANT_ALL), PadMouseMode::Touchpad);
        s.set_mode(0b001, PadMouseMode::Full);
        assert_eq!(
            s.mode(0b011, GRANT_ALL),
            PadMouseMode::Off,
            "a mixed set reads as off"
        );
        s.request(0b010);
        assert_eq!((s.requested(), s.touchpad_active(GRANT_ALL)), (0b010, 0));
        let mut m = PadMouse::default();
        m.enter(3, PadMouseMode::Full, GamepadSnapshot::default());
        assert!(m.is_on(3) && !m.is_on(2));
    }

    /// Every row of [`MAP`], down and up, plus both triggers. Changing any row fails here.
    #[test]
    fn every_shipped_row_presses_and_releases() {
        let shipped = [
            (BTN_A, InputKind::MouseButtonDown, MOUSE_LEFT),
            (BTN_X, InputKind::MouseButtonDown, MOUSE_RIGHT),
            (BTN_Y, InputKind::MouseButtonDown, MOUSE_MIDDLE),
            (BTN_B, InputKind::KeyDown, 0x1B),
            (BTN_DPAD_UP, InputKind::KeyDown, 0x26),
            (BTN_DPAD_DOWN, InputKind::KeyDown, 0x28),
            (BTN_DPAD_LEFT, InputKind::KeyDown, 0x25),
            (BTN_DPAD_RIGHT, InputKind::KeyDown, 0x27),
            (BTN_START, InputKind::KeyDown, 0x0D),
            (BTN_LB, InputKind::KeyDown, 0x11),
            (BTN_RB, InputKind::KeyDown, 0x12),
            (BTN_LS_CLICK, InputKind::KeyDown, 0x10),
            (BTN_RS_CLICK, InputKind::KeyDown, 0x20),
            (BTN_GUIDE, InputKind::KeyDown, 0x5B),
        ];
        for (bit, down, code) in shipped {
            let up = match down {
                InputKind::KeyDown => InputKind::KeyUp,
                _ => InputKind::MouseButtonUp,
            };
            let mut m = entered();
            assert_eq!(
                kinds(&m.fold(0, &button(bit, true), GRANT_ALL)),
                [(down, code)],
                "{bit:#x} down"
            );
            assert_eq!(
                kinds(&m.fold(0, &button(bit, false), GRANT_ALL)),
                [(up, code)],
                "{bit:#x} up"
            );
        }
        let mut m = entered();
        assert_eq!(
            kinds(&m.fold(0, &axis(AXIS_RT, 255), GRANT_ALL)),
            [(InputKind::MouseButtonDown, MOUSE_LEFT)]
        );
        assert_eq!(
            kinds(&m.fold(0, &axis(AXIS_LT, 255), GRANT_ALL)),
            [(InputKind::MouseButtonDown, MOUSE_RIGHT)]
        );
    }
}
