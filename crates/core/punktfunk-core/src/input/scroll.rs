//! Normalized scroll events ([`InputKind::Scroll`]): source, gesture phase, and a
//! Q24.8 distance on one axis — plus the client-side quantizer and the single
//! outbound compatibility seam.
//!
//! `delta` is signed Q24.8 in the source's own unit: 120-per-detent ("v120") for
//! [`ScrollSource::Wheel`]/[`ScrollSource::Unknown`], device-independent pixels
//! ("DIP") for every other source. Positive is up on the vertical axis and right
//! on the horizontal, matching the legacy [`InputKind::MouseScroll`] convention.
//! `code` picks the axis (0 = vertical, 1 = horizontal); `y` stays 0; `flags`
//! carries the source in its low byte and the phase in bits 8–15.
//!
//! [`ScrollOutput`] is the final outbound gate: a host that advertised
//! `HOST_CAP2_SCROLL` receives the event unchanged (modulo the invert toggle);
//! toward an older host it converts once into a `MouseScroll`, so no caller
//! needs its own conversion or a second inversion point.

use super::{InputEvent, InputKind, PRECISE_PX_PER_DETENT, SCROLL_FLAG_PRECISE};

/// Q24.8 fixed-point scale of [`ScrollEvent::delta`].
pub const SCROLL_SCALE: f64 = 256.0;

/// Device-independent pixels one wheel detent spans. Converts DIP to v120 at
/// `DIP * 120 / 60 = DIP * 2`.
pub const SCROLL_DIP_PER_DETENT: f64 = 60.0;

/// What produced the distance. `flags` low byte.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollSource {
    /// Source not reported; treated as detent-counted wheel units.
    Unknown = 0,
    /// Notched wheel; `delta` is Q24.8 v120.
    Wheel = 1,
    /// Finger on a touchpad or similar; `delta` is Q24.8 DIP.
    Finger = 2,
    /// Continuous surface without finger tracking (dial, tilt wheel).
    Continuous = 3,
    /// Touchscreen pan; `delta` is Q24.8 DIP.
    Touch = 4,
    /// Controller-driven scroll (stick, gyro, touchpad emulation).
    Controller = 5,
}

/// Gesture boundary marker. `flags` bits 8–15.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollPhase {
    /// No boundary — a plain delta (the only phase wheel sources may send).
    None = 0,
    /// Gesture starts; may carry its first delta.
    Begin = 1,
    /// Gesture delta.
    Update = 2,
    /// Gesture ends; `delta` must be 0.
    End = 3,
    /// Gesture cancelled; `delta` must be 0.
    Cancel = 4,
    /// Client-computed kinetic tail starts after the gesture's `End`.
    MomentumBegin = 5,
    /// Kinetic delta.
    Momentum = 6,
    /// Kinetic tail ends; `delta` must be 0.
    MomentumEnd = 7,
}

/// Unit a [`ScrollEvent::delta`] carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollUnits {
    /// 120-per-detent wheel units (one notch = 120).
    V120,
    /// Device-independent pixels (one detent = [`SCROLL_DIP_PER_DETENT`]).
    Dip,
}

/// One normalized scroll event, decoded out of the [`InputEvent`] field packing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollEvent {
    pub source: ScrollSource,
    pub phase: ScrollPhase,
    /// 0 = vertical, 1 = horizontal.
    pub axis: u32,
    /// Signed Q24.8 delta in [`Self::units`]; 0 on a stop.
    pub delta: i32,
}

impl ScrollEvent {
    /// Pack into the fixed 18-byte event.
    pub fn to_event(&self) -> InputEvent {
        InputEvent {
            kind: InputKind::Scroll,
            _pad: [0; 3],
            code: self.axis,
            x: self.delta,
            y: 0,
            flags: (self.source as u32) | ((self.phase as u32) << 8),
        }
    }

    /// Decode and validate. `None` on a wrong tag or any malformed body:
    /// axis > 1, `y` ≠ 0, flags outside the low 16 bits, an out-of-range source
    /// or phase, a stop carrying distance, a wheel with a phase, or a momentum
    /// phase from a source that cannot glide.
    pub fn from_event(ev: &InputEvent) -> Option<ScrollEvent> {
        if ev.kind != InputKind::Scroll {
            return None;
        }
        if ev.code > 1 || ev.y != 0 || ev.flags & !0xffff != 0 {
            return None;
        }
        let source = match (ev.flags & 0xff) as u8 {
            0 => ScrollSource::Unknown,
            1 => ScrollSource::Wheel,
            2 => ScrollSource::Finger,
            3 => ScrollSource::Continuous,
            4 => ScrollSource::Touch,
            5 => ScrollSource::Controller,
            _ => return None,
        };
        let phase = match ((ev.flags >> 8) & 0xff) as u8 {
            0 => ScrollPhase::None,
            1 => ScrollPhase::Begin,
            2 => ScrollPhase::Update,
            3 => ScrollPhase::End,
            4 => ScrollPhase::Cancel,
            5 => ScrollPhase::MomentumBegin,
            6 => ScrollPhase::Momentum,
            7 => ScrollPhase::MomentumEnd,
            _ => return None,
        };
        let se = ScrollEvent {
            source,
            phase,
            axis: ev.code,
            delta: ev.x,
        };
        // A stop carries no distance: capture splits any final displacement
        // into a movement phase before the close.
        if se.is_stop() && se.delta != 0 {
            return None;
        }
        // Detent counters carry no gesture state.
        if se.is_wheel() && phase != ScrollPhase::None {
            return None;
        }
        // Kinetic phases exist only where a surface can glide; a controller
        // emits its tail as ordinary deltas instead.
        if se.is_momentum()
            && !matches!(
                se.source,
                ScrollSource::Finger | ScrollSource::Continuous | ScrollSource::Touch
            )
        {
            return None;
        }
        Some(se)
    }

    /// Detent-counted source: `delta` is Q24.8 v120.
    pub fn is_wheel(&self) -> bool {
        matches!(self.source, ScrollSource::Wheel | ScrollSource::Unknown)
    }

    /// A kinetic phase the client computed — the host must not invent a second tail.
    pub fn is_momentum(&self) -> bool {
        matches!(
            self.phase,
            ScrollPhase::MomentumBegin | ScrollPhase::Momentum | ScrollPhase::MomentumEnd
        )
    }

    /// Gesture close: `End`, `Cancel`, or `MomentumEnd`. Always `delta == 0`.
    pub fn is_stop(&self) -> bool {
        matches!(
            self.phase,
            ScrollPhase::End | ScrollPhase::Cancel | ScrollPhase::MomentumEnd
        )
    }

    /// Unit `delta` carries.
    pub fn units(&self) -> ScrollUnits {
        if self.is_wheel() {
            ScrollUnits::V120
        } else {
            ScrollUnits::Dip
        }
    }
}

/// Quantizes floating-point capture deltas into wire integers.
///
/// Input `delta` is in the source's own unit (v120 for Wheel/Unknown, DIP for
/// the rest). The unsent fraction rides per axis; a source switch or a gesture
/// boundary clears it, so a wheel residue is never repriced as DIP and a new
/// gesture starts clean.
#[derive(Default)]
pub struct ScrollAccumulator {
    /// Unsent fraction per axis, in Q24.8 sub-units.
    rem: [f64; 2],
    /// Source of the last delta per axis; a switch clears `rem`.
    last: [Option<ScrollSource>; 2],
}

impl ScrollAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Quantize `delta` into a wire event. `None` on an invalid axis, a
    /// non-finite delta, a nonzero stop delta, an invalid source/phase
    /// combination, or a zero-quantized movement phase — boundary phases
    /// (`Begin`, `MomentumBegin` and the stops) still emit at zero distance so
    /// a gesture never loses its close. A rejected call changes no state.
    pub fn event(
        &mut self,
        source: ScrollSource,
        phase: ScrollPhase,
        axis: u32,
        delta: f64,
    ) -> Option<InputEvent> {
        if axis > 1 || !delta.is_finite() {
            return None;
        }
        let stop = matches!(
            phase,
            ScrollPhase::End | ScrollPhase::Cancel | ScrollPhase::MomentumEnd
        );
        // A stop carries no distance — reject before the residue can swallow
        // it, and run the wire validator on a zero-delta probe so the same
        // rules apply to the pair itself (a phased wheel drops here).
        if stop && delta != 0.0 {
            return None;
        }
        let probe = ScrollEvent {
            source,
            phase,
            axis,
            delta: 0,
        }
        .to_event();
        ScrollEvent::from_event(&probe)?;
        let a = axis as usize;
        // A boundary restarts the residue as surely as a source switch: a
        // missed stop cannot leak last gesture's fraction into the new one.
        let boundary = stop || matches!(phase, ScrollPhase::Begin | ScrollPhase::MomentumBegin);
        if self.last[a] != Some(source) || boundary {
            self.rem[a] = 0.0;
        }
        self.last[a] = Some(source);
        let q = if stop {
            0i32
        } else {
            // Clamp before the split so a huge delta saturates instead of
            // leaving an unreachable residue.
            let total = (delta * SCROLL_SCALE + self.rem[a])
                .clamp(f64::from(i32::MIN), f64::from(i32::MAX));
            let out = total.trunc();
            self.rem[a] = total - out;
            out as i32
        };
        if q == 0 && !boundary {
            return None;
        }
        Some(
            ScrollEvent {
                source,
                phase,
                axis,
                delta: q,
            }
            .to_event(),
        )
    }
}

/// The one outbound scroll seam: validates a normalized event, applies the
/// invert toggle once, and rewrites it to the legacy [`InputKind::MouseScroll`]
/// wire when the host never advertised `HOST_CAP2_SCROLL`.
///
/// Legacy conversion keeps the unsent integer fraction per axis — consecutive
/// sub-detent deltas still reach a click — cleared on a source switch or
/// gesture boundary. The residue holds raw (uninverted) deltas and the toggle
/// flips only what goes out, so a mid-session invert change cannot strand
/// stale-sign residue. The old wire carries no phases: a stop produces no
/// event, and momentum and `Begin`/`Update` deltas degrade to ordinary scroll
/// distance.
pub struct ScrollOutput {
    /// Host advertised `HOST_CAP2_SCROLL`.
    normalized: bool,
    /// Unsent fraction per axis, in legacy `x` units (v120; DIP ×12).
    rem: [f64; 2],
    /// Source of the last converted delta per axis; a switch clears `rem`.
    last: [Option<ScrollSource>; 2],
}

impl ScrollOutput {
    /// `normalized` = the host advertised `HOST_CAP2_SCROLL` in `Welcome`.
    pub fn new(normalized: bool) -> Self {
        ScrollOutput {
            normalized,
            rem: [0.0; 2],
            last: [None; 2],
        }
    }

    /// Next wire event, or `None` when nothing goes out: a malformed normalized
    /// event, a dropped phase, or a zero converted delta.
    pub fn prepare(&mut self, event: InputEvent, invert: bool) -> Option<InputEvent> {
        let mut event = match event.kind {
            InputKind::Scroll => {
                let se = ScrollEvent::from_event(&event)?;
                if self.normalized {
                    event
                } else {
                    self.legacy(se)?
                }
            }
            _ => event,
        };
        if invert && matches!(event.kind, InputKind::Scroll | InputKind::MouseScroll) {
            event.x = event.x.saturating_neg();
        }
        Some(event)
    }

    /// `MouseScroll` equivalent of `se`, or `None` when the old wire cannot say
    /// it (a stop phase) or there is nothing to send.
    fn legacy(&mut self, se: ScrollEvent) -> Option<InputEvent> {
        let a = se.axis as usize;
        // A gesture boundary restarts the residue as surely as a source switch.
        let boundary = matches!(se.phase, ScrollPhase::Begin | ScrollPhase::MomentumBegin);
        if self.last[a] != Some(se.source) || boundary {
            self.rem[a] = 0.0;
        }
        self.last[a] = Some(se.source);
        if se.is_stop() {
            self.rem[a] = 0.0;
            return None;
        }
        // v120 goes out as a counted delta; DIP re-prices into the same
        // 120-space with the precise bit (`x / 12` is the measured distance —
        // see `PRECISE_PX_PER_DETENT`).
        let total = match se.units() {
            ScrollUnits::V120 => f64::from(se.delta) / SCROLL_SCALE + self.rem[a],
            ScrollUnits::Dip => {
                f64::from(se.delta) / SCROLL_SCALE * (120.0 / PRECISE_PX_PER_DETENT) + self.rem[a]
            }
        };
        // Clamp before the split, same saturation rule as `ScrollAccumulator`.
        let total = total.clamp(f64::from(i32::MIN), f64::from(i32::MAX));
        let out = total.trunc();
        self.rem[a] = total - out;
        if out == 0.0 {
            return None;
        }
        Some(InputEvent {
            kind: InputKind::MouseScroll,
            _pad: [0; 3],
            code: se.axis,
            x: out as i32,
            y: 0,
            flags: if se.is_wheel() {
                0
            } else {
                SCROLL_FLAG_PRECISE
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(source: ScrollSource, phase: ScrollPhase, axis: u32, delta: i32) -> InputEvent {
        ScrollEvent {
            source,
            phase,
            axis,
            delta,
        }
        .to_event()
    }

    #[test]
    fn tag16_roundtrip() {
        let e = ev(ScrollSource::Finger, ScrollPhase::Update, 0, -(7 * 256));
        assert_eq!(e.kind as u8, 16);
        let wire = e.encode();
        assert_eq!(wire[1], 16);
        let back = InputEvent::decode(&wire).unwrap();
        assert_eq!(back, e);
        let se = ScrollEvent::from_event(&back).unwrap();
        assert_eq!(
            (se.source, se.phase, se.axis, se.delta),
            (ScrollSource::Finger, ScrollPhase::Update, 0, -(7 * 256))
        );
    }

    #[test]
    fn all_sources_and_units_roundtrip() {
        for (i, source) in [
            ScrollSource::Unknown,
            ScrollSource::Wheel,
            ScrollSource::Finger,
            ScrollSource::Continuous,
            ScrollSource::Touch,
            ScrollSource::Controller,
        ]
        .into_iter()
        .enumerate()
        {
            let phase = if i <= 1 {
                ScrollPhase::None
            } else {
                ScrollPhase::Update
            };
            let e = ev(source, phase, 1, i as i32 + 1);
            let se = ScrollEvent::from_event(&e).unwrap();
            assert_eq!(se.source, source);
            assert_eq!(
                se.units(),
                if se.is_wheel() {
                    ScrollUnits::V120
                } else {
                    ScrollUnits::Dip
                }
            );
        }
    }

    #[test]
    fn horizontal_and_negative_carry() {
        let e = ev(ScrollSource::Wheel, ScrollPhase::None, 1, -(120 * 256));
        let se = ScrollEvent::from_event(&e).unwrap();
        assert_eq!((se.axis, se.delta), (1, -(120 * 256)));
    }

    #[test]
    fn malformed_bodies_reject() {
        let base = ev(ScrollSource::Wheel, ScrollPhase::None, 0, 120 * 256);
        // Bad axis.
        assert!(ScrollEvent::from_event(&InputEvent { code: 2, ..base }).is_none());
        // Nonzero y.
        assert!(ScrollEvent::from_event(&InputEvent { y: 1, ..base }).is_none());
        // Reserved flag bits.
        assert!(ScrollEvent::from_event(&InputEvent {
            flags: base.flags | 0x1_0000,
            ..base
        })
        .is_none());
        // Out-of-range source/phase bytes.
        assert!(ScrollEvent::from_event(&InputEvent { flags: 6, ..base }).is_none());
        assert!(ScrollEvent::from_event(&InputEvent {
            flags: 1 | (8 << 8),
            ..base
        })
        .is_none());
        // A wheel may not carry a phase.
        assert!(
            ScrollEvent::from_event(&ev(ScrollSource::Wheel, ScrollPhase::Begin, 0, 0)).is_none()
        );
        // A stop may not carry distance.
        assert!(
            ScrollEvent::from_event(&ev(ScrollSource::Finger, ScrollPhase::End, 0, 256)).is_none()
        );
        // Momentum needs a gliding surface: not Controller, not Wheel.
        assert!(ScrollEvent::from_event(&ev(
            ScrollSource::Controller,
            ScrollPhase::Momentum,
            0,
            256
        ))
        .is_none());
        assert!(
            ScrollEvent::from_event(&ev(ScrollSource::Wheel, ScrollPhase::Momentum, 0, 256))
                .is_none()
        );
        // Wrong tag.
        assert!(ScrollEvent::from_event(&InputEvent {
            kind: InputKind::MouseScroll,
            ..base
        })
        .is_none());
        // Decode applies the same validation.
        let mut wire = base.encode();
        wire[10] = 1; // y = 1
        assert!(InputEvent::decode(&wire).is_none());
    }

    #[test]
    fn decode_accepts_valid_scroll() {
        let e = ev(ScrollSource::Touch, ScrollPhase::Begin, 1, 0);
        assert_eq!(InputEvent::decode(&e.encode()), Some(e));
    }

    #[test]
    fn accumulator_quantizes_and_carries_fraction() {
        let mut acc = ScrollAccumulator::new();
        // 0.502 v120 each: 128 wire + 0.512 residue — the second event carries.
        let a = acc
            .event(ScrollSource::Wheel, ScrollPhase::None, 0, 0.502)
            .unwrap();
        let b = acc
            .event(ScrollSource::Wheel, ScrollPhase::None, 0, 0.502)
            .unwrap();
        assert_eq!(a.x, 128);
        assert_eq!(b.x, 129);
        // Axes accumulate independently; sign truncates toward zero.
        let h = acc
            .event(ScrollSource::Wheel, ScrollPhase::None, 1, -0.4)
            .unwrap();
        assert_eq!(h.x, -102);
        let h2 = acc
            .event(ScrollSource::Wheel, ScrollPhase::None, 1, -0.4)
            .unwrap();
        assert_eq!(h2.x, -102); // -102.4 - 0.4 rem → -102.8 → -102
    }

    #[test]
    fn accumulator_rejects_bad_input_and_never_quantizes_nan() {
        let mut acc = ScrollAccumulator::new();
        assert!(acc
            .event(ScrollSource::Wheel, ScrollPhase::None, 2, 1.0)
            .is_none());
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(acc
                .event(ScrollSource::Wheel, ScrollPhase::None, 0, bad)
                .is_none());
        }
        // NaN leaves no residue: the next honest delta is unaffected.
        let e = acc
            .event(ScrollSource::Wheel, ScrollPhase::None, 0, 120.0)
            .unwrap();
        assert_eq!(e.x, 120 * 256);
    }

    #[test]
    fn accumulator_boundaries_emit_zero_and_stops_reset() {
        let mut acc = ScrollAccumulator::new();
        acc.event(ScrollSource::Finger, ScrollPhase::Begin, 0, 0.6)
            .unwrap();
        acc.event(ScrollSource::Finger, ScrollPhase::Update, 0, 0.6)
            .unwrap();
        // End always emits, delta 0, and clears the residue: the next gesture
        // restarts from zero rather than the stale fraction.
        let end = acc
            .event(ScrollSource::Finger, ScrollPhase::End, 0, 0.0)
            .unwrap();
        assert_eq!((end.x, end.flags >> 8), (0, ScrollPhase::End as u32));
        let next = acc
            .event(ScrollSource::Finger, ScrollPhase::Begin, 0, 0.4)
            .unwrap();
        assert_eq!(next.x, 102); // 0.4 * 256, no carry from the ended gesture
                                 // Cancel emits at zero too.
        assert!(acc
            .event(ScrollSource::Finger, ScrollPhase::Cancel, 0, 0.0)
            .is_some());
    }

    #[test]
    fn accumulator_rejects_before_touching_residue() {
        let mut acc = ScrollAccumulator::new();
        // Two 0.9-v120 deltas: 0.8 of a wire unit held.
        acc.event(ScrollSource::Wheel, ScrollPhase::None, 0, 0.9)
            .unwrap();
        acc.event(ScrollSource::Wheel, ScrollPhase::None, 0, 0.9)
            .unwrap();
        // A phased wheel and a stop carrying distance are both invalid —
        // rejected before any state moves.
        assert!(acc
            .event(ScrollSource::Wheel, ScrollPhase::Begin, 0, 0.0)
            .is_none());
        assert!(acc
            .event(ScrollSource::Wheel, ScrollPhase::End, 0, 0.5)
            .is_none());
        // The residue survived: 0.9 more reaches 231.2 → 231, not a clean 230.
        let e = acc
            .event(ScrollSource::Wheel, ScrollPhase::None, 0, 0.9)
            .unwrap();
        assert_eq!(e.x, 231);
    }

    #[test]
    fn accumulator_begin_resets_same_source_residue() {
        let mut acc = ScrollAccumulator::new();
        acc.event(ScrollSource::Finger, ScrollPhase::Begin, 0, 0.9)
            .unwrap();
        acc.event(ScrollSource::Finger, ScrollPhase::Update, 0, 0.9)
            .unwrap();
        // rem is 0.8 of a wire unit; the next gesture's Begin restarts it —
        // 0.9 alone is 230.4 → 230, not 231.2 → 231.
        let e = acc
            .event(ScrollSource::Finger, ScrollPhase::Begin, 0, 0.9)
            .unwrap();
        assert_eq!(e.x, 230);
    }

    #[test]
    fn accumulator_source_switch_does_not_reprice_residue() {
        let mut acc = ScrollAccumulator::new();
        // 0.9 v120: 230 wire, 0.4 residue held on the axis.
        let w = acc
            .event(ScrollSource::Wheel, ScrollPhase::None, 0, 0.9)
            .unwrap();
        assert_eq!(w.x, 230);
        // A DIP source does not inherit it: 10.6 DIP → 2713, not 2714.
        let d = acc
            .event(ScrollSource::Finger, ScrollPhase::Update, 0, 10.6)
            .unwrap();
        assert_eq!(d.x, 2713);
        // Nor does the wheel on return: 0.6 v120 → 153, not 154.
        let w2 = acc
            .event(ScrollSource::Wheel, ScrollPhase::None, 0, 0.6)
            .unwrap();
        assert_eq!(w2.x, 153);
    }

    #[test]
    fn accumulator_huge_delta_saturates() {
        let mut acc = ScrollAccumulator::new();
        let e = acc
            .event(ScrollSource::Wheel, ScrollPhase::None, 0, 1e15)
            .unwrap();
        assert_eq!(e.x, i32::MAX);
        let e = acc
            .event(ScrollSource::Wheel, ScrollPhase::None, 0, -1e15)
            .unwrap();
        assert_eq!(e.x, i32::MIN);
    }

    fn out(normalized: bool) -> ScrollOutput {
        ScrollOutput::new(normalized)
    }

    #[test]
    fn normalized_host_passes_scroll_through() {
        let mut o = out(true);
        let e = ev(ScrollSource::Finger, ScrollPhase::Update, 0, 10 * 256);
        assert_eq!(o.prepare(e, false), Some(e));
        // Inverted: only the delta flips.
        let got = o.prepare(e, true).unwrap();
        assert_eq!(got.x, -(10 * 256));
        assert_eq!(
            got,
            ev(ScrollSource::Finger, ScrollPhase::Update, 0, -(10 * 256))
        );
        // i32::MIN cannot negate; saturate instead of wrapping.
        let min = ev(ScrollSource::Wheel, ScrollPhase::None, 0, i32::MIN);
        assert_eq!(o.prepare(min, true).unwrap().x, i32::MAX);
        // Malformed drops.
        assert!(o.prepare(InputEvent { code: 9, ..e }, false).is_none());
    }

    #[test]
    fn old_host_wheel_120() {
        let mut o = out(false);
        let got = o
            .prepare(
                ev(ScrollSource::Wheel, ScrollPhase::None, 0, 120 * 256),
                false,
            )
            .unwrap();
        assert_eq!(
            (got.kind, got.code, got.x, got.flags),
            (InputKind::MouseScroll, 0, 120, 0)
        );
    }

    #[test]
    fn old_host_finger_dip_becomes_precise() {
        let mut o = out(false);
        let got = o
            .prepare(
                ev(ScrollSource::Finger, ScrollPhase::Update, 0, 10 * 256),
                false,
            )
            .unwrap();
        // 10 DIP = 120 v120 measured units, flagged precise.
        assert_eq!(
            (got.kind, got.code, got.x, got.flags),
            (InputKind::MouseScroll, 0, 120, SCROLL_FLAG_PRECISE)
        );
    }

    #[test]
    fn old_host_fractional_deltas_accumulate() {
        let mut o = out(false);
        // 0.5 v120 four times → one unit every other event.
        let mut total = 0;
        for _ in 0..4 {
            if let Some(g) = o.prepare(
                ev(ScrollSource::Wheel, ScrollPhase::None, 0, 128), // 0.5 v120
                false,
            ) {
                total += g.x;
            }
        }
        assert_eq!(total, 2);
    }

    #[test]
    fn old_host_invert_flip_keeps_raw_residue() {
        let mut o = out(false);
        // 0.75 v120 held raw; a live invert flips only what goes out.
        assert!(o
            .prepare(ev(ScrollSource::Wheel, ScrollPhase::None, 0, 192), false)
            .is_none());
        let g = o
            .prepare(ev(ScrollSource::Wheel, ScrollPhase::None, 0, 128), true)
            .unwrap();
        assert_eq!(g.x, -1);
        // The 0.25 remainder kept its raw sign: un-inverting emits forward.
        let g = o
            .prepare(ev(ScrollSource::Wheel, ScrollPhase::None, 0, 192), false)
            .unwrap();
        assert_eq!(g.x, 1);
    }

    #[test]
    fn old_host_begin_resets_residue() {
        let mut o = out(false);
        // Two 2.39-v120 finger deltas: 2 out each, 0.78 residue held.
        for _ in 0..2 {
            assert_eq!(
                o.prepare(ev(ScrollSource::Finger, ScrollPhase::Update, 0, 51), false)
                    .unwrap()
                    .x,
                2
            );
        }
        // A Begin restarts the gesture: its delta quantizes alone (2.39 → 2),
        // not against the stale 0.78 (which would have made 3).
        assert_eq!(
            o.prepare(ev(ScrollSource::Finger, ScrollPhase::Begin, 0, 51), false)
                .unwrap()
                .x,
            2
        );
    }

    #[test]
    fn old_host_source_switch_and_axis_state_are_separate() {
        let mut o = out(false);
        assert!(o
            .prepare(ev(ScrollSource::Wheel, ScrollPhase::None, 0, 128), false)
            .is_none()); // 0.5 held
                         // Finger on the same axis does not inherit the wheel residue.
        let g = o
            .prepare(
                ev(ScrollSource::Finger, ScrollPhase::Update, 0, 10 * 256),
                false,
            )
            .unwrap();
        assert_eq!(g.x, 120);
        // Horizontal kept its own state all along.
        assert!(o
            .prepare(ev(ScrollSource::Wheel, ScrollPhase::None, 1, 128), false)
            .is_none());
        let h = o
            .prepare(ev(ScrollSource::Wheel, ScrollPhase::None, 1, 128), false)
            .unwrap();
        assert_eq!(h.x, 1);
    }

    #[test]
    fn old_host_stop_drops_and_resets() {
        let mut o = out(false);
        // ~0.05 DIP ≈ 0.56 v120: held as residue, nothing emitted.
        let half = (0.05 * SCROLL_SCALE) as i32;
        assert!(o
            .prepare(
                ev(ScrollSource::Finger, ScrollPhase::Update, 0, half),
                false
            )
            .is_none());
        // The Cancel emits nothing (the old wire has no phases) and clears the
        // residue: the same delta after it emits nothing again, not a unit.
        assert!(o
            .prepare(ev(ScrollSource::Finger, ScrollPhase::Cancel, 0, 0), false)
            .is_none());
        assert!(o
            .prepare(
                ev(ScrollSource::Finger, ScrollPhase::Update, 0, half),
                false
            )
            .is_none());
        // Without the reset this second pair would have reached a unit — and
        // the next one does, proving the residue only restarted.
        assert_eq!(
            o.prepare(
                ev(ScrollSource::Finger, ScrollPhase::Update, 0, half),
                false
            )
            .unwrap()
            .x,
            1
        );
    }

    #[test]
    fn old_host_momentum_and_begin_degrade_to_distance() {
        let mut o = out(false);
        let g = o
            .prepare(
                ev(
                    ScrollSource::Continuous,
                    ScrollPhase::Momentum,
                    1,
                    -(5 * 256),
                ),
                false,
            )
            .unwrap();
        assert_eq!((g.code, g.x, g.flags), (1, -60, SCROLL_FLAG_PRECISE));
        // MomentumEnd is a stop: no wire event.
        assert!(o
            .prepare(
                ev(ScrollSource::Continuous, ScrollPhase::MomentumEnd, 1, 0),
                false
            )
            .is_none());
    }

    #[test]
    fn legacy_mousescroll_inverts_and_passes() {
        let mut o = out(false);
        let e = InputEvent {
            kind: InputKind::MouseScroll,
            _pad: [0; 3],
            code: 1,
            x: -240,
            y: 0,
            flags: SCROLL_FLAG_PRECISE,
        };
        assert_eq!(o.prepare(e, false), Some(e)); // byte-identical
        assert_eq!(o.prepare(e, true).unwrap().x, 240);
        // i32::MIN saturates rather than wrapping to itself.
        let min = InputEvent { x: i32::MIN, ..e };
        assert_eq!(o.prepare(min, true).unwrap().x, i32::MAX);
        // Other kinds are untouched either way.
        let key = InputEvent {
            kind: InputKind::KeyDown,
            ..e
        };
        assert_eq!(o.prepare(key, true), Some(key));
    }

    /// `testdata/scroll-vectors.json`: capture sequences and what [`ScrollAccumulator`]
    /// puts on the wire for each step. Sources and phases are their wire numbers.
    fn scroll_vectors() -> String {
        use ScrollPhase::*;
        use ScrollSource::*;
        type Step = (ScrollSource, ScrollPhase, u32, f64);
        let cases: [(&str, &[Step]); 10] = [
            (
                "wheel detents",
                &[(Wheel, None, 0, 120.0), (Wheel, None, 0, -240.0)],
            ),
            ("hi-res wheel quarters", &[(Wheel, None, 0, 30.0); 4]),
            (
                "unknown carries its fraction",
                &[(Unknown, None, 0, 0.4); 3],
            ),
            (
                "finger gesture",
                &[
                    (Finger, Begin, 0, 1.5),
                    (Finger, Update, 0, 0.01),
                    (Finger, Update, 0, 0.001),
                    (Finger, End, 0, 0.0),
                ],
            ),
            (
                "momentum tail",
                &[
                    (Finger, MomentumBegin, 0, 3.3),
                    (Finger, Momentum, 0, 0.2),
                    (Finger, MomentumEnd, 0, 0.0),
                ],
            ),
            (
                "a source switch drops the residue",
                &[
                    (Continuous, None, 0, 0.003),
                    (Continuous, None, 0, 0.003),
                    (Touch, Update, 0, 0.003),
                ],
            ),
            (
                "a boundary drops the residue",
                &[(Touch, Update, 0, 0.003), (Touch, Begin, 0, 0.003)],
            ),
            ("horizontal", &[(Finger, Update, 1, -2.5)]),
            ("saturates", &[(Continuous, None, 0, 1e9)]),
            (
                "rejects",
                &[
                    (Finger, End, 0, 1.0),
                    (Wheel, Begin, 0, 120.0),
                    (Controller, Momentum, 0, 1.0),
                    (Finger, Update, 2, 1.0),
                ],
            ),
        ];
        let about = "Generated from punktfunk_core::input::scroll::ScrollAccumulator by \
            scroll_vectors_are_checked_in (UPDATE_VECTORS=1 rewrites it). The Kotlin \
            ScrollNormalizer test replays every case.";
        let mut out = format!("{{\n  \"$comment\": \"{about}\",\n  \"cases\": [\n");
        for (i, (name, steps)) in cases.iter().enumerate() {
            let mut acc = ScrollAccumulator::new();
            out += &format!("    {{\"name\": \"{name}\", \"steps\": [\n");
            for (j, &(source, phase, axis, delta)) in steps.iter().enumerate() {
                let wire = acc
                    .event(source, phase, axis, delta)
                    .map_or("null".into(), |e| {
                        let se = ScrollEvent::from_event(&e).unwrap();
                        format!(
                            "{{\"source\": {}, \"phase\": {}, \"axis\": {}, \"delta\": {}}}",
                            se.source as u8, se.phase as u8, se.axis, se.delta
                        )
                    });
                let comma = if j + 1 < steps.len() { "," } else { "" };
                out += &format!(
                    "      {{\"source\": {}, \"phase\": {}, \"axis\": {axis}, \"delta\": {delta:?}, \
                     \"wire\": {wire}}}{comma}\n",
                    source as u8, phase as u8
                );
            }
            let comma = if i + 1 < cases.len() { "," } else { "" };
            out += &format!("    ]}}{comma}\n");
        }
        out + "  ]\n}\n"
    }

    #[test]
    fn scroll_vectors_are_checked_in() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/scroll-vectors.json");
        let fresh = scroll_vectors();
        if std::env::var_os("UPDATE_VECTORS").is_some() {
            std::fs::write(path, &fresh).unwrap();
        }
        let on_disk = std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            on_disk == fresh,
            "{path} is stale: rerun with UPDATE_VECTORS=1"
        );
    }
}
