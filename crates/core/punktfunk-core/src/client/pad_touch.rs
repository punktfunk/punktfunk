//! Controller touchpads as a laptop trackpad, in either controller-mouse mode.
//!
//! Every source becomes a [`Contact`] first: a DualSense's `Touchpad`, a Steam pad's `TouchpadEx`,
//! and the trackpads inside a Steam Controller 2's raw report. A single pad or a right pad moves
//! the pointer and clicks left, or right with two fingers down; a left pad scrolls and clicks
//! right. A landing finger moves nothing, and a tap never clicks: a thumb brushing the pad
//! mid-game must not fire.

use super::pad_mouse::{event, scroll_event, stream_height, take, MOUSE_LEFT, MOUSE_RIGHT};
use crate::input::scroll::{ScrollPhase, ScrollSource, SCROLL_SCALE};
use crate::input::{InputEvent, InputKind};
use crate::quic::{RichInput, GRANT_POINTER};

/// Surfaces, in `TouchpadEx::surface` numbering.
const SINGLE: usize = 0;
const LEFT: usize = 1;
const RIGHT: usize = 2;

/// Stream heights a full-width stroke moves, and the surface's height over its width. The
/// DualSense pad is about twice as wide as a Steam pad, so equal finger travel moves about as far.
const GAIN: [(f64, f64); 3] = [(1.5, 1080.0 / 1920.0), (0.8, 1.0), (0.8, 1.0)];
/// Scroll distance ignores the stream mode, as the stick's does.
const SCROLL_HEIGHT: f64 = 1080.0;

/// Steam Controller 2 state reports (SDL `ETritonReportIDTypes`); `0x47` differs only past the pads.
const TRITON_STATES: [u8; 3] = [0x42, 0x45, 0x47];
/// SDL `TritonButtons`: (touch, click) per pad.
const TRITON_LPAD: (u32, u32) = (0x0200_0000, 0x0400_0000);
const TRITON_RPAD: (u32, u32) = (0x0020_0000, 0x0040_0000);
/// `TritonMTUNoQuat_t` after the id byte: buttons at 2, left pad x/y/pressure at 18, right pad at
/// 24. Each is i16 little-endian with +y up.
const TRITON_PADS: std::ops::Range<usize> = 18..30;

/// One touchpad sample in the translator's terms.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Contact {
    pub(crate) pad: u8,
    /// `0` single, `1` left, `2` right.
    pub(crate) surface: u8,
    pub(crate) finger: u8,
    pub(crate) touch: bool,
    /// Click level, when the source carries one. A DualSense's rides `BTN_TOUCHPAD` instead.
    pub(crate) click: Option<bool>,
    /// `0..=1` across the surface, origin top-left.
    pub(crate) x: f64,
    pub(crate) y: f64,
    /// Decoded from a Steam Controller 2 raw report.
    pub(crate) raw: bool,
}

/// Split a mouse pad's rich input: touch becomes contacts, and the rest goes to the host pad
/// unless the whole pad is a mouse. A raw report reaches the host without its trackpads.
pub(crate) fn route(rich: &mut RichInput, full: bool) -> (Vec<Contact>, bool) {
    match rich {
        RichInput::Touchpad {
            pad,
            finger,
            active,
            x,
            y,
        } => {
            let c = Contact {
                pad: *pad,
                surface: SINGLE as u8,
                finger: *finger,
                touch: *active,
                click: None,
                x: f64::from(*x) / 65535.0,
                y: f64::from(*y) / 65535.0,
                raw: false,
            };
            (vec![c], false)
        }
        RichInput::TouchpadEx {
            pad,
            surface,
            finger,
            touch,
            click,
            x,
            y,
            ..
        } if usize::from(*surface) <= RIGHT => {
            let c = Contact {
                pad: *pad,
                surface: *surface,
                finger: *finger,
                touch: *touch,
                click: Some(*click),
                x: (f64::from(*x) + 32768.0) / 65535.0,
                y: (f64::from(*y) + 32768.0) / 65535.0,
                raw: false,
            };
            (vec![c], false)
        }
        RichInput::TouchpadEx { .. } => (Vec::new(), false),
        RichInput::HidReport { pad, len, data } => {
            let n = usize::from(*len).min(data.len());
            let contacts = triton(*pad, &data[..n]).map_or_else(Vec::new, Vec::from);
            if !full && !contacts.is_empty() {
                blank_triton(&mut data[..n]);
            }
            (contacts, !full)
        }
        RichInput::Motion { .. } => (Vec::new(), !full),
    }
}

fn triton(pad: u8, r: &[u8]) -> Option<[Contact; 2]> {
    if r.len() < TRITON_PADS.end || !TRITON_STATES.contains(&r[0]) {
        return None;
    }
    let buttons = u32::from_le_bytes([r[2], r[3], r[4], r[5]]);
    let axis = |o: usize| f64::from(i16::from_le_bytes([r[o], r[o + 1]])) / 65536.0;
    let side = |surface: usize, (touch, click): (u32, u32), o: usize| Contact {
        pad,
        surface: surface as u8,
        finger: 0,
        touch: buttons & touch != 0,
        click: Some(buttons & click != 0),
        x: axis(o) + 0.5,
        y: 0.5 - axis(o + 2),
        raw: true,
    };
    Some([side(LEFT, TRITON_LPAD, 18), side(RIGHT, TRITON_RPAD, 24)])
}

fn blank_triton(r: &mut [u8]) {
    let pads = TRITON_LPAD.0 | TRITON_LPAD.1 | TRITON_RPAD.0 | TRITON_RPAD.1;
    let buttons = u32::from_le_bytes([r[2], r[3], r[4], r[5]]) & !pads;
    r[2..6].copy_from_slice(&buttons.to_le_bytes());
    r[TRITON_PADS].fill(0);
}

#[derive(Clone, Copy, Default)]
struct Surface {
    /// Fingers down, a bit per finger id.
    fingers: u8,
    /// The finger that steers and where it last was.
    lead: Option<(u8, f64, f64)>,
    /// The mouse button this surface's click holds.
    held: Option<u32>,
    /// Scroll axes mid-stroke, vertical then horizontal.
    scrolling: [bool; 2],
    /// Sub-unit carry per axis.
    rem: [f64; 2],
}

impl Surface {
    fn travel(&mut self, surface: usize, dx: f64, dy: f64, height: u32, evs: &mut Vec<InputEvent>) {
        let (gain, aspect) = GAIN[surface];
        if surface == LEFT {
            // Finger up and right are positive, as a touchscreen pan sends them.
            let units = gain * SCROLL_HEIGHT * SCROLL_SCALE;
            for (axis, v) in [(0usize, -dy * aspect), (1, dx)] {
                let delta = take(&mut self.rem[axis], v * units);
                if delta != 0 {
                    let phase = if std::mem::replace(&mut self.scrolling[axis], true) {
                        ScrollPhase::Update
                    } else {
                        ScrollPhase::Begin
                    };
                    evs.push(scroll_event(
                        ScrollSource::Finger,
                        axis as u32,
                        delta,
                        phase,
                    ));
                }
            }
            return;
        }
        let px = gain * stream_height(height);
        let mx = take(&mut self.rem[0], dx * px);
        let my = take(&mut self.rem[1], dy * aspect * px);
        if mx != 0 || my != 0 {
            evs.push(event(InputKind::MouseMove, 0, mx, my, 0));
        }
    }

    fn end_scroll(&mut self, phase: ScrollPhase, evs: &mut Vec<InputEvent>) {
        self.rem = [0.0; 2];
        for axis in 0..2 {
            if std::mem::take(&mut self.scrolling[axis]) {
                evs.push(scroll_event(ScrollSource::Finger, axis as u32, 0, phase));
            }
        }
    }

    fn click(&mut self, surface: usize, down: bool, grants: u32) -> Option<InputEvent> {
        match (down, self.held) {
            (true, None) if grants & GRANT_POINTER != 0 => {
                let button = if surface == LEFT || self.fingers.count_ones() >= 2 {
                    MOUSE_RIGHT
                } else {
                    MOUSE_LEFT
                };
                self.held = Some(button);
                Some(event(InputKind::MouseButtonDown, button, 0, 0, 0))
            }
            (false, Some(button)) => {
                self.held = None;
                Some(event(InputKind::MouseButtonUp, button, 0, 0, 0))
            }
            _ => None,
        }
    }
}

/// One pad's touch surfaces.
#[derive(Clone, Copy, Default)]
pub(super) struct Touchpads {
    surfaces: [Surface; 3],
    /// A raw Steam Controller 2 feed drives these surfaces; typed touch and clicks only echo it.
    raw: bool,
}

impl Touchpads {
    /// Pointer, scroll and click edges for one contact. The lead finger steers until it lifts.
    pub(super) fn contact(&mut self, c: Contact, height: u32, grants: u32) -> Vec<InputEvent> {
        if c.raw {
            self.raw = true;
        } else if self.raw {
            return Vec::new();
        }
        let surface = usize::from(c.surface);
        let s = &mut self.surfaces[surface];
        let bit = 1u8.checked_shl(u32::from(c.finger)).unwrap_or(0);
        if c.touch {
            s.fingers |= bit;
        } else {
            s.fingers &= !bit;
        }
        let mut evs = Vec::new();
        match s.lead {
            Some((f, ..)) if f != c.finger => {}
            Some((_, lx, ly)) if c.touch => {
                s.lead = Some((c.finger, c.x, c.y));
                if grants & GRANT_POINTER != 0 {
                    s.travel(surface, c.x - lx, c.y - ly, height, &mut evs);
                }
            }
            None if c.touch => s.lead = Some((c.finger, c.x, c.y)),
            _ => {
                s.lead = None;
                s.end_scroll(ScrollPhase::End, &mut evs);
            }
        }
        if let Some(down) = c.click {
            evs.extend(s.click(surface, down, grants));
        }
        evs
    }

    /// `BTN_TOUCHPAD`: a DualSense's click. A raw Steam Controller 2 already clicked in its report.
    pub(super) fn button_click(&mut self, down: bool, grants: u32) -> Option<InputEvent> {
        if self.raw {
            return None;
        }
        self.surfaces[SINGLE].click(SINGLE, down, grants)
    }

    /// Lift every held click and cancel open scrolls.
    pub(super) fn release(&mut self) -> Vec<InputEvent> {
        let mut evs = Vec::new();
        for s in &mut self.surfaces {
            s.end_scroll(ScrollPhase::Cancel, &mut evs);
            if let Some(button) = s.held.take() {
                evs.push(event(InputKind::MouseButtonUp, button, 0, 0, 0));
            }
        }
        evs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::scroll::ScrollEvent;
    use crate::quic::{GRANT_ALL, GRANT_KEYBOARD};

    fn single(finger: u8, touch: bool, x: f64, y: f64) -> Contact {
        Contact {
            pad: 0,
            surface: SINGLE as u8,
            finger,
            touch,
            click: None,
            x,
            y,
            raw: false,
        }
    }

    fn on(surface: usize, touch: bool, click: bool, x: f64, y: f64) -> Contact {
        Contact {
            surface: surface as u8,
            click: Some(click),
            ..single(0, touch, x, y)
        }
    }

    fn moved(evs: &[InputEvent]) -> (i32, i32) {
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(evs[0].kind, InputKind::MouseMove);
        (evs[0].x, evs[0].y)
    }

    fn kinds(evs: &[InputEvent]) -> Vec<(InputKind, u32)> {
        evs.iter().map(|e| (e.kind, e.code)).collect()
    }

    #[test]
    fn the_lead_finger_moves_the_pointer_like_a_trackpad() {
        let mut t = Touchpads::default();
        assert!(
            t.contact(single(0, true, 0.2, 0.2), 1080, GRANT_ALL)
                .is_empty(),
            "landing"
        );
        // A third of the width is half a stream height; y travel is scaled by the pad's aspect.
        let (dx, dy) = moved(&t.contact(
            single(0, true, 0.2 + 1.0 / 3.0, 0.2 + 1.0 / 3.0),
            1080,
            GRANT_ALL,
        ));
        assert!((539..=540).contains(&dx), "{dx}");
        assert!((303..=304).contains(&dy), "{dy}");
        assert!(
            t.contact(single(1, true, 0.9, 0.9), 1080, GRANT_ALL)
                .is_empty(),
            "a second finger does not steer"
        );
        t.contact(single(0, false, 0.0, 0.0), 1080, GRANT_ALL);
        assert!(
            t.contact(single(1, true, 0.9, 0.9), 1080, GRANT_ALL)
                .is_empty(),
            "the next finger lands without a jump"
        );
        assert!(
            t.contact(single(1, true, 0.5, 0.9), 1080, GRANT_KEYBOARD)
                .is_empty(),
            "no pointer grant, no motion"
        );
    }

    #[test]
    fn a_click_is_left_or_right_with_two_fingers() {
        let mut t = Touchpads::default();
        t.contact(single(0, true, 0.5, 0.5), 1080, GRANT_ALL);
        assert_eq!(
            kinds(&[t.button_click(true, GRANT_ALL).unwrap()]),
            [(InputKind::MouseButtonDown, MOUSE_LEFT)]
        );
        t.contact(single(1, true, 0.6, 0.5), 1080, GRANT_ALL);
        assert_eq!(
            kinds(&[t.button_click(false, GRANT_ALL).unwrap()]),
            [(InputKind::MouseButtonUp, MOUSE_LEFT)],
            "the release lifts what the press held"
        );
        assert_eq!(
            kinds(&[t.button_click(true, GRANT_ALL).unwrap()]),
            [(InputKind::MouseButtonDown, MOUSE_RIGHT)]
        );
        assert_eq!(
            kinds(&t.release()),
            [(InputKind::MouseButtonUp, MOUSE_RIGHT)]
        );
    }

    #[test]
    fn a_steam_pad_points_right_and_scrolls_left() {
        let mut t = Touchpads::default();
        t.contact(on(RIGHT, true, false, 0.5, 0.5), 1080, GRANT_ALL);
        let (dx, _) = moved(&t.contact(on(RIGHT, true, false, 0.75, 0.5), 1080, GRANT_ALL));
        assert_eq!(dx, 216, "a quarter of 0.8 heights");
        assert_eq!(
            kinds(&t.contact(on(RIGHT, true, true, 0.75, 0.5), 1080, GRANT_ALL)),
            [(InputKind::MouseButtonDown, MOUSE_LEFT)]
        );

        t.contact(on(LEFT, true, false, 0.5, 0.5), 1080, GRANT_ALL);
        let up = t.contact(on(LEFT, true, false, 0.5, 0.4), 1080, GRANT_ALL);
        let s = ScrollEvent::from_event(&up[0]).unwrap();
        assert_eq!(
            (s.source, s.axis, s.phase),
            (ScrollSource::Finger, 0, ScrollPhase::Begin)
        );
        assert!(s.delta > 0, "finger up is positive");
        let lift = t.contact(on(LEFT, false, true, 0.5, 0.4), 1080, GRANT_ALL);
        assert_eq!(
            ScrollEvent::from_event(&lift[0]).unwrap().phase,
            ScrollPhase::End
        );
        assert_eq!(
            kinds(&lift[1..]),
            [(InputKind::MouseButtonDown, MOUSE_RIGHT)]
        );
    }

    fn triton_report(buttons: u32, left: (i16, i16), right: (i16, i16)) -> RichInput {
        let mut data = [0u8; crate::quic::HID_REPORT_MAX];
        data[0] = 0x42;
        data[2..6].copy_from_slice(&buttons.to_le_bytes());
        data[18..20].copy_from_slice(&left.0.to_le_bytes());
        data[20..22].copy_from_slice(&left.1.to_le_bytes());
        data[24..26].copy_from_slice(&right.0.to_le_bytes());
        data[26..28].copy_from_slice(&right.1.to_le_bytes());
        data[30] = 0xAB;
        RichInput::HidReport {
            pad: 0,
            len: 46,
            data,
        }
    }

    #[test]
    fn a_raw_steam_controller_2_report_gives_up_its_trackpads() {
        let a = 0x1;
        let mut rich = triton_report(a | TRITON_RPAD.0 | TRITON_RPAD.1, (0, 0), (16384, 16384));
        let (contacts, forward) = route(&mut rich, false);
        assert!(forward, "the pad still plays");
        let right = contacts[1];
        assert_eq!(
            (right.surface, right.touch, right.click, right.raw),
            (2, true, Some(true), true)
        );
        assert_eq!((right.x, right.y), (0.75, 0.25), "+y up becomes screen y");
        let RichInput::HidReport { data, .. } = rich else {
            unreachable!()
        };
        assert_eq!(
            u32::from_le_bytes([data[2], data[3], data[4], data[5]]),
            a,
            "A survives"
        );
        assert!(
            data[18..30].iter().all(|&b| b == 0),
            "the host sees no trackpad"
        );
        assert_eq!(data[30], 0xAB, "the IMU survives");

        let mut full = triton_report(TRITON_RPAD.0, (0, 0), (0, 0));
        assert!(
            !route(&mut full, true).1,
            "a full mouse pad keeps its reports"
        );
    }

    #[test]
    fn a_raw_feed_silences_the_typed_echo() {
        let mut t = Touchpads::default();
        let (contacts, _) = route(&mut triton_report(TRITON_RPAD.0, (0, 0), (0, 0)), false);
        t.contact(contacts[1], 1080, GRANT_ALL);
        assert!(t
            .contact(on(RIGHT, true, true, 0.9, 0.9), 1080, GRANT_ALL)
            .is_empty());
        assert!(t.button_click(true, GRANT_ALL).is_none());
    }
}
