//! A stylus's [`PenTransition`]s → the CoreGraphics tablet events the macOS pen posts.
//!
//! Pure like [`crate::cgevent_plan`]; `cgevent.rs` builds and posts the [`Post`]s. The pen lands
//! on the first screen's display, as the Windows pen does.
//!
//! - Entering range is one tablet-proximity event naming a pen or an eraser, with fixed device
//!   ids and the capability mask: Adobe apps take a pen only with both.
//! - Each sample is a mouse event with the tablet-point subtype: moved in hover; left down,
//!   dragged and up for the tip. Pressure is 0 in hover.
//! - Barrel buttons are bits of the tablet event, as the Linux and Windows pens keep them; none
//!   makes a right click.
//! - Leaving range lifts the tip first. `PenTracker` re-enters on a tool switch and after a
//!   silence, so both arrive here as transitions.

use crate::cgevent_plan::{clamp_to, ty, Post, Rect, CLICK_SLOP};
use punktfunk_core::quic::{
    PenSample, PenTool, PenTransition, PEN_ANGLE_UNKNOWN, PEN_BARREL1, PEN_BARREL2,
    PEN_TILT_UNKNOWN,
};
use std::time::{Duration, Instant};

/// The numbers a tablet event carries: `IOLLEvent.h` and `CGEventTypes.h` values, and the ids
/// this pen goes by.
pub(crate) mod tablet {
    /// `kCGEventMouseSubtypeTabletPoint`.
    pub const SUBTYPE_POINT: i64 = 1;
    /// `NX_TABLET_POINTER_PEN`, `NX_TABLET_POINTER_ERASER`.
    pub const PEN: u32 = 1;
    pub const ERASER: u32 = 3;
    /// Device id, absolute x and y, buttons, tilt x and y, pressure.
    pub const CAPS: u32 = 0x5C7;
    /// `NX_TABLET_CAPABILITY_ROTATIONMASK`.
    pub const CAP_ROTATION: u32 = 0x2000;
    /// `NX_TABLET_BUTTON_PENTIPMASK`, `..._PENLOWERSIDEMASK`, `..._PENUPPERSIDEMASK`.
    pub const TIP: u32 = 0x1;
    pub const BARREL1: u32 = 0x2;
    pub const BARREL2: u32 = 0x4;
    /// Every proximity and point event carries this device id; an app pairs them by it.
    pub const DEVICE_ID: i64 = 0x50;
    /// The pid.codes vendor and "PF" product the Linux pen carries too.
    pub const VENDOR_ID: i64 = 0x1209;
    pub const TABLET_ID: i64 = 0x5046;
    pub const UNIQUE_ID: i64 = 1;
    /// A Wacom grip pen's vendor pointer type.
    pub const VENDOR_POINTER_TYPE: i64 = 0x802;
}

/// Degrees of tilt an app reads as 1.0: a Wacom pen's full tilt, which Qt multiplies back by.
const FULL_TILT_DEG: f64 = 60.0;

/// The least pressure a tip down posts: a stylus with no pressure axis sends 0, and an app inks
/// nothing at 0. One step of the Windows pen's 0..1024.
const MIN_INK: f64 = 1.0 / 1024.0;

/// One session's pen: where it is and what it holds.
pub(crate) struct PenPlanner {
    at: (f64, f64),
    /// The latest sample's pressure, 0..1. Posted only while the tip is down.
    pressure: f64,
    tilt: (f64, f64),
    /// Degrees, when the pen reports its barrel roll.
    roll: Option<f64>,
    pointer: u32,
    in_range: bool,
    /// `tablet::TIP` and the barrel bits held.
    buttons: u32,
    /// The latest tip down: when, where, and its click count.
    press: Option<(Instant, (f64, f64), i64)>,
    double_click: Duration,
}

impl PenPlanner {
    pub fn new(double_click: Duration) -> Self {
        PenPlanner {
            at: (0.0, 0.0),
            pressure: 0.0,
            tilt: (0.0, 0.0),
            roll: None,
            pointer: tablet::PEN,
            in_range: false,
            buttons: 0,
            press: None,
            double_click,
        }
    }

    /// The events one batch of transitions posts, the pen mapped onto `bounds`.
    pub fn plan(&mut self, transitions: &[PenTransition], bounds: Rect, now: Instant) -> Vec<Post> {
        let mut out = Vec::new();
        for (i, t) in transitions.iter().enumerate() {
            match *t {
                PenTransition::ProximityIn { tool } => {
                    // The tracker follows an entry with its first sample: the entry takes its
                    // place and whether it reports roll.
                    if let Some(PenTransition::Motion { sample }) = transitions.get(i + 1) {
                        self.sample(sample, bounds);
                    }
                    self.pointer = match tool {
                        PenTool::Eraser => tablet::ERASER,
                        PenTool::Pen | PenTool::Unknown => tablet::PEN,
                    };
                    self.in_range = true;
                    out.push(self.proximity(true));
                }
                PenTransition::Motion { sample } => {
                    self.sample(&sample, bounds);
                    out.push(self.point(self.motion_ty()));
                }
                PenTransition::TipDown => {
                    self.buttons |= tablet::TIP;
                    self.count_press(now);
                    out.push(self.point(ty::LEFT_DOWN));
                }
                PenTransition::ButtonsChanged { pressed, released } => {
                    self.buttons = (self.buttons | barrels(pressed)) & !barrels(released);
                    out.push(self.point(self.motion_ty()));
                }
                PenTransition::TipUp => {
                    self.buttons &= !tablet::TIP;
                    out.push(self.point(ty::LEFT_UP));
                }
                PenTransition::ProximityOut => {
                    out.push(self.proximity(false));
                    self.in_range = false;
                    self.buttons = 0;
                }
            }
        }
        out
    }

    /// What a pen still in range owes before it goes: its buttons, its tip, then its range.
    pub fn release(&mut self, now: Instant) -> Vec<Post> {
        let mut owed = Vec::new();
        let held = self.buttons & (tablet::BARREL1 | tablet::BARREL2);
        if held != 0 {
            owed.push(PenTransition::ButtonsChanged {
                pressed: 0,
                released: PEN_BARREL1 | PEN_BARREL2,
            });
        }
        if self.buttons & tablet::TIP != 0 {
            owed.push(PenTransition::TipUp);
        }
        if self.in_range {
            owed.push(PenTransition::ProximityOut);
        }
        // None of these reads the bounds.
        self.plan(&owed, (0.0, 0.0, 0.0, 0.0), now)
    }

    fn sample(&mut self, s: &PenSample, b: Rect) {
        let (x, y) = (f64::from(s.x), f64::from(s.y));
        self.at = clamp_to(b, (b.0 + x * b.2, b.1 + y * b.3));
        self.pressure = f64::from(s.pressure) / f64::from(u16::MAX);
        self.tilt = if s.tilt_deg != PEN_TILT_UNKNOWN && s.azimuth_deg != PEN_ANGLE_UNKNOWN {
            let (sin, cos) = f64::from(s.azimuth_deg).to_radians().sin_cos();
            let axis = |v: f64| (v * f64::from(s.tilt_deg) / FULL_TILT_DEG).clamp(-1.0, 1.0);
            // The azimuth runs clockwise from north. macOS reads tilt y up, the other pens down.
            (axis(sin), axis(cos))
        } else {
            (0.0, 0.0)
        };
        self.roll = (s.roll_deg != PEN_ANGLE_UNKNOWN).then(|| f64::from(s.roll_deg % 360));
    }

    fn motion_ty(&self) -> u32 {
        if self.buttons & tablet::TIP != 0 {
            ty::LEFT_DRAGGED
        } else {
            ty::MOVED
        }
    }

    /// A tip down close to the last one in time and place counts up, as a mouse press does.
    fn count_press(&mut self, now: Instant) {
        let count = match self.press {
            Some((when, at, n))
                if now.saturating_duration_since(when) <= self.double_click
                    && (at.0 - self.at.0).abs() <= CLICK_SLOP
                    && (at.1 - self.at.1).abs() <= CLICK_SLOP =>
            {
                n + 1
            }
            _ => 1,
        };
        self.press = Some((now, self.at, count));
    }

    fn point(&self, ty: u32) -> Post {
        let tip = self.buttons & tablet::TIP != 0;
        let clicks = match ty {
            ty::MOVED => 0,
            _ => self.press.map_or(1, |p| p.2),
        };
        Post::Tablet {
            ty,
            at: self.at,
            buttons: self.buttons,
            pressure: if tip { self.pressure.max(MIN_INK) } else { 0.0 },
            tilt: self.tilt,
            rotation: self.roll.unwrap_or(0.0),
            clicks,
        }
    }

    fn proximity(&self, enter: bool) -> Post {
        let roll = if self.roll.is_some() {
            tablet::CAP_ROTATION
        } else {
            0
        };
        Post::Proximity {
            enter,
            pointer: self.pointer,
            caps: tablet::CAPS | roll,
            at: self.at,
        }
    }
}

/// The wire's barrel bits as tablet button bits.
fn barrels(wire: u8) -> u32 {
    let bit = |b: u8, t: u32| if wire & b != 0 { t } else { 0 };
    bit(PEN_BARREL1, tablet::BARREL1) | bit(PEN_BARREL2, tablet::BARREL2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::{PenBatch, PenTracker, PEN_IN_RANGE, PEN_TOUCHING};

    const DISPLAY: Rect = (1920.0, 0.0, 2560.0, 1440.0);

    fn hover(x: f32, y: f32) -> PenSample {
        PenSample {
            state: PEN_IN_RANGE,
            x,
            y,
            ..Default::default()
        }
    }

    fn touch(x: f32, y: f32, pressure: u16) -> PenSample {
        PenSample {
            state: PEN_IN_RANGE | PEN_TOUCHING,
            pressure,
            ..hover(x, y)
        }
    }

    /// Feeds samples through a real tracker, as the pen sink does, one batch each.
    struct Pen {
        tracker: PenTracker,
        plan: PenPlanner,
        seq: u16,
        now: Instant,
    }

    impl Pen {
        fn new() -> Self {
            Pen {
                tracker: PenTracker::default(),
                plan: PenPlanner::new(Duration::from_millis(500)),
                seq: 0,
                now: Instant::now(),
            }
        }

        fn send(&mut self, s: PenSample) -> Vec<Post> {
            let mut t = Vec::new();
            self.tracker.apply(&PenBatch::new(self.seq, &[s]), &mut t);
            self.seq += 1;
            self.now += Duration::from_millis(10);
            self.plan.plan(&t, DISPLAY, self.now)
        }
    }

    /// `(ty, buttons, pressure)` of each tablet event.
    fn points(posts: &[Post]) -> Vec<(u32, u32, f64)> {
        (posts.iter())
            .filter_map(|p| match *p {
                Post::Tablet {
                    ty,
                    buttons,
                    pressure,
                    ..
                } => Some((ty, buttons, pressure)),
                _ => None,
            })
            .collect()
    }

    fn proximity(enter: bool, pointer: u32) -> Post {
        Post::Proximity {
            enter,
            pointer,
            caps: tablet::CAPS,
            at: (3200.0, 720.0),
        }
    }

    /// Entering range announces the pen at its first sample's place before that sample.
    #[test]
    fn a_pen_enters_range_before_its_first_sample() {
        let mut pen = Pen::new();
        let posts = pen.send(hover(0.5, 0.5));
        assert_eq!(posts[0], proximity(true, tablet::PEN));
        let Post::Tablet { ty, at, clicks, .. } = posts[1] else {
            panic!("{posts:?}")
        };
        assert_eq!((ty, at, clicks), (ty::MOVED, (3200.0, 720.0), 0));
        assert_eq!(posts.len(), 2);
    }

    /// A stroke presses at its first contact with that contact's pressure, drags with each
    /// sample's, and lifts with none. Hover carries no pressure.
    #[test]
    fn a_stroke_carries_its_pressure_and_hover_none() {
        let mut pen = Pen::new();
        pen.send(hover(0.5, 0.5));
        let tip = tablet::TIP;
        let posts = [
            pen.send(touch(0.5, 0.5, 16384)),
            pen.send(touch(0.5, 0.6, 49151)),
            pen.send(hover(0.5, 0.6)),
            pen.send(hover(0.6, 0.6)),
        ]
        .concat();
        let quarter = 16384.0 / 65535.0;
        assert_eq!(
            points(&posts),
            [
                (ty::MOVED, 0, 0.0),
                (ty::LEFT_DOWN, tip, quarter),
                (ty::LEFT_DRAGGED, tip, 49151.0 / 65535.0),
                (ty::LEFT_DRAGGED, tip, MIN_INK),
                (ty::LEFT_UP, 0, 0.0),
                (ty::MOVED, 0, 0.0),
            ]
        );
    }

    /// A pen with no pressure axis still inks.
    #[test]
    fn a_tip_without_pressure_still_inks() {
        let mut pen = Pen::new();
        let posts = pen.send(touch(0.5, 0.5, 0));
        assert_eq!(points(&posts)[1], (ty::LEFT_DOWN, tablet::TIP, MIN_INK));
    }

    /// 60° is full tilt; east is +x and north +y; past 60° clamps.
    #[test]
    fn tilt_is_a_sixtieth_of_a_degree_per_unit() {
        let tilt = |tilt_deg, azimuth_deg| {
            let mut pen = Pen::new();
            let s = PenSample {
                tilt_deg,
                azimuth_deg,
                ..hover(0.5, 0.5)
            };
            match pen.send(s)[1] {
                Post::Tablet { tilt, .. } => tilt,
                ref other => panic!("{other:?}"),
            }
        };
        let near = |(x, y): (f64, f64), (wx, wy): (f64, f64)| {
            assert!((x - wx).abs() < 1e-9 && (y - wy).abs() < 1e-9, "{x},{y}");
        };
        near(tilt(30, 90), (0.5, 0.0));
        near(tilt(60, 0), (0.0, 1.0));
        near(tilt(30, 180), (0.0, -0.5));
        near(tilt(90, 270), (-1.0, 0.0));
        near(tilt(PEN_TILT_UNKNOWN, 90), (0.0, 0.0));
    }

    /// A pen that reports roll says so on entry and posts it in degrees.
    #[test]
    fn roll_is_announced_and_posted_in_degrees() {
        let mut pen = Pen::new();
        let posts = pen.send(PenSample {
            roll_deg: 400,
            ..hover(0.5, 0.5)
        });
        let Post::Proximity { caps, .. } = posts[0] else {
            panic!("{posts:?}")
        };
        assert_eq!(caps, tablet::CAPS | tablet::CAP_ROTATION);
        let Post::Tablet { rotation, .. } = posts[1] else {
            panic!("{posts:?}")
        };
        assert_eq!(rotation, 40.0);
    }

    /// Turning the pen round lifts the pen, leaves range, and enters again as the eraser.
    #[test]
    fn an_eraser_is_a_new_entry() {
        let mut pen = Pen::new();
        pen.send(touch(0.5, 0.5, 30000));
        let posts = pen.send(PenSample {
            tool: PenTool::Eraser,
            ..touch(0.5, 0.5, 30000)
        });
        assert_eq!(points(&posts[..1]), [(ty::LEFT_UP, 0, 0.0)]);
        assert_eq!(posts[1], proximity(false, tablet::PEN));
        assert_eq!(posts[2], proximity(true, tablet::ERASER));
        assert_eq!(
            points(&posts[3..]),
            [
                (ty::MOVED, 0, 0.0),
                (ty::LEFT_DOWN, tablet::TIP, 30000.0 / 65535.0)
            ]
        );
    }

    /// Barrel buttons are bits on the tablet event, in hover and while drawing; nothing posts
    /// a right button.
    #[test]
    fn barrel_buttons_ride_the_tablet_event() {
        let mut pen = Pen::new();
        pen.send(hover(0.5, 0.5));
        let barrel = |s: PenSample, b| PenSample {
            state: s.state | b,
            ..s
        };
        let posts = [
            pen.send(barrel(hover(0.5, 0.5), PEN_BARREL1)),
            pen.send(barrel(touch(0.5, 0.5, 30000), PEN_BARREL1 | PEN_BARREL2)),
            pen.send(touch(0.5, 0.5, 30000)),
        ]
        .concat();
        let (b1, b2, tip) = (tablet::BARREL1, tablet::BARREL2, tablet::TIP);
        let types_and_buttons: Vec<(u32, u32)> =
            points(&posts).iter().map(|&(t, b, _)| (t, b)).collect();
        assert_eq!(
            types_and_buttons,
            [
                (ty::MOVED, 0),
                (ty::MOVED, b1),
                (ty::MOVED, b1),
                (ty::LEFT_DOWN, b1 | tip),
                (ty::LEFT_DRAGGED, b1 | b2 | tip),
                (ty::LEFT_DRAGGED, b1 | b2 | tip),
                (ty::LEFT_DRAGGED, tip),
            ]
        );
    }

    /// Two quick taps in place are a double click.
    #[test]
    fn quick_taps_in_place_count_up() {
        let mut pen = Pen::new();
        let mut clicks = Vec::new();
        for _ in 0..2 {
            for p in [pen.send(touch(0.5, 0.5, 30000)), pen.send(hover(0.5, 0.5))].concat() {
                if let Post::Tablet {
                    ty: ty::LEFT_DOWN,
                    clicks: n,
                    ..
                } = p
                {
                    clicks.push(n);
                }
            }
        }
        assert_eq!(clicks, [1, 2]);
    }

    /// The far edge of the picture stays on the display.
    #[test]
    fn the_pen_lands_on_the_display() {
        let mut pen = Pen::new();
        let Post::Tablet { at, .. } = pen.send(hover(1.0, 1.0))[1] else {
            panic!()
        };
        assert_eq!(at, (4479.0, 1439.0));
    }

    /// A pen dropped mid-stroke lets go of its buttons and tip and leaves range, once.
    #[test]
    fn release_lifts_what_the_pen_holds_once() {
        let mut pen = Pen::new();
        pen.send(PenSample {
            state: PEN_IN_RANGE | PEN_TOUCHING | PEN_BARREL2,
            ..touch(0.5, 0.5, 30000)
        });
        let posts = pen.plan.release(pen.now);
        assert_eq!(
            points(&posts),
            [
                (ty::LEFT_DRAGGED, tablet::TIP, 30000.0 / 65535.0),
                (ty::LEFT_UP, 0, 0.0)
            ]
        );
        assert_eq!(posts[2], proximity(false, tablet::PEN));
        assert!(pen.plan.release(pen.now).is_empty());
    }
}
