//! Console editor for the in-stream quick-action [`Ring`].
//!
//! Same type as the overlay, in editing mode. Stick or D-pad to a slot; A opens
//! the catalogue [`Picker`]; Y lifts a disc and A on another swaps. Key-combination rows
//! under the ring open [`super::shortcut_editor::ShortcutEditorScreen`].
//!
//! Writes go through a [`Scope`]: load-then-save on the device's settings, so another
//! writer's edits are never reverted, or a preset's overlay. Design:
//! `design/touch-client-overlay.md`.

use super::preset::Scope;
use crate::glyphs::{Hint, HintKey};
use crate::pointer::{Pointer, PointerKind};
use crate::ring::{EditEvent, Ring, LABEL_H};
use crate::screens::{Ctx, Outbox, Screen, ScreenView};
use crate::theme::{fill, focus_halo, stroke, Fonts, Frame};
use crate::widgets::{ListMsg, MenuList, RowSpec, ROW_MAX_W};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use pf_client_core::overlay_actions::{
    catalogue, chord_chip, CatalogueEntry, CatalogueGroup, OverlayConfig, RingPlatform, SlotId,
};
use pf_client_core::ring::{RingFacts, RING_RADIUS, SLOT_DIAMETER};
use pf_l10n::client as c;
use skia_safe::{Canvas, Color4f, RRect, Rect};

/// Stage and caption in design units. Height is the in-stream ring (top slot, bottom
/// slot, label band) plus `STAGE_PAD` above and below so nothing is clipped.
const STAGE_PAD: f64 = 16.0;
const RING_ABOVE: f64 = (RING_RADIUS + SLOT_DIAMETER / 2.0) as f64;
const RING_BELOW: f64 = (RING_RADIUS + SLOT_DIAMETER + LABEL_H) as f64;
const STAGE_H: f64 = STAGE_PAD + RING_ABOVE + RING_BELOW + STAGE_PAD;
const CAPTION_H: f64 = 34.0;
const CORNER: f64 = 22.0;
/// Shortcut-row floor: below this the stage scales down rather than shrinking the list.
const LIST_MIN: f64 = 140.0;
/// Horizontal fit: in-stream ring width plus 24 px of pad.
const RING_W: f64 = 2.0 * RING_ABOVE + 24.0;

/// Same platform mapping the in-stream ring uses to pick a default blob.
pub(crate) fn ring_platform(platform: crate::platform::Platform) -> RingPlatform {
    match platform {
        crate::platform::Platform::Desktop
        | crate::platform::Platform::Web
        | crate::platform::Platform::Tizen => RingPlatform::Desktop,
        // Glass either way: a phone or iPad's screen, and the Siri Remote's trackpad.
        crate::platform::Platform::Android | crate::platform::Platform::Apple => {
            RingPlatform::Touch
        }
        // Not `Touch`: the TV's ring is driven by a pad or the remote's D-pad, so it wants the
        // keyboard/pad default blob. The Magic Remote is a pointer, but it never makes the
        // ring a touch surface.
        crate::platform::Platform::WebOS | crate::platform::Platform::Ps5 => RingPlatform::Desktop,
    }
}

/// The action catalogue as a modal list over the screen: a ring slot's pick, and a
/// shortcut's action ([`super::shortcuts`]).
pub(crate) struct Picker {
    list: MenuList,
    rows: Vec<(String, RowSpec)>,
    rect: Rect,
}

/// What a picker event came to.
pub(crate) enum Picked {
    Stay(Option<MenuPulse>),
    Chose(String),
    Closed,
}

impl Picker {
    /// `groups` as rows, `current`'s marked and under the cursor.
    pub(crate) fn new(groups: Vec<CatalogueGroup>, current: &str) -> Picker {
        let mut rows: Vec<(String, RowSpec)> = Vec::new();
        let mut cursor = 0;
        for group in groups {
            for (i, entry) in group.entries.into_iter().enumerate() {
                let CatalogueEntry { id, label, note } = entry;
                let is_current = id == current;
                let now = pf_l10n::console::console_ring_picker_current();
                let value = match (is_current, note.is_empty()) {
                    (true, true) => now.to_string(),
                    // Two messages side by side; the dot is a list separator.
                    (true, false) => format!("{now} · {note}"),
                    (false, _) => note,
                };
                let mut r = RowSpec::field(label, value, "");
                r.adjustable = false;
                if i == 0 {
                    r.header = Some(group.title);
                }
                if is_current {
                    cursor = rows.len();
                }
                rows.push((id, r));
            }
        }
        let mut list = MenuList::new();
        list.jump_to(cursor);
        Picker {
            list,
            rows,
            rect: Rect::new_empty(),
        }
    }

    pub(crate) fn list(&mut self) -> &mut MenuList {
        &mut self.list
    }

    fn chosen(&self) -> Picked {
        match self.rows.get(self.list.cursor) {
            Some((id, _)) => Picked::Chose(id.clone()),
            None => Picked::Closed,
        }
    }

    pub(crate) fn menu(&mut self, ev: MenuEvent) -> Picked {
        if ev == MenuEvent::Back {
            return Picked::Closed;
        }
        let (msg, pulse) = self.list.menu(ev, self.rows.len());
        match msg {
            ListMsg::Activate => self.chosen(),
            ListMsg::Adjust(_) => Picked::Stay(Some(MenuPulse::Boundary)),
            ListMsg::None => Picked::Stay(pulse),
        }
    }

    /// Modal: every pointer event is the picker's. A press outside the card closes it.
    pub(crate) fn pointer(&mut self, p: Pointer) -> Picked {
        if p.hits(self.rect) || matches!(p.kind, PointerKind::Scroll { .. }) {
            let (msg, _) = self.list.pointer(p, self.rows.len());
            if msg == ListMsg::Activate {
                return self.chosen();
            }
        } else if p.press() {
            return Picked::Closed;
        }
        Picked::Stay(None)
    }

    pub(crate) fn hints() -> Vec<Hint> {
        use pf_l10n::common as m;
        vec![
            Hint::new(HintKey::Confirm, m::common_choose()),
            Hint::new(HintKey::Back, m::common_cancel()),
        ]
    }

    /// The card centred in `rect`, over a scrim of the whole canvas.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        ui: &Frame,
        rect: Rect,
        fonts: &Fonts,
        k: f64,
        dt: f64,
    ) {
        let kf = k as f32;
        // Scrim the full canvas, not `rect`: the content rect is not the screen, and
        // the canvas is translated (insets, transitions). 2000 px overshoots any
        // heading or inset; the hint bar draws after this and stays legible.
        canvas.draw_rect(
            rect.with_outset((2000.0 * kf, 2000.0 * kf)),
            &fill(Color4f::new(0.0, 0.0, 0.0, 0.45)),
        );
        let w = (520.0 * kf).min(rect.width() * 0.92);
        let h = ((self.rows.len() as f32 * 50.0 + 60.0) * kf).min(rect.height() * 0.9);
        let card = Rect::from_xywh(rect.center_x() - w / 2.0, rect.center_y() - h / 2.0, w, h);
        canvas.draw_rrect(
            RRect::new_rect_xy(card, 16.0 * kf, 16.0 * kf),
            &fill(Color4f::new(0.06, 0.055, 0.1, 0.96)),
        );
        canvas.draw_rrect(
            RRect::new_rect_xy(card, 16.0 * kf, 16.0 * kf),
            &stroke(ui.fg(0.18), kf),
        );
        let inner = Rect::from_ltrb(
            card.left + 8.0 * kf,
            card.top + 12.0 * kf,
            card.right - 8.0 * kf,
            card.bottom - 12.0 * kf,
        );
        let specs: Vec<RowSpec> = self.rows.iter().map(|(_, r)| r.clone()).collect();
        canvas.save();
        canvas.clip_rect(inner, None, None);
        self.list
            .render(canvas, ui, inner, &specs, fonts, k, dt, true);
        canvas.restore();
        self.rect = card;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Ring,
    List,
}

pub(crate) struct RingEditorScreen {
    ring: Ring,
    cfg: OverlayConfig,
    platform: crate::platform::Platform,
    scope: Scope,
    /// Settings string `cfg` was parsed from. Re-adopted when the shortcut editor pops
    /// back after writing it.
    blob: String,
    list: MenuList,
    focus: Focus,
    /// The slot being picked for, and its picker.
    picker: Option<(usize, Picker)>,
    stage: Rect,
    list_rect: Rect,
    reset_armed: bool,
    /// Last stick sector, even while the list is focused, so a handoff back to the ring
    /// knows whether the stick is still held.
    stick: Option<u8>,
    /// Stick sector just entered the list; the four-way Move in the same sample must not
    /// also step a row.
    swallow_move: bool,
}

impl RingEditorScreen {
    pub(crate) fn new(ctx: &Ctx, scope: Scope) -> RingEditorScreen {
        let mut s = RingEditorScreen {
            ring: Ring::new(),
            cfg: OverlayConfig::platform_default(ring_platform(ctx.device.platform)),
            platform: ctx.device.platform,
            scope,
            blob: String::new(),
            list: MenuList::new(),
            focus: Focus::Ring,
            picker: None,
            stage: Rect::new_empty(),
            list_rect: Rect::new_empty(),
            reset_armed: false,
            stick: None,
            swallow_move: false,
        };
        s.ring.edit_at(0.0, 0.0);
        let blob = s.scope.settings(ctx).overlay_actions.clone();
        s.adopt(&blob, ctx.device.platform);
        s
    }

    /// Parse `blob` into the ring. Empty is replaced with this platform's default JSON:
    /// the ring parser otherwise assumes desktop.
    fn adopt(&mut self, blob: &str, platform: crate::platform::Platform) {
        self.platform = platform;
        self.blob = blob.to_string();
        self.cfg = OverlayConfig::parse(blob, ring_platform(platform));
        let effective = if blob.trim().is_empty() {
            self.cfg.to_json()
        } else {
            blob.to_string()
        };
        self.ring.set_facts(&RingFacts {
            overlay_actions: effective,
            touch_mode: "trackpad".into(),
            stats_tier: pf_l10n::client::player_stats_verbosity_compact().into(),
            mic_available: true,
            pad_mouse_target: 0b1,
            pointer_granted: true,
            // The desktop's stream sits in a window and captures the pointer.
            windowed: matches!(platform, crate::platform::Platform::Desktop),
            pointer_capture: matches!(platform, crate::platform::Platform::Desktop),
            mode: (1920, 1080, 60),
            native_mode: (1920, 1080, 60),
            ..RingFacts::default()
        });
    }

    /// Stores `blob` as the ring and adopts it.
    fn write(&mut self, blob: String, ctx: &mut Ctx, fx: &mut Outbox) {
        self.scope
            .write("overlay_actions", ctx, fx, |s| s.overlay_actions = blob);
        let b = self.scope.settings(ctx).overlay_actions.clone();
        self.adopt(&b, ctx.device.platform);
    }

    fn pick(&mut self, slot: usize, id: &str, ctx: &mut Ctx, fx: &mut Outbox) {
        let mut cfg = self.cfg.clone();
        cfg.ring[slot] = if id.is_empty() {
            None
        } else {
            SlotId::parse(id)
        };
        self.write(cfg.to_json(), ctx, fx);
    }

    fn swap(&mut self, a: usize, b: usize, ctx: &mut Ctx, fx: &mut Outbox) {
        let mut cfg = self.cfg.clone();
        cfg.ring.swap(a, b);
        self.write(cfg.to_json(), ctx, fx);
    }

    /// Empty blob = platform default, same as the settings row.
    fn reset(&mut self, ctx: &mut Ctx, fx: &mut Outbox) {
        self.write(String::new(), ctx, fx);
        fx.toast = Some(pf_l10n::console::console_ring_reset_toast().into());
    }

    /// The picker's list while it is open, else the slot list.
    pub(super) fn pan_list(&mut self) -> &mut MenuList {
        match &mut self.picker {
            Some((_, pk)) => pk.list(),
            None => &mut self.list,
        }
    }

    fn open_picker(&mut self, slot: usize) {
        let current = self.cfg.ring[slot]
            .as_ref()
            .map(SlotId::id)
            .unwrap_or_default();
        let groups = catalogue(&self.cfg, ring_platform(self.platform));
        self.picker = Some((slot, Picker::new(groups, &current)));
    }

    fn drain_edits(&mut self, ctx: &mut Ctx, fx: &mut Outbox) {
        while let Some(ev) = self.ring.take_edit() {
            match ev {
                EditEvent::Pick(k) => self.open_picker(k),
                EditEvent::Swap(a, b) => self.swap(a, b, ctx, fx),
            }
        }
    }

    fn rows(&self) -> Vec<RowSpec> {
        let mut rows: Vec<RowSpec> = self
            .cfg
            .shortcuts
            .iter()
            .enumerate()
            .map(|(i, sc)| {
                let chip = chord_chip(&sc.keys);
                let mut r = if sc.label.is_empty() {
                    RowSpec::field(chip, String::new(), "")
                } else {
                    RowSpec::field(sc.label.clone(), chip, "")
                };
                r.adjustable = false;
                if i == 0 {
                    r.header = Some(c::ring_editor_shortcuts());
                }
                r
            })
            .collect();
        let mut new = RowSpec::action(c::ring_shortcut_add(), true);
        if rows.is_empty() {
            new.header = Some(c::ring_editor_shortcuts());
        }
        rows.push(new);
        rows.push(RowSpec::action(
            if self.reset_armed {
                c::ring_reset_confirm()
            } else {
                c::ring_reset()
            },
            true,
        ));
        rows
    }

    fn row_count(&self) -> usize {
        self.cfg.shortcuts.len() + 2
    }

    fn activate_row(&mut self, i: usize, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        use super::shortcut_editor::ShortcutEditorScreen;
        let n = self.cfg.shortcuts.len();
        if i <= n {
            let existing = self.cfg.shortcuts.get(i);
            let editor = ShortcutEditorScreen::new(ctx, existing, self.scope.clone());
            fx.push(Screen::ShortcutEditor(editor));
            self.reset_armed = false;
            return Some(MenuPulse::Confirm);
        }
        if self.reset_armed {
            self.reset_armed = false;
            self.reset(ctx, fx);
            Some(MenuPulse::Confirm)
        } else {
            self.reset_armed = true;
            Some(MenuPulse::Boundary)
        }
    }

    /// A picker event: the slot takes a pick, and the picker closes on it or on Back.
    fn picked(&mut self, picked: Picked, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        match picked {
            Picked::Stay(pulse) => pulse,
            Picked::Closed => {
                self.picker = None;
                Some(MenuPulse::Confirm)
            }
            Picked::Chose(id) => {
                if let Some((slot, _)) = self.picker.take() {
                    self.pick(slot, &id, ctx, fx);
                }
                Some(MenuPulse::Confirm)
            }
        }
    }
}

impl ScreenView for RingEditorScreen {
    fn title(&self) -> String {
        c::ring_editor_title().into()
    }

    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if let MenuEvent::Sector(s) = ev {
            self.stick = s;
        }
        if let Some((_, p)) = self.picker.as_mut() {
            let picked = p.menu(ev);
            return self.picked(picked, ctx, fx);
        }
        if matches!(ev, MenuEvent::Move(_)) {
            self.reset_armed = false;
        }
        match self.focus {
            Focus::Ring => {
                // Down from 6 o'clock: D-pad Down, or a second stick push once highlight
                // is there. Not the stick's four-way Move (it rides behind the sector).
                let at_six = self.ring.highlight() == Some(3) && !self.ring.carrying();
                let walks = at_six
                    && !self.ring.stick_engaged()
                    && matches!(
                        ev,
                        MenuEvent::Move(MenuDir::Down) | MenuEvent::Sector(Some(3))
                    );
                if walks {
                    self.focus = Focus::List;
                    self.list.jump_to(0);
                    self.swallow_move = matches!(ev, MenuEvent::Sector(_));
                    return Some(MenuPulse::Move);
                }
                let pulse = self.ring.menu(ev);
                self.drain_edits(ctx, fx);
                if pulse.is_none() && ev == MenuEvent::Back {
                    fx.pop();
                }
                pulse
            }
            Focus::List => {
                if std::mem::take(&mut self.swallow_move) && matches!(ev, MenuEvent::Move(_)) {
                    return None;
                }
                if ev == MenuEvent::Back {
                    fx.pop();
                    return None;
                }
                if ev == MenuEvent::Move(MenuDir::Up) && self.list.cursor == 0 {
                    // Up from row 0 returns to 6 o'clock. A held stick stays the ring's
                    // so repeats do not step slots.
                    self.focus = Focus::Ring;
                    self.ring.set_highlight(3);
                    self.ring.adopt_stick(self.stick.is_some());
                    return Some(MenuPulse::Move);
                }
                let (msg, pulse) = self.list.menu(ev, self.row_count());
                match msg {
                    ListMsg::Activate => self.activate_row(self.list.cursor, ctx, fx),
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                }
            }
        }
    }

    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        if let Some((_, pk)) = self.picker.as_mut() {
            let picked = pk.pointer(p);
            self.picked(picked, ctx, fx);
            return true;
        }
        if self.ring.pointer(p) {
            self.focus = Focus::Ring;
            self.reset_armed = false;
            self.drain_edits(ctx, fx);
            return true;
        }
        if p.hits(self.list_rect) || matches!(p.kind, PointerKind::Scroll { .. }) {
            if p.press() {
                self.focus = Focus::List;
            }
            let (msg, pulse) = self.list.pointer(p, self.row_count());
            return match msg {
                ListMsg::Activate => {
                    self.activate_row(self.list.cursor, ctx, fx);
                    true
                }
                ListMsg::Adjust(_) => true,
                ListMsg::None => pulse.is_some(),
            };
        }
        false
    }

    fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        use pf_l10n::common as m;
        use pf_l10n::console as con;
        if self.picker.is_some() {
            return Picker::hints();
        }
        match self.focus {
            Focus::Ring => {
                let carrying = self.ring.carrying();
                let (confirm, lift, back) = if carrying {
                    let put_down = con::console_hint_put_down();
                    (con::console_hint_drop_here(), put_down, put_down)
                } else {
                    (
                        con::console_hint_change(),
                        con::console_hint_lift(),
                        m::common_back(),
                    )
                };
                vec![
                    Hint::new(HintKey::Confirm, confirm),
                    Hint::new(HintKey::Secondary, lift),
                    Hint::new(HintKey::Back, back),
                ]
            }
            Focus::List => {
                let n = self.cfg.shortcuts.len();
                let label = match self.list.cursor {
                    i if i < n => m::common_edit(),
                    i if i == n => m::common_add(),
                    _ => m::common_reset(),
                };
                vec![
                    Hint::new(HintKey::Confirm, label),
                    Hint::new(HintKey::Back, m::common_back()),
                ]
            }
        }
    }

    fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        let ui = ctx.ui;
        let blob = self.scope.settings(ctx).overlay_actions.clone();
        if blob != self.blob {
            self.adopt(&blob, ctx.device.platform);
        }
        self.ring.tick();
        let kf = k as f32;
        // A and Y name the buttons as the ring's own hints do, in every pad style.
        crate::widgets::blurb(
            canvas,
            ui,
            fonts,
            &pf_l10n::console::console_ring_editor_hint("A", "Y"),
            rect,
            k,
        );

        // Side-by-side when stacked fit would shrink the ring below 0.75 and width
        // holds RING_W plus a 420-wide list. Scale down only if even that cannot hold it.
        let caption_h = (CAPTION_H + 12.0) * k;
        let fit_below = (f64::from(rect.height()) - caption_h - LIST_MIN * k) / (STAGE_H * k);
        let side = fit_below < 0.75 && f64::from(rect.width()) >= (RING_W + 420.0) * k;
        let (fit, stage_w) = if side {
            let fit = ((f64::from(rect.height()) - caption_h) / (STAGE_H * k)).clamp(0.35, 1.0);
            (fit as f32, (RING_W * k * fit) as f32)
        } else {
            let stage_w = (ROW_MAX_W * k).min(f64::from(rect.width()) - 48.0 * k) as f32;
            let fit_w = f64::from(stage_w) / (RING_W * k);
            (fit_below.min(fit_w).clamp(0.35, 1.0) as f32, stage_w)
        };
        let rk = kf * fit;
        let stage_x = if side {
            rect.left + (ui.edge(k)) as f32
        } else {
            rect.center_x() - stage_w / 2.0
        };
        let stage = Rect::from_xywh(
            stage_x,
            rect.top + caption_h as f32,
            stage_w,
            (STAGE_H * k) as f32 * fit,
        );
        self.stage = stage;
        let rr = RRect::new_rect_xy(stage, CORNER as f32 * kf, CORNER as f32 * kf);
        canvas.draw_rrect(rr, &fill(ui.card_face(0.20)));
        canvas.draw_rrect(rr, &stroke(ui.fg(0.14), kf));
        if self.focus == Focus::Ring && self.picker.is_none() {
            // Corner in design units: `focus_halo` scales it. A pre-scaled corner double-scales.
            focus_halo(canvas, ui, stage, CORNER as f32, kf, 1.0);
        }

        self.ring.recentre(
            stage.center_x(),
            stage.top + (STAGE_PAD + RING_ABOVE) as f32 * rk,
        );
        canvas.save();
        canvas.clip_rrect(rr, None, None);
        self.ring.render(
            canvas,
            ui,
            rect.right.max(1.0) as u32,
            rect.bottom.max(1.0) as u32,
            rk,
            fonts,
            dt,
        );
        canvas.restore();

        let list_rect = if side {
            Rect::from_ltrb(
                stage.right + 8.0 * kf,
                rect.top + caption_h as f32,
                rect.right,
                rect.bottom,
            )
        } else {
            Rect::from_ltrb(rect.left, stage.bottom + 10.0 * kf, rect.right, rect.bottom)
        };
        self.list_rect = list_rect;
        let rows = self.rows();
        self.list.render(
            canvas,
            ui,
            list_rect,
            &rows,
            fonts,
            k,
            dt,
            self.focus == Focus::List && self.picker.is_none(),
        );

        if let Some((_, pk)) = self.picker.as_mut() {
            pk.render(canvas, ui, rect, fonts, k, dt);
        }
    }

    fn press(&mut self) {
        self.pan_list().dip();
    }

    fn pan(&mut self, p: Pointer) -> bool {
        self.pan_list().pan(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_picker_marks_the_current_pick_and_empty_empties_the_slot() {
        let blob = r#"{"v":2,"ring":["stats",null,null,null,null,null]}"#;
        let cfg = OverlayConfig::parse(blob, RingPlatform::Desktop);
        let mut s = RingEditorScreen {
            ring: Ring::new(),
            cfg,
            platform: crate::platform::Platform::Desktop,
            scope: Scope::Global,
            blob: blob.into(),
            list: MenuList::new(),
            focus: Focus::Ring,
            picker: None,
            stage: Rect::new_empty(),
            list_rect: Rect::new_empty(),
            reset_armed: false,
            stick: None,
            swallow_move: false,
        };
        s.open_picker(0);
        let (_, p) = s.picker.as_ref().expect("the picker");
        let (id, row) = &p.rows[p.list.cursor];
        assert_eq!(id, "stats");
        assert_eq!(row.value.as_deref(), Some("Current"));
        assert_eq!(p.rows.last().map(|(id, _)| id.as_str()), Some(""));
        assert_eq!(p.rows[0].1.header, Some("Session"));
        let mut cfg = s.cfg.clone();
        cfg.ring[0] = None;
        assert_eq!(cfg.ring[0], None, "the empty entry's id is the empty slot");
    }
}
