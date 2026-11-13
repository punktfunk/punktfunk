//! The sort and view pills: the sorts on the leading edge behind an arrows mark, the views
//! on the trailing behind an eye, and Search last as a magnifier: it opens a screen rather
//! than applying a state.
//!
//! A line of focus targets in the screen's own tree, so the plate walks onto it from the
//! line below: Left and Right move between pills and OK applies the focused one. Each
//! group's accent capsule slides to the pill it applies. The settings are the state, so
//! this row and Settings never disagree. Collections draws the SORT group alone.

use crate::anim::Spring;
use crate::collate::SortKey;
use crate::el::{El, Id};
use crate::icons::{self, Icon};
use crate::library::LibraryView;
use crate::theme::{Fonts, Frame, PanelStroke, W};
use skia_safe::Rect;

/// Row height, design units: a pill and its air.
pub(crate) const BAR_H: f64 = 48.0;
const PILL_H: f64 = 32.0;
const PILL_TEXT: f64 = 15.0;
const PILL_PAD: f64 = 13.0;
const PILL_GAP: f64 = 6.0;
/// A group's mark and Search's magnifier.
const MARK: f64 = 18.0;
const MARK_GAP: f64 = 10.0;
/// Least air between the two groups.
const GROUP_GAP: f64 = 24.0;

/// One pill: a sort, an arrangement, or the way into a title search.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Pill {
    Sort(SortKey),
    View(LibraryView),
    Search,
}

impl Pill {
    /// Every pill in row order; `views` adds the VIEW group and Search, the shelf's own.
    pub(crate) fn all(views: bool) -> Vec<Pill> {
        let sorts = SortKey::ALL.iter().map(|&s| Pill::Sort(s));
        let rest = (LibraryView::ALL.iter().map(|&v| Pill::View(v)))
            .chain([Pill::Search])
            .filter(|_| views);
        sorts.chain(rest).collect()
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Pill::Sort(s) => s.label(),
            Pill::View(v) => v.label(),
            Pill::Search => pf_l10n::client::lib_search(),
        }
    }
}

pub(crate) fn pill_id(i: usize) -> Id {
    Id::new("library-pill", i)
}

/// Pill rects and each group mark's left edge, bar-local px.
struct Layout {
    pills: Vec<Rect>,
    marks: Vec<(f64, Icon)>,
}

fn layout(fonts: &Fonts, k: f64, width: f64, views: bool) -> Layout {
    let pills = Pill::all(views);
    let widths: Vec<f64> = (pills.iter())
        .map(|p| match p {
            Pill::Search => PILL_H * k,
            _ => {
                f64::from(fonts.measure(p.label(), W::SemiBold, PILL_TEXT * k)) + 2.0 * PILL_PAD * k
            }
        })
        .collect();
    let run = |r: std::ops::Range<usize>| {
        let n = r.len();
        widths[r].iter().sum::<f64>() + PILL_GAP * k * n.saturating_sub(1) as f64
    };
    let top = (BAR_H - PILL_H) / 2.0 * k;
    let mut out = Layout {
        pills: Vec::new(),
        marks: Vec::new(),
    };
    let lead = (MARK + MARK_GAP) * k;
    let mut place = |x0: f64, r: std::ops::Range<usize>, mark: Icon| {
        out.marks.push((x0, mark));
        let mut x = x0 + lead;
        for w in &widths[r] {
            out.pills.push(Rect::from_xywh(
                x as f32,
                top as f32,
                *w as f32,
                (PILL_H * k) as f32,
            ));
            x += w + PILL_GAP * k;
        }
    };
    let sorts = SortKey::ALL.len();
    let sort_w = lead + run(0..sorts);
    place(0.0, 0..sorts, Icon(pf_client_core::lucide::ARROW_UP_DOWN));
    if views {
        let search = pills.len() - 1;
        let search_w = widths[search];
        let view_w = lead + run(sorts..search);
        // A narrow window crowds the groups rather than pushing SORT off the leading edge.
        let x = (width - search_w - GROUP_GAP * k - view_w).max(sort_w + GROUP_GAP * k);
        place(x, sorts..search, Icon(pf_client_core::lucide::EYE));
        let x = (width - search_w).max(x + view_w + GROUP_GAP * k);
        out.pills.push(Rect::from_xywh(
            x as f32,
            top as f32,
            search_w as f32,
            (PILL_H * k) as f32,
        ));
    }
    out
}

/// The capsules' travel: each group's left and right edge, bar-local px.
#[derive(Default)]
pub(crate) struct Bar {
    caps: [Option<[Spring; 2]>; 2],
}

impl Bar {
    /// Chase each group's capsule toward its applied pill, `applied[g]` in row order.
    /// `snap` seats it: a caller under reduced motion passes it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn step(
        &mut self,
        fonts: &Fonts,
        k: f64,
        width: f64,
        views: bool,
        applied: [usize; 2],
        dt: f64,
        snap: bool,
    ) {
        let l = layout(fonts, k, width, views);
        for (g, cap) in self
            .caps
            .iter_mut()
            .enumerate()
            .take(1 + usize::from(views))
        {
            let Some(r) = l.pills.get(applied[g]) else {
                continue;
            };
            let goal = [f64::from(r.left), f64::from(r.right)];
            let springs = cap.get_or_insert(goal.map(Spring::rest));
            for (s, want) in springs.iter_mut().zip(goal) {
                if snap {
                    *s = Spring::rest(want);
                } else {
                    s.step_spec(want, crate::anim::springs::FOCUS, dt);
                    s.settle(want, 0.25, 4.0);
                }
            }
        }
    }

    /// The row `width` px wide: group marks and capsules under the pills, each pill a focus
    /// target. `seen` hears each pill's rect as it is painted.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn el<'a>(
        &'a self,
        ui: &'a Frame,
        fonts: &'a Fonts,
        k: f64,
        width: f64,
        views: bool,
        applied: [usize; 2],
        focus: Option<usize>,
        seen: &'a dyn Fn(usize, Rect),
    ) -> El<'a> {
        let l = layout(fonts, k, width, views);
        let h = (BAR_H * k) as f32;
        let marks = l.marks;
        let pill_top = (BAR_H - PILL_H) / 2.0 * k;
        let under = El::paint(move |canvas, r| {
            let cy = r.top + (BAR_H / 2.0 * k) as f32;
            for &(x, mark) in &marks {
                let cx = r.left + (x + MARK / 2.0 * k) as f32;
                let size = (MARK * k) as f32;
                icons::draw_icon(canvas, mark, cx, cy, size, k as f32, ui.fg(0.5));
            }
            for [left, right] in self.caps.iter().flatten() {
                let pill = Rect::from_xywh(
                    r.left + left.pos as f32,
                    r.top + pill_top as f32,
                    (right.pos - left.pos) as f32,
                    (PILL_H * k) as f32,
                );
                let corner = (PILL_H / 2.0) as f32;
                let tint = Some(ui.accent(0.85));
                crate::theme::panel(
                    canvas,
                    ui,
                    pill,
                    corner,
                    tint,
                    PanelStroke::Plain(0.14),
                    k as f32,
                );
            }
        })
        .place(Rect::from_xywh(0.0, 0.0, width as f32, h));
        let mut row = El::column().size(width as f32, h).child(under);
        for (i, (pill, r)) in Pill::all(views).into_iter().zip(l.pills).enumerate() {
            let ink = match (applied.contains(&i), focus == Some(i)) {
                (true, _) => ui.on_accent(),
                (false, true) => ui.fg(1.0),
                (false, false) => ui.fg(0.62),
            };
            let label = pill.label();
            row = row.child(
                El::paint(move |canvas, r| {
                    seen(i, r);
                    if pill == Pill::Search {
                        let (cx, cy, size) = (r.center_x(), r.center_y(), (MARK * k) as f32);
                        let mark = Icon(pf_client_core::lucide::SEARCH);
                        icons::draw_icon(canvas, mark, cx, cy, size, k as f32, ink);
                        return;
                    }
                    let size = PILL_TEXT * k;
                    let tw = f64::from(fonts.measure(label, W::SemiBold, size));
                    let x = f64::from(r.center_x()) - tw / 2.0;
                    let y = f64::from(r.center_y()) + size * 0.36;
                    fonts.draw(canvas, label, x, y, W::SemiBold, size, ink);
                })
                .id(pill_id(i))
                .focusable((PILL_H * k / 2.0) as f32)
                .place(r),
            );
        }
        row
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SORT leads at the margin; Search ends on the trailing edge, VIEW before it; no pill
    /// overlaps another.
    #[test]
    fn the_groups_sit_on_both_edges_without_touching() {
        let fonts = crate::theme::build_fonts().unwrap();
        let l = layout(&fonts, 1.0, 1200.0, true);
        assert_eq!(l.pills.len(), 9);
        assert_eq!(l.marks[0].0, 0.0);
        assert!((f64::from(l.pills[8].right) - 1200.0).abs() < 0.5);
        assert!(l.pills.windows(2).all(|w| w[0].right < w[1].left));
        let narrow = layout(&fonts, 1.0, 400.0, true);
        assert!(narrow.pills.windows(2).all(|w| w[0].right < w[1].left));
    }
}
