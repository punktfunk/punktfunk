//! The five category pages. Each lays its fields out in groups; [`Build::put`] decides where a
//! field goes from its spec: its group, the page's Advanced tier, or nowhere in this scope.

mod audio;
mod controllers;
mod display;
mod general;
mod input;

pub use audio::audio;
pub use controllers::controllers;
pub use display::display;
pub use general::general;
pub use input::input;

use super::choice::{cap_captions, ChoiceRow};
use super::field::Field;
use super::spec::{Page, Spec};
use super::DeviceProbes;
use crate::store::Store;
use crate::trust::Settings;
use adw::prelude::*;
use pf_client_core::presets::SettingsOverlay;
use std::rc::Rc;

/// What the pages are built from, and what they leave behind for the dialog to bind.
pub struct Build<'a> {
    pub dialog: adw::PreferencesDialog,
    /// Pickers as in-window subpages (gamescope).
    pub inline: bool,
    pub preset_scope: bool,
    /// The preset's overrides, in preset scope.
    pub overlay: Option<&'a SettingsOverlay>,
    pub store: &'a Rc<Store>,
    /// The values the rows open on: the defaults, or the preset over them.
    pub seed: &'a Settings,
    pub probes: &'a DeviceProbes,
    pub gamepads: &'a crate::gamepad::GamepadService,
    pub fields: Vec<Rc<Field>>,
    pub tiers: Vec<Tier>,
    pub show_advanced: Option<adw::SwitchRow>,
}

impl Build<'_> {
    pub fn choice(&self, spec: &Spec, options: &[&str]) -> ChoiceRow {
        ChoiceRow::new(&self.dialog, self.inline, spec.title, spec.caption, options)
    }

    pub fn page(&self, kind: Page, icon: &str) -> PageB {
        PageB {
            kind,
            page: adw::PreferencesPage::builder()
                // The name addresses the page (`set_visible_page_name`, the screenshot knob).
                .name(kind.title().to_lowercase())
                .title(kind.title())
                .icon_name(icon)
                .build(),
            advanced: Vec::new(),
            counted: Vec::new(),
            overridden: false,
        }
    }

    /// Put `field` in `group`, or behind Show advanced when its spec says so; nowhere when
    /// its layer does not show in this scope.
    pub fn put(&mut self, page: &mut PageB, group: Option<&adw::PreferencesGroup>, field: Field) {
        assert_eq!(
            field.spec.page, page.kind,
            "{} sits on its spec's page",
            field.spec.key
        );
        if !field.spec.layer.shown(self.preset_scope) {
            return;
        }
        let field = Rc::new(field);
        if field.spec.advanced {
            page.advanced.extend(field.widgets.iter().cloned());
            page.counted.push(field.clone());
            page.overridden |= self.overlay.is_some_and(|o| o.overrides(field.spec.key));
        } else {
            let group = group.expect("a basic row names its group");
            for w in &field.widgets {
                page.row(group, w);
            }
        }
        self.fields.push(field);
    }

    /// Close the page: its Advanced tier after the groups, captions capped, onto the dialog.
    pub fn finish(&mut self, page: PageB) {
        let tier = Tier::new(&page);
        cap_captions(page.page.upcast_ref());
        self.dialog.add(&page.page);
        self.tiers.push(tier);
    }
}

/// A page being built.
pub struct PageB {
    kind: Page,
    pub page: adw::PreferencesPage,
    advanced: Vec<gtk::Widget>,
    /// The advanced fields, counted while hidden when they hold a changed value.
    counted: Vec<Rc<Field>>,
    /// The preset overrides one of the advanced fields.
    overridden: bool,
}

impl PageB {
    /// A group, hidden until a row lands in it.
    pub fn group(&self, title: &str, note: &str) -> adw::PreferencesGroup {
        let g = adw::PreferencesGroup::builder().title(title).build();
        if !note.is_empty() {
            g.set_description(Some(note));
        }
        g.set_visible(false);
        self.page.add(&g);
        g
    }

    pub fn row(&self, group: &adw::PreferencesGroup, w: &impl IsA<gtk::Widget>) {
        group.add(w);
        group.set_visible(true);
    }

    /// A row that is not a setting, behind Show advanced.
    pub fn advanced_row(&mut self, w: &impl IsA<gtk::Widget>) {
        self.advanced.push(w.clone().upcast());
    }
}

/// One page's advanced rows: a group that follows Show advanced, and a row naming how many of
/// them hold a changed value while they are hidden.
pub struct Tier {
    group: adw::PreferencesGroup,
    /// Preset scope can leave a page with no advanced row; the group then stays hidden.
    has_rows: bool,
    note: adw::PreferencesGroup,
    pub note_row: adw::ActionRow,
    counted: Vec<Rc<Field>>,
    /// Preset scope with this preset overriding one of the rows: the group stays shown.
    overridden: bool,
}

impl Tier {
    fn new(p: &PageB) -> Tier {
        let group = adw::PreferencesGroup::builder().title("Advanced").build();
        for w in &p.advanced {
            group.add(w);
        }
        group.set_visible(false);
        let note_row = adw::ActionRow::builder().activatable(true).build();
        note_row.add_suffix(&crate::widgets::lucide::row_icon("chevron-right"));
        let note = adw::PreferencesGroup::new();
        note.add(&note_row);
        note.set_visible(false);
        p.page.add(&group);
        p.page.add(&note);
        Tier {
            group,
            has_rows: !p.advanced.is_empty(),
            note,
            note_row,
            counted: p.counted.clone(),
            overridden: p.overridden,
        }
    }

    pub fn show(&self, on: bool, preset_scope: bool) {
        self.group
            .set_visible(self.has_rows && (on || self.overridden));
        let n = if on || preset_scope {
            0
        } else {
            self.counted.iter().filter(|f| f.changed()).count()
        };
        self.note.set_visible(n > 0);
        self.note_row.set_title(&match n {
            1 => "1 advanced setting changed".to_string(),
            n => format!("{n} advanced settings changed"),
        });
    }
}
