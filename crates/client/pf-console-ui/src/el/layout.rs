//! Layout, paint and hit-test for an [`El`] tree.

use super::focus::{score, Group, Plate};
use super::{Axis, El, Id, Kind, Painter, Virtual};
use crate::anim::{springs, Spring};
use crate::pointer::{Pointer, PointerKind};
use pf_client_core::menu_nav::MenuDir;
use skia_safe::{Canvas, Rect};
use std::collections::HashMap;
use taffy::{AvailableSpace, Dimension, NodeId, Size, TaffyTree};

/// A released fling loses 1/e of its speed every this many seconds.
const FLING_TAU: f32 = 0.4;
/// Slower than this, px/s, a fling has stopped.
const FLING_STOP: f32 = 20.0;
/// Past an end, a pan moves the content at most this fraction of the finger...
const RUBBER: f32 = 0.5;
/// ...halving again once the stretch reaches this fraction of the viewport.
const RUBBER_SPAN: f32 = 0.25;

/// What outlives a frame: scroll state, focus and the rects last painted, keyed by [`Id`].
pub struct Tree {
    taffy: TaffyTree,
    /// Virtual items, each laid out as its own root at its item size.
    items: TaffyTree,
    scrolls: HashMap<Id, Scroll>,
    placed: Vec<Placed>,
    /// Focus containers as last painted; [`Placed::group`] indexes here.
    groups: Vec<GroupBox>,
    focus: Option<Id>,
    /// [`Tree::set_focus`] named `focus` since the last paint: the caller holds it, so a
    /// paint that lacks it fades the plate instead of reseating.
    held: bool,
    /// Per group id, the child that last had focus.
    memory: HashMap<Id, Id>,
    plate: Plate,
}

#[derive(Clone, Copy, Default)]
struct Scroll {
    offset: f32,
    /// px/s: a fling, or a bounce back from past an end.
    vel: f32,
    /// A finger is panning it.
    held: bool,
    /// The end it is springing back to, until the spring settles there.
    bounce: Option<f32>,
    /// Furthest in-range offset and the viewport's length, as of the last layout.
    max: f32,
    view: f32,
}

impl Scroll {
    /// Signed distance past the nearest end; zero in range.
    fn excess(&self) -> f32 {
        self.offset - self.offset.clamp(0.0, self.max)
    }
}

struct Placed {
    id: Id,
    rect: Rect,
    /// `rect` inside every scroll viewport around it: what a pointer can reach.
    visible: Rect,
    /// Set on a scroll viewport.
    axis: Option<Axis>,
    /// Plate corner, px; set on a focus target.
    focus: Option<f32>,
    /// Innermost focus container around the node.
    group: Option<usize>,
}

#[derive(Clone, Copy)]
struct GroupBox {
    id: Option<Id>,
    kind: Group,
    parent: Option<usize>,
}

/// A plate that stopped being live this paint leaves its rect for another tree's plate;
/// one whose rect was taken hides.
fn hand_off(plate: &mut super::Plate, canvas: &Canvas, was: bool) {
    let id = plate.id();
    if was && !plate.live() {
        let m = canvas.local_to_device_as_3x3();
        if let Some((r, corner)) = plate.rect().filter(|_| m.is_scale_translate()) {
            super::focus::offer(id, m.map_rect(r).0, corner * m.scale_y());
        }
    }
    if super::focus::taken_from(id) {
        plate.hide();
    }
}

/// A plate waiting for focus to arrive from another tree starts where that tree's left.
fn take_over(plate: &mut super::Plate, canvas: &Canvas, space: Option<Id>, shift: (f32, f32)) {
    if !plate.waiting() {
        return;
    }
    let m = canvas.local_to_device_as_3x3();
    let Some(inv) = m.invert().filter(|_| m.is_scale_translate()) else {
        return;
    };
    let id = plate.id();
    if let Some((dev, corner)) = super::focus::take(id) {
        // Local to the canvas, then into the scroll's content: the plate draws content - shift.
        let local = inv.map_rect(dev).0.with_offset(shift);
        plate.seed(local, corner / m.scale_y(), space, shift);
    }
}

/// One laid-out tree: nodes in paint order at content-space rects, before scroll offsets.
pub struct Frame<'a> {
    nodes: Vec<Node<'a>>,
    scrolls: Vec<ScrollBox>,
    groups: Vec<GroupBox>,
}

struct Node<'a> {
    id: Option<Id>,
    /// Set on a scroll viewport.
    axis: Option<Axis>,
    rect: Rect,
    /// Innermost scroll around the node, an index into [`Frame::scrolls`].
    scroll: Option<usize>,
    paint: Option<Painter<'a>>,
    focus: Option<f32>,
    group: Option<usize>,
}

struct ScrollBox {
    id: Id,
    axis: Axis,
    viewport: Rect,
    /// Furthest offset that still shows content.
    max: f32,
    parent: Option<usize>,
}

impl Default for Tree {
    fn default() -> Tree {
        Tree::new()
    }
}

impl Tree {
    pub fn new() -> Tree {
        let mut taffy = TaffyTree::new();
        let mut items = TaffyTree::new();
        // Painters take fractional rects today; rounding would move a row by up to half a pixel.
        taffy.disable_rounding();
        items.disable_rounding();
        Tree {
            taffy,
            items,
            scrolls: HashMap::new(),
            placed: Vec::new(),
            groups: Vec::new(),
            focus: None,
            held: false,
            memory: HashMap::new(),
            plate: Plate::default(),
        }
    }

    /// Lay `root` out to fill `rect`. A resting offset clamps to this frame's content; a
    /// scroll absent from it forgets its state.
    pub fn layout<'a>(&mut self, mut root: El<'a>, rect: Rect) -> Frame<'a> {
        self.taffy.clear();
        root.style.size = definite(rect.width(), rect.height());
        let node = build(&mut self.taffy, &mut root);
        self.taffy
            .compute_layout(node, available(rect.width(), rect.height()))
            .expect("layout on a fresh tree");
        let mut frame = Frame {
            nodes: Vec::new(),
            scrolls: Vec::new(),
            groups: Vec::new(),
        };
        let mut walk = Walk {
            tree: &self.taffy,
            items: Some(&mut self.items),
            scrolls: &mut self.scrolls,
            frame: &mut frame,
        };
        walk.node(root, node, (rect.left, rect.top), None, None);
        self.scrolls
            .retain(|id, _| frame.scrolls.iter().any(|s| s.id == *id));
        frame
    }

    /// Paint `frame` in tree order. Scrolled nodes shift and clip to their viewports;
    /// a node wholly outside them is skipped.
    pub fn paint(&mut self, canvas: &Canvas, frame: Frame<'_>) {
        self.paint_inner(canvas, frame, None);
    }

    /// [`Self::paint`], with the focus plate advanced `dt` seconds toward this frame's
    /// rect of the focused node and drawn behind its whole focus group; with no focused
    /// node it glides on and fades. `k` scales the plate's outset; `cheap` drops its
    /// blurred shadow.
    pub fn paint_focus(&mut self, canvas: &Canvas, frame: Frame<'_>, k: f32, dt: f64, cheap: bool) {
        self.paint_inner(canvas, frame, Some((k, dt, cheap)));
    }

    /// A focus the caller did not hold and this frame lacks reseats on the target nearest
    /// where it stood. Every paint reports its targets to the running [`super::census`].
    fn paint_inner(&mut self, canvas: &Canvas, frame: Frame<'_>, plate: Option<(f32, f64, bool)>) {
        let last = self.focus.and_then(|f| self.rect(f));
        let anchor = last.or_else(|| self.plate.rect().map(|p| p.0));
        self.placed.clear();
        self.groups = frame.groups;
        // Per scroll: the summed offset of it and its ancestors, and its on-screen clip.
        let mut shift: Vec<(f32, f32)> = Vec::with_capacity(frame.scrolls.len());
        let mut clip: Vec<Rect> = Vec::with_capacity(frame.scrolls.len());
        for s in &frame.scrolls {
            let (px, py) = s.parent.map_or((0.0, 0.0), |p| shift[p]);
            let mut view = s.viewport.with_offset((-px, -py));
            if let Some(p) = s.parent {
                if !view.intersect(clip[p]) {
                    view = Rect::new_empty();
                }
            }
            let off = self.offset(s.id);
            shift.push(match s.axis {
                Axis::Vertical => (px, py + off),
                Axis::Horizontal => (px + off, py),
            });
            clip.push(view);
        }
        if !std::mem::take(&mut self.held) {
            self.reseat(&frame.nodes, &shift, anchor);
        }
        // The plate goes under the first node of the focused node's group, so it never
        // covers a neighbour.
        let at = plate.and_then(|look| {
            let f = frame
                .nodes
                .iter()
                .position(|n| n.id.is_some() && n.id == self.focus && n.focus.is_some())?;
            let n = &frame.nodes[f];
            let at = frame.nodes.iter().position(|m| m.group == n.group)?;
            let (d, c) = n
                .scroll
                .map_or(((0.0, 0.0), None), |i| (shift[i], Some(clip[i])));
            let space = n.scroll.map(|i| frame.scrolls[i].id);
            Some((at, look, n.id?, n.rect, n.focus?, space, d, c))
        });
        // Nothing focused to rest on: the plate fades under everything, in its scroll.
        if let (None, Some((k, dt, cheap))) = (at, plate) {
            let space = self.plate.space();
            let i = frame.scrolls.iter().position(|s| Some(s.id) == space);
            canvas.save();
            if let Some(i) = i {
                canvas.clip_rect(clip[i], None, true);
            }
            let was = self.plate.live();
            self.plate.lose(dt, i.map(|i| shift[i]));
            hand_off(&mut self.plate, canvas, was);
            self.plate.draw(canvas, k, cheap);
            canvas.restore();
        }
        for (i, n) in frame.nodes.into_iter().enumerate() {
            if let Some((_, (k, dt, cheap), id, target, corner, space, d, c)) =
                at.filter(|p| p.0 == i)
            {
                canvas.save();
                let was = self.plate.live();
                take_over(&mut self.plate, canvas, space, d);
                self.plate.step(id, target, corner, dt, space, d);
                hand_off(&mut self.plate, canvas, was);
                // A plate still coming in from outside its viewport would be cut at its edge.
                if let Some(c) = c.filter(|_| !self.plate.arriving()) {
                    canvas.clip_rect(c, None, true);
                }
                self.plate.draw(canvas, k, cheap);
                canvas.restore();
            }
            let (d, c) = n
                .scroll
                .map_or(((0.0, 0.0), None), |i| (shift[i], Some(clip[i])));
            let rect = n.rect.with_offset((-d.0, -d.1));
            let mut visible = rect;
            let shown = c.is_none_or(|c| visible.intersect(c));
            // A target out of view still answers a direction; it only stops taking a pointer.
            if !shown && n.focus.is_none() {
                continue;
            }
            if !shown {
                visible = Rect::new_empty();
            }
            if let Some(id) = n.id {
                self.placed.push(Placed {
                    id,
                    rect,
                    visible,
                    axis: n.axis,
                    focus: n.focus,
                    group: n.group,
                });
            }
            let Some(p) = n.paint.filter(|_| shown) else {
                continue;
            };
            canvas.save();
            if let Some(c) = c {
                canvas.clip_rect(c, None, true);
            }
            // A press dips the focused element with its plate.
            let dip = self.plate.press_scale();
            if n.id == self.focus && n.focus.is_some() && dip < 1.0 {
                let (cx, cy) = (rect.center_x(), rect.center_y());
                canvas.translate((cx, cy));
                canvas.scale((dip, dip));
                canvas.translate((-cx, -cy));
            }
            p(canvas, rect);
            canvas.restore();
        }
        super::claim(self.placed.iter().filter(|p| p.focus.is_some()).count());
        self.remember();
    }

    /// Move a focus this frame's `nodes` lack to the target nearest `anchor`, on screen.
    /// With no target in the frame it stays, to reseat once one is back.
    fn reseat(&mut self, nodes: &[Node<'_>], shift: &[(f32, f32)], anchor: Option<Rect>) {
        let (Some(f), Some(a)) = (self.focus, anchor) else {
            return;
        };
        let targets = nodes.iter().filter(|n| n.focus.is_some());
        if targets.clone().any(|n| n.id == Some(f)) {
            return;
        }
        let far = |n: &Node<'_>| {
            let r = n
                .scroll
                .map_or(n.rect, |i| n.rect.with_offset((-shift[i].0, -shift[i].1)));
            (r.center_x() - a.center_x()).hypot(r.center_y() - a.center_y())
        };
        if let Some(n) = targets.min_by(|x, y| far(x).total_cmp(&far(y))) {
            self.focus = n.id;
        }
    }

    pub fn focus(&self) -> Option<Id> {
        self.focus
    }

    /// OK went down on the focused node: its plate dips.
    pub fn press(&mut self) {
        self.plate.press();
    }

    /// The caller's focus, held through the next paint even when that paint lacks it.
    /// `None` is no focus here: the plate fades out.
    pub fn set_focus(&mut self, id: Option<Id>) {
        self.focus = id;
        self.held = true;
        self.remember();
    }

    /// The plate is still moving: keep drawing frames.
    pub fn plate_busy(&self) -> bool {
        self.plate.busy()
    }

    /// Where the plate is this frame, before its outset, and its corner radius. `None`
    /// once it has faded out.
    pub fn plate_rect(&self) -> Option<(Rect, f32)> {
        self.plate.rect().filter(|_| self.plate.visible())
    }

    /// Move focus from the focused target one step `dir`, by last frame's rects. `None`
    /// leaves focus alone: nothing is that way, or nothing was painted focused.
    pub fn move_focus(&mut self, dir: MenuDir) -> Option<Id> {
        let from = self
            .placed
            .iter()
            .find(|p| Some(p.id) == self.focus && p.focus.is_some())?;
        let targets = || {
            self.placed
                .iter()
                .filter(move |p| p.focus.is_some() && p.id != from.id)
        };
        let nearest = |cands: &mut dyn Iterator<Item = &Placed>| {
            cands
                .filter_map(|p| score(from.rect, p.rect, dir).map(|s| (s, p.id)))
                .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(_, id)| id)
        };
        let mut best = None;
        let mut g = from.group;
        while let Some(gi) = g {
            if self.groups[gi].kind.holds(dir) {
                best = nearest(&mut targets().filter(|p| self.inside(p.group, gi)));
                if best.is_some() {
                    break;
                }
            }
            g = self.groups[gi].parent;
        }
        let to = best.or_else(|| nearest(&mut targets()))?;
        let to = self.recall(from.group, to);
        self.set_focus(Some(to));
        Some(to)
    }

    /// Entering groups from outside lands on the child the outermost of them last had.
    fn recall(&self, from: Option<usize>, to: Id) -> Id {
        let Some(target) = self.placed.iter().find(|p| p.id == to) else {
            return to;
        };
        let mut out = to;
        let mut g = target.group;
        while let Some(gi) = g {
            if !self.inside(from, gi) {
                let back = self.groups[gi]
                    .id
                    .and_then(|gid| self.memory.get(&gid))
                    .filter(|m| self.placed.iter().any(|p| p.id == **m && p.focus.is_some()));
                if let Some(back) = back {
                    out = *back;
                }
            }
            g = self.groups[gi].parent;
        }
        out
    }

    /// `group` is `outer` or nested in it.
    fn inside(&self, mut group: Option<usize>, outer: usize) -> bool {
        while let Some(g) = group {
            if g == outer {
                return true;
            }
            group = self.groups[g].parent;
        }
        false
    }

    fn remember(&mut self) {
        let Some(p) = self.placed.iter().find(|p| Some(p.id) == self.focus) else {
            return;
        };
        let (id, mut g) = (p.id, p.group);
        while let Some(gi) = g {
            if let Some(gid) = self.groups[gi].id {
                self.memory.insert(gid, id);
            }
            g = self.groups[gi].parent;
        }
    }

    /// The topmost node painted last frame under `(x, y)`. Half-open, like `Pointer::hits`.
    pub fn hit(&self, x: f32, y: f32) -> Option<Id> {
        self.placed
            .iter()
            .rev()
            .find(|p| {
                let v = p.visible;
                x >= v.left && x < v.right && y >= v.top && y < v.bottom
            })
            .map(|p| p.id)
    }

    /// Where `id` was painted last frame, scroll offsets applied.
    pub fn rect(&self, id: Id) -> Option<Rect> {
        self.placed.iter().find(|p| p.id == id).map(|p| p.rect)
    }

    /// The innermost scroll on `axis` painted under `(x, y)` last frame.
    pub fn scroll_at(&self, x: f32, y: f32, axis: Axis) -> Option<Id> {
        self.placed
            .iter()
            .rev()
            .filter(|p| p.axis == Some(axis))
            .find(|p| {
                let v = p.visible;
                x >= v.left && x < v.right && y >= v.top && y < v.bottom
            })
            .map(|p| p.id)
    }

    pub fn offset(&self, scroll: Id) -> f32 {
        self.scrolls.get(&scroll).map_or(0.0, |s| s.offset)
    }

    /// Jump there and stop; clamped to the content on the next [`Self::layout`].
    pub fn set_offset(&mut self, scroll: Id, offset: f32) {
        let s = self.scrolls.entry(scroll).or_default();
        s.offset = offset;
        s.vel = 0.0;
        s.bounce = None;
    }

    /// A finger moved the offset by `delta`. Past an end the content lags the finger,
    /// more the further it is stretched.
    pub fn pan(&mut self, scroll: Id, delta: f32) {
        let s = self.scrolls.entry(scroll).or_default();
        s.held = true;
        s.vel = 0.0;
        s.bounce = None;
        let excess = s.excess();
        s.offset += if excess * delta > 0.0 {
            delta * RUBBER / (1.0 + excess.abs() / (RUBBER_SPAN * s.view.max(1.0)))
        } else {
            delta
        };
    }

    /// A finger drag on the vertical scroll `scroll`: taken at `PanStart` when its anchor
    /// is on it, its steps then move the offset and its lift flings it.
    pub fn drag(&mut self, scroll: Id, p: Pointer) -> bool {
        match p.kind {
            PointerKind::PanStart { horizontal: false } => {
                let on = self.scroll_at(p.x as f32, p.y as f32, Axis::Vertical) == Some(scroll);
                if on {
                    self.pan(scroll, 0.0);
                }
                on
            }
            PointerKind::Pan { dy, .. } => {
                self.pan(scroll, -dy as f32);
                true
            }
            PointerKind::Fling { vy, .. } => {
                self.release(scroll, -vy as f32);
                true
            }
            _ => false,
        }
    }

    /// The finger lifted with the offset moving at `vel` px/s.
    pub fn release(&mut self, scroll: Id, vel: f32) {
        let s = self.scrolls.entry(scroll).or_default();
        s.held = false;
        s.vel = vel;
    }

    /// Advance flings and bounces by `dt` seconds.
    pub fn tick(&mut self, dt: f32) {
        for s in self.scrolls.values_mut().filter(|s| !s.held) {
            // Past an end: spring back to it, the fling's speed carried into the bounce.
            // The spring owns the offset until it settles, swings into range included.
            if s.bounce.is_none() && s.excess() != 0.0 {
                s.bounce = Some(s.offset - s.excess());
            }
            if let Some(end) = s.bounce {
                let mut sp = Spring {
                    pos: f64::from(s.offset - end),
                    vel: f64::from(s.vel),
                };
                sp.step_spec(0.0, springs::FOCUS, f64::from(dt));
                sp.settle(0.0, 0.5, 5.0);
                s.offset = end + sp.pos as f32;
                s.vel = sp.vel as f32;
                if sp.pos == 0.0 && sp.vel == 0.0 {
                    s.bounce = None;
                }
            } else if s.vel != 0.0 {
                s.offset += s.vel * dt;
                s.vel *= (-dt / FLING_TAU).exp();
                if s.vel.abs() < FLING_STOP {
                    s.vel = 0.0;
                }
            }
        }
    }

    /// Held, flinging or bouncing: not the moment for a widget to move it.
    pub fn moving(&self, scroll: Id) -> bool {
        self.scrolls
            .get(&scroll)
            .is_some_and(|s| s.held || s.vel != 0.0 || s.bounce.is_some() || s.excess() != 0.0)
    }
}

impl Frame<'_> {
    /// `id`'s rect this frame, before scroll offsets.
    pub fn rect(&self, id: Id) -> Option<Rect> {
        self.nodes.iter().find(|n| n.id == Some(id)).map(|n| n.rect)
    }

    /// A scroll's viewport and its furthest offset.
    pub fn scroll(&self, id: Id) -> Option<(Rect, f32)> {
        self.scrolls
            .iter()
            .find(|s| s.id == id)
            .map(|s| (s.viewport, s.max))
    }
}

/// Styles move into taffy; the painters stay on the `El` for the walk.
fn build(taffy: &mut TaffyTree, el: &mut El<'_>) -> NodeId {
    let kids: Vec<NodeId> = el.children.iter_mut().map(|c| build(taffy, c)).collect();
    taffy
        .new_with_children(std::mem::take(&mut el.style), &kids)
        .expect("taffy node")
}

struct Walk<'t, 'a> {
    tree: &'t TaffyTree,
    /// `None` inside a virtual item: its tree is the one being walked.
    items: Option<&'t mut TaffyTree>,
    scrolls: &'t mut HashMap<Id, Scroll>,
    frame: &'t mut Frame<'a>,
}

impl<'a> Walk<'_, 'a> {
    fn node(
        &mut self,
        el: El<'a>,
        node: NodeId,
        origin: (f32, f32),
        scroll: Option<usize>,
        group: Option<usize>,
    ) {
        let l = *self.tree.layout(node).expect("laid-out node");
        let rect = Rect::from_xywh(
            origin.0 + l.location.x,
            origin.1 + l.location.y,
            l.size.width,
            l.size.height,
        );
        let El {
            id,
            kind,
            children,
            focus,
            group: own_group,
            ..
        } = el;
        debug_assert!(
            focus.is_none() || id.is_some(),
            "a focus target needs an id"
        );
        let inner_group = match own_group {
            Some(kind) => {
                self.frame.groups.push(GroupBox {
                    id,
                    kind,
                    parent: group,
                });
                Some(self.frame.groups.len() - 1)
            }
            None => group,
        };
        let mut inner = scroll;
        let mut items = None;
        let mut scroll_axis = None;
        let paint = match kind {
            Kind::Box => None,
            Kind::Paint(p) => Some(p),
            Kind::Scroll(axis) => {
                let id = id.expect("El::scroll sets an id");
                let o = l.scrollable_overflow_rect;
                let max = match axis {
                    Axis::Vertical => o.bottom - l.size.height,
                    Axis::Horizontal => o.right - l.size.width,
                }
                .max(0.0);
                let s = self.scrolls.entry(id).or_default();
                s.max = max;
                s.view = match axis {
                    Axis::Vertical => l.size.height,
                    Axis::Horizontal => l.size.width,
                };
                if !s.held && s.vel == 0.0 && s.bounce.is_none() {
                    s.offset = s.offset.clamp(0.0, max);
                }
                scroll_axis = Some(axis);
                self.frame.scrolls.push(ScrollBox {
                    id,
                    axis,
                    viewport: rect,
                    max,
                    parent: scroll,
                });
                inner = Some(self.frame.scrolls.len() - 1);
                None
            }
            Kind::Virtual(v) => {
                items = Some(v);
                None
            }
        };
        if id.is_some() || paint.is_some() {
            self.frame.nodes.push(Node {
                id,
                axis: scroll_axis,
                rect,
                scroll,
                paint,
                focus,
                group,
            });
        }
        if let Some(v) = items {
            self.virtual_items(v, rect, scroll, inner_group);
        }
        let kids = self.tree.children(node).expect("laid-out node");
        for (child, n) in children.into_iter().zip(kids) {
            self.node(child, n, (rect.left, rect.top), inner, inner_group);
        }
    }

    /// Build and lay out the items of `v` inside the scroll viewport, half a viewport of
    /// overscan each side: the offset may still move this frame.
    fn virtual_items(
        &mut self,
        v: Virtual<'a>,
        rect: Rect,
        scroll: Option<usize>,
        group: Option<usize>,
    ) {
        // One items tree, so a virtual item cannot hold another Virtual.
        let Some(items) = self.items.as_deref_mut() else {
            debug_assert!(false, "a virtual item holds a Virtual");
            return;
        };
        let pitch = v.extent + v.gap;
        if v.count == 0 || pitch <= 0.0 {
            return;
        }
        let view = match scroll.map(|i| &self.frame.scrolls[i]) {
            Some(s) => {
                let off = self.scrolls.get(&s.id).map_or(0.0, |s| s.offset);
                match s.axis {
                    Axis::Vertical => s.viewport.with_offset((0.0, off)),
                    Axis::Horizontal => s.viewport.with_offset((off, 0.0)),
                }
            }
            None => rect,
        };
        let (start, lo, hi) = match v.axis {
            Axis::Vertical => (rect.top, view.top, view.bottom),
            Axis::Horizontal => (rect.left, view.left, view.right),
        };
        let overscan = (hi - lo) / 2.0;
        let first = ((lo - overscan - start) / pitch).floor().max(0.0) as usize;
        let end = (((hi + overscan - start) / pitch).ceil().max(0.0) as usize).min(v.count);
        for i in first..end {
            let mut el = (v.build)(i);
            let at = start + i as f32 * pitch;
            let ((x, y), (w, h)) = match v.axis {
                Axis::Vertical => ((rect.left, at), (rect.width(), v.extent)),
                Axis::Horizontal => ((at, rect.top), (v.extent, rect.height())),
            };
            el.style.size = definite(w, h);
            items.clear();
            let n = build(items, &mut el);
            items
                .compute_layout(n, available(w, h))
                .expect("layout on a fresh tree");
            Walk {
                tree: items,
                items: None,
                scrolls: &mut *self.scrolls,
                frame: &mut *self.frame,
            }
            .node(el, n, (x, y), scroll, group);
        }
    }
}

fn definite(w: f32, h: f32) -> Size<Dimension> {
    Size {
        width: Dimension::length(w),
        height: Dimension::length(h),
    }
}

fn available(w: f32, h: f32) -> Size<AvailableSpace> {
    Size {
        width: AvailableSpace::Definite(w),
        height: AvailableSpace::Definite(h),
    }
}
