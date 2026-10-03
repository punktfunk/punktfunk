//! One title as the Games tab draws it, after the Apple library's card.
//!
//! The 2:3 poster carries its store badge top-left and the Resume badge top-right, or the
//! title's files badge when it isn't installed or is downloading. Under it
//! run the title on two held lines, the host's OS mark and name, and a caption where the row
//! or the sort has one. The focus plate is the lift, so the card never grows. The host desk
//! tile, the section heading and the focused-title band sit here too.

use super::draw_poster_placeholder;
use crate::library::{store_label, LibraryGame, DESKTOP_ID};
use crate::model::HostRow;
use crate::theme::{accent, art_sampling, fg, fill, on_accent, Fonts, PanelStroke, W};
use skia_safe::{Canvas, Color4f, Image, Matrix, RRect, Rect, TileMode};

/// Poster corner, design units.
pub(super) const COVER_CORNER: f64 = 11.0;
const TEXT_GAP: f64 = 8.0;
const TITLE: f64 = 13.0;
const TITLE_LINE: f64 = 16.5;
const META: f64 = 11.5;
const META_LINE: f64 = 15.0;
/// The host desk tile, design units.
pub(super) const DESK_W: f64 = 224.0;
pub(super) const DESK_H: f64 = 124.0;
/// Height under the poster, design units: two title lines, the host line, a caption line.
pub(super) fn text_h(caption: bool) -> f64 {
    TEXT_GAP + 2.0 * TITLE_LINE + 2.0 + META_LINE + if caption { META_LINE } else { 0.0 }
}

/// What one card shows besides its poster.
pub(super) struct Card<'a> {
    pub game: &'a LibraryGame,
    pub art: Option<&'a Image>,
    pub title: &'a str,
    pub host: &'a HostRow,
    pub caption: Option<&'a str>,
    pub focused: bool,
}

impl Card<'_> {
    /// The card in `r`: the poster `ch` px tall, the text under it. `alpha` is the entrance
    /// fade, carried by every piece's paint.
    pub(super) fn paint(
        &self,
        canvas: &Canvas,
        fonts: &Fonts,
        r: Rect,
        ch: f64,
        k: f64,
        alpha: f32,
    ) {
        let cover = Rect::from_xywh(r.left, r.top, r.width(), ch as f32);
        paint_cover(canvas, fonts, self.game, self.art, cover, k, alpha);
        store_badge(canvas, fonts, self.game, cover, k, false, alpha);
        if self.game.running {
            running_badge(canvas, fonts, cover, k, alpha);
        } else if let Some(b) = self.game.install.as_ref().and_then(|f| {
            pf_client_core::library::TileBadge::for_title(Some(&f.install), f.download.as_ref())
        }) {
            files_badge(canvas, fonts, cover, k, alpha, &b);
        }
        let ink = |a: f32| {
            let c = fg(a);
            Color4f::new(c.r, c.g, c.b, c.a * alpha)
        };
        let (x, w) = (f64::from(r.left), f64::from(r.width()));
        let mut top = f64::from(cover.bottom) + TEXT_GAP * k;
        let (one, two) = two_lines(fonts, self.title, TITLE * k, w);
        let title = ink(if self.focused { 1.0 } else { 0.72 });
        for line in [one.as_str(), two.as_str()] {
            let base = top + TITLE_LINE * 0.78 * k;
            fonts.draw_clipped(canvas, line, x, base, W::Medium, TITLE * k, title, w);
            top += TITLE_LINE * k;
        }
        top += 2.0 * k;
        let base = top + META_LINE * 0.76 * k;
        let side = META * k;
        let mark = Rect::from_xywh(
            x as f32,
            (base - side * 0.86) as f32,
            side as f32,
            side as f32,
        );
        let name_x = match crate::os_marks::os_mark(&self.host.os, mark) {
            Some(path) => {
                canvas.draw_path(&path, &fill(ink(0.6)));
                x + side + 5.0 * k
            }
            None => x,
        };
        let name = &self.host.name;
        fonts.draw_clipped(
            canvas,
            name,
            name_x,
            base,
            W::Regular,
            META * k,
            ink(0.6),
            w - (name_x - x),
        );
        if let Some(caption) = self.caption {
            let base = base + META_LINE * k;
            fonts.draw_clipped(canvas, caption, x, base, W::Regular, META * k, ink(0.45), w);
        }
    }
}

/// `text` broken for two lines at `max_w`: at the last space that fits, else inside the
/// word. The second line is whatever is left; the draw ellipsizes it.
pub(super) fn two_lines(fonts: &Fonts, text: &str, size: f64, max_w: f64) -> (String, String) {
    let fits = |s: &str| f64::from(fonts.measure(s, W::Medium, size)) <= max_w;
    if fits(text) {
        return (text.to_string(), String::new());
    }
    let mut end = 0;
    for (i, _) in text.match_indices(' ') {
        if !fits(&text[..i]) {
            break;
        }
        end = i;
    }
    if end == 0 {
        // One word longer than the card: break where it stops fitting.
        end = (text.char_indices().map(|(i, _)| i).skip(1))
            .take_while(|&i| fits(&text[..i]))
            .last()
            .unwrap_or(text.len());
    }
    let (a, b) = text.split_at(end);
    (a.trim_end().to_string(), b.trim_start().to_string())
}

/// A poster cropped to fill `cell`, or the placeholder. `alpha` is the poster's own.
pub(super) fn paint_cover(
    canvas: &Canvas,
    fonts: &Fonts,
    game: &LibraryGame,
    art: Option<&Image>,
    cell: Rect,
    k: f64,
    alpha: f32,
) {
    let corner = (COVER_CORNER * k) as f32;
    let rr = RRect::new_rect_xy(cell, corner, corner);
    let Some(img) = art else {
        canvas.save();
        canvas.clip_rrect(rr, None, true);
        draw_poster_placeholder(canvas, fonts, Some(game), cell, k, alpha);
        canvas.restore();
        return;
    };
    let src = crop(img, cell);
    // Shader rrect, not clip+image: coverage AA, no clip-stack per card.
    let (sx, sy) = (cell.width() / src.width(), cell.height() / src.height());
    let mut local = Matrix::scale((sx, sy));
    local.post_translate((cell.left - src.left * sx, cell.top - src.top * sy));
    if let Some(shader) = img.to_shader(
        (TileMode::Clamp, TileMode::Clamp),
        art_sampling(),
        Some(&local),
    ) {
        // Opaque: Skia modulates the shader by paint alpha; 0 draws nothing.
        let mut p = crate::theme::shaded();
        p.set_shader(shader);
        p.set_alpha_f(alpha);
        canvas.draw_rrect(rr, &p);
    }
}

/// The centred part of `img` with `cell`'s aspect.
pub(super) fn crop(img: &Image, cell: Rect) -> Rect {
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let aspect = cell.width() / cell.height();
    if iw / ih > aspect {
        let sw = ih * aspect;
        Rect::from_xywh((iw - sw) / 2.0, 0.0, sw, ih)
    } else {
        let sh = iw / aspect;
        Rect::from_xywh(0.0, (ih - sh) / 2.0, iw, sh)
    }
}

/// The store's name in `cover`'s top-left. A launcher's is the accent; any other is a dark
/// wash with white ink, since it sits on cover art the palette has no say over. `big` is
/// the shelf's size; `alpha` is the cover's, so the badge never arrives ahead of it.
pub(super) fn store_badge(
    canvas: &Canvas,
    fonts: &Fonts,
    game: &LibraryGame,
    cover: Rect,
    k: f64,
    big: bool,
    alpha: f32,
) {
    if game.id == DESKTOP_ID {
        return;
    }
    let s = if big { 1.15 } else { 1.0 };
    let label = store_label(&game.store);
    let size = 11.0 * k * s;
    let tw = f64::from(fonts.measure(label, W::SemiBold, size));
    let (bw, bh, inset) = (tw + 14.0 * k * s, 19.0 * k * s, 7.0 * k * s);
    let (x, y) = (f64::from(cover.left) + inset, f64::from(cover.top) + inset);
    let (face, ink) = if game.launcher {
        (accent(1.0), on_accent())
    } else {
        (
            Color4f::new(0.0, 0.0, 0.0, 0.58),
            Color4f::new(1.0, 1.0, 1.0, 1.0),
        )
    };
    let (face, ink) = (faded(face, alpha), faded(ink, alpha));
    let r = Rect::from_xywh(x as f32, y as f32, bw as f32, bh as f32);
    canvas.draw_rrect(
        RRect::new_rect_xy(r, r.height() / 2.0, r.height() / 2.0),
        &fill(face),
    );
    fonts.draw(
        canvas,
        label,
        x + 7.0 * k * s,
        y + bh / 2.0 + size * 0.36,
        W::SemiBold,
        size,
        ink,
    );
}

/// `RESUME` in `rect`'s top-right, opposite the store badge. Fixed green on every palette;
/// `alpha` is the cover's.
pub(super) fn running_badge(canvas: &Canvas, fonts: &Fonts, rect: Rect, k: f64, alpha: f32) {
    const LABEL: &str = "RESUME";
    let size = 11.0 * k;
    let tw = f64::from(fonts.measure(LABEL, W::SemiBold, size));
    let (bw, bh) = (tw + 14.0 * k, 19.0 * k);
    let pad = 7.0 * k;
    let x = f64::from(rect.right) - pad - bw;
    let y = f64::from(rect.top) + pad;
    let r = Rect::from_xywh(x as f32, y as f32, bw as f32, bh as f32);
    canvas.draw_rrect(
        RRect::new_rect_xy(r, r.height() / 2.0, r.height() / 2.0),
        &fill(faded(crate::theme::ONLINE_GREEN, alpha)),
    );
    let ink = Color4f::new(0.04, 0.10, 0.05, alpha);
    fonts.draw(
        canvas,
        LABEL,
        x + 7.0 * k,
        y + bh / 2.0 + size * 0.36,
        W::SemiBold,
        size,
        ink,
    );
}

/// A title's files in `rect`'s top-right: a mark and `42 %`, `26 GB` or `Not installed`, on
/// a dark plate that reads over any poster.
fn files_badge(
    canvas: &Canvas,
    fonts: &Fonts,
    rect: Rect,
    k: f64,
    alpha: f32,
    badge: &pf_client_core::library::TileBadge,
) {
    let size = 11.0 * k;
    let mark = 12.0 * k;
    let tw = f64::from(fonts.measure(&badge.text, W::SemiBold, size));
    let (bw, bh) = (tw + mark + 18.0 * k, 19.0 * k);
    let pad = 7.0 * k;
    let x = f64::from(rect.right) - pad - bw;
    let y = f64::from(rect.top) + pad;
    let r = Rect::from_xywh(x as f32, y as f32, bw as f32, bh as f32);
    canvas.draw_rrect(
        RRect::new_rect_xy(r, r.height() / 2.0, r.height() / 2.0),
        &fill(Color4f::new(0.0, 0.0, 0.0, 0.72 * alpha)),
    );
    let ink = Color4f::new(1.0, 1.0, 1.0, 0.94 * alpha);
    let icon = match badge.icon {
        "alert" => "triangle-alert",
        name => name,
    };
    if let Some(icon) = crate::icons::by_name(icon) {
        let (cx, cy) = (x + 6.0 * k + mark / 2.0, y + bh / 2.0);
        crate::icons::draw_icon(canvas, icon, cx as f32, cy as f32, mark as f32, ink);
    }
    fonts.draw(
        canvas,
        &badge.text,
        x + mark + 11.0 * k,
        y + bh / 2.0 + size * 0.36,
        W::SemiBold,
        size,
        ink,
    );
}

/// `c` at `alpha` of its own opacity.
fn faded(c: Color4f, alpha: f32) -> Color4f {
    Color4f::new(c.r, c.g, c.b, c.a * alpha)
}

/// A host's desk in the Desktops row: OS mark and presence up top, name and what OK does
/// below — "Resume <title>" in green when the host has a game up.
pub(super) fn desk_tile(canvas: &Canvas, fonts: &Fonts, h: &HostRow, r: Rect, k: f64) {
    crate::theme::panel(canvas, r, 14.0, None, PanelStroke::Plain(0.10), k as f32);
    let pad = 16.0 * k;
    let (l, t) = (f64::from(r.left) + pad, f64::from(r.top) + pad);
    let side = 26.0 * k;
    let mark = Rect::from_xywh(l as f32, t as f32, side as f32, side as f32);
    match crate::os_marks::os_mark(&h.os, mark) {
        Some(path) => {
            canvas.draw_path(&path, &fill(fg(0.92)));
        }
        None => {
            if let Some(icon) = crate::icons::by_name(crate::library::DESKTOP_ICON) {
                let (cx, cy) = (mark.center_x(), mark.center_y());
                crate::icons::draw_icon(canvas, icon, cx, cy, side as f32, fg(0.92));
            }
        }
    }
    let dot = if h.online {
        crate::theme::live()
    } else {
        fg(0.28)
    };
    let rd = 4.0 * k;
    let at = (
        (f64::from(r.right) - pad - rd) as f32,
        (t + side / 2.0) as f32,
    );
    canvas.draw_circle(at, rd as f32, &fill(dot));
    let w = f64::from(r.width()) - 2.0 * pad;
    let status_base = f64::from(r.bottom) - pad - 2.0 * k;
    let (line, ink) = if h.running.is_empty() {
        ("Desktop".to_string(), fg(0.62))
    } else {
        (format!("Resume {}", h.running), crate::theme::live())
    };
    fonts.draw_clipped(canvas, &line, l, status_base, W::Medium, 13.5 * k, ink, w);
    let name_base = status_base - 21.0 * k;
    fonts.draw_clipped(canvas, &h.name, l, name_base, W::Bold, 17.0 * k, fg(1.0), w);
}

/// A section's name over its row: semibold, tracked, in the secondary ink.
pub(super) fn heading(canvas: &Canvas, fonts: &Fonts, label: &str, x: f64, baseline: f64, k: f64) {
    fonts.draw_tracked(
        canvas,
        label,
        x,
        baseline,
        W::SemiBold,
        14.0 * k,
        1.1 * k,
        fg(0.62),
    );
}

/// The one line of text a band draws, centred in `max_w`, ellipsized.
/// What the focused-title band says: the title, the provenance line under it, and the
/// cache note that leads that line.
pub(super) struct TitleBand<'a> {
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub note: Option<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A title breaks at a space that fits; a single long word breaks inside itself; a
    /// short one stays on one line.
    #[test]
    fn a_title_breaks_for_two_lines() {
        let fonts = crate::theme::build_fonts().unwrap();
        let w = f64::from(fonts.measure("Hollow Knight", W::Medium, 13.0)) + 1.0;
        assert_eq!(
            two_lines(&fonts, "Hollow Knight", 13.0, w),
            ("Hollow Knight".into(), String::new())
        );
        let (a, b) = two_lines(&fonts, "Hollow Knight Silksong", 13.0, w);
        assert_eq!((a.as_str(), b.as_str()), ("Hollow Knight", "Silksong"));
        let (a, b) = two_lines(&fonts, "Supercalifragilistic", 13.0, 40.0);
        assert!(!a.is_empty() && !b.is_empty(), "{a:?} {b:?}");
        assert_eq!(format!("{a}{b}"), "Supercalifragilistic");
    }
}
