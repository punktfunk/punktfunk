//! Layout, paint and hit-test for an [`El`] tree.

use super::{Axis, El, Id, Kind, Painter, Virtual};
use crate::anim::{springs, Spring};
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

/// What outlives a frame: scroll state and the rects last painted, keyed by [`Id`].
pub struct Tree {
    taffy: TaffyTree,
    /// Virtual items, each laid out as its own root at its item size.
    items: TaffyTree,
    scrolls: HashMap<Id, Scroll>,
    placed: Vec<Placed>,
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
}

/// One laid-out tree: nodes in paint order at content-space rects, before scroll offsets.
pub struct Frame<'a> {
    nodes: Vec<Node<'a>>,
    scrolls: Vec<ScrollBox>,
}

struct Node<'a> {
    id: Option<Id>,
    /// Set on a scroll viewport.
    axis: Option<Axis>,
    rect: Rect,
    /// Innermost scroll around the node, an index into [`Frame::scrolls`].
    scroll: Option<usize>,
    paint: Option<Painter<'a>>,
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
        };
        let mut walk = Walk {
            tree: &self.taffy,
            items: Some(&mut self.items),
            scrolls: &mut self.scrolls,
            frame: &mut frame,
        };
        walk.node(root, node, (rect.left, rect.top), None);
        self.scrolls
            .retain(|id, _| frame.scrolls.iter().any(|s| s.id == *id));
        frame
    }

    /// Paint `frame` in tree order. Scrolled nodes shift and clip to their viewports;
    /// a node wholly outside them is skipped.
    pub fn paint(&mut self, canvas: &Canvas, frame: Frame<'_>) {
        self.placed.clear();
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
        for n in frame.nodes {
            let (d, c) = n
                .scroll
                .map_or(((0.0, 0.0), None), |i| (shift[i], Some(clip[i])));
            let rect = n.rect.with_offset((-d.0, -d.1));
            let mut visible = rect;
            if c.is_some_and(|c| !visible.intersect(c)) {
                continue;
            }
            if let Some(id) = n.id {
                self.placed.push(Placed {
                    id,
                    rect,
                    visible,
                    axis: n.axis,
                });
            }
            if let Some(p) = n.paint {
                canvas.save();
                if let Some(c) = c {
                    canvas.clip_rect(c, None, true);
                }
                p(canvas, rect);
                canvas.restore();
            }
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
    fn node(&mut self, el: El<'a>, node: NodeId, origin: (f32, f32), scroll: Option<usize>) {
        let l = *self.tree.layout(node).expect("laid-out node");
        let rect = Rect::from_xywh(
            origin.0 + l.location.x,
            origin.1 + l.location.y,
            l.size.width,
            l.size.height,
        );
        let El {
            id, kind, children, ..
        } = el;
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
            });
        }
        if let Some(v) = items {
            self.virtual_items(v, rect, scroll);
        }
        let kids = self.tree.children(node).expect("laid-out node");
        for (child, n) in children.into_iter().zip(kids) {
            self.node(child, n, (rect.left, rect.top), inner);
        }
    }

    /// Build and lay out the items of `v` inside the scroll viewport, half a viewport of
    /// overscan each side: the offset may still move this frame.
    fn virtual_items(&mut self, v: Virtual<'a>, rect: Rect, scroll: Option<usize>) {
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
            .node(el, n, (x, y), scroll);
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
