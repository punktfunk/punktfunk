//! The menu list: the row a screen describes each frame, the trays and foot band
//! around it, and the list that steps, scrolls and paints the rows.
//!
//! Pixel contracts live in the tests below: a stepped value stays in its field,
//! and a slip on one row does not blank the rest of the column.

use crate::anim::{approach, entrances, springs, Entrance, EntranceAt, Spring};
use crate::anim::{BUMP_C, BUMP_K, BUMP_V};
use crate::el::{Axis, El, Id, Tree};
use crate::pointer::{Pointer, PointerKind};
use crate::theme::{accent, edge, fg, fill, stroke, Fonts, PanelStroke, W};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, PathBuilder, RRect, Rect};
use std::cell::RefCell;

/// What a consumed menu event means for the owning screen.
#[derive(Debug, PartialEq, Eq)]
pub enum ListMsg {
    None,
    /// Left/right on the focused row.
    Adjust(i32),
    Activate,
}

/// What sits at a row's trailing end. [`Value`](Self::Value) is the console's own row —
/// text, `‹ ›` when adjustable. The rest are the pointer shells' controls (a switch, a
/// track), drawn so a hover-to-focus pointer and a pad read the same row.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Control {
    #[default]
    Value,
    /// A switch, on or off. The value string is not drawn.
    Toggle(bool),
    /// A track filled to `frac` ∈ [0, 1], the value string beside it.
    Slider(f32),
}

/// One row, rebuilt by the screen each frame. `..RowSpec::default()` fills what a row
/// does not name.
#[derive(Clone)]
pub struct RowSpec {
    /// Header above this row; only the first row of a group carries it.
    pub header: Option<&'static str>,
    pub label: String,
    /// `None` = action row: the label alone, where every row's label starts.
    pub value: Option<String>,
    /// Dim the value as a placeholder when the field is empty.
    pub value_dim: bool,
    /// Live-edit caret; this is the row the keyboard types into.
    pub caret: bool,
    /// ‹ › while focused; left/right steps the value.
    pub adjustable: bool,
    /// Dimmed when not actionable (a setting that depends on another being on).
    pub enabled: bool,
    pub control: Control,
    /// Lucide mark before the label (`icons::by_name`).
    pub icon: Option<&'static str>,
    /// One line under the label, small: why a row is locked, what a value costs.
    pub note: Option<String>,
    /// A loss: label and value in the error tone.
    pub danger: bool,
    /// Accent dot before the label — a value this scope overrides.
    pub dot: bool,
    /// Round icon buttons after the value (rename, remove). The pointer shells' rows carry
    /// these; a pad steps onto them with left/right, which is the screen's to track.
    pub buttons: &'static [&'static str],
    /// Which of [`buttons`](Self::buttons) is lit on the focused row.
    pub button: Option<usize>,
    /// A grip before the label: the row can be picked up and moved.
    pub handle: bool,
    /// The row is picked up — the grip lights and the row reads as in hand.
    pub held: bool,
}

impl Default for RowSpec {
    fn default() -> Self {
        RowSpec {
            header: None,
            label: String::new(),
            value: None,
            value_dim: false,
            caret: false,
            adjustable: false,
            enabled: true,
            control: Control::Value,
            icon: None,
            note: None,
            danger: false,
            dot: false,
            buttons: &[],
            button: None,
            handle: false,
            held: false,
        }
    }
}

impl RowSpec {
    pub fn field(label: impl Into<String>, value: String, placeholder: &str) -> RowSpec {
        let empty = value.is_empty();
        RowSpec {
            label: label.into(),
            value: Some(if empty {
                placeholder.to_string()
            } else {
                value
            }),
            value_dim: empty,
            ..RowSpec::default()
        }
    }

    pub fn action(label: impl Into<String>, enabled: bool) -> RowSpec {
        RowSpec {
            label: label.into(),
            enabled,
            ..RowSpec::default()
        }
    }

    /// A switch row. Activate flips it; the value string is the switch.
    pub fn toggle(label: impl Into<String>, on: bool) -> RowSpec {
        RowSpec {
            label: label.into(),
            value: Some(if on { "On" } else { "Off" }.into()),
            control: Control::Toggle(on),
            ..RowSpec::default()
        }
    }

    /// A stepped pick: the value with `‹ ›`, Activate cycles it forward.
    pub fn choice(label: impl Into<String>, value: impl Into<String>) -> RowSpec {
        RowSpec {
            label: label.into(),
            value: Some(value.into()),
            adjustable: true,
            ..RowSpec::default()
        }
    }

    /// A track at `frac`, with `value` as its readout; left/right step it.
    pub fn slider(label: impl Into<String>, value: impl Into<String>, frac: f32) -> RowSpec {
        RowSpec {
            label: label.into(),
            value: Some(value.into()),
            adjustable: true,
            control: Control::Slider(frac.clamp(0.0, 1.0)),
            ..RowSpec::default()
        }
    }

    pub fn with_icon(mut self, icon: &'static str) -> RowSpec {
        self.icon = Some(icon);
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> RowSpec {
        self.note = Some(note.into());
        self
    }

    pub fn with_header(mut self, header: &'static str) -> RowSpec {
        self.header = Some(header);
        self
    }

    /// Greyed, with the reason under the label. Still focusable, so the reason can be read.
    pub fn locked(mut self, why: impl Into<String>) -> RowSpec {
        self.enabled = false;
        self.adjustable = false;
        self.note = Some(why.into());
        self
    }

    /// Trailing icon buttons, `lit` the one the pad has stepped onto.
    pub fn with_buttons(mut self, buttons: &'static [&'static str], lit: Option<usize>) -> RowSpec {
        self.buttons = buttons;
        self.button = lit;
        self
    }

    /// A drag grip before the label, `held` while the row is picked up.
    pub fn with_handle(mut self, held: bool) -> RowSpec {
        self.handle = true;
        self.held = held;
        self
    }
}

/// Trailing button diameter and pitch, design units.
const BUTTON_D: f64 = 32.0;
const BUTTON_PITCH: f64 = 40.0;

pub const ROW_H: f64 = 50.0;
/// Full blur under pinned text, design units: Glur's radius on the Apple gamepad trays.
const TRAY_SIGMA: f64 = 14.0;
/// How far a tray notionally runs past the glass, design units: Glur's 80 pt bleed.
const TRAY_OVERHANG: f64 = 80.0;
/// Clears the plate's 7 dp outset with air to spare.
const ROW_GAP: f64 = 10.0;

/// The screen edge a [`tray`] leans on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Toward {
    Top,
    Bottom,
}

/// The backdrop under pinned text: what already lies under `rect` blurs from nothing on
/// its content side to full on its `toward` side. No tint, as Glur has none: text reads on
/// the blur. Draw it after what scrolls under, before the text; trays stacked edge to edge
/// ramp on from each other without a seam.
pub fn tray(canvas: &Canvas, rect: Rect, toward: Toward, k: f64) {
    if rect.height() < 1.0 {
        return;
    }
    let (edge, clear) = match toward {
        Toward::Top => (rect.top, rect.bottom),
        Toward::Bottom => (rect.bottom, rect.top),
    };
    let sigma = (TRAY_SIGMA * k) as f32;
    let over = (TRAY_OVERHANG * k) as f32;
    let band = crate::blur::Band {
        edge,
        clear,
        sigma,
        over,
    };
    crate::blur::backdrop(canvas, rect, band);
}
/// The band at a screen's foot, on the shell's bottom tray: a focused title with its
/// provenance, or one detail line led by a mark. A screen reaches the tray in by the
/// band's height (`Screen::pinned`) and paints this from `render_pinned`, so one tray
/// serves every screen and none draws its own.
#[derive(Default)]
pub struct Foot<'a> {
    /// Centred, bold: the focused game.
    pub title: Option<&'a str>,
    /// Tracked capitals under the title: where it comes from.
    pub subtitle: Option<&'a str>,
    /// A short note on the leading margin, on the subtitle's line.
    pub note: Option<&'a str>,
    /// One explainer line at the band's top.
    pub detail: Option<&'a str>,
    /// Icon name leading `detail`.
    pub mark: Option<&'a str>,
    /// A scrim deepening toward the foot: posters are too bright for white on blur alone.
    pub deep: bool,
}

/// Band heights, design units: a title with its line, or one detail line.
pub const FOOT_TITLE_H: f64 = 66.0;
pub const FOOT_DETAIL_H: f64 = 34.0;

impl Foot<'_> {
    /// Over `band`, the tray already under it; the detail runs from `left` to `right`.
    pub fn paint(
        &self,
        canvas: &Canvas,
        fonts: &Fonts,
        band: Rect,
        (left, right): (f64, f64),
        k: f64,
    ) {
        use crate::theme::{fg, W};
        if self.deep {
            let clip = canvas.local_clip_bounds().unwrap_or(band);
            let back = Rect::from_ltrb(
                clip.left.min(band.left),
                band.top,
                clip.right.max(band.right),
                clip.bottom.max(band.bottom),
            );
            let deep = band.top + (FOOT_TITLE_H * 0.45 * k) as f32;
            let colors = [crate::theme::shade(0.0), crate::theme::shade(0.42)];
            let mut scrim = crate::theme::shaded();
            scrim.set_shader(skia_safe::gradient::shaders::linear_gradient(
                (
                    skia_safe::Point::new(band.left, band.top),
                    skia_safe::Point::new(band.left, deep),
                ),
                &skia_safe::gradient::Gradient::new(
                    skia_safe::gradient::Colors::new_evenly_spaced(
                        &colors,
                        skia_safe::TileMode::Clamp,
                        None,
                    ),
                    skia_safe::gradient::Interpolation::default(),
                ),
                None,
            ));
            canvas.draw_rect(back, &scrim);
        }
        let cx = f64::from(band.center_x());
        let max_w = f64::from(band.width()) - 2.0 * edge(k);
        if let Some(title) = self.title {
            let (w, size) = (W::Bold, 25.0 * k);
            let tw = f64::from(fonts.measure(title, w, size)).min(max_w);
            let base = f64::from(band.top) + 32.0 * k;
            fonts.draw_clipped(canvas, title, cx - tw / 2.0, base, w, size, fg(1.0), max_w);
        }
        let base = f64::from(band.top) + 53.0 * k;
        if let Some(note) = self.note {
            let x = f64::from(band.left) + edge(k);
            fonts.draw_clipped(
                canvas,
                note,
                x,
                base,
                W::Regular,
                11.0 * k,
                fg(0.55),
                max_w / 3.0,
            );
        }
        if let Some(sub) = self.subtitle {
            let (size, track) = (11.0 * k, 1.2 * k);
            let tw = f64::from(fonts.measure(sub, W::SemiBold, size))
                + track * sub.chars().count().saturating_sub(1) as f64;
            fonts.draw_tracked(
                canvas,
                sub,
                cx - tw / 2.0,
                base,
                W::SemiBold,
                size,
                track,
                fg(0.55),
            );
        }
        if let Some(detail) = self.detail.filter(|d| !d.is_empty()) {
            let top = f64::from(band.top) + 6.0 * k;
            let mark = self.mark.and_then(crate::icons::by_name);
            let lead = if mark.is_some() { 22.0 * k } else { 0.0 };
            let ink = fg(0.55);
            fonts.leading(
                canvas,
                detail,
                W::Regular,
                13.0 * k,
                ink,
                left + lead,
                top,
                right - left - lead,
            );
            if let Some(mark) = mark {
                let (cx, cy) = ((left + 7.0 * k) as f32, (top + 8.0 * k) as f32);
                crate::icons::draw_icon(canvas, mark, cx, cy, (14.0 * k) as f32, ink);
            }
        }
    }
}

/// A section header's band above its row, and its baseline's rise over the row: the text
/// stays clear of the plate, stretched in flight, on the row below.
const HEADER_H: f64 = 44.0;
const HEADER_RISE: f64 = 18.0;
pub const ROW_MAX_W: f64 = 620.0;
/// Air between a form's blurb and its first row: the plate's outset and then some.
const BLURB_GAP: f64 = 20.0;

/// The form column in `rect`: at most [`ROW_MAX_W`] wide, centred, never nearer the
/// screen's edge than the shared margin.
pub fn column(rect: Rect, k: f64) -> Rect {
    let w = (ROW_MAX_W * k).min(f64::from(rect.width()) - 2.0 * edge(k)) as f32;
    Rect::from_xywh(rect.center_x() - w / 2.0, rect.top, w, rect.height())
}

/// A form's blurb at the top of `rect`, on the column. Returns what is left below it for
/// the rows, however many lines it took.
pub fn blurb(canvas: &Canvas, fonts: &Fonts, text: &str, rect: Rect, k: f64) -> Rect {
    let col = column(rect, k);
    let (x, y, w) = (
        f64::from(col.left),
        f64::from(rect.top) + 2.0 * k,
        f64::from(col.width()),
    );
    let h = fonts.leading(canvas, text, W::Regular, 13.0 * k, fg(0.55), x, y, w);
    let top = y + h + BLURB_GAP * k;
    Rect::from_ltrb(
        rect.left,
        top as f32,
        rect.right,
        rect.bottom.max(top as f32),
    )
}

/// The list's scroll node, and row `i`'s cell in it.
const LIST: &str = "menu-list";
fn row_id(i: usize) -> Id {
    Id::new("menu-row", i)
}

/// A row's trailing button rects at `cell`, in order. Drawing and hit-testing share it.
fn button_rects(cell: Rect, row: &RowSpec, k: f64) -> impl Iterator<Item = Rect> + '_ {
    let (x0, row_w, cy) = (
        f64::from(cell.left),
        f64::from(cell.width()),
        f64::from(cell.center_y()),
    );
    let buttons_w = row.buttons.len() as f64 * BUTTON_PITCH * k;
    let d = BUTTON_D * k;
    (0..row.buttons.len()).map(move |j| {
        let cx = x0 + row_w - 16.0 * k - buttons_w + (j as f64 + 0.5) * BUTTON_PITCH * k;
        Rect::from_xywh(
            (cx - d / 2.0) as f32,
            (cy - d / 2.0) as f32,
            d as f32,
            d as f32,
        )
    })
}

/// A slider row's track at `cell`, inside its value field; empty on any other row.
fn track_rect(cell: Rect, row: &RowSpec, k: f64, dot_gutter: f64) -> Rect {
    if row.value.is_none() || !matches!(row.control, Control::Slider(_)) {
        return Rect::new_empty();
    }
    let buttons_w = row.buttons.len() as f64 * BUTTON_PITCH * k;
    let row_w = f64::from(cell.width()) - buttons_w - dot_gutter;
    let chevron_w = if row.adjustable { 18.0 * k } else { 0.0 };
    let (tw, th) = (120.0 * k, 6.0 * k);
    let right = f64::from(cell.left) + row_w - 16.0 * k - chevron_w;
    Rect::from_xywh(
        (right - tw) as f32,
        (f64::from(cell.center_y()) - th / 2.0) as f32,
        tw as f32,
        th as f32,
    )
}

/// How far a stepped value slips before springing back, design units.
/// One sprung offset plus a crossfade: rows draw a single text run, not neighbouring values.
const SLIP_DP: f64 = 14.0;
/// Cap so a held repeat is one travel, not a value thrown off the row.
const SLIP_MAX: f64 = 22.0;
/// Confirm-dip floor (visual sibling of the haptic).
/// Mount rise, design units. A twelfth of the carousel travel — same language, smaller.
const ROW_RISE: f64 = 12.0;

/// Room above a list's first row and left of its column for the plate's outset, design units.
const PLATE_AIR: f64 = 12.0;
/// A scroller's soft edge, design units: where content hides past an edge, this much of
/// the scroller fades and blurs toward it.
const SOFT_EDGE: f64 = 40.0;

/// Paints a scroller through `paint` inside `view`, with soft edges where content hides
/// past `rect`: `scrolled` is its offset and reach. A scroller at rest on its top keeps a
/// crisp top; pinned text past it sits on the field, never on a row.
pub fn soft_scroll(
    canvas: &Canvas,
    view: Rect,
    rect: Rect,
    (offset, max): (f32, f32),
    k: f64,
    paint: impl FnOnce(),
) {
    let soft = (SOFT_EDGE * k) as f32;
    let above = (offset / soft).clamp(0.0, 1.0);
    let below = ((max - offset) / soft).clamp(0.0, 1.0);
    let edged = above > 0.0 || below > 0.0;
    if edged {
        canvas.save_layer(
            &skia_safe::canvas::SaveLayerRec::default()
                .bounds(&view)
                .flags(crate::theme::layer_flags(canvas)),
        );
    }
    paint();
    if edged {
        soft_edges(canvas, view, rect, soft, (above, below), k);
    }
}

/// Closes the layer a list painted into with its soft edges. `strength` is how much hides
/// past the top and the bottom, 0–1, so a list resting on its first row keeps a crisp top.
/// Rows fade toward an edge and what is left blurs, as a scroll view's edge does on Apple's
/// systems; pinned text past the list sits on the field, never on a row.
fn soft_edges(canvas: &Canvas, view: Rect, rect: Rect, depth: f32, strength: (f32, f32), k: f64) {
    use skia_safe::{gradient, BlendMode, Color4f, Point, TileMode};
    let (top, bottom) = strength;
    let alpha = |a: f32| Color4f::new(0.0, 0.0, 0.0, a);
    let edge = (depth / rect.height().max(1.0)).min(0.5);
    let colors = [
        alpha(1.0 - top),
        alpha(1.0),
        alpha(1.0),
        alpha(1.0 - bottom),
    ];
    let pos = [0.0, edge, 1.0 - edge, 1.0];
    let mut p = crate::theme::fill(alpha(1.0));
    p.set_blend_mode(BlendMode::DstIn);
    p.set_shader(gradient::shaders::linear_gradient(
        (Point::new(0.0, rect.top), Point::new(0.0, rect.bottom)),
        &gradient::Gradient::new(
            gradient::Colors::new(&colors, Some(&pos), TileMode::Clamp, None),
            gradient::Interpolation::default(),
        ),
        None,
    ));
    canvas.draw_rect(view, &p);
    canvas.restore();
    let sigma = (TRAY_SIGMA * k) as f32;
    let (l, r) = (view.left, view.right);
    if top > 0.0 {
        let band = Rect::from_ltrb(l, view.top, r, rect.top + depth);
        let clear = rect.top + depth;
        let b = crate::blur::Band {
            edge: rect.top,
            clear,
            sigma: sigma * top,
            over: 0.0,
        };
        crate::blur::backdrop(canvas, band, b);
    }
    if bottom > 0.0 {
        let band = Rect::from_ltrb(l, rect.bottom - depth, r, view.bottom);
        let clear = rect.bottom - depth;
        let b = crate::blur::Band {
            edge: rect.bottom,
            clear,
            sigma: sigma * bottom,
            over: 0.0,
        };
        crate::blur::backdrop(canvas, band, b);
    }
}

struct SlipPrev {
    /// Index and label both: the screen rebuilds rows every frame, so an index
    /// alone would slide this text onto whichever row inherited it.
    row: usize,
    label: String,
    text: String,
    /// Offset from the incoming value (∓[`SLIP_DP`]). Relative so a reverse
    /// through zero on a held repeat still travels the right way.
    offset: f64,
    /// Slip position at arm time — the span the crossfade divides by. Not [`SLIP_DP`]:
    /// a held repeat reaches [`SLIP_MAX`], and dividing by the smaller constant leaves
    /// the incoming value at alpha 0 for the first third of travel.
    arm: f64,
}

pub struct MenuList {
    pub cursor: usize,
    bump: Spring,
    /// Layout, scroll and hit rects. A cell: the row painters borrow the list while
    /// the tree lays them out.
    tree: RefCell<Tree>,
    /// The scroll centres the focused row. A finger pan lets go until focus moves.
    follow: bool,
    /// Colour channel of focus (alpha, chevrons), eased. Not sprung: overshoot would
    /// leave the palette. The plate behind the row is the focus mark itself.
    focus: Vec<f64>,
    /// Stepped-value displacement, chasing 0 from ±[`SLIP_DP`]. Not reset on a
    /// new step: velocity is what lets held repeats accumulate into one travel.
    slip: Spring,
    /// Value sliding out. `None` = no mid-step, so every other row draws no crossfade.
    slip_prev: Option<SlipPrev>,
    /// Last emitted step direction, consumed next render. The list arms slip by
    /// noticing the value changed; a refused adjust produces none.
    step_dir: i32,
    /// Last-drawn value per row: the "before" of the crossfade, and whether an adjust landed.
    shown: Vec<String>,
    /// Switch knob position per row, eased toward 0/1 so a flip slides rather than snaps.
    knobs: Vec<f64>,
    /// Mount entrance. Not replayed on a tab switch: `jump_to` seats instantly,
    /// and chasing rows that no longer exist reads as a glitch.
    entrance: Option<Entrance>,
    entrance_armed: bool,
    age: f64,
    /// Next render seats scroll and focus instantly; see [`MenuList::jump_to`].
    snap: bool,
    /// Last-drawn row cells, device px. Empty for rows scrolled out of view, so
    /// an index here is an index into `rows`.
    geom: Vec<Rect>,
    /// Last-drawn trailing button rects per row, same indexing.
    buttons_geom: Vec<Vec<Rect>>,
    /// Last-drawn slider tracks by row; empty for rows without one.
    tracks_geom: Vec<Rect>,
    /// True once nothing is still moving. `false` until the first render so a
    /// fresh list always asks for a frame.
    settled: bool,
    /// Rows run on past the list's rect to the layer's edges, under whatever the screen
    /// draws after the list; a [`tray`] there treats them. The resting layout does not move.
    pub bleed: bool,
}

impl Default for MenuList {
    fn default() -> Self {
        Self::new()
    }
}

impl MenuList {
    pub fn new() -> MenuList {
        MenuList {
            cursor: 0,
            bump: Spring::rest(0.0),
            tree: RefCell::new(Tree::new()),
            follow: true,
            focus: Vec::new(),
            slip: Spring::rest(0.0),
            slip_prev: None,
            step_dir: 0,
            shown: Vec::new(),
            knobs: Vec::new(),
            entrance: None,
            entrance_armed: false,
            age: 0.0,
            snap: true,
            geom: Vec::new(),
            buttons_geom: Vec::new(),
            tracks_geom: Vec::new(),
            settled: false,
            bleed: false,
        }
    }

    /// True while an entrance, ease, or spring is still moving. The damage-gated
    /// stream overlay redraws until this is false; the console paints every frame.
    // Unused on Android, and on any build without the Vulkan overlay — the wasm console is the
    // second of those.
    #[cfg_attr(
        any(target_os = "android", not(feature = "vulkan-overlay")),
        allow(dead_code)
    )]
    pub fn animating(&self) -> bool {
        !self.settled
    }

    /// Move the cursor without the scroll gliding. For a tab switch: chasing
    /// would sweep through rows that no longer exist.
    pub fn jump_to(&mut self, cursor: usize) {
        self.cursor = cursor;
        self.snap = true;
        self.follow = true;
    }

    /// Up/down move focus (Boundary = recoil). Left/right → [`ListMsg::Adjust`],
    /// A → [`ListMsg::Activate`]. B is the screen's.
    pub fn menu(&mut self, ev: MenuEvent, len: usize) -> (ListMsg, Option<MenuPulse>) {
        match ev {
            MenuEvent::Move(MenuDir::Up) => (ListMsg::None, self.step(-1, len)),
            MenuEvent::Move(MenuDir::Down) => (ListMsg::None, self.step(1, len)),
            MenuEvent::Move(MenuDir::Left) => (ListMsg::Adjust(-1), self.armed(-1)),
            MenuEvent::Move(MenuDir::Right) => (ListMsg::Adjust(1), self.armed(1)),
            // A cycles a value row forward (same slip as Right); an action row does not step.
            MenuEvent::Confirm => {
                self.armed(1);
                self.dip();
                (ListMsg::Activate, Some(MenuPulse::Confirm))
            }
            _ => (ListMsg::None, None),
        }
    }

    /// Record that a step went out in `dir`. Returns `None` so it fills the pulse
    /// slot without changing it; the next render sees whether the value moved.
    fn armed(&mut self, dir: i32) -> Option<MenuPulse> {
        self.step_dir = dir;
        None
    }

    /// Confirm dip; also OK going down on a remote. Separate from [`Self::armed`]: an
    /// action row still presses.
    pub fn dip(&mut self) {
        self.tree.get_mut().press();
    }

    /// The trailing button under the pointer, as `(row, button)`. The screen decides what
    /// a hover or a press on it does; [`Self::pointer`] only knows rows.
    pub fn button_at(&self, p: Pointer) -> Option<(usize, usize)> {
        self.buttons_geom
            .iter()
            .enumerate()
            .find_map(|(i, rects)| p.pick(rects).map(|j| (i, j)))
    }

    /// Where along row `i`'s slider track the pointer's `x` falls, 0..=1, if the row drew
    /// one. A press on the row that lands within the track's row band is a seek; the
    /// screen turns the fraction into a value and keeps dragging with it.
    pub fn track_frac(&self, i: usize, x: f64) -> Option<f32> {
        let track = self.tracks_geom.get(i).copied().filter(|r| !r.is_empty())?;
        Some(((x as f32 - track.left) / track.width().max(1.0)).clamp(0.0, 1.0))
    }

    /// Whether the pointer is on row `i`'s slider track, widened to the row's height so a
    /// 6 dp line is not the target.
    pub fn on_track(&self, i: usize, p: Pointer) -> bool {
        let Some(track) = self.tracks_geom.get(i).copied().filter(|r| !r.is_empty()) else {
            return false;
        };
        let Some(row) = self.geom.get(i) else {
            return false;
        };
        p.hits(Rect::from_ltrb(
            track.left - 8.0,
            row.top,
            track.right + 8.0,
            row.bottom,
        ))
    }

    /// Last-drawn row rect; tests assert what a press can reach.
    #[cfg(test)]
    pub fn row_rect(&self, i: usize) -> Option<Rect> {
        self.geom.get(i).copied().filter(|r| !r.is_empty())
    }

    /// Press focuses and activates the row under it (click = move + A). A press
    /// in empty margin is swallowed so it does not fall through to the screen.
    pub fn pointer(&mut self, p: Pointer, len: usize) -> (ListMsg, Option<MenuPulse>) {
        match p.kind {
            PointerKind::Scroll { up } => (ListMsg::None, self.step(if up { -1 } else { 1 }, len)),
            // Hover focuses. Only a real change pulses: a pointer resting on a row emits a
            // Move every frame, and a pulse per frame would be a stuck note.
            PointerKind::Move => match p.pick(&self.geom) {
                Some(i) if i < len && i != self.cursor => {
                    self.cursor = i;
                    (ListMsg::None, Some(MenuPulse::Move))
                }
                _ => (ListMsg::None, None),
            },
            PointerKind::Press => match p.pick(&self.geom) {
                Some(i) if i < len => {
                    self.cursor = i;
                    // Same forward cycle as A, so a click matches a pad press.
                    self.armed(1);
                    self.dip();
                    (ListMsg::Activate, Some(MenuPulse::Confirm))
                }
                _ => (ListMsg::None, None),
            },
            _ => (ListMsg::None, None),
        }
    }

    /// A finger drag: the rows follow it and a lift flings them. `false` when the drag
    /// did not start on the list, so it scrolls by ticks instead.
    pub fn pan(&mut self, p: Pointer) -> bool {
        let taken = self.tree.get_mut().drag(Id::new(LIST, 0), p);
        if taken && matches!(p.kind, PointerKind::PanStart { .. }) {
            self.follow = false;
        }
        taken
    }

    fn step(&mut self, delta: i32, len: usize) -> Option<MenuPulse> {
        self.follow = true;
        // Rows can shrink under the cursor (another writer forgot a host): step from the last.
        self.cursor = self.cursor.min(len.saturating_sub(1));
        let target = self.cursor as i32 + delta;
        if len == 0 || target < 0 || target >= len as i32 {
            // End of the list: Boundary pulse plus a rubbery vertical recoil.
            self.bump = Spring {
                pos: self.bump.pos,
                vel: -BUMP_V * f64::from(delta.signum()),
            };
            return Some(MenuPulse::Boundary);
        }
        self.cursor = target as usize;
        Some(MenuPulse::Move)
    }

    /// Draw the rows in `rect`. `active` is false when a keyboard tray parks
    /// focus: rows keep their look, the focus ring rests.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        rows: &[RowSpec],
        fonts: &Fonts,
        k: f64,
        dt: f64,
        active: bool,
    ) {
        let reduce = crate::theme::reduce_motion();
        // Own clock: `render` gets `dt` but never the shell's `t`.
        self.age += dt;
        if !self.entrance_armed {
            self.entrance_armed = true;
            self.entrance = Some(Entrance::new(entrances::ROWS, self.cursor, self.age));
        }
        if self.entrance.is_some_and(|e| e.done(self.age)) {
            self.entrance = None;
        }
        if self.snap {
            // Replaced rows share no history: snap focus, drop slip. A value that
            // "changed" because the row set swapped is not a step.
            self.focus.clear();
            self.shown.clear();
            self.slip = Spring::rest(0.0);
            self.slip_prev = None;
            self.step_dir = 0;
        }
        self.focus.resize(rows.len(), 0.0);
        if self.snap || self.knobs.len() != rows.len() {
            self.knobs.clear();
            self.knobs.extend(rows.iter().map(|r| match r.control {
                Control::Toggle(on) => f64::from(u8::from(on)),
                _ => 0.0,
            }));
        }
        for (knob, row) in self.knobs.iter_mut().zip(rows) {
            if let Control::Toggle(on) = row.control {
                let target = f64::from(u8::from(on));
                *knob = if reduce {
                    target
                } else {
                    approach(*knob, target, dt, 0.05)
                };
                if (*knob - target).abs() < 0.005 {
                    *knob = target;
                }
            }
        }
        for (i, f) in self.focus.iter_mut().enumerate() {
            let target = if active && i == self.cursor { 1.0 } else { 0.0 };
            *f = if self.snap {
                target
            } else {
                approach(*f, target, dt, 0.06)
            };
            // `approach` never arrives; land once the delta is imperceptible so we can settle.
            if (*f - target).abs() < 0.002 {
                *f = target;
            }
        }
        self.bump.step(0.0, BUMP_K, BUMP_C, dt);
        self.bump.settle(0.0, 0.3, 4.0);
        if reduce {
            // Reduced motion: keep the Boundary haptic, drop the recoil travel.
            self.bump = Spring::rest(0.0);
        }

        // Arm slip only if `step_dir` is set AND this row is `adjustable` AND the
        // drawn value actually changed. Take `step_dir` anyway so it cannot leak
        // into a later frame. A and pointer press arm regardless of row type, so
        // a re-read value (host count) would otherwise slide with no chevrons.
        let dir = std::mem::take(&mut self.step_dir);
        let stepped = if dir != 0 && !reduce {
            rows.get(self.cursor).filter(|r| r.adjustable)
        } else {
            None
        };
        if let Some(row) = stepped {
            let now = row.value.as_deref().unwrap_or_default();
            if self.shown.get(self.cursor).is_some_and(|p| p != now) {
                let prev = self.shown[self.cursor].clone();
                // Add, never reset `vel`: two fast presses become one accelerating travel.
                self.slip.pos =
                    (self.slip.pos + SLIP_DP * f64::from(dir)).clamp(-SLIP_MAX, SLIP_MAX);
                // Exact cancel of in-flight slip: no travel, no fade span — new value takes the row.
                self.slip_prev = if self.slip.pos == 0.0 {
                    None
                } else {
                    Some(SlipPrev {
                        row: self.cursor,
                        label: row.label.clone(),
                        text: prev,
                        offset: -SLIP_DP * f64::from(dir),
                        arm: self.slip.pos,
                    })
                };
            }
        }
        self.slip.step_spec(0.0, springs::FOCUS, dt);
        self.slip.settle(0.0, 0.02, 0.2);
        if self.slip.pos == 0.0 {
            self.slip_prev = None;
        }
        // Record every row's value, including culled ones: a stale off-screen
        // value would later fake a change.
        self.shown.clear();
        self.shown
            .extend(rows.iter().map(|r| r.value.clone().unwrap_or_default()));

        // Rows are cells in a scroll column, `ROW_GAP` apart, a header band above a
        // sectioned row. Each cell's painter draws the row as it always has.
        let list = Id::new(LIST, 0);
        let col = column(rect, k);
        let row_w = f64::from(col.width());
        let dot_gutter = if rows.iter().any(|r| r.dot) {
            16.0 * k
        } else {
            0.0
        };
        let snap = std::mem::take(&mut self.snap);
        self.tree.get_mut().tick(dt as f32);
        // The viewport holds the plate's outset around the column; rows never reach the
        // chrome past it.
        let air = (PLATE_AIR * k) as f32;
        // Bleeding, the viewport reaches the layer's edges and pads back to `rect`; else it
        // holds the plate's outset. Without a band to treat them, rows stay in their rect.
        let bleed = self.bleed && crate::blur::active();
        let view = if bleed {
            let clip = canvas.local_clip_bounds().unwrap_or(rect);
            Rect::from_ltrb(
                rect.left - air,
                clip.top.min(rect.top - air),
                rect.right,
                clip.bottom.max(rect.bottom + air),
            )
        } else {
            Rect::from_ltrb(
                rect.left - air,
                rect.top - air,
                rect.right,
                rect.bottom + air,
            )
        };
        let (pad_top, pad_bottom) = (rect.top - view.top, view.bottom - rect.bottom);
        let this = &*self;
        let root = El::scroll(list, Axis::Vertical)
            .gap((ROW_GAP * k) as f32)
            .style(|s| {
                s.align_items = Some(taffy::AlignItems::START);
                s.padding.left = taffy::LengthPercentage::length(air + col.left - rect.left);
                s.padding.top = taffy::LengthPercentage::length(pad_top);
                s.padding.bottom = taffy::LengthPercentage::length(pad_bottom);
            })
            .children(rows.iter().enumerate().map(|(i, row)| {
                let cell = El::paint(move |canvas, cell| {
                    this.paint_row(canvas, fonts, i, row, cell, k, dot_gutter);
                })
                .id(row_id(i))
                .focusable((14.0 * k) as f32)
                .size(row_w as f32, (ROW_H * k) as f32);
                match row.header {
                    Some(_) => cell.style(|s| {
                        s.margin.top = taffy::LengthPercentageAuto::length((HEADER_H * k) as f32);
                    }),
                    None => cell,
                }
            }));
        let mut tree = this.tree.borrow_mut();
        let frame = tree.layout(root, view);
        // Centre the focused row in `rect`, eased, while the list follows focus and no finger
        // has it.
        let (_, max) = frame.scroll(list).expect("the list is a scroll");
        let target = frame
            .rect(row_id(this.cursor))
            .map_or(0.0, |r| (r.center_y() - rect.center_y()).clamp(0.0, max));
        let following = this.follow && !tree.moving(list);
        if following {
            let next = if snap {
                target
            } else {
                approach(f64::from(tree.offset(list)), f64::from(target), dt, 0.08) as f32
            };
            let next = if (next - target).abs() < 0.25 {
                target
            } else {
                next
            };
            tree.set_offset(list, next);
        }
        let scroll_settled = !tree.moving(list) && (!following || tree.offset(list) == target);
        tree.set_focus(active.then(|| row_id(this.cursor)));
        if bleed {
            tree.paint_focus(canvas, frame, k as f32, dt, false);
        } else {
            let scrolled = (tree.offset(list), max);
            soft_scroll(canvas, view, rect, scrolled, k, || {
                tree.paint_focus(canvas, frame, k as f32, dt, false);
            });
        }
        drop(tree);

        // What a pointer hits: the cells as painted, not the drawing's ease.
        let tree = self.tree.get_mut();
        self.geom = (0..rows.len())
            .map(|i| tree.rect(row_id(i)).unwrap_or_else(Rect::new_empty))
            .collect();
        self.buttons_geom = rows
            .iter()
            .zip(&self.geom)
            .map(|(row, cell)| match cell.is_empty() {
                true => Vec::new(),
                false => button_rects(*cell, row, k).collect(),
            })
            .collect();
        self.tracks_geom = rows
            .iter()
            .zip(&self.geom)
            .map(|(row, cell)| match cell.is_empty() {
                true => Rect::new_empty(),
                false => track_rect(*cell, row, k, dot_gutter),
            })
            .collect();

        let cursor = if active { Some(self.cursor) } else { None };
        let focus_target = |i: usize| if Some(i) == cursor { 1.0 } else { 0.0 };
        self.settled = self.entrance.is_none()
            && self
                .focus
                .iter()
                .enumerate()
                .all(|(i, f)| *f == focus_target(i))
            && !self.tree.get_mut().plate_busy()
            && self.bump.pos == 0.0
            && self.bump.vel == 0.0
            && self.slip.pos == 0.0
            && scroll_settled
            && self
                .knobs
                .iter()
                .zip(rows)
                .all(|(knob, r)| !matches!(r.control, Control::Toggle(on) if *knob != f64::from(u8::from(on))));
    }

    /// One row at `cell`, its laid-out rect. Bump, entrance rise and focus scale move
    /// the drawing, never the rect a pointer hits.
    #[allow(clippy::too_many_arguments)]
    fn paint_row(
        &self,
        canvas: &Canvas,
        fonts: &Fonts,
        i: usize,
        row: &RowSpec,
        cell: Rect,
        k: f64,
        dot_gutter: f64,
    ) {
        let f = self.focus[i];
        let (x0, row_w) = (f64::from(cell.left), f64::from(cell.width()));
        let ent = self
            .entrance
            .map_or(EntranceAt::SETTLED, |e| e.at(i, self.age));
        // The recoil whips: rows nearer the cursor move most, the far end least, so the
        // list compresses like a spring rather than shifting as a block.
        let whip = 1.0 / (1.0 + 0.12 * (i as f64 - self.cursor as f64).abs());
        let top =
            f64::from(cell.top) + self.bump.pos * k * whip + (1.0 - ent.travel) * ROW_RISE * k;
        if let Some(header) = row.header {
            fonts.draw_tracked(
                canvas,
                &header.to_uppercase(),
                x0 + 16.0 * k,
                top - HEADER_RISE * k,
                W::SemiBold,
                12.0 * k,
                1.4 * k,
                fg(0.45),
            );
        }
        let cy = top + ROW_H * k / 2.0;
        canvas.save();
        // Per-row layer only while arriving (panel + two text runs). Bounds
        // are the row rect so this is never a full-screen pass.
        let fading = ent.fade < 1.0;
        if fading {
            let bounds = Rect::from_xywh(x0 as f32, top as f32, row_w as f32, (ROW_H * k) as f32);
            crate::theme::save_layer_alpha(canvas, bounds, ent.fade as f32);
        }
        let r = Rect::from_xywh(x0 as f32, top as f32, row_w as f32, (ROW_H * k) as f32);
        // The field being typed into keeps its accent; focus is the plate behind the row.
        let (stroke, tint) = if row.caret {
            (PanelStroke::Brand(0.7), Some(accent(0.30)))
        } else {
            (PanelStroke::Plain(0.08), None)
        };
        crate::theme::panel(canvas, r, 14.0, tint, stroke, k as f32);

        // A note under the label lifts the label; the two share the row's height. The value
        // stays on the row's centre line with the switch and the chevrons — lifted with the
        // label it sat visibly above them on every row that carries a note.
        let centred = cy + 16.0 * k * 0.36;
        let baseline = if row.note.is_some() {
            cy - 2.0 * k
        } else {
            centred
        };
        let label_x = paint_leading(canvas, row, x0 + 16.0 * k, cy, k);
        // Trailing buttons take the row's right end; the value field ends before them.
        let row_w = row_w - paint_buttons(canvas, row, r, cy, f, k);
        // The dot marks the row's value, so it reads as part of that group: outboard of
        // the value, inboard of the buttons. The gutter is reserved on every row, or one
        // marked row pulls its own value in past its neighbours'.
        if row.dot {
            canvas.draw_circle(
                ((x0 + row_w - 12.0 * k) as f32, cy as f32),
                (4.0 * k) as f32,
                &fill(accent(1.0)),
            );
        }
        let row_w = row_w - dot_gutter;
        if let Some(note) = &row.note {
            fonts.draw_clipped(
                canvas,
                note,
                label_x,
                cy + 14.0 * k,
                W::Regular,
                11.5 * k,
                fg(0.5),
                row_w * 0.6,
            );
        }
        let off = if row.value.is_none() { 0.35 } else { 0.55 };
        fonts.draw(
            canvas,
            &row.label,
            label_x,
            baseline,
            W::SemiBold,
            16.0 * k,
            row_tone(row, fg(1.0), fg(off)),
        );
        let fr = RowFrame {
            r,
            x0,
            w: row_w,
            cy,
            centred,
            f,
            k,
            dot_gutter,
        };
        if row.value.is_some() {
            match row.control {
                Control::Toggle(_) => self.paint_switch(canvas, i, row, &fr),
                _ => self.paint_value(canvas, fonts, i, row, &fr),
            }
        }
        if fading {
            canvas.restore(); // entrance layer
        }
        canvas.restore();
    }

    /// The switch: a 36×20 track at the value field's right end, the knob eased across
    /// it, accent when on.
    fn paint_switch(&self, canvas: &Canvas, i: usize, row: &RowSpec, fr: &RowFrame) {
        let RowFrame {
            x0,
            w: row_w,
            cy,
            k,
            ..
        } = *fr;
        let knob = self.knobs.get(i).copied().unwrap_or(0.0);
        let (tw, th) = (36.0 * k, 20.0 * k);
        let track = Rect::from_xywh(
            (x0 + row_w - 16.0 * k - tw) as f32,
            (cy - th / 2.0) as f32,
            tw as f32,
            th as f32,
        );
        let on_alpha = if row.enabled { 1.0 } else { 0.35 };
        let track_color = skia_safe::Color4f::new(
            accent(1.0).r * knob as f32 + fg(0.25).r * (1.0 - knob as f32),
            accent(1.0).g * knob as f32 + fg(0.25).g * (1.0 - knob as f32),
            accent(1.0).b * knob as f32 + fg(0.25).b * (1.0 - knob as f32),
            (0.25 + 0.75 * knob as f32) * on_alpha,
        );
        canvas.draw_rrect(
            RRect::new_rect_xy(track, th as f32 / 2.0, th as f32 / 2.0),
            &fill(track_color),
        );
        let kx = track.left as f64 + th / 2.0 + knob * (tw - th);
        canvas.draw_circle(
            (kx as f32, cy as f32),
            (th / 2.0 - 3.0 * k) as f32,
            &fill(skia_safe::Color4f::new(1.0, 1.0, 1.0, on_alpha)),
        );
    }

    /// The value field: slider track, the right-aligned readout with its slip crossfade,
    /// the caret and the chevrons.
    fn paint_value(&self, canvas: &Canvas, fonts: &Fonts, i: usize, row: &RowSpec, fr: &RowFrame) {
        let RowFrame {
            r,
            x0,
            w: row_w,
            cy,
            centred,
            f,
            k,
            dot_gutter,
        } = *fr;
        let value = row.value.as_deref().unwrap_or_default();
        let vcolor = if row.danger {
            crate::theme::ERROR
        } else if row.value_dim || !row.enabled {
            fg(0.35)
        } else if f > 0.5 {
            fg(1.0)
        } else {
            fg(0.6 + 0.4 * f as f32)
        };
        let chevron_w = if row.adjustable { 18.0 * k } else { 0.0 };
        let caret_w = if row.caret { 8.0 * k } else { 0.0 };
        // A slider's track sits inside the value field, the readout to its left.
        let track_w = if let Control::Slider(_) = row.control {
            120.0 * k
        } else {
            0.0
        };
        if let Control::Slider(frac) = row.control {
            let th = 6.0 * k;
            let track = track_rect(r, row, k, dot_gutter);
            canvas.draw_rrect(
                RRect::new_rect_xy(track, th as f32 / 2.0, th as f32 / 2.0),
                &fill(fg(0.18)),
            );
            let filled =
                Rect::from_xywh(track.left, track.top, track.width() * frac, track.height());
            canvas.draw_rrect(
                RRect::new_rect_xy(filled, th as f32 / 2.0, th as f32 / 2.0),
                &fill(if row.enabled { accent(1.0) } else { fg(0.35) }),
            );
        }
        // Each string right-aligns on its own measured width against a
        // fixed right edge. Sharing the incoming string's anchor left-
        // aligns the outgoing one by the width delta and hangs it past
        // the field.
        let vmax = row_w * 0.55 - track_w;
        let val_right = x0 + row_w
            - 16.0 * k
            - chevron_w
            - caret_w
            - track_w
            - if track_w > 0.0 { 12.0 * k } else { 0.0 };
        let place = |s: &str| val_right - f64::from(fonts.measure(s, W::Medium, 15.0 * k));
        // Gate on index AND label: an index is not identity across a rebuild.
        let slipping = self
            .slip_prev
            .as_ref()
            .filter(|p| p.row == i && p.label == row.label);
        let dx = if slipping.is_some() {
            self.slip.pos * k
        } else {
            0.0
        };
        // Same row-gate as `dx`: one slip spring for the list, so an
        // ungated fade blanks every value. Signed, not `.abs()`, so
        // overshoot through zero does not fade the ghost back in.
        let gone = slipping.map_or(0.0, |p| (self.slip.pos / p.arm).clamp(0.0, 1.0) as f32);
        let alpha = |c: skia_safe::Color4f, a: f32| skia_safe::Color4f::new(c.r, c.g, c.b, c.a * a);
        // Truncate the head before placing: right-align the drawn string.
        // Measuring the untruncated one floats long values short of the edge.
        let shown = truncate_head(fonts, value, W::Medium, 15.0 * k, vmax);
        // Clip the field only while slipping: the list clip is the full
        // window, and a settled value already fits.
        if slipping.is_some() {
            canvas.save();
            canvas.clip_rect(
                Rect::from_ltrb((val_right - vmax) as f32, r.top, val_right as f32, r.bottom),
                None,
                true,
            );
        }
        if let Some(p) = slipping {
            let prev_text = truncate_head(fonts, &p.text, W::Medium, 15.0 * k, vmax);
            fonts.draw(
                canvas,
                &prev_text,
                place(&prev_text) + dx + p.offset * k,
                centred,
                W::Medium,
                15.0 * k,
                alpha(vcolor, gone),
            );
        }
        fonts.draw(
            canvas,
            &shown,
            place(&shown) + dx,
            centred,
            W::Medium,
            15.0 * k,
            alpha(vcolor, 1.0 - gone),
        );
        if slipping.is_some() {
            canvas.restore(); // value-field clip
        }
        if row.caret {
            // Ride `dx` so the caret stays on the text end mid-slip.
            canvas.draw_rect(
                Rect::from_xywh(
                    (val_right + 3.0 * k + dx) as f32,
                    (cy - 9.0 * k) as f32,
                    (2.0 * k) as f32,
                    (18.0 * k) as f32,
                ),
                &fill(accent(1.0)),
            );
        }
        if row.adjustable && f > 0.01 {
            let alpha = 0.6 * f as f32;
            // After, outside the field clip: a moving value passes under the chevrons.
            chevron(canvas, place(&shown) - 11.0 * k, cy, 4.0 * k, true, alpha);
            chevron(canvas, x0 + row_w - 16.0 * k, cy, 4.0 * k, false, alpha);
        }
    }
}

/// A row's drawing frame: the plate `r`, the value field `x0 .. x0 + w` (inboard of the
/// trailing buttons and the dot gutter), the centre line `cy`, the value baseline `centred`,
/// focus `f`, scale `k` and the list's dot gutter.
#[derive(Clone, Copy)]
struct RowFrame {
    r: Rect,
    x0: f64,
    w: f64,
    cy: f64,
    centred: f64,
    f: f64,
    k: f64,
    dot_gutter: f64,
}

/// A row's tone: the error tone on a danger row, `on` while enabled, `off` otherwise.
fn row_tone(row: &RowSpec, on: skia_safe::Color4f, off: skia_safe::Color4f) -> skia_safe::Color4f {
    if row.danger {
        crate::theme::ERROR
    } else if row.enabled {
        on
    } else {
        off
    }
}

/// Leading marks, a Lucide icon then the drag grip, from `label_x`; returns where the label starts.
fn paint_leading(canvas: &Canvas, row: &RowSpec, mut label_x: f64, cy: f64, k: f64) -> f64 {
    if let Some(icon) = row.icon.and_then(crate::icons::by_name) {
        crate::icons::draw_icon(
            canvas,
            icon,
            (label_x + 10.0 * k) as f32,
            cy as f32,
            (20.0 * k) as f32,
            row_tone(row, fg(0.85), fg(0.4)),
        );
        label_x += 32.0 * k;
    }
    if row.handle {
        if let Some(grip) = crate::icons::by_name("grip-vertical") {
            let grip_tone = if row.held {
                accent(1.0)
            } else {
                row_tone(row, fg(0.7), fg(0.3))
            };
            crate::icons::draw_icon(
                canvas,
                grip,
                (label_x + 8.0 * k) as f32,
                cy as f32,
                (20.0 * k) as f32,
                grip_tone,
            );
        }
        label_x += 28.0 * k;
    }
    label_x
}

/// Round icon buttons at the row's right end; returns the width they take.
fn paint_buttons(canvas: &Canvas, row: &RowSpec, r: Rect, cy: f64, f: f64, k: f64) -> f64 {
    let buttons_w = row.buttons.len() as f64 * BUTTON_PITCH * k;
    for (j, (name, b)) in row.buttons.iter().zip(button_rects(r, row, k)).enumerate() {
        let (cx, d) = (f64::from(b.center_x()), BUTTON_D * k);
        let lit = row.button == Some(j) && f > 0.5;
        if lit {
            canvas.draw_circle((cx as f32, cy as f32), (d / 2.0) as f32, &fill(accent(1.0)));
        } else {
            canvas.draw_circle(
                (cx as f32, cy as f32),
                (d / 2.0) as f32,
                &fill(fg(0.04 + 0.06 * f as f32)),
            );
        }
        if let Some(icon) = crate::icons::by_name(name) {
            let icon_tone = if lit {
                crate::theme::on_accent()
            } else {
                row_tone(row, fg(0.9), fg(0.45))
            };
            crate::icons::draw_icon(
                canvas,
                icon,
                cx as f32,
                cy as f32,
                (18.0 * k) as f32,
                icon_tone,
            );
        }
    }
    buttons_w
}

fn truncate_head(fonts: &Fonts, text: &str, w: W, size: f64, max_w: f64) -> String {
    if f64::from(fonts.measure(text, w, size)) <= max_w {
        return text.to_string();
    }
    let mut s: Vec<char> = text.chars().collect();
    while s.len() > 1 {
        s.remove(0);
        let candidate: String = std::iter::once('…').chain(s.iter().copied()).collect();
        if f64::from(fonts.measure(&candidate, w, size)) <= max_w {
            return candidate;
        }
    }
    "…".into()
}

fn chevron(canvas: &Canvas, x: f64, cy: f64, r: f64, left: bool, alpha: f32) {
    let dir = if left { -1.0 } else { 1.0 };
    let mut p = stroke(fg(alpha), (1.8 * r / 4.0) as f32);
    p.set_stroke_cap(skia_safe::PaintCap::Round);
    let mut path = PathBuilder::new();
    path.move_to(((x - dir * r / 2.0) as f32, (cy - r) as f32));
    path.line_to(((x + dir * r / 2.0) as f32, cy as f32));
    path.line_to(((x - dir * r / 2.0) as f32, (cy + r) as f32));
    canvas.draw_path(&path.detach(), &p);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_list_moves_and_recoils() {
        let mut l = MenuList::new();
        assert!(matches!(
            l.menu(MenuEvent::Move(MenuDir::Down), 3).1,
            Some(MenuPulse::Move)
        ));
        assert_eq!(l.cursor, 1);
        assert!(matches!(
            l.menu(MenuEvent::Move(MenuDir::Up), 3).1,
            Some(MenuPulse::Move)
        ));
        assert!(matches!(
            l.menu(MenuEvent::Move(MenuDir::Up), 3).1,
            Some(MenuPulse::Boundary)
        ));
        assert!(l.bump.vel.abs() > 1.0, "recoil engaged");
        assert_eq!(
            l.menu(MenuEvent::Move(MenuDir::Right), 3).0,
            ListMsg::Adjust(1)
        );
        assert_eq!(l.menu(MenuEvent::Confirm, 3).0, ListMsg::Activate);
    }

    fn value_row(value: &str) -> Vec<RowSpec> {
        vec![RowSpec {
            label: "Bitrate".into(),
            value: Some(value.into()),
            adjustable: true,
            ..RowSpec::default()
        }]
    }

    type Band = (usize, (i32, i32), (i32, i32));

    /// A column of steppable rows. A one-row list cannot catch a crossfade that
    /// leaks: the slipping row would be the only row.
    fn value_rows(values: &[&str]) -> Vec<RowSpec> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| RowSpec {
                label: format!("Option {i}"),
                value: Some((*v).to_string()),
                adjustable: true,
                ..RowSpec::default()
            })
            .collect()
    }

    /// BGRA bytes whatever the platform's n32 is: Apple's Skia is RGBA.
    fn read_back(surface: &mut skia_safe::Surface, w: i32, h: i32) -> Vec<u8> {
        let mut px = vec![0u8; (w * h * 4) as usize];
        let info = skia_safe::ImageInfo::new(
            (w, h),
            skia_safe::ColorType::BGRA8888,
            skia_safe::AlphaType::Premul,
            None,
        );
        assert!(
            surface.read_pixels(&info, &mut px, (w * 4) as usize, (0, 0)),
            "raster surface read-back"
        );
        px
    }

    /// Differing bytes in one band. A count: `assert_eq!` on a megapixel prints a megapixel.
    fn band_diff(a: &[u8], b: &[u8], w: i32, x: (i32, i32), y: (i32, i32)) -> usize {
        let mut differing = 0;
        for row in y.0..y.1 {
            let base = (row * w * 4) as usize;
            let span = base + x.0 as usize * 4..base + x.1 as usize * 4;
            differing += a[span.clone()]
                .iter()
                .zip(&b[span])
                .filter(|(p, q)| p != q)
                .count();
        }
        differing
    }

    /// Rows shrink under a cursor when another writer forgets a host.
    #[test]
    fn a_cursor_past_a_shrunk_list_steps_back_onto_it() {
        let mut list = MenuList::new();
        list.jump_to(4);
        let (_, pulse) = list.menu(MenuEvent::Move(MenuDir::Up), 2);
        assert!(matches!(pulse, Some(MenuPulse::Move)));
        assert_eq!(list.cursor, 0);
    }

    /// Stepping one row must leave every other row's pixels unchanged. One
    /// slip spring for the list: ungated alpha blanks the whole value column.
    #[test]
    fn stepping_one_value_leaves_the_other_rows_alone() {
        let fonts = crate::theme::build_fonts().unwrap();
        let (w, h) = (900, 600);
        let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
        let rect = Rect::from_xywh(0.0, 0.0, w as f32, h as f32);
        let clear = skia_safe::Color4f::new(0.0, 0.0, 0.0, 1.0);
        let dt = 1.0 / 60.0;
        let before = value_rows(&["Native", "Automatic", "20 Mbps", "Balanced", "On", "Off"]);
        let mut list = MenuList::new();
        // Settle first: entrance, scroll, and focus travel on their own; the claim is nothing else moves.
        for _ in 0..240 {
            surface.canvas().clear(clear);
            list.render(surface.canvas(), rect, &before, &fonts, 1.0, dt, true);
        }
        let bands: Vec<Band> = (1..before.len())
            .map(|i| {
                let r = list.row_rect(i).expect("every row is on screen");
                (
                    i,
                    (r.left as i32, r.right as i32),
                    (r.top as i32, r.bottom as i32),
                )
            })
            .collect();
        let settled = read_back(&mut surface, w, h);

        // Probe: a different value in this band must actually differ, so the
        // equality below is not two identical patches of background.
        let mut probe = MenuList::new();
        let probed = value_rows(&["Native", "Automatic", "20 Mbps", "Native", "On", "Off"]);
        for _ in 0..240 {
            surface.canvas().clear(clear);
            probe.render(surface.canvas(), rect, &probed, &fonts, 1.0, dt, true);
        }
        let probe_px = read_back(&mut surface, w, h);
        let (_, px_x, px_y) = bands[2];
        assert!(
            band_diff(&probe_px, &settled, w, px_x, px_y) > 0,
            "row 3's band must contain row 3's value"
        );

        assert_eq!(
            list.menu(MenuEvent::Move(MenuDir::Right), before.len()).0,
            ListMsg::Adjust(1)
        );
        let after = value_rows(&[
            "Match window",
            "Automatic",
            "20 Mbps",
            "Balanced",
            "On",
            "Off",
        ]);
        let mut armed = false;
        for frame in 0..12 {
            surface.canvas().clear(clear);
            list.render(surface.canvas(), rect, &after, &fonts, 1.0, dt, true);
            armed |= list.slip_prev.is_some();
            let px = read_back(&mut surface, w, h);
            for (i, bx, by) in &bands {
                assert_eq!(
                    band_diff(&px, &settled, w, *bx, *by),
                    0,
                    "row {i} redrew on frame {frame} of a step made on row 0"
                );
            }
        }
        assert!(armed, "the step must have animated, or this proves nothing");
    }

    /// A stepped value stays inside its field, however wide the value it leaves.
    /// Asserted on the band outside the field (chevron, panel edge): that ink
    /// is fixed once settled, so any change there is escaped text.
    #[test]
    fn a_stepped_value_stays_inside_its_field() {
        // This one needs motion, and `reduce_motion` is a thread-local: a harness that gives every
        // test its own thread hides that, a single-threaded one (wasm) hands over whatever the
        // last shell render left. Say what this test needs rather than inherit it.
        crate::theme::set_reduce_motion(false);
        let fonts = crate::theme::build_fonts().unwrap();
        let (w, h) = (900, 600);
        let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
        let rect = Rect::from_xywh(0.0, 0.0, w as f32, h as f32);
        let clear = skia_safe::Color4f::new(0.0, 0.0, 0.0, 1.0);
        let dt = 1.0 / 60.0;
        let mut list = MenuList::new();
        let wide = value_row("PyroWave (wired LAN)");
        for _ in 0..240 {
            surface.canvas().clear(clear);
            list.render(surface.canvas(), rect, &wide, &fonts, 1.0, dt, true);
        }
        let r = list.row_rect(0).expect("the row is on screen");
        // Field right edge as `render` computes it (16 dp gutter + 18 dp chevrons, k = 1). Two px of clip slack.
        let field_right = (f64::from(r.right) - 16.0 - 18.0).ceil() as i32;
        let outside = (field_right + 2, w);
        let band_y = (r.top as i32, r.bottom.ceil() as i32);
        let settled = read_back(&mut surface, w, h);

        list.menu(MenuEvent::Move(MenuDir::Right), 1);
        let narrow = value_row("AV1");
        let mut armed = false;
        for frame in 0..16 {
            surface.canvas().clear(clear);
            list.render(surface.canvas(), rect, &narrow, &fonts, 1.0, dt, true);
            armed |= list.slip_prev.is_some();
            let px = read_back(&mut surface, w, h);
            assert_eq!(
                band_diff(&px, &settled, w, outside, band_y),
                0,
                "value ink escaped the field on frame {frame}"
            );
        }
        assert!(armed, "the step must have animated, or this proves nothing");
    }

    /// Trailing buttons sit at the row's right end, in the order given, and the pointer
    /// picks them by index; the lit one is accent-filled. A handle shifts the label.
    #[test]
    fn trailing_buttons_are_hit_in_order_and_light_when_focused() {
        crate::theme::set_ink(crate::theme::Ink::of(crate::palette::palette("violet")));
        let fonts = crate::theme::build_fonts().unwrap();
        let (w, h) = (900, 400);
        let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
        let rect = Rect::from_xywh(0.0, 0.0, w as f32, h as f32);
        let rows = vec![
            RowSpec::choice("Favourites", "3 games")
                .with_handle(true)
                .with_buttons(&["pencil", "trash-2"], Some(1)),
            RowSpec::action("Add collection", true),
        ];
        let mut list = MenuList::new();
        for _ in 0..90 {
            surface
                .canvas()
                .clear(skia_safe::Color4f::new(0.0, 0.0, 0.0, 1.0));
            list.render(surface.canvas(), rect, &rows, &fonts, 1.0, 1.0 / 60.0, true);
        }
        let r0 = list.row_rect(0).unwrap();
        let pencil = list.buttons_geom[0][0];
        let trash = list.buttons_geom[0][1];
        assert!(pencil.right < trash.left && trash.right <= r0.right);
        let at = |r: Rect| Pointer {
            x: f64::from(r.center_x()),
            y: f64::from(r.center_y()),
            kind: PointerKind::Move,
        };
        assert_eq!(list.button_at(at(pencil)), Some((0, 0)));
        assert_eq!(list.button_at(at(trash)), Some((0, 1)));
        assert_eq!(list.button_at(at(list.row_rect(1).unwrap())), None);
        assert!(list.track_frac(0, 0.0).is_none(), "no track on a value row");
        // The lit button is accent (blue-heavy in BGRA), the other is not.
        let buf = read_back(&mut surface, w, h);
        let px = |r: Rect| {
            let (x, y) = (r.center_x() as i32 - 10, r.center_y() as i32 - 10);
            let i = ((y * w + x) * 4) as usize;
            [buf[i], buf[i + 1], buf[i + 2]]
        };
        assert!(
            px(trash)[0] > 150 && px(trash)[1] < 150,
            "lit: {:?}",
            px(trash)
        );
        assert!(
            px(trash)[0] > px(pencil)[0] + 60,
            "unlit is dimmer: {:?} vs {:?}",
            px(pencil),
            px(trash)
        );
    }

    /// A slider row records its track: the pointer's x maps to 0..=1 along it and a point
    /// in the row band over the track counts as on it.
    #[test]
    fn slider_tracks_are_seekable_by_the_pointer() {
        crate::theme::set_ink(crate::theme::Ink::of(crate::palette::palette("violet")));
        let fonts = crate::theme::build_fonts().unwrap();
        let (w, h) = (900, 300);
        let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
        let rect = Rect::from_xywh(0.0, 0.0, w as f32, h as f32);
        let rows = vec![RowSpec::slider("Bitrate", "40 Mb/s", 0.5)];
        let mut list = MenuList::new();
        for _ in 0..30 {
            list.render(surface.canvas(), rect, &rows, &fonts, 1.0, 1.0 / 60.0, true);
        }
        let track = list.tracks_geom[0];
        assert!(track.width() > 100.0);
        let at = |x: f32, y: f32| Pointer {
            x: f64::from(x),
            y: f64::from(y),
            kind: PointerKind::Press,
        };
        assert!(list.on_track(0, at(track.center_x(), track.top - 12.0)));
        assert!(!list.on_track(0, at(track.left - 40.0, track.center_y())));
        let f = list
            .track_frac(0, f64::from(track.left + track.width() * 0.25))
            .unwrap();
        assert!((f - 0.25).abs() < 0.02, "{f}");
        assert_eq!(list.track_frac(0, -10.0), Some(0.0));
    }

    /// The pointer shells' controls: a switch's knob crosses its track when the row flips
    /// and lands accent-lit; a slider's fill follows its fraction; an icon and a note draw
    /// beside the label without leaving the row.
    #[test]
    fn toggle_and_slider_rows_draw_their_controls() {
        crate::theme::set_ink(crate::theme::Ink::of(crate::palette::palette("violet")));
        let fonts = crate::theme::build_fonts().unwrap();
        let (w, h) = (900, 600);
        let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
        let rect = Rect::from_xywh(0.0, 0.0, w as f32, h as f32);
        let clear = skia_safe::Color4f::new(0.0, 0.0, 0.0, 1.0);
        let dt = 1.0 / 60.0;
        let rows = |on: bool, frac: f32| {
            vec![
                RowSpec::toggle("HDR", on)
                    .with_icon("sun")
                    .with_note("10-bit, BT.2020 PQ"),
                RowSpec::slider("Bitrate", "40 Mb/s", frac),
                RowSpec::choice("Codec", "HEVC"),
            ]
        };
        let mut list = MenuList::new();
        for _ in 0..120 {
            surface.canvas().clear(clear);
            list.render(
                surface.canvas(),
                rect,
                &rows(false, 0.25),
                &fonts,
                1.0,
                dt,
                true,
            );
        }
        let off = read_back(&mut surface, w, h);
        let r0 = list.row_rect(0).unwrap();
        let r1 = list.row_rect(1).unwrap();
        // The knob rests at the track's left while off: that end is lit, the right end is not.
        let knob_off = (
            f64::from(r0.right) - 16.0 - 36.0 + 10.0,
            f64::from(r0.center_y()),
        );
        let knob_on = (f64::from(r0.right) - 16.0 - 10.0, f64::from(r0.center_y()));
        let px = |buf: &[u8], (x, y): (f64, f64)| {
            let i = ((y as i32 * w + x as i32) * 4) as usize;
            [buf[i], buf[i + 1], buf[i + 2]]
        };
        assert!(
            px(&off, knob_off).iter().all(|c| *c > 200),
            "knob at left while off: {:?}",
            px(&off, knob_off)
        );
        assert!(
            !px(&off, knob_on).iter().all(|c| *c > 200),
            "no knob at the right while off: {:?}",
            px(&off, knob_on)
        );

        for _ in 0..120 {
            surface.canvas().clear(clear);
            list.render(
                surface.canvas(),
                rect,
                &rows(true, 0.75),
                &fonts,
                1.0,
                dt,
                true,
            );
        }
        let on = read_back(&mut surface, w, h);
        assert!(
            px(&on, knob_on).iter().all(|c| *c > 200),
            "knob at right while on: {:?}",
            px(&on, knob_on)
        );
        // The track under the knob's old seat is now accent-coloured, not grey. `read_back`
        // is BGRA, so the accent's blue leads.
        let seat = px(&on, knob_off);
        assert!(seat[0] > seat[1] + 20, "accent track while on: {seat:?}");
        // Slider: the fill at a quarter is dark past the midpoint, lit at three quarters.
        let mid = (
            f64::from(r1.right) - 16.0 - 18.0 - 60.0,
            f64::from(r1.center_y()),
        );
        assert!(
            px(&off, mid)[0] < 110,
            "quarter fill leaves the midpoint dark: {:?}",
            px(&off, mid)
        );
        assert!(
            px(&on, mid)[0] > 150,
            "three-quarter fill lights the midpoint: {:?}",
            px(&on, mid)
        );
        // The icon sits in the gutter the label used to start in, so its box holds ink. A box,
        // not one pixel: the sun's centre is hollow and would read the row's ground.
        let gutter = (f64::from(r0.left) + 26.0, f64::from(r0.center_y()));
        let inked = (-10..=10).any(|dy| {
            (-12..=12).any(|dx| {
                let at = (gutter.0 + f64::from(dx), gutter.1 + f64::from(dy));
                px(&on, at).iter().all(|c| *c > 120)
            })
        });
        assert!(inked, "icon in the gutter: {:?}", px(&on, gutter));
    }

    /// Slip arms only when the value actually changed, and settles back to
    /// identity. A slip that never returns leaves the value permanently offset.
    #[test]
    fn value_slip_arms_on_a_real_step_and_settles_to_identity() {
        let fonts = crate::theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((900, 600)).unwrap();
        let rect = Rect::from_xywh(0.0, 0.0, 900.0, 600.0);
        let dt = 1.0 / 60.0;
        let mut list = MenuList::new();
        let mut frame = |list: &mut MenuList, rows: &[RowSpec]| {
            list.render(surface.canvas(), rect, rows, &fonts, 1.0, dt, true);
        };

        frame(&mut list, &value_row("10 Mbps"));
        assert_eq!(list.slip.pos, 0.0, "nothing has stepped yet");

        // Honoured: the next frame's value differs.
        assert_eq!(
            list.menu(MenuEvent::Move(MenuDir::Right), 1).0,
            ListMsg::Adjust(1)
        );
        frame(&mut list, &value_row("20 Mbps"));
        assert!(list.slip.pos.abs() > 1.0, "armed: {}", list.slip.pos);
        assert!(
            list.slip_prev.is_some(),
            "the old value is held for the fade"
        );

        for _ in 0..240 {
            frame(&mut list, &value_row("20 Mbps"));
        }
        assert_eq!(list.slip.pos, 0.0, "settled back onto the row");
        assert!(list.slip_prev.is_none(), "and forgot the old value");

        // Refused (value unchanged) must not move. The list detects the change; it does not trust the event.
        list.menu(MenuEvent::Move(MenuDir::Right), 1);
        frame(&mut list, &value_row("20 Mbps"));
        assert_eq!(list.slip.pos, 0.0);
        assert!(list.slip_prev.is_none());
    }

    /// Reduced motion keeps state and drops travel: new value, focused row, no recoil.
    #[test]
    fn reduce_motion_drops_travel_but_not_state() {
        let fonts = crate::theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((900, 600)).unwrap();
        let rect = Rect::from_xywh(0.0, 0.0, 900.0, 600.0);
        let dt = 1.0 / 60.0;
        crate::theme::set_reduce_motion(true);
        let mut list = MenuList::new();
        list.render(
            surface.canvas(),
            rect,
            &value_row("10 Mbps"),
            &fonts,
            1.0,
            dt,
            true,
        );
        list.menu(MenuEvent::Move(MenuDir::Right), 1);
        list.render(
            surface.canvas(),
            rect,
            &value_row("20 Mbps"),
            &fonts,
            1.0,
            dt,
            true,
        );
        assert_eq!(list.slip.pos, 0.0, "no slip under reduced motion");
        assert!(list.slip_prev.is_none());
        // Refused move still pulses; no recoil travel.
        assert!(matches!(
            list.menu(MenuEvent::Move(MenuDir::Up), 1).1,
            Some(MenuPulse::Boundary)
        ));
        list.render(
            surface.canvas(),
            rect,
            &value_row("20 Mbps"),
            &fonts,
            1.0,
            dt,
            true,
        );
        assert_eq!(list.bump.pos, 0.0, "recoil travel suppressed");
        // Focus still arrives: reduced motion is not unfocused.
        assert_eq!(list.focus[0], 1.0);
        crate::theme::set_reduce_motion(false);
    }

    /// Mount entrance arms once, retires when done, and is not replayed by a
    /// tab switch (re-fanning on every L1/R1 would flicker a skim).
    #[test]
    fn menu_list_entrance_plays_once_and_retires() {
        let fonts = crate::theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((900, 600)).unwrap();
        let rect = Rect::from_xywh(0.0, 0.0, 900.0, 600.0);
        let dt = 1.0 / 60.0;
        let rows: Vec<RowSpec> = (0..6)
            .map(|i| RowSpec::action(format!("Row {i}"), true))
            .collect();
        let mut list = MenuList::new();

        list.render(surface.canvas(), rect, &rows, &fonts, 1.0, dt, true);
        assert!(list.entrance.is_some(), "armed on the first frame");

        for _ in 0..90 {
            list.render(surface.canvas(), rect, &rows, &fonts, 1.0, dt, true);
        }
        assert!(list.entrance.is_none(), "retired once it played out");

        list.jump_to(3);
        list.render(surface.canvas(), rect, &rows, &fonts, 1.0, dt, true);
        assert!(list.entrance.is_none(), "a tab switch must not replay it");
    }

    #[test]
    fn head_truncation_keeps_the_tail() {
        let fonts = crate::theme::build_fonts().unwrap();
        let t = truncate_head(&fonts, "verylonghostname.local", W::Medium, 15.0, 60.0);
        assert!(t.starts_with('…'));
        assert!(t.ends_with("local"));
    }
}
