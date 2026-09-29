//! Recolour the GTK shell from the Omarchy desktop theme.
//!
//! The theme itself — where it lives, how it parses, the contrast maths — is
//! `pf_client_core::omarchy`, shared with the session console. This file owns only the GTK
//! half: turning an [`OsTheme`] into libadwaita `@define-color`s and keeping them current.
//!
//! **Colours only.** The widget vocabulary stays Adwaita — this recolours the shell, it does
//! not reshape it. Rounded cards, boxed lists and the header bar are unchanged.

use gtk::{gdk, glib};
use pf_client_core::omarchy::{current, on_accent, present, readable, OsTheme};
use std::cell::{Cell, RefCell};

thread_local! {
    /// One provider, re-loaded in place on every change — adding a second would stack
    /// definitions and leak one per theme switch. `None` until [`install`] ran (a non-Omarchy
    /// box, or a headless path that never had a display).
    static PROVIDER: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
    /// The "Follow the Omarchy theme" setting, mirrored here so the 2 s tick and the
    /// preferences switch read one value ([`set_enabled`] is the only writer after install).
    static ENABLED: Cell<bool> = const { Cell::new(true) };
    /// What the provider currently holds: outer `None` = nothing applied yet, inner `None` =
    /// explicitly cleared. The distinction is what keeps a disabled tick from re-clearing —
    /// and re-forcing the colour scheme — every 2 seconds.
    static APPLIED: RefCell<Option<Option<OsTheme>>> = const { RefCell::new(None) };
}

/// libadwaita's named colours, redefined from the theme.
///
/// Surfaces are mixed out of the background/foreground pair — "toward the foreground" is
/// darker on a light theme and lighter on a dark one, so neither branch has to know which it
/// is. The accent is split because libadwaita spends `accent_color` on TEXT and
/// `accent_bg_color` on a fill: the fill keeps the theme's exact colour, and only the text
/// form is lifted until it reads at 3:1.
///
/// 🛑 `success_*`, `warning_*` and `destructive_*`/`error_*` are deliberately absent: a theme
/// with a red accent must not make "Unpair" and "Connect" the same colour. The console's
/// stylesheet records the same rule.
fn css(t: &OsTheme) -> String {
    let s = |k: f64| t.bg.mix(t.fg, k).hex();
    let (bg, fg) = (t.bg.hex(), t.fg.hex());
    let (surface, sidebar, header) = (s(0.07), s(0.03), s(0.05));
    format!(
        "@define-color window_bg_color {bg};\n\
         @define-color window_fg_color {fg};\n\
         @define-color view_bg_color {bg};\n\
         @define-color view_fg_color {fg};\n\
         @define-color headerbar_bg_color {header};\n\
         @define-color headerbar_fg_color {fg};\n\
         @define-color sidebar_bg_color {sidebar};\n\
         @define-color sidebar_fg_color {fg};\n\
         @define-color secondary_sidebar_bg_color {sidebar};\n\
         @define-color secondary_sidebar_fg_color {fg};\n\
         @define-color card_bg_color {surface};\n\
         @define-color card_fg_color {fg};\n\
         @define-color dialog_bg_color {surface};\n\
         @define-color dialog_fg_color {fg};\n\
         @define-color popover_bg_color {surface};\n\
         @define-color popover_fg_color {fg};\n\
         @define-color accent_color {text};\n\
         @define-color accent_bg_color {fill};\n\
         @define-color accent_fg_color {on};\n",
        text = readable(t.accent, t.bg, t.fg).hex(),
        fill = t.accent.hex(),
        on = on_accent(t.accent).hex(),
    )
}

/// Bring the provider in line with the setting and the file, if either changed.
fn apply_now() {
    let Some(provider) = PROVIDER.with(|p| p.borrow().clone()) else {
        return;
    };
    let want = if ENABLED.with(Cell::get) {
        current()
    } else {
        None
    };
    if APPLIED.with(|a| *a.borrow() == Some(want)) {
        return;
    }
    let style = adw::StyleManager::default();
    match &want {
        Some(t) => {
            provider.load_from_string(&css(t));
            style.set_color_scheme(if t.dark {
                adw::ColorScheme::ForceDark
            } else {
                adw::ColorScheme::ForceLight
            });
        }
        // Switched off, opted back out, or the file is mid-rewrite: drop our definitions and
        // hand the palette back to libadwaita.
        None => {
            provider.load_from_string("");
            style.set_color_scheme(adw::ColorScheme::Default);
        }
    }
    APPLIED.with(|a| *a.borrow_mut() = Some(want));
}

/// Apply the Omarchy theme, if this box has one, and keep following it. `enabled` seeds the
/// preference (`trust::Settings::follow_os_theme`); [`set_enabled`] tracks the switch after.
///
/// Does nothing at all off Omarchy: one `stat` is the whole cost on every other box, and no
/// timer is ever armed. Checking the state DIRECTORY rather than any theme file is what lets
/// a theme registered later be picked up by the poll without a restart.
pub fn install(enabled: bool) {
    thread_local! {
        static DONE: Cell<bool> = const { Cell::new(false) };
    }
    if DONE.with(|d| d.replace(true)) {
        return;
    }
    let Some(display) = gdk::Display::default() else {
        return;
    };
    if !present() {
        return;
    }
    let provider = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        // Above `app::CSS`, which sits at APPLICATION and *uses* `@accent_color`, and above
        // libadwaita's own sheet, which defines these same names at THEME priority.
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    PROVIDER.with(|p| *p.borrow_mut() = Some(provider));
    ENABLED.with(|e| e.set(enabled));
    apply_now();
    // A 2 s poll, not a `GFileMonitor`: `~/.local/state/omarchy/current` is a SYMLINK that
    // `omarchy-theme-set` re-points, so a monitor on the resolved path would watch the
    // PREVIOUS theme's file forever. The web console polls the same file on the same interval.
    glib::timeout_add_seconds_local(2, || {
        apply_now();
        glib::ControlFlow::Continue
    });
}

/// The preferences switch. Applies immediately — flipping it must not wait out the poll.
pub fn set_enabled(on: bool) {
    ENABLED.with(|e| e.set(on));
    apply_now();
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_client_core::omarchy::{parse_hex, parse_punktfunk_json};

    fn theme() -> OsTheme {
        OsTheme {
            dark: true,
            bg: parse_hex("#1a1b26").unwrap(),
            fg: parse_hex("#c0caf5").unwrap(),
            accent: parse_hex("#7aa2f7").unwrap(),
        }
    }

    #[test]
    fn the_semantic_colours_are_left_to_libadwaita() {
        // A red-accented theme must not make "Unpair" and "Connect" the same colour.
        let sheet = css(&OsTheme {
            accent: parse_hex("#f7768e").unwrap(),
            ..theme()
        });
        for name in [
            "success_color",
            "warning_color",
            "destructive_color",
            "error_color",
        ] {
            assert!(!sheet.contains(name), "{name} must keep libadwaita's value");
        }
    }

    #[test]
    fn every_emitted_value_is_a_plain_hex_literal() {
        // The injection guard, stated as a property: nothing reaches the stylesheet that
        // could close a declaration and open another.
        for line in css(&theme()).lines().filter(|l| !l.is_empty()) {
            let value = line.rsplit(' ').next().unwrap().trim_end_matches(';');
            assert!(parse_hex(value).is_some(), "not a hex literal: {line}");
        }
    }

    #[test]
    fn the_fill_keeps_the_themes_exact_accent() {
        // Everforest Light's shape: the accent is handsome as a fill and illegible as text.
        // The text form moves (see pf-client-core's `readable` tests); the fill must not.
        let raw = r##"{"schema":1,"mode":"light","background":"#fdf6e3","foreground":"#5c6a72","accent":"#dfa000"}"##;
        let t = parse_punktfunk_json(raw).unwrap();
        assert!(css(&t).contains("@define-color accent_bg_color #dfa000;"));
    }

    /// The load-bearing assumption, stated as a test: a `@define-color` in OUR provider
    /// reaches a widget styled by a DIFFERENT provider that only ever *names*
    /// `@accent_color` — which is exactly what `app::CSS` does. Every test above would still
    /// pass if this were false and the shell quietly ignored the theme.
    ///
    /// 🛑 GTK initialises ONCE per process, from ONE thread, and libtest gives every test its
    /// own thread — `--test-threads=1` included. So this crate's display tests must each run
    /// in their OWN process; whichever starts first wins and the rest panic with "Attempted
    /// to initialize GTK from two different threads". Run it by name:
    ///
    ///     cargo test -p punktfunk-client-linux -- --ignored the_theme_reaches_a_widget
    #[test]
    #[ignore = "needs a Wayland/X display"]
    fn the_theme_reaches_a_widget_through_the_app_stylesheet() {
        use gtk::prelude::*;
        gtk::init().expect("gtk init");
        let display = gdk::Display::default().expect("a display");

        // `app::CSS` as the shell installs it: it names the colour and never defines it.
        let app = gtk::CssProvider::new();
        app.load_from_string(".pf-probe { color: @accent_color; }");
        gtk::style_context_add_provider_for_display(
            &display,
            &app,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let ours = gtk::CssProvider::new();
        ours.load_from_string(&css(&theme()));
        gtk::style_context_add_provider_for_display(
            &display,
            &ours,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );

        let label = gtk::Label::new(None);
        label.add_css_class("pf-probe");
        let window = gtk::Window::new();
        window.set_child(Some(&label));
        window.present();
        // Style resolves when the widget is mapped, which happens on the loop, not on
        // `present`.
        while glib::MainContext::default().iteration(false) {}

        let c = label.color();
        let hex = pf_client_core::omarchy::Rgb(
            f64::from(c.red()),
            f64::from(c.green()),
            f64::from(c.blue()),
        )
        .hex();
        assert_eq!(
            hex, "#7aa2f7",
            "the theme's accent never reached the widget"
        );
    }
}
