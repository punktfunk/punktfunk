//! SDL fingers → host mouse for the `trackpad`/`pointer` models.
//!
//! Incremental port of Android `TouchInput.kt` and Apple `TouchMouse.swift`.
//! The `touch` model never reaches here: those contacts go on the wire as
//! real multi-touch (`Capture::on_touch_*`).
//!
//! `trackpad`: cursor stays put on down; relative delta with mild
//! acceleration. `pointer`: cursor jumps to the finger (absolute, letterbox).
//! Tap = left click · two-finger tap = right · two-finger drag = scroll ·
//! tap-then-press = held left drag · three-finger tap = cycle stats overlay.
//! Three or more fingers never scroll (the twins map a three-finger swipe to
//! their keyboard; SDL builds have none).
//!
//! SDL delivers one finger transition per event, so this machine keeps every
//! live finger and recomputes the centroid. Positions are physical window
//! pixels so ballistics port from Android 1:1; timestamps are milliseconds.
//!
//! [`FrameMode`]: `trackpad` toward a host that presents a touchpad. The fingers reach the
//! host as contacts and its OS makes motion and scroll. This machine still runs the local
//! gestures; its clicks reach the wire only toward a host that does not tap.

use std::collections::HashMap;

/// px; under this, a lift is still a tap.
const TAP_SLOP: f32 = 12.0;
/// ms after a tap, nearby: the next down starts a held left drag.
const TAP_DRAG_MS: f64 = 250.0;
/// ms of a still single finger: press left and hold until lift.
const LONG_PRESS_MS: f64 = 500.0;
/// Q24.8 wire units per DIP of two-finger pan — `delta` is DIP × 256.
const SCROLL_UNITS_PER_DIP: f32 = 256.0;
/// Degrees of two-finger twist before the quick-action ring arms. Natural scrolls
/// rotate a few degrees; a smaller twist stays a scroll.
const DIAL_ARM_DEG: f32 = 10.0;
/// Degrees at which the ring stays open after lift.
const DIAL_COMMIT_DEG: f32 = 30.0;
/// Centroid travel (px) until which an unarmed pair can still become a twist; past it,
/// the gesture is a scroll. The scroll itself never waits for it.
const DIAL_SLOP: f32 = 2.0 * TAP_SLOP;
/// ms. SDL splits one frame's fingers into separate events with the same timestamp.
/// Judge twist only against a position this fresh — a mid-scroll pair otherwise
/// looks rotated by tens of degrees.
const DIAL_FRAME_MS: f64 = 4.0;
/// ms of stillness on the other finger: a thumb-pivot is a real twist and never
/// completes a same-frame pair.
const DIAL_PIVOT_MS: f64 = 50.0;
/// Base finger-px → host-px gain, before acceleration.
const POINTER_SENS: f32 = 1.3;
/// Extra gain per px/ms above `ACCEL_SPEED_FLOOR`. `ACCEL_MAX` stops a fast swipe
/// flinging the cursor.
const ACCEL_GAIN: f32 = 0.6;
const ACCEL_SPEED_FLOOR: f32 = 0.3;
const ACCEL_MAX: f32 = 3.0;

/// GameStream mouse button ids.
const BTN_LEFT: u32 = 1;
const BTN_RIGHT: u32 = 3;

/// A frame pixel plus the frame size, the `MouseMoveAbs` packing the host rescales
/// into its output. `pointer` absolute moves carry this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Abs {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Scroll-axis lifecycle carried by [`Act::Scroll`]. A nonzero delta opens its
/// axis with `Begin` and continues as `Update`; `End` and `Cancel` close it and
/// always carry `delta` 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchScrollPhase {
    Begin,
    Update,
    End,
    Cancel,
}

/// Wire intent. `Capture` in `input.rs` sends each one and folds `CycleStats` back
/// to the run loop; the `InputKind` translation lives there so this module takes no
/// `punktfunk_core` dependency.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Act {
    MoveRel {
        dx: i32,
        dy: i32,
    },
    MoveAbs(Abs),
    /// GameStream button id; `down` is press/release.
    Button {
        gs: u32,
        down: bool,
    },
    /// `axis` 0 = vertical, 1 = horizontal; `delta` is Q24.8 DIP (256 per DIP),
    /// finger-up / finger-right positive.
    Scroll {
        axis: u32,
        delta: i32,
        phase: TouchScrollPhase,
    },
    /// Three-finger tap. The run loop owns the overlay tier.
    CycleStats,
    /// Armed twist driving the quick-action ring. `progress` 0…1; `x`/`y` are the
    /// centroid in window px. Emitted on every move once armed.
    Dial {
        progress: f32,
        clockwise: bool,
        x: f32,
        y: f32,
    },
    /// Reached `DIAL_COMMIT_DEG`; the ring stays open after lift.
    DialCommit,
    /// Ended short of commit, or wound back below the arm angle after commit.
    DialCancel,
}

/// Finger-to-finger vector at the second finger's landing, compared on every move.
#[derive(Clone, Copy)]
struct Dial {
    ids: (u64, u64),
    vec: (f32, f32),
    /// Centroid at second-finger landing. Travel past `DIAL_SLOP` before arming locks a scroll.
    anchor: (f32, f32),
    armed: bool,
    committed: bool,
}

/// Trackpad/pointer state machine. One per session; `trackpad` false is pointer.
/// Fed only direct touchscreen fingers.
pub struct Gestures {
    trackpad: bool,
    /// Physical px per DIP — the window's display scale. Scroll distance is
    /// measured in DIP so the host's pricing never depends on the panel.
    density: f32,
    /// Live fingers → window px. A move event carries only the finger that changed.
    positions: HashMap<u64, (f32, f32)>,
    /// Live fingers → last-event time (ms). The dial's same-frame test.
    times: HashMap<u64, f64>,
    active: bool,
    start: (f32, f32),
    /// First-finger down time (ms). Long-press clock.
    down_t: f64,
    max_fingers: usize,
    moved: bool,
    scrolling: bool,
    /// Centroid at the last scroll step.
    scroll_anchor: (f32, f32),
    /// Sub-unit scroll remainder, so a slow pan is not lost to truncation.
    scroll_carry: (f32, f32),
    /// Wire scroll axes (0 = vertical) currently inside a `Begin`…`End`.
    scroll_axes: [bool; 2],
    /// The pair travelled past `DIAL_SLOP` unarmed: scroll for the gesture's lifetime.
    scroll_locked: bool,
    /// Units scrolled while the pair was undecided, sent back if it becomes a twist or a tap.
    provisional: (i32, i32),
    dial: Option<Dial>,
    /// Many-finger centroid, per finger count (0 = none). Fingers never land and lift
    /// in the same event, so a count change must re-anchor, not read as travel.
    many_count: usize,
    many_anchor: (f32, f32),
    drag_held: bool,
    // Tracked finger, last position/time, and sub-pixel remainder so a slow drag
    // is not lost to integer truncation.
    track_id: Option<u64>,
    prev: (f32, f32),
    prev_t: f64,
    carry: (f32, f32),
    // Last tap's up-time and point: a nearby down inside TAP_DRAG_MS holds left.
    last_tap_up: f64,
    last_tap_pt: (f32, f32),
}

impl Gestures {
    pub fn new(trackpad: bool) -> Gestures {
        Gestures {
            trackpad,
            density: 1.0,
            positions: HashMap::new(),
            times: HashMap::new(),
            active: false,
            start: (0.0, 0.0),
            down_t: 0.0,
            max_fingers: 0,
            moved: false,
            scrolling: false,
            scroll_anchor: (0.0, 0.0),
            scroll_carry: (0.0, 0.0),
            scroll_axes: [false; 2],
            scroll_locked: false,
            provisional: (0, 0),
            dial: None,
            many_count: 0,
            many_anchor: (0.0, 0.0),
            drag_held: false,
            track_id: None,
            prev: (0.0, 0.0),
            prev_t: 0.0,
            carry: (0.0, 0.0),
            last_tap_up: 0.0,
            last_tap_pt: (0.0, 0.0),
        }
    }

    /// Physical px per DIP — the window's display scale. A non-finite or
    /// non-positive value falls back to 1 (unscaled).
    pub fn set_density(&mut self, pixels_per_dip: f32) {
        self.density = if pixels_per_dip.is_finite() && pixels_per_dip > 0.0 {
            pixels_per_dip
        } else {
            1.0
        };
    }

    /// Pointer mode jumps the cursor to `abs` on the first finger. `t` is ms.
    pub fn down(&mut self, id: u64, wx: f32, wy: f32, abs: Abs, t: f64) -> Vec<Act> {
        let mut acts = Vec::new();
        let first = self.positions.is_empty() && !self.active;
        self.positions.insert(id, (wx, wy));
        self.times.insert(id, t);
        if first {
            // A leaked open axis (an Up the engine never saw) cancels before the
            // new gesture starts.
            self.close_scroll(TouchScrollPhase::Cancel, &mut acts);
            self.active = true;
            self.start = (wx, wy);
            self.down_t = t;
            self.max_fingers = 0;
            self.moved = false;
            self.scrolling = false;
            self.scroll_carry = (0.0, 0.0);
            self.scroll_locked = false;
            self.provisional = (0, 0);
            self.dial = None;
            self.many_count = 0;
            self.drag_held = t - self.last_tap_up < TAP_DRAG_MS
                && (wx - self.last_tap_pt.0).abs() < TAP_SLOP
                && (wy - self.last_tap_pt.1).abs() < TAP_SLOP;
            self.last_tap_up = 0.0; // consume the arming either way
            if !self.trackpad {
                acts.push(Act::MoveAbs(abs)); // pointer: place the cursor before any press
            }
            if self.drag_held {
                acts.push(Act::Button {
                    gs: BTN_LEFT,
                    down: true,
                });
            }
            self.track_id = Some(id);
            self.prev = (wx, wy);
            self.prev_t = t;
            self.carry = (0.0, 0.0);
        }
        self.max_fingers = self.max_fingers.max(self.positions.len());
        // A pair that re-forms mid-scroll (2→1→2) scrolls from where it lands, not from
        // the old pair's centroid.
        if self.positions.len() == 2 && self.scrolling {
            self.scroll_anchor = self.centroid();
            self.scroll_carry = (0.0, 0.0);
        }
        match self.positions.len() {
            // Second finger: snapshot the pair vector unless this gesture is a locked scroll.
            2 if !self.scroll_locked && self.dial.is_none() => {
                if let Some((&other, &op)) = self.positions.iter().find(|(k, _)| **k != id) {
                    self.dial = Some(Dial {
                        ids: (other, id),
                        vec: (wx - op.0, wy - op.1),
                        anchor: self.centroid(),
                        armed: false,
                        committed: false,
                    });
                }
            }
            n if n > 2 => {
                acts.extend(self.end_dial(false));
                // Three or more fingers never scroll: an in-flight pair ends here.
                self.close_scroll(TouchScrollPhase::End, &mut acts);
            }
            _ => {}
        }
        acts
    }

    pub fn motion(&mut self, id: u64, wx: f32, wy: f32, abs: Abs, t: f64) -> Vec<Act> {
        if !self.active || !self.positions.contains_key(&id) {
            return Vec::new();
        }
        self.positions.insert(id, (wx, wy));
        self.times.insert(id, t);
        // Below three fingers, drop the many-finger anchor so a 3→2→3 bounce re-anchors
        // instead of reading the count change as travel.
        if self.positions.len() < 3 {
            self.many_count = 0;
        }
        match self.positions.len() {
            2 => self
                .dial_step(id, t)
                .unwrap_or_else(|| self.scroll_by_centroid()),
            n if n >= 3 => {
                let mut acts = Vec::new();
                self.many_fingers();
                self.close_scroll(TouchScrollPhase::End, &mut acts);
                acts
            }
            // One finger and never a scroll: dropping 2→1 must not jerk the cursor.
            _ if !self.scrolling => self.single_finger(id, wx, wy, abs, t),
            _ => Vec::new(),
        }
    }

    /// The gesture concludes only on the last lift (click / drag-end / stats). `t` is ms.
    pub fn up(&mut self, id: u64, t: f64) -> Vec<Act> {
        let mut acts = Vec::new();
        self.positions.remove(&id);
        self.times.remove(&id);
        if self.track_id == Some(id) {
            self.track_id = None;
        }
        // Any lift ends the twist. Committed: the ring stays open. Short of commit: wind
        // back, and keep the remaining finger inert (`scrolling`) so it cannot move the
        // cursor.
        acts.extend(self.end_dial(true));
        // A committed scroll ends when the pair breaks up. Provisional scroll is
        // still possibly a tap — `roll_back_provisional` cancels it at the last lift.
        if self.positions.len() < 2 && (self.moved || !self.active) {
            self.close_scroll(TouchScrollPhase::End, &mut acts);
        }
        if !self.positions.is_empty() || !self.active {
            return acts;
        }
        self.active = false;
        if self.drag_held {
            self.drag_held = false;
            acts.push(Act::Button {
                gs: BTN_LEFT,
                down: false,
            });
        } else if !self.moved {
            acts.extend(self.roll_back_provisional()); // a tap's jitter leaves no scroll
            match self.max_fingers {
                n if n >= 3 => acts.push(Act::CycleStats),
                2 => {
                    acts.push(Act::Button {
                        gs: BTN_RIGHT,
                        down: true,
                    });
                    acts.push(Act::Button {
                        gs: BTN_RIGHT,
                        down: false,
                    });
                }
                _ => {
                    acts.push(Act::Button {
                        gs: BTN_LEFT,
                        down: true,
                    });
                    acts.push(Act::Button {
                        gs: BTN_LEFT,
                        down: false,
                    });
                    self.last_tap_up = t;
                    self.last_tap_pt = self.start;
                }
            }
        }
        acts
    }

    /// A still finger produces no event, so long-press needs the clock. Call once per
    /// run-loop iteration. `t` is ms.
    pub fn tick(&mut self, t: f64) -> Vec<Act> {
        let mut acts = Vec::new();
        if self.active
            && self.positions.len() == 1
            && self.max_fingers == 1
            && !self.moved
            && !self.drag_held
            && t - self.down_t >= LONG_PRESS_MS
        {
            self.drag_held = true;
            acts.push(Act::Button {
                gs: BTN_LEFT,
                down: true,
            });
        }
        acts
    }

    /// Drop in-flight state (capture release / model change). Open scroll axes come
    /// back as `Cancel` acts — the owner sends them before dropping the gesture; the
    /// held-button flush releases any left button the engine was holding.
    pub fn reset(&mut self) -> Vec<Act> {
        let mut acts = Vec::new();
        self.close_scroll(TouchScrollPhase::Cancel, &mut acts);
        self.positions.clear();
        self.times.clear();
        self.track_id = None;
        self.active = false;
        self.scrolling = false;
        self.scroll_locked = false;
        self.provisional = (0, 0);
        self.dial = None;
        self.moved = false;
        self.drag_held = false;
        self.last_tap_up = 0.0;
        acts
    }

    /// Two-finger move. `Some` when the twist owns the gesture (scroll never runs);
    /// `None` when the hand is scrolling, or might still be.
    ///
    /// Order: centroid past `DIAL_SLOP` before arming locks a scroll; rotation ≥
    /// `DIAL_ARM_DEG` arms the twist, which takes back the provisional scroll. Progress is
    /// `(|Δφ| − arm) / (commit − arm)`. At 1 the ring commits; winding back to 0 after
    /// a commit closes it. A pinch with no rotation never arms and moves no centroid.
    fn dial_step(&mut self, id: u64, t: f64) -> Option<Vec<Act>> {
        let dial = self.dial?;
        if !dial.armed && self.scroll_locked {
            return None;
        }
        // Judge rotation only against a current other-finger position: this same input
        // frame, or older than a pivot's stillness. In between, that finger's event for
        // this frame has not arrived, and the pair vector is stale.
        let other = if dial.ids.0 == id {
            dial.ids.1
        } else {
            dial.ids.0
        };
        let gap = t - *self.times.get(&other)?;
        if gap > DIAL_FRAME_MS && gap < DIAL_PIVOT_MS {
            return dial.armed.then(Vec::new);
        }
        let (a, b) = (
            *self.positions.get(&dial.ids.0)?,
            *self.positions.get(&dial.ids.1)?,
        );
        let v = (b.0 - a.0, b.1 - a.1);
        let cross = dial.vec.0 * v.1 - dial.vec.1 * v.0;
        let dot = dial.vec.0 * v.0 + dial.vec.1 * v.1;
        // Signed rotation of the pair vector; positive is clockwise on a y-down screen.
        let phi = cross.atan2(dot).to_degrees();
        let (cx, cy) = self.centroid();
        let mut acts = Vec::new();
        if !dial.armed {
            let travel = (cx - dial.anchor.0).hypot(cy - dial.anchor.1);
            if travel >= TAP_SLOP {
                self.moved = true;
            }
            if travel >= DIAL_SLOP {
                self.scroll_locked = true;
                self.provisional = (0, 0);
                return None;
            }
            // Undecided: the scroll goes out now, provisionally.
            if phi.abs() < DIAL_ARM_DEG {
                return None;
            }
            acts = self.roll_back_provisional();
            self.moved = true; // a twist is never a tap
            self.scrolling = true; // and dropping to one finger must not jerk the cursor
        }
        let progress =
            ((phi.abs() - DIAL_ARM_DEG) / (DIAL_COMMIT_DEG - DIAL_ARM_DEG)).clamp(0.0, 1.0);
        acts.push(Act::Dial {
            progress,
            clockwise: phi > 0.0,
            x: cx,
            y: cy,
        });
        let d = self.dial.as_mut()?;
        d.armed = true;
        if progress >= 1.0 && !d.committed {
            d.committed = true;
            acts.push(Act::DialCommit);
        } else if progress <= 0.0 && d.committed {
            d.committed = false;
            acts.push(Act::DialCancel);
        }
        Some(acts)
    }

    /// A local gesture owns the fingers down: the twist has armed. The host must not have them.
    pub fn keeps(&self) -> bool {
        self.dial.is_some_and(|d| d.armed)
    }

    /// The wire saw none of this gesture's scroll (frame mode ended mid-gesture): the next
    /// delta opens its axis with `Begin`, and a take-back returns only what was sent.
    fn scroll_unsent(&mut self) {
        self.scroll_axes = [false; 2];
        self.provisional = (0, 0);
    }

    /// The twist is over (a finger lifted or a third landed). An armed-but-uncommitted
    /// ring winds back in; a committed ring stays open for the UI.
    fn end_dial(&mut self, _lift: bool) -> Vec<Act> {
        match self.dial.take() {
            Some(d) if d.armed && !d.committed => vec![Act::DialCancel],
            _ => Vec::new(),
        }
    }

    fn centroid(&self) -> (f32, f32) {
        let n = self.positions.len() as f32;
        let (mut sx, mut sy) = (0.0f32, 0.0f32);
        for &(px, py) in self.positions.values() {
            sx += px;
            sy += py;
        }
        (sx / n, sy / n)
    }

    /// Two fingers: scroll by centroid delta as a DIP distance, never move the cursor.
    /// While the pair is undecided the units are provisional (`roll_back_provisional`).
    /// Finger-up / finger-right are positive.
    fn scroll_by_centroid(&mut self) -> Vec<Act> {
        let (cx, cy) = self.centroid();
        if !self.scrolling {
            // From the pair's landing, so the first move's travel scrolls too.
            self.scrolling = true;
            self.scroll_anchor = self.dial.map_or((cx, cy), |d| d.anchor);
        }
        let gain = SCROLL_UNITS_PER_DIP / self.density;
        self.scroll_carry.1 += (self.scroll_anchor.1 - cy) * gain;
        self.scroll_carry.0 += (cx - self.scroll_anchor.0) * gain;
        self.scroll_anchor = (cx, cy);
        let dy = self.scroll_carry.1 as i32; // toward zero; remainder keeps the sign
        let dx = self.scroll_carry.0 as i32;
        self.scroll_carry.1 -= dy as f32;
        self.scroll_carry.0 -= dx as f32;
        if dx == 0 && dy == 0 {
            return Vec::new();
        }
        if !self.scroll_locked && self.dial.is_some_and(|d| !d.armed) {
            self.provisional.0 += dx;
            self.provisional.1 += dy;
        } else {
            self.scroll_locked = true;
            self.moved = true;
        }
        self.scroll_acts(dx, dy)
    }

    /// The undecided pair became a twist or a tap: send back what it scrolled, then
    /// cancel the axes — a rolled-back gesture never runs a kinetic tail.
    fn roll_back_provisional(&mut self) -> Vec<Act> {
        let (x, y) = std::mem::take(&mut self.provisional);
        self.scroll_carry = (0.0, 0.0);
        let mut acts = self.scroll_acts(-x, -y);
        self.close_scroll(TouchScrollPhase::Cancel, &mut acts);
        acts
    }

    /// Vertical then horizontal scroll acts for a Q24.8 delta, skipping a zero axis.
    /// The first nonzero delta opens the axis with `Begin`; later ones are `Update`.
    fn scroll_acts(&mut self, dx: i32, dy: i32) -> Vec<Act> {
        let mut acts = Vec::new();
        for (axis, delta) in [(0u32, dy), (1, dx)] {
            if delta != 0 {
                let phase = if self.scroll_axes[axis as usize] {
                    TouchScrollPhase::Update
                } else {
                    self.scroll_axes[axis as usize] = true;
                    TouchScrollPhase::Begin
                };
                acts.push(Act::Scroll { axis, delta, phase });
            }
        }
        acts
    }

    /// A zero-delta `End`/`Cancel` on every open scroll axis, and its sub-unit
    /// remainder drops with it.
    fn close_scroll(&mut self, phase: TouchScrollPhase, acts: &mut Vec<Act>) {
        for (axis, open) in self.scroll_axes.iter_mut().enumerate() {
            if *open {
                *open = false;
                if axis == 0 {
                    self.scroll_carry.1 = 0.0;
                } else {
                    self.scroll_carry.0 = 0.0;
                }
                acts.push(Act::Scroll {
                    axis: axis as u32,
                    delta: 0,
                    phase,
                });
            }
        }
    }

    /// Three or more fingers: no scroll, no cursor. Travel past `TAP_SLOP` disqualifies
    /// the tap. Clear scroll and the tracked finger so a 3→2 drop cannot fire a notch
    /// from the centroid jump or a stale track position.
    fn many_fingers(&mut self) {
        let (cx, cy) = self.centroid();
        if self.positions.len() != self.many_count {
            self.many_count = self.positions.len();
            self.many_anchor = (cx, cy);
        } else if (cx - self.many_anchor.0).abs() > TAP_SLOP
            || (cy - self.many_anchor.1).abs() > TAP_SLOP
        {
            self.moved = true;
        }
        self.scrolling = false;
        self.track_id = None;
    }

    fn single_finger(&mut self, id: u64, wx: f32, wy: f32, abs: Abs, t: f64) -> Vec<Act> {
        let mut acts = Vec::new();
        if (wx - self.start.0).abs() > TAP_SLOP || (wy - self.start.1).abs() > TAP_SLOP {
            self.moved = true;
        }
        if !self.trackpad {
            acts.push(Act::MoveAbs(abs));
            return acts;
        }
        // Zero delta this frame if the tracked finger changed, so lifting one of several
        // fingers never jumps the cursor.
        if self.track_id != Some(id) {
            self.track_id = Some(id);
            self.prev = (wx, wy);
            self.prev_t = t;
            return acts;
        }
        let dx = wx - self.prev.0;
        let dy = wy - self.prev.1;
        let dt_ms = (t - self.prev_t).max(1.0) as f32;
        self.prev = (wx, wy);
        self.prev_t = t;
        let speed = dx.hypot(dy) / dt_ms;
        let accel = (1.0 + ACCEL_GAIN * (speed - ACCEL_SPEED_FLOOR).max(0.0)).min(ACCEL_MAX);
        let gain = POINTER_SENS * accel;
        self.carry.0 += dx * gain;
        self.carry.1 += dy * gain;
        let out_x = self.carry.0 as i32; // toward zero; remainder keeps the sign
        let out_y = self.carry.1 as i32;
        if out_x != 0 || out_y != 0 {
            acts.push(Act::MoveRel {
                dx: out_x,
                dy: out_y,
            });
            self.carry.0 -= out_x as f32;
            self.carry.1 -= out_y as f32;
        }
        acts
    }
}

/// One SDL finger event. SDL never batches fingers in a single event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FingerPhase {
    Down,
    Move,
    Up,
    /// The OS took the touch away; no `Up` follows.
    Cancel,
}

/// Contacts a touchscreen forwards to the host's touchpad. A third finger takes the gesture
/// back, so three-finger taps stay the client's.
pub const FRAME_CONTACTS: u8 = 2;

/// Logical pixels per inch a touchscreen is assumed to have: SDL 3.4 reports no physical size.
const LOGICAL_PPI: f32 = 160.0;

/// The view in tenths of a millimetre, from its window pixels and display scale at
/// [`LOGICAL_PPI`]. A Steam Deck (1280×800 at scale 1) reads 203×127 mm; its panel is
/// 151×94 mm (LCD) or 159×100 mm (OLED), so the host sees strokes 1.27–1.35× as long.
pub fn view_dmm(px: (u32, u32), scale: f32) -> (u16, u16) {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let dmm = |v: u32| (v as f32 / scale / LOGICAL_PPI * 254.0).round() as u16;
    (dmm(px.0), dmm(px.1))
}

/// One update of core's touch surface: every finger down, `x`/`y` as fractions of the view,
/// and whether a local gesture keeps them from the host.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SurfaceUpdate {
    pub touches: Vec<(u64, f32, f32)>,
    pub keep: bool,
}

/// Frame mode: `trackpad` toward a host that presents a touchpad. The fingers go to the
/// host's touchpad; the gesture machine keeps running for the local gestures and
/// [`FrameMode::sends`] filters what it puts on the wire. Off, every act passes and no
/// surface update is made.
#[derive(Debug, Default)]
pub struct FrameMode {
    on: bool,
    /// The host OS clicks on a tap (`FEATURE_TOUCHPAD_TAPS`), so the client sends no buttons.
    host_taps: bool,
    /// Fingers down, in landing order.
    fingers: Vec<(u64, f32, f32)>,
}

impl FrameMode {
    /// Whether frame mode runs. Inverted scroll is the player's pick, and the host's touchpad
    /// scrolls by the host's own setting, so inverting keeps the gesture machine's scroll.
    pub fn wanted(trackpad: bool, host_presents: bool, invert_scroll: bool) -> bool {
        trackpad && host_presents && !invert_scroll
    }

    /// Turning on returns the announce: an update with no touches. Turning off forgets the
    /// fingers and lets every act of `g` through again, mid-gesture too.
    pub fn set(&mut self, on: bool, host_taps: bool, g: &mut Gestures) -> Option<SurfaceUpdate> {
        let announce = (on && !self.on).then(SurfaceUpdate::default);
        if self.on && !on {
            self.fingers.clear();
            g.scroll_unsent();
        }
        self.on = on;
        self.host_taps = host_taps;
        announce
    }

    pub fn on(&self) -> bool {
        self.on
    }

    /// Core retracts tap-like lifts: the client makes the clicks.
    pub fn retracts_taps(&self) -> bool {
        !self.host_taps
    }

    /// Whether a gesture act reaches the wire. The host makes motion and scroll from the
    /// contacts, and clicks itself when it taps.
    pub fn sends(&self, act: &Act) -> bool {
        match act {
            _ if !self.on => true,
            Act::MoveRel { .. } | Act::MoveAbs(_) | Act::Scroll { .. } => false,
            Act::Button { .. } => !self.host_taps,
            Act::CycleStats | Act::Dial { .. } | Act::DialCommit | Act::DialCancel => true,
        }
    }

    /// A finger event the gesture machine has taken; `keep` is [`Gestures::keeps`]. `None`
    /// when off or for a finger that never landed here.
    pub fn finger(
        &mut self,
        phase: FingerPhase,
        id: u64,
        x: f32,
        y: f32,
        keep: bool,
    ) -> Option<SurfaceUpdate> {
        if !self.on {
            return None;
        }
        let at = self.fingers.iter().position(|f| f.0 == id);
        match (phase, at) {
            (FingerPhase::Down, None) => self.fingers.push((id, x, y)),
            (FingerPhase::Down | FingerPhase::Move, Some(i)) => self.fingers[i] = (id, x, y),
            (FingerPhase::Up | FingerPhase::Cancel, Some(i)) => {
                self.fingers.remove(i);
            }
            _ => return None,
        }
        Some(self.update(keep))
    }

    /// Take every finger back from the host as a kept gesture, then lift them all. Nothing
    /// while off.
    pub fn release(&mut self) -> Vec<SurfaceUpdate> {
        if !self.on {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !self.fingers.is_empty() {
            out.push(self.update(true));
            self.fingers.clear();
        }
        out.push(self.update(false));
        out
    }

    fn update(&self, keep: bool) -> SurfaceUpdate {
        SurfaceUpdate {
            touches: self.fingers.clone(),
            keep,
        }
    }
}

/// px. No finger drag moves this far in one SDL event; a leaked absolute position does.
const LEAK_PX: f32 = 150.0;

/// Gaming Mode: Steam Input owns the touchscreen and replays it as a mouse whose
/// "relative" deltas are absolute positions. Under the stream's relative-mouse lock
/// those walk the host cursor into a corner. SDL sees no fingers, so every touch
/// model is bypassed; `SDL_TOUCH_MOUSE_EVENTS` cannot help (the events are Steam's).
/// Drop a gamescope session with no finger yet and a delta no finger drag produces.
/// A real mouse in Gaming Mode keeps working: its deltas are small.
pub struct SteamTouchMouse {
    game_mode: bool,
    /// A direct-touch finger reached SDL this session: the touchscreen is ours.
    pub fingers_seen: bool,
    /// A non-direct finger was ignored this session — said once in the log.
    pub indirect_seen: bool,
    leaked: bool,
    noticed: bool,
}

impl SteamTouchMouse {
    pub fn new(game_mode: bool) -> Self {
        Self {
            game_mode,
            fingers_seen: false,
            indirect_seen: false,
            leaked: false,
            noticed: false,
        }
    }

    pub fn leaks(&mut self, xrel: f32, yrel: f32) -> bool {
        if !self.game_mode || self.fingers_seen {
            return false;
        }
        let leak = xrel.abs() >= LEAK_PX || yrel.abs() >= LEAK_PX;
        if leak {
            self.leaked = true;
        }
        leak
    }

    /// `true` once, after the first leak: the caller shows the notice.
    pub fn take_notice(&mut self) -> bool {
        let first = self.leaked && !self.noticed;
        if first {
            self.noticed = true;
        }
        first
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn gaming_mode_drops_leaked_touch_positions_until_a_finger_is_seen() {
        let mut m = super::SteamTouchMouse::new(true);
        assert!(!m.leaks(4.0, -3.0));
        assert!(!m.take_notice());
        assert!(m.leaks(340.0, 12.0));
        assert!(m.take_notice());
        assert!(!m.take_notice());
        assert!(m.leaks(0.0, -420.0));
        // A real finger reached SDL: stop treating large deltas as leaks.
        m.fingers_seen = true;
        assert!(!m.leaks(340.0, 12.0));
        // Outside Gaming Mode a mouse may move that far in one event.
        let mut d = super::SteamTouchMouse::new(false);
        assert!(!d.leaks(340.0, 12.0));
        assert!(!d.take_notice());
    }

    use super::*;

    const ABS: Abs = Abs {
        x: 100,
        y: 200,
        w: 1280,
        h: 720,
    };

    fn abs_at(x: i32, y: i32) -> Abs {
        Abs {
            x,
            y,
            w: 1280,
            h: 720,
        }
    }

    #[test]
    fn trackpad_tap_is_a_left_click_with_no_motion() {
        let mut g = Gestures::new(true);
        let mut acts = g.down(1, 50.0, 50.0, ABS, 0.0);
        acts.extend(g.up(1, 40.0));
        assert_eq!(
            acts,
            vec![
                Act::Button {
                    gs: BTN_LEFT,
                    down: true
                },
                Act::Button {
                    gs: BTN_LEFT,
                    down: false
                },
            ]
        );
    }

    #[test]
    fn pointer_tap_places_the_cursor_then_clicks() {
        let mut g = Gestures::new(false);
        let mut acts = g.down(1, 50.0, 50.0, abs_at(640, 360), 0.0);
        acts.extend(g.up(1, 40.0));
        assert_eq!(
            acts,
            vec![
                Act::MoveAbs(abs_at(640, 360)),
                Act::Button {
                    gs: BTN_LEFT,
                    down: true
                },
                Act::Button {
                    gs: BTN_LEFT,
                    down: false
                },
            ]
        );
    }

    #[test]
    fn two_finger_tap_is_a_right_click() {
        let mut g = Gestures::new(true);
        let mut acts = g.down(1, 50.0, 50.0, ABS, 0.0);
        acts.extend(g.down(2, 80.0, 52.0, ABS, 5.0));
        acts.extend(g.up(1, 40.0));
        acts.extend(g.up(2, 42.0));
        assert_eq!(
            acts,
            vec![
                Act::Button {
                    gs: BTN_RIGHT,
                    down: true
                },
                Act::Button {
                    gs: BTN_RIGHT,
                    down: false
                },
            ]
        );
    }

    #[test]
    fn three_finger_tap_cycles_stats() {
        let mut g = Gestures::new(true);
        let mut acts = g.down(1, 50.0, 50.0, ABS, 0.0);
        acts.extend(g.down(2, 80.0, 50.0, ABS, 2.0));
        acts.extend(g.down(3, 110.0, 50.0, ABS, 4.0));
        acts.extend(g.up(1, 40.0));
        acts.extend(g.up(2, 41.0));
        acts.extend(g.up(3, 42.0));
        assert_eq!(acts, vec![Act::CycleStats]);
    }

    #[test]
    fn trackpad_drag_emits_relative_motion() {
        let mut g = Gestures::new(true);
        assert!(g.down(1, 100.0, 100.0, ABS, 0.0).is_empty());
        // 40 px in 16 ms: acceleration should exceed 1:1.
        let acts = g.motion(1, 140.0, 100.0, ABS, 16.0);
        match acts.as_slice() {
            [Act::MoveRel { dx, dy }] => {
                assert!(*dx >= 40, "expected accelerated dx ≥ raw 40, got {dx}");
                assert_eq!(*dy, 0);
            }
            other => panic!("expected one MoveRel, got {other:?}"),
        }
        // Moved: the lift is not a tap.
        assert!(g.up(1, 32.0).is_empty());
    }

    #[test]
    fn pointer_motion_follows_the_finger_absolutely() {
        let mut g = Gestures::new(false);
        let _ = g.down(1, 100.0, 100.0, abs_at(300, 300), 0.0);
        let acts = g.motion(1, 140.0, 120.0, abs_at(360, 340), 16.0);
        assert_eq!(acts, vec![Act::MoveAbs(abs_at(360, 340))]);
    }

    #[test]
    fn two_finger_pan_scrolls_by_the_centroid() {
        let mut g = Gestures::new(true);
        let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
        let _ = g.down(2, 120.0, 200.0, ABS, 2.0);
        // Both up 40 px: centroid up → positive (finger-up) notches.
        let a1 = g.motion(1, 100.0, 160.0, ABS, 10.0);
        let a2 = g.motion(2, 120.0, 160.0, ABS, 12.0);
        let scrolls: Vec<_> = a1.into_iter().chain(a2).collect();
        assert!(
            scrolls
                .iter()
                .any(|a| matches!(a, Act::Scroll { axis: 0, delta, .. } if *delta > 0)),
            "expected an upward vertical scroll, got {scrolls:?}"
        );
    }

    /// A pair that re-forms mid-scroll (2→1→2) scrolls from where it lands: the old
    /// pair's centroid is no anchor for the new one.
    #[test]
    fn a_reformed_pair_scrolls_from_where_it_lands() {
        let mut g = Gestures::new(true);
        let _ = g.down(1, 100.0, 100.0, ABS, 0.0);
        let _ = g.down(2, 140.0, 100.0, ABS, 1.0);
        // 80 px of pan: a committed scroll.
        for i in 1..=8 {
            let y = 100.0 + 10.0 * i as f32;
            let _ = g.motion(1, 100.0, y, ABS, 10.0 * f64::from(i));
            let _ = g.motion(2, 140.0, y, ABS, 10.0 * f64::from(i));
        }
        let _ = g.up(1, 100.0);
        // A new second finger lands 200 px away; its first move is 2 px.
        let _ = g.down(3, 340.0, 180.0, ABS, 110.0);
        let acts = g.motion(3, 340.0, 182.0, ABS, 120.0);
        let jump = acts
            .iter()
            .any(|a| matches!(a, Act::Scroll { delta, .. } if delta.abs() > 4 * 256));
        assert!(
            !jump,
            "the first move scrolls by its own travel, got {acts:?}"
        );
    }

    #[test]
    fn scroll_delta_is_dips_not_physical_px() {
        // 2 DIP of centroid travel: at 2x that is twice the physical px — the wire
        // delta must be identical either way.
        for (density, finger_travel) in [(1.0f32, 4.0f32), (2.0, 8.0)] {
            let mut g = Gestures::new(true);
            g.set_density(density);
            let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
            let _ = g.down(2, 140.0, 200.0, ABS, 2.0);
            // One finger carries the whole pair move: finger travel is 2x centroid.
            let acts = g.motion(1, 100.0, 200.0 - finger_travel, ABS, 10.0);
            assert_eq!(net_scroll(&acts), (512, 0), "density {density}: {acts:?}");
        }
        // A bogus density falls back to unscaled rather than blowing up.
        let mut g = Gestures::new(true);
        g.set_density(f32::NAN);
        let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
        let _ = g.down(2, 140.0, 200.0, ABS, 2.0);
        let acts = g.motion(1, 100.0, 196.0, ABS, 10.0);
        assert_eq!(net_scroll(&acts), (512, 0), "{acts:?}");
    }

    #[test]
    fn a_pan_runs_begin_update_end_and_the_next_gesture_begins_fresh() {
        let mut g = Gestures::new(true);
        let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
        let _ = g.down(2, 140.0, 200.0, ABS, 2.0);
        assert_eq!(
            g.motion(1, 100.0, 198.0, ABS, 10.0),
            vec![Act::Scroll {
                axis: 0,
                delta: 256,
                phase: TouchScrollPhase::Begin,
            }]
        );
        assert_eq!(
            g.motion(2, 140.0, 198.0, ABS, 11.0),
            vec![Act::Scroll {
                axis: 0,
                delta: 256,
                phase: TouchScrollPhase::Update,
            }]
        );
        // Past the tap slop the scroll is committed; small steps keep the pair
        // vector straight — a big lone-finger move reads as a twist.
        for step in 2..=10 {
            let y = 200.0 - 2.0 * step as f32;
            let _ = g.motion(1, 100.0, y, ABS, 10.0 + step as f64);
            let _ = g.motion(2, 140.0, y, ABS, 10.0 + step as f64 + 1.0);
        }
        // The pair is broken up: the committed scroll ends even with no distance left.
        assert_eq!(
            g.up(1, 40.0),
            vec![Act::Scroll {
                axis: 0,
                delta: 0,
                phase: TouchScrollPhase::End,
            }]
        );
        assert!(g.up(2, 41.0).is_empty());
        // A fresh pair opens a fresh Begin.
        let _ = g.down(3, 100.0, 200.0, ABS, 100.0);
        let _ = g.down(4, 140.0, 200.0, ABS, 102.0);
        assert_eq!(
            g.motion(3, 100.0, 198.0, ABS, 110.0),
            vec![Act::Scroll {
                axis: 0,
                delta: 256,
                phase: TouchScrollPhase::Begin,
            }]
        );
    }

    #[test]
    fn reset_cancels_open_scroll_axes() {
        let mut g = Gestures::new(true);
        let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
        let _ = g.down(2, 140.0, 200.0, ABS, 2.0);
        let _ = g.motion(1, 100.0, 198.0, ABS, 10.0);
        assert_eq!(
            g.reset(),
            vec![Act::Scroll {
                axis: 0,
                delta: 0,
                phase: TouchScrollPhase::Cancel,
            }]
        );
    }

    #[test]
    fn three_finger_drag_scrolls_nothing() {
        let mut g = Gestures::new(true);
        let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
        let _ = g.down(2, 130.0, 200.0, ABS, 2.0);
        let _ = g.down(3, 160.0, 200.0, ABS, 4.0);
        // Twins use a three-finger swipe for a keyboard; emit nothing, and travel kills the tap.
        let mut acts = g.motion(1, 100.0, 160.0, ABS, 10.0);
        acts.extend(g.motion(2, 130.0, 160.0, ABS, 12.0));
        acts.extend(g.motion(3, 160.0, 160.0, ABS, 14.0));
        assert_eq!(acts, vec![], "a three-finger drag must emit nothing");
        acts.extend(g.up(1, 40.0));
        acts.extend(g.up(2, 41.0));
        acts.extend(g.up(3, 42.0));
        assert_eq!(acts, vec![]);
    }

    #[test]
    fn tap_then_press_drag_holds_the_left_button() {
        let mut g = Gestures::new(true);
        let _ = g.down(1, 50.0, 50.0, ABS, 0.0);
        let click = g.up(1, 10.0);
        assert_eq!(
            click,
            vec![
                Act::Button {
                    gs: BTN_LEFT,
                    down: true
                },
                Act::Button {
                    gs: BTN_LEFT,
                    down: false
                },
            ]
        );
        let down2 = g.down(2, 52.0, 51.0, ABS, 120.0);
        assert_eq!(
            down2,
            vec![Act::Button {
                gs: BTN_LEFT,
                down: true
            }]
        );
        let _ = g.motion(2, 90.0, 51.0, ABS, 140.0);
        let end = g.up(2, 160.0);
        assert_eq!(
            end,
            vec![Act::Button {
                gs: BTN_LEFT,
                down: false
            }]
        );
    }

    /// Two fingers 40 px apart, rotated `deg` clockwise about `c`.
    fn twisted(c: (f32, f32), deg: f32) -> ((f32, f32), (f32, f32)) {
        let (s, k) = deg.to_radians().sin_cos();
        let (rx, ry) = (20.0 * k, 20.0 * s); // half the finger-to-finger vector
        ((c.0 - rx, c.1 - ry), (c.0 + rx, c.1 + ry))
    }

    fn dial_acts(acts: &[Act]) -> Vec<Act> {
        acts.iter()
            .copied()
            .filter(|a| matches!(a, Act::Dial { .. } | Act::DialCommit | Act::DialCancel))
            .collect()
    }

    #[test]
    fn a_pure_scroll_never_arms_the_dial() {
        let mut g = Gestures::new(true);
        let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
        let _ = g.down(2, 140.0, 200.0, ABS, 2.0);
        let mut acts = Vec::new();
        for step in 1..=10 {
            let y = 200.0 - 4.0 * step as f32;
            acts.extend(g.motion(1, 100.0, y, ABS, 10.0 * step as f64));
            acts.extend(g.motion(2, 140.0, y, ABS, 10.0 * step as f64 + 1.0));
        }
        assert!(acts.iter().any(|a| matches!(a, Act::Scroll { .. })));
        assert!(dial_acts(&acts).is_empty(), "{acts:?}");
        acts.clear();
        acts.extend(g.up(1, 200.0));
        acts.extend(g.up(2, 201.0));
        assert!(dial_acts(&acts).is_empty());
    }

    #[test]
    fn a_thirty_five_degree_twist_commits_at_the_first_sample_past_thirty() {
        let mut g = Gestures::new(true);
        let c = (120.0, 200.0);
        let (p1, p2) = twisted(c, 0.0);
        let _ = g.down(1, p1.0, p1.1, ABS, 0.0);
        let _ = g.down(2, p2.0, p2.1, ABS, 2.0);
        let mut commit_at = None;
        let mut all = Vec::new();
        // 7° steps so no sample sits on a threshold where float rounding picks the side.
        for step in 1..=5 {
            let deg = 7.0 * step as f32;
            let (p1, p2) = twisted(c, deg);
            let mut acts = g.motion(1, p1.0, p1.1, ABS, 10.0 * step as f64);
            acts.extend(g.motion(2, p2.0, p2.1, ABS, 10.0 * step as f64 + 1.0));
            all.extend(acts.iter().copied());
            let dial = dial_acts(&acts);
            if deg < DIAL_ARM_DEG {
                assert!(
                    dial.is_empty(),
                    "under the arm angle nothing fires: {dial:?}"
                );
            } else {
                let Some(Act::Dial {
                    progress,
                    clockwise,
                    ..
                }) = dial.first()
                else {
                    panic!("expected a Dial act at {deg}°, got {dial:?}");
                };
                assert!(*clockwise, "the ring turns the way the hand turns");
                let expected =
                    ((deg - DIAL_ARM_DEG) / (DIAL_COMMIT_DEG - DIAL_ARM_DEG)).clamp(0.0, 1.0);
                assert!(
                    (progress - expected).abs() < 0.05,
                    "{deg}°: {progress} vs {expected}"
                );
            }
            if dial.contains(&Act::DialCommit) && commit_at.is_none() {
                commit_at = Some(deg);
            }
        }
        assert_eq!(
            commit_at,
            Some(35.0),
            "commits exactly once, at the first sample past the commit angle"
        );
        // One finger per SDL event shifts the centroid mid-frame; arming takes it back.
        assert_eq!(
            net_scroll(&all),
            (0, 0),
            "a twist leaves no scroll: {all:?}"
        );
        // Committed: lift leaves the ring open.
        let mut acts = g.up(1, 100.0);
        acts.extend(g.up(2, 101.0));
        assert_eq!(acts, vec![]);
    }

    #[test]
    fn a_twenty_degree_twist_then_a_lift_cancels_and_sends_nothing() {
        let mut g = Gestures::new(true);
        let c = (120.0, 200.0);
        let (p1, p2) = twisted(c, 0.0);
        let _ = g.down(1, p1.0, p1.1, ABS, 0.0);
        let _ = g.down(2, p2.0, p2.1, ABS, 2.0);
        let (p1, p2) = twisted(c, 20.0);
        let mut acts = g.motion(1, p1.0, p1.1, ABS, 10.0);
        acts.extend(g.motion(2, p2.0, p2.1, ABS, 11.0));
        assert!(matches!(dial_acts(&acts).first(), Some(Act::Dial { .. })));
        assert_eq!(
            net_scroll(&acts),
            (0, 0),
            "arming takes the scroll back: {acts:?}"
        );
        assert!(!acts.contains(&Act::DialCommit));
        let mut lift = g.up(1, 50.0);
        assert_eq!(lift, vec![Act::DialCancel], "the ring winds back in");
        // The finger still down must not move the cursor, and the last lift is not a tap.
        lift.extend(g.motion(2, p2.0 + 40.0, p2.1, ABS, 60.0));
        lift.extend(g.up(2, 70.0));
        assert_eq!(lift, vec![Act::DialCancel]);
    }

    /// Net scroll units per axis (vertical, horizontal).
    fn net_scroll(acts: &[Act]) -> (i32, i32) {
        acts.iter().fold((0, 0), |(v, h), a| match a {
            Act::Scroll { axis: 0, delta, .. } => (v + delta, h),
            Act::Scroll { delta, .. } => (v, h + delta),
            _ => (v, h),
        })
    }

    #[test]
    fn a_pan_scrolls_from_the_first_pixel_as_a_precise_distance() {
        let mut g = Gestures::new(true);
        let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
        let _ = g.down(2, 140.0, 200.0, ABS, 2.0);
        // 2 px up, both fingers: far under the tap slop, and it already scrolls.
        let mut acts = g.motion(1, 100.0, 198.0, ABS, 10.0);
        acts.extend(g.motion(2, 140.0, 198.0, ABS, 11.0));
        assert_eq!(net_scroll(&acts), (512, 0), "{acts:?}");
        for step in 2..=10 {
            let y = 200.0 - 2.0 * step as f32;
            acts.extend(g.motion(1, 100.0, y, ABS, 10.0 * step as f64));
            acts.extend(g.motion(2, 140.0, y, ABS, 10.0 * step as f64 + 1.0));
        }
        // 20 px of centroid travel is 20 DIP: 5120 wire units, every one sent.
        assert_eq!(net_scroll(&acts), (5120, 0), "{acts:?}");
        assert!(dial_acts(&acts).is_empty());
        // The committed scroll ends on the first lift; the second emits nothing.
        assert_eq!(
            g.up(1, 200.0),
            vec![Act::Scroll {
                axis: 0,
                delta: 0,
                phase: TouchScrollPhase::End,
            }]
        );
        assert!(g.up(2, 201.0).is_empty(), "a scroll is not a tap");
    }

    #[test]
    fn a_two_finger_tap_takes_back_its_jitter_before_the_click() {
        let mut g = Gestures::new(true);
        let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
        let _ = g.down(2, 140.0, 200.0, ABS, 2.0);
        let mut acts = g.motion(1, 100.0, 196.0, ABS, 10.0);
        acts.extend(g.motion(2, 140.0, 196.0, ABS, 11.0));
        acts.extend(g.up(1, 40.0));
        acts.extend(g.up(2, 41.0));
        assert_eq!(
            acts,
            vec![
                Act::Scroll {
                    axis: 0,
                    delta: 512,
                    phase: TouchScrollPhase::Begin,
                },
                Act::Scroll {
                    axis: 0,
                    delta: 512,
                    phase: TouchScrollPhase::Update,
                },
                // The lift resolves as a tap: send back the provisional scroll on the
                // still-open axis, then cancel it — never a kinetic tail.
                Act::Scroll {
                    axis: 0,
                    delta: -1024,
                    phase: TouchScrollPhase::Update,
                },
                Act::Scroll {
                    axis: 0,
                    delta: 0,
                    phase: TouchScrollPhase::Cancel,
                },
                Act::Button {
                    gs: BTN_RIGHT,
                    down: true
                },
                Act::Button {
                    gs: BTN_RIGHT,
                    down: false
                },
            ]
        );
    }

    #[test]
    fn a_drifting_twist_arms_and_takes_back_its_scroll() {
        // Real fingers drift a few px per sample while turning. The drift scrolls
        // provisionally; arming the twist sends it back.
        let mut g = Gestures::new(true);
        let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
        let _ = g.down(2, 140.0, 200.0, ABS, 2.0);
        let mut acts = Vec::new();
        for step in 1..=5 {
            let c = (120.0 + 2.0 * step as f32, 200.0 - 2.0 * step as f32); // ~14 px in all
            let (p1, p2) = twisted(c, 7.0 * step as f32);
            acts.extend(g.motion(1, p1.0, p1.1, ABS, 10.0 * step as f64));
            acts.extend(g.motion(2, p2.0, p2.1, ABS, 10.0 * step as f64 + 1.0));
        }
        assert_eq!(
            net_scroll(&acts),
            (0, 0),
            "the twist leaves no scroll: {acts:?}"
        );
        assert!(
            acts.iter().any(|a| matches!(a, Act::Dial { .. })),
            "the twist arms despite the drift: {acts:?}"
        );
        assert!(acts.contains(&Act::DialCommit), "35° commits: {acts:?}");
    }

    #[test]
    fn a_scroll_then_a_rotation_stays_a_scroll() {
        let mut g = Gestures::new(true);
        let c = (120.0, 200.0);
        let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
        let _ = g.down(2, 140.0, 200.0, ABS, 2.0);
        let mut acts = g.motion(1, 100.0, 170.0, ABS, 10.0); // 30 px up: past the slop
        acts.extend(g.motion(2, 140.0, 170.0, ABS, 11.0));
        assert!(acts.iter().any(|a| matches!(a, Act::Scroll { .. })));
        let (p1, p2) = twisted((c.0, 170.0), 40.0); // twist after the scroll already owns it
        acts = g.motion(1, p1.0, p1.1, ABS, 20.0);
        acts.extend(g.motion(2, p2.0, p2.1, ABS, 21.0));
        assert!(
            dial_acts(&acts).is_empty(),
            "a scroll is a scroll for its lifetime: {acts:?}"
        );
    }

    #[test]
    fn long_press_arms_a_drag() {
        let mut g = Gestures::new(true);
        assert!(g.down(1, 50.0, 50.0, ABS, 0.0).is_empty());
        assert!(g.tick(400.0).is_empty(), "under the hold time: nothing");
        assert_eq!(
            g.tick(520.0),
            vec![Act::Button {
                gs: BTN_LEFT,
                down: true
            }]
        );
        assert!(g.tick(600.0).is_empty(), "arms once");
        let _ = g.motion(1, 90.0, 50.0, ABS, 620.0);
        assert_eq!(
            g.up(1, 700.0),
            vec![Act::Button {
                gs: BTN_LEFT,
                down: false
            }]
        );
    }

    #[test]
    fn long_press_after_motion_or_a_second_finger_does_not_arm() {
        let mut g = Gestures::new(true);
        let _ = g.down(1, 50.0, 50.0, ABS, 0.0);
        let _ = g.motion(1, 90.0, 50.0, ABS, 100.0); // past the slop: a swipe, not a press
        assert!(g.tick(600.0).is_empty());
        assert!(g.up(1, 700.0).is_empty());

        let _ = g.down(2, 50.0, 50.0, ABS, 1000.0);
        let _ = g.down(3, 80.0, 50.0, ABS, 1010.0);
        let _ = g.up(3, 1020.0); // second finger came and went: no press
        assert!(g.tick(1600.0).is_empty());
    }

    /// The gesture machine and frame mode fed as `Capture` feeds them: the machine first,
    /// then the surface with its claim. Positions are px of a 1280×720 view.
    struct Rig {
        g: Gestures,
        frames: FrameMode,
        all: Vec<Act>,
        wire: Vec<Act>,
        surface: Vec<SurfaceUpdate>,
    }

    impl Rig {
        fn new(frame_mode: Option<bool>) -> Rig {
            let mut r = Rig {
                g: Gestures::new(true),
                frames: FrameMode::default(),
                all: Vec::new(),
                wire: Vec::new(),
                surface: Vec::new(),
            };
            if let Some(host_taps) = frame_mode {
                r.set_frames(true, host_taps);
                assert_eq!(
                    r.surface.pop(),
                    Some(SurfaceUpdate::default()),
                    "entering announces the surface"
                );
            }
            r
        }

        /// `Capture::set_frames`: a new decision, or a refused update turning it off.
        fn set_frames(&mut self, on: bool, host_taps: bool) {
            let announce = self.frames.set(on, host_taps, &mut self.g);
            self.surface.extend(announce);
        }

        fn finger(&mut self, phase: FingerPhase, id: u64, x: f32, y: f32, t: f64) {
            let acts = match phase {
                FingerPhase::Down => self.g.down(id, x, y, ABS, t),
                FingerPhase::Move => self.g.motion(id, x, y, ABS, t),
                FingerPhase::Up => self.g.up(id, t),
                FingerPhase::Cancel => {
                    if self.frames.on() {
                        let acts = self.g.reset();
                        self.surface.extend(self.frames.release());
                        self.take(acts);
                    }
                    return;
                }
            };
            let keep = self.g.keeps();
            self.surface
                .extend(self.frames.finger(phase, id, x / 1280.0, y / 720.0, keep));
            self.take(acts);
        }

        fn tick(&mut self, t: f64) {
            let acts = self.g.tick(t);
            self.take(acts);
        }

        fn take(&mut self, acts: Vec<Act>) {
            self.all.extend(acts.iter().copied());
            self.wire
                .extend(acts.into_iter().filter(|a| self.frames.sends(a)));
        }

        /// Tap, drag, two-finger scroll, two-finger tap, long-press drag, tap-drag and a
        /// three-finger tap. Returns how many finger events it made.
        fn tour(&mut self) -> usize {
            use FingerPhase::{Down, Move, Up};
            let mut n = 0;
            let mut f = |r: &mut Rig, p, id, x, y, t| {
                n += 1;
                r.finger(p, id, x, y, t);
            };
            f(self, Down, 1, 50.0, 50.0, 0.0);
            f(self, Up, 1, 50.0, 50.0, 40.0);
            f(self, Down, 2, 100.0, 100.0, 1000.0);
            f(self, Move, 2, 140.0, 100.0, 1016.0);
            f(self, Up, 2, 140.0, 100.0, 1032.0);
            f(self, Down, 3, 100.0, 200.0, 2000.0);
            f(self, Down, 4, 140.0, 200.0, 2002.0);
            for step in 1..=10 {
                let y = 200.0 - 2.0 * step as f32;
                f(self, Move, 3, 100.0, y, 2000.0 + 10.0 * step as f64);
                f(self, Move, 4, 140.0, y, 2001.0 + 10.0 * step as f64);
            }
            f(self, Up, 3, 100.0, 180.0, 2200.0);
            f(self, Up, 4, 140.0, 180.0, 2201.0);
            f(self, Down, 5, 50.0, 50.0, 3000.0);
            f(self, Down, 6, 80.0, 52.0, 3005.0);
            f(self, Up, 5, 50.0, 50.0, 3040.0);
            f(self, Up, 6, 80.0, 52.0, 3042.0);
            f(self, Down, 7, 50.0, 50.0, 4000.0);
            self.tick(4520.0);
            f(self, Move, 7, 90.0, 50.0, 4620.0);
            f(self, Up, 7, 90.0, 50.0, 4700.0);
            f(self, Down, 8, 300.0, 300.0, 5000.0);
            f(self, Up, 8, 300.0, 300.0, 5010.0);
            f(self, Down, 9, 302.0, 301.0, 5100.0);
            f(self, Move, 9, 340.0, 301.0, 5120.0);
            f(self, Up, 9, 340.0, 301.0, 5140.0);
            for (id, x) in [(10, 50.0), (11, 80.0), (12, 110.0)] {
                f(self, Down, id, x, 50.0, 6000.0 + x as f64);
            }
            for id in 10..=12 {
                f(self, Up, id, 0.0, 0.0, 6200.0 + id as f64);
            }
            n
        }
    }

    fn is_button(a: &Act) -> bool {
        matches!(a, Act::Button { .. })
    }

    #[test]
    fn frame_mode_off_sends_every_act_and_no_surface() {
        let mut r = Rig::new(None);
        r.tour();
        assert_eq!(r.wire, r.all);
        assert!(r.surface.is_empty());
        // The tour exercises every wire act the filter could drop.
        let kinds: [fn(&Act) -> bool; 3] = [
            |a| matches!(a, Act::MoveRel { .. }),
            |a| matches!(a, Act::Scroll { .. }),
            is_button,
        ];
        for kind in kinds {
            assert!(r.all.iter().any(kind), "{:?}", r.all);
        }
        assert!(r.all.contains(&Act::CycleStats));
        let mut off = FrameMode::default();
        assert!(off.release().is_empty());
        assert_eq!(off.set(false, true, &mut Gestures::new(true)), None);
    }

    #[test]
    fn toward_a_tapping_host_only_local_acts_reach_the_wire() {
        let mut r = Rig::new(Some(true));
        let events = r.tour();
        assert_eq!(r.wire, vec![Act::CycleStats], "{:?}", r.wire);
        // Every finger event reaches the surface, and the last lift leaves it empty.
        assert_eq!(r.surface.len(), events);
        assert_eq!(r.surface.last(), Some(&SurfaceUpdate::default()));
        assert_eq!(r.surface.iter().map(|u| u.touches.len()).max(), Some(3));
        assert!(!r.frames.retracts_taps());
    }

    #[test]
    fn toward_a_host_that_does_not_tap_the_clicks_still_reach_the_wire() {
        let mut r = Rig::new(Some(false));
        r.tour();
        let clicks: Vec<Act> = r
            .all
            .iter()
            .copied()
            .filter(|a| is_button(a) || *a == Act::CycleStats)
            .collect();
        // Tap, two-finger tap, long-press drag, tap then tap-drag: five presses.
        assert_eq!(clicks.iter().filter(|a| is_button(a)).count(), 10);
        assert_eq!(r.wire, clicks);
        assert!(r.frames.retracts_taps());
    }

    #[test]
    fn the_surface_gets_every_finger_down() {
        let mut f = FrameMode::default();
        let _ = f.set(true, true, &mut Gestures::new(true));
        let at = |touches: &[(u64, f32, f32)]| SurfaceUpdate {
            touches: touches.to_vec(),
            keep: false,
        };
        assert_eq!(
            f.finger(FingerPhase::Down, 7, 0.1, 0.2, false),
            Some(at(&[(7, 0.1, 0.2)]))
        );
        assert_eq!(
            f.finger(FingerPhase::Down, 9, 0.5, 0.5, false),
            Some(at(&[(7, 0.1, 0.2), (9, 0.5, 0.5)]))
        );
        assert_eq!(
            f.finger(FingerPhase::Move, 7, 0.2, 0.3, false),
            Some(at(&[(7, 0.2, 0.3), (9, 0.5, 0.5)]))
        );
        // A finger that landed before frame mode saw it stays off the surface.
        assert_eq!(f.finger(FingerPhase::Move, 4, 0.9, 0.9, false), None);
        assert_eq!(f.finger(FingerPhase::Up, 4, 0.9, 0.9, false), None);
        assert_eq!(
            f.finger(FingerPhase::Up, 7, 0.2, 0.3, false),
            Some(at(&[(9, 0.5, 0.5)]))
        );
        assert_eq!(
            f.finger(FingerPhase::Cancel, 9, 0.5, 0.5, false),
            Some(at(&[]))
        );
    }

    #[test]
    fn an_armed_twist_keeps_its_fingers_from_the_host() {
        let mut r = Rig::new(Some(false));
        let c = (120.0, 200.0);
        let (p1, p2) = twisted(c, 0.0);
        r.finger(FingerPhase::Down, 1, p1.0, p1.1, 0.0);
        r.finger(FingerPhase::Down, 2, p2.0, p2.1, 2.0);
        for step in 1..=3 {
            let (p1, p2) = twisted(c, 7.0 * step as f32);
            r.finger(FingerPhase::Move, 1, p1.0, p1.1, 10.0 * step as f64);
            r.finger(FingerPhase::Move, 2, p2.0, p2.1, 10.0 * step as f64 + 1.0);
        }
        // 7° has not armed; the 14° sample arms on the second finger's move and keeps.
        let keeps: Vec<bool> = r.surface.iter().map(|u| u.keep).collect();
        assert_eq!(keeps, [false, false, false, false, false, true, true, true]);
        assert!(
            r.wire.iter().all(|a| matches!(a, Act::Dial { .. })),
            "{:?}",
            r.wire
        );
        assert!(!r.wire.is_empty());
    }

    #[test]
    fn a_cancel_takes_every_finger_back_and_ends_the_gesture() {
        let mut r = Rig::new(Some(false));
        r.finger(FingerPhase::Down, 1, 100.0, 360.0, 0.0);
        r.finger(FingerPhase::Down, 2, 200.0, 360.0, 5.0);
        r.surface.clear();
        r.finger(FingerPhase::Cancel, 1, 100.0, 360.0, 30.0);
        assert_eq!(
            r.surface,
            vec![
                SurfaceUpdate {
                    touches: vec![(1, 100.0 / 1280.0, 0.5), (2, 200.0 / 1280.0, 0.5)],
                    keep: true,
                },
                SurfaceUpdate::default(),
            ]
        );
        // The rest of the touch is gone too: no click, no surface update.
        r.finger(FingerPhase::Move, 2, 220.0, 360.0, 40.0);
        r.finger(FingerPhase::Up, 2, 220.0, 360.0, 50.0);
        assert_eq!(r.surface.len(), 2);
        assert!(r.wire.is_empty(), "{:?}", r.wire);
        // Off, a cancel changes nothing.
        let mut off = Rig::new(None);
        off.finger(FingerPhase::Down, 1, 100.0, 360.0, 0.0);
        off.finger(FingerPhase::Cancel, 1, 100.0, 360.0, 30.0);
        off.finger(FingerPhase::Up, 1, 100.0, 360.0, 40.0);
        assert_eq!(off.wire, off.all);
        assert_eq!(off.all.len(), 2, "the lift still clicks: {:?}", off.all);
    }

    #[test]
    fn a_release_takes_every_finger_back_then_lifts() {
        let mut f = FrameMode::default();
        let _ = f.set(true, false, &mut Gestures::new(true));
        let _ = f.finger(FingerPhase::Down, 1, 0.5, 0.5, false);
        assert_eq!(
            f.release(),
            vec![
                SurfaceUpdate {
                    touches: vec![(1, 0.5, 0.5)],
                    keep: true,
                },
                SurfaceUpdate::default(),
            ]
        );
        assert_eq!(f.release(), vec![SurfaceUpdate::default()]);
    }

    #[test]
    fn inverted_scroll_keeps_frame_mode_off() {
        assert!(FrameMode::wanted(true, true, false));
        assert!(!FrameMode::wanted(true, true, true));
        assert!(!FrameMode::wanted(true, false, false));
        assert!(!FrameMode::wanted(false, true, false));
    }

    #[test]
    fn inverting_mid_stroke_hands_the_finger_back_to_the_gesture_machine() {
        use FingerPhase::{Down, Move, Up};
        let mut r = Rig::new(Some(true));
        r.finger(Down, 1, 100.0, 100.0, 0.0);
        r.finger(Move, 1, 140.0, 100.0, 16.0);
        assert!(r.wire.is_empty());
        let updates = r.surface.len();
        // The player turns inverted scroll on and the decision is made again.
        r.set_frames(FrameMode::wanted(true, true, true), true);
        r.finger(Move, 1, 180.0, 100.0, 32.0);
        assert!(
            matches!(r.wire.as_slice(), [Act::MoveRel { dx, .. }] if *dx > 0),
            "the same finger moves the pointer: {:?}",
            r.wire
        );
        r.finger(Up, 1, 180.0, 100.0, 48.0);
        assert_eq!(r.surface.len(), updates, "nothing more reaches the surface");
        // Turned off again: the announce, and the next tap is the host's.
        r.set_frames(FrameMode::wanted(true, true, false), true);
        assert_eq!(r.surface.last(), Some(&SurfaceUpdate::default()));
        r.wire.clear();
        r.finger(Down, 2, 50.0, 50.0, 1000.0);
        r.finger(Up, 2, 50.0, 50.0, 1040.0);
        assert!(r.wire.is_empty(), "{:?}", r.wire);
        assert_eq!(r.surface.len(), updates + 3);
    }

    #[test]
    fn a_refused_update_mid_scroll_opens_the_scroll_on_the_wire() {
        use FingerPhase::{Down, Move, Up};
        let mut r = Rig::new(Some(true));
        r.finger(Down, 1, 100.0, 200.0, 0.0);
        r.finger(Down, 2, 140.0, 200.0, 2.0);
        let pan = |r: &mut Rig, steps: std::ops::RangeInclusive<i32>| {
            for step in steps {
                let y = 200.0 - 2.0 * step as f32;
                r.finger(Move, 1, 100.0, y, 10.0 * f64::from(step));
                r.finger(Move, 2, 140.0, y, 10.0 * f64::from(step) + 1.0);
            }
        };
        pan(&mut r, 1..=15);
        assert!(r.wire.is_empty(), "{:?}", r.wire);
        // `Capture` turns frame mode off on a refused update.
        r.set_frames(false, false);
        pan(&mut r, 16..=17);
        r.finger(Up, 1, 100.0, 166.0, 200.0);
        r.finger(Up, 2, 140.0, 166.0, 201.0);
        assert_eq!(
            r.wire.first(),
            Some(&Act::Scroll {
                axis: 0,
                delta: 256,
                phase: TouchScrollPhase::Begin,
            })
        );
        assert_eq!(net_scroll(&r.wire), (1024, 0), "only what moved since");
        assert_eq!(
            r.wire.last(),
            Some(&Act::Scroll {
                axis: 0,
                delta: 0,
                phase: TouchScrollPhase::End,
            })
        );
    }

    #[test]
    fn a_refused_update_mid_tap_sends_back_no_unsent_scroll() {
        use FingerPhase::{Down, Move, Up};
        let mut r = Rig::new(Some(true));
        r.finger(Down, 1, 100.0, 200.0, 0.0);
        r.finger(Down, 2, 140.0, 200.0, 2.0);
        r.finger(Move, 1, 100.0, 196.0, 10.0);
        r.finger(Move, 2, 140.0, 196.0, 11.0);
        r.set_frames(false, false);
        r.finger(Up, 1, 100.0, 196.0, 40.0);
        r.finger(Up, 2, 140.0, 196.0, 41.0);
        assert_eq!(
            r.wire,
            vec![
                Act::Button {
                    gs: BTN_RIGHT,
                    down: true
                },
                Act::Button {
                    gs: BTN_RIGHT,
                    down: false
                },
            ]
        );
    }

    #[test]
    fn the_view_size_counts_160_logical_pixels_per_inch() {
        assert_eq!(view_dmm((1280, 800), 1.0), (2032, 1270));
        assert_eq!(view_dmm((2560, 1600), 2.0), (2032, 1270));
        assert_eq!(view_dmm((1280, 800), f32::NAN), (2032, 1270));
        assert_eq!(view_dmm((0, 0), 1.0), (0, 0));
    }

    #[test]
    fn reset_clears_a_drag_without_re_emitting() {
        let mut g = Gestures::new(true);
        let _ = g.down(1, 50.0, 50.0, ABS, 0.0);
        let _ = g.up(1, 5.0);
        let _ = g.down(2, 51.0, 50.0, ABS, 50.0);
        g.reset();
        // Reset must not leave a stuck drag: a later tap is an ordinary click.
        let mut acts = g.down(3, 400.0, 400.0, ABS, 500.0);
        acts.extend(g.up(3, 510.0));
        assert_eq!(
            acts,
            vec![
                Act::Button {
                    gs: BTN_LEFT,
                    down: true
                },
                Act::Button {
                    gs: BTN_LEFT,
                    down: false
                },
            ]
        );
    }
}
