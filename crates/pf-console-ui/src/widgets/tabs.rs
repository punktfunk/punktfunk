//! The section tab strip and the console's pill buttons.

use crate::el::{El, Id, Tree};
use crate::pointer::Pointer;
use crate::theme::{edge, fg, Fonts, PanelStroke, W};
use skia_safe::{Canvas, Rect};

/// Strip band height, including air under the pills before the first row.
pub const TAB_STRIP_H: f64 = 46.0;

/// Pill row inside the band: top 2, height 30; the remaining 14 is air below.
/// Published so a backdrop (library focus wash) uses the row, not the 46 band.
pub const TAB_PILL_TOP: f64 = 2.0;
pub const TAB_PILL_H: f64 = 30.0;

/// Horizontal section switcher, drawn as the console's tabs: bold text, the current one
/// in full ink, the plate behind it while the strip has focus. Presentational: the screen
/// owns selection and the shoulders.
pub struct TabStrip {
    /// The tabs as focus targets, so the plate travels between them. Boxed: a strip sits
    /// inside screens that are variants of one enum.
    tree: Box<Tree>,
    /// Last-drawn tab rects, device px — the pointer hit-tests what was drawn.
    pills: Vec<Rect>,
}

const PILL_TEXT: f64 = 16.0;
const PILL_PAD_X: f64 = 10.0;
const PILL_GAP: f64 = 4.0;

/// A button's label size, its side padding, and its height, design units.
pub(crate) const BUTTON_TEXT: f64 = 15.0;
pub(crate) const BUTTON_PAD: f64 = 18.0;
pub(crate) const BUTTON_H: f64 = 38.0;

/// The console's button: a glass pill with its label. The plate behind it is focus.
pub(crate) fn button(canvas: &Canvas, fonts: &Fonts, label: &str, r: Rect, k: f64) {
    let corner = (f64::from(r.height()) / 2.0 / k) as f32;
    crate::theme::panel(canvas, r, corner, None, PanelStroke::Plain(0.08), k as f32);
    let size = BUTTON_TEXT * k;
    let tw = f64::from(fonts.measure(label, W::SemiBold, size));
    let (x, y) = (
        f64::from(r.center_x()) - tw / 2.0,
        f64::from(r.center_y()) + size * 0.36,
    );
    fonts.draw(canvas, label, x, y, W::SemiBold, size, fg(0.95));
}

/// A button's width for `label`, device px.
pub(crate) fn button_w(fonts: &Fonts, label: &str, k: f64) -> f64 {
    f64::from(fonts.measure(label, W::SemiBold, BUTTON_TEXT * k)) + 2.0 * BUTTON_PAD * k
}

/// A tab's label, bold and centred in `r`. Every row of tabs in the console draws with it.
pub(crate) fn text_tab(
    canvas: &Canvas,
    fonts: &Fonts,
    label: &str,
    r: Rect,
    size: f64,
    ink: skia_safe::Color4f,
) {
    let tw = f64::from(fonts.measure(label, W::Bold, size));
    let x = f64::from(r.center_x()) - tw / 2.0;
    let y = f64::from(r.center_y()) + size * 0.36;
    fonts.draw(canvas, label, x, y, W::Bold, size, ink);
}

/// Each pill's width and the run's total, device px. Shared with
/// [`TabStrip::width`]: a trailing-aligned caller needs the width before draw,
/// and a second copy of this arithmetic would disagree with the drawn edge.
fn pill_widths(labels: &[&str], fonts: &Fonts, k: f64) -> (Vec<f64>, f64) {
    let size = PILL_TEXT * k;
    let widths: Vec<f64> = labels
        .iter()
        .map(|l| f64::from(fonts.measure(l, W::Bold, size)) + 2.0 * PILL_PAD_X * k)
        .collect();
    let total = widths.iter().sum::<f64>() + PILL_GAP * k * (labels.len().saturating_sub(1)) as f64;
    (widths, total)
}

fn tab_id(i: usize) -> Id {
    Id::new("tab-strip", i)
}

impl Default for TabStrip {
    fn default() -> Self {
        Self::new()
    }
}

impl TabStrip {
    /// Drawn width of this pill run, for a caller placing it on a trailing edge.
    pub fn width(labels: &[&str], fonts: &Fonts, k: f64) -> f64 {
        pill_widths(labels, fonts, k).1
    }

    pub fn new() -> TabStrip {
        TabStrip {
            tree: Box::new(Tree::new()),
            pills: Vec::new(),
        }
    }

    /// Last-drawn pill rect; tests assert what a press can reach.
    #[cfg(test)]
    pub fn pill(&self, i: usize) -> Option<Rect> {
        self.pills.get(i).copied()
    }

    /// OK went down on the focused tab: its plate dips.
    pub fn press(&mut self) {
        self.tree.press();
    }

    /// Tab a press landed on. Hit box is the full strip height: pills are too
    /// small for a tap that misses the text.
    pub fn pointer(&self, p: Pointer) -> Option<usize> {
        p.press().then(|| p.pick(&self.pills)).flatten()
    }

    /// Draw the tabs on the leading edge of `rect`'s top band, the text on [`edge`].
    /// `focused` is D-pad focus (no-shoulder remote): the plate stands behind the current tab.
    #[allow(clippy::too_many_arguments)] // same render signature as MenuList
    pub fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        labels: &[&str],
        selected: usize,
        focused: bool,
        fonts: &Fonts,
        k: f64,
        dt: f64,
    ) {
        if labels.is_empty() {
            return;
        }
        let (pill_h, size, pad) = (TAB_PILL_H * k, PILL_TEXT * k, PILL_PAD_X * k);
        let (widths, total) = pill_widths(labels, fonts, k);
        // Text on the heading's column: a centred strip under a left-aligned title reads as
        // two pieces of chrome. Clamp, not a branch: full inset, then centred, then
        // flush-left (overflow spends on the unread right).
        let slack = f64::from(rect.width()) - total;
        let mut x = f64::from(rect.left) + (edge(k) - pad).min((slack / 2.0).max(0.0));
        let top = f64::from(rect.top) + TAB_PILL_TOP * k;
        let sel = selected.min(labels.len() - 1);
        self.pills.clear();
        let mut row = El::column();
        for (i, label) in labels.iter().copied().enumerate() {
            // Full-height hit box; width is this tab only, so neighbours cannot both claim a press.
            let hit = rect.height().max((pill_h + 4.0 * k) as f32);
            self.pills
                .push(Rect::from_xywh(x as f32, rect.top, widths[i] as f32, hit));
            let ink = fg(if i == sel { 1.0 } else { 0.6 });
            let r = Rect::from_xywh(
                x as f32 - rect.left,
                top as f32 - rect.top,
                widths[i] as f32,
                pill_h as f32,
            );
            row = row.child(
                El::paint(move |canvas, r| text_tab(canvas, fonts, label, r, size, ink))
                    .id(tab_id(i))
                    .focusable((10.0 * k) as f32)
                    .place(r),
            );
            x += widths[i] + PILL_GAP * k;
        }
        let frame = self.tree.layout(row, rect);
        self.tree.set_focus(focused.then(|| tab_id(sel)));
        self.tree.paint_focus(canvas, frame, k as f32, dt, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABS: [&str; 7] = [
        "Stream",
        "Video",
        "Audio",
        "Controller",
        "Input",
        "Interface",
        "Presets",
    ];

    /// Focused, a burst of section changes leaves the plate on the selected tab.
    #[test]
    fn the_plate_lands_on_the_selected_tab() {
        let fonts = crate::theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((900, 120)).unwrap();
        let rect = Rect::from_xywh(0.0, 0.0, 900.0, TAB_STRIP_H as f32);
        let mut strip = TabStrip::new();
        let dt = 1.0 / 60.0;
        for sel in (0..=5).chain([5; 240]) {
            strip.render(surface.canvas(), rect, &TABS, sel, true, &fonts, 1.0, dt);
        }
        let pill = strip.pill(5).expect("the selected tab was drawn");
        let (plate, _) = strip
            .tree
            .plate_rect()
            .expect("a focused strip has a plate");
        assert!(
            (plate.left - pill.left).abs() < 0.5 && (plate.width() - pill.width()).abs() < 0.5,
            "the plate rests at {plate:?}, the tab is at {pill:?}"
        );
    }

    /// Same column as the heading ([`edge`]); a shrinking window gives it
    /// up as inset, then centred, then flush. Ordering, not three pixel x's,
    /// so a renamed tab does not break the pin.
    #[test]
    fn tab_strip_stands_on_the_edge_inset_and_gives_it_up_in_order() {
        let fonts = crate::theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((1400, 160)).unwrap();
        let dt = 1.0 / 60.0;
        // Measure the actual run so nothing below is a hardcoded pixel.
        let mut run = |w: f32, k: f64| {
            let rect = Rect::from_xywh(0.0, 0.0, w, (TAB_STRIP_H * k) as f32);
            let mut strip = TabStrip::new();
            strip.render(surface.canvas(), rect, &TABS, 0, false, &fonts, k, dt);
            let first = strip.pill(0).expect("the first section was drawn");
            let text = f64::from(first.left) + PILL_PAD_X * k;
            let last = strip
                .pill(TABS.len() - 1)
                .expect("the last section was drawn");
            (rect, f64::from(first.left), f64::from(last.right), text)
        };

        // Both insets fit: the text starts on the heading column at every scale.
        for k in [0.75, 1.0, 2.0] {
            let (rect, _, right, left) = run(1400.0, k);
            assert!(
                (left - (f64::from(rect.left) + edge(k))).abs() < 0.5,
                "k={k}: strip starts at {left}, not on the {} column",
                edge(k)
            );
            assert!(
                right <= f64::from(rect.right),
                "k={k}: strip overran its band"
            );
        }

        let (_, wide_left, wide_right, _) = run(1400.0, 1.0);
        let total = wide_right - wide_left;

        // Narrower than both insets, wider than the run: centre so the shortfall is not all on one edge.
        let (rect, left, right, _) = run((total + crate::theme::EDGE_INSET) as f32, 1.0);
        assert!(
            (left - f64::from(rect.left) - (f64::from(rect.right) - right)).abs() < 0.5,
            "a squeezed strip should sit even: {left} in from the left, {} from the right",
            f64::from(rect.right) - right
        );

        // Wider than the band: flush left, overflow right only.
        let (rect, left, right, _) = run((total - 40.0) as f32, 1.0);
        assert!(
            (left - f64::from(rect.left)).abs() < 0.5,
            "an overflowing strip should go flush left, not to {left}"
        );
        assert!(
            right > f64::from(rect.right),
            "the run was supposed to overflow"
        );
    }
}
