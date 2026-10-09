//! A preset's own screens, reached from the Presets tab: its menu, its name, and its
//! settings. The console never writes the catalog itself; each change leaves as
//! [`ConsoleCmd::SavePreset`] or [`ConsoleCmd::DeletePreset`] and comes back as the host's
//! next catalog push.
//!
//! The editor shows every row a preset can hold, at the preset's value over the global one.
//! A change is recorded as an override ([`SettingsOverlay::absorb`]); X clears one, so the
//! row follows the global value again.

use super::settings::rows::{adjust, advanced, overrides_row, preset_field, preset_rows, row_spec};
use crate::glyphs::{Hint, HintKey};
use crate::model::ConsoleCmd;
use crate::pointer::Pointer;
use crate::screens::{Ctx, EditField, Outbox, Screen, ScreenView, TextEntry};
use crate::theme::Fonts;
use crate::widgets::{blurb, field_key, type_text, Entry, ListMsg, MenuList, RowSpec};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use pf_client_core::presets::SettingsOverlay;
use skia_safe::{Canvas, Rect};

fn save(id: &str, name: &str, overlay: &SettingsOverlay) -> ConsoleCmd {
    ConsoleCmd::SavePreset {
        id: id.into(),
        name: name.into(),
        overrides: serde_json::to_value(overlay).unwrap_or_default(),
    }
}

/// `“name”`, as every preset screen names it.
fn quoted(name: &str) -> String {
    format!("\u{201c}{name}\u{201d}")
}

// --- the menu --------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Item {
    Edit,
    Rename,
    Pin,
    Delete,
}

const ITEMS: [Item; 4] = [Item::Edit, Item::Rename, Item::Pin, Item::Delete];

pub(crate) struct PresetMenu {
    id: String,
    name: String,
    list: MenuList,
    /// Delete fires on the second press.
    armed: bool,
    /// When `name` was last read back from the store: a rename lands through the host.
    read_at: f64,
}

impl PresetMenu {
    pub(crate) fn new(id: String, name: String) -> PresetMenu {
        PresetMenu {
            id,
            name,
            list: MenuList::new(),
            armed: false,
            read_at: 0.0,
        }
    }

    /// Shared by pad and pointer. Arming is per row: focus on any other row disarms Delete.
    fn dispatch(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        let item = ITEMS[self.list.cursor];
        if item != Item::Delete {
            self.armed = false;
        }
        match msg {
            ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
            ListMsg::None => pulse,
            ListMsg::Activate => self.run(item, ctx, fx),
        }
    }

    fn run(&mut self, item: Item, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        match item {
            Item::Edit => {
                let overlay = (ctx.store.preset_overrides())
                    .remove(&self.id)
                    .unwrap_or_default();
                let edit = PresetEdit::new(self.id.clone(), self.name.clone(), overlay);
                fx.push(Screen::PresetEdit(edit));
            }
            Item::Rename => {
                let overlay = (ctx.store.preset_overrides())
                    .remove(&self.id)
                    .unwrap_or_default();
                let name = PresetName::rename(self.id.clone(), self.name.clone(), overlay);
                fx.push(Screen::PresetName(name));
            }
            Item::Pin => {
                let pin = super::pin_hosts::PinHostsScreen::new(self.id.clone(), self.name.clone());
                fx.push(Screen::PinHosts(pin));
            }
            Item::Delete if !self.armed => {
                self.armed = true;
                return Some(MenuPulse::Boundary);
            }
            Item::Delete => {
                fx.cmds.push(ConsoleCmd::DeletePreset {
                    id: self.id.clone(),
                });
                fx.toast = Some(format!("Deleted {}", quoted(&self.name)));
                fx.pop();
            }
        }
        Some(MenuPulse::Confirm)
    }
}

impl ScreenView for PresetMenu {
    fn title(&self) -> String {
        format!("Preset {}", quoted(&self.name))
    }

    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        let (msg, pulse) = self.list.menu(ev, ITEMS.len());
        self.dispatch(msg, pulse, ctx, fx)
    }

    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let (msg, pulse) = self.list.pointer(p, ITEMS.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.dispatch(msg, pulse, ctx, fx);
        true
    }

    fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        vec![
            Hint::new(HintKey::Confirm, "Select"),
            Hint::new(HintKey::Back, "Back"),
        ]
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
        if (ctx.t - self.read_at).abs() >= 0.5 {
            self.read_at = ctx.t;
            if let Some((_, name)) = ctx
                .store
                .presets()
                .into_iter()
                .find(|(id, _)| *id == self.id)
            {
                self.name = name;
            }
        }
        let rows: Vec<RowSpec> = (ITEMS.iter())
            .map(|item| {
                let mut row = RowSpec::action(
                    match item {
                        Item::Edit => "Edit settings\u{2026}",
                        Item::Rename => "Rename\u{2026}",
                        Item::Pin => "Pin to hosts\u{2026}",
                        Item::Delete if self.armed => "Delete preset \u{2014} press again",
                        Item::Delete => "Delete preset",
                    },
                    true,
                );
                row.danger = *item == Item::Delete;
                row
            })
            .collect();
        self.list.render(canvas, rect, &rows, fonts, k, dt, true);
    }

    fn press(&mut self) {
        self.list.dip();
    }

    fn pan(&mut self, p: Pointer) -> bool {
        self.list.pan(p)
    }
}

// --- the name --------------------------------------------------------------------------------

pub(crate) struct PresetName {
    /// `None` makes a new preset.
    id: Option<String>,
    overlay: SettingsOverlay,
    name: String,
    list: MenuList,
    keyboard: TextEntry,
    editing: bool,
    error: Option<String>,
}

impl PresetName {
    /// A new preset: it overrides nothing until its editor sets something.
    pub(crate) fn new() -> PresetName {
        PresetName {
            id: None,
            overlay: SettingsOverlay::default(),
            name: String::new(),
            list: MenuList::new(),
            keyboard: TextEntry::default(),
            editing: true,
            error: None,
        }
    }

    pub(crate) fn rename(id: String, name: String, overlay: SettingsOverlay) -> PresetName {
        PresetName {
            id: Some(id),
            overlay,
            name,
            ..PresetName::new()
        }
    }

    /// A name is up to 40 printable characters.
    fn admits(text: &str, ch: char) -> bool {
        !ch.is_control() && text.chars().count() < 40
    }

    /// Typing clears a taken-name error; deleting leaves it up.
    fn typed(&mut self, before: usize) {
        if self.name.len() > before {
            self.error = None;
        }
    }

    /// A name no other preset has, any case. A new preset opens its editor next.
    fn save(&mut self, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            self.editing = true;
            return Some(MenuPulse::Boundary);
        }
        let taken = (ctx.store.presets().iter())
            .any(|(id, other)| Some(id) != self.id.as_ref() && other.eq_ignore_ascii_case(&name));
        if taken {
            self.error = Some(format!("A preset is already called {}.", quoted(&name)));
            self.editing = false;
            return Some(MenuPulse::Boundary);
        }
        match &self.id {
            Some(id) => {
                fx.cmds.push(save(id, &name, &self.overlay));
                fx.pop();
            }
            None => {
                let id = pf_client_core::presets::new_preset_id();
                fx.cmds.push(save(&id, &name, &self.overlay));
                let edit = PresetEdit::new(id, name, self.overlay.clone());
                fx.replace(Screen::PresetEdit(edit));
            }
        }
        Some(MenuPulse::Confirm)
    }
}

impl ScreenView for PresetName {
    fn title(&self) -> String {
        match self.id {
            Some(_) => "Rename Preset".into(),
            None => "New Preset".into(),
        }
    }

    fn editing(&self) -> bool {
        self.editing
    }

    fn edit_field(&self) -> Option<EditField> {
        let field = EditField {
            label: "Name".into(),
            text: self.name.clone(),
            digits: false,
        };
        self.editing.then_some(field)
    }

    fn text_input(&mut self, typed: &str) {
        if self.editing {
            let before = self.name.len();
            type_text(&mut self.name, typed, Self::admits);
            self.typed(before);
        }
    }

    /// Return closes the keyboard onto Save; the next Return saves.
    fn edit_key(&mut self, key: crate::input::Key, _ctx: &mut Ctx) -> bool {
        if !self.editing {
            return false;
        }
        let Some(entry) = field_key(key, &mut self.name) else {
            return false;
        };
        if entry != Entry::Stay {
            self.editing = false;
            self.list.cursor = 1;
        }
        true
    }

    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if self.editing {
            let before = self.name.len();
            let (entry, pulse) = (self.keyboard).menu(ev, ctx.device, &mut self.name, Self::admits);
            self.typed(before);
            return match entry {
                Entry::Stay => pulse,
                Entry::Close => {
                    self.editing = false;
                    pulse
                }
                Entry::Done => self.save(ctx, fx),
            };
        }
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        let (msg, pulse) = self.list.menu(ev, 2);
        match msg {
            ListMsg::Activate if self.list.cursor == 0 => {
                self.editing = true;
                Some(MenuPulse::Confirm)
            }
            ListMsg::Activate => self.save(ctx, fx),
            _ => pulse,
        }
    }

    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let before = self.name.len();
        let tray = self
            .editing
            .then(|| (self.keyboard).pointer(p, ctx.device, &mut self.name, Self::admits));
        if let Some(entry) = tray.flatten() {
            self.typed(before);
            match entry {
                None => return false,
                Some(Entry::Stay) => {}
                Some(Entry::Close) => self.editing = false,
                Some(Entry::Done) => {
                    self.save(ctx, fx);
                }
            }
            return true;
        }
        let (msg, pulse) = self.list.pointer(p, 2);
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        if matches!(msg, ListMsg::Activate) {
            if self.list.cursor == 0 {
                self.editing = true;
            } else {
                self.save(ctx, fx);
            }
        }
        true
    }

    fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if self.editing {
            return TextEntry::hints(ctx.device, "Save");
        }
        vec![
            Hint::new(HintKey::Confirm, "Select"),
            Hint::new(HintKey::Back, "Cancel"),
        ]
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
        let text = self.error.as_deref().unwrap_or(
            "Name it for what it is for: Couch, Low latency, Travel. The name shows on the \
             cards it is pinned to.",
        );
        let below = blurb(canvas, fonts, text, rect, k);
        self.keyboard.seat(self.editing, ctx.device, dt);
        let tray_h = self.keyboard.reserve(k);
        let list_rect = Rect::from_ltrb(
            rect.left,
            below.top,
            rect.right,
            rect.bottom - tray_h as f32,
        );
        let mut field = RowSpec::field("Name", self.name.clone(), "Preset name");
        field.caret = self.editing;
        let action = if self.id.is_some() { "Save" } else { "Create" };
        let rows = [field, RowSpec::action(action, !self.name.trim().is_empty())];
        self.list
            .render(canvas, list_rect, &rows, fonts, k, dt, !self.editing);
        self.keyboard.render(canvas, fonts, rect, k);
    }

    fn press(&mut self) {
        self.list.dip();
    }

    fn pan(&mut self, p: Pointer) -> bool {
        self.list.pan(p)
    }
}

// --- the settings ----------------------------------------------------------------------------

pub(crate) struct PresetEdit {
    id: String,
    name: String,
    overlay: SettingsOverlay,
    list: MenuList,
}

impl PresetEdit {
    pub(crate) fn new(id: String, name: String, overlay: SettingsOverlay) -> PresetEdit {
        PresetEdit {
            id,
            name,
            overlay,
            list: MenuList::new(),
        }
    }

    /// `f` sees `ctx.settings` as the preset streams them: the overlay over the global.
    fn in_preset<R>(&self, ctx: &mut Ctx, f: impl FnOnce(&mut Ctx) -> R) -> R {
        let preset = self.overlay.apply(ctx.settings);
        let global = std::mem::replace(ctx.settings, preset);
        let out = f(ctx);
        *ctx.settings = global;
        out
    }

    /// The preset's rows. An advanced one shows under Show advanced, or while this preset
    /// overrides it, so no override is ever out of sight.
    fn rows(&self, ctx: &mut Ctx) -> Vec<(&'static str, super::settings::rows::RowId)> {
        let overlay = &self.overlay;
        self.in_preset(ctx, |ctx| {
            let all = ctx.settings.show_advanced;
            (preset_rows(ctx).into_iter())
                .filter(|(_, id)| all || !advanced(*id) || overrides_row(*id, overlay))
                .collect()
        })
    }

    /// Step the row at the preset's value and keep what changed as its override. Android's
    /// safe-area mode is a device setting no preset holds: a Resolution step that moved only
    /// that steps on, or Native could never be left.
    fn step(
        &mut self,
        id: super::settings::rows::RowId,
        delta: i32,
        wrap: bool,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        let before = self.overlay.apply(ctx.settings);
        let overlay = &self.overlay;
        let after = self.in_preset(ctx, |ctx| {
            if !adjust(id, delta, wrap, ctx) {
                return None;
            }
            let mut held = overlay.clone();
            held.absorb(&before, ctx.settings);
            let unheld = id == super::settings::rows::RowId::Resolution && held == *overlay;
            if unheld && !adjust(id, delta, wrap, ctx) {
                return None;
            }
            Some(ctx.settings.clone())
        });
        let Some(after) = after else {
            return Some(MenuPulse::Boundary);
        };
        self.overlay.absorb(&before, &after);
        fx.cmds.push(save(&self.id, &self.name, &self.overlay));
        Some(MenuPulse::Move)
    }
}

impl ScreenView for PresetEdit {
    fn title(&self) -> String {
        format!("Preset {}", quoted(&self.name))
    }

    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        let rows = self.rows(ctx);
        let focused = rows.get(self.list.cursor).map(|(_, id)| *id);
        if ev == MenuEvent::Tertiary {
            let field = focused.and_then(preset_field)?;
            if !self.overlay.clear(field) {
                return Some(MenuPulse::Boundary);
            }
            fx.cmds.push(save(&self.id, &self.name, &self.overlay));
            return Some(MenuPulse::Confirm);
        }
        let (msg, pulse) = self.list.menu(ev, rows.len());
        let (delta, wrap) = match msg {
            ListMsg::Adjust(delta) => (delta, false),
            ListMsg::Activate => (1, true),
            ListMsg::None => return pulse,
        };
        let Some(id) = focused else {
            return pulse;
        };
        self.step(id, delta, wrap, ctx, fx)
    }

    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let rows = self.rows(ctx);
        let (msg, pulse) = self.list.pointer(p, rows.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        if let (ListMsg::Activate, Some((_, id))) = (msg, rows.get(self.list.cursor)) {
            self.step(*id, 1, true, ctx, fx);
        }
        true
    }

    fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        let _ = ctx;
        vec![
            Hint::new(HintKey::Adjust, "Change"),
            Hint::new(HintKey::Tertiary, "Use global"),
            Hint::new(HintKey::Back, "Done"),
        ]
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
        let below = blurb(
            canvas,
            fonts,
            "A dot marks what this preset changes; everything else follows Settings. X puts a \
             row back on the global value.",
            rect,
            k,
        );
        let overlay = self.overlay.clone();
        let listed = self.rows(ctx);
        let rows: Vec<RowSpec> = self.in_preset(ctx, |ctx| {
            let mut last = "";
            (listed.into_iter())
                .map(|(tab, id)| {
                    let mut spec = row_spec(id, ctx, &[], &Default::default());
                    spec.header = (tab != last).then_some(tab);
                    last = tab;
                    spec.dot = overrides_row(id, &overlay);
                    spec
                })
                .collect()
        });
        self.list.cursor = self.list.cursor.min(rows.len().saturating_sub(1));
        let list_rect = Rect::from_ltrb(rect.left, below.top, rect.right, rect.bottom);
        self.list
            .render(canvas, list_rect, &rows, fonts, k, dt, true);
    }

    fn press(&mut self) {
        self.list.dip();
    }

    fn pan(&mut self, p: Pointer) -> bool {
        self.list.pan(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screens::settings::rows::RowId;
    use crate::screens::Nav;
    use pf_client_core::menu_nav::MenuDir;
    use pf_client_core::trust::Settings;

    fn with_ctx<R>(f: impl FnOnce(&mut Ctx) -> R) -> R {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx::test(&mut settings, &library);
        f(&mut ctx)
    }

    fn saved(fx: &Outbox) -> SettingsOverlay {
        match fx.cmds.last() {
            Some(ConsoleCmd::SavePreset { overrides, .. }) => {
                serde_json::from_value(overrides.clone()).unwrap()
            }
            other => panic!("no save: {other:?}"),
        }
    }

    /// A step on a row becomes that row's override and saves; X puts it back on the global.
    #[test]
    fn a_step_overrides_and_x_clears() {
        let mut s = PresetEdit::new("p1".into(), "Couch".into(), SettingsOverlay::default());
        let hdr = with_ctx(|ctx| s.rows(ctx).iter().position(|(_, id)| *id == RowId::Hdr));
        s.list.cursor = hdr.expect("HDR is a preset row");
        let mut fx = Outbox::default();
        with_ctx(|ctx| s.menu(MenuEvent::Confirm, ctx, &mut fx));
        let o = saved(&fx);
        assert_eq!(o.hdr_enabled, Some(!Settings::default().hdr_enabled));
        let mut fx = Outbox::default();
        with_ctx(|ctx| s.menu(MenuEvent::Tertiary, ctx, &mut fx));
        assert_eq!(saved(&fx).hdr_enabled, None, "back on the global value");
    }

    /// On a phone the step from Native lands on the first size, not on the safe-area slot
    /// the preset cannot hold.
    #[test]
    fn a_preset_resolution_steps_past_the_safe_area_slot() {
        let mut s = PresetEdit::new("p1".into(), "Couch".into(), SettingsOverlay::default());
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let phone = crate::screens::Device {
            platform: crate::platform::Platform::Android,
            fallback_ui: true,
            ..crate::screens::Device::test()
        };
        let mut ctx = Ctx {
            device: &phone,
            ..Ctx::test(&mut settings, &library)
        };
        let mut fx = Outbox::default();
        let pulse = s.step(RowId::Resolution, 1, false, &mut ctx, &mut fx);
        assert!(matches!(pulse, Some(MenuPulse::Move)));
        let o = saved(&fx);
        assert_eq!((o.width, o.height), (Some(1280), Some(720)));
    }

    /// Only rows a preset can hold are listed, each under its section.
    #[test]
    fn the_editor_lists_only_preset_rows() {
        let s = PresetEdit::new("p1".into(), "Couch".into(), SettingsOverlay::default());
        let rows = with_ctx(|ctx| s.rows(ctx));
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|(_, id)| preset_field(*id).is_some()));
        assert!(!rows.iter().any(|(_, id)| *id == RowId::Palette));
    }

    /// A new name saves an empty preset and opens its editor; a taken name stays put.
    #[test]
    fn naming_a_new_preset_opens_its_editor() {
        let mut s = PresetName::new();
        s.text_input("Travel");
        let mut fx = Outbox::default();
        with_ctx(|ctx| s.menu(MenuEvent::Back, ctx, &mut fx));
        with_ctx(|ctx| {
            s.menu(MenuEvent::Move(MenuDir::Down), ctx, &mut fx);
            s.menu(MenuEvent::Confirm, ctx, &mut fx)
        });
        assert!(
            matches!(fx.cmds.last(), Some(ConsoleCmd::SavePreset { name, .. }) if name == "Travel")
        );
        assert!(
            matches!(fx.nav, Some(Nav::Replace(ref b)) if matches!(**b, Screen::PresetEdit(_)))
        );
    }

    /// Delete arms first, then sends one DeletePreset and leaves.
    #[test]
    fn delete_arms_before_it_fires() {
        let mut s = PresetMenu::new("p1".into(), "Couch".into());
        s.list.cursor = 3;
        let mut fx = Outbox::default();
        with_ctx(|ctx| s.menu(MenuEvent::Confirm, ctx, &mut fx));
        assert!(fx.cmds.is_empty());
        with_ctx(|ctx| s.menu(MenuEvent::Confirm, ctx, &mut fx));
        assert_eq!(fx.cmds, vec![ConsoleCmd::DeletePreset { id: "p1".into() }]);
        assert!(matches!(fx.nav, Some(Nav::Pop)));
    }

    /// A pointer press on another row disarms Delete, as a pad move off it does.
    #[test]
    fn a_press_on_another_row_disarms_delete() {
        use crate::pointer::{Pointer, PointerKind};
        let mut s = PresetMenu::new("p1".into(), "Couch".into());
        let fonts = crate::theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
        let rect = Rect::from_xywh(0.0, 0.0, 1280.0, 800.0);
        with_ctx(|ctx| s.render(surface.canvas(), rect, 1.0, 1.0 / 60.0, &fonts, ctx));
        let mut fx = Outbox::default();
        let mut tap = |s: &mut PresetMenu, row: usize| {
            let r = s.list.row_rect(row).expect("the menu drew");
            let p = Pointer {
                x: f64::from(r.center_x()),
                y: f64::from(r.center_y()),
                kind: PointerKind::Press,
            };
            with_ctx(|ctx| s.pointer(p, ctx, &mut fx));
        };
        tap(&mut s, 3);
        tap(&mut s, 0);
        tap(&mut s, 3);
        assert!(
            !fx.cmds
                .iter()
                .any(|c| matches!(c, ConsoleCmd::DeletePreset { .. })),
            "Edit in between: this Delete only arms"
        );
        assert!(s.armed);
    }
}
