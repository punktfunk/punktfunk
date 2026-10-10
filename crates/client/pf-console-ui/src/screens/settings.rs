//! Console settings: the couch-facing subset of the shared Settings store.
//!
//! One row per setting, in the sections of [`TABS`], named in a strip of tabs over the
//! list. Up from the first row, or B from any, reaches the sections; Left/Right there
//! switch them, Down returns. Left/right steps the focused value (clamped); A cycles
//! wrapping, except on Bitrate, where it types a rate; L1/R1 change section; B on the
//! sections closes. Every change writes the
//! store immediately so desktop shells round-trip the same file.
//! Each section remembers its cursor. Presets lists the catalog and ends on New preset;
//! a preset's own screens ([`super::preset`]) edit it through the host.
//!
//! Section names match `settings_sections` in `clients/shared/console-vectors.json`.
//! The rows themselves (ids, platform split, availability, spec, step) are [`rows`].

use crate::glyphs::{Hint, HintKey};
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox, Screen, ScreenView, TextEntry};
use crate::theme::Fonts;
use crate::widgets::{
    column, field_key, permits, type_text, Charset, Entry, ListMsg, MenuList, RowSpec, TabStrip,
    TAB_STRIP_H,
};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use pf_client_core::presets::SettingsOverlay;
use skia_safe::{Canvas, Rect};

pub(crate) mod rows;
use rows::*;

/// The explainer band under the rows, design units.
const DETAIL_H: f64 = crate::widgets::FOOT_DETAIL_H;

/// What the open typed field sets. A or Y opens it on Bitrate; Y on Resolution, for a width
/// and then a height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Typing {
    Bitrate,
    Width,
    /// The width typed a step earlier.
    Height(u32),
}

pub(crate) struct SettingsScreen {
    list: MenuList,
    strip: TabStrip,
    tab: usize,
    /// Per-tab cursor so a detour does not reset the one you left.
    tab_cursors: [usize; TABS.len()],
    /// `(id, name)`, re-read at most twice a second while the Presets tab is up: a preset
    /// saved from its own screens lands through the host.
    presets: Vec<(String, String)>,
    presets_at: f64,
    /// Each preset's overrides by id, loaded with `presets`: the rows say when a
    /// host's bound preset outranks the global value they show.
    overrides: std::collections::HashMap<String, SettingsOverlay>,
    /// D-pad focus on the section strip. TV remotes have no shoulders and no Tab key.
    strip_focus: bool,
    /// The open typed field and its digits.
    typing: Option<(Typing, String)>,
    /// The typed field's keyboard.
    keyboard: TextEntry,
}

impl SettingsScreen {
    pub(crate) fn new(store: &dyn crate::store::SettingsStore) -> SettingsScreen {
        let mut s = Self::with_presets(store.presets());
        s.overrides = store.preset_overrides();
        s
    }

    fn with_presets(presets: Vec<(String, String)>) -> SettingsScreen {
        SettingsScreen {
            list: MenuList::new(),
            strip: TabStrip::new(),
            tab: 0,
            tab_cursors: [0; TABS.len()],
            presets,
            presets_at: 0.0,
            overrides: Default::default(),
            strip_focus: false,
            typing: None,
            keyboard: TextEntry::default(),
        }
    }

    /// Digits only; four is 2000 Mbps and 8192 px, the ceilings.
    fn admits(text: &str, ch: char) -> bool {
        permits(Charset::Digits, ch) && text.chars().count() < 4
    }

    /// Close the field, or move from the width to the height. Empty or `0` abandons the edit:
    /// it is not Automatic or Native, the rows' first entries.
    fn commit_field(&mut self, ctx: &mut Ctx) {
        let Some((typing, text)) = self.typing.take() else {
            return;
        };
        let Some(n) = text.parse::<u32>().ok().filter(|n| *n > 0) else {
            return;
        };
        match typing {
            Typing::Bitrate => {
                ctx.write(|c| {
                    let ceiling_mbps = bitrate_ceiling_kbps(c.device.platform) / 1_000;
                    c.settings.bitrate_kbps = n.min(ceiling_mbps) * 1000;
                    true
                });
            }
            Typing::Width => self.typing = Some((Typing::Height(n), String::new())),
            Typing::Height(w) => {
                ctx.write(|c| {
                    let s = &mut *c.settings;
                    (s.width, s.height) = punktfunk_core::resolutions::custom(w, n, &s.codec);
                    s.match_window = false;
                    if c.device.platform == crate::platform::Platform::Android {
                        set_extra_bool(s, device_keys::SAFE_AREA_MODE, false);
                    }
                    true
                });
            }
        }
    }

    fn field_menu(&mut self, ev: MenuEvent, ctx: &mut Ctx) -> Option<MenuPulse> {
        let (_, text) = self.typing.as_mut()?;
        let (entry, pulse) = self.keyboard.menu(ev, ctx.device, text, Self::admits);
        if entry != Entry::Stay {
            self.commit_field(ctx);
        }
        pulse
    }

    /// The catalog as the store has it now, on the Presets tab.
    fn sync_presets(&mut self, ctx: &Ctx) {
        if self.tab != PRESETS_TAB || (ctx.t - self.presets_at).abs() < 0.5 {
            return;
        }
        self.presets_at = ctx.t;
        let presets = ctx.store.presets();
        if presets != self.presets {
            self.presets = presets;
            self.overrides = ctx.store.preset_overrides();
            self.list.cursor = self.list.cursor.min(self.presets.len());
        }
    }

    /// Filtered by [`row_on`] / [`row_applies`], and [`advanced`] rows by Show advanced: a tab
    /// hiding changed ones ends on [`RowId::AdvancedChanged`]. Presets comes from the catalog.
    fn row_ids(&self, ctx: &Ctx) -> Vec<RowId> {
        if self.tab != PRESETS_TAB {
            let offered = TABS[self.tab]
                .1
                .iter()
                .copied()
                .filter(|id| row_on(*id, ctx.device.platform) && row_applies(*id, ctx))
                // The Mac's picker stands in for the toggle, which presets keep.
                .filter(|id| !(*id == RowId::Fullscreen && is_mac(ctx)));
            if ctx.settings.show_advanced {
                return offered.collect();
            }
            let mut rows: Vec<RowId> = offered.filter(|id| !advanced(*id)).collect();
            if !changed_advanced(self.tab, ctx).is_empty() {
                rows.push(RowId::AdvancedChanged);
            }
            return rows;
        }
        if self.presets.is_empty() {
            vec![RowId::NewPreset]
        } else {
            (0..self.presets.len())
                .map(RowId::Preset)
                .chain([RowId::NewPreset])
                .collect()
        }
    }

    /// [`row_spec`], with the facts a row cannot know alone: how many hidden rows changed, and
    /// the Advanced heading over the first advanced row the tab shows.
    fn spec(&self, id: RowId, ids: &[RowId], ctx: &Ctx) -> RowSpec {
        let mut spec = row_spec(id, ctx, &self.presets, &self.overrides);
        if id == RowId::AdvancedChanged {
            spec.label = match changed_advanced(self.tab, ctx).len() {
                1 => "1 advanced setting changed".into(),
                n => format!("{n} advanced settings changed"),
            };
        } else if advanced(id) && ids.iter().find(|r| advanced(**r)) == Some(&id) {
            spec.header = Some("Advanced");
        }
        spec
    }

    /// Pull the cursor back. The smoothness buffer (and other writers) can shrink the list
    /// between frames.
    fn clamp_cursor(&mut self, len: usize) {
        if self.list.cursor >= len {
            self.list.jump_to(len.saturating_sub(1));
        }
    }

    #[cfg(test)]
    pub(crate) fn strip_focus_for_test(&self) -> bool {
        self.strip_focus
    }

    /// The section strip above the rows and the explainer band under them: the shell's
    /// trays run in this far so the rows bleed under both on one ramp. The keyboard
    /// lifts the bottom reach away, or the tray would slab the keys.
    pub(crate) fn pinned(&self, k: f64) -> (f32, f32) {
        let bottom = DETAIL_H * k * (1.0 - self.keyboard.seated().min(1.0));
        ((TAB_STRIP_H * k) as f32, bottom as f32)
    }

    /// The rows' band: under the section strip, above the explainer and the keyboard.
    fn list_rect(&self, rect: Rect, k: f64) -> Rect {
        Rect::from_ltrb(
            rect.left,
            rect.top + (TAB_STRIP_H * k) as f32,
            rect.right,
            rect.bottom - (DETAIL_H * k + self.keyboard.reserve(k)) as f32,
        )
    }

    /// Down from the shell's tabs lands on the section strip, not the rows under it.
    pub(crate) fn enter_from_top(&mut self) {
        self.strip_focus = true;
    }

    #[cfg(test)]
    pub(crate) fn tab_for_test(&self) -> usize {
        self.tab
    }

    /// Last-drawn row rect — hit tests press real coordinates.
    #[cfg(test)]
    pub(crate) fn row_rect_for_test(&self, i: usize) -> Option<Rect> {
        self.list.row_rect(i)
    }

    fn switch_tab(&mut self, delta: i32, ctx: &Ctx) -> Option<MenuPulse> {
        let n = TABS.len() as i32;
        self.show_tab((self.tab as i32 + delta).rem_euclid(n) as usize, ctx)
    }

    /// Park the outgoing tab's cursor, then jump. Pointer pills name a tab outright.
    fn show_tab(&mut self, tab: usize, ctx: &Ctx) -> Option<MenuPulse> {
        if tab >= TABS.len() {
            return None;
        }
        self.tab_cursors[self.tab] = self.list.cursor;
        self.tab = tab;
        // Remembered cursor can outlive a shorter tab (Presets catalog, smoothness buffer).
        let len = self.row_ids(ctx).len();
        self.list
            .jump_to(self.tab_cursors[self.tab].min(len.saturating_sub(1)));
        Some(MenuPulse::Move)
    }

    /// The D-pad on the section strip: Left/Right walk it, wrapping; Down returns to the
    /// rows; Up is the shell's tab strip.
    fn sections_menu(&mut self, ev: MenuEvent, ctx: &Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        match ev {
            MenuEvent::Back => {
                fx.pop();
                None
            }
            MenuEvent::JumpBack => self.switch_tab(-1, ctx),
            MenuEvent::JumpForward => self.switch_tab(1, ctx),
            MenuEvent::Confirm => {
                self.strip_focus = false;
                Some(MenuPulse::Move)
            }
            MenuEvent::Move(MenuDir::Down) => {
                self.strip_focus = false;
                Some(MenuPulse::Move)
            }
            MenuEvent::Move(MenuDir::Left) => self.switch_tab(-1, ctx),
            MenuEvent::Move(MenuDir::Right) => self.switch_tab(1, ctx),
            MenuEvent::Move(_) => Some(MenuPulse::Boundary),
            _ => None,
        }
    }

    /// Shared by pad and pointer so a click and an A press cannot drift apart.
    fn apply_row(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        ids: &[RowId],
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        // List shrank between clamp and here: drop the keypress, do not panic.
        let Some(&focused) = ids.get(self.list.cursor) else {
            return pulse;
        };
        // Presets navigate; they must not hit the settings save path.
        match focused {
            RowId::Preset(i) => {
                return match msg {
                    ListMsg::Activate => {
                        let (id, name) = self.presets[i].clone();
                        fx.push(Screen::PresetMenu(super::preset::PresetMenu::new(id, name)));
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            RowId::NewPreset => {
                return match msg {
                    ListMsg::Activate => {
                        fx.push(Screen::PresetName(super::preset::PresetName::new()));
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            RowId::Version => {
                return match msg {
                    ListMsg::Adjust(_) | ListMsg::Activate => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            RowId::LibrarySections => {
                return match msg {
                    ListMsg::Activate => {
                        fx.push(Screen::Customize(super::library::CustomizeScreen::new()));
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            RowId::Palette => {
                return match msg {
                    ListMsg::Activate => {
                        fx.push(Screen::Palette(super::palette::PaletteScreen::new(
                            &ctx.settings.ui_palette,
                        )));
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            // Action row: adjust is a boundary.
            RowId::QuickActions => {
                return match msg {
                    ListMsg::Activate => {
                        fx.push(Screen::RingEditor(Box::new(
                            super::ring_editor::RingEditorScreen::new(ctx),
                        )));
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            RowId::Controllers => {
                return match msg {
                    ListMsg::Activate => {
                        fx.tab = Some(crate::shell::Tab::Players);
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            // Shows the advanced rows and lands on the first changed one.
            RowId::AdvancedChanged => {
                return match msg {
                    ListMsg::Activate => {
                        let first = changed_advanced(self.tab, ctx).first().copied();
                        ctx.write(|c| {
                            c.settings.show_advanced = true;
                            true
                        });
                        let ids = self.row_ids(ctx);
                        if let Some(i) = first.and_then(|id| ids.iter().position(|r| *r == id)) {
                            self.list.jump_to(i);
                        }
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            RowId::StreamControls => {
                return match msg {
                    ListMsg::Activate => {
                        if let Some(screen) = super::licenses::LicensesScreen::controls(ctx) {
                            fx.push(Screen::Licenses(screen));
                        }
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            // The console draws the licences with the host's sections.
            RowId::Licenses => {
                return match msg {
                    ListMsg::Activate => {
                        let screen = super::licenses::LicensesScreen::new(fx);
                        fx.push(Screen::Licenses(screen));
                        pulse
                    }
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                };
            }
            // A remote has no Y: its OK types a rate, as a click does.
            RowId::Bitrate if matches!(msg, ListMsg::Activate) => {
                self.typing = Some((Typing::Bitrate, String::new()));
                return Some(MenuPulse::Confirm);
            }
            _ => {}
        }
        // Cursor moves must not touch the disk.
        match msg {
            ListMsg::Adjust(delta) => {
                if ctx.write(|c| adjust(focused, delta, false, c)) {
                    Some(MenuPulse::Move)
                } else {
                    Some(MenuPulse::Boundary)
                }
            }
            ListMsg::Activate => {
                ctx.write(|c| adjust(focused, 1, true, c));
                pulse
            }
            ListMsg::None => pulse,
        }
    }

    /// The section tabs on the margin, under the shell's tabs, and the explainer on the
    /// rows' inner column. The shell draws these over its trays, after [`Self::render`].
    pub(crate) fn render_pinned(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &Ctx,
    ) {
        let list_rect = self.list_rect(rect, k);
        let col = column(list_rect, k);
        let inner = f64::from(col.left) + 16.0 * k;
        let labels: Vec<&str> = TABS.iter().map(|(name, _)| *name).collect();
        self.strip.render(
            canvas,
            Rect::from_ltrb(rect.left, rect.top, rect.right, list_rect.top),
            &labels,
            self.tab,
            self.strip_focus,
            fonts,
            k,
            dt,
        );
        let ids = self.row_ids(ctx);
        let focused = ids.get(self.list.cursor).copied();
        let detail = focused.map_or("", |id| detail(id, ctx));
        // The explainer under the list, led by the row's mark; above the keyboard when up.
        crate::widgets::Foot {
            detail: Some(detail),
            mark: focused.map(row_icon),
            ..Default::default()
        }
        .paint(
            canvas,
            fonts,
            Rect::from_ltrb(rect.left, list_rect.bottom, rect.right, rect.bottom),
            (inner, f64::from(col.right)),
            k,
        );
    }
}

impl ScreenView for SettingsScreen {
    /// True while the typed field is open; the run loop keeps SDL text input started.
    fn editing(&self) -> bool {
        self.typing.is_some()
    }

    fn edit_field(&self) -> Option<crate::screens::EditField> {
        let (typing, text) = self.typing.as_ref()?;
        let label = match typing {
            Typing::Bitrate => "Bitrate in Mbps",
            Typing::Width => "Width in pixels",
            Typing::Height(_) => "Height in pixels",
        };
        crate::screens::EditField::new(label, text, true)
    }

    /// SDL text into the open field.
    fn text_input(&mut self, typed: &str) {
        if let Some((_, text)) = self.typing.as_mut() {
            type_text(text, typed, Self::admits);
        }
    }

    /// Every way out of the field commits it: the typed number is the setting.
    fn edit_key(&mut self, key: crate::input::Key, ctx: &mut Ctx) -> bool {
        let Some((_, text)) = self.typing.as_mut() else {
            return false;
        };
        let Some(entry) = field_key(key, text) else {
            return false;
        };
        if entry != Entry::Stay {
            self.commit_field(ctx);
        }
        true
    }

    /// OK went down on the focused row: it dips before the release acts.
    fn press(&mut self) {
        if self.strip_focus {
            self.strip.press();
        } else if self.typing.is_none() {
            self.list.dip();
        }
    }

    /// Strip first: pills sit above the list, so a press there is never a row.
    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let tray = (self.typing.as_mut())
            .and_then(|(_, text)| self.keyboard.pointer(p, ctx.device, text, Self::admits));
        if let Some(entry) = tray {
            if entry.is_some_and(|e| e != Entry::Stay) {
                self.commit_field(ctx);
            }
            return entry.is_some();
        }
        if let Some(tab) = self.strip.pointer(p) {
            if p.press() {
                self.show_tab(tab, ctx);
            }
            return true;
        }
        // A press on the rows takes D-pad focus back from the strip.
        if p.press() {
            self.strip_focus = false;
        }
        let ids = self.row_ids(ctx);
        self.clamp_cursor(ids.len());
        let (msg, pulse) = self.list.pointer(p, ids.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.apply_row(msg, pulse, &ids, ctx, fx);
        true
    }

    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if self.typing.is_some() {
            return self.field_menu(ev, ctx);
        }
        if self.strip_focus {
            return self.sections_menu(ev, ctx, fx);
        }
        match ev {
            MenuEvent::JumpBack => return self.switch_tab(-1, ctx),
            MenuEvent::JumpForward => return self.switch_tab(1, ctx),
            // Back and Up from row 0 focus the sections, not a boundary.
            MenuEvent::Back => {
                self.strip_focus = true;
                return Some(MenuPulse::Move);
            }
            MenuEvent::Move(MenuDir::Up) if self.list.cursor == 0 => {
                self.strip_focus = true;
                return Some(MenuPulse::Move);
            }
            _ => {}
        }
        let ids = self.row_ids(ctx);
        self.clamp_cursor(ids.len());
        // Y opens the typed bitrate or size.
        if ev == MenuEvent::Secondary {
            let typing = match ids.get(self.list.cursor) {
                Some(RowId::Bitrate) => Typing::Bitrate,
                Some(RowId::Resolution) => Typing::Width,
                _ => return None,
            };
            self.typing = Some((typing, String::new()));
            return Some(MenuPulse::Confirm);
        }
        let (msg, pulse) = self.list.menu(ev, ids.len());
        self.apply_row(msg, pulse, &ids, ctx, fx)
    }

    /// What a screen reader speaks: the section while the strip holds focus, otherwise the
    /// focused row's label and the value drawn beside it.
    fn announcement(&self, ctx: &Ctx) -> Option<String> {
        if self.strip_focus {
            return Some(format!("{} section", TABS[self.tab].0));
        }
        let ids = self.row_ids(ctx);
        let row = self.spec(*ids.get(self.list.cursor)?, &ids, ctx);
        Some(match row.value {
            Some(value) => format!("{}, {}", row.label, value),
            None => row.label,
        })
    }

    fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if let Some((typing, _)) = &self.typing {
            let done = if *typing == Typing::Width {
                "Next"
            } else {
                "Done"
            };
            return TextEntry::hints(ctx.device, done);
        }
        // Strip-focused: hints describe the D-pad, not the rows.
        if self.strip_focus {
            return vec![
                Hint::new(HintKey::Adjust, "Section"),
                Hint::new(HintKey::Confirm, "Rows"),
                Hint::new(HintKey::Back, "Done"),
            ];
        }
        let ids = self.row_ids(ctx);
        let mut hints = vec![Hint::new(HintKey::Shoulders, "Section")];
        hints.extend(match ids.get(self.list.cursor) {
            Some(RowId::Preset(_)) => vec![
                Hint::new(HintKey::Confirm, "Options\u{2026}"),
                Hint::new(HintKey::Back, "Done"),
            ],
            Some(RowId::NewPreset) => vec![
                Hint::new(HintKey::Confirm, "Create\u{2026}"),
                Hint::new(HintKey::Back, "Done"),
            ],
            Some(RowId::Version) | None => {
                vec![Hint::new(HintKey::Back, "Done")]
            }
            Some(
                RowId::Controllers
                | RowId::StreamControls
                | RowId::Licenses
                | RowId::LibrarySections
                | RowId::Palette
                | RowId::AdvancedChanged,
            ) => vec![
                Hint::new(HintKey::Confirm, "Open"),
                Hint::new(HintKey::Back, "Done"),
            ],
            Some(RowId::Bitrate) => vec![
                Hint::new(HintKey::Adjust, "Adjust"),
                Hint::new(HintKey::Confirm, "Custom rate"),
                Hint::new(HintKey::Back, "Done"),
            ],
            Some(RowId::Resolution) => vec![
                Hint::new(HintKey::Adjust, "Adjust"),
                Hint::new(HintKey::Secondary, "Type a size"),
                Hint::new(HintKey::Back, "Done"),
            ],
            Some(_) => vec![
                Hint::new(HintKey::Adjust, "Adjust"),
                Hint::new(HintKey::Confirm, "Change"),
                Hint::new(HintKey::Back, "Done"),
            ],
        });
        hints
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
        self.keyboard.seat(self.typing.is_some(), ctx.device, dt);
        self.sync_presets(ctx);
        let list_rect = self.list_rect(rect, k);
        let ids = self.row_ids(ctx);
        self.clamp_cursor(ids.len());
        let mut rows: Vec<RowSpec> = ids.iter().map(|id| self.spec(*id, &ids, ctx)).collect();
        // Field-open: the row being typed shows the digits so far and the caret.
        if let Some((typing, text)) = self.typing.as_ref() {
            let (row, value) = match typing {
                Typing::Bitrate if text.is_empty() => (RowId::Bitrate, "Mbps".into()),
                Typing::Bitrate => (RowId::Bitrate, format!("{text} Mbps")),
                Typing::Width if text.is_empty() => (RowId::Resolution, "Width".into()),
                Typing::Width => (RowId::Resolution, format!("{text} × \u{2026}")),
                Typing::Height(w) if text.is_empty() => {
                    (RowId::Resolution, format!("{w} × height"))
                }
                Typing::Height(w) => (RowId::Resolution, format!("{w} × {text}")),
            };
            if let Some(i) = ids.iter().position(|id| *id == row) {
                rows[i].value = Some(value);
                rows[i].value_dim = text.is_empty();
                rows[i].caret = true;
            }
        }
        // Rows run on under the section strip and the explainer, on the shell's trays;
        // with the keyboard up they stay in their band, or a tray would slab the keys.
        self.list.bleed = self.keyboard.seated() == 0.0;
        self.list.lift = self.keyboard.lift(rect, k);
        self.list.render(
            canvas,
            list_rect,
            &rows,
            fonts,
            k,
            dt,
            // No row focus ring while the tray or the strip holds it.
            self.typing.is_none() && !self.strip_focus,
        );
        self.keyboard.render(canvas, fonts, rect, k);
    }

    fn title(&self) -> String {
        "Settings".into()
    }

    fn pan(&mut self, p: Pointer) -> bool {
        self.list.pan(p)
    }
}

// `pub(crate)` so shell tests outside `screens` share `fake_home`.
#[cfg(test)]
pub(crate) mod tests;
