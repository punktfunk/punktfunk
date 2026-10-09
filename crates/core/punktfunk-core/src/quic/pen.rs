//! Stylus batches on the 0xCC rich-input datagram (`RICH_PEN` = 0x05).
//!
//! Every sample is a complete snapshot (range, contact, buttons, tool, axes),
//! never an edge. [`PenTracker`] diffs against last state so a lost datagram
//! heals on the next sample; [`PEN_TOUCH_TIMEOUT_MS`] covers a client that dies
//! mid-stroke. Stale batches drop whole ([`pen_seq_newer`]) — a rewind would
//! jump the stroke.
//!
//! Send only after the host advertised [`HOST_CAP_PEN`](super::HOST_CAP_PEN).
//! Wire and injection: `design/pen-tablet-input.md`.

use super::datagram::{RICH_INPUT_MAGIC, RICH_PEN};

/// Implied by [`PEN_TOUCHING`]; [`PenTracker`] ORs it so a client that only sets TOUCHING still looks in-range.
pub const PEN_IN_RANGE: u8 = 0x01;
pub const PEN_TOUCHING: u8 = 0x02;
/// Primary barrel, or the client's squeeze mapping.
pub const PEN_BARREL1: u8 = 0x04;
/// Secondary barrel, or the client's double-tap mapping.
pub const PEN_BARREL2: u8 = 0x08;
/// Reserved: predicted sample. Never sent v1; [`PenTracker::apply`] skips it until a capability says otherwise.
pub const PEN_PREDICTED: u8 = 0x80;

const PEN_BUTTONS_MASK: u8 = PEN_BARREL1 | PEN_BARREL2;

pub const PEN_TILT_UNKNOWN: u8 = 0xFF;
pub const PEN_ANGLE_UNKNOWN: u16 = 0xFFFF;
pub const PEN_DISTANCE_UNKNOWN: u16 = 0xFFFF;

/// Coalesced capture at video-frame cadence: 240 Hz ÷ 30 fps = 8. More samples split across batches.
pub const PEN_BATCH_MAX: usize = 8;

pub const PEN_SAMPLE_WIRE_LEN: usize = 21;

/// `[0xCC][0x05][flags][count][u16 seq LE]` — bytes before the first sample.
const PEN_HEADER_LEN: usize = 6;

/// Force-release if still in range after this many ms with no sample (dead client).
/// Capture only fires on change, so senders repeat the last sample every ~100 ms while
/// in range — two heartbeats clear of this deadline. Repeats re-decode as Motion.
pub const PEN_TOUCH_TIMEOUT_MS: u32 = 200;

/// A tool switch while in range is a physical re-entry:
/// [`PenTracker`] emits a full release + re-proximity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum PenTool {
    #[default]
    Pen = 0,
    Eraser = 1,
    /// Unrecognized wire value. Injectors treat it as [`PenTool::Pen`]. Inside a
    /// proximity session it inherits the session tool instead of forcing re-entry.
    Unknown = 0xFF,
}

impl PenTool {
    fn from_u8(v: u8) -> PenTool {
        match v {
            0 => PenTool::Pen,
            1 => PenTool::Eraser,
            _ => PenTool::Unknown,
        }
    }
}

/// Unknown axes use their sentinel, never 0.
/// `x`/`y` are `0.0..=1.0` in video-frame space (client maps letterbox before send);
/// f32 keeps sub-pixel precision at any resolution.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PenSample {
    pub state: u8,
    pub tool: PenTool,
    pub x: f32,
    pub y: f32,
    /// `0` while hovering. Injectors rescale (Windows pens are 0..1024); full u16 keeps source precision.
    pub pressure: u16,
    /// `0` = hover floor. `0xFFFF` is unknown, so the live range is `0..=65534`.
    pub distance: u16,
    /// Polar form (what capture produces). Injectors that need tiltX/tiltY convert.
    pub tilt_deg: u8,
    pub azimuth_deg: u16,
    pub roll_deg: u16,
    /// `0` for the first sample in a batch. Preserves coalesced capture spacing for paced injectors.
    pub dt_us: u16,
}

impl Default for PenSample {
    fn default() -> PenSample {
        PenSample {
            state: 0,
            tool: PenTool::Pen,
            x: 0.0,
            y: 0.0,
            pressure: 0,
            distance: PEN_DISTANCE_UNKNOWN,
            tilt_deg: PEN_TILT_UNKNOWN,
            azimuth_deg: PEN_ANGLE_UNKNOWN,
            roll_deg: PEN_ANGLE_UNKNOWN,
            dt_us: 0,
        }
    }
}

impl PenSample {
    fn encode_into(&self, out: &mut Vec<u8>) {
        out.push(self.state);
        out.push(self.tool as u8);
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
        out.extend_from_slice(&self.pressure.to_le_bytes());
        out.extend_from_slice(&self.distance.to_le_bytes());
        out.push(self.tilt_deg);
        out.extend_from_slice(&self.azimuth_deg.to_le_bytes());
        out.extend_from_slice(&self.roll_deg.to_le_bytes());
        out.extend_from_slice(&self.dt_us.to_le_bytes());
    }

    /// `None` on a non-finite coordinate — NaN/∞ must never reach pixel scaling.
    /// Finite out-of-range values clamp to `0.0..=1.0` (a stroke past the letterbox is real).
    fn decode(b: &[u8]) -> Option<PenSample> {
        let f32at = |o: usize| f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let (x, y) = (f32at(2), f32at(6));
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        Some(PenSample {
            state: b[0],
            tool: PenTool::from_u8(b[1]),
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
            pressure: u16at(10),
            distance: u16at(12),
            tilt_deg: b[14],
            azimuth_deg: u16at(15),
            roll_deg: u16at(17),
            dt_us: u16at(19),
        })
    }
}

/// `[0xCC][0x05][flags][count][u16 seq LE]` + `count` × [`PEN_SAMPLE_WIRE_LEN`] samples, oldest first.
/// `flags` is reserved (sent 0, ignored) — a semantic change takes a new 0xCC kind.
/// `seq` is the wrapping reorder gate ([`pen_seq_newer`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PenBatch {
    pub seq: u16,
    count: u8,
    samples: [PenSample; PEN_BATCH_MAX],
}

impl PenBatch {
    pub fn new(seq: u16, samples: &[PenSample]) -> PenBatch {
        let count = samples.len().min(PEN_BATCH_MAX);
        let mut buf = [PenSample::default(); PEN_BATCH_MAX];
        buf[..count].copy_from_slice(&samples[..count]);
        PenBatch {
            seq,
            count: count as u8,
            samples: buf,
        }
    }

    pub fn samples(&self) -> &[PenSample] {
        &self.samples[..self.count as usize]
    }

    pub fn encode(&self) -> Vec<u8> {
        let n = self.count as usize;
        let mut out = Vec::with_capacity(PEN_HEADER_LEN + n * PEN_SAMPLE_WIRE_LEN);
        out.extend_from_slice(&[RICH_INPUT_MAGIC, RICH_PEN, 0, self.count]);
        out.extend_from_slice(&self.seq.to_le_bytes());
        for s in self.samples() {
            s.encode_into(&mut out);
        }
        out
    }

    /// `count` clamps to declared, [`PEN_BATCH_MAX`], and what the buffer holds —
    /// a torn datagram yields complete samples only, never an over-read.
    pub fn decode(b: &[u8]) -> Option<PenBatch> {
        if b.len() < PEN_HEADER_LEN || b[0] != RICH_INPUT_MAGIC || b[1] != RICH_PEN {
            return None;
        }
        let count = (b[3] as usize)
            .min(PEN_BATCH_MAX)
            .min((b.len() - PEN_HEADER_LEN) / PEN_SAMPLE_WIRE_LEN);
        if count == 0 {
            return None;
        }
        let mut samples = [PenSample::default(); PEN_BATCH_MAX];
        for (i, slot) in samples.iter_mut().enumerate().take(count) {
            let o = PEN_HEADER_LEN + i * PEN_SAMPLE_WIRE_LEN;
            *slot = PenSample::decode(&b[o..o + PEN_SAMPLE_WIRE_LEN])?;
        }
        Some(PenBatch {
            seq: u16::from_le_bytes([b[4], b[5]]),
            count: count as u8,
            samples,
        })
    }
}

/// Wrapping u16 analog of [`GamepadSnapshot::seq_newer`](crate::input::GamepadSnapshot::seq_newer):
/// newer ⇔ forward distance `1..=0x7FFF`. `None` (nothing applied yet) always passes.
pub fn pen_seq_newer(new: u16, last: Option<u16>) -> bool {
    match last {
        None => true,
        Some(last) => (new.wrapping_sub(last) as i16) > 0,
    }
}

/// Emission order for one sample: `ProximityIn?` → `Motion` → `TipDown?` →
/// `ButtonsChanged?` → `TipUp?` → `ProximityOut?`.
/// Motion before TipDown so contact lands at this sample; a release orders
/// `ButtonsChanged?` → `TipUp?` → `ProximityOut` so nothing is left held.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PenTransition {
    ProximityIn {
        tool: PenTool,
    },
    Motion {
        sample: PenSample,
    },
    TipDown,
    /// `pressed` / `released` are disjoint `PEN_BARREL*` subsets.
    ButtonsChanged {
        pressed: u8,
        released: u8,
    },
    TipUp,
    ProximityOut,
}

/// Clock-free: the owner arms [`PEN_TOUCH_TIMEOUT_MS`] over [`PenTracker::is_active`]
/// and calls [`PenTracker::force_release`] on fire and on session teardown.
#[derive(Debug, Default)]
pub struct PenTracker {
    last_seq: Option<u16>,
    in_range: bool,
    touching: bool,
    buttons: u8,
    tool: PenTool,
}

impl PenTracker {
    pub fn apply(&mut self, batch: &PenBatch, out: &mut Vec<PenTransition>) {
        if !pen_seq_newer(batch.seq, self.last_seq) {
            return;
        }
        self.last_seq = Some(batch.seq);
        for s in batch.samples() {
            if s.state & PEN_PREDICTED != 0 {
                continue;
            }
            self.apply_sample(s, out);
        }
    }

    /// A dead client could leave in-range / mid-stroke stuck.
    pub fn is_active(&self) -> bool {
        self.in_range || self.touching
    }

    /// Leaves the seq gate armed so a late stale datagram from the dead stroke cannot re-apply.
    pub fn force_release(&mut self, out: &mut Vec<PenTransition>) {
        if self.buttons != 0 {
            out.push(PenTransition::ButtonsChanged {
                pressed: 0,
                released: self.buttons,
            });
            self.buttons = 0;
        }
        if self.touching {
            out.push(PenTransition::TipUp);
            self.touching = false;
        }
        if self.in_range {
            out.push(PenTransition::ProximityOut);
            self.in_range = false;
        }
    }

    fn apply_sample(&mut self, s: &PenSample, out: &mut Vec<PenTransition>) {
        let touching = s.state & PEN_TOUCHING != 0;
        // Touching implies in-range. Normalize once so every consumer sees coherent state.
        let in_range = touching || s.state & PEN_IN_RANGE != 0;
        if !in_range {
            self.force_release(out);
            return;
        }
        // Unknown inherits the session tool; outside a session it grounds to default.
        let tool = match s.tool {
            PenTool::Unknown if self.in_range => self.tool,
            t => t,
        };
        // A tool switch mid-session is a physical re-entry (see [`PenTool`]).
        if self.in_range && tool != self.tool {
            self.force_release(out);
        }
        if !self.in_range {
            out.push(PenTransition::ProximityIn { tool });
            self.in_range = true;
        }
        self.tool = tool;
        out.push(PenTransition::Motion { sample: *s });
        if touching && !self.touching {
            out.push(PenTransition::TipDown);
            self.touching = true;
        }
        let buttons = s.state & PEN_BUTTONS_MASK;
        if buttons != self.buttons {
            out.push(PenTransition::ButtonsChanged {
                pressed: buttons & !self.buttons,
                released: self.buttons & !buttons,
            });
            self.buttons = buttons;
        }
        if !touching && self.touching {
            out.push(PenTransition::TipUp);
            self.touching = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::RichInput;

    fn hover(x: f32, y: f32) -> PenSample {
        PenSample {
            state: PEN_IN_RANGE,
            x,
            y,
            distance: 300,
            ..Default::default()
        }
    }

    fn touch(x: f32, y: f32, pressure: u16) -> PenSample {
        PenSample {
            state: PEN_IN_RANGE | PEN_TOUCHING,
            x,
            y,
            pressure,
            distance: 0,
            tilt_deg: 35,
            azimuth_deg: 180,
            roll_deg: 90,
            ..Default::default()
        }
    }

    #[test]
    fn pen_batch_roundtrip_and_truncation() {
        let samples = [hover(0.25, 0.5), touch(0.26, 0.5, 40000), {
            let mut s = touch(0.27, 0.51, 42000);
            s.state |= PEN_BARREL1;
            s.dt_us = 4167;
            s
        }];
        let b = PenBatch::new(7, &samples);
        let d = b.encode();
        assert_eq!(d[0], RICH_INPUT_MAGIC);
        assert_eq!(d.len(), 6 + 3 * PEN_SAMPLE_WIRE_LEN);
        let back = PenBatch::decode(&d).unwrap();
        assert_eq!(back.seq, 7);
        assert_eq!(back.samples(), &samples);

        let torn = PenBatch::decode(&d[..6 + 2 * PEN_SAMPLE_WIRE_LEN + 5]).unwrap();
        assert_eq!(torn.samples(), &samples[..2]);
        assert!(PenBatch::decode(&d[..PEN_HEADER_LEN]).is_none());
        assert!(PenBatch::decode(&PenBatch::new(0, &[]).encode()).is_none());
        let mut bad = d.clone();
        bad[0] = 0xC8;
        assert!(PenBatch::decode(&bad).is_none());
        let mut bad = d;
        bad[1] = 0x01; // RICH_TOUCHPAD
        assert!(PenBatch::decode(&bad).is_none());
    }

    #[test]
    fn pen_batch_oversize_truncates_and_flags_reserved() {
        let many: Vec<PenSample> = (0..10).map(|i| hover(i as f32 / 10.0, 0.5)).collect();
        let b = PenBatch::new(1, &many);
        assert_eq!(b.samples().len(), PEN_BATCH_MAX);
        let mut d = b.encode();
        d[3] = 200;
        assert_eq!(PenBatch::decode(&d).unwrap().samples().len(), PEN_BATCH_MAX);
        d[2] = 0xAA;
        assert!(PenBatch::decode(&d).is_some());
    }

    #[test]
    fn pen_batch_rejects_forged_floats_and_clamps_stragglers() {
        for forged in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut s = touch(0.5, 0.5, 1);
            s.x = forged;
            assert!(PenBatch::decode(&PenBatch::new(0, &[s]).encode()).is_none());
        }
        let mut s = hover(0.5, 0.5);
        s.x = -0.01;
        s.y = 1.25;
        let back = PenBatch::decode(&PenBatch::new(0, &[s]).encode()).unwrap();
        assert_eq!((back.samples()[0].x, back.samples()[0].y), (0.0, 1.0));
    }

    #[test]
    fn pen_plane_is_disjoint_from_rich_input() {
        let d = PenBatch::new(3, &[touch(0.5, 0.5, 100)]).encode();
        assert!(RichInput::decode(&d).is_none());
        let rich = RichInput::Touchpad {
            pad: 0,
            finger: 0,
            active: true,
            x: 1,
            y: 2,
        }
        .encode();
        assert!(PenBatch::decode(&rich).is_none());
    }

    #[test]
    fn seq_gate_wraps_and_drops_stale() {
        assert!(pen_seq_newer(0, None));
        assert!(pen_seq_newer(6, Some(5)));
        assert!(!pen_seq_newer(5, Some(5)));
        assert!(!pen_seq_newer(4, Some(5)));
        assert!(pen_seq_newer(2, Some(0xFFFE))); // wrap
        assert!(!pen_seq_newer(0xFFFE, Some(2)));
    }

    fn run(t: &mut PenTracker, seq: u16, samples: &[PenSample]) -> Vec<PenTransition> {
        let mut out = Vec::new();
        t.apply(&PenBatch::new(seq, samples), &mut out);
        out
    }

    #[test]
    fn tracker_full_stroke_lifecycle() {
        let mut t = PenTracker::default();
        let h = hover(0.2, 0.2);
        assert_eq!(
            run(&mut t, 0, &[h]),
            vec![
                PenTransition::ProximityIn { tool: PenTool::Pen },
                PenTransition::Motion { sample: h },
            ]
        );
        let c = touch(0.21, 0.2, 30000);
        assert_eq!(
            run(&mut t, 1, &[c]),
            vec![PenTransition::Motion { sample: c }, PenTransition::TipDown]
        );
        assert!(t.is_active());
        let m = touch(0.3, 0.25, 45000);
        assert_eq!(
            run(&mut t, 2, &[m]),
            vec![PenTransition::Motion { sample: m }]
        );
        let l = hover(0.3, 0.25);
        assert_eq!(
            run(&mut t, 3, &[l]),
            vec![PenTransition::Motion { sample: l }, PenTransition::TipUp]
        );
        let gone = PenSample::default(); // state 0 = out of range
        assert_eq!(run(&mut t, 4, &[gone]), vec![PenTransition::ProximityOut]);
        assert!(!t.is_active());
    }

    #[test]
    fn tracker_self_heals_lost_transitions() {
        let mut t = PenTracker::default();
        let m = touch(0.5, 0.5, 20000);
        assert_eq!(
            run(&mut t, 10, &[m]),
            vec![
                PenTransition::ProximityIn { tool: PenTool::Pen },
                PenTransition::Motion { sample: m },
                PenTransition::TipDown,
            ]
        );
        assert_eq!(
            run(&mut t, 11, &[PenSample::default()]),
            vec![PenTransition::TipUp, PenTransition::ProximityOut]
        );
    }

    #[test]
    fn tracker_drops_stale_batches_whole() {
        let mut t = PenTracker::default();
        let c = touch(0.5, 0.5, 100);
        assert!(!run(&mut t, 5, &[c]).is_empty());
        assert!(run(&mut t, 4, &[hover(0.4, 0.4)]).is_empty());
        assert!(t.is_active());
    }

    #[test]
    fn tracker_buttons_and_eraser_reentry() {
        let mut t = PenTracker::default();
        let mut held = touch(0.5, 0.5, 100);
        held.state |= PEN_BARREL1;
        assert_eq!(
            run(&mut t, 0, &[held]),
            vec![
                PenTransition::ProximityIn { tool: PenTool::Pen },
                PenTransition::Motion { sample: held },
                PenTransition::TipDown,
                PenTransition::ButtonsChanged {
                    pressed: PEN_BARREL1,
                    released: 0
                },
            ]
        );
        let mut swapped = held;
        swapped.state = (swapped.state & !PEN_BARREL1) | PEN_BARREL2;
        assert_eq!(
            run(&mut t, 1, &[swapped]),
            vec![
                PenTransition::Motion { sample: swapped },
                PenTransition::ButtonsChanged {
                    pressed: PEN_BARREL2,
                    released: PEN_BARREL1
                },
            ]
        );
        let mut erase = touch(0.5, 0.5, 200);
        erase.tool = PenTool::Eraser;
        assert_eq!(
            run(&mut t, 2, &[erase]),
            vec![
                PenTransition::ButtonsChanged {
                    pressed: 0,
                    released: PEN_BARREL2
                },
                PenTransition::TipUp,
                PenTransition::ProximityOut,
                PenTransition::ProximityIn {
                    tool: PenTool::Eraser
                },
                PenTransition::Motion { sample: erase },
                PenTransition::TipDown,
            ]
        );
        let mut unk = touch(0.51, 0.5, 210);
        unk.tool = PenTool::Unknown;
        assert_eq!(
            run(&mut t, 3, &[unk]),
            vec![PenTransition::Motion { sample: unk }]
        );
    }

    #[test]
    fn tracker_force_release_and_late_stale_datagram() {
        let mut t = PenTracker::default();
        let mut held = touch(0.5, 0.5, 100);
        held.state |= PEN_BARREL2;
        run(&mut t, 100, &[held]);
        let mut out = Vec::new();
        t.force_release(&mut out);
        assert_eq!(
            out,
            vec![
                PenTransition::ButtonsChanged {
                    pressed: 0,
                    released: PEN_BARREL2
                },
                PenTransition::TipUp,
                PenTransition::ProximityOut,
            ]
        );
        assert!(!t.is_active());
        let mut out = Vec::new();
        t.force_release(&mut out);
        assert!(out.is_empty());
        assert!(run(&mut t, 99, &[held]).is_empty());
        assert!(!t.is_active());
        assert!(!run(&mut t, 101, &[held]).is_empty());
        assert!(t.is_active());
    }

    #[test]
    fn tracker_skips_reserved_predicted_samples() {
        let mut t = PenTracker::default();
        let mut p = touch(0.5, 0.5, 100);
        p.state |= PEN_PREDICTED;
        let real = touch(0.6, 0.5, 120);
        let out = run(&mut t, 0, &[p, real]);
        assert_eq!(
            out,
            vec![
                PenTransition::ProximityIn { tool: PenTool::Pen },
                PenTransition::Motion { sample: real },
                PenTransition::TipDown,
            ]
        );
    }
}
