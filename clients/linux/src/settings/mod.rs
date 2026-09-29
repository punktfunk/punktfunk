//! Preferences on the cross-client category map: General, Display, Input, Audio and
//! Controllers, each row's meaning in its caption. About stays in the primary menu.
//!
//! Every row writes when it changes, to the layer in scope: the defaults, or one preset's
//! overrides (design/client-settings-profiles.md §5.1). In preset scope only the rows a preset
//! can carry show, each at its effective value; a change pins the field even at the global's
//! value, and only the row's reset drops the pin.
//!
//! [`spec`] holds each row's key and text, [`field`] binds a row to its setting and writes it,
//! [`pages`] lays the rows out, and [`presets`] is the scope switcher.

mod about;
mod choice;
mod field;
mod pages;
mod presets;
pub mod quick_actions;
mod spec;
mod tables;

pub use about::show_about;

use crate::store::Store;
use adw::prelude::*;
use std::rc::Rc;

/// Which layer the dialog is editing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The defaults every preset inherits from.
    Defaults,
    /// One preset's overrides, by id.
    Preset(String),
}

/// Startup device probes for the pickers, filled by the app shell in the background: GPUs via
/// `punktfunk-session --list-adapters`, audio endpoints via the PipeWire registry. An empty
/// list hides its picker.
#[derive(Default)]
pub struct DeviceProbes {
    pub adapters: Vec<String>,
    pub speakers: Vec<pf_client_core::audio::AudioDevice>,
    pub mics: Vec<pf_client_core::audio::AudioDevice>,
}

/// The dialog in a given [`Scope`]. `on_scope` asks the app to open it again in another one:
/// a scope change closes this dialog, so one path builds the rows.
pub fn show_scoped(
    parent: &impl IsA<gtk::Widget>,
    store: Rc<Store>,
    gamepads: &crate::gamepad::GamepadService,
    probes: &DeviceProbes,
    scope: Scope,
    on_scope: impl Fn(Scope) + 'static,
    on_closed: impl Fn() + 'static,
) -> adw::PreferencesDialog {
    // A scope naming a deleted preset opens the defaults, as a dangling host binding does.
    let active = match &scope {
        Scope::Preset(id) => store.presets().find_by_id(id).cloned(),
        Scope::Defaults => None,
    };
    // Rows open on the effective value: the defaults with this preset's overrides on top, so a
    // row the preset leaves alone reads as the live global.
    let seed = match &active {
        Some(p) => p.overrides.apply(&store.settings()),
        None => store.settings().clone(),
    };
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Preferences");
    dialog.set_search_enabled(true);
    // Wide enough that the page switcher sits in the header: the dialog moves it to a bottom
    // bar below 110 pt per page, ≈ 733 px for five.
    dialog.set_content_width(830);
    let next: presets::NextScope = Rc::default();
    let mut b = pages::Build {
        dialog: dialog.clone(),
        inline: choice::gamescope_session(),
        preset_scope: active.is_some(),
        overlay: active.as_ref().map(|p| &p.overrides),
        store: &store,
        seed: &seed,
        probes,
        gamepads,
        fields: Vec::new(),
        tiers: Vec::new(),
        show_advanced: None,
    };
    let switcher = presets::scope_group(&b, &scope, active.as_ref(), &next, parent.as_ref());
    pages::general(&mut b, switcher);
    pages::display(&mut b);
    pages::input(&mut b);
    pages::audio(&mut b);
    pages::controllers(&mut b);

    let binder = field::Binder::new(
        store.clone(),
        active.as_ref().map(|p| p.id.clone()),
        &dialog,
    );
    binder.seed(&b.fields, &seed);
    for f in &b.fields {
        binder.bind(f, b.overlay);
    }
    // One switch drives every page's tier; each note row turns it on.
    let show = b.show_advanced.clone().expect("General has Show advanced");
    let (tiers, preset_scope) = (Rc::new(std::mem::take(&mut b.tiers)), b.preset_scope);
    for tier in tiers.iter() {
        tier.show(show.is_active(), preset_scope);
        let show = show.clone();
        tier.note_row
            .connect_activated(move |_| show.set_active(true));
    }
    show.connect_active_notify(move |r| {
        for tier in tiers.iter() {
            tier.show(r.is_active(), preset_scope);
        }
    });
    field::keep_alive(&dialog, binder, std::mem::take(&mut b.fields));

    dialog.connect_closed(move |_| {
        if let Some(scope) = next.borrow_mut().take() {
            on_scope(scope);
        }
        on_closed();
    });
    dialog.present(Some(parent));
    dialog
}
