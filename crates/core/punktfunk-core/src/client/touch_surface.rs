//! A touch surface's touches → touchpad frames, and the one rule for what is forwarded: a
//! gesture the client's OS or app keeps for itself is not. When the client claims a gesture
//! already on the host, one frame lists its contacts as tombstones, so the host cancels what
//! it started, and the gesture stays the embedder's until every finger is up. A source that
//! clicks on taps itself (`retract_taps`) has each tap-like lift tombstoned the same way.
//!
//! A tombstone rides every frame for [`TOUCHPAD_TOMBSTONE_MS`], so a later frame heals a lost
//! one, and its id stays reserved as long. [`TouchpadSources`] shares the one host pad between
//! the surface and [`pinch_frame`]. Design: `design/touchpad-passthrough.md` §4.

use crate::error::{PunktfunkError, Result};
use crate::quic::{
    pinch_frame, PinchPhase, TouchpadContact, TouchpadFrame, TOUCHPAD_ASSUMED_DMM,
    TOUCHPAD_MAX_CONTACTS,
};

/// A lift this soon after landing may be a tap on the host. At or above every host OS's own
/// tap time, so whatever a host could click on is retracted; higher only retracts more.
pub(crate) const TOUCHPAD_TAP_MAX_MS: u64 = 600;
/// ...from a contact that never got further than this from where it landed. At or above every
/// host OS's own tap travel, for the same reason.
pub(crate) const TOUCHPAD_TAP_MAX_MM: f32 = 5.0;
/// How long a tombstone rides every frame the surface sends: several frames past the task's
/// 20 ms echo, short enough that its id is soon free again.
pub(crate) const TOUCHPAD_TOMBSTONE_MS: u64 = 150;

/// One platform touch on the surface.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceTouch {
    /// The platform's identity for the touch, stable while it is down.
    pub id: u64,
    /// `0.0..=1.0` across the surface, origin top-left. Finite values outside it clamp.
    pub x: f32,
    pub y: f32,
}

/// A touch down and forwarded.
#[derive(Clone, Copy, Debug)]
struct Held {
    platform: u64,
    wire: u8,
    landed_ms: u64,
    landed: (f32, f32),
    /// The farthest it got from where it landed, in millimetres.
    moved_mm: f32,
}

impl Held {
    fn is_tap(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.landed_ms) < TOUCHPAD_TAP_MAX_MS
            && self.moved_mm <= TOUCHPAD_TAP_MAX_MM
    }
}

#[derive(Debug, Default)]
pub(crate) struct TouchSurface {
    /// The embedder holds this gesture until every finger lifts.
    kept: bool,
    /// Each touch down in the last update.
    held: Vec<Held>,
    /// Tombstones, oldest first, each with the time it stops riding frames.
    tombs: Vec<(TouchpadContact, u64)>,
    /// The last frame emitted; `None` before the announce.
    last: Option<TouchpadFrame>,
}

impl TouchSurface {
    /// The frames to send for this update, and whether core owns it. `false` means the
    /// gesture is the embedder's until an update with no touches. `max_contacts` clamps to
    /// `1..=5`; more touches than that, or `keep`, claims the gesture for the embedder.
    /// `now_ms` is monotonic. `InvalidArg` on a non-finite coordinate or one platform id twice.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update(
        &mut self,
        touches: &[SurfaceTouch],
        max_contacts: u8,
        button: bool,
        keep: bool,
        retract_taps: bool,
        width_dmm: u16,
        height_dmm: u16,
        now_ms: u64,
    ) -> Result<(Vec<TouchpadFrame>, bool)> {
        validate(touches)?;
        self.tombs.retain(|&(_, until)| now_ms < until);
        if self.kept {
            self.kept = !touches.is_empty();
            return Ok((Vec::new(), false));
        }
        let max = usize::from(max_contacts.clamp(1, TOUCHPAD_MAX_CONTACTS as u8));
        if !touches.is_empty() && (keep || touches.len() > max) {
            self.kept = true;
            self.held.clear();
            return Ok((self.take_back(now_ms).into_iter().collect(), false));
        }
        let lifted = self.track(touches, width_dmm, height_dmm, now_ms);
        if retract_taps {
            for h in lifted.iter().filter(|h| h.is_tap(now_ms)) {
                self.bury(h.wire, now_ms);
            }
        }
        let frame = self.frame(touches, button, width_dmm, height_dmm);
        if self.last == Some(frame) {
            return Ok((Vec::new(), true));
        }
        self.last = Some(frame);
        Ok((vec![frame], true))
    }

    /// A touch is down or the gesture is the embedder's.
    pub(crate) fn busy(&self) -> bool {
        self.kept || !self.held.is_empty()
    }

    /// One frame: every contact the host holds as a tombstone, and the button up.
    fn take_back(&mut self, now_ms: u64) -> Option<TouchpadFrame> {
        let last = self.last.filter(TouchpadFrame::is_active)?;
        for c in last.contacts().iter().filter(|c| c.tip) {
            self.bury(c.id, now_ms);
        }
        let frame = self.frame(&[], false, last.width_dmm, last.height_dmm);
        self.last = Some(frame);
        Some(frame)
    }

    /// Tombstone `wire` at its last forwarded position, if the host was told it touches.
    fn bury(&mut self, wire: u8, now_ms: u64) {
        let Some(last) = self.last else { return };
        let Some(&c) = last.contacts().iter().find(|c| c.id == wire && c.tip) else {
            return;
        };
        self.tombs.retain(|(t, _)| t.id != wire);
        let tomb = TouchpadContact {
            tip: false,
            confident: false,
            ..c
        };
        self.tombs.push((tomb, now_ms + TOUCHPAD_TOMBSTONE_MS));
    }

    /// Follows each touch and returns the ones that lifted. Wire ids are the lowest free in
    /// `0..=31`, past every tombstone's; one freed now stays taken until the next update, or the
    /// host would read a lift and a landing as one finger jumping.
    fn track(
        &mut self,
        touches: &[SurfaceTouch],
        width_dmm: u16,
        height_dmm: u16,
        now_ms: u64,
    ) -> Vec<Held> {
        let held = self.held.iter().map(|h| h.wire);
        let mut taken = held
            .chain(self.tombs.iter().map(|(t, _)| t.id))
            .fold(0u32, |m, id| m | 1 << id);
        let (lifted, held): (Vec<_>, Vec<_>) = self
            .held
            .drain(..)
            .partition(|h| !touches.iter().any(|t| t.id == h.platform));
        self.held = held;
        let (w_mm, h_mm) = surface_mm(width_dmm, height_dmm);
        for t in touches {
            let at = (t.x.clamp(0.0, 1.0), t.y.clamp(0.0, 1.0));
            match self.held.iter_mut().find(|h| h.platform == t.id) {
                Some(h) => {
                    let off = ((at.0 - h.landed.0) * w_mm).hypot((at.1 - h.landed.1) * h_mm);
                    h.moved_mm = h.moved_mm.max(off);
                }
                None => {
                    let wire = (!taken).trailing_zeros() as u8;
                    taken |= 1 << wire;
                    self.held.push(Held {
                        platform: t.id,
                        wire,
                        landed_ms: now_ms,
                        landed: at,
                        moved_mm: 0.0,
                    });
                }
            }
        }
        lifted
    }

    /// Every touch, then the newest tombstones that fit in five.
    fn frame(
        &self,
        touches: &[SurfaceTouch],
        button: bool,
        width_dmm: u16,
        height_dmm: u16,
    ) -> TouchpadFrame {
        let unit = |v: f32| (v.clamp(0.0, 1.0) * 65535.0).round() as u16;
        let mut contacts: Vec<_> = touches
            .iter()
            .filter_map(|t| {
                let h = self.held.iter().find(|h| h.platform == t.id)?;
                Some(TouchpadContact {
                    id: h.wire,
                    tip: true,
                    confident: true,
                    x: unit(t.x),
                    y: unit(t.y),
                })
            })
            .collect();
        let room = TOUCHPAD_MAX_CONTACTS.saturating_sub(contacts.len());
        contacts.extend(self.tombs.iter().rev().take(room).map(|&(t, _)| t));
        contacts.sort_by_key(|c| c.id);
        TouchpadFrame::new(0, button, width_dmm, height_dmm, &contacts)
            .expect("at most five contacts, each with its own id")
    }
}

/// The surface in millimetres; an unknown one is [`TOUCHPAD_ASSUMED_DMM`].
fn surface_mm(width_dmm: u16, height_dmm: u16) -> (f32, f32) {
    let (w, h) = if width_dmm == 0 || height_dmm == 0 {
        TOUCHPAD_ASSUMED_DMM
    } else {
        (width_dmm, height_dmm)
    };
    (f32::from(w) / 10.0, f32::from(h) / 10.0)
}

fn validate(touches: &[SurfaceTouch]) -> Result<()> {
    let finite = touches.iter().all(|t| t.x.is_finite() && t.y.is_finite());
    let distinct = touches
        .iter()
        .enumerate()
        .all(|(i, t)| touches[..i].iter().all(|u| u.id != t.id));
    if finite && distinct {
        Ok(())
    } else {
        Err(PunktfunkError::InvalidArg(
            "surface touches need finite coordinates and distinct ids",
        ))
    }
}

/// The surface and the pinch helper over the one host pad. A pinch begins only while the
/// surface is idle; a surface touch ends a live pinch first.
#[derive(Debug, Default)]
pub(crate) struct TouchpadSources {
    surface: TouchSurface,
    /// `Begin` took the pad; `End` or a surface touch gives it back.
    pinch: bool,
}

impl TouchpadSources {
    /// [`TouchSurface::update`]. An update with no touches during a pinch sends nothing, or
    /// its empty frame would lift the pinch's fingers.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn surface(
        &mut self,
        touches: &[SurfaceTouch],
        max_contacts: u8,
        button: bool,
        keep: bool,
        retract_taps: bool,
        width_dmm: u16,
        height_dmm: u16,
        now_ms: u64,
    ) -> Result<(Vec<TouchpadFrame>, bool)> {
        validate(touches)?;
        if self.pinch && touches.is_empty() {
            return Ok((Vec::new(), true));
        }
        let ended = std::mem::take(&mut self.pinch).then(|| pinch_frame(PinchPhase::End, 1.0, 0.0));
        let (frames, forwarded) = self.surface.update(
            touches,
            max_contacts,
            button,
            keep,
            retract_taps,
            width_dmm,
            height_dmm,
            now_ms,
        )?;
        Ok((ended.into_iter().chain(frames).collect(), forwarded))
    }

    /// The pinch's frame, or `None` while the surface holds the pad: a `Begin` then is
    /// ignored, and so is everything after it until the next `Begin`.
    pub(crate) fn pinch(
        &mut self,
        phase: PinchPhase,
        scale: f32,
        rotation: f32,
    ) -> Option<TouchpadFrame> {
        let live = match phase {
            PinchPhase::Begin => {
                self.pinch = !self.surface.busy();
                self.pinch
            }
            PinchPhase::Update => self.pinch,
            PinchPhase::End => std::mem::take(&mut self.pinch),
        };
        live.then(|| pinch_frame(phase, scale, rotation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u16 = 700;
    const H: u16 = 1500;

    /// One update's arguments beside the touches and the clock.
    #[derive(Clone, Copy)]
    struct Call {
        max: u8,
        button: bool,
        keep: bool,
        retract: bool,
        w: u16,
        h: u16,
    }

    const PLAIN: Call = Call {
        max: 2,
        button: false,
        keep: false,
        retract: false,
        w: W,
        h: H,
    };
    const TAPS: Call = Call {
        retract: true,
        ..PLAIN
    };
    const KEEP: Call = Call {
        keep: true,
        ..PLAIN
    };

    fn t(id: u64, x: f32, y: f32) -> SurfaceTouch {
        SurfaceTouch { id, x, y }
    }

    fn run(
        s: &mut TouchSurface,
        touches: &[SurfaceTouch],
        now_ms: u64,
        c: Call,
    ) -> (Vec<TouchpadFrame>, bool) {
        let (max, b, k, r, w, h) = (c.max, c.button, c.keep, c.retract, c.w, c.h);
        s.update(touches, max, b, k, r, w, h, now_ms).unwrap()
    }

    fn step(s: &mut TouchSurface, touches: &[SurfaceTouch]) -> (Vec<TouchpadFrame>, bool) {
        run(s, touches, 0, PLAIN)
    }

    fn sources(p: &mut TouchpadSources, touches: &[SurfaceTouch]) -> (Vec<TouchpadFrame>, bool) {
        p.surface(touches, 2, false, false, false, W, H, 0).unwrap()
    }

    fn ids(f: &TouchpadFrame) -> Vec<u8> {
        f.contacts().iter().map(|c| c.id).collect()
    }

    fn tombs(f: &TouchpadFrame) -> Vec<u8> {
        let tomb = f.contacts().iter().filter(|c| c.is_tombstone());
        tomb.map(|c| c.id).collect()
    }

    #[test]
    fn the_first_update_announces_and_an_unchanged_one_stays_quiet() {
        let mut s = TouchSurface::default();
        let (out, fwd) = step(&mut s, &[]);
        assert!(fwd);
        assert_eq!(out.len(), 1);
        assert!(out[0].contacts().is_empty() && (out[0].width_dmm, out[0].height_dmm) == (W, H));
        assert_eq!(step(&mut s, &[]), (vec![], true));
        let (out, _) = step(&mut s, &[t(9, 0.5, 2.0)]);
        let c = out[0].contacts()[0];
        assert_eq!(
            (c.id, c.tip, c.confident, c.x, c.y),
            (0, true, true, 32768, 65535)
        );
        assert_eq!(step(&mut s, &[t(9, 0.5, 2.0)]), (vec![], true));
    }

    #[test]
    fn ids_stay_with_their_touch_and_come_back_lowest_first() {
        let mut s = TouchSurface::default();
        step(&mut s, &[]);
        let (out, _) = step(&mut s, &[t(100, 0.1, 0.1), t(200, 0.2, 0.2)]);
        assert_eq!(ids(&out[0]), [0, 1]);
        let (out, _) = step(&mut s, &[t(200, 0.3, 0.3)]);
        assert_eq!(ids(&out[0]), [1]);
        let (out, _) = step(&mut s, &[t(300, 0.4, 0.4), t(200, 0.3, 0.3)]);
        assert_eq!(ids(&out[0]), [0, 1]);
        let c = out[0].contacts();
        assert_eq!((c[0].x, c[1].x), (26214, 19661), "each id keeps its touch");
        let (out, _) = step(&mut s, &[t(400, 0.5, 0.5), t(200, 0.3, 0.3)]);
        assert_eq!(ids(&out[0]), [1, 2], "an id freed now is not reused now");
    }

    /// One tombstone frame takes the pair back; the tombstones ride later frames for 150 ms
    /// and hold their ids that long.
    #[test]
    fn a_third_finger_takes_the_gesture_back_in_one_frame() {
        let mut s = TouchSurface::default();
        let two = [t(1, 0.2, 0.5), t(2, 0.8, 0.5)];
        let (sent, _) = run(&mut s, &two, 1000, PLAIN);
        let (out, fwd) = run(&mut s, &[two[0], two[1], t(3, 0.5, 0.9)], 1010, PLAIN);
        assert!(!fwd);
        assert_eq!(out.len(), 1);
        assert_eq!(tombs(&out[0]), [0, 1]);
        assert!(!out[0].button && !out[0].is_active());
        let at = |f: &TouchpadFrame| f.contacts().iter().map(|c| (c.x, c.y)).collect::<Vec<_>>();
        assert_eq!(at(&out[0]), at(&sent[0]), "at their last positions");
        assert_eq!(run(&mut s, &[t(2, 0.1, 0.1)], 1020, PLAIN), (vec![], false));
        let lift = run(&mut s, &[], 1030, PLAIN);
        assert_eq!(lift, (vec![], false), "the lift is the embedder's too");

        let (out, fwd) = run(&mut s, &[t(5, 0.5, 0.5)], 1100, PLAIN);
        assert!(fwd);
        assert_eq!((ids(&out[0]), tombs(&out[0])), (vec![0, 1, 2], vec![0, 1]));
        let (out, _) = run(&mut s, &[t(5, 0.6, 0.5)], 1160, PLAIN);
        assert_eq!(ids(&out[0]), [2], "carried 150 ms, then gone");
        let (out, _) = run(&mut s, &[t(5, 0.6, 0.5), t(6, 0.1, 0.1)], 1170, PLAIN);
        assert_eq!(ids(&out[0]), [0, 2], "its id is free again");
    }

    #[test]
    fn keep_mid_gesture_is_one_tombstone_frame() {
        let mut s = TouchSurface::default();
        step(&mut s, &[]);
        step(&mut s, &[t(1, 0.5, 0.5)]);
        let (out, fwd) = run(&mut s, &[t(1, 0.6, 0.5)], 0, KEEP);
        assert!(!fwd);
        assert_eq!(out.len(), 1);
        assert_eq!(tombs(&out[0]), [0]);
        let x = out[0].contacts()[0].x;
        assert_eq!(x, 32768, "the last position sent, not the new one");
        assert_eq!(run(&mut s, &[], 0, KEEP), (vec![], false));
        assert_eq!(run(&mut s, &[], 0, KEEP), (vec![], true));
    }

    #[test]
    fn a_kept_first_touch_sends_nothing_but_a_held_button_is_released() {
        let mut s = TouchSurface::default();
        step(&mut s, &[]);
        assert_eq!(run(&mut s, &[t(1, 0.5, 0.5)], 0, KEEP), (vec![], false));
        let mut s = TouchSurface::default();
        let pressed = Call {
            button: true,
            ..PLAIN
        };
        let (out, _) = run(&mut s, &[], 0, pressed);
        assert!(
            out[0].button && out[0].contacts().is_empty(),
            "a button-only frame"
        );
        let held_keep = Call {
            keep: true,
            ..pressed
        };
        let (out, fwd) = run(&mut s, &[t(1, 0.5, 0.5)], 0, held_keep);
        assert!(!fwd);
        assert_eq!(out.len(), 1);
        assert!(!out[0].button && out[0].contacts().is_empty());
    }

    #[test]
    fn button_and_surface_changes_send_again() {
        let mut s = TouchSurface::default();
        step(&mut s, &[t(1, 0.5, 0.5)]);
        let pressed = Call {
            button: true,
            ..PLAIN
        };
        let (out, _) = run(&mut s, &[t(1, 0.5, 0.5)], 0, pressed);
        assert!(out[0].button);
        let (out, _) = run(&mut s, &[t(1, 0.5, 0.5)], 0, Call { h: 800, ..pressed });
        assert_eq!(out[0].height_dmm, 800);
        let five: Vec<_> = (0..5).map(|i| t(i, 0.1, 0.1)).collect();
        let (out, fwd) = run(&mut s, &five, 0, Call { max: 9, ..PLAIN });
        assert!(fwd && out[0].contacts().len() == 5, "max clamps to five");
        let (_, fwd) = run(&mut s, &five[..1], 0, Call { max: 0, ..PLAIN });
        assert!(fwd, "max clamps to one");
    }

    #[test]
    fn bad_touches_are_refused_whole() {
        let mut s = TouchSurface::default();
        let bad = |s: &mut TouchSurface, touches: &[SurfaceTouch]| {
            s.update(touches, 2, false, false, false, W, H, 0).is_err()
        };
        for v in [f32::NAN, f32::INFINITY] {
            assert!(bad(&mut s, &[t(1, v, 0.5)]));
            assert!(bad(&mut s, &[t(1, 0.5, v)]));
        }
        assert!(bad(&mut s, &[t(1, 0.5, 0.5), t(1, 0.6, 0.5)]));
        assert!(s.last.is_none() && !s.busy(), "nothing moved");
    }

    #[test]
    fn a_quick_still_tap_lifts_as_a_tombstone() {
        for retract in [true, false] {
            let mut s = TouchSurface::default();
            let c = Call { retract, ..PLAIN };
            run(&mut s, &[], 0, c);
            run(&mut s, &[t(1, 0.5, 0.5)], 1000, c);
            run(&mut s, &[t(1, 0.51, 0.5)], 1050, c);
            let (out, fwd) = run(&mut s, &[], 1100, c);
            assert!(fwd);
            assert_eq!(out.len(), 1, "retract {retract}");
            if retract {
                assert_eq!(tombs(&out[0]), [0]);
                assert_eq!(
                    out[0].contacts()[0].x,
                    33423,
                    "at its last forwarded position"
                );
                assert!(!out[0].is_active());
            } else {
                assert!(out[0].contacts().is_empty());
            }
        }
    }

    #[test]
    fn a_touch_that_moved_or_stayed_lifts_plainly() {
        // 70 mm wide: 0.12 of it is 8.4 mm, and coming back does not undo it.
        let mut s = TouchSurface::default();
        run(&mut s, &[t(1, 0.5, 0.5)], 1000, TAPS);
        run(&mut s, &[t(1, 0.62, 0.5)], 1050, TAPS);
        run(&mut s, &[t(1, 0.5, 0.5)], 1100, TAPS);
        assert!(run(&mut s, &[], 1150, TAPS).0[0].contacts().is_empty());

        let mut s = TouchSurface::default();
        run(&mut s, &[t(1, 0.5, 0.5)], 1000, TAPS);
        let held = run(&mut s, &[], 1700, TAPS).0;
        assert!(held[0].contacts().is_empty(), "held 700 ms");

        // An unknown surface is 100 × 60 mm: 0.06 of 60 mm is 3.6 mm, still a tap.
        let mut s = TouchSurface::default();
        let unknown = Call { w: 0, h: 0, ..TAPS };
        run(&mut s, &[t(1, 0.5, 0.5)], 1000, unknown);
        run(&mut s, &[t(1, 0.5, 0.56)], 1050, unknown);
        assert_eq!(tombs(&run(&mut s, &[], 1100, unknown).0[0]), [0]);
    }

    #[test]
    fn two_fingers_tapping_each_retract() {
        let c = Call { max: 5, ..TAPS };
        let mut s = TouchSurface::default();
        run(&mut s, &[t(1, 0.3, 0.5), t(2, 0.7, 0.5)], 1000, c);
        let (out, _) = run(&mut s, &[], 1100, c);
        assert_eq!(out.len(), 1);
        assert_eq!(tombs(&out[0]), [0, 1]);

        let mut s = TouchSurface::default();
        run(&mut s, &[t(1, 0.3, 0.5), t(2, 0.7, 0.5)], 1000, c);
        let (out, _) = run(&mut s, &[t(2, 0.7, 0.5), t(3, 0.5, 0.2)], 1080, c);
        assert_eq!(out.len(), 1);
        assert_eq!((ids(&out[0]), tombs(&out[0])), (vec![0, 1, 2], vec![0]));
        let (out, _) = run(&mut s, &[t(3, 0.5, 0.2)], 1150, c);
        assert_eq!((ids(&out[0]), tombs(&out[0])), (vec![0, 1, 2], vec![0, 1]));
    }

    #[test]
    fn a_full_frame_drops_the_oldest_tombstone_but_keeps_its_id() {
        let c = Call { max: 5, ..TAPS };
        let mut s = TouchSurface::default();
        run(&mut s, &[t(1, 0.5, 0.5)], 1000, c);
        run(&mut s, &[], 1010, c);
        run(&mut s, &[t(2, 0.5, 0.5)], 1020, c);
        let (out, _) = run(&mut s, &[], 1030, c);
        assert_eq!(tombs(&out[0]), [0, 1]);
        let four: Vec<_> = (10..14).map(|i| t(i, 0.2, 0.2)).collect();
        let (out, _) = run(&mut s, &four, 1040, c);
        assert_eq!(ids(&out[0]), [1, 2, 3, 4, 5], "id 0 is reserved, not sent");
        assert_eq!(tombs(&out[0]), [1]);
    }

    #[test]
    fn neither_a_kept_gesture_nor_a_cancel_retracts() {
        let mut s = TouchSurface::default();
        run(&mut s, &[t(1, 0.5, 0.5)], 1000, TAPS);
        let three = [t(1, 0.5, 0.5), t(2, 0.6, 0.5), t(3, 0.7, 0.5)];
        let (out, fwd) = run(&mut s, &three, 1050, TAPS);
        assert!(!fwd);
        assert_eq!(out.len(), 1, "the cancel alone");
        assert_eq!(tombs(&out[0]), [0]);
        assert_eq!(run(&mut s, &three[1..], 1080, TAPS), (vec![], false));
        assert_eq!(run(&mut s, &[], 1100, TAPS), (vec![], false));
        let (out, _) = run(&mut s, &[t(4, 0.5, 0.5)], 1120, TAPS);
        assert_eq!((ids(&out[0]), tombs(&out[0])), (vec![0, 1], vec![0]));
        let (out, _) = run(&mut s, &[], 1150, TAPS);
        assert_eq!(tombs(&out[0]), [0, 1], "a fresh tap still retracts");
    }

    #[test]
    fn a_pinch_waits_for_an_idle_surface_and_a_touch_ends_it() {
        let mut p = TouchpadSources::default();
        sources(&mut p, &[t(1, 0.5, 0.5)]);
        assert_eq!(
            p.pinch(PinchPhase::Begin, 1.0, 0.0),
            None,
            "a touch is down"
        );
        sources(&mut p, &[]);
        assert_eq!(
            p.pinch(PinchPhase::Update, 1.2, 0.0),
            None,
            "its Begin was refused"
        );
        assert_eq!(p.pinch(PinchPhase::End, 1.2, 0.0), None);

        let begin = p.pinch(PinchPhase::Begin, 1.0, 0.0).unwrap();
        assert_eq!(begin.contacts().len(), 2);
        assert_eq!(sources(&mut p, &[]), (vec![], true));
        let (out, fwd) = sources(&mut p, &[t(7, 0.5, 0.5)]);
        assert!(fwd);
        assert!(out[0].contacts().is_empty(), "the pinch ends first");
        assert_eq!(out[1].contacts().len(), 1);
        assert_eq!(p.pinch(PinchPhase::Update, 1.5, 0.0), None);

        let three = [t(1, 0.1, 0.1), t(2, 0.2, 0.2), t(3, 0.3, 0.3)];
        sources(&mut p, &three);
        sources(&mut p, &[]);
        sources(&mut p, &three);
        assert_eq!(
            p.pinch(PinchPhase::Begin, 1.0, 0.0),
            None,
            "a kept gesture holds the pad"
        );
        assert!(p.pinch(PinchPhase::End, 1.0, 0.0).is_none());
    }
}
