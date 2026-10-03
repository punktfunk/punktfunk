//! Translate Moonlight `SS_PEN` / `SS_TOUCH` into punktfunk [`PenSample`]s and wire-touch events.
//!
//! Pen: each packet merges over last state into one sample (`BUTTON_ONLY` has no
//! position by spec), then the [`PenSink`](crate::pen_sink::PenSink) both planes share.
//!
//! No stroke timeout. Native's 200 ms failsafe exists because datagrams are lossy
//! and clients heartbeat. Moonlight's ENet control stream is ordered and reliable,
//! and clients do not heartbeat a stationary pen — a timeout would lift a held
//! stroke. Lost-client cleanup is the ENet disconnect: the control loop rebuilds
//! this translator, and destroying the uinput device releases kernel-side state.
//!
//! Touch: `TouchDown` / `Move` / `Up` on a synthetic 65535² surface. Pressure and
//! area have no wire field. Spec: `design/pen-tablet-input.md`.

use super::input::{SsPen, SsPointer, SsTouch};
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::quic::{
    PenSample, PenTool, PEN_ANGLE_UNKNOWN, PEN_BARREL1, PEN_BARREL2, PEN_IN_RANGE,
    PEN_TILT_UNKNOWN, PEN_TOUCHING,
};

// moonlight-common-c Limelight.h event/tool/button vocabulary.
const LI_TOUCH_EVENT_HOVER: u8 = 0x00;
const LI_TOUCH_EVENT_DOWN: u8 = 0x01;
const LI_TOUCH_EVENT_UP: u8 = 0x02;
const LI_TOUCH_EVENT_MOVE: u8 = 0x03;
const LI_TOUCH_EVENT_CANCEL: u8 = 0x04;
const LI_TOUCH_EVENT_BUTTON_ONLY: u8 = 0x05;
const LI_TOUCH_EVENT_HOVER_LEAVE: u8 = 0x06;
const LI_TOUCH_EVENT_CANCEL_ALL: u8 = 0x07;
const LI_TOOL_TYPE_PEN: u8 = 0x01;
const LI_TOOL_TYPE_ERASER: u8 = 0x02;
const LI_PEN_BUTTON_PRIMARY: u8 = 0x01;
const LI_PEN_BUTTON_SECONDARY: u8 = 0x02;
const LI_ROT_UNKNOWN: u16 = 0xFFFF;
const LI_TILT_UNKNOWN: u8 = 0xFF;

/// Normalized touches map onto this full-range surface; injector rescales via `flags = (w << 16) | h`.
const TOUCH_SURFACE: u32 = 65535;

/// Cap of tracked ids for `CANCEL_ALL` replay. Extra ids still forward down/up; they miss cancel-all.
const MAX_TOUCH_IDS: usize = 32;

/// Per-session pen and touch translator. Recreated on disconnect; dropping the pen's
/// [`crate::inject::pen::VirtualPen`] releases uinput.
pub struct GsPointer {
    pen: crate::pen_sink::PenSink,
    seq: u16,
    /// Merge base for packets that omit fields (`BUTTON_ONLY` has no x/y/pressure/tilt/rotation).
    last: PenSample,
    /// This client sent hover, so `UP` is back-to-hover. Without hover, lift must leave range.
    saw_hover: bool,
    /// Tracked ids so `CANCEL_ALL` can replay per-id ups.
    touch_ids: Vec<u32>,
    /// One-shot: first SS_TOUCH is the breadcrumb if later stages stay silent.
    touch_seen: bool,
    /// The injector drops touch (wlroots), but the pen flag already told Moonlight not to
    /// emulate a mouse: the first finger drives the pointer and left button instead. Decided at
    /// the first touch, once the session's injector is known.
    touch_as_mouse: Option<bool>,
    /// gamescope reads no tablet, so there the pen is one more finger. Decided at the first
    /// pen packet, once the session's injector is known.
    pen_as_touch: Option<bool>,
}

impl GsPointer {
    pub fn new() -> GsPointer {
        GsPointer {
            pen: Default::default(),
            seq: 0,
            last: PenSample::default(),
            saw_hover: false,
            touch_ids: Vec::new(),
            touch_seen: false,
            touch_as_mouse: None,
            pen_as_touch: None,
        }
    }

    pub fn apply(&mut self, p: &SsPointer, sink: impl FnMut(InputEvent)) {
        match p {
            SsPointer::Pen(pen) if *self.pen_as_touch.get_or_insert_with(pen_misses_session) => {
                self.apply_touch(&pen_contact(pen), sink)
            }
            SsPointer::Pen(pen) => self.apply_pen(pen),
            SsPointer::Touch(touch) => self.apply_touch(touch, sink),
        }
    }

    fn apply_pen(&mut self, p: &SsPen) {
        let Some(sample) = self.pen_sample(p) else {
            return;
        };
        self.last = sample;
        let batch = punktfunk_core::quic::PenBatch::new(self.seq, &[sample]);
        self.seq = self.seq.wrapping_add(1);
        self.pen.apply(&batch);
    }

    /// Merge one edge packet over last state. `None` if `BUTTON_ONLY` arrives before any position.
    fn pen_sample(&mut self, p: &SsPen) -> Option<PenSample> {
        let buttons = (if p.buttons & LI_PEN_BUTTON_PRIMARY != 0 {
            PEN_BARREL1
        } else {
            0
        }) | (if p.buttons & LI_PEN_BUTTON_SECONDARY != 0 {
            PEN_BARREL2
        } else {
            0
        });
        let tool = match p.tool {
            LI_TOOL_TYPE_PEN => PenTool::Pen,
            LI_TOOL_TYPE_ERASER => PenTool::Eraser,
            _ => PenTool::Unknown,
        };
        if p.event_type == LI_TOUCH_EVENT_BUTTON_ONLY {
            if !self.pen.active() {
                return None;
            }
            let mut s = self.last;
            s.state = (s.state & !(PEN_BARREL1 | PEN_BARREL2)) | buttons;
            return Some(s);
        }

        let mut s = PenSample {
            state: buttons,
            tool,
            x: p.x.clamp(0.0, 1.0),
            y: p.y.clamp(0.0, 1.0),
            dt_us: 0,
            roll_deg: PEN_ANGLE_UNKNOWN, // the GameStream wire has no barrel-roll axis
            azimuth_deg: if p.rotation == LI_ROT_UNKNOWN {
                PEN_ANGLE_UNKNOWN
            } else {
                p.rotation % 360
            },
            tilt_deg: if p.tilt == LI_TILT_UNKNOWN {
                PEN_TILT_UNKNOWN
            } else {
                p.tilt.min(90)
            },
            ..PenSample::default()
        };
        match p.event_type {
            LI_TOUCH_EVENT_DOWN | LI_TOUCH_EVENT_MOVE => {
                s.state |= PEN_IN_RANGE | PEN_TOUCHING;
                // 0.0 in contact is UNKNOWN, not zero — binary-stylus clients must still ink.
                s.pressure = if p.pressure_or_distance <= 0.0 {
                    u16::MAX
                } else {
                    (p.pressure_or_distance.clamp(0.0, 1.0) * 65535.0) as u16
                };
                s.distance = 0;
            }
            LI_TOUCH_EVENT_HOVER => {
                self.saw_hover = true;
                s.state |= PEN_IN_RANGE;
                // Hovering: pressureOrDistance is distance, 1.0 = farthest.
                s.distance = (p.pressure_or_distance.clamp(0.0, 1.0) * 65534.0) as u16;
            }
            LI_TOUCH_EVENT_UP => {
                // Hover clients keep proximity (HOVER_LEAVE exits); others would park the pen forever.
                if self.saw_hover {
                    s.state |= PEN_IN_RANGE;
                }
            }
            LI_TOUCH_EVENT_HOVER_LEAVE | LI_TOUCH_EVENT_CANCEL | LI_TOUCH_EVENT_CANCEL_ALL => {
                s.state &= !(PEN_BARREL1 | PEN_BARREL2); // out of range releases buttons too
            }
            _ => return None, // unknown future event type — drop, never guess
        }
        Some(s)
    }

    fn apply_touch(&mut self, t: &SsTouch, mut sink: impl FnMut(InputEvent)) {
        if !self.touch_seen {
            self.touch_seen = true;
            tracing::info!(
                event_type = t.event_type,
                "gamestream: touch plane active (first SS_TOUCH from this client)"
            );
        }
        let ev = |kind: InputKind, id: u32, x: f32, y: f32| InputEvent {
            kind,
            _pad: [0; 3],
            code: id,
            x: (x.clamp(0.0, 1.0) * TOUCH_SURFACE as f32) as i32,
            y: (y.clamp(0.0, 1.0) * TOUCH_SURFACE as f32) as i32,
            flags: (TOUCH_SURFACE << 16) | TOUCH_SURFACE,
        };
        if *self
            .touch_as_mouse
            .get_or_insert_with(|| !crate::inject::touch_supported())
        {
            // Moonlight's left button is 1. `touch_ids` holds only the finger that owns the pointer.
            let owner = self.touch_ids.first() == Some(&t.pointer_id);
            let (x, y) = (t.x, t.y);
            match t.event_type {
                LI_TOUCH_EVENT_DOWN if self.touch_ids.is_empty() => {
                    self.touch_ids.push(t.pointer_id);
                    sink(ev(InputKind::MouseMoveAbs, 0, x, y));
                    sink(ev(InputKind::MouseButtonDown, 1, x, y));
                }
                LI_TOUCH_EVENT_MOVE if owner => sink(ev(InputKind::MouseMoveAbs, 0, x, y)),
                LI_TOUCH_EVENT_UP | LI_TOUCH_EVENT_CANCEL if owner => {
                    self.touch_ids.clear();
                    sink(ev(InputKind::MouseButtonUp, 1, x, y));
                }
                LI_TOUCH_EVENT_CANCEL_ALL if !self.touch_ids.is_empty() => {
                    self.touch_ids.clear();
                    sink(ev(InputKind::MouseButtonUp, 1, 0.0, 0.0));
                }
                _ => {}
            }
            return;
        }
        match t.event_type {
            LI_TOUCH_EVENT_DOWN => {
                if !self.touch_ids.contains(&t.pointer_id) && self.touch_ids.len() < MAX_TOUCH_IDS {
                    self.touch_ids.push(t.pointer_id);
                }
                sink(ev(InputKind::TouchDown, t.pointer_id, t.x, t.y));
            }
            LI_TOUCH_EVENT_MOVE => sink(ev(InputKind::TouchMove, t.pointer_id, t.x, t.y)),
            LI_TOUCH_EVENT_UP | LI_TOUCH_EVENT_CANCEL => {
                self.touch_ids.retain(|&id| id != t.pointer_id);
                sink(ev(InputKind::TouchUp, t.pointer_id, t.x, t.y));
            }
            LI_TOUCH_EVENT_CANCEL_ALL => {
                for id in self.touch_ids.drain(..) {
                    sink(ev(InputKind::TouchUp, id, 0.0, 0.0));
                }
            }
            // Touch hover has no wire kind; BUTTON_ONLY is pen-only. Nothing to forward.
            _ => {}
        }
    }
}

/// Touch id the pen takes when it lands as touch.
const PEN_CONTACT_ID: u32 = u32::MAX;

/// The pen as a contact. Touch has no hover or button-only kind, so those drop.
fn pen_contact(p: &SsPen) -> SsTouch {
    SsTouch {
        event_type: p.event_type,
        rotation: p.rotation,
        pointer_id: PEN_CONTACT_ID,
        x: p.x,
        y: p.y,
        pressure_or_distance: p.pressure_or_distance,
    }
}

/// This plane injects through the shared backend, so it names the session's compositor.
#[cfg(target_os = "linux")]
fn pen_misses_session() -> bool {
    crate::inject::default_backend() == crate::inject::Backend::GamescopeEi
}

#[cfg(not(target_os = "linux"))]
fn pen_misses_session() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pen(event_type: u8, x: f32, y: f32, pod: f32) -> SsPen {
        SsPen {
            event_type,
            tool: LI_TOOL_TYPE_PEN,
            buttons: 0,
            x,
            y,
            pressure_or_distance: pod,
            rotation: 270,
            tilt: 40,
        }
    }

    #[test]
    fn pen_down_move_up_maps_to_state_full_samples() {
        let mut g = GsPointer::new();
        let s = g
            .pen_sample(&pen(LI_TOUCH_EVENT_DOWN, 0.25, 0.5, 0.5))
            .unwrap();
        assert_eq!(s.state, PEN_IN_RANGE | PEN_TOUCHING);
        assert_eq!(s.pressure, 32767);
        assert_eq!((s.tilt_deg, s.azimuth_deg), (40, 270));
        assert_eq!(s.roll_deg, PEN_ANGLE_UNKNOWN);
        let s = g
            .pen_sample(&pen(LI_TOUCH_EVENT_MOVE, 0.3, 0.5, 0.0))
            .unwrap();
        assert_eq!(s.pressure, u16::MAX);
        let s = g
            .pen_sample(&pen(LI_TOUCH_EVENT_UP, 0.3, 0.5, 0.0))
            .unwrap();
        assert_eq!(s.state, 0);
    }

    #[test]
    fn hover_capable_client_keeps_proximity_on_lift() {
        let mut g = GsPointer::new();
        let s = g
            .pen_sample(&pen(LI_TOUCH_EVENT_HOVER, 0.2, 0.2, 0.5))
            .unwrap();
        assert_eq!(s.state, PEN_IN_RANGE);
        assert_eq!(s.distance, 32767);
        let s = g
            .pen_sample(&pen(LI_TOUCH_EVENT_UP, 0.2, 0.2, 0.0))
            .unwrap();
        assert_eq!(s.state, PEN_IN_RANGE);
        let s = g
            .pen_sample(&pen(LI_TOUCH_EVENT_HOVER_LEAVE, 0.2, 0.2, 0.0))
            .unwrap();
        assert_eq!(s.state, 0);
    }

    #[test]
    fn button_only_merges_over_last_state() {
        let mut g = GsPointer::new();
        let mut b = pen(LI_TOUCH_EVENT_BUTTON_ONLY, 0.0, 0.0, 0.0);
        b.buttons = LI_PEN_BUTTON_PRIMARY;
        assert!(g.pen_sample(&b).is_none());
        g.apply_pen(&pen(LI_TOUCH_EVENT_DOWN, 0.4, 0.6, 0.8));
        let s = g.pen_sample(&b).unwrap();
        assert_eq!(s.state & (PEN_BARREL1 | PEN_BARREL2), PEN_BARREL1);
        assert_eq!(s.state & PEN_TOUCHING, PEN_TOUCHING);
        assert!((s.x - 0.4).abs() < 1e-6);
        assert_eq!(s.pressure, (0.8f32 * 65535.0) as u16);
    }

    #[test]
    fn eraser_and_unknown_tools_map() {
        let mut g = GsPointer::new();
        let mut p = pen(LI_TOUCH_EVENT_DOWN, 0.5, 0.5, 1.0);
        p.tool = LI_TOOL_TYPE_ERASER;
        assert_eq!(g.pen_sample(&p).unwrap().tool, PenTool::Eraser);
        p.tool = 0x7F;
        assert_eq!(g.pen_sample(&p).unwrap().tool, PenTool::Unknown);
        p.rotation = LI_ROT_UNKNOWN;
        p.tilt = LI_TILT_UNKNOWN;
        let s = g.pen_sample(&p).unwrap();
        assert_eq!(s.azimuth_deg, PEN_ANGLE_UNKNOWN);
        assert_eq!(s.tilt_deg, PEN_TILT_UNKNOWN);
    }

    #[test]
    fn touch_without_an_injector_drives_the_pointer_with_the_first_finger() {
        let mut g = GsPointer {
            touch_as_mouse: Some(true),
            ..GsPointer::new()
        };
        let mut got: Vec<InputEvent> = Vec::new();
        let t = |event_type, pointer_id| SsTouch {
            event_type,
            rotation: 0,
            pointer_id,
            x: 0.5,
            y: 0.5,
            pressure_or_distance: 0.5,
        };
        g.apply_touch(&t(LI_TOUCH_EVENT_DOWN, 7), |e| got.push(e));
        g.apply_touch(&t(LI_TOUCH_EVENT_DOWN, 9), |e| got.push(e)); // second finger: ignored
        g.apply_touch(&t(LI_TOUCH_EVENT_MOVE, 7), |e| got.push(e));
        g.apply_touch(&t(LI_TOUCH_EVENT_UP, 7), |e| got.push(e));
        let kinds: Vec<(InputKind, u32)> = got.iter().map(|e| (e.kind, e.code)).collect();
        assert_eq!(
            kinds,
            [
                (InputKind::MouseMoveAbs, 0),
                (InputKind::MouseButtonDown, 1),
                (InputKind::MouseMoveAbs, 0),
                (InputKind::MouseButtonUp, 1),
            ]
        );
        assert_eq!(got[0].x, 32767);
    }

    #[test]
    fn touch_forwards_wire_touch_and_cancel_all_replays_ups() {
        let mut g = GsPointer {
            touch_as_mouse: Some(false),
            ..GsPointer::new()
        };
        let mut got: Vec<InputEvent> = Vec::new();
        let t = |event_type, pointer_id, x: f32| SsTouch {
            event_type,
            rotation: 0,
            pointer_id,
            x,
            y: 0.5,
            pressure_or_distance: 0.5,
        };
        g.apply_touch(&t(LI_TOUCH_EVENT_DOWN, 7, 0.5), |e| got.push(e));
        g.apply_touch(&t(LI_TOUCH_EVENT_DOWN, 9, 0.25), |e| got.push(e));
        g.apply_touch(&t(LI_TOUCH_EVENT_MOVE, 7, 0.75), |e| got.push(e));
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].kind, InputKind::TouchDown);
        assert_eq!(got[0].code, 7);
        assert_eq!(got[0].x, 32767);
        assert_eq!(got[0].flags, (65535 << 16) | 65535);
        assert_eq!(got[2].kind, InputKind::TouchMove);
        assert_eq!(got[2].x, 49151);
        got.clear();
        g.apply_touch(&t(LI_TOUCH_EVENT_CANCEL_ALL, 0, 0.0), |e| got.push(e));
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|e| e.kind == InputKind::TouchUp));
        let mut ids: Vec<u32> = got.iter().map(|e| e.code).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![7, 9]);
        got.clear();
        g.apply_touch(&t(LI_TOUCH_EVENT_CANCEL_ALL, 0, 0.0), |e| got.push(e));
        assert!(got.is_empty());
    }

    #[test]
    fn pen_lands_as_touch_where_no_tablet_is_read() {
        let mut g = GsPointer {
            touch_as_mouse: Some(false),
            pen_as_touch: Some(true),
            ..GsPointer::new()
        };
        let mut got: Vec<InputEvent> = Vec::new();
        for ev in [
            LI_TOUCH_EVENT_HOVER,
            LI_TOUCH_EVENT_DOWN,
            LI_TOUCH_EVENT_MOVE,
            LI_TOUCH_EVENT_UP,
        ] {
            g.apply(&SsPointer::Pen(pen(ev, 0.5, 0.5, 0.5)), |e| got.push(e));
        }
        let kinds: Vec<(InputKind, u32)> = got.iter().map(|e| (e.kind, e.code)).collect();
        assert_eq!(
            kinds,
            [
                (InputKind::TouchDown, PEN_CONTACT_ID),
                (InputKind::TouchMove, PEN_CONTACT_ID),
                (InputKind::TouchUp, PEN_CONTACT_ID),
            ]
        );
        assert!(!g.pen.active(), "the tablet saw the pen");
    }
}
