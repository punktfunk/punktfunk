//! The scroll of a read-only page: Up and Down step, Left and Right page, a wheel steps,
//! a finger drags and its fling coasts. A step or a page glides there on the focus spring,
//! the same travel as a list following its cursor. The licences screen scrolls with it.

use crate::anim::{springs, Spring};
use crate::pointer::{Pointer, PointerKind};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};

/// Up, Down and a wheel step move this many lines; a fling decays at this rate per second.
const STEP_LINES: f64 = 3.0;
const FLING_DECAY: f64 = 4.0;

#[derive(Default)]
pub struct Flick {
    /// Where the page is drawn, px.
    pub scroll: f64,
    /// Where a key or a wheel asked it to go. A finger sets both at once.
    goal: f64,
    /// The glide's speed, or the fling's while `flinging`.
    vel: f64,
    flinging: bool,
    /// The viewport's and the content's heights, px; the owner sets both before `tick`.
    pub view_h: f64,
    pub content_h: f64,
    /// One Up or Down moves [`STEP_LINES`] of these, px: the owner's line height.
    pub step: f64,
    /// Reduce Motion, as the owner last drew: a step lands at once.
    pub reduced: bool,
}

impl Flick {
    /// Asks for `by` further on; `false` when already at that end.
    pub fn by(&mut self, by: f64) -> bool {
        let goal = (self.goal + by).clamp(0.0, self.max());
        if goal == self.goal {
            return false;
        }
        self.goal = goal;
        self.flinging = false;
        if self.reduced {
            self.scroll = goal;
            self.vel = 0.0;
        }
        true
    }

    /// Where the page is heading.
    #[cfg(test)]
    pub fn goal(&self) -> f64 {
        self.goal
    }

    fn max(&self) -> f64 {
        (self.content_h - self.view_h).max(0.0)
    }

    /// The offset and its reach, for a scroller's soft edges.
    pub fn scrolled(&self) -> (f32, f32) {
        (self.scroll as f32, self.max() as f32)
    }

    /// A finger moved the page by `dy`: it follows at once, and the glide forgets its goal.
    fn drag(&mut self, dy: f64) {
        self.scroll = (self.scroll - dy).clamp(0.0, self.max());
        self.goal = self.scroll;
        self.vel = 0.0;
        self.flinging = false;
    }

    /// Coasts a fling or glides to the goal, and keeps both inside the content after a
    /// relayout.
    pub fn tick(&mut self, dt: f64) {
        let max = self.max();
        if self.flinging {
            let before = self.scroll;
            self.scroll = (self.scroll + self.vel * dt).clamp(0.0, max);
            self.vel *= (-FLING_DECAY * dt).exp();
            if self.scroll == before || self.vel.abs() < 20.0 {
                self.vel = 0.0;
                self.flinging = false;
            }
            self.goal = self.scroll;
            return;
        }
        self.goal = self.goal.min(max);
        if self.scroll != self.goal || self.vel != 0.0 {
            let mut sp = Spring {
                pos: self.scroll,
                vel: self.vel,
            };
            sp.step_spec(self.goal, springs::FOCUS, dt);
            sp.settle(self.goal, 0.5, 5.0);
            self.scroll = sp.pos.clamp(0.0, max);
            self.vel = sp.vel;
        }
    }

    /// Up and Down step, Left and Right page; `None` leaves any other event to the owner.
    pub fn menu(&mut self, ev: MenuEvent) -> Option<MenuPulse> {
        let page = (self.view_h - self.step).max(self.step);
        let by = match ev {
            MenuEvent::Move(MenuDir::Up) => -STEP_LINES * self.step,
            MenuEvent::Move(MenuDir::Down) => STEP_LINES * self.step,
            MenuEvent::Move(MenuDir::Left) => -page,
            MenuEvent::Move(MenuDir::Right) => page,
            _ => return None,
        };
        Some(if self.by(by) {
            MenuPulse::Move
        } else {
            MenuPulse::Boundary
        })
    }

    /// A wheel step scrolls; every other press lands on the page and does nothing.
    pub fn pointer(&mut self, p: Pointer) {
        if let PointerKind::Scroll { up } = p.kind {
            let by = STEP_LINES * self.step;
            self.by(if up { -by } else { by });
        }
    }

    /// A vertical drag moves the page with the finger; its fling coasts.
    pub fn pan(&mut self, p: Pointer) -> bool {
        match p.kind {
            PointerKind::PanStart { horizontal } => {
                self.drag(0.0);
                !horizontal
            }
            PointerKind::Pan { dy, .. } => {
                self.drag(dy);
                true
            }
            PointerKind::Fling { vy, .. } => {
                self.vel = -vy;
                self.flinging = true;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Flick {
        Flick {
            view_h: 600.0,
            content_h: 3000.0,
            step: 20.0,
            ..Flick::default()
        }
    }

    /// A step moves the goal at once and the page over the next frames, never past it.
    #[test]
    fn a_step_glides_to_its_goal() {
        let mut f = page();
        assert!(matches!(
            f.menu(MenuEvent::Move(MenuDir::Down)),
            Some(MenuPulse::Move)
        ));
        assert_eq!((f.goal(), f.scroll), (60.0, 0.0));
        f.tick(1.0 / 60.0);
        assert!(f.scroll > 0.0 && f.scroll < 60.0, "{}", f.scroll);
        for _ in 0..120 {
            f.tick(1.0 / 60.0);
            assert!(f.scroll <= 61.0, "{}", f.scroll);
        }
        assert_eq!(f.scroll, 60.0);
    }

    /// The ends bump; a page is the viewport less a step; reduced motion lands at once.
    #[test]
    fn the_ends_bump_and_reduced_motion_lands_at_once() {
        let mut f = page();
        assert!(matches!(
            f.menu(MenuEvent::Move(MenuDir::Up)),
            Some(MenuPulse::Boundary)
        ));
        f.menu(MenuEvent::Move(MenuDir::Right));
        assert_eq!(f.goal(), 580.0);
        f.reduced = true;
        f.menu(MenuEvent::Move(MenuDir::Right));
        assert_eq!((f.goal(), f.scroll), (1160.0, 1160.0));
    }

    /// A finger owns the offset while it drags; its fling coasts and stops inside the content.
    #[test]
    fn a_finger_drags_and_its_fling_coasts() {
        let mut f = page();
        let pan = |kind| Pointer {
            x: 0.0,
            y: 0.0,
            kind,
        };
        assert!(f.pan(pan(PointerKind::PanStart { horizontal: false })));
        f.pan(pan(PointerKind::Pan {
            dx: 0.0,
            dy: -100.0,
        }));
        assert_eq!((f.scroll, f.goal()), (100.0, 100.0));
        f.pan(pan(PointerKind::Fling {
            vx: 0.0,
            vy: -2000.0,
        }));
        for _ in 0..300 {
            f.tick(1.0 / 60.0);
        }
        assert!(f.scroll > 100.0 && f.scroll <= 2400.0, "{}", f.scroll);
        assert_eq!(f.scroll, f.goal());
    }
}
