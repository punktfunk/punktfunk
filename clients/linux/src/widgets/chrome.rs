//! The chrome every destination shares (design §2.1): a header whose title is the Hosts ·
//! Library switcher, the same switcher as a bottom bar once the window is narrow, and the
//! primary menu.

use adw::prelude::*;
use gtk::gio;

/// A destination's header and bottom bar, both switching `views`. `narrow` swaps the header's
/// switcher for the bar.
pub fn destination_header(
    views: &adw::ViewStack,
    narrow: &adw::Breakpoint,
) -> (adw::HeaderBar, adw::ViewSwitcherBar) {
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(
        &adw::ViewSwitcher::builder()
            .stack(views)
            .policy(adw::ViewSwitcherPolicy::Wide)
            .build(),
    ));
    let bar = adw::ViewSwitcherBar::builder().stack(views).build();
    narrow.add_setter(&header, "show-title", Some(&false.to_value()));
    narrow.add_setter(&bar, "reveal", Some(&true.to_value()));
    (header, bar)
}

/// The primary menu, the same on every destination.
pub fn primary_menu() -> gtk::MenuButton {
    let menu = gio::Menu::new();
    if cfg!(feature = "console") {
        menu.append(Some("Console UI"), Some("win.console"));
    }
    menu.append(Some("Preferences"), Some("win.preferences"));
    menu.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
    menu.append(Some("About Punktfunk"), Some("win.about"));
    gtk::MenuButton::builder()
        .child(&crate::widgets::lucide::row_icon("menu"))
        .menu_model(&menu)
        .primary(true)
        .tooltip_text("Main menu")
        .build()
}
