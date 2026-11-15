//! Touchpad frames on the 0xCC rich-input datagram (`RICH_TOUCHPAD_FRAME` = 0x06), and the
//! fixed virtual pad a host presents them on.
//!
//! A [`TouchpadFrame`] is the whole pad: every contact down, the button, the surface size. A
//! lost datagram heals on the next frame; a stale one drops whole ([`pen_seq_newer`]). While
//! anything is down the client repeats its last frame every [`TOUCHPAD_REPEAT_MS`] with the same
//! `seq`: the gate drops the copy, and the host's [`TOUCHPAD_TIMEOUT_MS`] clock still sees it.
//!
//! The host keeps state in a [`TouchpadTracker`] and maps each frame onto the virtual pad
//! ([`TouchpadFrame::to_pad`]): a millimetre on the client is a millimetre on the pad, centred,
//! scaled down only when the surface is larger. [`pinch_frame`] turns an OS pinch into two
//! fingers for a source that gives no finger positions.
//!
//! Send only under `FEATURE_TOUCHPAD`. Wire, pad and sources: `design/touchpad-passthrough.md`.

use super::datagram::{RICH_INPUT_MAGIC, RICH_TOUCHPAD_FRAME};
use super::pen::pen_seq_newer;

pub const TOUCHPAD_MAX_CONTACTS: usize = 5;
/// Contact ids run `0..=TOUCHPAD_MAX_ID`.
pub const TOUCHPAD_MAX_ID: u8 = 31;
/// The client's repeat while a contact or the button is down: two inside the host's timeout.
pub const TOUCHPAD_REPEAT_MS: u32 = 100;
/// The host lifts everything after this long without a frame.
pub const TOUCHPAD_TIMEOUT_MS: u32 = 200;

/// The virtual pad every host presents: 320 × 240 mm, large enough that a phone's screen edges
/// stay out of the host's palm and edge zones.
pub const TOUCHPAD_PAD_WIDTH_MM: u16 = 320;
pub const TOUCHPAD_PAD_HEIGHT_MM: u16 = 240;
/// Pad resolution; the backends declare it so the host OS measures travel in millimetres.
pub const TOUCHPAD_UNITS_PER_MM: u16 = 100;
/// Logical X runs `0..=TOUCHPAD_PAD_MAX_X`.
pub const TOUCHPAD_PAD_MAX_X: u16 = TOUCHPAD_PAD_WIDTH_MM * TOUCHPAD_UNITS_PER_MM;
/// Logical Y runs `0..=TOUCHPAD_PAD_MAX_Y`.
pub const TOUCHPAD_PAD_MAX_Y: u16 = TOUCHPAD_PAD_HEIGHT_MM * TOUCHPAD_UNITS_PER_MM;
/// The surface, in tenths of a millimetre, of a frame that says `0`: 100 × 60 mm.
pub const TOUCHPAD_ASSUMED_DMM: (u16, u16) = (1000, 600);

/// `[0xCC][0x06][u16 seq][flags][u16 width_dmm][u16 height_dmm][count]`.
const HEADER_LEN: usize = 10;
/// `[id][state][u16 x][u16 y]`.
const CONTACT_LEN: usize = 6;
const FLAG_BUTTON: u8 = 0x01;
const STATE_TIP: u8 = 0x01;
const STATE_CONFIDENT: u8 = 0x02;

/// A contact with `tip` clear is not touching. With `confident` clear too it is a tombstone:
/// it lifted, and the host treats that lift as accidental, so it never taps or clicks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TouchpadContact {
    /// Stable for the life of one contact, `0..=`[`TOUCHPAD_MAX_ID`].
    pub id: u8,
    /// Touching the surface.
    pub tip: bool,
    /// Clear: the host treats the contact as accidental. A client takes a gesture back this way.
    pub confident: bool,
    /// `0..=65535` across the surface, origin top-left.
    pub x: u16,
    pub y: u16,
}

impl TouchpadContact {
    pub fn is_tombstone(&self) -> bool {
        !self.tip && !self.confident
    }
}

/// `[0xCC][0x06][u16 seq LE][u8 flags][u16 width_dmm LE][u16 height_dmm LE][u8 count]` +
/// `count` × `[u8 id][u8 state][u16 x LE][u16 y LE]`. Flags bit 0 is the button; state bit 0
/// tip, bit 1 confident; other bits are ignored. A contact absent from a frame has lifted; one
/// listed as a tombstone lifted by accident, which one frame says whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TouchpadFrame {
    /// Wrapping; the host applies only a newer one.
    pub seq: u16,
    pub button: bool,
    /// The client's surface in tenths of a millimetre; `0` is unknown.
    pub width_dmm: u16,
    pub height_dmm: u16,
    count: u8,
    contacts: [TouchpadContact; TOUCHPAD_MAX_CONTACTS],
}

impl TouchpadFrame {
    /// `None` on more than [`TOUCHPAD_MAX_CONTACTS`] contacts, an id above
    /// [`TOUCHPAD_MAX_ID`], or one id twice: a backend keys its slots on the id.
    pub fn new(
        seq: u16,
        button: bool,
        width_dmm: u16,
        height_dmm: u16,
        contacts: &[TouchpadContact],
    ) -> Option<TouchpadFrame> {
        let ids_ok = contacts
            .iter()
            .enumerate()
            .all(|(i, c)| c.id <= TOUCHPAD_MAX_ID && !contacts[..i].iter().any(|d| d.id == c.id));
        if contacts.len() > TOUCHPAD_MAX_CONTACTS || !ids_ok {
            return None;
        }
        let mut buf = [TouchpadContact::default(); TOUCHPAD_MAX_CONTACTS];
        buf[..contacts.len()].copy_from_slice(contacts);
        Some(TouchpadFrame {
            seq,
            button,
            width_dmm,
            height_dmm,
            count: contacts.len() as u8,
            contacts: buf,
        })
    }

    pub fn contacts(&self) -> &[TouchpadContact] {
        &self.contacts[..usize::from(self.count)]
    }

    /// A contact touches or the button is down: the client repeats it, the host times it out.
    /// Tombstones alone are not active.
    pub fn is_active(&self) -> bool {
        self.button || self.contacts().iter().any(|c| c.tip)
    }

    pub fn has_tombstone(&self) -> bool {
        self.contacts().iter().any(TouchpadContact::is_tombstone)
    }

    /// Every contact lifted and the button up, on the same surface. Unused slots stay at their
    /// default, as `new` leaves them, so two equal pads compare equal.
    pub fn lifted(&self) -> TouchpadFrame {
        TouchpadFrame::new(self.seq, false, self.width_dmm, self.height_dmm, &[])
            .expect("no contacts is a valid frame")
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.contacts().len() * CONTACT_LEN);
        out.extend_from_slice(&[RICH_INPUT_MAGIC, RICH_TOUCHPAD_FRAME]);
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.push(if self.button { FLAG_BUTTON } else { 0 });
        out.extend_from_slice(&self.width_dmm.to_le_bytes());
        out.extend_from_slice(&self.height_dmm.to_le_bytes());
        out.push(self.count);
        for c in self.contacts() {
            let state =
                if c.tip { STATE_TIP } else { 0 } | if c.confident { STATE_CONFIDENT } else { 0 };
            out.extend_from_slice(&[c.id, state]);
            out.extend_from_slice(&c.x.to_le_bytes());
            out.extend_from_slice(&c.y.to_le_bytes());
        }
        out
    }

    /// `None` on a count above [`TOUCHPAD_MAX_CONTACTS`], a body shorter than its count, or
    /// contacts [`Self::new`] refuses: a malformed frame drops whole. Trailing bytes are ignored.
    pub fn decode(b: &[u8]) -> Option<TouchpadFrame> {
        if b.len() < HEADER_LEN || b[0] != RICH_INPUT_MAGIC || b[1] != RICH_TOUCHPAD_FRAME {
            return None;
        }
        let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let count = usize::from(b[9]);
        if count > TOUCHPAD_MAX_CONTACTS || b.len() < HEADER_LEN + count * CONTACT_LEN {
            return None;
        }
        let mut contacts = [TouchpadContact::default(); TOUCHPAD_MAX_CONTACTS];
        for (i, slot) in contacts.iter_mut().enumerate().take(count) {
            let o = HEADER_LEN + i * CONTACT_LEN;
            *slot = TouchpadContact {
                id: b[o],
                tip: b[o + 1] & STATE_TIP != 0,
                confident: b[o + 1] & STATE_CONFIDENT != 0,
                x: u16at(o + 2),
                y: u16at(o + 4),
            };
        }
        TouchpadFrame::new(
            u16at(2),
            b[4] & FLAG_BUTTON != 0,
            u16at(5),
            u16at(7),
            &contacts[..count],
        )
    }

    /// This frame's touching contacts on the virtual pad, in pad units. The surface keeps its
    /// millimetres, centred on the pad; one larger than the pad shrinks uniformly to fit. Zero
    /// is [`TOUCHPAD_ASSUMED_DMM`].
    pub fn to_pad(&self) -> PadState {
        let (w, h) = if self.width_dmm == 0 || self.height_dmm == 0 {
            TOUCHPAD_ASSUMED_DMM
        } else {
            (self.width_dmm, self.height_dmm)
        };
        let (max_x, max_y) = (f64::from(TOUCHPAD_PAD_MAX_X), f64::from(TOUCHPAD_PAD_MAX_Y));
        let per_dmm = f64::from(TOUCHPAD_UNITS_PER_MM) / 10.0;
        let (w, h) = (f64::from(w) * per_dmm, f64::from(h) * per_dmm);
        let fit = (max_x / w).min(max_y / h).min(1.0);
        let (w, h) = (w * fit, h * fit);
        let (ox, oy) = ((max_x - w) / 2.0, (max_y - h) / 2.0);
        let at = |o: f64, span: f64, v: u16| (o + span * f64::from(v) / 65535.0).round() as u16;
        let mut contacts = [PadContact::default(); TOUCHPAD_MAX_CONTACTS];
        let mut count = 0;
        for c in self.contacts().iter().filter(|c| c.tip) {
            contacts[count] = PadContact {
                id: c.id,
                tip: true,
                confident: c.confident,
                x: at(ox, w, c.x),
                y: at(oy, h, c.y),
            };
            count += 1;
        }
        PadState {
            button: self.button,
            count: count as u8,
            contacts,
        }
    }
}

/// One contact on the virtual pad: `x` in `0..=TOUCHPAD_PAD_MAX_X`, `y` in
/// `0..=TOUCHPAD_PAD_MAX_Y`, origin top-left.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PadContact {
    pub id: u8,
    pub tip: bool,
    pub confident: bool,
    pub x: u16,
    pub y: u16,
}

/// The virtual pad's whole state, what a backend presents. Every contact touches; one absent
/// has lifted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PadState {
    pub button: bool,
    count: u8,
    contacts: [PadContact; TOUCHPAD_MAX_CONTACTS],
}

impl PadState {
    pub fn contacts(&self) -> &[PadContact] {
        &self.contacts[..usize::from(self.count)]
    }
}

/// Host side. Clock-free: the owner arms [`TOUCHPAD_TIMEOUT_MS`] over [`Self::is_active`] and
/// calls [`Self::force_release`] when it fires and at session end.
#[derive(Debug, Default)]
pub struct TouchpadTracker {
    last_seq: Option<u16>,
    last: Option<TouchpadFrame>,
}

impl TouchpadTracker {
    /// The states to present for `frame`, in order; none for a frame no newer than the last
    /// one applied. A tombstone for a contact that was touching comes first as that contact,
    /// still down where it was but not confident, beside the frame's touching contacts. A
    /// backend then always sees the doubt before the lift, whichever frame carried it.
    pub fn apply(&mut self, frame: &TouchpadFrame) -> Vec<TouchpadFrame> {
        if !pen_seq_newer(frame.seq, self.last_seq) {
            return Vec::new();
        }
        self.last_seq = Some(frame.seq);
        let before = self.last.replace(*frame);
        let was = |id: u8| {
            before?
                .contacts()
                .iter()
                .find(|p| p.id == id && p.tip)
                .copied()
        };
        let doubted: Vec<_> = frame
            .contacts()
            .iter()
            .filter(|c| c.is_tombstone())
            .filter_map(|c| was(c.id))
            .map(|p| TouchpadContact {
                confident: false,
                ..p
            })
            .collect();
        if doubted.is_empty() {
            return vec![*frame];
        }
        let mut contacts: Vec<_> = frame.contacts().iter().filter(|c| c.tip).copied().collect();
        contacts.extend(doubted);
        contacts.sort_by_key(|c| c.id);
        let (button, w, h) = (frame.button, frame.width_dmm, frame.height_dmm);
        TouchpadFrame::new(frame.seq, button, w, h, &contacts)
            .into_iter()
            .chain([*frame])
            .collect()
    }

    /// The last frame applied, or lifted by [`Self::force_release`].
    pub fn last(&self) -> Option<&TouchpadFrame> {
        self.last.as_ref()
    }

    pub fn is_active(&self) -> bool {
        self.last.is_some_and(|f| f.is_active())
    }

    /// The all-lifted frame to present when anything is down, else `None`. The seq gate stays
    /// armed, so a late datagram from before cannot press again.
    pub fn force_release(&mut self) -> Option<TouchpadFrame> {
        let lifted = self.last.filter(TouchpadFrame::is_active)?.lifted();
        self.last = Some(lifted);
        Some(lifted)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinchPhase {
    Begin,
    Update,
    End,
}

/// The two fingers' distance at scale 1.0 on the nominal surface.
const PINCH_SPAN_MM: f32 = 30.0;

/// An OS pinch and rotation as two confident fingers, ids 0 and 1, for a source that gives no
/// finger positions (iPadOS). `scale` and `rotation` (radians, clockwise) are cumulative since
/// `Begin`. The fingers sit symmetric about the centre of a nominal 100 × 60 mm surface,
/// 30 mm × `scale` apart and kept on it; `End` lifts both. Ratios in, so the host's gesture
/// curve is the only one applied. `seq` is the sender's to stamp.
pub fn pinch_frame(phase: PinchPhase, scale: f32, rotation: f32) -> TouchpadFrame {
    let (w_dmm, h_dmm) = TOUCHPAD_ASSUMED_DMM;
    let (half_w, half_h) = (f32::from(w_dmm) / 20.0, f32::from(h_dmm) / 20.0);
    let contacts = if phase == PinchPhase::End {
        Vec::new()
    } else {
        let (sin, cos) = rotation.sin_cos();
        let r = (PINCH_SPAN_MM / 2.0 * scale.max(0.0))
            .min(half_w / cos.abs().max(f32::EPSILON))
            .min(half_h / sin.abs().max(f32::EPSILON));
        let at = |mm: f32, span: f32| (mm / span * 65535.0).round() as u16;
        [(0, -1.0), (1, 1.0)]
            .map(|(id, side)| TouchpadContact {
                id,
                tip: true,
                confident: true,
                x: at(half_w + side * r * cos, 2.0 * half_w),
                y: at(half_h + side * r * sin, 2.0 * half_h),
            })
            .to_vec()
    };
    TouchpadFrame::new(0, false, w_dmm, h_dmm, &contacts).expect("two distinct ids")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::{PenBatch, RichInput};

    fn finger(id: u8, x: u16, y: u16) -> TouchpadContact {
        TouchpadContact {
            id,
            tip: true,
            confident: true,
            x,
            y,
        }
    }

    fn frame(seq: u16, contacts: &[TouchpadContact]) -> TouchpadFrame {
        TouchpadFrame::new(seq, false, 700, 1500, contacts).unwrap()
    }

    #[test]
    fn frame_round_trips() {
        let mut taken_back = finger(7, 65535, 0);
        taken_back.confident = false;
        let mut hover = finger(31, 1, 2);
        hover.tip = false;
        let f = TouchpadFrame::new(
            0xFFFE,
            true,
            1234,
            567,
            &[finger(0, 100, 200), taken_back, hover],
        )
        .unwrap();
        let d = f.encode();
        assert_eq!(&d[..2], &[RICH_INPUT_MAGIC, RICH_TOUCHPAD_FRAME]);
        assert_eq!(d.len(), HEADER_LEN + 3 * CONTACT_LEN);
        assert_eq!(TouchpadFrame::decode(&d), Some(f));

        let empty = TouchpadFrame::new(1, false, 0, 0, &[]).unwrap();
        assert_eq!(TouchpadFrame::decode(&empty.encode()), Some(empty));
        assert!(!empty.is_active());
    }

    #[test]
    fn malformed_frames_drop_whole() {
        let five: Vec<_> = (0..5).map(|i| finger(i, 0, 0)).collect();
        let d = TouchpadFrame::new(3, false, 0, 0, &five).unwrap().encode();
        assert!(TouchpadFrame::decode(&d).is_some());
        assert!(TouchpadFrame::decode(&d[..d.len() - 1]).is_none(), "torn");
        assert!(TouchpadFrame::decode(&d[..HEADER_LEN - 1]).is_none());
        let mut six = d.clone();
        six[9] = 6;
        six.extend_from_slice(&[5, STATE_TIP, 0, 0, 0, 0]);
        assert!(TouchpadFrame::decode(&six).is_none(), "count above five");
        let mut dup = d.clone();
        dup[HEADER_LEN + CONTACT_LEN] = 0;
        assert!(TouchpadFrame::decode(&dup).is_none(), "one id twice");
        let mut far = d.clone();
        far[HEADER_LEN] = TOUCHPAD_MAX_ID + 1;
        assert!(TouchpadFrame::decode(&far).is_none(), "id above 31");
        let mut kind = d.clone();
        kind[1] = 0x05;
        assert!(TouchpadFrame::decode(&kind).is_none());

        let six: Vec<_> = (0..6).map(|i| finger(i, 0, 0)).collect();
        assert!(TouchpadFrame::new(0, false, 0, 0, &six).is_none());
    }

    #[test]
    fn unknown_bits_are_ignored_and_trailing_bytes_skipped() {
        let f = TouchpadFrame::new(9, false, 10, 20, &[finger(2, 3, 4)]).unwrap();
        let mut d = f.encode();
        d[4] = 0xFE;
        d[HEADER_LEN + 1] = 0xFF;
        d.extend_from_slice(&[0xAA; 3]);
        assert_eq!(TouchpadFrame::decode(&d), Some(f));
    }

    #[test]
    fn touchpad_plane_is_disjoint_from_rich_input_and_pen() {
        let d = frame(0, &[finger(0, 1, 1)]).encode();
        assert!(RichInput::decode(&d).is_none());
        assert!(PenBatch::decode(&d).is_none());
    }

    #[test]
    fn tracker_gates_stale_frames_and_lifts_on_release() {
        let mut t = TouchpadTracker::default();
        let down = frame(10, &[finger(0, 5, 5)]);
        assert_eq!(t.apply(&down), [down]);
        assert!(t.is_active());
        assert!(
            t.apply(&frame(10, &[])).is_empty(),
            "a repeat keeps its seq"
        );
        assert!(t.apply(&frame(9, &[])).is_empty(), "stale");
        assert_eq!(t.last(), Some(&down));

        let lifted = t.force_release().unwrap();
        assert!(lifted.contacts().is_empty() && !lifted.button);
        assert_eq!((lifted.width_dmm, lifted.height_dmm), (700, 1500));
        assert!(!t.is_active());
        assert_eq!(t.force_release(), None, "nothing left to lift");
        assert!(
            t.apply(&down).is_empty(),
            "a late datagram cannot press again"
        );
        assert_eq!(t.apply(&frame(11, &[finger(0, 6, 6)])).len(), 1);

        let mut held = TouchpadTracker::default();
        held.apply(&TouchpadFrame::new(0, true, 0, 0, &[]).unwrap());
        assert!(held.is_active(), "a held button with no finger");
        assert!(!held.force_release().unwrap().button);
    }

    fn tombstone(id: u8) -> TouchpadContact {
        TouchpadContact {
            id,
            tip: false,
            confident: false,
            x: 1,
            y: 1,
        }
    }

    #[test]
    fn a_tombstone_frame_is_inactive_and_never_reaches_a_backend() {
        let f = frame(0, &[tombstone(3)]);
        assert!(f.has_tombstone() && !f.is_active());
        assert!(f.to_pad().contacts().is_empty());
        let mixed = frame(0, &[finger(0, 1, 1), tombstone(3)]);
        assert!(mixed.is_active());
        assert_eq!(mixed.to_pad().contacts().len(), 1);
    }

    /// The doubt comes first for a contact that was touching, at its last position; a
    /// tombstone for anything else changes nothing.
    #[test]
    fn a_tombstone_presents_doubt_then_lift() {
        let mut t = TouchpadTracker::default();
        t.apply(&frame(0, &[finger(0, 100, 200), finger(1, 300, 400)]));
        let out = t.apply(&frame(
            1,
            &[tombstone(0), finger(1, 310, 400), finger(2, 9, 9)],
        ));
        assert_eq!(out.len(), 2);
        let doubt = out[0].contacts();
        assert_eq!(doubt.len(), 3);
        assert_eq!(
            (doubt[0].id, doubt[0].tip, doubt[0].confident),
            (0, true, false)
        );
        assert_eq!((doubt[0].x, doubt[0].y), (100, 200), "where it was");
        assert!(doubt[1].confident && doubt[1].x == 310 && doubt[2].id == 2);
        let gone = out[1].to_pad();
        assert_eq!(
            gone.contacts().iter().map(|c| c.id).collect::<Vec<_>>(),
            [1, 2]
        );

        let carried = frame(2, &[tombstone(0), finger(1, 320, 400)]);
        assert_eq!(t.apply(&carried), [carried], "already lifted");
        let unknown = frame(3, &[tombstone(7)]);
        assert_eq!(t.apply(&unknown), [unknown]);
    }

    #[test]
    fn a_carried_tombstone_heals_a_lost_one() {
        let mut t = TouchpadTracker::default();
        t.apply(&frame(0, &[finger(0, 100, 200)]));
        // Frame 1, the first tombstone, never arrived.
        let out = t.apply(&frame(2, &[tombstone(0), finger(1, 5, 5)]));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].contacts()[0].id, 0);
        assert!(!out[0].contacts()[0].confident && out[0].contacts()[0].tip);
        assert_eq!(out[1].to_pad().contacts()[0].id, 1);
    }

    #[test]
    fn a_portrait_phone_keeps_its_millimetres_centred() {
        // 70 × 150 mm → 7000 × 15000 units, offset (12500, 4500).
        let pad = frame(0, &[finger(0, 0, 0), finger(1, 65535, 65535)]).to_pad();
        let c = pad.contacts();
        assert_eq!((c[0].x, c[0].y), (12500, 4500));
        assert_eq!((c[1].x, c[1].y), (19500, 19500));
        assert_eq!((c[1].id, c[1].tip, c[1].confident), (1, true, true));
        let mid = frame(0, &[finger(0, 32768, 32768)]).to_pad();
        assert_eq!((mid.contacts()[0].x, mid.contacts()[0].y), (16000, 12000));
    }

    #[test]
    fn a_surface_larger_than_the_pad_shrinks_uniformly() {
        // 500 × 100 mm: fit 0.64 → 32000 × 6400, offset y 8800.
        let f = TouchpadFrame::new(
            0,
            true,
            5000,
            1000,
            &[finger(0, 0, 0), finger(1, 65535, 65535)],
        );
        let pad = f.unwrap().to_pad();
        assert!(pad.button);
        let c = pad.contacts();
        assert_eq!((c[0].x, c[0].y), (0, 8800));
        assert_eq!((c[1].x, c[1].y), (TOUCHPAD_PAD_MAX_X, 15200));
    }

    #[test]
    fn an_unknown_surface_is_100_by_60_mm() {
        let f = TouchpadFrame::new(0, false, 0, 800, &[finger(0, 0, 65535)]).unwrap();
        let c = f.to_pad().contacts()[0];
        assert_eq!((c.x, c.y), (11000, 15000));
    }

    fn mm(p: (u16, u16)) -> (f32, f32) {
        (
            f32::from(p.0) / 65535.0 * 100.0,
            f32::from(p.1) / 65535.0 * 60.0,
        )
    }

    fn pair(f: &TouchpadFrame) -> ((f32, f32), (f32, f32)) {
        let c = f.contacts();
        assert_eq!(c.len(), 2);
        assert!(c.iter().all(|c| c.tip && c.confident));
        (mm((c[0].x, c[0].y)), mm((c[1].x, c[1].y)))
    }

    #[test]
    fn a_pinch_spreads_and_turns_two_fingers_about_the_centre() {
        let ((ax, ay), (bx, by)) = pair(&pinch_frame(PinchPhase::Begin, 1.0, 0.0));
        assert!((bx - ax - 30.0).abs() < 0.01 && (ay - 30.0).abs() < 0.01 && ay == by);
        assert!(((ax + bx) / 2.0 - 50.0).abs() < 0.01);

        let ((ax, _), (bx, _)) = pair(&pinch_frame(PinchPhase::Update, 2.0, 0.0));
        assert!((bx - ax - 60.0).abs() < 0.01);

        let ((ax, ay), (bx, by)) = pair(&pinch_frame(
            PinchPhase::Update,
            1.0,
            std::f32::consts::FRAC_PI_2,
        ));
        assert!((ax - 50.0).abs() < 0.01 && (bx - 50.0).abs() < 0.01);
        assert!((by - ay - 30.0).abs() < 0.01, "clockwise: finger 1 below");

        let ((ax, _), (bx, _)) = pair(&pinch_frame(PinchPhase::Update, 10.0, 0.0));
        assert!(
            ax.abs() < 0.01 && (bx - 100.0).abs() < 0.01,
            "kept on the surface"
        );
        let ((_, ay), (_, by)) = pair(&pinch_frame(
            PinchPhase::Update,
            10.0,
            std::f32::consts::FRAC_PI_2,
        ));
        assert!(ay.abs() < 0.01 && (by - 60.0).abs() < 0.01);

        let end = pinch_frame(PinchPhase::End, 3.0, 1.0);
        assert!(end.contacts().is_empty() && !end.is_active());
        assert_eq!((end.width_dmm, end.height_dmm), TOUCHPAD_ASSUMED_DMM);
    }
}
