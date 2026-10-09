//! Pointer and touch input inside the console.
//!
//! Widgets act on press, not release. Focused list and carousel items scroll toward
//! the centre, so the pressed row has already moved by lift; click-on-release would
//! hit the wrong row. A finger's press reaches them only at its lift: `Touch` turns
//! a tap into a press at the contact point and a drag into pans or scroll ticks.
//!
//! Coordinates are device pixels: the run loop converts (it owns the window and the
//! display scale). A widget hit-tests the rect it drew last frame.

use pf_client_core::console::{PointerButton, PointerInput};
use skia_safe::Rect;
use std::collections::VecDeque;

/// Max finger wander (design units × `k`) that still counts as a tap. 12 dp is
/// classic touch slop; in device pixels it matches Android ViewConfiguration.
const TOUCH_SLOP_DP: f64 = 12.0;
/// Dominant-axis travel (design units × `k`) per synthetic scroll tick. 56 is
/// the menu row pitch (`widgets::ROW_H` + gap), so the list tracks the finger.
pub(crate) const DRAG_TICK_DP: f64 = 56.0;
/// A finger held this still for this long is a long press.
const LONG_PRESS_S: f64 = 0.5;
/// A fling's velocity is the finger's over its last this-many seconds.
const FLING_WINDOW_S: f64 = 0.1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pointer {
    pub x: f64,
    pub y: f64,
    pub kind: PointerKind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerKind {
    /// Primary button down, or a finger's tap (sent at its lift) — the acting edge.
    Press,
    /// Primary button or finger up. Ignored today; kept so a drag can close.
    Release,
    /// Motion, with or without a button held.
    Move,
    /// Gesture abandoned (pointer left the window).
    Cancel,
    /// One scroll step; `up` = away from the user.
    Scroll { up: bool },
    /// Secondary (right) button down — the pointer's B. The shell handles it for every screen.
    Back,
    /// A finger dragged past slop, locked to this axis. At the anchor. Consumed, the drag
    /// arrives as `Pan` steps and a `Fling`; declined, it becomes `Scroll` ticks.
    PanStart { horizontal: bool },
    /// A taken drag moved this far since its last step, device pixels, on its axis.
    Pan { dx: f64, dy: f64 },
    /// A panning finger lifted, moving this fast (px/s, locked axis). Zero is a plain
    /// release; every taken drag ends in one.
    Fling { vx: f64, vy: f64 },
    /// A finger held still for [`LONG_PRESS_S`], at the anchor. Consumed, its lift is no tap.
    LongPress,
}

impl Pointer {
    pub fn press(&self) -> bool {
        self.kind == PointerKind::Press
    }

    /// Half-open, so neighbours can share an edge. An empty rect never hits — culled
    /// list rows store `Rect::new_empty()` and keep their indices aligned.
    pub fn hits(&self, rect: Rect) -> bool {
        let (x, y) = (self.x as f32, self.y as f32);
        x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
    }

    pub fn pick(&self, rects: &[Rect]) -> Option<usize> {
        rects.iter().position(|r| self.hits(*r))
    }
}

/// Host pointer events as [`Pointer`]s: one translator per surface, so every surface
/// reads a finger the same way.
///
/// A mouse press acts on contact. A finger down arms; a lift within slop is a tap,
/// sent as Press + Release at the *anchor*, since widgets hit-test last frame's rects
/// and the focused item may have scrolled since. Held still, [`Self::tick`] turns it
/// into a [`PointerKind::LongPress`]. Past slop the gesture axis-locks and offers a
/// [`PointerKind::PanStart`]; a surface that takes it gets every step and a
/// [`PointerKind::Fling`] at lift, one that does not gets a Scroll tick per
/// [`DRAG_TICK_DP`]·k of travel. That lift acts on nothing. A second finger is
/// ignored. Secondary-down is Back and its release is dropped, or a right-click would
/// pop two screens. Wheel is discrete scroll.
#[derive(Default)]
pub(crate) struct Touch {
    gesture: Option<Gesture>,
}

#[derive(Clone, Debug)]
enum Gesture {
    /// Finger down at `t`, still within slop.
    Armed { x: f64, y: f64, t: f64 },
    /// A long press was taken: the lift acts on nothing.
    Held,
    /// Slop exceeded. Axis-locked from the first exit so diagonal jitter cannot
    /// alternate a carousel with a list.
    Drag {
        x: f64,
        y: f64,
        horizontal: bool,
        mode: DragMode,
    },
}

#[derive(Clone, Debug)]
enum DragMode {
    /// Nobody took the pan. `last` is the last tick's dominant-axis position.
    Ticks { last: f64 },
    /// Taken. `samples` are `(t, position)` on the locked axis, the fling window's worth.
    Pan { samples: VecDeque<(f64, f64)> },
}

impl Touch {
    /// One host event at design scale `k` and shell time `now` (seconds). `deliver`
    /// takes each [`Pointer`] it becomes and says whether it was consumed; so does the
    /// return value.
    pub(crate) fn feed(
        &mut self,
        input: PointerInput,
        k: f64,
        now: f64,
        mut deliver: impl FnMut(Pointer) -> bool,
    ) -> bool {
        let (x, y, kind) = match input {
            PointerInput::Move { x, y } => {
                if self.gesture.is_some() {
                    self.drag(f64::from(x), f64::from(y), k, now, &mut deliver);
                    return true;
                }
                (x, y, PointerKind::Move)
            }
            PointerInput::Down {
                x,
                y,
                button: PointerButton::Primary,
                touch,
            } => {
                if touch {
                    if self.gesture.is_none() {
                        self.gesture = Some(Gesture::Armed {
                            x: f64::from(x),
                            y: f64::from(y),
                            t: now,
                        });
                    }
                    return true;
                }
                (x, y, PointerKind::Press)
            }
            PointerInput::Down {
                x,
                y,
                button: PointerButton::Secondary,
                ..
            } => (x, y, PointerKind::Back),
            PointerInput::Up {
                x,
                y,
                button: PointerButton::Primary,
            } => match self.gesture.take() {
                Some(Gesture::Armed { x, y, .. }) => {
                    let consumed = deliver(Pointer {
                        x,
                        y,
                        kind: PointerKind::Press,
                    });
                    deliver(Pointer {
                        x,
                        y,
                        kind: PointerKind::Release,
                    });
                    return consumed;
                }
                Some(Gesture::Drag {
                    x: ax,
                    y: ay,
                    horizontal,
                    mode: DragMode::Pan { samples },
                }) => {
                    let v = velocity(&samples, now);
                    let (vx, vy) = if horizontal { (v, 0.0) } else { (0.0, v) };
                    deliver(Pointer {
                        x: ax,
                        y: ay,
                        kind: PointerKind::Fling { vx, vy },
                    });
                    return true;
                }
                // Ticks already fired; a held press already acted.
                Some(Gesture::Drag { .. } | Gesture::Held) => return true,
                None => (x, y, PointerKind::Release),
            },
            PointerInput::Up { .. } => return true,
            PointerInput::Wheel { x, y, dy } => {
                if dy == 0.0 {
                    return true;
                }
                (x, y, PointerKind::Scroll { up: dy > 0.0 })
            }
            PointerInput::Cancel => {
                // A taken drag always ends in a fling, or its scroll stays held.
                if let Some(Gesture::Drag {
                    x,
                    y,
                    mode: DragMode::Pan { .. },
                    ..
                }) = self.gesture
                {
                    deliver(Pointer {
                        x,
                        y,
                        kind: PointerKind::Fling { vx: 0.0, vy: 0.0 },
                    });
                }
                self.reset();
                (0.0, 0.0, PointerKind::Cancel)
            }
        };
        deliver(Pointer {
            x: f64::from(x),
            y: f64::from(y),
            kind,
        })
    }

    /// Once a frame: a finger armed for [`LONG_PRESS_S`] becomes a long press. Taken,
    /// its lift is nothing; declined, the finger stays armed and can still tap or drag.
    pub(crate) fn tick(&mut self, now: f64, mut deliver: impl FnMut(Pointer) -> bool) {
        let Some(Gesture::Armed { x, y, t }) = self.gesture else {
            return;
        };
        if now - t < LONG_PRESS_S {
            return;
        }
        let taken = deliver(Pointer {
            x,
            y,
            kind: PointerKind::LongPress,
        });
        self.gesture = Some(if taken {
            Gesture::Held
        } else {
            // Declined: never offered again for this finger.
            Gesture::Armed {
                x,
                y,
                t: f64::INFINITY,
            }
        });
    }

    /// Forget a live finger without acting; its lift then arrives as a plain Release.
    pub(crate) fn reset(&mut self) {
        self.gesture = None;
    }

    /// Advance a finger Move. Past slop, lock to the dominant axis and offer the pan;
    /// down/right = previous (wheel-up), up/left = next.
    fn drag(
        &mut self,
        x: f64,
        y: f64,
        k: f64,
        now: f64,
        deliver: &mut impl FnMut(Pointer) -> bool,
    ) {
        match &mut self.gesture {
            Some(Gesture::Armed { x: ax, y: ay, .. }) => {
                let (ax, ay) = (*ax, *ay);
                let (dx, dy) = (x - ax, y - ay);
                if dx.hypot(dy) < TOUCH_SLOP_DP * k {
                    return;
                }
                let horizontal = dx.abs() > dy.abs();
                // Pans and ticks both start where slop was left, not at the anchor.
                let pos = if horizontal { x } else { y };
                let taken = deliver(Pointer {
                    x: ax,
                    y: ay,
                    kind: PointerKind::PanStart { horizontal },
                });
                let mode = if taken {
                    DragMode::Pan {
                        samples: VecDeque::from([(now, pos)]),
                    }
                } else {
                    DragMode::Ticks { last: pos }
                };
                self.gesture = Some(Gesture::Drag {
                    x: ax,
                    y: ay,
                    horizontal,
                    mode,
                });
            }
            Some(Gesture::Drag {
                x: ax,
                y: ay,
                horizontal,
                mode,
            }) => {
                let (ax, ay, horizontal) = (*ax, *ay, *horizontal);
                let pos = if horizontal { x } else { y };
                match mode {
                    DragMode::Pan { samples } => {
                        let step = pos - samples.back().map_or(pos, |s| s.1);
                        samples.push_back((now, pos));
                        while samples.front().is_some_and(|s| now - s.0 > FLING_WINDOW_S) {
                            samples.pop_front();
                        }
                        let (dx, dy) = if horizontal { (step, 0.0) } else { (0.0, step) };
                        deliver(Pointer {
                            x: ax,
                            y: ay,
                            kind: PointerKind::Pan { dx, dy },
                        });
                    }
                    DragMode::Ticks { last } => {
                        let tick = DRAG_TICK_DP * k;
                        let steps = ((pos - *last) / tick).trunc();
                        if steps == 0.0 {
                            return;
                        }
                        *last += steps * tick;
                        let up = steps > 0.0;
                        for _ in 0..steps.abs() as u32 {
                            deliver(Pointer {
                                x: ax,
                                y: ay,
                                kind: PointerKind::Scroll { up },
                            });
                        }
                    }
                }
            }
            Some(Gesture::Held) | None => {}
        }
    }
}

/// The finger's speed over the fling window ending at `now`, px/s. A finger that
/// stopped before lifting has no samples left in the window and flings nothing.
fn velocity(samples: &VecDeque<(f64, f64)>, now: f64) -> f64 {
    let mut recent = samples.iter().filter(|s| now - s.0 <= FLING_WINDOW_S);
    let (Some(first), Some(last)) = (recent.next(), recent.next_back()) else {
        return 0.0;
    };
    let dt = last.0 - first.0;
    if dt <= 0.0 {
        return 0.0;
    }
    (last.1 - first.1) / dt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f64, y: f64) -> Pointer {
        Pointer {
            x,
            y,
            kind: PointerKind::Press,
        }
    }

    #[test]
    fn hit_testing_is_half_open_and_skips_empty_rects() {
        let r = Rect::from_xywh(10.0, 10.0, 20.0, 20.0);
        assert!(at(10.0, 10.0).hits(r), "the top-left corner is inside");
        assert!(
            !at(30.0, 20.0).hits(r),
            "the right edge belongs to the next"
        );
        assert!(!at(9.0, 20.0).hits(r));
        assert!(!at(0.0, 0.0).hits(Rect::new_empty()));
    }

    #[test]
    fn pick_returns_the_first_match() {
        let rects = [
            Rect::new_empty(),
            Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
            Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
        ];
        assert_eq!(at(5.0, 5.0).pick(&rects), Some(1));
        assert_eq!(at(50.0, 5.0).pick(&rects), None);
    }

    fn down(y: f32) -> PointerInput {
        PointerInput::Down {
            x: 100.0,
            y,
            button: PointerButton::Primary,
            touch: true,
        }
    }

    fn up(y: f32) -> PointerInput {
        PointerInput::Up {
            x: 100.0,
            y,
            button: PointerButton::Primary,
        }
    }

    fn to(y: f32) -> PointerInput {
        PointerInput::Move { x: 100.0, y }
    }

    /// A finger dragged up 200 px over 100 ms, 10 ms a step, then lifted.
    fn swipe(touch: &mut Touch, take_pans: bool) -> Vec<PointerKind> {
        let mut got = Vec::new();
        let mut deliver = |p: Pointer| {
            got.push(p.kind);
            take_pans
                && matches!(
                    p.kind,
                    PointerKind::PanStart { .. }
                        | PointerKind::Pan { .. }
                        | PointerKind::Fling { .. }
                )
        };
        touch.feed(down(400.0), 1.0, 0.0, &mut deliver);
        for i in 1..=10 {
            let y = 400.0 - 20.0 * i as f32;
            touch.feed(to(y), 1.0, 0.01 * f64::from(i), &mut deliver);
        }
        touch.feed(up(200.0), 1.0, 0.1, &mut deliver);
        got
    }

    #[test]
    fn a_taken_drag_pans_the_finger_and_flings_at_lift() {
        let got = swipe(&mut Touch::default(), true);
        // Slop is left on the first 20 px step: the offer, then nine steps of -20.
        assert_eq!(got[0], PointerKind::PanStart { horizontal: false });
        assert!(got[1..10]
            .iter()
            .all(|k| *k == PointerKind::Pan { dx: 0.0, dy: -20.0 }));
        let PointerKind::Fling { vx, vy } = got[10] else {
            panic!("a fling ends the drag: {got:?}");
        };
        assert_eq!(vx, 0.0);
        assert!((vy + 2000.0).abs() < 1.0, "20 px per 10 ms up: {vy}");
        assert!(
            !got.contains(&PointerKind::Press),
            "a drag's lift never taps"
        );
    }

    #[test]
    fn a_declined_drag_scrolls_by_ticks() {
        let got = swipe(&mut Touch::default(), false);
        assert_eq!(got[0], PointerKind::PanStart { horizontal: false });
        // 180 px past the slop exit at 56 px a tick: three ticks toward "next".
        assert_eq!(&got[1..], [PointerKind::Scroll { up: false }; 3]);
    }

    #[test]
    fn a_finger_that_stops_before_lifting_flings_nothing() {
        let mut touch = Touch::default();
        let mut flung = None;
        let mut deliver = |p: Pointer| {
            if let PointerKind::Fling { vy, .. } = p.kind {
                flung = Some(vy);
            }
            true
        };
        touch.feed(down(400.0), 1.0, 0.0, &mut deliver);
        touch.feed(to(300.0), 1.0, 0.05, &mut deliver);
        touch.feed(to(200.0), 1.0, 0.10, &mut deliver);
        touch.feed(up(200.0), 1.0, 0.50, &mut deliver);
        assert_eq!(flung, Some(0.0));
    }

    #[test]
    fn a_still_finger_long_presses_once_and_its_lift_does_not_tap() {
        let mut touch = Touch::default();
        let mut got = Vec::new();
        touch.feed(down(400.0), 1.0, 0.0, |p| {
            got.push(p.kind);
            true
        });
        touch.tick(0.4, |p| {
            got.push(p.kind);
            true
        });
        assert!(got.is_empty(), "not yet");
        touch.tick(0.6, |p| {
            got.push(p.kind);
            true
        });
        touch.tick(0.7, |p| {
            got.push(p.kind);
            true
        });
        touch.feed(up(400.0), 1.0, 0.8, |p| {
            got.push(p.kind);
            true
        });
        assert_eq!(got, [PointerKind::LongPress]);
    }

    #[test]
    fn a_declined_long_press_still_taps() {
        let mut touch = Touch::default();
        let mut got = Vec::new();
        touch.feed(down(400.0), 1.0, 0.0, |_| true);
        touch.tick(0.6, |p| {
            got.push(p.kind);
            false
        });
        touch.tick(0.9, |p| {
            got.push(p.kind);
            false
        });
        touch.feed(up(400.0), 1.0, 1.0, |p| {
            got.push(p.kind);
            true
        });
        assert_eq!(
            got,
            [
                PointerKind::LongPress,
                PointerKind::Press,
                PointerKind::Release
            ]
        );
    }
}
