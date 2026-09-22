//! Layout, paint and hit-test for an [`El`] tree.

use super::{Axis, El, Id, Kind, Painter, Virtual};
use skia_safe::{Canvas, Rect};
use std::collections::HashMap;
use taffy::{AvailableSpace, Dimension, NodeId, Size, TaffyTree};

/// What outlives a frame: scroll offsets and the rects last painted, keyed by [`Id`].
pub struct Tree {
    taffy: TaffyTree,
    /// Virtual items, each laid out as its own root at its item size.
    items: TaffyTree,
    offsets: HashMap<Id, f32>,
    placed: Vec<Placed>,
}

struct Placed {
    id: Id,
    rect: Rect,
    /// `rect` inside every scroll viewport around it: what a pointer can reach.
    visible: Rect,
}

/// One laid-out tree: nodes in paint order at content-space rects, before scroll offsets.
pub struct Frame<'a> {
    nodes: Vec<Node<'a>>,
    scrolls: Vec<ScrollBox>,
}

struct Node<'a> {
    id: Option<Id>,
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
            offsets: HashMap::new(),
            placed: Vec::new(),
        }
    }

    /// Lay `root` out to fill `rect`. Offsets clamp to this frame's content; a scroll
    /// absent from it forgets its offset.
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
            offsets: &mut self.offsets,
            frame: &mut frame,
        };
        walk.node(root, node, (rect.left, rect.top), None);
        self.offsets
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
                self.placed.push(Placed { id, rect, visible });
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

    pub fn offset(&self, scroll: Id) -> f32 {
        self.offsets.get(&scroll).copied().unwrap_or(0.0)
    }

    /// Clamped to the content on the next [`Self::layout`].
    pub fn set_offset(&mut self, scroll: Id, offset: f32) {
        self.offsets.insert(scroll, offset);
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
    offsets: &'t mut HashMap<Id, f32>,
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
                let off = self.offsets.entry(id).or_insert(0.0);
                *off = off.clamp(0.0, max);
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
                let off = self.offsets.get(&s.id).copied().unwrap_or(0.0);
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
                offsets: &mut *self.offsets,
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
