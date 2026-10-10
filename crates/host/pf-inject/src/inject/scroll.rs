//! Scroll ([`InputKind::Scroll`], and the legacy [`InputKind::MouseScroll`]
//! through [`from_legacy`]) → per-backend primitive plans.
//!
//! Pure and target-independent: the injectors execute the ops their
//! [`ScrollMapper`] emits, so tests on any platform assert the same mapping
//! production runs. A plan is a flat op list; [`ScrollOp::Frame`] marks the
//! backend's frame/flush boundary — on ei a nonzero scroll and a stop for the
//! same axis may not share a frame.
//!
//! Sign inside a plan is backend-native: the Wayland-family backends (libei,
//! Mutter, gamescope, KWin, wlroots) take positive-down on the vertical axis —
//! the wire's positive-up is negated there — while the horizontal axis and the
//! Windows wheel deltas stay positive.
//!
//! A finger's `End` is held for [`MOMENTUM_GRACE`] on the backends with a stop:
//! the client's momentum then continues the same interaction, and the stop goes
//! out at `MomentumEnd`. The injector flushes a held stop that falls due
//! ([`ScrollMapper::stop_due`], [`ScrollMapper::flush_due`]).
//!
//! A phased finger or touch gesture passes [`Gesture`] first: the host is the
//! touchpad driver for a remote finger, so libinput's start threshold and
//! direction lock apply here, once, for every client and backend.

use std::time::{Duration, Instant};

use punktfunk_core::input::scroll::{
    ScrollEvent, ScrollPhase, ScrollSource, ScrollUnits, SCROLL_DIP_PER_DETENT, SCROLL_SCALE,
};
use punktfunk_core::input::{InputEvent, InputKind, PRECISE_PX_PER_DETENT, SCROLL_FLAG_PRECISE};

/// How long a lifted finger's stop waits for the client's momentum. The Mac
/// starts momentum a frame after the lift; the rest is network jitter. Momentum
/// that comes later opens a new interaction.
pub const MOMENTUM_GRACE: Duration = Duration::from_millis(50);

/// DIP a gesture travels on an axis before that axis scrolls; less is the
/// fingers landing. libinput's `scroll.threshold`, in its 1000-dpi units.
pub const SCROLL_START_DIP: f64 = 5.0;

/// DIP one delta must move on the other axis of a scrolling gesture for that
/// axis to scroll too; less is drift. libinput's `direction_lock_threshold`.
pub const DIRECTION_LOCK_DIP: f64 = 5.0;

/// Where the client's finger stands on one axis of a [`Gesture`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Axis {
    /// Nothing open: no delta yet, or a cancel or momentum end came.
    #[default]
    Closed,
    /// Deltas may still come: between a begin and its stop, or in momentum.
    Open,
    /// The finger lifted (`End`); its momentum continues this gesture, a
    /// `Begin` starts the next one.
    Lifted,
}

/// One finger or touch gesture's start threshold and direction lock, libinput's
/// `evdev_post_scroll` rule per axis: motion builds up until it passes
/// [`SCROLL_START_DIP`], the axis that passes first scrolls, the other joins
/// only on a single delta of [`DIRECTION_LOCK_DIP`], and a delta outside the
/// lock is zero. A gesture runs from its first phased delta through the
/// momentum after the lift; the next `Begin` starts a new one.
#[derive(Default)]
struct Gesture {
    /// Motion on an axis that is not yet scrolling, in DIP.
    buildup: [f64; 2],
    /// Axes that scroll in this gesture.
    scrolling: [bool; 2],
    axis: [Axis; 2],
}

impl Gesture {
    /// Apply the rule to `se`, zeroing a delta outside the lock. `false` drops
    /// the event: a stop on an axis this gesture never scrolled, which no
    /// backend has open. A wheel, a controller, or a phaseless delta passes.
    fn admit(&mut self, se: &mut ScrollEvent) -> bool {
        if !matches!(se.source, ScrollSource::Finger | ScrollSource::Touch)
            || se.phase == ScrollPhase::None
        {
            return true;
        }
        let a = se.axis as usize;
        if se.is_stop() {
            let scrolled = self.scrolling[a];
            self.axis[a] = if se.phase == ScrollPhase::End {
                Axis::Lifted
            } else {
                Axis::Closed
            };
            if self.axis == [Axis::Closed; 2] {
                *self = Self::default();
            }
            return scrolled;
        }
        // A `Begin` after a lift is the next gesture; on an axis still open it
        // is one whose stop was lost. Its partner axis begins into the same
        // gesture.
        if se.phase == ScrollPhase::Begin
            && (self.axis[a] != Axis::Closed || self.axis.contains(&Axis::Lifted))
        {
            *self = Self::default();
        }
        self.axis[a] = Axis::Open;
        if self.scrolling[a] {
            return true;
        }
        let d = delta_units(se);
        self.buildup[a] += d;
        let starts = if self.scrolling[1 - a] {
            d.abs() >= DIRECTION_LOCK_DIP
        } else {
            self.buildup[a].abs() >= SCROLL_START_DIP
        };
        if starts {
            self.scrolling[a] = true;
        } else {
            se.delta = 0;
        }
        true
    }
}

/// Injection backend a plan is built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollBackend {
    /// libei (`reis`) through the RemoteDesktop portal: `scroll` in logical
    /// pixels, `scroll_discrete` in v120.
    Libei,
    /// Mutter's own RemoteDesktop session. Its EIS reads every scroll as a
    /// wheel, so the plan goes to `NotifyPointerAxis` instead, which carries
    /// the source ([`mutter_axis_calls`]).
    Mutter,
    /// gamescope's EIS socket — counts clicks, sees no stops.
    Gamescope,
    /// KWin `org_kde_kwin_fake_input` — a bare axis value, nothing else.
    Kwin,
    /// Windows `SendInput` `WHEEL`/`HWHEEL` deltas.
    Windows,
    /// wlroots `zwlr_virtual_pointer_v1`.
    Wlr,
}

/// wlroots `axis_source` the next axis ops in the frame are counted under.
/// Never `continuous`: GTK hands that source to a trackpoint device, and
/// Firefox pans by distance only for a touchpad. A finger is the one source
/// every toolkit scrolls by the distance sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxisSource {
    Wheel,
    Finger,
}

/// One primitive in backend-native units and sign.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScrollOp {
    /// Continuous distance: ei `scroll`, wl `axis`, KWin `axis`, Mutter
    /// `NotifyPointerAxis`. Logical pixels on ei/wl/Mutter, the 10-per-detent
    /// unit on KWin.
    Continuous { horizontal: bool, value: f64 },
    /// Whole v120 units: ei `scroll_discrete`, Windows `mouseData`.
    Discrete120 { horizontal: bool, value: i32 },
    /// wl `axis_discrete`: surface distance plus whole detents.
    DiscreteDetents {
        horizontal: bool,
        value: f64,
        detents: i32,
    },
    /// wl `axis_source` (Mutter: the source flag), ahead of the axis ops it
    /// describes.
    AxisSource(AxisSource),
    /// End or cancel the axis interaction: ei `scroll_stop`, wl `axis_stop`.
    /// wl cannot tell a cancel from an end; `cancel` is informational there.
    Stop { horizontal: bool, cancel: bool },
    /// Emit the backend frame (`ei_device.frame`, wl `frame`) before more ops.
    /// Backends with no frame primitive ignore it.
    Frame,
}

/// Stateful lowering of normalized scroll events onto one backend's
/// primitives. Lives on the injector — the integer residue it keeps per axis
/// is what lets sub-detent deltas still reach a click.
pub struct ScrollMapper {
    backend: ScrollBackend,
    /// Unsent integer fraction per axis, in v120 units. A source switch or a
    /// gesture boundary clears it, so wheel residue is never repriced as DIP.
    rem: [f64; 2],
    /// Source of the last event per axis.
    last_source: [Option<ScrollSource>; 2],
    /// libei/Mutter/wlr: the axis has an open continuous scroll interaction,
    /// held by this source. A different source's movement or an explicit
    /// `Begin` cancels it first — never in the same frame as the new delta.
    ongoing: [Option<ScrollSource>; 2],
    /// When a finger's `End` arrived that has not gone out yet; the
    /// interaction stays open for its momentum until [`MOMENTUM_GRACE`] ends.
    held: [Option<Instant>; 2],
    /// The open finger or touch gesture's threshold and lock.
    gesture: Gesture,
}

/// Mutter `RemoteDesktop.Session.NotifyPointerAxis` flags.
pub const MUTTER_AXIS_FINISH: u32 = 1 << 0;
pub const MUTTER_AXIS_WHEEL: u32 = 1 << 1;
pub const MUTTER_AXIS_FINGER: u32 = 1 << 2;

/// A Mutter plan as `NotifyPointerAxis(dx, dy, flags)` calls: each distance
/// under the source ahead of it, a stop as a zero `FINISH`, which ends both
/// axes. A cancel sends nothing: Mutter has no cancel, and a finish would start
/// the app's own kinetic tail.
pub fn mutter_axis_calls(ops: &[ScrollOp]) -> Vec<(f64, f64, u32)> {
    let mut source = MUTTER_AXIS_FINGER;
    let mut calls = Vec::new();
    for op in ops {
        match *op {
            ScrollOp::AxisSource(s) => {
                source = match s {
                    AxisSource::Wheel => MUTTER_AXIS_WHEEL,
                    AxisSource::Finger => MUTTER_AXIS_FINGER,
                }
            }
            ScrollOp::Continuous { horizontal, value } => calls.push(if horizontal {
                (value, 0.0, source)
            } else {
                (0.0, value, source)
            }),
            ScrollOp::Stop { cancel: false, .. } => {
                calls.push((0.0, 0.0, source | MUTTER_AXIS_FINISH))
            }
            _ => {}
        }
    }
    calls
}

/// A legacy [`InputKind::MouseScroll`] as the normalized event it means: a
/// counted `x` is a wheel in v120, a precise one a distance of
/// `x / 120 × PRECISE_PX_PER_DETENT` DIP. No phase: the old wire has none.
pub fn from_legacy(ev: &InputEvent) -> Option<InputEvent> {
    if ev.kind != InputKind::MouseScroll {
        return None;
    }
    let (source, units) = if ev.flags & SCROLL_FLAG_PRECISE != 0 {
        (
            ScrollSource::Continuous,
            f64::from(ev.x) * PRECISE_PX_PER_DETENT / 120.0,
        )
    } else {
        (ScrollSource::Wheel, f64::from(ev.x))
    };
    Some(
        ScrollEvent {
            source,
            phase: ScrollPhase::None,
            axis: u32::from(ev.code == 1),
            delta: (units * SCROLL_SCALE) as i32, // `as` saturates
        }
        .to_event(),
    )
}

/// Wire delta in the source's own unit — v120 for Wheel/Unknown, DIP for the
/// rest (same number, different meaning; [`ScrollEvent::units`] picks the rate).
fn delta_units(se: &ScrollEvent) -> f64 {
    f64::from(se.delta) / SCROLL_SCALE
}

/// Mutter turns a wheel-sourced distance into `value120 = 12 × distance` and
/// keeps the fraction, so v120 ÷ 12 arrives exact, hi-res wheels included.
fn mutter_wheel(se: ScrollEvent, ops: &mut Vec<ScrollOp>) {
    if se.delta == 0 {
        return;
    }
    let h = se.axis == 1;
    let v = delta_units(&se) / 12.0;
    ops.extend([
        ScrollOp::AxisSource(AxisSource::Wheel),
        ScrollOp::Continuous {
            horizontal: h,
            value: if h { v } else { -v },
        },
    ]);
}

impl ScrollMapper {
    pub fn new(backend: ScrollBackend) -> Self {
        ScrollMapper {
            backend,
            rem: [0.0; 2],
            last_source: [None; 2],
            ongoing: [None; 2],
            held: [None; 2],
            gesture: Gesture::default(),
        }
    }

    /// Ops for `ev` — a normalized or a legacy scroll — in wire order. Empty
    /// when the event is malformed, the gesture rule swallows it, or the
    /// backend has nothing to say for it.
    pub fn plan(&mut self, ev: &InputEvent) -> Vec<ScrollOp> {
        let Some(mut se) = ScrollEvent::from_event(&from_legacy(ev).unwrap_or(*ev)) else {
            return Vec::new();
        };
        tracing::trace!(source = ?se.source, phase = ?se.phase, axis = se.axis, delta = se.delta, "scroll in");
        let a = se.axis as usize;
        // A stale stop from a different source must not close the live
        // interaction or its residue.
        if se.is_stop() && self.ongoing[a].is_some_and(|open| open != se.source) {
            return Vec::new();
        }
        // A stop the gesture drops is for an axis nothing below has open.
        if !self.gesture.admit(&mut se) {
            return Vec::new();
        }
        // A held stop resolves on the axis's next event: the same source's
        // momentum continues the interaction, anything else closes it first.
        let mut ops = Vec::new();
        if self.held[a].take().is_some()
            && !(se.is_momentum() && self.ongoing[a] == Some(se.source))
        {
            ops.extend(self.close(a));
        }
        // A source switch or any gesture boundary restarts the residue — a
        // missed stop cannot leak last gesture's fraction into the new one.
        if self.last_source[a] != Some(se.source)
            || se.is_stop()
            || matches!(se.phase, ScrollPhase::Begin | ScrollPhase::MomentumBegin)
        {
            self.rem[a] = 0.0;
        }
        self.last_source[a] = Some(se.source);
        ops.extend(match self.backend {
            ScrollBackend::Libei | ScrollBackend::Mutter | ScrollBackend::Wlr => self.native(se),
            ScrollBackend::Gamescope | ScrollBackend::Windows => self.counted(se),
            ScrollBackend::Kwin => self.kwin(se),
        });
        ops
    }

    /// When the earliest held stop falls due; `None` when nothing is held.
    pub fn stop_due(&self) -> Option<Instant> {
        self.held
            .iter()
            .flatten()
            .min()
            .map(|&t| t + MOMENTUM_GRACE)
    }

    /// Held stops whose grace has run out by `now`: no momentum came, so the
    /// lift was the end.
    pub fn flush_due(&mut self, now: Instant) -> Vec<ScrollOp> {
        let mut ops = Vec::new();
        for a in 0..2 {
            if self.held[a].is_some_and(|t| now >= t + MOMENTUM_GRACE) {
                self.held[a] = None;
                ops.extend(self.close(a));
            }
        }
        ops
    }

    /// A stop in its own frame. wlroots and Mutter tag it finger: a frame
    /// without a source is a wheel's, and an app glides only from a finger's
    /// stop.
    fn stop(&self, a: usize, cancel: bool) -> Vec<ScrollOp> {
        let mut ops = Vec::with_capacity(3);
        if matches!(self.backend, ScrollBackend::Wlr | ScrollBackend::Mutter) {
            ops.push(ScrollOp::AxisSource(AxisSource::Finger));
        }
        ops.extend([
            ScrollOp::Stop {
                horizontal: a == 1,
                cancel,
            },
            ScrollOp::Frame,
        ]);
        ops
    }

    /// End the axis's interaction cleanly.
    fn close(&mut self, a: usize) -> Vec<ScrollOp> {
        self.ongoing[a] = None;
        self.stop(a, false)
    }

    /// Stops for every axis still mid-interaction, held stops included,
    /// clearing all state. Emit on teardown so a compositor is not left holding
    /// a live gesture.
    pub fn cancel_all(&mut self) -> Vec<ScrollOp> {
        let mut ops = Vec::new();
        for a in 0..2 {
            if self.ongoing[a].is_some() {
                ops.extend(self.stop(a, true));
            }
        }
        *self = ScrollMapper::new(self.backend);
        ops
    }

    /// Integer v120 the backend owes this axis now; the fraction stays in `rem`.
    fn take_v120(&mut self, a: usize, v120: f64) -> i32 {
        let total = (self.rem[a] + v120).clamp(f64::from(i32::MIN), f64::from(i32::MAX));
        let whole = total.trunc();
        self.rem[a] = total - whole;
        whole as i32
    }

    /// Whole v120 clicks the click-counter backends owe this axis now: wheel
    /// units pass through, DIP re-prices at [`SCROLL_DIP_PER_DETENT`] per
    /// detent.
    fn clicks_v120(&mut self, a: usize, se: &ScrollEvent) -> i32 {
        let v = match se.units() {
            ScrollUnits::V120 => delta_units(se),
            ScrollUnits::Dip => delta_units(se) * (120.0 / SCROLL_DIP_PER_DETENT),
        };
        self.take_v120(a, v)
    }

    /// An open interaction belongs to one source: a different source's
    /// movement — wheel clicks included — or an explicit `Begin` first cancels
    /// it, in its own frame. ei and wl both forbid a stop sharing a frame with
    /// a nonzero delta on the axis.
    fn cancel_open(&mut self, a: usize, se: &ScrollEvent) -> Vec<ScrollOp> {
        match self.ongoing[a] {
            Some(open) if open != se.source || se.phase == ScrollPhase::Begin => {
                self.ongoing[a] = None;
                self.stop(a, true)
            }
            _ => Vec::new(),
        }
    }

    fn native(&mut self, se: ScrollEvent) -> Vec<ScrollOp> {
        let a = se.axis as usize;
        if se.is_stop() {
            return self.native_stop(se);
        }
        if self.backend == ScrollBackend::Wlr && se.is_momentum() && se.delta == 0 {
            return Vec::new();
        }
        let mut ops = self.cancel_open(a, &se);
        if se.is_wheel() {
            match self.backend {
                ScrollBackend::Libei => self.libei_wheel(se, &mut ops),
                ScrollBackend::Mutter => mutter_wheel(se, &mut ops),
                ScrollBackend::Wlr => self.wlr_wheel(se, &mut ops),
                _ => unreachable!(),
            }
        } else if se.delta != 0 {
            self.native_distance(se, &mut ops);
        }
        if !ops.is_empty() && !matches!(ops.last(), Some(ScrollOp::Frame)) {
            ops.push(ScrollOp::Frame);
        }
        ops
    }

    fn native_stop(&mut self, se: ScrollEvent) -> Vec<ScrollOp> {
        let a = se.axis as usize;
        // A lifted finger may still glide: its stop waits for the momentum.
        if se.phase == ScrollPhase::End
            && matches!(se.source, ScrollSource::Finger | ScrollSource::Touch)
            && self.ongoing[a] == Some(se.source)
        {
            self.held[a] = Some(Instant::now());
            return Vec::new();
        }
        self.ongoing[a] = None;
        // libei and Mutter cancel non-finger tails; wlroots has no distinct
        // cancel primitive.
        let cancel = se.phase == ScrollPhase::Cancel
            || (self.backend != ScrollBackend::Wlr
                && matches!(
                    se.source,
                    ScrollSource::Continuous | ScrollSource::Controller
                ));
        self.stop(a, cancel)
    }

    fn native_distance(&mut self, se: ScrollEvent, ops: &mut Vec<ScrollOp>) {
        if matches!(self.backend, ScrollBackend::Wlr | ScrollBackend::Mutter) {
            ops.push(ScrollOp::AxisSource(AxisSource::Finger));
        }
        let d = delta_units(&se);
        ops.push(ScrollOp::Continuous {
            horizontal: se.axis == 1,
            value: if se.axis == 1 { d } else { -d },
        });
        self.ongoing[se.axis as usize] = Some(se.source);
    }

    /// v120 alone: an EIS server prices a detent itself, and a pixel axis
    /// beside it would scroll twice.
    fn libei_wheel(&mut self, se: ScrollEvent, ops: &mut Vec<ScrollOp>) {
        let h = se.axis == 1;
        let disc = self.take_v120(se.axis as usize, delta_units(&se));
        if disc != 0 {
            ops.push(ScrollOp::Discrete120 {
                horizontal: h,
                value: if h { disc } else { -disc },
            });
        }
    }

    fn wlr_wheel(&mut self, se: ScrollEvent, ops: &mut Vec<ScrollOp>) {
        let a = se.axis as usize;
        let h = se.axis == 1;
        // No axis_value120 on this protocol: accumulate whole detents before emitting clicks.
        self.rem[a] += delta_units(&se);
        let steps = (self.rem[a] / 120.0).trunc() as i32;
        if steps == 0 {
            return;
        }
        self.rem[a] -= f64::from(steps) * 120.0;
        let signed = if h { steps } else { -steps };
        ops.extend([
            ScrollOp::AxisSource(AxisSource::Wheel),
            ScrollOp::DiscreteDetents {
                horizontal: h,
                value: f64::from(signed) * 15.0,
                detents: signed,
            },
        ]);
    }

    fn counted(&mut self, se: ScrollEvent) -> Vec<ScrollOp> {
        if se.is_stop() {
            return Vec::new();
        }
        // Integer wheel APIs have no stop primitive. The OS applies its own wheel preferences.
        let disc = self.clicks_v120(se.axis as usize, &se);
        if disc == 0 {
            return Vec::new();
        }
        let value = if self.backend == ScrollBackend::Gamescope && se.axis == 0 {
            -disc
        } else {
            disc
        };
        vec![
            ScrollOp::Discrete120 {
                horizontal: se.axis == 1,
                value,
            },
            ScrollOp::Frame,
        ]
    }

    fn kwin(&mut self, se: ScrollEvent) -> Vec<ScrollOp> {
        if se.is_stop() {
            return Vec::new(); // no stop primitive; the glide is the app's own
        }
        // A sourceless axis is read by every toolkit as 10 units per click,
        // so v120 goes at 10/120 and DIP at 10 per [`SCROLL_DIP_PER_DETENT`].
        // It takes a double, so nothing truncates early.
        let v = match se.units() {
            ScrollUnits::V120 => delta_units(&se) * (10.0 / 120.0),
            ScrollUnits::Dip => delta_units(&se) * (10.0 / SCROLL_DIP_PER_DETENT),
        };
        if v == 0.0 {
            return Vec::new();
        }
        vec![
            ScrollOp::Continuous {
                horizontal: se.axis == 1,
                value: if se.axis == 1 { v } else { -v },
            },
            ScrollOp::Frame,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::input::InputKind;

    fn ev(source: ScrollSource, phase: ScrollPhase, axis: u32, delta: f64) -> InputEvent {
        ScrollEvent {
            source,
            phase,
            axis,
            delta: (delta * SCROLL_SCALE) as i32,
        }
        .to_event()
    }

    fn plan(backend: ScrollBackend, events: &[InputEvent]) -> Vec<ScrollOp> {
        let mut m = ScrollMapper::new(backend);
        let mut ops = Vec::new();
        for e in events {
            ops.extend(m.plan(e));
        }
        ops
    }

    const V: bool = false;
    const H: bool = true;

    /// A vertical stop as `backend` emits it: wlroots and Mutter put the
    /// finger source in front.
    fn stopped(backend: ScrollBackend, cancel: bool) -> Vec<ScrollOp> {
        let mut ops = Vec::new();
        if matches!(backend, ScrollBackend::Wlr | ScrollBackend::Mutter) {
            ops.push(ScrollOp::AxisSource(AxisSource::Finger));
        }
        ops.extend([
            ScrollOp::Stop {
                horizontal: V,
                cancel,
            },
            ScrollOp::Frame,
        ]);
        ops
    }

    #[test]
    fn a_stop_carries_its_source_on_wlroots() {
        // No axis_source in a frame means a wheel: a finger's lift has to say finger.
        let mut m = ScrollMapper::new(ScrollBackend::Wlr);
        m.plan(&ev(ScrollSource::Finger, ScrollPhase::Begin, 0, 8.0));
        m.plan(&ev(ScrollSource::Finger, ScrollPhase::End, 0, 0.0));
        assert_eq!(
            m.flush_due(Instant::now() + MOMENTUM_GRACE),
            vec![
                ScrollOp::AxisSource(AxisSource::Finger),
                ScrollOp::Stop {
                    horizontal: V,
                    cancel: false
                },
                ScrollOp::Frame,
            ]
        );
    }

    #[test]
    fn zero_momentum_keeps_backend_cancel_behavior() {
        for backend in [ScrollBackend::Libei, ScrollBackend::Wlr] {
            let stop = stopped(backend, true);
            let mut mapper = ScrollMapper::new(backend);
            mapper.plan(&ev(ScrollSource::Controller, ScrollPhase::Update, 0, 5.0));
            let ops = mapper.plan(&ev(
                ScrollSource::Continuous,
                ScrollPhase::MomentumBegin,
                0,
                0.0,
            ));
            if backend == ScrollBackend::Libei {
                assert_eq!(ops, stop);
                assert!(mapper.cancel_all().is_empty());
            } else {
                assert!(ops.is_empty());
                assert_eq!(mapper.cancel_all(), stop);
            }
        }
    }

    #[test]
    fn continuous_momentum_end_stop_policy() {
        for backend in [ScrollBackend::Libei, ScrollBackend::Wlr] {
            assert_eq!(
                plan(
                    backend,
                    &[ev(
                        ScrollSource::Continuous,
                        ScrollPhase::MomentumEnd,
                        0,
                        0.0
                    )]
                ),
                stopped(backend, backend == ScrollBackend::Libei)
            );
        }
    }

    #[test]
    fn libei_wheel_is_v120_alone() {
        let ops = plan(
            ScrollBackend::Libei,
            &[ev(ScrollSource::Wheel, ScrollPhase::None, 0, 120.0)],
        );
        assert_eq!(
            ops,
            vec![
                ScrollOp::Discrete120 {
                    horizontal: V,
                    value: -120
                },
                ScrollOp::Frame,
            ]
        );
    }

    #[test]
    fn libei_unknown_fraction_and_finger_dip() {
        // 0.25 v120 carries no whole unit: nothing yet, the residue holds it.
        let mut m = ScrollMapper::new(ScrollBackend::Libei);
        for _ in 0..3 {
            assert!(m
                .plan(&ev(ScrollSource::Unknown, ScrollPhase::None, 0, 0.25))
                .is_empty());
        }
        assert_eq!(
            m.plan(&ev(ScrollSource::Unknown, ScrollPhase::None, 0, 0.25)),
            vec![
                ScrollOp::Discrete120 {
                    horizontal: V,
                    value: -1
                },
                ScrollOp::Frame,
            ]
        );
        let ops = plan(
            ScrollBackend::Libei,
            &[ev(ScrollSource::Finger, ScrollPhase::Update, 0, 60.0)],
        );
        assert_eq!(
            ops,
            vec![
                ScrollOp::Continuous {
                    horizontal: V,
                    value: -60.0
                },
                ScrollOp::Frame,
            ]
        );
    }

    #[test]
    fn horizontal_keeps_sign_and_axis() {
        // True continuous channels carry the DIP distance as-is.
        for backend in [ScrollBackend::Libei, ScrollBackend::Wlr] {
            let ops = plan(
                backend,
                &[ev(ScrollSource::Finger, ScrollPhase::Update, 1, -10.0)],
            );
            assert!(
                ops.iter().any(|o| matches!(
                    o,
                    ScrollOp::Continuous { horizontal: H, value } if *value == -10.0
                )),
                "{backend:?} horizontal finger: {ops:?}"
            );
        }
        // The click-priced channels reprice instead: KWin at 10 units/60 DIP,
        // gamescope at 120 v120/60 DIP — same axis, unnegated sign.
        let ops = plan(
            ScrollBackend::Kwin,
            &[ev(ScrollSource::Finger, ScrollPhase::Update, 1, -10.0)],
        );
        assert!(ops.iter().any(|o| matches!(
            o,
            ScrollOp::Continuous { horizontal: H, value } if (*value + 10.0 / 6.0).abs() < 1e-9
        )));
        let ops = plan(
            ScrollBackend::Gamescope,
            &[ev(ScrollSource::Finger, ScrollPhase::Update, 1, -10.0)],
        );
        assert!(ops.contains(&ScrollOp::Discrete120 {
            horizontal: H,
            value: -20
        }));
        // Horizontal does not negate.
        let ops = plan(
            ScrollBackend::Gamescope,
            &[ev(ScrollSource::Wheel, ScrollPhase::None, 1, -120.0)],
        );
        assert!(ops.contains(&ScrollOp::Discrete120 {
            horizontal: H,
            value: -120
        }));
        let ops = plan(
            ScrollBackend::Windows,
            &[ev(ScrollSource::Wheel, ScrollPhase::None, 1, -120.0)],
        );
        assert!(ops.contains(&ScrollOp::Discrete120 {
            horizontal: H,
            value: -120
        }));
    }

    #[test]
    fn stops_by_backend() {
        // libei: a finger's or touch's lift is held for its momentum and ends
        // clean, Cancel is always a cancel — once the axis scrolled.
        let scrolled = |source| ev(source, ScrollPhase::Begin, 0, 8.0);
        for (source, phase, cancel) in [
            (ScrollSource::Finger, ScrollPhase::End, false),
            (ScrollSource::Touch, ScrollPhase::End, false),
            (ScrollSource::Finger, ScrollPhase::Cancel, true),
        ] {
            let mut m = ScrollMapper::new(ScrollBackend::Libei);
            m.plan(&scrolled(source));
            let mut ops = m.plan(&ev(source, phase, 0, 0.0));
            ops.extend(m.flush_due(Instant::now() + MOMENTUM_GRACE));
            assert_eq!(
                ops,
                vec![
                    ScrollOp::Stop {
                        horizontal: V,
                        cancel
                    },
                    ScrollOp::Frame,
                ],
                "{source:?} {phase:?}"
            );
        }
        // Continuous/Controller end cancelled, and a zero-delta stop is not dropped.
        for source in [ScrollSource::Continuous, ScrollSource::Controller] {
            let ops = plan(
                ScrollBackend::Libei,
                &[ev(source, ScrollPhase::End, 0, 0.0)],
            );
            assert_eq!(
                ops,
                vec![
                    ScrollOp::Stop {
                        horizontal: V,
                        cancel: true
                    },
                    ScrollOp::Frame,
                ],
                "{source:?}"
            );
        }
        // A finger that never scrolled has nothing to stop.
        assert!(plan(
            ScrollBackend::Libei,
            &[ev(ScrollSource::Finger, ScrollPhase::End, 0, 0.0)]
        )
        .is_empty());
        // wlr: a stop emits source + axis_stop + frame; cancel is informational only.
        let mut m = ScrollMapper::new(ScrollBackend::Wlr);
        m.plan(&scrolled(ScrollSource::Finger));
        m.plan(&ev(ScrollSource::Finger, ScrollPhase::End, 0, 0.0));
        assert_eq!(
            m.flush_due(Instant::now() + MOMENTUM_GRACE),
            stopped(ScrollBackend::Wlr, false)
        );
        // gamescope/kwin/windows: stops are no-ops.
        for backend in [
            ScrollBackend::Gamescope,
            ScrollBackend::Kwin,
            ScrollBackend::Windows,
        ] {
            let ops = plan(
                backend,
                &[
                    scrolled(ScrollSource::Finger),
                    ev(ScrollSource::Finger, ScrollPhase::End, 0, 0.0),
                ],
            );
            assert!(
                !ops.iter().any(|o| matches!(o, ScrollOp::Stop { .. })),
                "{backend:?}: {ops:?}"
            );
        }
    }

    #[test]
    fn libei_begin_cancels_an_open_interaction() {
        let mut m = ScrollMapper::new(ScrollBackend::Libei);
        m.plan(&ev(ScrollSource::Finger, ScrollPhase::Update, 0, 5.0));
        // A new Begin on the same axis cancels the stale interaction first,
        // in its own frame — ei forbids scroll + stop on one axis per frame.
        let ops = m.plan(&ev(ScrollSource::Finger, ScrollPhase::Begin, 0, 6.0));
        assert_eq!(
            ops,
            vec![
                ScrollOp::Stop {
                    horizontal: V,
                    cancel: true
                },
                ScrollOp::Frame,
                ScrollOp::Continuous {
                    horizontal: V,
                    value: -6.0
                },
                ScrollOp::Frame,
            ]
        );
        // The other axis has no interaction: a Begin there stays quiet.
        let ops = m.plan(&ev(ScrollSource::Finger, ScrollPhase::Begin, 1, 0.0));
        assert!(ops.is_empty());
    }

    #[test]
    fn source_change_cancels_the_open_axis() {
        // A finger gesture still open, then wheel clicks: the interaction
        // closes in its own frame before the first click ops — ei/wl forbid a
        // stop sharing a frame with a nonzero delta on the axis.
        let mut m = ScrollMapper::new(ScrollBackend::Libei);
        m.plan(&ev(ScrollSource::Finger, ScrollPhase::Update, 0, 10.0));
        let ops = m.plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 0, 120.0));
        assert_eq!(
            ops,
            vec![
                ScrollOp::Stop {
                    horizontal: V,
                    cancel: true
                },
                ScrollOp::Frame,
                ScrollOp::Discrete120 {
                    horizontal: V,
                    value: -120
                },
                ScrollOp::Frame,
            ]
        );
        let mut m = ScrollMapper::new(ScrollBackend::Wlr);
        m.plan(&ev(ScrollSource::Finger, ScrollPhase::Update, 0, 10.0));
        let ops = m.plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 0, 120.0));
        assert_eq!(
            ops,
            vec![
                ScrollOp::AxisSource(AxisSource::Finger),
                ScrollOp::Stop {
                    horizontal: V,
                    cancel: true
                },
                ScrollOp::Frame,
                ScrollOp::AxisSource(AxisSource::Wheel),
                ScrollOp::DiscreteDetents {
                    horizontal: V,
                    value: -15.0,
                    detents: -1
                },
                ScrollOp::Frame,
            ]
        );
        // Between continuous sources the same rule holds.
        let mut m = ScrollMapper::new(ScrollBackend::Wlr);
        m.plan(&ev(ScrollSource::Controller, ScrollPhase::Update, 0, 10.0));
        let ops = m.plan(&ev(ScrollSource::Finger, ScrollPhase::Update, 0, 5.0));
        assert_eq!(&ops[..3], &stopped(ScrollBackend::Wlr, true)[..]);
    }

    #[test]
    fn foreign_momentum_cancels_the_open_interaction() {
        // A controller gesture open; a finger momentum tail is another
        // source's movement: the controller closes cancelled, in its own
        // frame, before the tail moves the axis.
        for backend in [
            ScrollBackend::Libei,
            ScrollBackend::Mutter,
            ScrollBackend::Wlr,
        ] {
            let mut m = ScrollMapper::new(backend);
            m.plan(&ev(ScrollSource::Controller, ScrollPhase::Begin, 0, 10.0));
            let ops = m.plan(&ev(ScrollSource::Finger, ScrollPhase::Momentum, 0, 5.0));
            let stop = stopped(backend, true);
            assert_eq!(&ops[..stop.len()], &stop[..], "{backend:?}");
            assert!(
                ops.contains(&ScrollOp::Continuous {
                    horizontal: V,
                    value: -5.0
                }),
                "{backend:?}: {ops:?}"
            );
        }
        // On click-counter backends finger momentum still counts distance.
        for backend in [ScrollBackend::Gamescope, ScrollBackend::Windows] {
            let ops = plan(
                backend,
                &[ev(ScrollSource::Finger, ScrollPhase::Momentum, 0, 5.0)],
            );
            assert!(
                ops.iter()
                    .any(|o| matches!(o, ScrollOp::Discrete120 { value: v, .. } if v.abs() == 10)),
                "{backend:?}: {ops:?}"
            );
        }
    }

    #[test]
    fn stale_stop_does_not_close_other_source() {
        let mut m = ScrollMapper::new(ScrollBackend::Wlr);
        m.plan(&ev(ScrollSource::Finger, ScrollPhase::Begin, 0, 10.0));
        // A Touch End while the finger gesture is open is stale: no stop
        // emitted, and the finger interaction stays open.
        assert!(m
            .plan(&ev(ScrollSource::Touch, ScrollPhase::End, 0, 0.0))
            .is_empty());
        // The finger's own End is held, then goes out clean.
        assert!(m
            .plan(&ev(ScrollSource::Finger, ScrollPhase::End, 0, 0.0))
            .is_empty());
        assert_eq!(
            m.flush_due(Instant::now() + MOMENTUM_GRACE),
            stopped(ScrollBackend::Wlr, false)
        );
    }

    #[test]
    fn cancel_all_closes_open_axes() {
        // Both interactive backends: two open axes stop in order; a second
        // teardown finds nothing.
        for backend in [ScrollBackend::Libei, ScrollBackend::Wlr] {
            let mut m = ScrollMapper::new(backend);
            m.plan(&ev(ScrollSource::Finger, ScrollPhase::Update, 0, 10.0));
            m.plan(&ev(ScrollSource::Touch, ScrollPhase::Update, 1, 10.0));
            let stops: Vec<ScrollOp> = m
                .cancel_all()
                .into_iter()
                .filter(|o| !matches!(o, ScrollOp::AxisSource(_)))
                .collect();
            assert_eq!(
                stops,
                vec![
                    ScrollOp::Stop {
                        horizontal: V,
                        cancel: true
                    },
                    ScrollOp::Frame,
                    ScrollOp::Stop {
                        horizontal: H,
                        cancel: true
                    },
                    ScrollOp::Frame,
                ],
                "{backend:?}"
            );
            assert!(m.cancel_all().is_empty(), "{backend:?}");
        }
        // Plain wheel clicks never open an interaction — nothing to cancel.
        for backend in [
            ScrollBackend::Libei,
            ScrollBackend::Wlr,
            ScrollBackend::Gamescope,
            ScrollBackend::Kwin,
            ScrollBackend::Windows,
        ] {
            let mut m = ScrollMapper::new(backend);
            m.plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 0, 120.0));
            assert!(m.cancel_all().is_empty(), "{backend:?}");
        }
    }

    #[test]
    fn momentum_continues_the_held_interaction() {
        // Lift, then the client's tail: no stop and no cancel between the
        // gesture and its momentum; the one stop goes out at MomentumEnd.
        for backend in [
            ScrollBackend::Libei,
            ScrollBackend::Mutter,
            ScrollBackend::Wlr,
        ] {
            let mut m = ScrollMapper::new(backend);
            m.plan(&ev(ScrollSource::Finger, ScrollPhase::Begin, 0, 10.0));
            m.plan(&ev(ScrollSource::Finger, ScrollPhase::Update, 0, 5.0));
            assert!(m
                .plan(&ev(ScrollSource::Finger, ScrollPhase::End, 0, 0.0))
                .is_empty());
            assert!(m.stop_due().is_some(), "{backend:?}");
            let mut tail = m.plan(&ev(
                ScrollSource::Finger,
                ScrollPhase::MomentumBegin,
                0,
                4.0,
            ));
            tail.extend(m.plan(&ev(ScrollSource::Finger, ScrollPhase::Momentum, 0, 2.0)));
            assert!(m.stop_due().is_none(), "{backend:?}");
            assert!(
                !tail.iter().any(|o| matches!(o, ScrollOp::Stop { .. })),
                "{backend:?}: {tail:?}"
            );
            assert!(
                tail.contains(&ScrollOp::Continuous {
                    horizontal: V,
                    value: -4.0
                }),
                "{backend:?}: {tail:?}"
            );
            assert_eq!(
                m.plan(&ev(ScrollSource::Finger, ScrollPhase::MomentumEnd, 0, 0.0)),
                stopped(backend, false),
                "{backend:?}"
            );
            assert!(m.cancel_all().is_empty(), "{backend:?}");
        }
    }

    #[test]
    fn held_stop_goes_out_after_the_grace() {
        for backend in [
            ScrollBackend::Libei,
            ScrollBackend::Mutter,
            ScrollBackend::Wlr,
        ] {
            let mut m = ScrollMapper::new(backend);
            m.plan(&ev(ScrollSource::Touch, ScrollPhase::Begin, 0, 10.0));
            m.plan(&ev(ScrollSource::Touch, ScrollPhase::End, 0, 0.0));
            let due = m.stop_due().expect("held");
            assert!(m.flush_due(due - Duration::from_millis(1)).is_empty());
            assert_eq!(m.flush_due(due), stopped(backend, false), "{backend:?}");
            assert!(m.stop_due().is_none());
            assert!(m.cancel_all().is_empty(), "{backend:?}");
        }
        // The click counters have no stop to hold.
        for backend in [
            ScrollBackend::Gamescope,
            ScrollBackend::Kwin,
            ScrollBackend::Windows,
        ] {
            let mut m = ScrollMapper::new(backend);
            m.plan(&ev(ScrollSource::Finger, ScrollPhase::Begin, 0, 10.0));
            m.plan(&ev(ScrollSource::Finger, ScrollPhase::End, 0, 0.0));
            assert!(m.stop_due().is_none(), "{backend:?}");
        }
    }

    #[test]
    fn held_stop_closes_before_anything_else_moves_the_axis() {
        let stop = [
            ScrollOp::Stop {
                horizontal: V,
                cancel: false,
            },
            ScrollOp::Frame,
        ];
        let lifted = || {
            let mut m = ScrollMapper::new(ScrollBackend::Libei);
            m.plan(&ev(ScrollSource::Finger, ScrollPhase::Begin, 0, 10.0));
            m.plan(&ev(ScrollSource::Finger, ScrollPhase::End, 0, 0.0));
            m
        };
        // The next gesture: the old one ends clean, no cancel on top.
        let ops = lifted().plan(&ev(ScrollSource::Finger, ScrollPhase::Begin, 0, 6.0));
        assert_eq!(&ops[..2], &stop);
        assert_eq!(
            &ops[2..],
            &[
                ScrollOp::Continuous {
                    horizontal: V,
                    value: -6.0
                },
                ScrollOp::Frame,
            ]
        );
        // A wheel click: the lift ends before the click lands.
        let ops = lifted().plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 0, 120.0));
        assert_eq!(&ops[..2], &stop);
        // The other axis leaves it held.
        let mut m = lifted();
        m.plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 1, 120.0));
        assert!(m.stop_due().is_some());
        // Teardown cancels a held lift like any open interaction.
        assert_eq!(
            lifted().cancel_all(),
            vec![
                ScrollOp::Stop {
                    horizontal: V,
                    cancel: true
                },
                ScrollOp::Frame,
            ]
        );
    }

    #[test]
    fn momentum_routes_per_source_and_backend() {
        // Continuous drives its own tail: forwarded on ei, finished cancelled.
        let mut m = ScrollMapper::new(ScrollBackend::Libei);
        let ops = m.plan(&ev(ScrollSource::Continuous, ScrollPhase::Momentum, 0, 5.0));
        assert!(ops.contains(&ScrollOp::Continuous {
            horizontal: V,
            value: -5.0
        }));
        let ops = m.plan(&ev(
            ScrollSource::Continuous,
            ScrollPhase::MomentumEnd,
            0,
            0.0,
        ));
        assert!(ops.contains(&ScrollOp::Stop {
            horizontal: V,
            cancel: true
        }));
        // Click counters forward momentum deltas as ordinary scroll (5 DIP
        // → 10 v120; ei sign on gamescope, wire sign on Windows).
        let ops = plan(
            ScrollBackend::Gamescope,
            &[ev(ScrollSource::Continuous, ScrollPhase::Momentum, 0, 5.0)],
        );
        assert!(ops.contains(&ScrollOp::Discrete120 {
            horizontal: V,
            value: -10
        }));
        let ops = plan(
            ScrollBackend::Windows,
            &[ev(ScrollSource::Continuous, ScrollPhase::Momentum, 0, 5.0)],
        );
        assert!(ops.contains(&ScrollOp::Discrete120 {
            horizontal: V,
            value: 10
        }));
        // wlr forwards continuous momentum as finger-sourced axis deltas: the
        // one source toolkits scroll by distance.
        let mut m = ScrollMapper::new(ScrollBackend::Wlr);
        let ops = m.plan(&ev(ScrollSource::Continuous, ScrollPhase::Momentum, 0, 5.0));
        assert_eq!(
            ops,
            vec![
                ScrollOp::AxisSource(AxisSource::Finger),
                ScrollOp::Continuous {
                    horizontal: V,
                    value: -5.0
                },
                ScrollOp::Frame,
            ]
        );
    }

    #[test]
    fn four_quarter_detents() {
        // 30 v120 × 4: libei/gamescope/windows emit 30 every event; wlr holds
        // sub-detent deltas until they complete a click.
        for backend in [
            ScrollBackend::Libei,
            ScrollBackend::Gamescope,
            ScrollBackend::Windows,
        ] {
            let mut m = ScrollMapper::new(backend);
            for _ in 0..4 {
                let ops = m.plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 0, 30.0));
                assert!(
                    ops.contains(&ScrollOp::Discrete120 {
                        horizontal: V,
                        value: -30
                    }) || ops.contains(&ScrollOp::Discrete120 {
                        horizontal: V,
                        value: 30
                    }),
                    "{backend:?}: {ops:?}"
                );
            }
        }
        let mut m = ScrollMapper::new(ScrollBackend::Wlr);
        for _ in 0..3 {
            assert!(m
                .plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 0, 30.0))
                .is_empty());
        }
        let ops = m.plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 0, 30.0));
        assert_eq!(
            ops,
            vec![
                ScrollOp::AxisSource(AxisSource::Wheel),
                ScrollOp::DiscreteDetents {
                    horizontal: V,
                    value: -15.0,
                    detents: -1
                },
                ScrollOp::Frame,
            ]
        );
    }

    #[test]
    fn continuous_tenth_dip_adds_up() {
        // ~0.1 DIP (wire 26) × 10 ≈ 1 DIP = 2 v120: ei/wl keep native floats
        // each event; the integer backends emit the whole units they accumulate.
        let mut m = ScrollMapper::new(ScrollBackend::Libei);
        for _ in 0..10 {
            let ops = m.plan(&ev(
                ScrollSource::Continuous,
                ScrollPhase::Update,
                0,
                26.0 / 256.0,
            ));
            assert!(ops.contains(&ScrollOp::Continuous {
                horizontal: V,
                value: -(26.0 / 256.0)
            }));
            assert!(!ops
                .iter()
                .any(|o| matches!(o, ScrollOp::Discrete120 { .. })));
        }
        let mut m = ScrollMapper::new(ScrollBackend::Wlr);
        for _ in 0..10 {
            let ops = m.plan(&ev(
                ScrollSource::Continuous,
                ScrollPhase::Update,
                0,
                26.0 / 256.0,
            ));
            assert!(ops.contains(&ScrollOp::Continuous {
                horizontal: V,
                value: -(26.0 / 256.0)
            }));
            assert!(!ops
                .iter()
                .any(|o| matches!(o, ScrollOp::DiscreteDetents { .. })));
        }
        for backend in [ScrollBackend::Gamescope, ScrollBackend::Windows] {
            let mut m = ScrollMapper::new(backend);
            let mut total = 0;
            for _ in 0..10 {
                for op in m.plan(&ev(
                    ScrollSource::Continuous,
                    ScrollPhase::Update,
                    0,
                    26.0 / 256.0,
                )) {
                    if let ScrollOp::Discrete120 { value, .. } = op {
                        total += value;
                    }
                }
            }
            // 10 × 26 wire = 1.015625 DIP → 2.03125 v120 → 2 whole units out.
            assert_eq!(total.abs(), 2, "{backend:?}");
        }
    }

    #[test]
    fn kwin_table() {
        // Wheel 120 v120 → 10 units; Finger 60 DIP → 10 units; doubles, no stops.
        let ops = plan(
            ScrollBackend::Kwin,
            &[ev(ScrollSource::Wheel, ScrollPhase::None, 0, 120.0)],
        );
        assert_eq!(
            ops,
            vec![
                ScrollOp::Continuous {
                    horizontal: V,
                    value: -10.0
                },
                ScrollOp::Frame,
            ]
        );
        let ops = plan(
            ScrollBackend::Kwin,
            &[ev(ScrollSource::Finger, ScrollPhase::Update, 0, 60.0)],
        );
        assert!(ops.contains(&ScrollOp::Continuous {
            horizontal: V,
            value: -10.0
        }));
    }

    #[test]
    fn residue_is_per_axis_and_source() {
        let mut m = ScrollMapper::new(ScrollBackend::Windows);
        // 0.5 v120 vertical held, then a horizontal event emits on its own.
        assert!(m
            .plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 0, 0.5))
            .is_empty());
        let ops = m.plan(&ev(ScrollSource::Wheel, ScrollPhase::None, 1, 120.0));
        assert!(ops.contains(&ScrollOp::Discrete120 {
            horizontal: H,
            value: 120
        }));
        // A source switch on axis 0 must not reprice the wheel residue: the
        // finger delta lands whole.
        let ops = m.plan(&ev(ScrollSource::Finger, ScrollPhase::Update, 0, 10.0));
        assert!(ops.contains(&ScrollOp::Discrete120 {
            horizontal: V,
            value: 20
        }));
    }

    #[test]
    fn malformed_events_plan_nothing() {
        let mut m = ScrollMapper::new(ScrollBackend::Libei);
        // Wrong tag, bad axis, phased wheel, stop with distance.
        assert!(m
            .plan(&InputEvent {
                kind: InputKind::MouseMove,
                _pad: [0; 3],
                code: 0,
                x: 120,
                y: 0,
                flags: 0,
            })
            .is_empty());
        assert!(m
            .plan(&ev(ScrollSource::Wheel, ScrollPhase::Begin, 0, 0.0))
            .is_empty());
        let mut bad = ev(ScrollSource::Finger, ScrollPhase::Update, 0, 5.0);
        bad.code = 9;
        assert!(m.plan(&bad).is_empty());
    }

    #[test]
    fn mutter_sends_every_scroll_with_its_source() {
        // A wheel detent: v120 ÷ 12 under the wheel flag, so Mutter's ×12
        // lands on exactly 120 — a quarter detent on exactly 30.
        let ops = plan(
            ScrollBackend::Mutter,
            &[ev(ScrollSource::Wheel, ScrollPhase::None, 0, 120.0)],
        );
        assert_eq!(
            mutter_axis_calls(&ops),
            vec![(0.0, -10.0, MUTTER_AXIS_WHEEL)]
        );
        let ops = plan(
            ScrollBackend::Mutter,
            &[ev(ScrollSource::Wheel, ScrollPhase::None, 1, 30.0)],
        );
        assert_eq!(mutter_axis_calls(&ops), vec![(2.5, 0.0, MUTTER_AXIS_WHEEL)]);
        // A finger: DIP as logical pixels under the finger flag, and the
        // lift as one FINISH once the grace runs out.
        let mut m = ScrollMapper::new(ScrollBackend::Mutter);
        let mut ops = m.plan(&ev(ScrollSource::Finger, ScrollPhase::Begin, 0, 60.0));
        ops.extend(m.plan(&ev(ScrollSource::Finger, ScrollPhase::End, 0, 0.0)));
        ops.extend(m.flush_due(Instant::now() + MOMENTUM_GRACE));
        assert_eq!(
            mutter_axis_calls(&ops),
            vec![
                (0.0, -60.0, MUTTER_AXIS_FINGER),
                (0.0, 0.0, MUTTER_AXIS_FINGER | MUTTER_AXIS_FINISH),
            ]
        );
        // A controller: a finger to Mutter too (a continuous source lands on
        // a trackpoint device in GTK), and its end is a cancel — nothing, so
        // the app starts no kinetic tail of its own.
        let ops = plan(
            ScrollBackend::Mutter,
            &[
                ev(ScrollSource::Controller, ScrollPhase::Update, 0, 8.0),
                ev(ScrollSource::Controller, ScrollPhase::End, 0, 0.0),
            ],
        );
        assert_eq!(
            mutter_axis_calls(&ops),
            vec![(0.0, -8.0, MUTTER_AXIS_FINGER)]
        );
    }

    fn legacy(x: i32, horizontal: bool, precise: bool) -> InputEvent {
        InputEvent {
            kind: InputKind::MouseScroll,
            _pad: [0; 3],
            code: u32::from(horizontal),
            x,
            y: 0,
            flags: if precise { SCROLL_FLAG_PRECISE } else { 0 },
        }
    }

    #[test]
    fn legacy_scroll_takes_the_same_table() {
        let one = |backend, e: InputEvent| plan(backend, &[e]);
        // Counted: a wheel in v120 on every backend.
        assert_eq!(
            one(ScrollBackend::Windows, legacy(120, false, false)),
            vec![
                ScrollOp::Discrete120 {
                    horizontal: V,
                    value: 120
                },
                ScrollOp::Frame
            ]
        );
        assert!(one(ScrollBackend::Kwin, legacy(120, true, false)).contains(
            &ScrollOp::Continuous {
                horizontal: H,
                value: 10.0
            }
        ));
        assert!(one(ScrollBackend::Wlr, legacy(120, false, false)).contains(
            &ScrollOp::DiscreteDetents {
                horizontal: V,
                value: -15.0,
                detents: -1
            }
        ));
        // Precise: x / 12 DIP, the distances the per-injector code priced.
        // gamescope 60 → 10 v120, Windows 120 → 20 (3 lines), KWin 72 → 1
        // unit, libei/wlr 120 → 10 px.
        assert!(
            one(ScrollBackend::Gamescope, legacy(60, false, true)).contains(
                &ScrollOp::Discrete120 {
                    horizontal: V,
                    value: -10
                }
            )
        );
        assert!(
            one(ScrollBackend::Windows, legacy(120, false, true)).contains(
                &ScrollOp::Discrete120 {
                    horizontal: V,
                    value: 20
                }
            )
        );
        assert!(one(ScrollBackend::Kwin, legacy(72, false, true))
            .iter()
            .any(
                |o| matches!(o, ScrollOp::Continuous { value, .. } if (value + 1.0).abs() < 1e-3)
            ));
        for backend in [
            ScrollBackend::Libei,
            ScrollBackend::Mutter,
            ScrollBackend::Wlr,
        ] {
            assert!(
                one(backend, legacy(120, false, true)).contains(&ScrollOp::Continuous {
                    horizontal: V,
                    value: -10.0
                }),
                "{backend:?}"
            );
        }
        assert_eq!(
            from_legacy(&legacy(-120, true, true))
                .and_then(|e| ScrollEvent::from_event(&e))
                .map(|se| (se.source, se.phase, se.axis, se.delta)),
            Some((ScrollSource::Continuous, ScrollPhase::None, 1, -10 * 256))
        );
    }

    fn finger(phase: ScrollPhase, axis: u32, delta: f64) -> InputEvent {
        ev(ScrollSource::Finger, phase, axis, delta)
    }

    fn clicks(value: i32) -> Vec<ScrollOp> {
        vec![
            ScrollOp::Discrete120 {
                horizontal: V,
                value,
            },
            ScrollOp::Frame,
        ]
    }

    /// Vertical finger steps on Windows: each `(phase, DIP)` and the v120
    /// clicks it plans, 0 for nothing.
    fn gesture_steps(name: &str, steps: &[(ScrollPhase, f64, i32)]) {
        let mut m = ScrollMapper::new(ScrollBackend::Windows);
        for (i, &(phase, dip, v120)) in steps.iter().enumerate() {
            let ops = m.plan(&finger(phase, 0, dip));
            let want = if v120 == 0 { Vec::new() } else { clicks(v120) };
            assert_eq!(ops, want, "{name}, step {i}");
        }
    }

    #[test]
    fn a_gesture_scrolls_only_past_the_start_threshold() {
        use ScrollPhase::*;
        // The delta that crosses 5 DIP goes out as itself, not as the
        // buildup — no jump as scrolling engages; then every delta counts.
        gesture_steps(
            "buildup",
            &[
                (Begin, 2.0, 0),
                (Update, 2.0, 0),
                (Update, 2.0, 4),
                (Update, 0.5, 1),
            ],
        );
        // The fingers landing: a short move against the scroll cancels out
        // in the buildup and the scroll starts in the finger's direction.
        gesture_steps(
            "landing",
            &[
                (Begin, 2.0, 0),
                (Update, 1.0, 0),
                (Update, -3.0, 0),
                (Update, -6.0, -12),
            ],
        );
        // No End arrived: the next Begin restarts the rule all the same.
        gesture_steps(
            "lost stop",
            &[(Begin, 8.0, 16), (Begin, 3.0, 0), (Update, 3.0, 6)],
        );
    }

    #[test]
    fn drift_off_the_scrolling_axis_is_locked_out() {
        // Vertical scrolls; the small horizontal deltas a hand adds never do,
        // and their stop ends nothing — a FINISH would end the vertical
        // interaction with it.
        let mut m = ScrollMapper::new(ScrollBackend::Mutter);
        let mut ops = m.plan(&finger(ScrollPhase::Begin, 0, 8.0));
        ops.extend(m.plan(&finger(ScrollPhase::Begin, 1, 1.5)));
        ops.extend(m.plan(&finger(ScrollPhase::Update, 0, 6.0)));
        ops.extend(m.plan(&finger(ScrollPhase::Update, 1, 2.0)));
        ops.extend(m.plan(&finger(ScrollPhase::Update, 1, 4.0)));
        ops.extend(m.plan(&finger(ScrollPhase::End, 0, 0.0)));
        ops.extend(m.plan(&finger(ScrollPhase::End, 1, 0.0)));
        ops.extend(m.flush_due(Instant::now() + MOMENTUM_GRACE));
        assert_eq!(
            mutter_axis_calls(&ops),
            vec![
                (0.0, -8.0, MUTTER_AXIS_FINGER),
                (0.0, -6.0, MUTTER_AXIS_FINGER),
                (0.0, 0.0, MUTTER_AXIS_FINGER | MUTTER_AXIS_FINISH),
            ]
        );
        // A deliberate sideways move joins the scroll: one delta of 5 DIP,
        // and its lift is then a real stop, held for momentum like the other.
        let mut m = ScrollMapper::new(ScrollBackend::Libei);
        m.plan(&finger(ScrollPhase::Begin, 0, 8.0));
        assert!(m.plan(&finger(ScrollPhase::Begin, 1, 4.0)).is_empty());
        assert_eq!(
            m.plan(&finger(ScrollPhase::Update, 1, 5.0)),
            vec![
                ScrollOp::Continuous {
                    horizontal: H,
                    value: 5.0
                },
                ScrollOp::Frame,
            ]
        );
        assert!(m.plan(&finger(ScrollPhase::End, 1, 0.0)).is_empty());
        assert!(m.stop_due().is_some());
    }

    #[test]
    fn momentum_keeps_the_gestures_lock() {
        // The lift ends the tracking, not the gesture: momentum scrolls from
        // its first delta with no new threshold, the locked-out axis stays
        // out, and MomentumEnd closes the gesture for the next Begin.
        let mut m = ScrollMapper::new(ScrollBackend::Libei);
        m.plan(&finger(ScrollPhase::Begin, 0, 8.0));
        m.plan(&finger(ScrollPhase::Begin, 1, 1.0));
        m.plan(&finger(ScrollPhase::End, 0, 0.0));
        m.plan(&finger(ScrollPhase::End, 1, 0.0));
        assert_eq!(
            m.plan(&finger(ScrollPhase::MomentumBegin, 0, 3.0)),
            vec![
                ScrollOp::Continuous {
                    horizontal: V,
                    value: -3.0
                },
                ScrollOp::Frame,
            ]
        );
        assert!(m
            .plan(&finger(ScrollPhase::MomentumBegin, 1, 2.0))
            .is_empty());
        assert!(m.plan(&finger(ScrollPhase::MomentumEnd, 1, 0.0)).is_empty());
        assert_eq!(
            m.plan(&finger(ScrollPhase::MomentumEnd, 0, 0.0)),
            vec![
                ScrollOp::Stop {
                    horizontal: V,
                    cancel: false
                },
                ScrollOp::Frame,
            ]
        );
        assert!(m.plan(&finger(ScrollPhase::Begin, 0, 3.0)).is_empty());
    }

    #[test]
    fn wheels_and_phaseless_fingers_skip_the_gesture_rule() {
        for (source, phase) in [
            (ScrollSource::Finger, ScrollPhase::None),
            (ScrollSource::Continuous, ScrollPhase::Update),
            (ScrollSource::Controller, ScrollPhase::Update),
        ] {
            let ops = plan(ScrollBackend::Windows, &[ev(source, phase, 0, 1.0)]);
            assert_eq!(ops, clicks(2), "{source:?}");
        }
        let ops = plan(
            ScrollBackend::Windows,
            &[ev(ScrollSource::Wheel, ScrollPhase::None, 0, 30.0)],
        );
        assert_eq!(ops, clicks(30));
    }
}
