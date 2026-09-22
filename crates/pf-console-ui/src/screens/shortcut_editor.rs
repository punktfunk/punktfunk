//! One ring shortcut on the console: name, key, four modifiers, Save, Remove.
//!
//! The disc preview is the same keycap the in-stream ring draws. The name is
//! typed on the keyboard tray (Steam's on a Deck); the key is picked on a
//! keyboard-shaped tray of every name `key_vk` knows. A new shortcut takes the
//! first empty ring slot. Writes go through the same load-then-save the
//! settings screen uses, so another writer's edits are never reverted.
//!
//! Pad form: design/touch-client-overlay.md.

use crate::anim::{approach, Spring, TRAY_C, TRAY_K};
use crate::glyphs::{Hint, HintKey};
use crate::pointer::Pointer;
use crate::ring::draw_keycap_disc;
use crate::screens::{Ctx, Outbox};
use crate::theme::{accent, fg, fill, on_accent, stroke, Fonts, PanelStroke, EDGE_INSET, W};
use crate::widgets::{permits, Charset, KeyMsg, Keyboard, ListMsg, MenuList, RowSpec, ROW_MAX_W};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use pf_client_core::overlay_actions::{chord_chip, key_legend, OverlayConfig, Shortcut};
use skia_safe::{Canvas, Color4f, RRect, Rect};

use super::ring_editor::ring_platform;

const MODIFIERS: [&str; 4] = ["ctrl", "alt", "shift", "win"];

const GRID: [&[&str]; 6] = [
    &[
        "escape", "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12",
    ],
    &[
        "1",
        "2",
        "3",
        "4",
        "5",
        "6",
        "7",
        "8",
        "9",
        "0",
        "backspace",
    ],
    &[
        "tab", "q", "w", "e", "r", "t", "y", "u", "i", "o", "p", "insert", "delete",
    ],
    &[
        "capslock", "a", "s", "d", "f", "g", "h", "j", "k", "l", "enter",
    ],
    &[
        "z", "x", "c", "v", "b", "n", "m", "home", "end", "pageup", "pagedown",
    ],
    &[
        "space",
        "left",
        "up",
        "down",
        "right",
        "printscreen",
        "pause",
    ],
];

const ROW_NAME: usize = 0;
const ROW_KEY: usize = 1;
const ROW_MODS: usize = 2;
const ROW_SAVE: usize = ROW_MODS + MODIFIERS.len();
const ROW_REMOVE: usize = ROW_SAVE + 1;

pub(crate) struct Draft {
    pub id: Option<String>,
    pub label: String,
    pub mods: [bool; 4],
    pub key: Option<String>,
}

impl Draft {
    fn of(sc: Option<&Shortcut>) -> Draft {
        let Some(sc) = sc else {
            return Draft {
                id: None,
                label: String::new(),
                mods: [false; 4],
                key: None,
            };
        };
        let has = |names: &[&str]| sc.keys.iter().any(|k| names.contains(&k.as_str()));
        Draft {
            id: Some(sc.id.clone()),
            label: sc.label.clone(),
            mods: [
                has(&["ctrl", "control"]),
                has(&["alt", "option"]),
                has(&["shift"]),
                has(&["win", "cmd", "super", "meta"]),
            ],
            key: sc
                .keys
                .iter()
                .rev()
                .find(|k| GRID.iter().any(|row| row.contains(&k.as_str())))
                .cloned(),
        }
    }

    /// Host send order: marked modifiers, then the key.
    fn keys(&self) -> Vec<String> {
        let mut v: Vec<String> = MODIFIERS
            .iter()
            .zip(self.mods)
            .filter(|(_, on)| *on)
            .map(|(m, _)| m.to_string())
            .collect();
        v.extend(self.key.clone());
        v
    }

    fn row_count(&self) -> usize {
        if self.id.is_some() {
            ROW_REMOVE + 1
        } else {
            ROW_SAVE + 1
        }
    }
}

fn rows(d: &Draft, typing: bool, picking: bool) -> Vec<RowSpec> {
    let mut v = Vec::with_capacity(ROW_REMOVE + 1);
    let mut name = RowSpec::field("Name", d.label.clone(), "Optional — e.g. Task Manager");
    name.header = Some("Shortcut");
    name.caret = typing;
    v.push(name);
    let mut key = RowSpec::field(
        "Key",
        d.key.as_deref().map(key_legend).unwrap_or_default(),
        "Choose…",
    );
    key.caret = picking;
    v.push(key);
    for (i, m) in MODIFIERS.iter().enumerate() {
        let mut row = RowSpec::field(
            key_legend(m),
            if d.mods[i] { "On" } else { "Off" }.into(),
            "",
        );
        row.header = if i == 0 { Some("Hold with") } else { None };
        row.adjustable = true;
        v.push(row);
    }
    v.push(RowSpec::action(
        if d.id.is_some() {
            "Save"
        } else {
            "Add to the dial"
        },
        d.key.is_some(),
    ));
    if d.id.is_some() {
        v.push(RowSpec::action("Remove shortcut", true));
    }
    v
}

/// Pure upsert so tests can drive the blob without the process-wide settings file.
fn apply_draft(cfg: &mut OverlayConfig, d: &Draft) {
    cfg.upsert_shortcut(d.id.as_deref(), &d.label, d.keys());
}

fn remove_shortcut(cfg: &mut OverlayConfig, id: &str) {
    cfg.remove_shortcut(id);
}

/// Nearest key centre to `x` — Up/Down keep a keyboard column on staggered rows.
fn nearest_col(row: &[Rect], x: f32) -> usize {
    row.iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (a.center_x() - x)
                .abs()
                .total_cmp(&(b.center_x() - x).abs())
        })
        .map_or(0, |(i, _)| i)
}

#[derive(Debug, PartialEq, Eq)]
enum TrayMsg {
    None,
    Pick(&'static str),
    Close,
}

struct KeyTray {
    row: usize,
    col: usize,
    /// 0 hidden → 1 seated; shares the keyboard tray's spring.
    tray: Spring,
    flash: f64,
    /// Last drawn key rects — hit-testing and the column walk follow the sliding tray.
    keys: Vec<Vec<Rect>>,
}

impl KeyTray {
    const KEY_H: f64 = 38.0;
    const GAP: f64 = 6.0;
    const PAD: f64 = 14.0;
    /// Letter-key width in design units; word keys grow to the legend.
    const UNIT: f64 = 34.0;

    fn new() -> KeyTray {
        KeyTray {
            row: 0,
            col: 0,
            tray: Spring::rest(0.0),
            flash: 0.0,
            keys: GRID
                .iter()
                .map(|r| vec![Rect::new_empty(); r.len()])
                .collect(),
        }
    }

    /// Focus `key` if it is on the grid; otherwise Esc at (0, 0).
    fn seat_on(&mut self, key: Option<&str>) {
        let at = key.and_then(|name| {
            GRID.iter()
                .enumerate()
                .find_map(|(r, row)| row.iter().position(|k| *k == name).map(|c| (r, c)))
        });
        (self.row, self.col) = at.unwrap_or((0, 0));
    }

    fn tray_height() -> f64 {
        let rows = GRID.len() as f64;
        rows * Self::KEY_H + (rows - 1.0) * Self::GAP + 2.0 * Self::PAD
    }

    /// Step the tray spring. Returns 0 while hidden so the caller can skip drawing.
    fn seat(&mut self, shown: bool, dt: f64) -> f64 {
        let target = if shown { 1.0 } else { 0.0 };
        self.tray.step(target, TRAY_K, TRAY_C, dt);
        self.tray.settle(target, 0.001, 0.01);
        self.flash = approach(self.flash, 0.0, dt, 0.10);
        self.tray.pos.clamp(0.0, 1.2)
    }

    fn covers(&self, p: Pointer) -> bool {
        self.keys.iter().flatten().any(|r| p.hits(*r))
    }

    /// A press on a key picks it. Misses are swallowed — the tray is modal.
    fn pointer(&mut self, p: Pointer) -> (TrayMsg, Option<MenuPulse>) {
        if !p.press() {
            return (TrayMsg::None, None);
        }
        let hit = self
            .keys
            .iter()
            .enumerate()
            .find_map(|(r, row)| p.pick(row).map(|c| (r, c)));
        let Some((r, c)) = hit else {
            return (TrayMsg::None, None);
        };
        (self.row, self.col) = (r, c);
        self.flash = 1.0;
        (TrayMsg::Pick(GRID[r][c]), Some(MenuPulse::Confirm))
    }

    fn menu(&mut self, ev: MenuEvent) -> (TrayMsg, Option<MenuPulse>) {
        match ev {
            MenuEvent::Move(dir) => {
                let (r, c) = (self.row, self.col);
                let x = self.keys[r][c].center_x();
                let next = match dir {
                    MenuDir::Left if c > 0 => (r, c - 1),
                    MenuDir::Right if c + 1 < GRID[r].len() => (r, c + 1),
                    MenuDir::Up if r > 0 => (r - 1, nearest_col(&self.keys[r - 1], x)),
                    MenuDir::Down if r + 1 < GRID.len() => {
                        (r + 1, nearest_col(&self.keys[r + 1], x))
                    }
                    _ => return (TrayMsg::None, Some(MenuPulse::Boundary)),
                };
                (self.row, self.col) = next;
                (TrayMsg::None, Some(MenuPulse::Move))
            }
            MenuEvent::Confirm => {
                self.flash = 1.0;
                (
                    TrayMsg::Pick(GRID[self.row][self.col]),
                    Some(MenuPulse::Confirm),
                )
            }
            MenuEvent::Back | MenuEvent::Secondary => (TrayMsg::Close, Some(MenuPulse::Confirm)),
            _ => (TrayMsg::None, None),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        canvas: &Canvas,
        fonts: &Fonts,
        w: f64,
        bottom: f64,
        seat: f64,
        k: f64,
        chosen: Option<&str>,
    ) {
        let kf = k as f32;
        let tray_w = (640.0 * k).min(w - 32.0 * k);
        let tray_h = Self::tray_height() * k;
        let x0 = (w - tray_w) / 2.0;
        let y0 = bottom - tray_h * seat;
        let rect = Rect::from_xywh(x0 as f32, y0 as f32, tray_w as f32, tray_h as f32);
        crate::theme::panel(
            canvas,
            rect,
            22.0,
            Some(Color4f::new(0.05, 0.045, 0.09, 0.55)),
            PanelStroke::Plain(0.12),
            kf,
        );
        let (gap, key_h, unit) = (Self::GAP * k, Self::KEY_H * k, Self::UNIT * k);
        let size = 13.0 * k;
        for (r, row) in GRID.iter().enumerate() {
            let legends: Vec<String> = row.iter().map(|n| key_legend(n)).collect();
            let widths: Vec<f64> = legends
                .iter()
                .map(|l| (f64::from(fonts.measure(l, W::Medium, size)) + 16.0 * k).max(unit))
                .collect();
            let total: f64 = widths.iter().sum::<f64>() + gap * (row.len() as f64 - 1.0);
            let y = y0 + Self::PAD * k + r as f64 * (key_h + gap);
            let mut x = x0 + (tray_w - total) / 2.0;
            for (c, (name, kw)) in row.iter().zip(&widths).enumerate() {
                let kr = Rect::from_xywh(x as f32, y as f32, *kw as f32, key_h as f32);
                self.keys[r][c] = kr;
                let focused = r == self.row && c == self.col;
                let is_chosen = chosen == Some(*name);
                let face = if focused {
                    let mut b = accent(1.0);
                    if self.flash > 0.02 {
                        let f = self.flash as f32;
                        b = Color4f::new(
                            b.r + (1.0 - b.r) * 0.5 * f,
                            b.g + (1.0 - b.g) * 0.5 * f,
                            b.b,
                            1.0,
                        );
                    }
                    b
                } else if is_chosen {
                    accent(0.35)
                } else {
                    fg(0.08)
                };
                let rr = RRect::new_rect_xy(kr, 9.0 * kf, 9.0 * kf);
                canvas.draw_rrect(rr, &fill(face));
                if is_chosen && !focused {
                    canvas.draw_rrect(rr, &stroke(accent(0.9), kf));
                }
                // Accent fill: legend ink is `on_accent`, not field `fg`.
                let ink = if focused { on_accent() } else { fg(1.0) };
                let tw = f64::from(fonts.measure(&legends[c], W::Medium, size));
                fonts.draw(
                    canvas,
                    &legends[c],
                    x + (kw - tw) / 2.0,
                    y + key_h / 2.0 + size * 0.36,
                    W::Medium,
                    size,
                    ink,
                );
                x += kw + gap;
            }
        }
    }
}

pub(crate) struct ShortcutEditorScreen {
    draft: Draft,
    list: MenuList,
    /// Own tray; a Deck uses Steam's keyboard instead.
    keyboard: Keyboard,
    keys: KeyTray,
    editing_name: bool,
    picking_key: bool,
}

impl ShortcutEditorScreen {
    pub(crate) fn new(_ctx: &Ctx, existing: Option<&Shortcut>) -> ShortcutEditorScreen {
        let draft = Draft::of(existing);
        let mut list = MenuList::new();
        list.jump_to(if draft.key.is_none() {
            ROW_KEY
        } else {
            ROW_NAME
        });
        ShortcutEditorScreen {
            draft,
            list,
            keyboard: Keyboard::new(),
            keys: KeyTray::new(),
            editing_name: false,
            picking_key: false,
        }
    }

    pub(crate) fn title(&self) -> String {
        if self.draft.id.is_some() {
            "Shortcut".into()
        } else {
            "New shortcut".into()
        }
    }

    pub(crate) fn editing(&self) -> bool {
        self.editing_name
    }

    /// The field list, while neither tray covers it.
    pub(super) fn pan_list(&mut self) -> Option<&mut MenuList> {
        (!self.editing_name && !self.picking_key).then_some(&mut self.list)
    }

    fn open_keys(&mut self) {
        self.keys.seat_on(self.draft.key.as_deref());
        self.picking_key = true;
    }

    fn save(&mut self, ctx: &mut Ctx, fx: &mut Outbox) {
        if self.draft.key.is_none() {
            fx.toast = Some("Pick a key first".into());
            self.list.jump_to(ROW_KEY);
            self.open_keys();
            return;
        }
        *ctx.settings = ctx.store.load();
        let mut cfg =
            OverlayConfig::parse(&ctx.settings.overlay_actions, ring_platform(ctx.platform));
        apply_draft(&mut cfg, &self.draft);
        ctx.settings.overlay_actions = cfg.to_json();
        ctx.store.save(ctx.settings);
        fx.toast = Some("Saved".into());
        fx.pop();
    }

    fn remove(&mut self, ctx: &mut Ctx, fx: &mut Outbox) {
        let Some(id) = self.draft.id.clone() else {
            return;
        };
        *ctx.settings = ctx.store.load();
        let mut cfg =
            OverlayConfig::parse(&ctx.settings.overlay_actions, ring_platform(ctx.platform));
        remove_shortcut(&mut cfg, &id);
        ctx.settings.overlay_actions = cfg.to_json();
        ctx.store.save(ctx.settings);
        fx.toast = Some("Removed".into());
        fx.pop();
    }

    fn type_char(&mut self, ch: char) -> bool {
        if !self.editing_name || !permits(Charset::Free, ch) {
            return false;
        }
        self.draft.label.push(ch);
        true
    }

    fn backspace(&mut self) -> bool {
        self.editing_name && self.draft.label.pop().is_some()
    }

    pub(crate) fn text_input(&mut self, text: &str) {
        for ch in text.chars() {
            self.type_char(ch);
        }
    }

    pub(crate) fn edit_key(&mut self, key: crate::input::Key) -> bool {
        use crate::input::Key as K;
        if !self.editing_name {
            return false;
        }
        match key {
            K::Backspace => {
                self.backspace();
                true
            }
            K::Return | K::Escape => {
                self.editing_name = false;
                true
            }
            _ => false,
        }
    }

    fn activate(&mut self, row: usize, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        match row {
            ROW_NAME => self.editing_name = true,
            ROW_KEY => self.open_keys(),
            r if (ROW_MODS..ROW_SAVE).contains(&r) => self.draft.mods[r - ROW_MODS] ^= true,
            ROW_SAVE => self.save(ctx, fx),
            _ => self.remove(ctx, fx),
        }
        Some(MenuPulse::Confirm)
    }

    /// Modifier rows: Left Off, Right On — same grammar as settings. Already-there recoils.
    fn adjust(&mut self, row: usize, delta: i32) -> Option<MenuPulse> {
        if !(ROW_MODS..ROW_SAVE).contains(&row) {
            return Some(MenuPulse::Boundary);
        }
        let on = &mut self.draft.mods[row - ROW_MODS];
        let want = delta > 0;
        if *on == want {
            return Some(MenuPulse::Boundary);
        }
        *on = want;
        Some(MenuPulse::Move)
    }

    fn take_pick(&mut self, msg: TrayMsg) -> Option<MenuPulse> {
        match msg {
            TrayMsg::Pick(name) => {
                self.draft.key = Some(name.to_string());
                self.picking_key = false;
                Some(MenuPulse::Confirm)
            }
            TrayMsg::Close => {
                self.picking_key = false;
                Some(MenuPulse::Confirm)
            }
            TrayMsg::None => None,
        }
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        if self.editing_name {
            if ctx.deck {
                return match ev {
                    MenuEvent::Back | MenuEvent::Confirm => {
                        self.editing_name = false;
                        Some(MenuPulse::Confirm)
                    }
                    _ => None,
                };
            }
            let (msg, pulse) = self.keyboard.menu(ev);
            return match msg {
                KeyMsg::Type(c) => Some(if self.type_char(c) {
                    MenuPulse::Move
                } else {
                    MenuPulse::Boundary
                }),
                KeyMsg::Backspace => Some(if self.backspace() {
                    MenuPulse::Move
                } else {
                    MenuPulse::Boundary
                }),
                KeyMsg::Done => {
                    self.editing_name = false;
                    Some(MenuPulse::Confirm)
                }
                KeyMsg::None => pulse,
            };
        }
        if self.picking_key {
            let (msg, pulse) = self.keys.menu(ev);
            return self.take_pick(msg).or(pulse);
        }
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        let (msg, pulse) = self.list.menu(ev, self.draft.row_count());
        match msg {
            ListMsg::Activate => self.activate(self.list.cursor, ctx, fx),
            ListMsg::Adjust(d) => self.adjust(self.list.cursor, d),
            ListMsg::None => pulse,
        }
    }

    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        if self.editing_name && !ctx.deck {
            if !self.keyboard.covers(p) {
                if p.press() {
                    self.editing_name = false;
                    return true;
                }
                return false;
            }
            let (msg, _) = self.keyboard.pointer(p);
            match msg {
                KeyMsg::Type(c) => {
                    self.type_char(c);
                }
                KeyMsg::Backspace => {
                    self.backspace();
                }
                KeyMsg::Done => self.editing_name = false,
                KeyMsg::None => {}
            }
            return true;
        }
        if self.picking_key {
            if !self.keys.covers(p) {
                if p.press() {
                    self.picking_key = false;
                    return true;
                }
                return false;
            }
            let (msg, _) = self.keys.pointer(p);
            self.take_pick(msg);
            return true;
        }
        let (msg, _) = self.list.pointer(p, self.draft.row_count());
        match msg {
            ListMsg::Activate => {
                self.activate(self.list.cursor, ctx, fx);
                true
            }
            ListMsg::Adjust(d) => {
                self.adjust(self.list.cursor, d);
                true
            }
            ListMsg::None => false,
        }
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if self.editing_name {
            if ctx.deck {
                return vec![
                    Hint::new(HintKey::Key("STEAM + X"), "Keyboard"),
                    Hint::new(HintKey::Confirm, "Done"),
                    Hint::new(HintKey::Back, "Done"),
                ];
            }
            return vec![
                Hint::new(HintKey::Confirm, "Type"),
                Hint::new(HintKey::Tertiary, "Delete"),
                Hint::new(HintKey::Back, "Done"),
            ];
        }
        if self.picking_key {
            return vec![
                Hint::new(HintKey::Confirm, "Choose"),
                Hint::new(HintKey::Back, "Close"),
            ];
        }
        let mut hints = Vec::with_capacity(3);
        match self.list.cursor {
            ROW_NAME => hints.push(Hint::new(HintKey::Confirm, "Edit name")),
            ROW_KEY => hints.push(Hint::new(HintKey::Confirm, "Choose key")),
            r if (ROW_MODS..ROW_SAVE).contains(&r) => {
                hints.push(Hint::new(HintKey::Adjust, "Off / On"));
                hints.push(Hint::new(HintKey::Confirm, "Toggle"));
            }
            ROW_SAVE => hints.push(Hint::new(HintKey::Confirm, "Save")),
            _ => hints.push(Hint::new(HintKey::Confirm, "Remove")),
        }
        hints.push(Hint::new(HintKey::Back, "Back"));
        hints
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        let kf = k as f32;
        let x0 = f64::from(rect.left) + EDGE_INSET * k;
        fonts.leading(
            canvas,
            "Hold the modifiers marked on, then press the key. The dial draws it as a keycap.",
            W::Regular,
            13.0 * k,
            fg(0.55),
            x0,
            f64::from(rect.top) + 2.0 * k,
            ROW_MAX_W * 0.9 * k,
        );

        let top = rect.top + 40.0 * kf;
        let r = 30.0 * kf;
        let chip = chord_chip(&self.draft.keys());
        let row_w = (ROW_MAX_W * k).min(f64::from(rect.width()) - 48.0 * k);
        let left = (f64::from(rect.center_x()) - row_w / 2.0) as f32;
        draw_keycap_disc(canvas, fonts, left + r, top + r, r, kf, &chip);
        let legend_x = f64::from(left + 2.0 * r + 18.0 * kf);
        fonts.draw(
            canvas,
            if chip.is_empty() {
                "No key yet"
            } else {
                chip.as_str()
            },
            legend_x,
            f64::from(top) + 26.0 * k,
            W::SemiBold,
            18.0 * k,
            fg(if chip.is_empty() { 0.45 } else { 0.95 }),
        );
        fonts.draw(
            canvas,
            "How the dial will draw it",
            legend_x,
            f64::from(top) + 46.0 * k,
            W::Regular,
            12.0 * k,
            fg(0.5),
        );

        // Shrink the list by whichever tray is seated so the edited row stays in view.
        let seat_kb = self.keyboard.seat(self.editing_name && !ctx.deck, dt);
        let seat_keys = self.keys.seat(self.picking_key, dt);
        let tray_h = (Keyboard::tray_height() + 12.0) * k * seat_kb
            + (KeyTray::tray_height() + 12.0) * k * seat_keys;
        let list_rect = Rect::from_ltrb(
            rect.left,
            top + 2.0 * r + 22.0 * kf,
            rect.right,
            rect.bottom - tray_h as f32,
        );
        let rows = rows(&self.draft, self.editing_name, self.picking_key);
        self.list.render(
            canvas,
            list_rect,
            &rows,
            fonts,
            k,
            dt,
            !self.editing_name && !self.picking_key,
        );
        if seat_kb > 0.0 {
            self.keyboard.render(
                canvas,
                fonts,
                f64::from(rect.width()),
                f64::from(rect.bottom),
                seat_kb,
                k,
            );
        }
        if seat_keys > 0.0 {
            self.keys.render(
                canvas,
                fonts,
                f64::from(rect.width()),
                f64::from(rect.bottom),
                seat_keys,
                k,
                self.draft.key.as_deref(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_client_core::overlay_actions::{RingPlatform, SlotId};

    /// Apply the blob directly. The settings store is one process-wide file; a round
    /// trip through `save` races the settings tests.
    #[test]
    fn a_new_shortcut_lands_in_the_blob_and_the_first_empty_slot() {
        let blob = r#"{"v":2,"ring":["end_stream",null,null,null,null,null]}"#;
        let mut d = Draft::of(None);
        d.label = "Task Manager".into();
        d.mods[0] = true;
        d.mods[2] = true;
        d.key = Some("escape".into());
        assert_eq!(d.keys(), vec!["ctrl", "shift", "escape"]);
        let mut cfg = OverlayConfig::parse(blob, RingPlatform::Desktop);
        apply_draft(&mut cfg, &d);
        assert_eq!(cfg.shortcuts.len(), 1);
        assert_eq!(cfg.shortcuts[0].label, "Task Manager");
        assert_eq!(cfg.shortcuts[0].keys, vec!["ctrl", "shift", "escape"]);
        assert_eq!(cfg.ring[1], Some(SlotId::Shortcut("s1".into())));
        let back = Draft::of(Some(&cfg.shortcuts[0]));
        assert_eq!(back.id.as_deref(), Some("s1"));
        assert_eq!(back.mods, [true, false, true, false]);
        assert_eq!(back.key.as_deref(), Some("escape"));
        remove_shortcut(&mut cfg, "s1");
        assert!(cfg.shortcuts.is_empty());
        assert_eq!(cfg.ring[1], None);
    }

    #[test]
    fn the_rows_follow_the_draft() {
        let mut d = Draft::of(None);
        let r = rows(&d, false, true);
        assert_eq!(r.len(), ROW_SAVE + 1);
        assert_eq!(r[ROW_NAME].header, Some("Shortcut"));
        assert_eq!(r[ROW_KEY].value.as_deref(), Some("Choose…"));
        assert!(r[ROW_KEY].value_dim && r[ROW_KEY].caret);
        assert_eq!(r[ROW_MODS].header, Some("Hold with"));
        assert!(r[ROW_MODS..ROW_SAVE]
            .iter()
            .all(|m| m.adjustable && m.value.as_deref() == Some("Off")));
        assert!(!r[ROW_SAVE].enabled, "no key, nothing to add");
        d.key = Some("escape".into());
        d.mods[1] = true;
        d.id = Some("s1".into());
        let r = rows(&d, true, false);
        assert_eq!(r.len(), ROW_REMOVE + 1);
        assert!(r[ROW_NAME].caret);
        assert_eq!(
            r[ROW_KEY].value.as_deref(),
            Some(key_legend("escape").as_str())
        );
        assert_eq!(r[ROW_MODS + 1].value.as_deref(), Some("On"));
        assert!(r[ROW_SAVE].enabled);
        assert_eq!(r[ROW_REMOVE].label, "Remove shortcut");
        assert_eq!(d.row_count(), ROW_REMOVE + 1);
    }

    #[test]
    fn the_key_tray_walks_like_a_keyboard() {
        let mut t = KeyTray::new();
        t.seat_on(Some("a"));
        assert_eq!((t.row, t.col), (3, 1));
        assert_eq!(t.menu(MenuEvent::Move(MenuDir::Right)).0, TrayMsg::None);
        assert_eq!(t.menu(MenuEvent::Confirm).0, TrayMsg::Pick("s"));
        // Undrawn rects are empty, so Up lands on column 0.
        assert_eq!(t.menu(MenuEvent::Move(MenuDir::Up)).0, TrayMsg::None);
        assert_eq!((t.row, t.col), (2, 0));
        t.seat_on(Some("not a key"));
        assert_eq!((t.row, t.col), (0, 0), "unknown: Esc");
        assert!(matches!(
            t.menu(MenuEvent::Move(MenuDir::Left)),
            (TrayMsg::None, Some(MenuPulse::Boundary))
        ));
        assert_eq!(t.menu(MenuEvent::Back).0, TrayMsg::Close);
    }

    #[test]
    fn the_grid_holds_every_key_once_and_no_modifier() {
        use pf_client_core::overlay_actions::key_vk;
        let mut seen: Vec<&str> = Vec::new();
        for row in GRID {
            for name in row {
                assert!(key_vk(name).is_some(), "{name} is unknown to the wire");
                assert!(!MODIFIERS.contains(name), "{name} is a modifier");
                assert!(!seen.contains(name), "{name} twice");
                seen.push(name);
            }
        }
        assert_eq!(seen.len(), 66);
    }

    #[test]
    fn the_grid_cursor_keeps_its_column_across_staggered_rows() {
        let row: Vec<Rect> = (0..5)
            .map(|i| Rect::from_xywh(i as f32 * 40.0 + 20.0, 0.0, 34.0, 30.0))
            .collect();
        assert_eq!(nearest_col(&row, 0.0), 0);
        assert_eq!(nearest_col(&row, 118.0), 2);
        assert_eq!(nearest_col(&row, 999.0), 4);
        assert_eq!(nearest_col(&[], 10.0), 0);
    }
}
