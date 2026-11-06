//! The controller keyboard, and the text-entry rules every open field shares.

use crate::anim::{Spring, TRAY_C, TRAY_K};
use crate::el::{El, Id, Tree};
use crate::pointer::Pointer;
use crate::theme::{fill, stroke, Fonts, Frame, PanelStroke, W};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, Paint, PathBuilder, RRect, Rect};

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
    /// Arms the next letter as a capital.
    Shift,
    /// Swaps the letters for the symbols, and back.
    Layer,
    Space,
    Backspace,
    Done,
}

/// Which keys the tray shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Layer {
    #[default]
    Letters,
    Symbols,
}

/// The layer's keys, row by row. Both lead with the digits and put the layer key first in the
/// bottom row, so a swap leaves the plate on it. Letters, Shift and the symbols type every
/// printable ASCII character; the letters' last column is hostname punctuation.
fn key_rows(layer: Layer) -> &'static [Vec<Key>] {
    use std::sync::OnceLock;
    static LETTERS: OnceLock<Vec<Vec<Key>>> = OnceLock::new();
    static SYMBOLS: OnceLock<Vec<Vec<Key>>> = OnceLock::new();
    let chars = |s: &str| s.chars().map(Key::Char).collect::<Vec<_>>();
    match layer {
        Layer::Letters => LETTERS.get_or_init(|| {
            vec![
                chars("1234567890"),
                chars("qwertyuiop"),
                chars("asdfghjkl-"),
                chars("zxcvbnm._:"),
                vec![
                    Key::Layer,
                    Key::Shift,
                    Key::Space,
                    Key::Backspace,
                    Key::Done,
                ],
            ]
        }),
        Layer::Symbols => SYMBOLS.get_or_init(|| {
            vec![
                chars("1234567890"),
                chars("!@#$%^&*()"),
                chars("`~-_=+[]{}\\"),
                chars("|;:'\",.<>/?"),
                vec![Key::Layer, Key::Space, Key::Backspace, Key::Done],
            ]
        }),
    }
}

/// What input to an open text field asks of the screen that owns it. Typing and deleting
/// already landed in the field's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    /// Nothing more to do: a key typed, deleted, moved over or refused.
    Stay,
    /// Put the field away: Back, Return or Escape, or a press outside the tray.
    Close,
    /// The keyboard's Done key or Y; where the device's own keyboard types, OK. A field
    /// whose Done means something (search, save) does it; the rest close.
    Done,
}

/// `ch` appended to `text` when `admits(text, ch)`; whether it was.
fn type_into(text: &mut String, ch: char, admits: impl Fn(&str, char) -> bool) -> bool {
    let ok = admits(text, ch);
    if ok {
        text.push(ch);
    }
    ok
}

/// SDL text (a hardware keyboard, or Steam's under gamescope) into an open field, one
/// character at a time through `admits`.
pub(crate) fn type_text(text: &mut String, typed: &str, admits: impl Fn(&str, char) -> bool) {
    for ch in typed.chars() {
        type_into(text, ch, &admits);
    }
}

/// A hardware key while a field is open: Backspace deletes, Return and Escape close.
/// `None` leaves the key to the shell.
pub(crate) fn field_key(key: crate::input::Key, text: &mut String) -> Option<Entry> {
    use crate::input::Key as K;
    match key {
        K::Backspace => {
            text.pop();
            Some(Entry::Stay)
        }
        K::Return | K::Escape => Some(Entry::Close),
        _ => None,
    }
}

/// Controller keyboard: a letters and a symbols grid in a bottom tray. D-pad moves, A types,
/// X backspaces, B/Y/Done confirms. Edits apply live; closing is done. Focus is the console's
/// plate: it glides in from the field's row, key to key, and back out on close.
pub struct Keyboard {
    row: usize,
    col: usize,
    layer: Layer,
    /// Shift is armed: the next letter types as a capital and disarms it.
    shift: bool,
    /// Tray slide-in, 0 hidden → 1 seated. Swift `.spring(0.32, 0.86)`.
    tray: Spring,
    /// Asked to show, and the frame's `dt`, both from [`Self::seat`].
    shown: bool,
    dt: f64,
    /// The keys as focus targets. Boxed: a keyboard sits inside screens that are variants
    /// of one enum.
    tree: Box<Tree>,
    /// Last-drawn tray. A press in its padding or between keys is still on the keyboard.
    tray_rect: Rect,
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
            layer: Layer::Letters,
            shift: false,
            tray: Spring::rest(0.0),
            shown: false,
            dt: 0.0,
            tree: Box::new(Tree::new()),
            tray_rect: Rect::new_empty(),
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
        if let Some((r, c)) = key_rows(self.layer)
            .iter()
            .enumerate()
            .find_map(|(r, row)| row.iter().position(|k| *k == key).map(|c| (r, c)))
        {
            self.row = r;
            self.col = c;
        }
        self.tree.press();
        self.strike(key)
    }

    /// What pressing `key` asks for. Shift arms one capital; any typed key spends it. The
    /// layer key swaps the rows under the cursor, which stays in the bottom row's first slot.
    fn strike(&mut self, key: Key) -> (KeyMsg, Option<MenuPulse>) {
        let shift = std::mem::take(&mut self.shift);
        match key {
            Key::Char(c) if shift => (KeyMsg::Type(c.to_ascii_uppercase()), None),
            Key::Char(c) => (KeyMsg::Type(c), None),
            Key::Shift => {
                self.shift = !shift;
                (KeyMsg::None, Some(MenuPulse::Move))
            }
            Key::Layer => {
                self.layer = match self.layer {
                    Layer::Letters => Layer::Symbols,
                    Layer::Symbols => Layer::Letters,
                };
                let rows = key_rows(self.layer);
                self.row = self.row.min(rows.len() - 1);
                self.col = self.col.min(rows[self.row].len() - 1);
                (KeyMsg::None, Some(MenuPulse::Move))
            }
            Key::Space => (KeyMsg::Type(' '), None),
            Key::Backspace => (KeyMsg::Backspace, None),
            Key::Done => (KeyMsg::Done, Some(MenuPulse::Confirm)),
        }
    }

    /// The plate on screen, before its outset; tests follow it in from the field's row.
    #[cfg(test)]
    pub(crate) fn plate(&self) -> Option<Rect> {
        self.tree.plate_rect().map(|(r, _)| r)
    }

    /// Whether `p` hits the tray, gaps and padding included. The screen asks first so only a
    /// press outside the raised keyboard dismisses it; a wobbly pointer between keys stays.
    pub fn covers(&self, p: Pointer) -> bool {
        p.hits(self.tray_rect)
    }

    /// The screen applies `Type`/`Backspace` (charset included); a refusal
    /// comes back as Boundary from the screen.
    pub fn menu(&mut self, ev: MenuEvent) -> (KeyMsg, Option<MenuPulse>) {
        let rows = key_rows(self.layer);
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
                self.tree.press();
                self.strike(rows[self.row][self.col])
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
        self.shown = shown;
        self.dt = dt;
        self.tray.pos.clamp(0.0, 1.2)
    }

    /// A pad or remote event for the open field over `text`. Typing lands through `admits`.
    /// Where the device's own keyboard types (`system`), the pad only confirms or closes.
    pub(crate) fn edit_menu(
        &mut self,
        ev: MenuEvent,
        system: bool,
        text: &mut String,
        admits: impl Fn(&str, char) -> bool,
    ) -> (Entry, Option<MenuPulse>) {
        if ev == MenuEvent::Back {
            return (Entry::Close, Some(MenuPulse::Confirm));
        }
        if system {
            return match ev {
                MenuEvent::Confirm => (Entry::Done, Some(MenuPulse::Confirm)),
                _ => (Entry::Stay, None),
            };
        }
        let moved = |ok: bool| {
            Some(if ok {
                MenuPulse::Move
            } else {
                MenuPulse::Boundary
            })
        };
        match self.menu(ev) {
            (KeyMsg::Type(c), _) => (Entry::Stay, moved(type_into(text, c, admits))),
            (KeyMsg::Backspace, _) => (Entry::Stay, moved(text.pop().is_some())),
            (KeyMsg::Done, _) => (Entry::Done, Some(MenuPulse::Confirm)),
            (KeyMsg::None, pulse) => (Entry::Stay, pulse),
        }
    }

    /// A pointer while the tray is up over `text`. The tray is modal: a press outside it
    /// closes the field rather than reaching the row underneath. `None` for a hover
    /// outside, which nothing takes.
    pub(crate) fn edit_pointer(
        &mut self,
        p: Pointer,
        text: &mut String,
        admits: impl Fn(&str, char) -> bool,
    ) -> Option<Entry> {
        if !self.covers(p) {
            return p.press().then_some(Entry::Close);
        }
        Some(match self.pointer(p).0 {
            KeyMsg::Type(c) => {
                type_into(text, c, admits);
                Entry::Stay
            }
            KeyMsg::Backspace => {
                text.pop();
                Entry::Stay
            }
            KeyMsg::Done => Entry::Done,
            KeyMsg::None => Entry::Stay,
        })
    }

    /// Tray height in design units (pre-`k`), for layout above it.
    pub fn tray_height() -> f64 {
        5.0 * 42.0 + 4.0 * 7.0 + 2.0 * 14.0
    }

    /// Draw the tray with its bottom at `bottom`, centred, slid by `seat` (0..1).
    /// The caller clips nothing: the tray rises from below the screen. The keys lay out
    /// seated and the slide is a translate, so the plate rides the tray instead of chasing it.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        canvas: &Canvas,
        ui: &Frame,
        fonts: &Fonts,
        w: f64,
        bottom: f64,
        seat: f64,
        k: f64,
    ) {
        let rows = key_rows(self.layer);
        self.keys.clear();
        let tray_w = (560.0 * k).min(w - 32.0 * k);
        let tray_h = Self::tray_height() * k;
        let x0 = (w - tray_w) / 2.0;
        let seated = Rect::from_xywh(
            x0 as f32,
            (bottom - tray_h) as f32,
            tray_w as f32,
            tray_h as f32,
        );
        let slide = (tray_h * (1.0 - seat)) as f32;
        self.tray_rect = seated.with_offset((0.0, slide));

        let pad = 14.0 * k;
        let gap = 7.0 * k;
        let key_h = 42.0 * k;
        let corner = (9.0 * k) as f32;
        let (shift, layer) = (self.shift, self.layer);
        let mut root = El::column();
        for (r, row) in rows.iter().enumerate() {
            let n = row.len() as f64;
            let key_w = (tray_w - 2.0 * pad - (n - 1.0) * gap) / n;
            let y = pad + r as f64 * (key_h + gap);
            for (c, &key) in row.iter().enumerate() {
                let x = pad + c as f64 * (key_w + gap);
                let kr = Rect::from_xywh(x as f32, y as f32, key_w as f32, key_h as f32);
                self.keys
                    .push((kr.with_offset((seated.left, seated.top + slide)), key));
                root = root.child(
                    El::paint(move |canvas, r| {
                        draw_key(canvas, ui, fonts, key, (shift, layer), r, corner, k)
                    })
                    .id(key_id(r, c))
                    .focusable(corner)
                    .place(kr),
                );
            }
        }
        canvas.save();
        canvas.translate((0.0, slide));
        crate::theme::panel(
            canvas,
            ui,
            seated,
            22.0,
            Some(skia_safe::Color4f::new(0.05, 0.045, 0.09, 0.55)),
            PanelStroke::Plain(0.12),
            k as f32,
        );
        let frame = self.tree.layout(root, seated);
        // Closing hands the plate back to the field's row.
        self.tree
            .set_focus(self.shown.then(|| key_id(self.row, self.col)));
        self.tree
            .paint_focus(canvas, ui, frame, k as f32, self.dt, false);
        canvas.restore();
    }
}

fn key_id(row: usize, col: usize) -> Id {
    Id::new("keyboard", row * 16 + col)
}

/// One key's face and legend in `r`. Focus is the plate behind it. Armed, Shift's face is
/// lit and the letters show the capital they would type. The layer key names the other layer.
#[allow(clippy::too_many_arguments)]
fn draw_key(
    canvas: &Canvas,
    ui: &Frame,
    fonts: &Fonts,
    key: Key,
    (shift, layer): (bool, Layer),
    r: Rect,
    corner: f32,
    k: f64,
) {
    let face = if shift && key == Key::Shift {
        0.3
    } else {
        0.08
    };
    canvas.draw_rrect(RRect::new_rect_xy(r, corner, corner), &fill(ui.fg(face)));
    let ink = ui.fg(1.0);
    let (cx, cy) = (f64::from(r.center_x()), f64::from(r.center_y()));
    match key {
        Key::Layer => {
            let label = match layer {
                Layer::Letters => "#+=",
                Layer::Symbols => "ABC",
            };
            let size = 15.0 * k;
            let tw = f64::from(fonts.measure(label, W::SemiBold, size));
            fonts.draw(
                canvas,
                label,
                cx - tw / 2.0,
                cy + size * 0.36,
                W::SemiBold,
                size,
                ink,
            );
        }
        Key::Char(ch) => {
            let s = if shift { ch.to_ascii_uppercase() } else { ch }.to_string();
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
        Key::Shift => draw_shift_icon(canvas, cx, cy, k, ink),
        Key::Space => draw_space_icon(canvas, cx, cy, k, ink),
        Key::Backspace => draw_backspace_icon(canvas, cx, cy, k, ink),
        Key::Done => {
            let size = 15.0 * k;
            let label = pf_l10n::common::common_done();
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

fn stroke_paint(ink: skia_safe::Color4f, width: f32) -> Paint {
    let mut p = stroke(ink, width);
    p.set_stroke_cap(skia_safe::PaintCap::Round);
    p.set_stroke_join(skia_safe::PaintJoin::Round);
    p
}

fn draw_shift_icon(canvas: &Canvas, cx: f64, cy: f64, k: f64, ink: skia_safe::Color4f) {
    // Shift: an up arrow, head over stem.
    let (w, h) = (14.0 * k, 16.0 * k);
    let p = stroke_paint(ink, (1.6 * k) as f32);
    let (l, r, t, b) = (cx - w / 2.0, cx + w / 2.0, cy - h / 2.0, cy + h / 2.0);
    let mut path = PathBuilder::new();
    path.move_to((l as f32, cy as f32));
    path.line_to((cx as f32, t as f32));
    path.line_to((r as f32, cy as f32));
    path.move_to((cx as f32, t as f32));
    path.line_to((cx as f32, b as f32));
    canvas.draw_path(&path.detach(), &p);
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
    use crate::pointer::PointerKind;

    /// Back closes an open field and Y is Done; where the device's keyboard types, OK is
    /// Done and the D-pad types nothing. OK types the focused key through the field's rule.
    #[test]
    fn an_open_field_routes_back_done_and_the_system_keyboard() {
        use MenuPulse::{Boundary, Confirm, Move};
        let (mut kb, mut text) = (Keyboard::new(), String::new());
        let any = |_: &str, _: char| true;
        let none = |_: &str, _: char| false;
        let mut ev = |ev, system, admits: fn(&str, char) -> bool| {
            kb.edit_menu(ev, system, &mut text, admits)
        };
        assert!(matches!(
            ev(MenuEvent::Back, false, any),
            (Entry::Close, Some(Confirm))
        ));
        assert!(matches!(
            ev(MenuEvent::Back, true, any),
            (Entry::Close, Some(Confirm))
        ));
        assert!(matches!(
            ev(MenuEvent::Secondary, false, any),
            (Entry::Done, Some(Confirm))
        ));
        assert!(matches!(
            ev(MenuEvent::Confirm, true, any),
            (Entry::Done, Some(Confirm))
        ));
        let left = MenuEvent::Move(MenuDir::Left);
        assert!(matches!(ev(left, true, any), (Entry::Stay, None)));
        assert!(matches!(
            ev(MenuEvent::Confirm, false, none),
            (Entry::Stay, Some(Boundary))
        ));
        assert!(matches!(
            ev(MenuEvent::Confirm, false, any),
            (Entry::Stay, Some(Move))
        ));
        assert!(matches!(
            ev(MenuEvent::Tertiary, false, any),
            (Entry::Stay, Some(Move))
        ));
        assert!(matches!(
            ev(MenuEvent::Tertiary, false, any),
            (Entry::Stay, Some(Boundary))
        ));
        let mut text = String::from("ab");
        assert_eq!(
            field_key(crate::input::Key::Backspace, &mut text),
            Some(Entry::Stay)
        );
        assert_eq!(text, "a");
        assert_eq!(
            field_key(crate::input::Key::Return, &mut text),
            Some(Entry::Close)
        );
        assert_eq!(field_key(crate::input::Key::Left, &mut text), None);
    }

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
        assert_eq!(k.col, 4, "rightmost column maps onto Done");
        let (msg, _) = k.menu(MenuEvent::Confirm);
        assert_eq!(msg, KeyMsg::Done);
    }

    /// Shift is the bottom row's second key: it arms one capital, which the next letter
    /// spends, and a second press disarms it. Digits type as they are.
    #[test]
    fn keyboard_shift_types_one_capital() {
        let mut k = kb();
        for _ in 0..3 {
            k.menu(MenuEvent::Move(MenuDir::Down));
        }
        k.menu(MenuEvent::Move(MenuDir::Right));
        assert_eq!((k.row, k.col), (4, 1));
        assert!(matches!(
            k.menu(MenuEvent::Confirm),
            (KeyMsg::None, Some(MenuPulse::Move))
        ));
        k.menu(MenuEvent::Move(MenuDir::Left));
        for _ in 0..3 {
            k.menu(MenuEvent::Move(MenuDir::Up));
        }
        assert_eq!(k.menu(MenuEvent::Confirm).0, KeyMsg::Type('Q'));
        assert_eq!(k.menu(MenuEvent::Confirm).0, KeyMsg::Type('q'), "one-shot");

        k.shift = true;
        k.strike(Key::Shift);
        assert!(!k.shift, "a second press disarms");
        k.shift = true;
        assert_eq!(k.strike(Key::Char('7')).0, KeyMsg::Type('7'));
    }

    /// A press between two keys is still on the keyboard: it types nothing and must not
    /// read as outside, which is what closes the field.
    #[test]
    fn keyboard_covers_its_gaps() {
        let ui = &crate::theme::Frame::default();
        let mut k = kb();
        let mut surface = skia_safe::surfaces::raster_n32_premul((800, 600)).unwrap();
        let fonts = crate::theme::build_fonts().unwrap();
        k.render(surface.canvas(), ui, &fonts, 800.0, 600.0, 1.0, 1.0);
        let (a, b) = (k.keys[10].0, k.keys[11].0);
        let at = |x: f32, y: f32| Pointer {
            x: f64::from(x),
            y: f64::from(y),
            kind: PointerKind::Press,
        };
        let gap = at((a.right + b.left) / 2.0, a.center_y());
        assert!(!gap.hits(a) && !gap.hits(b), "the press lands in no key");
        assert!(k.covers(gap), "between keys is on the tray");
        assert_eq!(k.pointer(gap).0, KeyMsg::None, "and types nothing");
        assert!(
            !k.covers(at(a.center_x(), k.tray_rect.top - 4.0)),
            "above it is not"
        );
    }

    /// Focus on the keyboard is the plate: it rests on the focused key, travels to the next,
    /// and fades out with the tray.
    #[test]
    fn keyboard_focus_is_the_plate() {
        let ui = &crate::theme::Frame::default();
        let mut k = kb();
        let mut surface = skia_safe::surfaces::raster_n32_premul((800, 600)).unwrap();
        let fonts = crate::theme::build_fonts().unwrap();
        let mut frames = |k: &mut Keyboard, shown: bool| {
            for _ in 0..90 {
                let seat = k.seat(shown, 1.0 / 60.0);
                k.render(surface.canvas(), ui, &fonts, 800.0, 600.0, seat, 1.0);
            }
        };
        let on = |k: &Keyboard, i: usize| {
            let (plate, _) = k.tree.plate_rect().expect("the plate is up");
            let key = k.keys[i].0;
            (plate.center_x() - key.center_x()).abs() < 1.0
                && (plate.center_y() - key.center_y()).abs() < 1.0
        };
        frames(&mut k, true);
        assert!(on(&k, 10), "on q");
        k.menu(MenuEvent::Move(MenuDir::Right));
        frames(&mut k, true);
        assert!(on(&k, 11), "on w");
        frames(&mut k, false);
        assert!(k.tree.plate_rect().is_none(), "gone with the tray");
    }

    /// The key that types `ch`: its layer, row and column, and whether Shift goes first.
    fn locate(ch: char) -> (Layer, usize, usize, bool) {
        for layer in [Layer::Letters, Layer::Symbols] {
            for (r, row) in key_rows(layer).iter().enumerate() {
                for (c, key) in row.iter().enumerate() {
                    match *key {
                        Key::Char(x) if x == ch => return (layer, r, c, false),
                        Key::Char(x) if x.is_ascii_lowercase() && x.to_ascii_uppercase() == ch => {
                            return (layer, r, c, true)
                        }
                        Key::Space if ch == ' ' => return (layer, r, c, false),
                        _ => {}
                    }
                }
            }
        }
        panic!("no key types {ch:?}");
    }

    /// The pad alone types every printable ASCII character, through the field's own path:
    /// the layer key swaps in place, Shift capitals one letter, and OK types what it says.
    #[test]
    fn the_pad_types_every_printable_ascii_character() {
        let all: String = (0x20u8..=0x7e).map(char::from).collect();
        let (mut k, mut text) = (kb(), String::new());
        let go = |k: &mut Keyboard, row: usize, col: usize| {
            while k.row != row {
                let dir = if k.row < row {
                    MenuDir::Down
                } else {
                    MenuDir::Up
                };
                k.menu(MenuEvent::Move(dir));
            }
            while k.col != col {
                let dir = if k.col < col {
                    MenuDir::Right
                } else {
                    MenuDir::Left
                };
                k.menu(MenuEvent::Move(dir));
            }
        };
        let any = |_: &str, _: char| true;
        for ch in all.chars() {
            let (layer, row, col, shift) = locate(ch);
            if k.layer != layer {
                go(&mut k, 4, 0);
                k.edit_menu(MenuEvent::Confirm, false, &mut text, any);
                assert_eq!(
                    (k.layer, k.row, k.col),
                    (layer, 4, 0),
                    "the plate stays put"
                );
            }
            if shift {
                go(&mut k, 4, 1);
                k.edit_menu(MenuEvent::Confirm, false, &mut text, any);
            }
            go(&mut k, row, col);
            k.edit_menu(MenuEvent::Confirm, false, &mut text, any);
        }
        assert_eq!(text, all);
        assert_eq!(all.len(), 95);
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
}
