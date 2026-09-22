//! Immediate-mode focus widgets: the settings menu list, the section tab strip,
//! and the controller keyboard.
//!
//! The widget owns cursor, springs, and scroll. The screen owns row content and
//! what an activation means; every frame it hands the widget a fresh `RowSpec`
//! slice. Ports of Apple's `GamepadMenuList` / `GamepadKeyboard`.
//!
//! Pixel contracts live in the tests below: a stepped value stays in its field,
//! and a slip on one row does not blank the rest of the column.

use crate::anim::{approach, entrances, springs, Entrance, EntranceAt, Spring, TRAY_C, TRAY_K};
use crate::el::{Axis, El, Id, Tree};
use crate::library::{BUMP_C, BUMP_K};
use crate::pointer::{Pointer, PointerKind};
use crate::theme::{accent, fg, fill, stroke, Fonts, PanelStroke, EDGE_INSET, W};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, Paint, PathBuilder, RRect, Rect};
use std::cell::RefCell;

// Menu list

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
    /// `None` = action row (centred label, brand tint).
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
const ROW_GAP: f64 = 6.0;
const HEADER_H: f64 = 34.0;
pub const ROW_MAX_W: f64 = 620.0;

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
const PRESS_DIP: f64 = 0.97;
/// Mount rise, design units. A twelfth of the carousel travel — same language, smaller.
const ROW_RISE: f64 = 12.0;

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
    /// Colour channel of focus (tint, alpha, chevrons), eased. Not sprung:
    /// overshoot would leave the palette.
    focus: Vec<f64>,
    /// Scale channel, sprung — the overshoot is the "picked up" pop.
    focus_pop: Vec<Spring>,
    /// Confirm dip, rest 1.0. One spring: only the focused row can be dipping.
    press: Spring,
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
            focus_pop: Vec::new(),
            press: Spring::rest(1.0),
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

    /// Confirm dip. Separate from [`Self::armed`]: an action row still presses.
    fn dip(&mut self) {
        self.press.pos = PRESS_DIP;
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
        let tree = self.tree.get_mut();
        match p.kind {
            PointerKind::PanStart { horizontal: false } => {
                let list = Id::new(LIST, 0);
                if tree.scroll_at(p.x as f32, p.y as f32, Axis::Vertical) != Some(list) {
                    return false;
                }
                tree.pan(list, 0.0);
                self.follow = false;
                true
            }
            PointerKind::Pan { dy, .. } => {
                tree.pan(Id::new(LIST, 0), -dy as f32);
                true
            }
            PointerKind::Fling { vy, .. } => {
                tree.release(Id::new(LIST, 0), -vy as f32);
                true
            }
            _ => false,
        }
    }

    fn step(&mut self, delta: i32, len: usize) -> Option<MenuPulse> {
        self.follow = true;
        let target = self.cursor as i32 + delta;
        if len == 0 || target < 0 || target >= len as i32 {
            // End of the list: Boundary pulse plus 14 dp vertical recoil.
            self.bump = Spring {
                pos: -14.0 * f64::from(delta.signum()),
                vel: 0.0,
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
            self.focus_pop.clear();
            self.shown.clear();
            self.slip = Spring::rest(0.0);
            self.slip_prev = None;
            self.step_dir = 0;
        }
        self.focus.resize(rows.len(), 0.0);
        self.focus_pop.resize(rows.len(), Spring::rest(0.0));
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
        for (i, s) in self.focus_pop.iter_mut().enumerate() {
            let target = if active && i == self.cursor { 1.0 } else { 0.0 };
            if self.snap || reduce {
                *s = Spring::rest(target);
            } else {
                s.step_spec(target, springs::FOCUS, dt);
                s.settle(target, 0.0005, 0.005);
            }
        }
        self.bump.step(0.0, BUMP_K, BUMP_C, dt);
        self.bump.settle(0.0, 0.3, 4.0);
        if reduce {
            // Reduced motion: keep the Boundary haptic, drop the recoil travel.
            self.bump = Spring::rest(0.0);
            self.press = Spring::rest(1.0);
        } else {
            self.press.step_spec(1.0, springs::PRESS, dt);
            self.press.settle(1.0, 0.0005, 0.005);
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
        let row_w = (ROW_MAX_W * k).min(f64::from(rect.width()) - 48.0 * k);
        let dot_gutter = if rows.iter().any(|r| r.dot) {
            16.0 * k
        } else {
            0.0
        };
        let snap = std::mem::take(&mut self.snap);
        self.tree.get_mut().tick(dt as f32);
        let this = &*self;
        let root = El::scroll(list, Axis::Vertical)
            .gap((ROW_GAP * k) as f32)
            .style(|s| s.align_items = Some(taffy::AlignItems::CENTER))
            .children(rows.iter().enumerate().map(|(i, row)| {
                let cell = El::paint(move |canvas, cell| {
                    this.paint_row(canvas, fonts, i, row, cell, k, dot_gutter);
                })
                .id(row_id(i))
                .size(row_w as f32, (ROW_H * k) as f32);
                match row.header {
                    Some(_) => cell.style(|s| {
                        s.margin.top = taffy::LengthPercentageAuto::length((HEADER_H * k) as f32);
                    }),
                    None => cell,
                }
            }));
        let mut tree = this.tree.borrow_mut();
        let frame = tree.layout(root, rect);
        // Centre the focused row, eased, while the list follows focus and no finger has it.
        let (view, max) = frame.scroll(list).expect("the list is a scroll");
        let target = frame.rect(row_id(this.cursor)).map_or(0.0, |r| {
            (r.center_y() - view.top - view.height() / 2.0).clamp(0.0, max)
        });
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
        tree.paint(canvas, frame);
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
            && self
                .focus_pop
                .iter()
                .enumerate()
                .all(|(i, s)| s.vel == 0.0 && s.pos == focus_target(i))
            && self.bump.pos == 0.0
            && self.bump.vel == 0.0
            && self.press.pos == 1.0
            && self.press.vel == 0.0
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
        let top = f64::from(cell.top) + self.bump.pos * k + (1.0 - ent.travel) * ROW_RISE * k;
        if let Some(header) = row.header {
            fonts.draw_tracked(
                canvas,
                &header.to_uppercase(),
                x0 + 16.0 * k,
                top - 12.0 * k,
                W::SemiBold,
                12.0 * k,
                1.4 * k,
                fg(0.45),
            );
        }
        // Scale 0.98 → 1.0 about the centre, times the confirm dip. Two
        // channels: pop = this is the row, dip = you just pressed it.
        let pop = self.focus_pop.get(i).map_or(f, |s| s.pos);
        // A switch is its own feedback: squashing the row around it reads as the list
        // lurching on what is one control moving.
        let dip = if i == self.cursor && !matches!(row.control, Control::Toggle(_)) {
            self.press.pos
        } else {
            1.0
        };
        let scale = (0.98 + 0.02 * pop) * dip;
        let (cx, cy) = (x0 + row_w / 2.0, top + ROW_H * k / 2.0);
        canvas.save();
        canvas.translate((cx as f32, cy as f32));
        canvas.scale((scale as f32, scale as f32));
        canvas.translate((-cx as f32, -cy as f32));
        // Per-row layer only while arriving (panel + two text runs). Bounds
        // are the row rect so this is never a full-screen pass.
        let fading = ent.fade < 1.0;
        if fading {
            let bounds = Rect::from_xywh(x0 as f32, top as f32, row_w as f32, (ROW_H * k) as f32);
            canvas.save_layer_alpha_f(bounds, ent.fade as f32);
        }
        let r = Rect::from_xywh(x0 as f32, top as f32, row_w as f32, (ROW_H * k) as f32);
        let stroke = if row.caret {
            PanelStroke::Brand(0.7)
        } else {
            PanelStroke::Plain(0.06 + 0.22 * f as f32)
        };
        let tint = if row.caret {
            Some(accent(0.30))
        } else if f > 0.01 {
            Some(accent(0.30 * f as f32))
        } else {
            None
        };
        crate::theme::panel(canvas, r, 14.0, tint, stroke, k as f32);
        // Specular only on the focused row; a settings screen paints dozens of idle rows.
        if f > 0.5 {
            crate::theme::panel_highlight(canvas, r, 14.0, k as f32);
        }

        // A note under the label lifts the label; the two share the row's height. The value
        // stays on the row's centre line with the switch and the chevrons — lifted with the
        // label it sat visibly above them on every row that carries a note.
        let centred = cy + 16.0 * k * 0.36;
        let baseline = if row.note.is_some() {
            cy - 2.0 * k
        } else {
            centred
        };
        let tone = |on: skia_safe::Color4f, off: skia_safe::Color4f| {
            if row.danger {
                crate::theme::ERROR
            } else if row.enabled {
                on
            } else {
                off
            }
        };
        // Leading marks shift the label: a Lucide icon.
        let mut label_x = x0 + 16.0 * k;
        if let Some(icon) = row.icon.and_then(crate::icons::by_name) {
            crate::icons::draw_icon(
                canvas,
                icon,
                (label_x + 10.0 * k) as f32,
                cy as f32,
                (20.0 * k) as f32,
                tone(fg(0.85), fg(0.4)),
            );
            label_x += 32.0 * k;
        }
        if row.handle {
            if let Some(grip) = crate::icons::by_name("grip-vertical") {
                let grip_tone = if row.held {
                    accent(1.0)
                } else {
                    tone(fg(0.7), fg(0.3))
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
        // Trailing buttons take the row's right end; the value field ends before them.
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
                    tone(fg(0.9), fg(0.45))
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
        let row_w = row_w - buttons_w;
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
        if row.value.is_none() {
            let color = tone(accent(1.0), fg(0.35));
            let tw = fonts.measure(&row.label, W::SemiBold, 16.0 * k) as f64;
            fonts.draw(
                canvas,
                &row.label,
                cx - tw / 2.0,
                baseline,
                W::SemiBold,
                16.0 * k,
                color,
            );
        } else if let Control::Toggle(_) = row.control {
            fonts.draw(
                canvas,
                &row.label,
                label_x,
                baseline,
                W::SemiBold,
                16.0 * k,
                tone(fg(1.0), fg(0.55)),
            );
            // The switch: a 36×20 track, the knob eased across it, accent when on.
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
        } else {
            fonts.draw(
                canvas,
                &row.label,
                label_x,
                baseline,
                W::SemiBold,
                16.0 * k,
                tone(fg(1.0), fg(0.55)),
            );
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
            let alpha =
                |c: skia_safe::Color4f, a: f32| skia_safe::Color4f::new(c.r, c.g, c.b, c.a * a);
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
        if fading {
            canvas.restore(); // entrance layer
        }
        canvas.restore();
    }
}

// Tab strip

/// Strip band height, including air under the pills before the first row.
pub const TAB_STRIP_H: f64 = 46.0;

/// Pill row inside the band: top 2, height 30; the remaining 14 is air below.
/// Published so a backdrop (library focus wash) uses the row, not the 46 band.
pub const TAB_PILL_TOP: f64 = 2.0;
pub const TAB_PILL_H: f64 = 30.0;

/// Horizontal section switcher. Presentational: the screen owns selection and
/// the shoulders; this draws the pills and slides one highlight between them.
pub struct TabStrip {
    /// Highlight `(x, width)` in device px, sprung so velocity carries across
    /// rapid L1/R1. `None` until first render so a new screen does not fly in
    /// from x = 0.
    indicator: Option<(Spring, Spring)>,
    /// Last-drawn pill rects, device px — the pointer hit-tests what was drawn.
    pills: Vec<Rect>,
}

const PILL_TEXT: f64 = 13.0;
const PILL_PAD_X: f64 = 13.0;
const PILL_GAP: f64 = 7.0;

/// Each pill's width and the run's total, device px. Shared with
/// [`TabStrip::width`]: a trailing-aligned caller needs the width before draw,
/// and a second copy of this arithmetic would disagree with the drawn edge.
fn pill_widths(labels: &[&str], fonts: &Fonts, k: f64) -> (Vec<f64>, f64) {
    let size = PILL_TEXT * k;
    let widths: Vec<f64> = labels
        .iter()
        .map(|l| f64::from(fonts.measure(l, W::SemiBold, size)) + 2.0 * PILL_PAD_X * k)
        .collect();
    let total = widths.iter().sum::<f64>() + PILL_GAP * k * (labels.len().saturating_sub(1)) as f64;
    (widths, total)
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
            indicator: None,
            pills: Vec::new(),
        }
    }

    /// Last-drawn pill rect; tests assert what a press can reach.
    #[cfg(test)]
    pub fn pill(&self, i: usize) -> Option<Rect> {
        self.pills.get(i).copied()
    }

    /// Tab a press landed on. Hit box is the full strip height: pills are too
    /// small for a tap that misses the text.
    pub fn pointer(&self, p: Pointer) -> Option<usize> {
        p.press().then(|| p.pick(&self.pills)).flatten()
    }

    /// Draw pills on the leading edge of `rect`'s top band, at [`EDGE_INSET`].
    /// `focused` is D-pad focus (no-shoulder remote): highlight brightens and
    /// grows ‹ ›, the same left/right affordance as a focused value row.
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
        let pill_h = TAB_PILL_H * k;
        let size = PILL_TEXT * k;
        let (widths, total) = pill_widths(labels, fonts, k);
        let gap = PILL_GAP * k;
        // Leading, under the heading: a centred strip under a left-aligned
        // title reads as two pieces of chrome. Clamp, not a branch: full inset,
        // then centred, then flush-left (overflow spends on the unread right).
        let inset = EDGE_INSET * k;
        let slack = f64::from(rect.width()) - total;
        let mut x = f64::from(rect.left) + inset.min((slack / 2.0).max(0.0));
        let top = f64::from(rect.top) + TAB_PILL_TOP * k;

        let sel = selected.min(labels.len() - 1);
        let target = (
            x + widths[..sel].iter().sum::<f64>() + gap * sel as f64,
            widths[sel],
        );
        if self.indicator.is_none() {
            self.indicator = Some((Spring::rest(target.0), Spring::rest(target.1)));
        }
        let (ix, iw) = {
            let (sx, sw) = self.indicator.as_mut().expect("seeded just above");
            if crate::theme::reduce_motion() {
                *sx = Spring::rest(target.0);
                *sw = Spring::rest(target.1);
            } else {
                sx.step_spec(target.0, springs::INDICATOR, dt);
                sw.step_spec(target.1, springs::INDICATOR, dt);
                // Settle in device px (already × `k`) so the pill stops sub-pixel jittering.
                sx.settle(target.0, 0.05, 0.5);
                sw.settle(target.1, 0.05, 0.5);
            }
            (sx.pos, sw.pos)
        };
        crate::theme::panel(
            canvas,
            Rect::from_xywh(ix as f32, top as f32, iw as f32, pill_h as f32),
            (pill_h / 2.0 / k) as f32,
            Some(accent(if focused { 1.0 } else { 0.85 })),
            PanelStroke::Plain(if focused { 0.5 } else { 0.22 }),
            k as f32,
        );
        if focused {
            // Same ‹ › as a focused value row: left/right travel here.
            let cy = top + pill_h / 2.0;
            chevron(canvas, ix - 9.0 * k, cy, 4.0 * k, true, 0.9);
            chevron(canvas, ix + iw + 9.0 * k, cy, 4.0 * k, false, 0.9);
        }

        let baseline = top + pill_h / 2.0 + size * 0.36;
        self.pills.clear();
        for (i, label) in labels.iter().enumerate() {
            // Fade toward white by highlight overlap so both labels light as it slides.
            let pill_x = x;
            // Full-height hit box; width is this pill only, so neighbours cannot both claim a press.
            self.pills.push(Rect::from_xywh(
                pill_x as f32,
                rect.top,
                widths[i] as f32,
                rect.height().max((pill_h + 4.0 * k) as f32),
            ));
            let overlap = (pill_x + widths[i]).min(ix + iw) - pill_x.max(ix);
            let covered = (overlap / widths[i]).clamp(0.0, 1.0) as f32;
            let tw = f64::from(fonts.measure(label, W::SemiBold, size));
            fonts.draw(
                canvas,
                label,
                pill_x + (widths[i] - tw) / 2.0,
                baseline,
                W::SemiBold,
                size,
                fg(0.5 + 0.5 * covered),
            );
            x += widths[i] + gap;
        }
    }
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

// On-screen keyboard

/// What a field accepts (backspace always works).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Charset {
    Free,
    /// Hostnames: everything but whitespace.
    Hostname,
    Digits,
}

pub fn permits(charset: Charset, ch: char) -> bool {
    match charset {
        Charset::Free => true,
        Charset::Hostname => !ch.is_whitespace(),
        Charset::Digits => ch.is_ascii_digit(),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum KeyMsg {
    None,
    Type(char),
    Backspace,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Char(char),
    Space,
    Backspace,
    Done,
}

/// Digits first, then letters; last char column is hostname punctuation. Swift grid, verbatim.
fn key_rows() -> &'static [Vec<Key>] {
    use std::sync::OnceLock;
    static ROWS: OnceLock<Vec<Vec<Key>>> = OnceLock::new();
    ROWS.get_or_init(|| {
        let chars = |s: &str| s.chars().map(Key::Char).collect::<Vec<_>>();
        vec![
            chars("1234567890"),
            chars("qwertyuiop"),
            chars("asdfghjkl-"),
            chars("zxcvbnm._:"),
            vec![Key::Space, Key::Backspace, Key::Done],
        ]
    })
}

/// Controller keyboard: fixed grid in a bottom tray. D-pad moves, A types, X
/// backspaces, B/Y/Done confirms. Edits apply live; closing is done.
pub struct Keyboard {
    row: usize,
    col: usize,
    /// Tray slide-in, 0 hidden → 1 seated. Swift `.spring(0.32, 0.86)`.
    tray: Spring,
    key_flash: f64,
    /// Last-drawn key rects. The tray slides, so hit-test what was drawn, not a seated layout.
    keys: Vec<(Rect, Key)>,
}

impl Default for Keyboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Keyboard {
    pub fn new() -> Keyboard {
        Keyboard {
            row: 1, // letter row, not digits
            col: 0,
            tray: Spring::rest(0.0),
            key_flash: 0.0,
            keys: Vec::new(),
        }
    }

    /// Press types the key under it and moves the cursor there. A miss between
    /// keys is swallowed: the tray is modal and must not reach the list behind.
    pub fn pointer(&mut self, p: Pointer) -> (KeyMsg, Option<MenuPulse>) {
        if !p.press() {
            return (KeyMsg::None, None);
        }
        let Some(i) = p.pick(&self.keys.iter().map(|(r, _)| *r).collect::<Vec<_>>()) else {
            return (KeyMsg::None, None);
        };
        let key = self.keys[i].1;
        // Cursor from key identity, not draw index: `key_rows` is the layout authority.
        if let Some((r, c)) = key_rows()
            .iter()
            .enumerate()
            .find_map(|(r, row)| row.iter().position(|k| *k == key).map(|c| (r, c)))
        {
            self.row = r;
            self.col = c;
        }
        self.key_flash = 1.0;
        match key {
            Key::Char(c) => (KeyMsg::Type(c), None),
            Key::Space => (KeyMsg::Type(' '), None),
            Key::Backspace => (KeyMsg::Backspace, None),
            Key::Done => (KeyMsg::Done, Some(MenuPulse::Confirm)),
        }
    }

    /// Whether `p` hits the tray. The screen asks first so a press outside a
    /// raised keyboard dismisses it instead of falling through to the list.
    pub fn covers(&self, p: Pointer) -> bool {
        self.keys.iter().any(|(r, _)| p.hits(*r))
    }

    /// The screen applies `Type`/`Backspace` (charset included); a refusal
    /// comes back as Boundary from the screen.
    pub fn menu(&mut self, ev: MenuEvent) -> (KeyMsg, Option<MenuPulse>) {
        let rows = key_rows();
        match ev {
            MenuEvent::Move(dir) => {
                let (mut row, mut col) = (self.row as i32, self.col as i32);
                match dir {
                    MenuDir::Left => col -= 1,
                    MenuDir::Right => col += 1,
                    MenuDir::Up | MenuDir::Down => {
                        let next = row + if dir == MenuDir::Down { 1 } else { -1 };
                        if next < 0 || next >= rows.len() as i32 {
                            return (KeyMsg::None, Some(MenuPulse::Boundary));
                        }
                        // Proportional column map across unequal row widths
                        // (Done goes up to the last letter, not "e").
                        let from = (rows[row as usize].len() - 1).max(1) as f64;
                        let to = (rows[next as usize].len() - 1) as f64;
                        col = (col as f64 * to / from).round() as i32;
                        row = next;
                    }
                }
                if row < 0
                    || row >= rows.len() as i32
                    || col < 0
                    || col >= rows[row as usize].len() as i32
                {
                    return (KeyMsg::None, Some(MenuPulse::Boundary));
                }
                self.row = row as usize;
                self.col = col as usize;
                (KeyMsg::None, Some(MenuPulse::Move))
            }
            MenuEvent::Confirm => {
                self.key_flash = 1.0;
                match rows[self.row][self.col] {
                    Key::Char(c) => (KeyMsg::Type(c), None),
                    Key::Space => (KeyMsg::Type(' '), None),
                    Key::Backspace => (KeyMsg::Backspace, None),
                    Key::Done => (KeyMsg::Done, Some(MenuPulse::Confirm)),
                }
            }
            MenuEvent::Tertiary => (KeyMsg::Backspace, None),
            MenuEvent::Secondary | MenuEvent::Back => (KeyMsg::Done, Some(MenuPulse::Confirm)),
            _ => (KeyMsg::None, None),
        }
    }

    /// Step the tray toward shown/hidden. Returns 0..1; exactly 0 while hidden so the caller can skip draw.
    pub fn seat(&mut self, shown: bool, dt: f64) -> f64 {
        self.tray
            .step(if shown { 1.0 } else { 0.0 }, TRAY_K, TRAY_C, dt);
        self.tray.settle(if shown { 1.0 } else { 0.0 }, 0.001, 0.01);
        self.key_flash = approach(self.key_flash, 0.0, dt, 0.10);
        self.tray.pos.clamp(0.0, 1.2)
    }

    /// Tray height in design units (pre-`k`), for layout above it.
    pub fn tray_height() -> f64 {
        5.0 * 42.0 + 4.0 * 7.0 + 2.0 * 14.0
    }

    /// Draw the tray with its bottom at `bottom`, centred, slid by `seat` (0..1).
    /// The caller clips nothing: the tray rises from below the screen.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        canvas: &Canvas,
        fonts: &Fonts,
        w: f64,
        bottom: f64,
        seat: f64,
        k: f64,
    ) {
        let rows = key_rows();
        self.keys.clear();
        let tray_w = (560.0 * k).min(w - 32.0 * k);
        let tray_h = Self::tray_height() * k;
        let x0 = (w - tray_w) / 2.0;
        let y0 = bottom - tray_h * seat;
        let rect = Rect::from_xywh(x0 as f32, y0 as f32, tray_w as f32, tray_h as f32);
        crate::theme::panel(
            canvas,
            rect,
            22.0,
            Some(skia_safe::Color4f::new(0.05, 0.045, 0.09, 0.55)),
            PanelStroke::Plain(0.12),
            k as f32,
        );

        let pad = 14.0 * k;
        let gap = 7.0 * k;
        let key_h = 42.0 * k;
        for (r, row) in rows.iter().enumerate() {
            let n = row.len() as f64;
            let key_w = (tray_w - 2.0 * pad - (n - 1.0) * gap) / n;
            let y = y0 + pad + r as f64 * (key_h + gap);
            for (c, key) in row.iter().enumerate() {
                let x = x0 + pad + c as f64 * (key_w + gap);
                let focused = r == self.row && c == self.col;
                let kr = Rect::from_xywh(x as f32, y as f32, key_w as f32, key_h as f32);
                self.keys.push((kr, *key));
                let face = if focused {
                    let mut b = accent(1.0);
                    if self.key_flash > 0.02 {
                        let f = self.key_flash as f32;
                        b = skia_safe::Color4f::new(
                            b.r + (1.0 - b.r) * 0.5 * f,
                            b.g + (1.0 - b.g) * 0.5 * f,
                            b.b,
                            1.0,
                        );
                    }
                    b
                } else {
                    fg(0.08)
                };
                canvas.draw_rrect(
                    RRect::new_rect_xy(kr, (9.0 * k) as f32, (9.0 * k) as f32),
                    &fill(face),
                );
                // Focused fill is accent; letter ink must read on that, not on the field.
                let ink = if focused {
                    crate::theme::on_accent()
                } else {
                    fg(1.0)
                };
                let (cx, cy) = (x + key_w / 2.0, y + key_h / 2.0);
                match key {
                    Key::Char(ch) => {
                        let s = ch.to_string();
                        let size = 18.0 * k;
                        let tw = fonts.measure(&s, W::Medium, size) as f64;
                        fonts.draw(
                            canvas,
                            &s,
                            cx - tw / 2.0,
                            cy + size * 0.36,
                            W::Medium,
                            size,
                            ink,
                        );
                    }
                    Key::Space => draw_space_icon(canvas, cx, cy, k, ink),
                    Key::Backspace => draw_backspace_icon(canvas, cx, cy, k, ink),
                    Key::Done => {
                        let size = 15.0 * k;
                        let label = "Done";
                        let tw = fonts.measure(label, W::SemiBold, size) as f64;
                        let check_w = 14.0 * k;
                        let total = check_w + 6.0 * k + tw;
                        draw_check(canvas, cx - total / 2.0 + check_w / 2.0, cy, k, ink);
                        fonts.draw(
                            canvas,
                            label,
                            cx - total / 2.0 + check_w + 6.0 * k,
                            cy + size * 0.36,
                            W::SemiBold,
                            size,
                            ink,
                        );
                    }
                }
            }
        }
    }
}

fn stroke_paint(ink: skia_safe::Color4f, width: f32) -> Paint {
    let mut p = stroke(ink, width);
    p.set_stroke_cap(skia_safe::PaintCap::Round);
    p.set_stroke_join(skia_safe::PaintJoin::Round);
    p
}

fn draw_space_icon(canvas: &Canvas, cx: f64, cy: f64, k: f64, ink: skia_safe::Color4f) {
    // Space: underline bracket.
    let (w, h) = (16.0 * k, 5.0 * k);
    let p = stroke_paint(ink, (1.6 * k) as f32);
    let mut path = PathBuilder::new();
    path.move_to(((cx - w / 2.0) as f32, (cy - h / 2.0) as f32));
    path.line_to(((cx - w / 2.0) as f32, (cy + h / 2.0) as f32));
    path.line_to(((cx + w / 2.0) as f32, (cy + h / 2.0) as f32));
    path.line_to(((cx + w / 2.0) as f32, (cy - h / 2.0) as f32));
    canvas.draw_path(&path.detach(), &p);
}

fn draw_backspace_icon(canvas: &Canvas, cx: f64, cy: f64, k: f64, ink: skia_safe::Color4f) {
    // Backspace: left-pointing cap with an × inside.
    let (w, h) = (18.0 * k, 12.0 * k);
    let nose = 6.0 * k;
    let p = stroke_paint(ink, (1.6 * k) as f32);
    let (l, r, t, b) = (cx - w / 2.0, cx + w / 2.0, cy - h / 2.0, cy + h / 2.0);
    let mut path = PathBuilder::new();
    path.move_to(((l + nose) as f32, t as f32));
    path.line_to((r as f32, t as f32));
    path.line_to((r as f32, b as f32));
    path.line_to(((l + nose) as f32, b as f32));
    path.line_to((l as f32, cy as f32));
    path.close();
    canvas.draw_path(&path.detach(), &p);
    let (xc, xr) = (cx + nose / 2.0, 2.6 * k);
    canvas.draw_line(
        ((xc - xr) as f32, (cy - xr) as f32),
        ((xc + xr) as f32, (cy + xr) as f32),
        &p,
    );
    canvas.draw_line(
        ((xc - xr) as f32, (cy + xr) as f32),
        ((xc + xr) as f32, (cy - xr) as f32),
        &p,
    );
}

fn draw_check(canvas: &Canvas, cx: f64, cy: f64, k: f64, ink: skia_safe::Color4f) {
    let p = stroke_paint(ink, (1.8 * k) as f32);
    let r = 5.0 * k;
    let mut path = PathBuilder::new();
    path.move_to(((cx - r) as f32, cy as f32));
    path.line_to(((cx - r * 0.25) as f32, (cy + r * 0.7) as f32));
    path.line_to(((cx + r) as f32, (cy - r * 0.7) as f32));
    canvas.draw_path(&path.detach(), &p);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb() -> Keyboard {
        Keyboard::new()
    }

    #[test]
    fn keyboard_opens_on_q_and_types() {
        let mut k = kb();
        let (msg, _) = k.menu(MenuEvent::Confirm);
        assert_eq!(msg, KeyMsg::Type('q'));
    }

    #[test]
    fn keyboard_proportional_column_mapping() {
        let mut k = kb();
        // Far-right of digits ("0"), then down: col must land on Done, not a middle key.
        for _ in 0..9 {
            k.menu(MenuEvent::Move(MenuDir::Right));
        }
        k.menu(MenuEvent::Move(MenuDir::Up)); // digits row
        assert_eq!((k.row, k.col), (0, 9));
        for _ in 0..4 {
            k.menu(MenuEvent::Move(MenuDir::Down));
        }
        assert_eq!(k.row, 4);
        assert_eq!(k.col, 2, "rightmost column maps onto Done");
        let (msg, _) = k.menu(MenuEvent::Confirm);
        assert_eq!(msg, KeyMsg::Done);
    }

    #[test]
    fn keyboard_edges_refuse() {
        let mut k = kb();
        let (_, pulse) = k.menu(MenuEvent::Move(MenuDir::Left));
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
        // X backspaces; B/Y are done, from anywhere.
        assert_eq!(k.menu(MenuEvent::Tertiary).0, KeyMsg::Backspace);
        assert_eq!(k.menu(MenuEvent::Secondary).0, KeyMsg::Done);
        assert_eq!(k.menu(MenuEvent::Back).0, KeyMsg::Done);
    }

    #[test]
    fn charsets() {
        assert!(permits(Charset::Digits, '7'));
        assert!(!permits(Charset::Digits, 'a'));
        assert!(permits(Charset::Hostname, '-'));
        assert!(!permits(Charset::Hostname, ' '));
        assert!(permits(Charset::Free, ' '));
    }

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
        assert!(l.bump.pos.abs() > 1.0, "recoil engaged");
        assert_eq!(
            l.menu(MenuEvent::Move(MenuDir::Right), 3).0,
            ListMsg::Adjust(1)
        );
        assert_eq!(l.menu(MenuEvent::Confirm, 3).0, ListMsg::Activate);
    }

    const TABS: [&str; 7] = [
        "Stream",
        "Video",
        "Audio",
        "Controller",
        "Input",
        "Interface",
        "Presets",
    ];

    /// A velocity-carrying spring can overshoot: pin that a burst never leaves
    /// the strip, and that it still lands on the selected pill.
    #[test]
    fn tab_indicator_rides_a_burst_without_leaving_the_strip() {
        let fonts = crate::theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((900, 120)).unwrap();
        let rect = Rect::from_xywh(0.0, 0.0, 900.0, TAB_STRIP_H as f32);
        let mut strip = TabStrip::new();
        let dt = 1.0 / 60.0;
        // Seat, then a 5-step burst at one press per frame — faster than the spring can settle.
        strip.render(surface.canvas(), rect, &TABS, 0, false, &fonts, 1.0, dt);
        let mut worst_left = f64::MAX;
        let mut worst_right = f64::MIN;
        for sel in 1..=5 {
            strip.render(surface.canvas(), rect, &TABS, sel, false, &fonts, 1.0, dt);
            let (ix, iw) = strip.indicator.map(|(x, w)| (x.pos, w.pos)).unwrap();
            worst_left = worst_left.min(ix);
            worst_right = worst_right.max(ix + iw);
        }
        for _ in 0..240 {
            strip.render(surface.canvas(), rect, &TABS, 5, false, &fonts, 1.0, dt);
            let (ix, iw) = strip.indicator.map(|(x, w)| (x.pos, w.pos)).unwrap();
            worst_left = worst_left.min(ix);
            worst_right = worst_right.max(ix + iw);
        }
        assert!(
            worst_left >= f64::from(rect.left) - 0.5,
            "pill ran off the left: {worst_left}"
        );
        assert!(
            worst_right <= f64::from(rect.right) + 0.5,
            "pill ran off the right: {worst_right}"
        );
        let pill = strip.pill(5).expect("the selected pill was drawn");
        let (ix, iw) = strip.indicator.map(|(x, w)| (x.pos, w.pos)).unwrap();
        assert!(
            (ix - f64::from(pill.left)).abs() < 0.5 && (iw - f64::from(pill.width())).abs() < 0.5,
            "settled at ({ix}, {iw}), pill is at ({}, {})",
            pill.left,
            pill.width()
        );
    }

    /// Same column as the heading ([`EDGE_INSET`]); a shrinking window gives it
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
            let last = strip
                .pill(TABS.len() - 1)
                .expect("the last section was drawn");
            (rect, f64::from(first.left), f64::from(last.right))
        };

        // Both insets fit: starts on the heading column at every scale, still inside the band.
        for k in [0.75, 1.0, 2.0] {
            let (rect, left, right) = run(1400.0, k);
            assert!(
                (left - (f64::from(rect.left) + EDGE_INSET * k)).abs() < 0.5,
                "k={k}: strip starts at {left}, not on the {} column",
                EDGE_INSET * k
            );
            assert!(
                right <= f64::from(rect.right),
                "k={k}: strip overran its band"
            );
        }

        let (_, wide_left, wide_right) = run(1400.0, 1.0);
        let total = wide_right - wide_left;

        // Narrower than both insets, wider than the run: centre so the shortfall is not all on one edge.
        let (rect, left, right) = run((total + EDGE_INSET) as f32, 1.0);
        assert!(
            (left - f64::from(rect.left) - (f64::from(rect.right) - right)).abs() < 0.5,
            "a squeezed strip should sit even: {left} in from the left, {} from the right",
            f64::from(rect.right) - right
        );

        // Wider than the band: flush left, overflow right only.
        let (rect, left, right) = run((total - 40.0) as f32, 1.0);
        assert!(
            (left - f64::from(rect.left)).abs() < 0.5,
            "an overflowing strip should go flush left, not to {left}"
        );
        assert!(
            right > f64::from(rect.right),
            "the run was supposed to overflow"
        );
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
        crate::theme::set_ink(crate::theme::Ink::of(crate::library::palette("violet")));
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
        crate::theme::set_ink(crate::theme::Ink::of(crate::library::palette("violet")));
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
        crate::theme::set_ink(crate::theme::Ink::of(crate::library::palette("violet")));
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
        // The icon sits in the gutter the label used to start in, so something is inked there.
        let gutter = (f64::from(r0.left) + 26.0, f64::from(r0.center_y()));
        assert!(
            px(&on, gutter).iter().any(|c| *c > 80),
            "icon in the gutter: {:?}",
            px(&on, gutter)
        );
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
        assert_eq!(list.focus_pop[0].pos, 1.0);
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
