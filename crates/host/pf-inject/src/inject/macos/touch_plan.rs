//! Touch contacts → the pointer, scroll and gesture input macOS emulates touch with.
//!
//! macOS takes no injected touches. This follows Sidecar's emulation for UI that is not
//! touch-native (TN3212): a tap clicks; a touch that pans scrolls with phases, then coasts
//! with momentum after the lift; a long press holds the right button and drags with the
//! finger; two fingers pan to scroll, or pinch and rotate as the trackpad's magnify and rotate
//! gestures together. A two-finger tap and a third finger post nothing. The first contact moves
//! the pointer to it: scroll and gestures go to the window under the pointer.
//!
//! Pure, with time passed in. [`TouchPlanner::due`] names the next instant it posts with no
//! touch arriving (a long press, a momentum frame). `cgevent_plan::Planner` turns its pointer
//! and button [`Act`]s into mouse events, so a double tap counts up like a double click.

use crate::cgevent_plan::{momentum, phase, Desk, Post};
use crate::plan::Abs;
use punktfunk_core::input::{InputEvent, InputKind};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The trackpad gesture event, which has no public API, as shipping apps post it. Private
/// numbers, checked on macOS 27.
pub(crate) mod gesture {
    /// `NSEventTypeGesture`.
    pub const TYPE: u32 = 29;
    /// Integer field: the gesture's `IOHIDEventType`.
    pub const SUBTYPE_FIELD: u32 = 110;
    /// `kIOHIDEventTypeZoom`, `kIOHIDEventTypeRotation`.
    pub const ZOOM: i64 = 8;
    pub const ROTATE: i64 = 5;
    /// Double fields: the magnification since the last event, and the degrees turned since it,
    /// counter-clockwise positive.
    pub const MAGNIFY_FIELD: u32 = 113;
    pub const ROTATE_FIELD: u32 = 114;
    /// Integer field: the `IOHIDEventPhaseBits` phase.
    pub const PHASE_FIELD: u32 = 132;
    pub const BEGAN: u32 = 1;
    pub const CHANGED: u32 = 2;
    pub const ENDED: u32 = 4;
    pub const CANCELLED: u32 = 8;
}

/// AppKit's `NSPressGestureRecognizer` defaults: a finger held this long without moving
/// [`SLOP`] points is a long press. A finger that moves further pans.
const LONG_PRESS: Duration = Duration::from_millis(500);
const SLOP: f64 = 10.0;
/// One momentum frame per 60 Hz refresh.
const FRAME: Duration = Duration::from_millis(16);
/// The release velocity is measured over this much of the pan; a finger that rested longer
/// before the lift does not coast.
const FLING_WINDOW: Duration = Duration::from_millis(100);
/// Points per second: the slowest lift that coasts, and the speed a coast ends at.
const MIN_FLING: f64 = 100.0;
const STOP: f64 = 20.0;
/// Speed kept per millisecond of coasting: `UIScrollView`'s normal deceleration rate.
const DECAY_PER_MS: f64 = 0.998;
/// Contacts followed at once: the Windows touch device's limit. Further downs are dropped.
const MAX_CONTACTS: usize = 10;

type P = (f64, f64);

/// What a touch asks of the injector.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Act {
    /// Move the pointer to this desktop point.
    Point(P),
    /// Press or lift a wire mouse button: 1 left, 3 right.
    Button(u32, bool),
    Post(Post),
}

/// A pair of fingers: their midpoint, the distance between them, and the angle from the first
/// to the second in radians, y down.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Pair {
    mid: P,
    spread: f64,
    angle: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Idle,
    /// One finger, not yet a tap, a pan or a press.
    Pending {
        id: u32,
        start: P,
        since: Instant,
    },
    /// One finger scrolling.
    Pan {
        id: u32,
        last: P,
    },
    /// One finger past the long press: the right button is down.
    Press {
        id: u32,
    },
    /// Two fingers, not yet a pan or a pinch.
    Two {
        start: Pair,
    },
    /// Two fingers scrolling by their midpoint.
    TwoPan {
        last: P,
    },
    /// Two fingers magnifying and rotating.
    Pinch {
        last: Pair,
    },
    /// Nothing posts until every finger lifts.
    Spent,
}

/// A scroll still moving after the lift.
#[derive(Clone, Copy, Debug)]
struct Coast {
    /// Points per second, in scroll terms: content moves along it.
    v: P,
    at: Instant,
    begun: bool,
}

pub(crate) struct TouchPlanner {
    contacts: Vec<(u32, P)>,
    mode: Mode,
    /// Recent places of the scrolling finger or midpoint, for the release velocity.
    track: VecDeque<(Instant, P)>,
    /// The fraction of a point each scroll axis owes.
    frac: P,
    coast: Option<Coast>,
}

impl TouchPlanner {
    pub fn new() -> Self {
        TouchPlanner {
            contacts: Vec::new(),
            mode: Mode::Idle,
            track: VecDeque::new(),
            frac: (0.0, 0.0),
            coast: None,
        }
    }

    /// What one wire touch asks for, on `desk`. `flags` are the modifiers held, which scroll
    /// and gesture events carry. A down for a live id moves it; a move or up for an id not
    /// down is dropped.
    pub fn step(&mut self, ev: &InputEvent, now: Instant, desk: &Desk, flags: u64) -> Vec<Act> {
        let mut out = Vec::new();
        self.fire_press(now, &mut out);
        let known = self.contacts.iter().position(|c| c.0 == ev.code);
        match (ev.kind, known) {
            (InputKind::TouchDown | InputKind::TouchMove, _) => {
                let Some(abs) = Abs::of(ev) else {
                    return out;
                };
                let ((fx, fy), b) = (abs.normalized(), desk.screen(abs.screen));
                let p = (b.0 + fx * b.2, b.1 + fy * b.3);
                match known {
                    Some(i) => {
                        self.contacts[i].1 = p;
                        self.moved(ev.code, now, flags, &mut out);
                    }
                    None if ev.kind == InputKind::TouchDown => {
                        self.down(ev.code, p, now, flags, &mut out)
                    }
                    None => {}
                }
            }
            (InputKind::TouchUp, Some(i)) => {
                self.contacts.remove(i);
                self.up(ev.code, now, flags, &mut out);
            }
            _ => {}
        }
        out
    }

    /// When the planner next posts with no touch arriving.
    pub fn due(&self) -> Option<Instant> {
        match (self.mode, self.coast) {
            (Mode::Pending { since, .. }, _) => Some(since + LONG_PRESS),
            (_, Some(c)) => Some(c.at + FRAME),
            _ => None,
        }
    }

    /// The long press or momentum frame due by `now`.
    pub fn flush(&mut self, now: Instant, flags: u64) -> Vec<Act> {
        let mut out = Vec::new();
        self.fire_press(now, &mut out);
        self.coast_frame(now, flags, &mut out);
        out
    }

    /// The end of whatever a vanished session left open: a scroll or gesture cancels, a coast
    /// ends, a pressed right button lifts. Leaves nothing down.
    pub fn release(&mut self, flags: u64) -> Vec<Act> {
        let mut out = Vec::new();
        match self.mode {
            Mode::Pan { .. } | Mode::TwoPan { .. } => {
                self.scroll_edge(phase::CANCELLED, 0, flags, &mut out)
            }
            Mode::Pinch { .. } => pinch(0.0, 0.0, gesture::CANCELLED, flags, &mut out),
            Mode::Press { .. } => out.push(Act::Button(3, false)),
            _ => {}
        }
        self.end_coast(flags, &mut out);
        self.contacts.clear();
        self.track.clear();
        self.mode = Mode::Idle;
        out
    }

    fn down(&mut self, id: u32, p: P, now: Instant, flags: u64, out: &mut Vec<Act>) {
        if self.contacts.len() >= MAX_CONTACTS {
            return;
        }
        self.end_coast(flags, out);
        self.contacts.push((id, p));
        self.mode = match (self.contacts.len(), self.mode) {
            (1, _) => {
                out.push(Act::Point(p));
                Mode::Pending {
                    id,
                    start: p,
                    since: now,
                }
            }
            (2, Mode::Pending { .. }) => Mode::Two { start: self.pair() },
            (2, Mode::Pan { .. }) => {
                self.scroll_edge(phase::ENDED, 0, flags, out);
                Mode::Two { start: self.pair() }
            }
            (_, Mode::Press { id }) => Mode::Press { id },
            (_, Mode::TwoPan { .. }) => {
                self.scroll_edge(phase::ENDED, 0, flags, out);
                Mode::Spent
            }
            (_, Mode::Pinch { .. }) => {
                pinch(0.0, 0.0, gesture::ENDED, flags, out);
                Mode::Spent
            }
            _ => Mode::Spent,
        };
    }

    fn moved(&mut self, id: u32, now: Instant, flags: u64, out: &mut Vec<Act>) {
        let at = |s: &Self| {
            s.contacts
                .iter()
                .find(|c| c.0 == id)
                .map_or((0.0, 0.0), |c| c.1)
        };
        match self.mode {
            Mode::Pending {
                id: own,
                start,
                since,
            } if own == id => {
                let p = at(self);
                if dist(p, start) > SLOP {
                    self.begin_scroll(start, p, since, now, flags, out);
                    self.mode = Mode::Pan { id, last: p };
                }
            }
            Mode::Pan { id: own, last } if own == id => {
                let p = at(self);
                self.scroll_by(sub(p, last), phase::CHANGED, 0, flags, out);
                self.note(now, p);
                self.mode = Mode::Pan { id, last: p };
            }
            Mode::Press { id: own } if own == id => out.push(Act::Point(at(self))),
            Mode::Two { start } => {
                let now_pair = self.pair();
                let pan = dist(now_pair.mid, start.mid);
                let turn = turned(start.angle, now_pair.angle).abs() * now_pair.spread / 2.0;
                let pinched = (now_pair.spread - start.spread).abs().max(turn);
                if pan.max(pinched) <= SLOP {
                    return;
                }
                if pan >= pinched {
                    self.begin_scroll(start.mid, now_pair.mid, now, now, flags, out);
                    self.mode = Mode::TwoPan { last: now_pair.mid };
                } else {
                    let (scale, deg) = pinch_since(start, now_pair);
                    pinch(scale, deg, gesture::BEGAN, flags, out);
                    self.mode = Mode::Pinch { last: now_pair };
                }
            }
            Mode::TwoPan { last } => {
                let mid = self.pair().mid;
                self.scroll_by(sub(mid, last), phase::CHANGED, 0, flags, out);
                self.note(now, mid);
                self.mode = Mode::TwoPan { last: mid };
            }
            Mode::Pinch { last } => {
                let now_pair = self.pair();
                let (scale, deg) = pinch_since(last, now_pair);
                pinch(scale, deg, gesture::CHANGED, flags, out);
                self.mode = Mode::Pinch { last: now_pair };
            }
            _ => {}
        }
    }

    fn up(&mut self, id: u32, now: Instant, flags: u64, out: &mut Vec<Act>) {
        let rest = if self.contacts.is_empty() {
            Mode::Idle
        } else {
            Mode::Spent
        };
        self.mode = match self.mode {
            Mode::Pending { id: own, .. } if own == id => {
                out.extend([Act::Button(1, true), Act::Button(1, false)]);
                rest
            }
            Mode::Pan { id: own, .. } if own == id => {
                self.scroll_edge(phase::ENDED, 0, flags, out);
                self.fling(now);
                rest
            }
            Mode::Press { id: own } if own == id => {
                out.push(Act::Button(3, false));
                rest
            }
            Mode::TwoPan { .. } => {
                self.scroll_edge(phase::ENDED, 0, flags, out);
                self.fling(now);
                rest
            }
            Mode::Pinch { .. } => {
                pinch(0.0, 0.0, gesture::ENDED, flags, out);
                rest
            }
            // A lift of a finger the press does not follow.
            Mode::Press { id: own } => Mode::Press { id: own },
            _ => rest,
        };
    }

    /// A long press that fell due: the right button goes down where the finger landed.
    fn fire_press(&mut self, now: Instant, out: &mut Vec<Act>) {
        if let Mode::Pending { id, since, .. } = self.mode
            && now >= since + LONG_PRESS
        {
            out.push(Act::Button(3, true));
            self.mode = Mode::Press { id };
        }
    }

    /// A pan past the slop: the scroll may begin and begins with the whole way from `start`,
    /// so the content stays under the finger.
    fn begin_scroll(
        &mut self,
        start: P,
        p: P,
        since: Instant,
        now: Instant,
        flags: u64,
        out: &mut Vec<Act>,
    ) {
        self.frac = (0.0, 0.0);
        self.scroll_edge(phase::MAY_BEGIN, 0, flags, out);
        self.scroll_by(sub(p, start), phase::BEGAN, 0, flags, out);
        self.track.clear();
        self.note(since, start);
        self.note(now, p);
    }

    /// A finger move of `d` points as scroll: content follows it, so down and right are wheel 1
    /// up and wheel 2 left. The fraction of a point waits for the next move.
    fn scroll_by(&mut self, d: P, phase: u32, momentum: u32, flags: u64, out: &mut Vec<Act>) {
        let total = (self.frac.0 + d.0, self.frac.1 + d.1);
        let whole = (total.0.trunc(), total.1.trunc());
        self.frac = (total.0 - whole.0, total.1 - whole.1);
        let continues = phase == phase::CHANGED || momentum == momentum::CONTINUE;
        if continues && whole == (0.0, 0.0) {
            return;
        }
        out.push(Act::Post(Post::Scroll {
            lines: false,
            wheel1: whole.1 as i32,
            wheel2: whole.0 as i32,
            phase,
            momentum,
            flags,
        }));
    }

    /// A scroll event that moves nothing: a phase or momentum edge.
    fn scroll_edge(&mut self, phase: u32, momentum: u32, flags: u64, out: &mut Vec<Act>) {
        self.frac = (0.0, 0.0);
        out.push(Act::Post(Post::Scroll {
            lines: false,
            wheel1: 0,
            wheel2: 0,
            phase,
            momentum,
            flags,
        }));
    }

    fn note(&mut self, at: Instant, p: P) {
        while self.track.len() >= 32 {
            self.track.pop_front();
        }
        self.track.push_back((at, p));
    }

    /// Coast from the velocity over the last [`FLING_WINDOW`] of the pan.
    fn fling(&mut self, now: Instant) {
        let track = std::mem::take(&mut self.track);
        let Some(&(t1, p1)) = track.back() else {
            return;
        };
        if now.saturating_duration_since(t1) > FLING_WINDOW {
            return;
        }
        let Some(&(t0, p0)) =
            (track.iter()).find(|(t, _)| t1.saturating_duration_since(*t) <= FLING_WINDOW)
        else {
            return;
        };
        let dt = t1.saturating_duration_since(t0).as_secs_f64();
        if dt <= 0.0 {
            return;
        }
        let v = ((p1.0 - p0.0) / dt, (p1.1 - p0.1) / dt);
        if v.0.hypot(v.1) >= MIN_FLING {
            self.coast = Some(Coast {
                v,
                at: now,
                begun: false,
            });
        }
    }

    /// One momentum frame: the distance the decaying speed covers since the last, until it
    /// falls under [`STOP`].
    fn coast_frame(&mut self, now: Instant, flags: u64, out: &mut Vec<Act>) {
        let Some(mut c) = self.coast else {
            return;
        };
        if now < c.at + FRAME {
            return;
        }
        let ms = now.saturating_duration_since(c.at).as_secs_f64() * 1000.0;
        let keep = DECAY_PER_MS.powf(ms);
        let v = (c.v.0 * keep, c.v.1 * keep);
        if v.0.hypot(v.1) < STOP {
            self.end_coast(flags, out);
            return;
        }
        let secs = ms / 1000.0;
        let d = ((c.v.0 + v.0) / 2.0 * secs, (c.v.1 + v.1) / 2.0 * secs);
        let edge = if c.begun {
            momentum::CONTINUE
        } else {
            momentum::BEGIN
        };
        (c.v, c.at, c.begun) = (v, now, true);
        self.coast = Some(c);
        self.scroll_by(d, 0, edge, flags, out);
    }

    /// A new contact or a release stops a coast; one that began ends its momentum.
    fn end_coast(&mut self, flags: u64, out: &mut Vec<Act>) {
        if self.coast.take().is_some_and(|c| c.begun) {
            self.scroll_edge(0, momentum::END, flags, out);
        }
    }

    /// The first two fingers down.
    fn pair(&self) -> Pair {
        let (a, b) = match &self.contacts[..] {
            [a, b, ..] => (a.1, b.1),
            [a] => (a.1, a.1),
            [] => ((0.0, 0.0), (0.0, 0.0)),
        };
        Pair {
            mid: ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0),
            spread: dist(a, b),
            angle: (b.1 - a.1).atan2(b.0 - a.0),
        }
    }
}

fn dist(a: P, b: P) -> f64 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

fn sub(a: P, b: P) -> P {
    (a.0 - b.0, a.1 - b.1)
}

/// Radians from `from` to `to`, in `-π..=π`.
fn turned(from: f64, to: f64) -> f64 {
    let d = (to - from).rem_euclid(std::f64::consts::TAU);
    if d > std::f64::consts::PI {
        d - std::f64::consts::TAU
    } else {
        d
    }
}

/// The magnification and the counter-clockwise degrees from `a` to `b`. y runs down, so a turn
/// the screen shows counter-clockwise lowers the angle.
fn pinch_since(a: Pair, b: Pair) -> (f64, f64) {
    let scale = if a.spread >= 1.0 {
        b.spread / a.spread - 1.0
    } else {
        0.0
    };
    (scale, -turned(a.angle, b.angle).to_degrees())
}

/// A magnify and a rotate event at one phase: macOS emulates a pinch with both.
fn pinch(scale: f64, deg: f64, phase: u32, flags: u64, out: &mut Vec<Act>) {
    for (subtype, value) in [(gesture::ZOOM, scale), (gesture::ROTATE, deg)] {
        out.push(Act::Post(Post::Gesture {
            subtype,
            value,
            phase,
            flags,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgevent_plan::SCREENS;

    /// One display, 1000 points square; touches are sent on a 1000×1000 surface, so a pixel is
    /// a point.
    const DESK: Desk = Desk {
        cursor: (0.0, 0.0),
        screens: [(0.0, 0.0, 1000.0, 1000.0); SCREENS],
    };

    fn ev(kind: InputKind, id: u32, (x, y): (i32, i32)) -> InputEvent {
        InputEvent {
            kind,
            _pad: [0; 3],
            code: id,
            x,
            y,
            flags: (1000 << 16) | 1000,
        }
    }

    fn down(id: u32, p: (i32, i32)) -> InputEvent {
        ev(InputKind::TouchDown, id, p)
    }

    fn mv(id: u32, p: (i32, i32)) -> InputEvent {
        ev(InputKind::TouchMove, id, p)
    }

    fn up(id: u32) -> InputEvent {
        ev(InputKind::TouchUp, id, (0, 0))
    }

    struct Touch {
        p: TouchPlanner,
        t0: Instant,
    }

    impl Touch {
        fn new() -> Self {
            Touch {
                p: TouchPlanner::new(),
                t0: Instant::now(),
            }
        }

        fn t(&self, ms: u64) -> Instant {
            self.t0 + Duration::from_millis(ms)
        }

        fn at(&mut self, ms: u64, e: InputEvent) -> Vec<Act> {
            self.p.step(&e, self.t(ms), &DESK, 0)
        }

        fn flush(&mut self, ms: u64) -> Vec<Act> {
            self.p.flush(self.t(ms), 0)
        }

        /// Every momentum frame until the coast ends.
        fn coast(&mut self) -> Vec<Act> {
            let mut out = Vec::new();
            while let Some(due) = self.p.due() {
                out.extend(self.p.flush(due, 0));
            }
            out
        }

        /// A finger flicked down 80 points in 40 ms and lifted at 45 ms.
        fn flick(&mut self) {
            self.at(0, down(1, (500, 100)));
            for i in 1..=4 {
                self.at(i * 10, mv(1, (500, 100 + 20 * i as i32)));
            }
            self.at(45, up(1));
        }
    }

    /// `(wheel1, wheel2, phase, momentum)` of each scroll.
    fn scrolls(acts: &[Act]) -> Vec<(i32, i32, u32, u32)> {
        (acts.iter())
            .filter_map(|a| match *a {
                Act::Post(Post::Scroll {
                    wheel1,
                    wheel2,
                    phase,
                    momentum,
                    ..
                }) => Some((wheel1, wheel2, phase, momentum)),
                _ => None,
            })
            .collect()
    }

    /// `(subtype, value, phase)` of each gesture.
    fn gestures(acts: &[Act]) -> Vec<(i64, f64, u32)> {
        (acts.iter())
            .filter_map(|a| match *a {
                Act::Post(Post::Gesture {
                    subtype,
                    value,
                    phase,
                    ..
                }) => Some((subtype, value, phase)),
                _ => None,
            })
            .collect()
    }

    fn near(got: f64, want: f64) {
        assert!((got - want).abs() < 1e-3, "{got} vs {want}");
    }

    const LEFT_CLICK: [Act; 2] = [Act::Button(1, true), Act::Button(1, false)];

    /// The first contact brings the pointer; a lift inside the slop and the long press clicks.
    #[test]
    fn a_tap_moves_the_pointer_then_clicks() {
        let mut t = Touch::new();
        assert_eq!(t.at(0, down(1, (500, 500))), [Act::Point((500.0, 500.0))]);
        assert!(t.at(50, mv(1, (507, 506))).is_empty(), "inside the slop");
        assert_eq!(t.at(100, up(1)), LEFT_CLICK);
        assert_eq!(t.p.due(), None);
    }

    /// Past the slop a finger scrolls: may-begin, then the whole way from where it landed, then
    /// each move. The content follows the finger. A finger that rests before the lift ends the
    /// scroll with no coast.
    #[test]
    fn a_pan_scrolls_with_the_finger() {
        let (b, c, e) = (phase::BEGAN, phase::CHANGED, phase::ENDED);
        let rows = [
            ("down", (0, 20), (20, 0)),
            ("up", (0, -20), (-20, 0)),
            ("right", (20, 0), (0, 20)),
            ("left", (-20, 0), (0, -20)),
        ];
        for (name, (dx, dy), (w1, w2)) in rows {
            let mut t = Touch::new();
            t.at(0, down(1, (500, 500)));
            let mut acts = t.at(16, mv(1, (500 + dx, 500 + dy)));
            acts.extend(t.at(32, mv(1, (500 + 2 * dx, 500 + 2 * dy))));
            acts.extend(t.at(400, up(1)));
            let want = [
                (0, 0, phase::MAY_BEGIN, 0),
                (w1, w2, b, 0),
                (w1, w2, c, 0),
                (0, 0, e, 0),
            ];
            assert_eq!(scrolls(&acts), want, "{name}");
            assert_eq!(t.p.due(), None, "{name}: no coast after a rest");
        }
    }

    /// A quick lift coasts: momentum begins, decays frame by frame and ends.
    #[test]
    fn a_flick_coasts_and_decays() {
        let mut t = Touch::new();
        t.at(0, down(1, (500, 100)));
        for i in 1..=4 {
            t.at(i * 10, mv(1, (500, 100 + 20 * i as i32)));
        }
        let lift = t.at(45, up(1));
        assert_eq!(scrolls(&lift), [(0, 0, phase::ENDED, 0)]);
        assert_eq!(t.p.due(), Some(t.t(45) + FRAME));
        let coast = scrolls(&t.coast());
        let (first, last) = (coast[0], coast[coast.len() - 1]);
        assert_eq!((first.2, first.3), (0, momentum::BEGIN));
        assert!(first.0 > 20, "about 2000 points a second: {first:?}");
        assert_eq!(last, (0, 0, 0, momentum::END));
        let middle = &coast[1..coast.len() - 1];
        assert!(middle.iter().all(|s| s.3 == momentum::CONTINUE && s.1 == 0));
        let decays = coast.windows(2).all(|w| w[1].0 <= w[0].0 + 1);
        assert!(decays, "{coast:?}");
    }

    /// A finger landing on a coast stops it; one landing before its first frame ends nothing.
    #[test]
    fn a_new_contact_stops_the_coast() {
        let mut t = Touch::new();
        t.flick();
        t.flush(61);
        let acts = t.at(70, down(2, (300, 300)));
        assert_eq!(scrolls(&acts), [(0, 0, 0, momentum::END)]);
        assert_eq!(acts[1], Act::Point((300.0, 300.0)));
        assert_eq!(t.p.due(), Some(t.t(70) + LONG_PRESS), "the coast is gone");

        let mut t = Touch::new();
        t.flick();
        assert_eq!(t.at(50, down(2, (300, 300))), [Act::Point((300.0, 300.0))]);
    }

    /// A finger held still for the long press holds the right button, drags with it, and lets
    /// go with the lift. A lift past the press time with no wake between still presses first.
    #[test]
    fn a_long_press_holds_the_right_button() {
        let mut t = Touch::new();
        t.at(0, down(1, (500, 500)));
        assert_eq!(t.p.due(), Some(t.t(500)));
        assert!(t.flush(499).is_empty());
        assert_eq!(t.flush(500), [Act::Button(3, true)]);
        assert_eq!(t.at(600, mv(1, (600, 650))), [Act::Point((600.0, 650.0))]);
        assert_eq!(t.at(700, up(1)), [Act::Button(3, false)]);

        let mut t = Touch::new();
        t.at(0, down(1, (500, 500)));
        let acts = t.at(600, up(1));
        assert_eq!(acts, [Act::Button(3, true), Act::Button(3, false)]);
    }

    /// Two fingers moving together scroll by their midpoint; tapped together they post nothing.
    #[test]
    fn two_fingers_pan_and_a_two_finger_tap_is_nothing() {
        let mut t = Touch::new();
        t.at(0, down(1, (400, 500)));
        assert!(t.at(5, down(2, (600, 500))).is_empty());
        assert!(
            t.at(10, mv(1, (400, 510))).is_empty(),
            "the midpoint moved 5"
        );
        let acts = t.at(15, mv(2, (600, 520)));
        let want = [(0, 0, phase::MAY_BEGIN, 0), (15, 0, phase::BEGAN, 0)];
        assert_eq!(scrolls(&acts), want);
        let acts = t.at(20, mv(1, (400, 520)));
        assert_eq!(scrolls(&acts), [(5, 0, phase::CHANGED, 0)]);
        let acts = [t.at(400, up(1)), t.at(410, up(2))].concat();
        assert_eq!(scrolls(&acts), [(0, 0, phase::ENDED, 0)]);
        assert_eq!(t.p.due(), None);

        let mut t = Touch::new();
        let acts = [
            t.at(0, down(1, (400, 500))),
            t.at(5, down(2, (600, 500))),
            t.at(80, up(1)),
            t.at(85, up(2)),
        ]
        .concat();
        assert_eq!(acts, [Act::Point((400.0, 500.0))]);
    }

    /// Spreading fingers magnify and turning them rotates, both in every event; the first lift
    /// ends the gesture.
    #[test]
    fn a_pinch_magnifies_and_rotates_together() {
        let mut t = Touch::new();
        t.at(0, down(1, (400, 500)));
        t.at(5, down(2, (600, 500)));
        let g = gestures(&t.at(10, mv(2, (650, 500))));
        assert_eq!(g.len(), 2);
        assert_eq!((g[0].0, g[0].2), (gesture::ZOOM, gesture::BEGAN));
        near(g[0].1, 0.25);
        assert_eq!((g[1].0, g[1].2), (gesture::ROTATE, gesture::BEGAN));
        near(g[1].1, 0.0);
        let g = gestures(&t.at(20, mv(1, (450, 500))));
        assert_eq!(g[0].2, gesture::CHANGED);
        near(g[0].1, 200.0 / 250.0 - 1.0);
        let ended = [
            (gesture::ZOOM, 0.0, gesture::ENDED),
            (gesture::ROTATE, 0.0, gesture::ENDED),
        ];
        assert_eq!(gestures(&t.at(30, up(2))), ended);
        assert!(t.at(40, mv(1, (300, 300))).is_empty(), "the rest is spent");
        assert!(t.at(50, up(1)).is_empty());

        // The left finger dropping turns the pair counter-clockwise, which is positive.
        let mut t = Touch::new();
        t.at(0, down(1, (400, 500)));
        t.at(5, down(2, (600, 500)));
        let g = gestures(&t.at(10, mv(1, (400, 520))));
        near(g[1].1, (20.0f64 / 200.0).atan().to_degrees());
    }

    /// A third finger ends the gesture and nothing posts until every finger is up; then a tap
    /// works again.
    #[test]
    fn three_fingers_post_nothing_and_clean_up() {
        let mut t = Touch::new();
        t.at(0, down(1, (400, 500)));
        t.at(5, down(2, (600, 500)));
        let acts = [
            t.at(10, down(3, (500, 300))),
            t.at(20, mv(1, (300, 500))),
            t.at(30, mv(3, (500, 100))),
            t.at(40, up(2)),
            t.at(50, up(1)),
            t.at(60, up(3)),
        ]
        .concat();
        assert!(acts.is_empty(), "{acts:?}");
        t.at(100, down(4, (200, 200)));
        assert_eq!(t.at(150, up(4)), LEFT_CLICK);
    }

    /// A second finger on a one-finger scroll ends it and starts a pair.
    #[test]
    fn a_second_finger_ends_a_pan() {
        let mut t = Touch::new();
        t.at(0, down(1, (500, 500)));
        t.at(16, mv(1, (500, 530)));
        let acts = t.at(30, down(2, (700, 530)));
        assert_eq!(scrolls(&acts), [(0, 0, phase::ENDED, 0)]);
        let acts = t.at(40, mv(2, (750, 530)));
        assert_eq!(gestures(&acts)[0].2, gesture::BEGAN);
    }

    /// A vanished session's touch lets go of whatever it held, once.
    #[test]
    fn release_ends_what_touch_left_open() {
        let mut t = Touch::new();
        t.at(0, down(1, (500, 500)));
        t.at(16, mv(1, (500, 530)));
        assert_eq!(scrolls(&t.p.release(0)), [(0, 0, phase::CANCELLED, 0)]);
        assert!(t.p.release(0).is_empty());
        assert!(t.at(30, mv(1, (500, 560))).is_empty(), "no finger is down");

        let mut t = Touch::new();
        t.at(0, down(1, (500, 500)));
        t.flush(500);
        assert_eq!(t.p.release(0), [Act::Button(3, false)]);

        let mut t = Touch::new();
        t.at(0, down(1, (400, 500)));
        t.at(5, down(2, (600, 500)));
        t.at(10, mv(2, (650, 500)));
        let phases: Vec<u32> = gestures(&t.p.release(0)).iter().map(|g| g.2).collect();
        assert_eq!(phases, [gesture::CANCELLED; 2]);

        let mut t = Touch::new();
        t.flick();
        t.flush(61);
        assert_eq!(scrolls(&t.p.release(0)), [(0, 0, 0, momentum::END)]);
        assert_eq!(t.p.due(), None);
    }

    /// A move or lift for a finger that is not down, and a surface of no size, do nothing.
    #[test]
    fn strays_do_nothing() {
        let mut t = Touch::new();
        assert!(t.at(0, mv(7, (1, 1))).is_empty());
        assert!(t.at(0, up(7)).is_empty());
        let zero = InputEvent {
            flags: 0,
            ..down(7, (1, 1))
        };
        assert!(t.at(0, zero).is_empty());
        assert_eq!(t.p.due(), None);
    }
}
