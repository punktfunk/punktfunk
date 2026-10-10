//! Wayland `wl_pointer` axis frames → normalized wire scroll events.
//!
//! Pure state machine with no Wayland types: `wayland_scroll` feeds it the
//! callbacks (`axis`, `axis_source`, `axis_value120`, `axis_discrete`,
//! `axis_stop`, `frame`, `cancel`) and each `frame` emits that frame's events —
//! wl_pointer promises all of a scroll event's fields land before `frame`, so
//! nothing crosses the wire mid-frame.

use punktfunk_core::input::scroll::{ScrollAccumulator, ScrollPhase, ScrollSource};
use punktfunk_core::input::InputEvent;

/// `wl_pointer.axis_source`, reduced to what the wire vocabulary carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NativeAxisSource {
    /// No source event, or a source the enum predates. Units resolve like a
    /// wheel (the wire rule's one fixed fallback).
    #[default]
    Unknown,
    Wheel,
    /// Touchpad two-finger scroll — axis values are surface-px ≈ DIP.
    Finger,
    /// Dial / tilt / other uninterruptible continuous surface.
    Continuous,
}

/// Axis units per wheel detent on a pre-value120 protocol — the libinput
/// convention compositors followed. The wire documents this as an
/// approximation; real counts win whenever the frame supplies them.
const AXIS_UNITS_PER_DETENT: f64 = 15.0;

/// One in-flight scroll frame plus the gesture state that outlives it.
#[derive(Default)]
pub struct WaylandScrollFrame {
    /// Applies until the next `axis_source`, so a mid-gesture frame may omit
    /// it. Cleared when no gliding axis is open and at the end of every
    /// wheel-ish frame — a wheel must not inherit a gesture's source.
    source: NativeAxisSource,
    /// Raw per-axis displacement this frame in source units, wire sign
    /// (vertical already flipped to up-positive).
    value: [f64; 2],
    /// `axis_value120` — a real detent count in v120.
    v120: [Option<i32>; 2],
    /// `axis_discrete` — a real detent count, poorer than `v120`.
    discrete: [Option<i32>; 2],
    stops: [bool; 2],
    /// Gliding axes inside a `Begin`…`End`, with the source that opened them.
    open: [Option<NativeAxisSource>; 2],
    acc: ScrollAccumulator,
}

impl WaylandScrollFrame {
    pub fn new() -> Self {
        Self::default()
    }

    /// `wl_pointer.axis`. `axis` 0 = vertical, 1 = horizontal; `value` is the
    /// native value (vertical down-positive) and is negated to wire sign here.
    pub fn axis(&mut self, axis: u32, value: f64) {
        if let Some(v) = self.value.get_mut(axis as usize) {
            *v += if axis == 0 { -value } else { value };
        }
    }

    pub fn axis_source(&mut self, source: NativeAxisSource) {
        self.source = source;
    }

    /// `wl_pointer.axis_value120`: `value120` is a signed v120 detent count.
    /// Wire sign follows the axis sign — vertical already flipped.
    pub fn value120(&mut self, axis: u32, value120: i32) {
        if let Some(v) = self.v120.get_mut(axis as usize) {
            let value = if axis == 0 {
                value120.saturating_neg()
            } else {
                value120
            };
            *v = Some(v.unwrap_or(0).saturating_add(value));
        }
    }

    /// `wl_pointer.axis_discrete`: `detents` is a signed click count.
    pub fn discrete(&mut self, axis: u32, detents: i32) {
        if let Some(v) = self.discrete.get_mut(axis as usize) {
            let value = if axis == 0 {
                detents.saturating_neg()
            } else {
                detents
            };
            *v = Some(v.unwrap_or(0).saturating_add(value));
        }
    }

    /// `wl_pointer.axis_stop`: the compositor ended a gliding axis.
    pub fn stop(&mut self, axis: u32) {
        if let Some(s) = self.stops.get_mut(axis as usize) {
            *s = true;
        }
    }

    /// The frame boundary. Per axis, in order: a source switch cancels the
    /// open axis first, a gliding delta emits `Begin`/`Update`, a stop emits a
    /// zero `End`. A wheel emits `None` only. A stop on an axis nothing opened
    /// is stale and sends nothing.
    pub fn frame(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::new();
        for a in 0..2usize {
            self.flush_axis(a, &mut out);
        }
        // A wheel-ish frame's source claim does not outlive the frame; a
        // gesture's does — until every gliding axis has closed.
        if matches!(
            self.source,
            NativeAxisSource::Wheel | NativeAxisSource::Unknown
        ) || self.open.iter().all(Option::is_none)
        {
            self.source = NativeAxisSource::Unknown;
        }
        out
    }

    fn flush_axis(&mut self, a: usize, out: &mut Vec<InputEvent>) {
        let axis = a as u32;
        let value = std::mem::take(&mut self.value[a]);
        let v120 = self.v120[a].take();
        let discrete = self.discrete[a].take();
        let stop = std::mem::take(&mut self.stops[a]);
        if value == 0.0 && v120.is_none() && discrete.is_none() && !stop {
            return;
        }
        let src = if v120.is_some() || discrete.is_some() {
            NativeAxisSource::Wheel
        } else {
            self.source
        };
        let wheelish = matches!(src, NativeAxisSource::Wheel | NativeAxisSource::Unknown);
        if let Some(old) = self.open[a].filter(|old| wheelish || *old != src) {
            out.extend(self.quantize(old, ScrollPhase::Cancel, axis, 0.0));
            self.open[a] = None;
        }
        if wheelish {
            let amount = v120
                .map(f64::from)
                .or_else(|| discrete.map(|v| f64::from(v) * 120.0))
                .unwrap_or(value * 120.0 / AXIS_UNITS_PER_DETENT);
            out.extend(self.quantize(src, ScrollPhase::None, axis, amount));
            self.open[a] = None;
            return;
        }
        if value != 0.0 {
            let phase = if self.open[a].is_some() {
                ScrollPhase::Update
            } else {
                ScrollPhase::Begin
            };
            self.open[a] = Some(src);
            out.extend(self.quantize(src, phase, axis, value));
        }
        if stop && self.open[a].is_some() {
            out.extend(self.quantize(src, ScrollPhase::End, axis, 0.0));
            self.open[a] = None;
        }
    }

    /// Focus/seat/capability loss: `Cancel` every open axis. Idempotent.
    pub fn cancel(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::new();
        for a in 0..2usize {
            if let Some(src) = self.open[a].take() {
                out.extend(self.quantize(src, ScrollPhase::Cancel, a as u32, 0.0));
            }
        }
        self.value = [0.0; 2];
        self.v120 = [None; 2];
        self.discrete = [None; 2];
        self.stops = [false; 2];
        self.source = NativeAxisSource::Unknown;
        out
    }

    fn quantize(
        &mut self,
        src: NativeAxisSource,
        phase: ScrollPhase,
        axis: u32,
        delta: f64,
    ) -> Option<InputEvent> {
        let source = match src {
            NativeAxisSource::Unknown => ScrollSource::Unknown,
            NativeAxisSource::Wheel => ScrollSource::Wheel,
            NativeAxisSource::Finger => ScrollSource::Finger,
            NativeAxisSource::Continuous => ScrollSource::Continuous,
        };
        self.acc.event(source, phase, axis, delta)
    }
}

pub fn sdl_wheel(acc: &mut ScrollAccumulator, dx: f32, dy: f32) -> Vec<InputEvent> {
    [(0, dy), (1, dx)]
        .into_iter()
        .filter_map(|(axis, delta)| {
            acc.event(
                ScrollSource::Unknown,
                ScrollPhase::None,
                axis,
                f64::from(delta) * 120.0,
            )
        })
        .collect()
}

pub fn forward_native_scroll(captured_before: bool, captured_after: bool, blocked: bool) -> bool {
    captured_before && captured_after && !blocked
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::input::scroll::ScrollEvent;

    fn decode(ev: &InputEvent) -> ScrollEvent {
        ScrollEvent::from_event(ev).expect("a frame event must validate")
    }

    /// Wire delta in Q24.8 of `v120` units.
    fn q(v120: f64) -> i32 {
        (v120 * 256.0) as i32
    }

    #[test]
    fn sdl_fraction_never_reclassifies_the_next_notch() {
        let mut acc = ScrollAccumulator::new();
        let first = sdl_wheel(&mut acc, 0.0, 0.25);
        let next = sdl_wheel(&mut acc, -1.0, 1.0);
        assert_eq!(decode(&first[0]).delta, q(30.0));
        assert_eq!(decode(&next[0]).delta, q(120.0));
        assert_eq!(decode(&next[1]).delta, q(-120.0));
        for event in first.iter().chain(next.iter()) {
            assert_eq!(decode(event).source, ScrollSource::Unknown);
            assert_eq!(decode(event).phase, ScrollPhase::None);
        }
    }

    #[test]
    fn ownership_transitions_drop_native_batches() {
        assert!(forward_native_scroll(true, true, false));
        assert!(!forward_native_scroll(false, true, false));
        assert!(!forward_native_scroll(true, false, false));
        assert!(!forward_native_scroll(true, true, true));
    }

    #[test]
    fn repeated_counts_accumulate_without_signed_overflow() {
        let mut f = WaylandScrollFrame::new();
        f.value120(0, -30);
        f.value120(0, -30);
        let out = f.frame();
        assert_eq!(decode(&out[0]).delta, q(60.0));
        assert_eq!(decode(&out[0]).source, ScrollSource::Wheel);
        f.discrete(1, 1);
        f.discrete(1, 2);
        assert_eq!(decode(&f.frame()[0]).delta, q(360.0));
        f.value120(0, i32::MIN);
        assert_eq!(decode(&f.frame()[0]).delta, i32::MAX);
        f.discrete(0, i32::MIN);
        assert_eq!(decode(&f.frame()[0]).delta, i32::MAX);
    }

    #[test]
    fn wheel_prefers_the_real_count_over_axis_and_discrete() {
        let mut f = WaylandScrollFrame::new();
        f.axis_source(NativeAxisSource::Wheel);
        f.axis(0, -15.0); // native down-positive: -15 is one detent up
        f.discrete(0, -1);
        f.value120(0, -120);
        let out = f.frame();
        assert_eq!(out.len(), 1, "{out:?}");
        let se = decode(&out[0]);
        assert_eq!(se.source, ScrollSource::Wheel);
        assert_eq!(se.phase, ScrollPhase::None);
        assert_eq!(se.axis, 0);
        assert_eq!(se.delta, q(120.0), "value120 wins: {se:?}");
    }

    #[test]
    fn wheel_value120_keeps_sub_detent_fractions() {
        let mut f = WaylandScrollFrame::new();
        f.axis_source(NativeAxisSource::Wheel);
        f.axis(0, -3.0);
        f.value120(0, -30);
        let out = f.frame();
        assert_eq!(decode(&out[0]).delta, q(30.0));
    }

    #[test]
    fn wheel_discrete_fills_in_without_value120() {
        let mut f = WaylandScrollFrame::new();
        f.axis_source(NativeAxisSource::Wheel);
        f.discrete(0, -2);
        let out = f.frame();
        assert_eq!(decode(&out[0]).delta, q(240.0));
    }

    #[test]
    fn axis_only_wheel_uses_the_fifteen_unit_fallback() {
        let mut f = WaylandScrollFrame::new();
        f.axis_source(NativeAxisSource::Wheel);
        f.axis(1, 30.0); // two detents right
        let out = f.frame();
        let se = decode(&out[0]);
        assert_eq!(se.source, ScrollSource::Wheel);
        assert_eq!(se.axis, 1);
        assert_eq!(se.delta, q(240.0));
    }

    #[test]
    fn finger_runs_begin_update_end() {
        let mut f = WaylandScrollFrame::new();
        f.axis_source(NativeAxisSource::Finger);
        f.axis(0, -10.0); // fingers up 10 px
        let out = f.frame();
        assert_eq!(out.len(), 1);
        let se = decode(&out[0]);
        assert_eq!(se.source, ScrollSource::Finger);
        assert_eq!(se.phase, ScrollPhase::Begin);
        assert_eq!(se.delta, q(10.0), "surface-px are DIP: {se:?}");

        f.axis(0, -5.0);
        let out = f.frame();
        assert_eq!(decode(&out[0]).phase, ScrollPhase::Update);
        assert_eq!(decode(&out[0]).delta, q(5.0));

        // A stop with a last delta splits: movement, then a zero End.
        f.axis(0, -2.0);
        f.stop(0);
        let out = f.frame();
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(decode(&out[0]).phase, ScrollPhase::Update);
        assert_eq!(decode(&out[0]).delta, q(2.0));
        assert_eq!(decode(&out[1]).phase, ScrollPhase::End);
        assert_eq!(decode(&out[1]).delta, 0);
    }

    #[test]
    fn both_axes_report_in_one_frame() {
        let mut f = WaylandScrollFrame::new();
        f.axis_source(NativeAxisSource::Finger);
        f.axis(0, -4.0);
        f.axis(1, 3.0);
        let out = f.frame();
        assert_eq!(out.len(), 2);
        assert_eq!(decode(&out[0]).axis, 0);
        assert_eq!(decode(&out[0]).delta, q(4.0));
        assert_eq!(decode(&out[1]).axis, 1);
        assert_eq!(decode(&out[1]).delta, q(3.0));
    }

    #[test]
    fn a_stop_only_closes_the_axis_it_names() {
        let mut f = WaylandScrollFrame::new();
        f.axis_source(NativeAxisSource::Finger);
        f.axis(0, -4.0);
        f.axis(1, 4.0);
        let _ = f.frame();
        f.stop(0);
        let out = f.frame();
        assert_eq!(out.len(), 1);
        assert_eq!(decode(&out[0]).axis, 0);
        assert_eq!(decode(&out[0]).phase, ScrollPhase::End);
        // Axis 1 is still open: the next delta is an Update, not a Begin.
        f.axis(1, 2.0);
        let out = f.frame();
        assert_eq!(decode(&out[0]).phase, ScrollPhase::Update);
    }

    #[test]
    fn wheel_after_finger_stop_stays_a_wheel() {
        let mut f = WaylandScrollFrame::new();
        f.axis_source(NativeAxisSource::Finger);
        f.axis(0, -10.0);
        let _ = f.frame();
        f.stop(0);
        let out = f.frame();
        assert_eq!(decode(&out[0]).phase, ScrollPhase::End);
        // No source latch: the next frame's wheel is a wheel.
        f.axis_source(NativeAxisSource::Wheel);
        f.value120(0, -120);
        let out = f.frame();
        let se = decode(&out[0]);
        assert_eq!(se.source, ScrollSource::Wheel);
        assert_eq!(se.phase, ScrollPhase::None);
        assert_eq!(se.delta, q(120.0));
    }

    #[test]
    fn a_source_switch_cancels_the_open_axis() {
        let mut f = WaylandScrollFrame::new();
        f.axis_source(NativeAxisSource::Finger);
        f.axis(0, -10.0);
        let _ = f.frame();
        // A wheel lands mid-gesture: cancel the finger axis, then the wheel.
        f.axis_source(NativeAxisSource::Wheel);
        f.value120(0, -120);
        let out = f.frame();
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(decode(&out[0]).phase, ScrollPhase::Cancel);
        assert_eq!(decode(&out[0]).source, ScrollSource::Finger);
        assert_eq!(decode(&out[1]).source, ScrollSource::Wheel);
        assert_eq!(decode(&out[1]).delta, q(120.0));
    }

    #[test]
    fn cancel_closes_open_axes_and_is_idempotent() {
        let mut f = WaylandScrollFrame::new();
        f.axis_source(NativeAxisSource::Finger);
        f.axis(0, -10.0);
        f.axis(1, 5.0);
        let _ = f.frame();
        let out = f.cancel();
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out
            .iter()
            .all(|ev| decode(ev).phase == ScrollPhase::Cancel && decode(ev).delta == 0));
        assert!(f.cancel().is_empty());
        // The cancelled gesture is gone: a new source opens a fresh Begin.
        f.axis_source(NativeAxisSource::Continuous);
        f.axis(0, -3.0);
        let out = f.frame();
        assert_eq!(decode(&out[0]).phase, ScrollPhase::Begin);
        assert_eq!(decode(&out[0]).source, ScrollSource::Continuous);
    }

    #[test]
    fn no_source_falls_back_to_unknown_v120() {
        // A v5+ compositor that never sends axis_source: axis units alone get
        // the one fixed fallback — detents at 15 units, wire Unknown.
        let mut f = WaylandScrollFrame::new();
        f.axis(0, -15.0);
        let out = f.frame();
        let se = decode(&out[0]);
        assert_eq!(se.source, ScrollSource::Unknown);
        assert_eq!(se.delta, q(120.0));
    }

    #[test]
    fn stale_stop_without_an_open_axis_emits_nothing() {
        let mut f = WaylandScrollFrame::new();
        f.stop(0);
        assert!(f.frame().is_empty());
    }
}
