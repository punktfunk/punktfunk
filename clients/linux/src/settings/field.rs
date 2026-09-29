//! One row bound to one setting, and the binder that writes each change as it happens: to the
//! defaults, or to the preset in scope. A write re-reads the file and moves that one field, so
//! whatever another process wrote while the dialog was open survives.

use super::choice::ChoiceRow;
use super::spec::Spec;
use crate::store::Store;
use crate::trust::Settings;
use adw::prelude::*;
use gtk::glib;
use pf_client_core::presets::SettingsOverlay;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

type Handler = Rc<dyn Fn()>;

pub struct Field {
    pub spec: &'static Spec,
    /// The row that carries the title, the override mark and its reset.
    pub row: adw::ActionRow,
    /// What the field puts on its page, in order, `row` among them.
    pub widgets: Vec<gtk::Widget>,
    seed: Box<dyn Fn(&Settings)>,
    write: Box<dyn Fn(&mut Settings)>,
    /// True while the row shows something a fresh install would not.
    changed: Box<dyn Fn() -> bool>,
    /// Calls the handler on each change the user makes.
    watch: Box<dyn Fn(Handler)>,
}

impl Field {
    pub fn new(
        spec: &'static Spec,
        row: &impl IsA<adw::ActionRow>,
        widgets: Vec<gtk::Widget>,
        seed: impl Fn(&Settings) + 'static,
        write: impl Fn(&mut Settings) + 'static,
        changed: impl Fn() -> bool + 'static,
        watch: impl Fn(Handler) + 'static,
    ) -> Field {
        Field {
            spec,
            row: row.clone().upcast(),
            widgets,
            seed: Box::new(seed),
            write: Box::new(write),
            changed: Box::new(changed),
            watch: Box::new(watch),
        }
    }

    /// A switch over one bool.
    pub fn switch(
        spec: &'static Spec,
        get: fn(&Settings) -> bool,
        set: fn(&mut Settings, bool),
    ) -> (Field, adw::SwitchRow) {
        let row = adw::SwitchRow::builder()
            .title(spec.title)
            .subtitle(spec.caption)
            .build();
        let fresh = get(&Settings::default());
        let r = row.clone();
        let field = Field::new(
            spec,
            &row,
            vec![row.clone().upcast()],
            {
                let r = r.clone();
                move |s| r.set_active(get(s))
            },
            {
                let r = r.clone();
                move |s| set(s, r.is_active())
            },
            {
                let r = r.clone();
                move || r.is_active() != fresh
            },
            move |f| {
                r.connect_active_notify(move |_| f());
            },
        );
        (field, row)
    }

    /// A picker over one value, by its place in the row's list.
    pub fn choice(
        spec: &'static Spec,
        row: &ChoiceRow,
        get: impl Fn(&Settings) -> u32 + 'static,
        set: impl Fn(&mut Settings, u32) + 'static,
    ) -> Field {
        let fresh = get(&Settings::default());
        let action = row
            .widget()
            .clone()
            .downcast::<adw::ActionRow>()
            .expect("both row shapes are action rows");
        Field::new(
            spec,
            &action,
            vec![row.widget().clone().upcast()],
            {
                let r = row.clone();
                move |s| r.set_selected(get(s))
            },
            {
                let r = row.clone();
                move |s| set(s, r.selected())
            },
            {
                let r = row.clone();
                move || r.selected() != fresh
            },
            {
                let r = row.clone();
                move |f| r.connect_changed(move |_| f())
            },
        )
    }

    pub fn changed(&self) -> bool {
        (self.changed)()
    }
}

pub struct Binder {
    store: Rc<Store>,
    /// The preset in scope, by id; `None` edits the defaults.
    preset: Option<String>,
    dialog: glib::WeakRef<adw::PreferencesDialog>,
    /// Set while the dialog moves rows itself, seeding or resetting, so nothing is written.
    quiet: Cell<bool>,
    /// The failed-write toast shows once per dialog.
    warned: Cell<bool>,
}

impl Binder {
    pub fn new(
        store: Rc<Store>,
        preset: Option<String>,
        dialog: &adw::PreferencesDialog,
    ) -> Rc<Binder> {
        Rc::new(Binder {
            store,
            preset,
            dialog: dialog.downgrade(),
            quiet: Cell::new(false),
            warned: Cell::new(false),
        })
    }

    /// Put each field at `s` without writing anything.
    pub fn seed(&self, fields: &[Rc<Field>], s: &Settings) {
        self.quietly(|| {
            for f in fields {
                (f.seed)(s);
            }
        });
    }

    fn quietly(&self, f: impl FnOnce()) {
        let was = self.quiet.replace(true);
        f();
        self.quiet.set(was);
    }

    /// Write each change to `field` as it happens. In preset scope a row the preset can
    /// override carries the override mark, with the reset that drops it.
    pub fn bind(self: &Rc<Self>, field: &Rc<Field>, overlay: Option<&SettingsOverlay>) {
        let to_preset = self.preset.is_some() && field.spec.layer.presetable();
        let mark = to_preset.then(|| {
            let overridden = overlay.is_some_and(|o| o.overrides(field.spec.key));
            self.mark(field, overridden)
        });
        let (binder, weak) = (Rc::downgrade(self), Rc::downgrade(field));
        (field.watch)(Rc::new(move || {
            let (Some(b), Some(f)) = (binder.upgrade(), weak.upgrade()) else {
                return;
            };
            if b.quiet.get() {
                return;
            }
            b.write(&f, to_preset);
            if let Some(show) = &mark {
                show();
            }
        }));
    }

    fn write(&self, field: &Field, to_preset: bool) {
        let result = match (&self.preset, to_preset) {
            (Some(id), true) => {
                let globals = self.store.settings().clone();
                self.store.update_presets(|catalog| {
                    if let Some(p) = catalog.presets.iter_mut().find(|p| &p.id == id) {
                        let mut v = p.overrides.apply(&globals);
                        (field.write)(&mut v);
                        p.overrides.pin(field.spec.key, &v);
                    }
                })
            }
            _ => {
                self.store.update_settings(|s| (field.write)(s));
                Ok(())
            }
        };
        self.report(result);
    }

    /// A settings write never fails loudly, so its latch is read after each one too.
    fn report(&self, result: anyhow::Result<()>) {
        let failed = result.is_err() || pf_client_core::trust::store_health::last_error().is_some();
        if let Err(e) = &result {
            tracing::warn!(error = %format!("{e:#}"), "preset catalog not saved");
        }
        if failed && !self.warned.replace(true) {
            if let Some(d) = self.dialog.upgrade() {
                d.add_toast(adw::Toast::new(
                    "Your changes aren\u{2019}t being saved \u{2014} Punktfunk can\u{2019}t \
                     write to its settings folder.",
                ));
            }
        }
    }

    /// The override mark: a dot and a reset, built on the first override so an unmarked row
    /// keeps its inset. Both sit left of the title: a suffix would move the control under the
    /// pointer between two clicks. Returns what shows the mark.
    fn mark(self: &Rc<Self>, field: &Rc<Field>, overridden: bool) -> Handler {
        let parts: Rc<RefCell<Option<(gtk::Box, gtk::Button)>>> = Rc::default();
        let (binder, weak) = (Rc::downgrade(self), Rc::downgrade(field));
        let show: Handler = Rc::new(move || {
            let Some(f) = weak.upgrade() else { return };
            if parts.borrow().is_some() {
                return;
            }
            let dot = gtk::Box::builder()
                .css_classes(["pf-override-dot"])
                .valign(gtk::Align::Center)
                .build();
            let reset = gtk::Button::builder()
                .child(&crate::widgets::lucide::row_icon("undo-2"))
                .tooltip_text("Reset to Default settings")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            f.row.add_prefix(&dot);
            f.row.add_prefix(&reset);
            let (binder, weak, held) = (binder.clone(), weak.clone(), Rc::downgrade(&parts));
            reset.connect_clicked(move |_| {
                let (Some(b), Some(f)) = (binder.upgrade(), weak.upgrade()) else {
                    return;
                };
                b.reset(&f);
                if let Some((dot, reset)) = held.upgrade().and_then(|p| p.borrow_mut().take()) {
                    f.row.remove(&dot);
                    f.row.remove(&reset);
                }
            });
            *parts.borrow_mut() = Some((dot, reset));
        });
        if overridden {
            show();
        }
        show
    }

    /// Drop the preset's override and show the value it inherits again.
    fn reset(&self, field: &Field) {
        let Some(id) = &self.preset else { return };
        let result = self.store.update_presets(|catalog| {
            if let Some(p) = catalog.presets.iter_mut().find(|p| &p.id == id) {
                p.overrides.clear(field.spec.key);
            }
        });
        self.report(result);
        let globals = self.store.settings().clone();
        self.quietly(|| (field.seed)(&globals));
    }
}

/// Keeps the fields and their binder for as long as the dialog lives: the handlers hold them
/// weakly, so a row never owns the closure that owns the row.
pub fn keep_alive(dialog: &adw::PreferencesDialog, binder: Rc<Binder>, fields: Vec<Rc<Field>>) {
    type Kept = (Rc<Binder>, Vec<Rc<Field>>);
    let kept: RefCell<Option<Kept>> = RefCell::new(Some((binder, fields)));
    dialog.connect_closed(move |_| drop(kept.borrow_mut().take()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::spec;
    use pf_client_core::presets::{PresetsFile, StreamPreset};

    fn hdr() -> (Rc<Field>, adw::SwitchRow) {
        let (field, row) = Field::switch(&spec::HDR, |s| s.hdr_enabled, |s, v| s.hdr_enabled = v);
        (Rc::new(field), row)
    }

    /// A row writes its own field when it changes, over what another writer saved meanwhile,
    /// and in preset scope it pins the preset and leaves the defaults. Writes the stores, so
    /// it refuses to run without a scratch `PUNKTFUNK_CONFIG_DIR`. Needs a display.
    #[test]
    #[ignore = "needs a Wayland/X display"]
    fn a_row_writes_its_field_to_the_layer_in_scope() {
        let scratch = std::env::var("PUNKTFUNK_CONFIG_DIR").unwrap_or_default();
        assert!(
            !scratch.is_empty(),
            "set PUNKTFUNK_CONFIG_DIR to a scratch dir"
        );
        assert!(gtk::init().is_ok() && adw::init().is_ok(), "no display");
        Settings::default().save();
        let preset = StreamPreset::new("Work");
        let id = preset.id.clone();
        let mut catalog = PresetsFile::load();
        catalog.presets = vec![preset];
        catalog.save().expect("save the catalog");
        let store = Store::open();
        let dialog = adw::PreferencesDialog::new();

        let (field, row) = hdr();
        let binder = Binder::new(store.clone(), None, &dialog);
        binder.seed(std::slice::from_ref(&field), &store.settings());
        binder.bind(&field, None);
        let mut other = Settings::load();
        other.bitrate_kbps = 9000;
        other.save();
        let was = row.is_active();
        row.set_active(!was);
        let s = Settings::load();
        assert_eq!(s.hdr_enabled, !was, "the row wrote its field");
        assert_eq!(s.bitrate_kbps, 9000, "and kept the other writer's");

        let globals = store.settings().clone();
        let (field, row) = hdr();
        let binder = Binder::new(store.clone(), Some(id), &dialog);
        binder.seed(std::slice::from_ref(&field), &globals);
        binder.bind(&field, Some(&SettingsOverlay::default()));
        let overrides = || PresetsFile::load().presets[0].overrides.clone();
        assert!(overrides().is_empty(), "seeding writes nothing");
        row.set_active(!globals.hdr_enabled);
        assert_eq!(overrides().hdr_enabled, Some(!globals.hdr_enabled));
        assert_eq!(
            Settings::load().hdr_enabled,
            globals.hdr_enabled,
            "the defaults stay"
        );
        binder.reset(&field);
        assert!(overrides().is_empty(), "the reset drops the pin");
        assert_eq!(
            row.is_active(),
            globals.hdr_enabled,
            "and shows the inherited value"
        );
    }
}
