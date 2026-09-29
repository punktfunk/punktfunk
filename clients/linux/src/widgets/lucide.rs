//! The shell's icons: [Lucide](https://lucide.dev), the same marks the console (gamepad UI)
//! draws, from the same table — [`pf_client_core::lucide`], generated from
//! `assets/lucide/*.svg` by `scripts/gen-lucide-assets.sh`.
//!
//! GTK parses the path with `gsk::Path` and cairo strokes it at Lucide's native width 2 with
//! round caps and joins, which is exactly what Skia does on the console, so one mark cannot
//! come out differently on the two shells. The colour is the widget's own CSS `color`, which
//! GTK inherits down the tree: an icon in an ordinary row follows the theme's foreground and
//! an icon on a ring disc comes out white, because `.pf-ring-disc` says so — no per-site
//! colour and no light/dark variants to keep in step.
//!
//! Nothing is baked and nothing is registered: no gresource entry, no icon-theme lookup, no
//! new dependency. Adding a mark is dropping one SVG in `assets/lucide` and re-running the
//! script.

use gtk::cairo;
use gtk::prelude::*;

/// A square widget `size` px across drawing the Lucide mark `name`, centred, in the colour it
/// inherits. An unknown name draws nothing rather than a placeholder — a missing mark reads
/// as a plain button, a "broken image" reads as a bug in the app.
pub fn icon(name: &str, size: i32) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_width(size)
        .content_height(size)
        // Its natural size, never its parent's: a `DrawingArea` fills by default, and the
        // draw below scales the mark to whatever box it is given — so an icon left to expand
        // comes out as large as the disc it sits on.
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        // Decoration: a click belongs to the row or button this sits in, never to the mark.
        .can_target(false)
        .build();
    let data = pf_client_core::lucide::path(name);
    area.set_draw_func(move |area, cr, w, h| {
        let Some(data) = data else { return };
        let Ok(path) = gtk::gsk::Path::parse(data) else {
            return;
        };
        let c = area.color();
        // The 24-unit box scaled to fit and centred; the stroke is 2 of those units, so it
        // thickens with the mark exactly as Lucide draws it.
        let f = f64::from(w.min(h)) / 24.0;
        cr.save().ok();
        cr.translate(
            (f64::from(w) - 24.0 * f) / 2.0,
            (f64::from(h) - 24.0 * f) / 2.0,
        );
        cr.scale(f, f);
        cr.set_source_rgba(
            f64::from(c.red()),
            f64::from(c.green()),
            f64::from(c.blue()),
            f64::from(c.alpha()),
        );
        cr.set_line_width(2.0);
        cr.set_line_cap(cairo::LineCap::Round);
        cr.set_line_join(cairo::LineJoin::Round);
        path.to_cairo(cr);
        cr.stroke().ok();
        cr.restore().ok();
    });
    area
}

/// The 16 px mark a list row or a header button carries — the size GTK's own `-symbolic`
/// icons render at, so a Lucide row suffix lines up with the rest of the shell.
pub fn row_icon(name: &str) -> gtk::DrawingArea {
    icon(name, 16)
}

/// An icon-only button, the shape [`gtk::Button::from_icon_name`] gives.
pub fn button(name: &str) -> gtk::Button {
    gtk::Button::builder().child(&row_icon(name)).build()
}

#[cfg(test)]
mod tests {
    /// Every mark this shell asks for by name really ships. A typo would otherwise be an
    /// invisible icon on exactly one row, which is the kind of thing nobody notices for a
    /// release; the shared table's own test proves the path data itself draws.
    #[test]
    fn every_mark_the_shell_names_ships() {
        for name in [
            "chevron-right",
            "refresh-cw",
            "check",
            "gamepad-2",
            "plus",
            "ellipsis",
            pf_client_core::library::DESKTOP_ICON,
        ] {
            assert!(
                pf_client_core::lucide::path(name).is_some(),
                "the icon set does not ship '{name}'"
            );
        }
    }
}
