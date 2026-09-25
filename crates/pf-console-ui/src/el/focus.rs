//! Focus on the element tree: which target a direction reaches, and the plate behind it.
//!
//! Search follows the tvOS focus engine: of the targets ahead in the direction, one that
//! overlaps the current target across the axis wins, then the nearest, then the most
//! centred. A [`Group`] is searched before the rest of the tree along the axes it holds;
//! a group with an id hands focus back to the child it last had.
//!
//! [`Plate`] is one sprung rounded rect that morphs from target to target behind the
//! focused node. Its leading edge runs ahead and its trailing edge overshoots into it, so
//! one motion stretches it on the way and squashes it on arrival. With no target it fades
//! out. Under Reduce Motion it jumps and fades. Pinned by `el::tests`.

use crate::anim::{Spring, SpringSpec};
use crate::theme::{accent, fg, fill, stroke};
use pf_client_core::menu_nav::MenuDir;
use skia_safe::{gradient, BlurStyle, Canvas, Color4f, MaskFilter, Point, RRect, Rect, TileMode};

/// A focus container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// Left and Right stay inside; Up and Down leave.
    Row,
    /// Up and Down stay inside; Left and Right leave.
    Column,
    /// Every direction stays inside while a target lies that way.
    Grid,
}

impl Group {
    pub(crate) fn holds(self, dir: MenuDir) -> bool {
        match self {
            Group::Row => matches!(dir, MenuDir::Left | MenuDir::Right),
            Group::Column => matches!(dir, MenuDir::Up | MenuDir::Down),
            Group::Grid => true,
        }
    }
}

/// How `to` ranks as the next target from `from` going `dir`; lower is better. `None`
/// when it is not ahead.
pub(crate) fn score(from: Rect, to: Rect, dir: MenuDir) -> Option<(u8, f32, f32)> {
    let (f, t) = (from.center(), to.center());
    let overlap = |a0: f32, a1: f32, b0: f32, b1: f32| a1.min(b1) - a0.max(b0);
    let (ahead, gap, across, off) = match dir {
        MenuDir::Right => (
            t.x - f.x,
            to.left - from.right,
            overlap(from.top, from.bottom, to.top, to.bottom),
            t.y - f.y,
        ),
        MenuDir::Left => (
            f.x - t.x,
            from.left - to.right,
            overlap(from.top, from.bottom, to.top, to.bottom),
            t.y - f.y,
        ),
        MenuDir::Down => (
            t.y - f.y,
            to.top - from.bottom,
            overlap(from.left, from.right, to.left, to.right),
            t.x - f.x,
        ),
        MenuDir::Up => (
            f.y - t.y,
            from.top - to.bottom,
            overlap(from.left, from.right, to.left, to.right),
            t.x - f.x,
        ),
    };
    (ahead > 0.5).then_some((u8::from(across <= 0.0), gap.max(0.0), off.abs()))
}

/// The plate's travel: the focus spring. The size it morphs through rides it too.
pub const TRAVEL: SpringSpec = crate::anim::springs::FOCUS;
/// The leading edge, quicker than [`TRAVEL`]: it covers most of the hop first.
const LEAD: SpringSpec = SpringSpec {
    response: 0.18,
    damping: 0.85,
};
/// The trailing edge, softer: it lags, then runs a little past its target into the front.
/// One motion stretches the plate on the way and squashes it on arrival: on a one-card hop
/// about 1.35× at 50 ms, 0.92× at 220 ms, settled by 0.55 s.
const TRAIL: SpringSpec = SpringSpec {
    response: 0.36,
    damping: 0.62,
};
/// An axis leads and trails only when the hop's unit direction moves along it this much.
const AXIS_MIN: f64 = 0.3;
/// Plate growth past its target, design units.
const OUTSET: f32 = 7.0;
/// Stretch and squash saturate toward these fractions of the plate's length.
const STRETCH_MAX: f64 = 0.5;
const SQUASH_MAX: f64 = 0.12;
/// Of a stretch, the share the cross axis gives up; of a squash, the share it bulges.
const THIN: f64 = 0.2;
const BULGE: f64 = 0.6;
/// Fade time constant, seconds: out with no target or while dormant, in on the way back.
const FADE_TAU: f64 = 0.06;

thread_local! {
    /// The last plate to give up focus, for a plate in another tree to glide on from.
    static HANDOFF: std::cell::Cell<Option<Handoff>> = const { std::cell::Cell::new(None) };
    /// Surface frames begun ([`begin_frame`]); a handoff keeps for its frame and the next.
    static FRAME: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static NEXT_PLATE: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

/// Where a plate stood when it gave up focus: device px, so any tree can read it.
#[derive(Clone, Copy)]
struct Handoff {
    owner: u64,
    rect: Rect,
    corner: f32,
    frame: u64,
    taken: bool,
}

/// One frame of the whole surface is starting. Called once per shell render.
pub fn begin_frame() {
    FRAME.with(|f| f.set(f.get() + 1));
}

/// `owner`'s plate just gave up focus at `rect`, device px.
pub(crate) fn offer(owner: u64, rect: Rect, corner: f32) {
    let frame = FRAME.with(std::cell::Cell::get);
    HANDOFF.with(|h| {
        h.set(Some(Handoff {
            owner,
            rect,
            corner,
            frame,
            taken: false,
        }));
    });
}

/// The rect another plate left this frame or the last, once: a plate that takes it glides
/// on from there, and the one that left hides.
pub(crate) fn take(taker: u64) -> Option<(Rect, f32)> {
    let frame = FRAME.with(std::cell::Cell::get);
    HANDOFF.with(|h| {
        let mut o = h
            .get()
            .filter(|o| o.owner != taker && !o.taken && frame <= o.frame + 1)?;
        o.taken = true;
        h.set(Some(o));
        Some((o.rect, o.corner))
    })
}

/// Drop any rect a plate left: nothing drawn since should glide from it.
pub(crate) fn forget_handoff() {
    HANDOFF.with(|h| h.set(None));
}

/// Another plate took up where `owner`'s left off.
pub(crate) fn taken_from(owner: u64) -> bool {
    HANDOFF.with(|h| h.get().is_some_and(|o| o.owner == owner && o.taken))
}

/// Sprung rect behind the focused node. It springs in its scroll's content space, so it
/// rides a scrolling list rigidly and only its own travel lags. In flight its leading edge
/// runs ahead and its trailing edge overshoots into it, so it stretches, squashes and settles
/// as one motion; with nothing to rest on it glides on and fades out, and never freezes on
/// a stale rect.
#[derive(Default)]
pub struct Plate {
    /// left, top, right, bottom, corner, in `space`'s content px; `None` until the first
    /// target.
    edges: Option<[Spring; 5]>,
    /// Width and height on [`TRAVEL`]: the size the plate morphs through, which its span is
    /// measured against.
    size: [Spring; 2],
    /// Came in from another tree or scroll this hop. Its viewport would cut it on the way
    /// in, so it draws unclipped until it lands.
    arriving: bool,
    /// Where the edges are springing to: the last target's rect and corner.
    goal: [f64; 5],
    /// Target the plate is travelling to or resting on.
    to: Option<super::Id>,
    /// Scroll whose content the plate lives in, and that scroll's shift this frame.
    space: Option<super::Id>,
    shift: (f32, f32),
    /// Seconds on the plate's own clock.
    t: f64,
    /// Unit direction of the last hop: which edges lead and which trail.
    axis: (f64, f64),
    /// Travelling: set by a new target, cleared on landing.
    armed: bool,
    /// Opacity 0..1, easing toward `fade_to`.
    shown: f64,
    fade_to: f64,
    /// OK went down: the plate's scale, springing back to 1.
    press: Option<Spring>,
    /// This plate's name in a [`Handoff`]; 0 until first asked.
    id: u64,
    /// Focus arrived with nothing on screen: this frame the plate waits, unseen, for a plate
    /// leaving another tree ([`take`]); `fresh` if it had never been drawn.
    waiting: bool,
    fresh: bool,
}

impl Plate {
    /// Chase `target`, `id`'s content rect in the scroll `space` shifted by `shift` this
    /// frame, by `dt` seconds. Dormant, it fades out on the way; a plate that had faded
    /// out starts over on its target instead of gliding in from where it vanished.
    pub(crate) fn step(
        &mut self,
        id: super::Id,
        target: Rect,
        corner: f32,
        dt: f64,
        space: Option<super::Id>,
        shift: (f32, f32),
    ) {
        // A new scroll: carry the plate over where it stands on screen.
        if let Some(e) = self.edges.as_mut().filter(|_| space != self.space) {
            let (dx, dy) = (
                f64::from(shift.0 - self.shift.0),
                f64::from(shift.1 - self.shift.1),
            );
            for (s, d) in e.iter_mut().zip([dx, dy, dx, dy, 0.0]) {
                s.pos += d;
            }
            self.arriving = true;
        }
        self.space = space;
        self.shift = shift;
        let from = self.goal;
        self.goal = [
            f64::from(target.left),
            f64::from(target.top),
            f64::from(target.right),
            f64::from(target.bottom),
            f64::from(corner),
        ];
        let reduced = crate::theme::reduce_motion();
        let live = !super::dormant();
        if self.to != Some(id) {
            self.armed = true;
            let (dx, dy) = (
                self.goal[0] + self.goal[2] - from[0] - from[2],
                self.goal[1] + self.goal[3] - from[1] - from[3],
            );
            if dx.hypot(dy) > 1.0 {
                self.axis = (dx / dx.hypot(dy), dy / dx.hypot(dy));
            }
            if reduced {
                self.shown = 0.0;
            }
        }
        self.to = Some(id);
        // Nothing on screen and focus arriving: wait one frame, unseen, for a plate leaving
        // another tree to glide on from. The leaving tree may paint after this one.
        let empty = self.edges.is_none() || self.shown == 0.0;
        if empty && live && !reduced && !self.waiting {
            self.waiting = true;
            self.fresh = self.edges.is_none();
            self.rest_on(self.goal);
            self.shown = 0.0;
            self.armed = true;
            return;
        }
        if std::mem::take(&mut self.waiting) && empty && self.fresh {
            // No plate left one: the first appears in place, as it always did.
            self.shown = 1.0;
        }
        if self.edges.is_none() {
            self.shown = if live && !reduced { 1.0 } else { 0.0 };
        }
        if reduced || self.edges.is_none() || self.shown == 0.0 {
            self.rest_on(self.goal);
        }
        self.travel(dt, live);
    }

    /// At rest on `at` (left, top, right, bottom, corner), at its own size.
    fn rest_on(&mut self, at: [f64; 5]) {
        self.edges = Some(at.map(Spring::rest));
        self.size = [Spring::rest(at[2] - at[0]), Spring::rest(at[3] - at[1])];
    }

    /// No target this frame: the plate glides on to its last goal and fades out, riding
    /// its scroll at `shift` while that scroll is drawn.
    pub(crate) fn lose(&mut self, dt: f64, shift: Option<(f32, f32)>) {
        if let Some(s) = shift {
            self.shift = s;
        }
        self.waiting = false;
        self.travel(dt, false);
    }

    /// This plate's name in a handoff.
    pub(crate) fn id(&mut self) -> u64 {
        if self.id == 0 {
            self.id = NEXT_PLATE.with(|n| n.replace(n.get() + 1));
        }
        self.id
    }

    /// Showing, and staying: what a plate is when it can hand off.
    pub(crate) fn live(&self) -> bool {
        self.fade_to == 1.0 && self.shown > 0.0
    }

    /// Waiting this frame for a plate to glide on from.
    pub(crate) fn waiting(&self) -> bool {
        self.waiting
    }

    /// Start from `rect` (content px of `space`, shifted by `shift`), fully shown and drawn
    /// unclipped: the next step glides from here to the target.
    pub(crate) fn seed(
        &mut self,
        rect: Rect,
        corner: f32,
        space: Option<super::Id>,
        shift: (f32, f32),
    ) {
        let at = [
            f64::from(rect.left),
            f64::from(rect.top),
            f64::from(rect.right),
            f64::from(rect.bottom),
            f64::from(corner),
        ];
        let (dx, dy) = (
            self.goal[0] + self.goal[2] - at[0] - at[2],
            self.goal[1] + self.goal[3] - at[1] - at[3],
        );
        if dx.hypot(dy) > 1.0 {
            self.axis = (dx / dx.hypot(dy), dy / dx.hypot(dy));
        }
        self.rest_on(at);
        self.space = space;
        self.shift = shift;
        self.shown = 1.0;
        self.fade_to = 1.0;
        self.waiting = false;
        self.arriving = true;
    }

    /// In from another tree or scroll and not landed yet: its viewport must not clip it.
    pub(crate) fn arriving(&self) -> bool {
        self.arriving
    }

    /// Gone at once: another plate took up where this one left.
    pub(crate) fn hide(&mut self) {
        self.shown = 0.0;
        self.fade_to = 0.0;
    }

    /// The scroll the plate lives in.
    pub(crate) fn space(&self) -> Option<super::Id> {
        self.space
    }

    /// Spring the edges and size `dt` toward the goal and fade toward `live`. Along the hop
    /// the leading edge takes [`LEAD`] and the trailing edge [`TRAIL`].
    fn travel(&mut self, dt: f64, live: bool) {
        self.t += dt;
        let Some(edges) = self.edges.as_mut() else {
            return;
        };
        let (ax, ay) = self.axis;
        let specs = [
            edge_spec(ax, false),
            edge_spec(ay, false),
            edge_spec(ax, true),
            edge_spec(ay, true),
            TRAVEL,
        ];
        for ((s, g), spec) in edges.iter_mut().zip(self.goal).zip(specs) {
            s.step_spec(g, spec, dt);
            s.settle(g, 0.25, 4.0);
        }
        let size = [self.goal[2] - self.goal[0], self.goal[3] - self.goal[1]];
        for (s, g) in self.size.iter_mut().zip(size) {
            s.step_spec(g, TRAVEL, dt);
            s.settle(g, 0.25, 4.0);
        }
        self.fade_to = if live { 1.0 } else { 0.0 };
        self.shown = crate::anim::approach(self.shown, self.fade_to, dt, FADE_TAU);
        if (self.shown - self.fade_to).abs() < 0.01 {
            self.shown = self.fade_to;
        }
        if let Some(p) = self.press.as_mut() {
            p.step_spec(1.0, crate::anim::springs::PRESS, dt);
            p.settle(1.0, 0.0005, 0.01);
        }
        self.press = self.press.filter(|p| p.pos != 1.0 || p.vel != 0.0);
        let landed = edges
            .iter()
            .zip(self.goal)
            .chain(self.size.iter().zip(size))
            .all(|(s, g)| s.pos == g && s.vel == 0.0);
        if landed {
            self.arriving = false;
            self.armed = false;
        }
    }

    /// Still travelling, fading or pressed: the frame loop must keep drawing.
    pub fn busy(&self) -> bool {
        self.armed || self.shown != self.fade_to || self.press.is_some()
    }

    /// Any of the plate is on screen.
    pub(crate) fn visible(&self) -> bool {
        self.shown > 0.0
    }

    /// OK went down on the focused node: the plate dips and springs back. Reduce Motion
    /// keeps it still; the release acts either way.
    pub(crate) fn press(&mut self) {
        if !crate::theme::reduce_motion() {
            self.press = Some(Spring::rest(crate::anim::PRESS_SCALE));
        }
    }

    /// The pressed element's scale this frame: 1 at rest.
    pub(crate) fn press_scale(&self) -> f32 {
        self.press.map_or(1.0, |p| p.pos as f32)
    }

    /// The plate on screen this frame, before its outset: stretched or squashed along its
    /// travel by where its edges are, the cross axis thinning or bulging to match, dipped by
    /// a press.
    pub(crate) fn rect(&self) -> Option<(Rect, f32)> {
        let e = self.edges.as_ref()?;
        let (w, h) = (self.size[0].pos, self.size[1].pos);
        let (l, r, rx) = shape(e[0].pos, e[2].pos, w, leader(self.axis.0));
        let (t, b, ry) = shape(e[1].pos, e[3].pos, h, leader(self.axis.1));
        let (qx, qy) = (cross(ry) * w / 2.0, cross(rx) * h / 2.0);
        let r = Rect::from_ltrb(
            (l - qx) as f32,
            (t - qy) as f32,
            (r + qx) as f32,
            (b + qy) as f32,
        );
        let s = self.press.map_or(1.0, |p| p.pos) as f32;
        let (dx, dy) = (r.width() * (1.0 - s) / 2.0, r.height() * (1.0 - s) / 2.0);
        Some((
            r.with_inset((dx, dy))
                .with_offset((-self.shift.0, -self.shift.1)),
            e[4].pos.max(0.0) as f32 * s,
        ))
    }

    /// A lifted glass plate with a brighter rim; the rim glows as the plate squashes on
    /// arrival. `k` scales the outset; `cheap` skips the blurred shadow.
    pub(crate) fn draw(&self, canvas: &Canvas, k: f32, cheap: bool) {
        let Some((r, corner)) = self.rect().filter(|_| self.visible()) else {
            return;
        };
        let alpha = self.shown as f32;
        let out = OUTSET * k;
        let rr = RRect::new_rect_xy(r.with_outset((out, out)), corner + out, corner + out);
        if !cheap {
            let mut shadow = fill(Color4f::new(
                0.0,
                0.0,
                0.0,
                alpha * crate::theme::shadow(0.4),
            ));
            shadow.set_mask_filter(MaskFilter::blur(BlurStyle::Normal, 12.0 * k, None));
            canvas.draw_rrect(rr.with_offset((0.0, 10.0 * k)), &shadow);
        }
        canvas.draw_rrect(rr, &fill(fg(0.12 * alpha)));
        canvas.draw_rrect(rr, &fill(accent(0.10 * alpha)));
        let squash = self.edges.as_ref().map_or(0.0, |e| {
            let rx = shape(e[0].pos, e[2].pos, self.size[0].pos, None).2;
            let ry = shape(e[1].pos, e[3].pos, self.size[1].pos, None).2;
            (1.0 - rx.min(ry)).max(0.0)
        });
        let pulse = (squash / SQUASH_MAX * 0.35).min(0.35) as f32;
        let rim = [fg((0.62 + pulse) * alpha), fg((0.14 + pulse / 2.0) * alpha)];
        let mut p = stroke(fg(1.0), 1.5 * k);
        p.set_shader(linear(rr.rect(), &rim, None));
        canvas.draw_rrect(rr, &p);
    }
}

/// The spring for one edge: `hi` is the right or bottom edge, `along` the hop's unit
/// direction on its axis. An axis the hop barely moves along keeps both edges together on
/// the leading edge's spring, so the front runs straight at the target, not along the row.
fn edge_spec(along: f64, hi: bool) -> SpringSpec {
    match leader(along) {
        None => LEAD,
        Some(lead) if lead == hi => LEAD,
        Some(_) => TRAIL,
    }
}

/// Which edge of an axis leads for the hop's unit direction `along` on it: `Some(true)`
/// the right or bottom one, `None` when the hop barely moves along the axis.
fn leader(along: f64) -> Option<bool> {
    (along.abs() >= AXIS_MIN).then_some(along > 0.0)
}

/// One axis: the edges' span against `size`, eased so a long throw saturates toward
/// [`STRETCH_MAX`] and [`SQUASH_MAX`] instead of tearing. The leading edge stays put and
/// the back gives; with no leader, the centre. The edges, then the span's ratio to `size`;
/// exactly the edges when they span `size`.
fn shape(lo: f64, hi: f64, size: f64, lead: Option<bool>) -> (f64, f64, f64) {
    let d = (hi - lo) / size.max(1.0) - 1.0;
    if d == 0.0 || size <= 1.0 {
        return (lo, hi, 1.0);
    }
    let cap = if d > 0.0 { STRETCH_MAX } else { SQUASH_MAX };
    let len = size * (1.0 + cap * (d / cap).tanh());
    let (lo, hi) = match lead {
        Some(true) => (hi - len, hi),
        Some(false) => (lo, lo + len),
        None => ((lo + hi - len) / 2.0, (lo + hi + len) / 2.0),
    };
    (lo, hi, len / size)
}

/// How far the cross axis grows, as a fraction of its size, for an axis at `ratio`:
/// thinner under a stretch, wider under a squash.
fn cross(ratio: f64) -> f64 {
    if ratio >= 1.0 {
        -(ratio - 1.0) * THIN
    } else {
        (1.0 - ratio) * BULGE
    }
}

/// Top-left to bottom-right across `r`.
fn linear(r: &Rect, colors: &[Color4f], pos: Option<&[f32]>) -> Option<skia_safe::Shader> {
    let (a, b) = (Point::new(r.left, r.top), Point::new(r.right, r.bottom));
    gradient::shaders::linear_gradient(
        (a, b),
        &gradient::Gradient::new(
            gradient::Colors::new(colors, pos, TileMode::Clamp, None),
            gradient::Interpolation::default(),
        ),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OK down dips the plate around its centre, and the spring brings it back whole.
    #[test]
    fn a_press_dips_the_plate_and_it_springs_back() {
        let mut p = Plate::default();
        let id = super::super::Id::new("t", 0);
        let r = Rect::from_xywh(0.0, 0.0, 100.0, 50.0);
        let frame = |p: &mut Plate| p.step(id, r, 8.0, 1.0 / 60.0, None, (0.0, 0.0));
        frame(&mut p);
        p.press();
        frame(&mut p);
        let (dipped, _) = p.rect().unwrap();
        assert!(dipped.width() < 100.0 && dipped.center_x() == 50.0);
        assert!(p.busy(), "a dipping plate keeps the frames coming");
        for _ in 0..120 {
            frame(&mut p);
        }
        assert_eq!(p.rect().unwrap().0, r);
        assert!(p.press.is_none());
    }

    /// Travelling right, as one motion: the front covers most of the hop while the back
    /// still trails, the plate stretches on the way and squashes on arrival, a long throw
    /// saturates inside the caps, and it rests on its target's rect exactly.
    #[test]
    fn the_plate_stretches_then_squashes_in_one_motion() {
        let (a, b) = (super::super::Id::new("t", 0), super::super::Id::new("t", 1));
        for dist in [120.0_f32, 1000.0] {
            let mut p = Plate::default();
            let from = Rect::from_xywh(0.0, 0.0, 100.0, 60.0);
            let to = from.with_offset((dist, 0.0));
            p.step(a, from, 8.0, 1.0 / 60.0, None, (0.0, 0.0));
            let (mut widest, mut narrowest, mut ahead) = (0.0f32, f32::MAX, false);
            for _ in 0..120 {
                p.step(b, to, 8.0, 1.0 / 60.0, None, (0.0, 0.0));
                let (r, _) = p.rect().unwrap();
                let w = r.width() / 100.0;
                widest = widest.max(w);
                if widest > 1.1 {
                    narrowest = narrowest.min(w);
                }
                ahead |= to.right - r.right < 0.1 * dist && to.left - r.left > 15.0;
            }
            assert!(ahead, "{dist}: the front arrives first");
            let (most, least) = (1.0 + STRETCH_MAX as f32, 1.0 - SQUASH_MAX as f32);
            assert!(widest > 1.2 && widest <= most + 0.01, "{dist}: {widest}");
            assert!(
                narrowest < 0.97 && narrowest >= least - 0.01,
                "{dist}: {narrowest}"
            );
            assert_eq!(p.rect().unwrap().0, to);
            assert!(!p.busy(), "{dist}: settled");
        }
    }

    /// A hop mostly along one axis runs straight: the plate's front covers the short axis at
    /// the long axis's pace, not after it. Main tabs down to Settings' sections is this hop.
    #[test]
    fn a_mostly_sideways_hop_runs_straight() {
        let (a, b) = (super::super::Id::new("t", 0), super::super::Id::new("t", 1));
        let mut p = Plate::default();
        let from = Rect::from_xywh(460.0, 40.0, 140.0, 50.0);
        let to = Rect::from_xywh(40.0, 120.0, 100.0, 40.0);
        p.step(a, from, 8.0, 1.0 / 60.0, None, (0.0, 0.0));
        for _ in 0..8 {
            p.step(b, to, 8.0, 1.0 / 60.0, None, (0.0, 0.0));
            let (r, _) = p.rect().unwrap();
            let x = (from.left - r.left) / (from.left - to.left);
            let y = (r.center_y() - from.center_y()) / (to.center_y() - from.center_y());
            assert!(
                (x - y).abs() < 0.1,
                "front off the line: x {x:.2}, y {y:.2}"
            );
        }
    }

    /// A plate seeded from another tree is arriving until it lands, and only then clips.
    #[test]
    fn a_handed_over_plate_arrives_then_lands() {
        let id = super::super::Id::new("t", 0);
        let mut p = Plate::default();
        p.seed(
            Rect::from_xywh(0.0, -80.0, 60.0, 30.0),
            8.0,
            None,
            (0.0, 0.0),
        );
        assert!(p.arriving());
        let to = Rect::from_xywh(0.0, 20.0, 200.0, 90.0);
        for _ in 0..3 {
            p.step(id, to, 12.0, 1.0 / 60.0, None, (0.0, 0.0));
        }
        assert!(p.arriving(), "still in flight");
        for _ in 0..120 {
            p.step(id, to, 12.0, 1.0 / 60.0, None, (0.0, 0.0));
        }
        assert!(!p.arriving());
        assert_eq!(p.rect().unwrap().0, to);
    }
}
