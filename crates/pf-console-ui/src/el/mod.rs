//! Element layer: a tree a surface builds every frame, laid out by taffy, painted in tree
//! order and hit-tested from the rects it painted (`console-ui-element-layer.md` §4).
//!
//! Screen state stays plain data; `view()` rebuilds the tree each frame, so nothing is
//! diffed. [`Tree`] keeps only what outlives a frame, keyed by [`Id`]: scroll offsets and
//! the last painted rects. Layout is in device pixels; hand-drawn content is a
//! [`Kind::Paint`] node that receives its rect, exactly what today's painters take.
//!
//! A [`Kind::Scroll`] clips and offsets its children. A long list goes under it as a
//! [`Kind::Virtual`], which builds only the items in view: a 2000-title grid laid out in
//! full every frame is the one way to make this slow.
//!
//! Focus is the tree's too: a node marked [`El::focusable`] is a target, a [`Group`] is a
//! container, and [`Tree::move_focus`] answers a direction from last frame's rects.
//! [`census`] tells the shell whether a surface has any target at all.

mod focus;
mod layout;

pub(crate) use focus::forget_handoff;
#[cfg(test)]
pub(crate) use focus::DRAWN;
pub use focus::{begin_frame, Group, Plate};
pub use layout::{Frame, Tree};

thread_local! {
    /// Focus is on the shell's tab strip, not in the layer painting now: its trees step
    /// their plates without drawing them, so one plate shows. Set by the shell per layer.
    static DORMANT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Focus targets placed inside the running [`census`]; `None` outside one.
    static CENSUS: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

pub fn set_dormant(on: bool) {
    DORMANT.with(|d| d.set(on));
}

fn dormant() -> bool {
    DORMANT.with(std::cell::Cell::get)
}

/// Runs `paint` and answers how many focus targets it placed. A tree counts its own;
/// focus drawn outside a tree [`claim`]s it. Zero is a surface focus cannot enter.
pub fn census(paint: impl FnOnce()) -> usize {
    let outer = CENSUS.with(|c| c.replace(Some(0)));
    paint();
    let n = CENSUS.with(|c| c.replace(outer)).unwrap_or(0);
    claim(n);
    n
}

/// `n` more focus targets for the running [`census`]; nothing outside one.
pub fn claim(n: usize) {
    CENSUS.with(|c| c.set(c.get().map(|m| m + n)));
}
use skia_safe::{Canvas, Rect};
pub use taffy::Style;
use taffy::{Dimension, FlexDirection, LengthPercentage, Overflow, Point, Size};

/// A node's identity across frames: a name and an index, hashed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Id(u64);

impl Id {
    pub fn new(name: &str, index: usize) -> Id {
        use std::hash::{Hash, Hasher};
        // `DefaultHasher::new` has fixed keys, so an id is stable across frames and runs.
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (name, index).hash(&mut h);
        Id(h.finish())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Vertical,
    Horizontal,
}

/// Draws a node at its laid-out rect, device pixels.
pub type Painter<'a> = Box<dyn Fn(&Canvas, Rect) + 'a>;

pub struct El<'a> {
    pub id: Option<Id>,
    pub style: Style,
    pub kind: Kind<'a>,
    pub children: Vec<El<'a>>,
    /// A focus target; the plate behind it rounds its corners by this many px.
    pub focus: Option<f32>,
    pub group: Option<Group>,
}

pub enum Kind<'a> {
    /// A flex container; its style sets direction, wrap and gap.
    Box,
    /// Hand-drawn content: carousel cards, glyph tables, a row's chrome.
    Paint(Painter<'a>),
    /// Clips its children and shifts them by the offset [`Tree`] keeps under the node's id.
    Scroll(Axis),
    /// Equal items along a scroll axis, built only while in view.
    Virtual(Virtual<'a>),
}

pub struct Virtual<'a> {
    pub axis: Axis,
    pub count: usize,
    /// Item length along `axis`, px. The cross size is the node's.
    pub extent: f32,
    pub gap: f32,
    pub build: Box<dyn Fn(usize) -> El<'a> + 'a>,
}

impl<'a> El<'a> {
    fn new(kind: Kind<'a>, style: Style) -> El<'a> {
        El {
            id: None,
            style,
            kind,
            children: Vec::new(),
            focus: None,
            group: None,
        }
    }

    pub fn column() -> El<'a> {
        El::new(Kind::Box, flex(FlexDirection::Column))
    }

    pub fn row() -> El<'a> {
        El::new(Kind::Box, flex(FlexDirection::Row))
    }

    pub fn paint(f: impl Fn(&Canvas, Rect) + 'a) -> El<'a> {
        El::new(Kind::Paint(Box::new(f)), Style::default())
    }

    /// A scroll viewport. It lays its children out along `axis` and needs a definite size
    /// from its parent; content past that size scrolls instead of growing the node.
    pub fn scroll(id: Id, axis: Axis) -> El<'a> {
        let (dir, overflow) = match axis {
            Axis::Vertical => (
                FlexDirection::Column,
                Point {
                    x: Overflow::Visible,
                    y: Overflow::Scroll,
                },
            ),
            Axis::Horizontal => (
                FlexDirection::Row,
                Point {
                    x: Overflow::Scroll,
                    y: Overflow::Visible,
                },
            ),
        };
        let mut style = flex(dir);
        style.overflow = overflow;
        style.scrollbar_width = 0.0;
        El::new(Kind::Scroll(axis), style).id(id)
    }

    /// `count` items of `extent` px along `axis`, `gap` apart. `build(i)` makes item `i`.
    pub fn virtual_list(
        axis: Axis,
        count: usize,
        extent: f32,
        gap: f32,
        build: impl Fn(usize) -> El<'a> + 'a,
    ) -> El<'a> {
        let length = if count == 0 {
            0.0
        } else {
            count as f32 * (extent + gap) - gap
        };
        let mut style = Style::default();
        match axis {
            Axis::Vertical => style.size.height = Dimension::length(length),
            Axis::Horizontal => style.size.width = Dimension::length(length),
        }
        style.flex_shrink = 0.0;
        let v = Virtual {
            axis,
            count,
            extent,
            gap,
            build: Box::new(build),
        };
        El::new(Kind::Virtual(v), style)
    }

    pub fn id(mut self, id: Id) -> El<'a> {
        self.id = Some(id);
        self
    }

    /// A focus target. Needs an id; `corner` rounds the plate, px.
    pub fn focusable(mut self, corner: f32) -> El<'a> {
        self.focus = Some(corner);
        self
    }

    /// A focus container. With an id, focus entering it returns to the child it left.
    pub fn group(mut self, group: Group) -> El<'a> {
        self.group = Some(group);
        self
    }

    /// Out of the flow, at `r` relative to the parent's top-left.
    pub fn place(self, r: Rect) -> El<'a> {
        self.style(|s| {
            s.position = taffy::Position::Absolute;
            s.inset = taffy::Rect {
                left: taffy::LengthPercentageAuto::length(r.left),
                top: taffy::LengthPercentageAuto::length(r.top),
                right: taffy::LengthPercentageAuto::auto(),
                bottom: taffy::LengthPercentageAuto::auto(),
            };
        })
        .size(r.width(), r.height())
    }

    /// Any style the named builders do not cover.
    pub fn style(mut self, f: impl FnOnce(&mut Style)) -> El<'a> {
        f(&mut self.style);
        self
    }

    pub fn size(self, width: f32, height: f32) -> El<'a> {
        self.style(|s| {
            s.size = Size {
                width: Dimension::length(width),
                height: Dimension::length(height),
            };
            s.flex_shrink = 0.0;
        })
    }

    pub fn height(self, height: f32) -> El<'a> {
        self.style(|s| {
            s.size.height = Dimension::length(height);
            s.flex_shrink = 0.0;
        })
    }

    pub fn width(self, width: f32) -> El<'a> {
        self.style(|s| {
            s.size.width = Dimension::length(width);
            s.flex_shrink = 0.0;
        })
    }

    /// Space between children on both axes.
    pub fn gap(self, gap: f32) -> El<'a> {
        self.style(|s| {
            s.gap = Size {
                width: LengthPercentage::length(gap),
                height: LengthPercentage::length(gap),
            };
        })
    }

    pub fn child(mut self, child: El<'a>) -> El<'a> {
        self.children.push(child);
        self
    }

    pub fn children(mut self, children: impl IntoIterator<Item = El<'a>>) -> El<'a> {
        self.children.extend(children);
        self
    }
}

fn flex(direction: FlexDirection) -> Style {
    Style {
        flex_direction: direction,
        ..Style::default()
    }
}

#[cfg(test)]
mod tests;
