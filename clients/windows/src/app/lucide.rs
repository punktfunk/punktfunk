//! The shell's icons: [Lucide](https://lucide.dev), the same marks the console (gamepad UI) and
//! the GTK shell draw, from the same masters in `assets/lucide`.
//!
//! Inside a control a mark is a glyph of Lucide's own icon FONT, from the codepoints in
//! [`pf_client_core::lucide`]: a text block in that font takes the control's foreground, so a
//! mark on an accent button comes out white and one on a plain button in the theme's text
//! colour, a vector at any DPI. Where WinUI wants an `Icon` (a navigation item), the mark is the
//! master's path stroked as SVG in the theme's text colour: reactor's icon font is fixed, and
//! it holds other glyphs at Lucide's codepoints. No PNGs: a bitmap keeps one colour on every
//! theme.
//!
//! The font: `ms-appx:///` resolves to the folder of the exe. A dev `cargo build` stages it
//! into `target/<profile>/Assets/` (`build.rs`); a shipped build carries it from
//! `clients/windows/packaging/assets/`, which `pack-msix.ps1` copies into the layout's
//! `Assets\`, and the installer and the portable zip pack that same layout.

use pf_client_core::lucide as set;
use pf_fluent::Palette;
use windows_reactor::{ColorScheme, EncodedImage, FontWeight, Icon, TextBlock};

/// The font as XAML names it: the file, then `#` and the family inside it. Both halves matter —
/// a family name alone would only find a font installed system-wide, which this one is not.
pub(super) const FAMILY: &str = "ms-appx:///Assets/lucide.ttf#lucide";

/// One mark's glyph. An unknown name yields an empty string, which draws nothing — a control
/// without its icon, rather than a "missing glyph" box. [`every_name_ships`] keeps the shell
/// from relying on that.
pub(super) fn glyph(name: &str) -> String {
    set::glyph(name).map(String::from).unwrap_or_default()
}

/// A mark for a control's content, at the 20 px of WinUI's `FontIcon`. It takes the
/// control's foreground.
pub(super) fn mark(name: &str) -> TextBlock {
    TextBlock::new()
        .text(glyph(name))
        .font_family(FAMILY)
        .font_size(20.0)
        .font_weight(FontWeight::NORMAL)
}

/// A mark as an `Icon`: the master's path at Lucide's stroke width, in the theme's primary
/// text colour (`TextFillColorPrimary`).
pub(super) fn icon(name: &str, pal: &Palette) -> Icon {
    let ink = match pal.scheme {
        ColorScheme::Dark => "#FFFFFF",
        ColorScheme::Light => "#000000\" stroke-opacity=\"0.894",
    };
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 24 24\" fill=\"none\" \
         stroke=\"{ink}\" stroke-width=\"2\" stroke-linecap=\"round\" stroke-linejoin=\"round\">\
         <path d=\"{}\"/></svg>",
        set::path(name).unwrap_or_default()
    );
    Icon::image_svg(EncodedImage::new(svg.into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every mark this shell asks for by name really ships, and each resolves to its own glyph
    /// in the Private Use Area. A typo would otherwise be one silently empty control.
    #[test]
    fn every_name_ships() {
        let named = [
            "arrow-left",
            "arrow-right",
            "arrow-up-down",
            "chart-column",
            "check",
            "circle-help",
            "copy",
            "ellipsis",
            "gamepad-2",
            "keyboard",
            "maximize",
            pf_client_core::library::DESKTOP_ICON,
            "plus",
            "refresh-cw",
            "rotate-cw",
            "save",
            "send",
            "settings",
            "trash-2",
            "volume-2",
            "x",
        ];
        for name in named {
            let g = set::glyph(name).unwrap_or_else(|| panic!("the icon set has no '{name}'"));
            assert!(
                ('\u{e000}'..='\u{f8ff}').contains(&g),
                "{name}: {g:?} is not a private-use codepoint"
            );
        }
        assert!(glyph("no-such-icon").is_empty());
    }

    /// Every slot the ring can hold has a glyph — the ring's discs are the one place this shell
    /// cannot fall back to a word, because the word is what the mark replaced.
    #[test]
    fn every_ring_slot_has_a_glyph() {
        use pf_client_core::overlay_actions::{catalogue, slot_icon, OverlayConfig, RingPlatform};
        let cfg = OverlayConfig::platform_default(RingPlatform::Desktop);
        for group in catalogue(&cfg, RingPlatform::Desktop) {
            for entry in group.entries {
                if entry.id.is_empty() {
                    continue; // the empty slot draws the plus, checked below
                }
                let name = slot_icon(&entry.id, false).unwrap_or_else(|| panic!("{}", entry.id));
                assert!(
                    set::glyph(name).is_some(),
                    "{}: the font has no '{name}'",
                    entry.id
                );
            }
        }
        for name in ["mic-off", "ellipsis", "plus"] {
            assert!(set::glyph(name).is_some(), "the font has no '{name}'");
        }
    }

    /// The family string names a FILE and a family inside it. A bare family name would silently
    /// fall back to the UI font and draw private-use boxes, which is the failure this catches.
    #[test]
    fn the_family_names_the_bundled_file() {
        assert!(FAMILY.starts_with("ms-appx:///Assets/"));
        assert!(FAMILY.ends_with(".ttf#lucide"));
    }
}
