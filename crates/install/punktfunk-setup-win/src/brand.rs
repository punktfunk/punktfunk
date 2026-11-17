//! The wizard's brand: the mark's two circles, its lens highlight and the four "funk" letters,
//! from the website's path data (punktfunk-website `BrandMark.tsx` / `Wordmark.tsx`), painted
//! in shades of the Windows accent so the logo and the accent controls beside it read as one
//! palette. The circles are drawn live as ellipses (`wizard::welcome::lockup`) so they can
//! orbit; the lens and letters are SVG. `punktfunk.ico` stays brand and is staged to disk: the
//! window icon takes a file path.

use std::path::PathBuf;
use std::sync::OnceLock;

use pf_fluent::{Palette, Shade};
use windows_reactor::{Color, ColorScheme, EncodedImage};

/// The brand's own tones (`web/public/favicon.svg`), where Windows names no accent.
const DEEP: Color = Color::rgb(0x6c, 0x5b, 0xf3);
const LIGHT: Color = Color::rgb(0xa7, 0x9f, 0xf8);
const PALE: Color = Color::rgb(0xd2, 0xc9, 0xfb);

/// The circles' overlap, in the mark's 1000 box.
const LENS: &str = "M647.84,590.737c-64.853,17.403 -136.871,0.597 -187.885,-50.416c-51.013,-51.013 -67.819,-123.032 -50.416,-187.885c64.853,-17.403 136.871,-0.597 187.885,50.416c51.013,51.013 67.819,123.032 50.416,187.885Z";
/// The letters f, u, n, k in the wordmark's 579 × 136 box.
const LETTERS: [&str; 4] = [
    "M16.782,16.051l0,102.687l31.253,0l0,-35.563l73.436,0l0,-23.555l-73.436,0l0,-19.398l77.285,0l0,-24.171l-108.537,0Z",
    "M131.785,16.051l0,47.264c0.154,16.627 0.154,16.627 0.308,20.014c0.77,15.087 2.463,21.4 7.544,26.634c7.698,8.16 20.014,10.315 59.272,10.315c23.863,0 34.178,-0.616 43.415,-2.463c11.7,-2.463 19.552,-10.623 21.246,-22.169c0.77,-5.542 1.078,-12.316 1.232,-31.868l0,-47.727l-31.253,0l0,47.264c-0.462,15.703 -0.462,15.703 -0.616,19.706c-0.616,10.007 -2.617,14.163 -8.006,16.627c-4.618,2.155 -10.777,2.771 -26.634,2.771c-30.021,0 -33.87,-1.847 -35.563,-16.319c-0.462,-4.926 -0.616,-8.006 -0.77,-22.785l0,-47.264l-31.253,0Z",
    "M271.575,15.943l0,102.687l31.868,0l-0.77,-76.669l3.387,0l54.038,76.669l54.346,0l0,-102.687l-31.868,0l0.77,76.515l-3.233,0l-53.73,-76.515l-54.808,0Z",
    "M420.91,15.943l0,102.687l31.253,0l0,-39.258l17.089,0l46.032,39.258l47.418,0l-64.353,-52.344l59.426,-50.959l-47.88,0l-40.644,37.873l-17.089,0l0,-37.257l-31.253,0Z",
];
const ICON: &[u8] = include_bytes!("../../../../packaging/windows/branding/punktfunk.ico");

/// The lockup's four tones in one palette.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shades {
    pub deep: Color,
    pub light: Color,
    pub lens: Color,
    pub letters: Color,
}

impl Shades {
    /// Darker shades on light, lighter on dark, the way WinUI picks the accent button's fill
    /// (Dark1 / Light2), so one circle always matches it. The brand's tones without an accent.
    pub fn of(palette: &Palette) -> Shades {
        let dark = palette.scheme == ColorScheme::Dark;
        let pick = |on_light: Shade, on_dark: Shade, fallback: Color| {
            palette
                .accent(if dark { on_dark } else { on_light })
                .unwrap_or(fallback)
        };
        Shades {
            deep: pick(Shade::Dark1, Shade::Accent, DEEP),
            light: pick(Shade::Light1, Shade::Light2, LIGHT),
            lens: pick(Shade::Light3, Shade::Light3, PALE),
            letters: pick(Shade::Dark1, Shade::Light3, if dark { PALE } else { DEEP }),
        }
    }
}

fn svg(view_box: &str, path: &str, fill: Color) -> EncodedImage {
    let Color { r, g, b, .. } = fill;
    EncodedImage::new(
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{view_box}"><path d="{path}" fill="#{r:02x}{g:02x}{b:02x}"/></svg>"##
        )
        .into_bytes(),
    )
}

/// The lens highlight, cropped to the mark's own box.
pub fn lens(fill: Color) -> EncodedImage {
    svg("208.627 208.443 583.776 583.776", LENS, fill)
}

/// The i-th "funk" letter, each on the whole wordmark's canvas so they stack in place.
pub fn letter(fill: Color, i: usize) -> EncodedImage {
    svg("0 0 579 136", LETTERS[i], fill)
}

/// `punktfunk.ico` under `%LOCALAPPDATA%\punktfunk\setup-brand`, written once per run; a
/// size mismatch rewrites so a re-baked icon lands. `None` (no icon) when staging failed.
pub fn icon_path() -> Option<PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("punktfunk")
            .join("setup-brand");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("punktfunk.ico");
        let stale = std::fs::metadata(&path).map_or(true, |m| m.len() != ICON.len() as u64);
        if stale {
            std::fs::write(&path, ICON).ok()?;
        }
        Some(path)
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_letter_is_one_path_in_the_given_fill() {
        let bytes = letter(Color::rgb(0x00, 0x67, 0xc0), 3);
        let text = std::str::from_utf8(bytes.as_bytes()).unwrap();
        assert!(
            text.starts_with("<svg") && text.ends_with("</svg>"),
            "{text}"
        );
        assert!(text.contains(r##"fill="#0067c0""##), "{text}");
        assert!(text.contains(LETTERS[3]));
    }
}
