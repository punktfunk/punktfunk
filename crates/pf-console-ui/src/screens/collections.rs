//! The Games tab's Collections row: one tile per platform or store group.
//!
//! [`crate::collate`] groups the shelf's list. OK on a tile pushes a `LibraryScreen` with
//! the group as its filter rather than a filtered copy, so the art pump, fetch, and shared
//! model never learn a collection exists. Tile metrics match the home's host tile.

use crate::collate::{collate, worth_browsing, GroupBy, GroupKey, SortKey};
use crate::library::{initials, LibraryGame};
use crate::theme::{accent, art_sampling, fg, fill, stroke, Fonts, PanelStroke, W};
use skia_safe::{Canvas, Color4f, Image, Matrix, Point, RRect, Rect, TileMode};
use std::collections::HashMap;

// Same numbers as home.rs — a collection tile is a host tile.
pub(crate) const TILE_W: f64 = 340.0;
pub(crate) const TILE_H: f64 = 224.0;
pub(crate) const TILE_CORNER: f64 = 26.0;
/// Deck depth. Three reads as a shelf; more and the back cards vanish at tile size.
const FAN: usize = 3;

// Reserved even with no covers, so the caption corner and the title rail stay clear.
const FAN_W: f64 = 120.0;
const FAN_H: f64 = 130.0;
// Front cover; the rest are this rect, smaller. 2:3, matching the shelf posters it opens.
const COVER_H: f64 = 118.0;
const COVER_W: f64 = COVER_H * 2.0 / 3.0;
const COVER_CORNER: f64 = 11.0;
// Back cards: smaller, higher, further right, and turned. One cue alone is a smaller cover.
const FAN_SCALE_STEP: f64 = 0.07;
const FAN_DX: f64 = 18.0;
const FAN_DY: f64 = -7.0;
const FAN_ROT_DEG: f64 = 6.0;
// Hard round-rect, not a blur: three `MaskFilter`s per tile, fifteen on a live strip.
const PLATE_OUTSET: f64 = 3.0;
const PLATE_DX: f64 = 1.5;
const PLATE_DY: f64 = 2.0;
const PLATE_ALPHA: f32 = 0.38;

/// One tile: the filter its drill-in applies and the titles its deck fans.
pub(crate) struct Collection {
    pub key: GroupKey,
    pub label: String,
    pub count: usize,
    /// Indices into the shelf's games, front card first.
    pub fan: Vec<usize>,
}

/// The row's tiles; none unless [`worth_browsing`]. Launchers keep their own row.
pub(crate) fn collections(games: &[LibraryGame], sort: SortKey) -> Vec<Collection> {
    if !worth_browsing(games) {
        return Vec::new();
    }
    collate(games, sort, Some(GroupBy::Platform))
        .into_iter()
        .filter(|g| g.key != GroupKey::Launchers)
        .map(|g| Collection {
            count: g.games.len(),
            fan: g.games.iter().copied().take(FAN).collect(),
            key: g.key,
            label: g.label,
        })
        .collect()
}

/// Collection `c` in `rect`, its deck drawn from the posters `art` holds for `games`.
/// The focus plate is the tile's focus mark, so the tile draws no halo of its own.
pub(crate) fn paint_tile(
    canvas: &Canvas,
    fonts: &Fonts,
    c: &Collection,
    games: &[LibraryGame],
    art: &HashMap<String, Image>,
    rect: Rect,
    k: f64,
) {
    crate::theme::panel(
        canvas,
        rect,
        TILE_CORNER as f32,
        Some(accent(0.16)),
        PanelStroke::Gradient,
        k as f32,
    );
    crate::theme::panel_highlight(canvas, rect, TILE_CORNER as f32, k as f32);

    let pad = 20.0 * k;
    let (l, t) = (f64::from(rect.left) + pad, f64::from(rect.top) + pad);

    // Fixed-size deck, top-left; title runs the full inner width. Scale by the
    // tile, not `k` alone, or a squeezed tile lets the deck hit the title.
    let fk = (f64::from(rect.height()) / TILE_H).min(k);
    let front = Rect::from_xywh(
        l as f32,
        (t + (FAN_H - COVER_H) * fk) as f32,
        (COVER_W * fk) as f32,
        (COVER_H * fk) as f32,
    );
    let rr = RRect::new_rect_xy(
        front,
        (COVER_CORNER * fk) as f32,
        (COVER_CORNER * fk) as f32,
    );
    // Compact: a hole in the middle of the deck reads as a draw fault.
    let have: Vec<&Image> = (c.fan.iter())
        .filter_map(|&i| art.get(&games.get(i)?.id))
        .collect();
    // Never deeper than the group has titles. `have` is compact, so covers fill
    // from the front and ghosts trail.
    let slots = FAN.min(c.count.max(1));
    // Device-pixel floor, same as `panel_highlight`: a sub-pixel hairline smears.
    let hair = fk.max(1.0) as f32;
    // Back to front so `fan[0]` (sort-first, the group's face) lands on top.
    for n in (0..slots).rev() {
        canvas.save();
        canvas.concat(&fan_matrix(front, n, fk));
        match have.get(n) {
            Some(img) => {
                plate(canvas, rr, fk);
                draw_cover(canvas, img, front, rr);
                // Recede toward the ground (`shade` tracks the palette). A colour
                // filter would cost a `save_layer` per cover — the deck must not.
                if n > 0 {
                    canvas.draw_rrect(rr, &fill(crate::theme::shade(0.14 * n as f32)));
                }
                // Plate under, ink hairline on top: an edge on both palettes.
                // Stronger rim on back cards; they need the separation more.
                canvas.draw_rrect(
                    rr.with_inset((hair / 2.0, hair / 2.0)),
                    &stroke(fg(if n == 0 { 0.18 } else { 0.28 }), hair),
                );
            }
            // Front slot with no art: finished monogram, not a gap. Art-less ROMs stay this.
            None if n == 0 => {
                plate(canvas, rr, fk);
                draw_monogram(canvas, fonts, &c.label, front, rr, hair);
            }
            // Empty silhouette, no plate (nothing to cast a shadow). Keeps deck depth
            // so the tile does not change shape as posters arrive.
            None => {
                canvas.draw_rrect(rr, &fill(crate::theme::shade(0.10)));
                canvas.draw_rrect(
                    rr.with_inset((hair / 2.0, hair / 2.0)),
                    &stroke(fg(0.12), hair),
                );
            }
        }
        canvas.restore();
    }

    // Bottom rail, full inner width — same place the host tile puts name and address.
    let max_w = f64::from(rect.width()) - 2.0 * pad;
    let sub_base = f64::from(rect.bottom) - pad;
    let count = if c.count == 1 {
        "1 title".to_string()
    } else {
        format!("{} titles", c.count)
    };
    fonts.draw_clipped(
        canvas,
        &count,
        l,
        sub_base,
        W::Regular,
        13.0 * k,
        fg(0.55),
        max_w,
    );
    fonts.draw_clipped(
        canvas,
        &c.label,
        l,
        sub_base - 22.0 * k,
        W::Bold,
        23.0 * k,
        fg(1.0),
        max_w,
    );
    // Platform vs store, so two "Steam" buckets stay distinct. Top-right, the
    // corner the deck leaves clear.
    let kind = match &c.key {
        GroupKey::Store(_) => "STORE",
        GroupKey::Platform(_) | GroupKey::Launchers => "PLATFORM",
    };
    // `draw_tracked` tracks after every character including the last, so the ink
    // ends one gap short of the pen — `n - 1` when hanging off the right edge.
    let track = 1.4 * k;
    let kind_w = f64::from(fonts.measure(kind, W::SemiBold, 11.0 * k))
        + track * (kind.chars().count().saturating_sub(1)) as f64;
    // Never left of the deck's reserved corner: a narrow tile would otherwise
    // walk the caption back into the covers.
    let kind_x = (f64::from(rect.right) - pad - kind_w).max(l + (FAN_W + 10.0) * fk);
    fonts.draw_tracked(
        canvas,
        kind,
        kind_x,
        t + 12.0 * k,
        W::SemiBold,
        11.0 * k,
        track,
        fg(0.45),
    );
}

/// Transform that places card `n` relative to the front card's rect.
///
/// Pure so the geometry can be asserted without a GPU. The trap is the deck
/// growing past the box it reserves and hitting the title under it.
fn fan_matrix(front: Rect, n: usize, k: f64) -> Matrix {
    let n = n as f64;
    let s = (1.0 - FAN_SCALE_STEP * n) as f32;
    let pivot = Point::new(front.center_x(), front.center_y());
    let mut m = Matrix::translate(((n * FAN_DX * k) as f32, (n * FAN_DY * k) as f32));
    m.pre_rotate((FAN_ROT_DEG * n) as f32, pivot);
    m.pre_scale((s, s), pivot);
    m
}

/// Contact shadow under one card. Drawn, not sampled from the cover.
fn plate(canvas: &Canvas, rr: RRect, k: f64) {
    // Black is weight on a dark field and dirt on a pale one.
    let alpha = crate::theme::shadow(PLATE_ALPHA);
    canvas.draw_rrect(
        rr.with_outset(((PLATE_OUTSET * k) as f32, (PLATE_OUTSET * k) as f32))
            .with_offset(((PLATE_DX * k) as f32, (PLATE_DY * k) as f32)),
        &fill(Color4f::new(0.0, 0.0, 0.0, alpha)),
    );
}

/// Centre-crop to the card's 2:3 and fill the round-rect with one shader.
///
/// Not `clip_rrect` + `draw_image_rect`: a rotated round-rect clip is no longer
/// axis-aligned and falls back to a clip mask (three covers × five tiles = fifteen
/// masks a frame). If this goes back to a clip, `FAN_ROT_DEG` must go to zero with it.
fn draw_cover(canvas: &Canvas, img: &Image, front: Rect, rr: RRect) {
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let aspect = front.width() / front.height();
    let src = if iw / ih > aspect {
        let sw = ih * aspect;
        Rect::from_xywh((iw - sw) / 2.0, 0.0, sw, ih)
    } else {
        let sh = iw / aspect;
        Rect::from_xywh(0.0, (ih - sh) / 2.0, iw, sh)
    };
    let (sx, sy) = (front.width() / src.width(), front.height() / src.height());
    let mut local = Matrix::scale((sx, sy));
    local.post_translate((front.left - src.left * sx, front.top - src.top * sy));
    let Some(shader) = img.to_shader(
        (TileMode::Clamp, TileMode::Clamp),
        art_sampling(),
        Some(&local),
    ) else {
        return;
    };
    // Opaque: Skia modulates a shader by the paint's alpha, so a transparent
    // placeholder here draws nothing.
    let mut p = crate::theme::shaded();
    p.set_shader(shader);
    canvas.draw_rrect(rr, &p);
}

/// Front card when the group has no art: initials on an accent-tinted face.
fn draw_monogram(canvas: &Canvas, fonts: &Fonts, label: &str, front: Rect, rr: RRect, hair: f32) {
    // Accent-tinted face, not a fixed near-black: `fg()` is itself near-black on
    // pale palettes, so a dark face and a dark glyph vanish into each other.
    canvas.draw_rrect(rr, &fill(accent(0.20)));
    canvas.draw_rrect(
        rr.with_inset((hair / 2.0, hair / 2.0)),
        &stroke(accent(0.5), hair),
    );
    let mono = initials(label);
    // Sized off the card, not `k`: deck cards are smaller than a shelf poster.
    let size = f64::from(front.height()) * 0.30;
    let font = fonts.font(W::Bold, size);
    let tw = font.measure_str(&mono, None).0;
    canvas.draw_str(
        &mono,
        Point::new(
            front.center_x() - tw / 2.0,
            front.center_y() + (size * 0.36) as f32,
        ),
        &font,
        &fill(fg(0.85)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(id: &str, launcher: bool, platform: Option<&str>) -> LibraryGame {
        LibraryGame {
            id: id.into(),
            title: id.into(),
            store: "steam".into(),
            launcher,
            icon: String::new(),
            platform: platform.map(str::to_string),
            developer: None,
            year: None,
            genres: Vec::new(),
            stats: None,
            running: false,
        }
    }

    /// Launchers have their own row, and a single group is the whole library again.
    #[test]
    fn the_row_leaves_launchers_out_and_needs_two_groups() {
        let one = [
            game("l", true, None),
            game("a", false, None),
            game("b", false, None),
        ];
        assert!(
            collections(&one, SortKey::HostOrder).is_empty(),
            "a launcher and one store made a row"
        );
        let mut two = one.to_vec();
        two.push(game("p", false, Some("PS2")));
        let row = collections(&two, SortKey::HostOrder);
        let labels: Vec<&str> = row.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["PS2", "Steam"]);
        assert_eq!(row[1].fan, [1, 2], "the deck fans the group's own titles");
    }

    /// Tile at `k`, as the Collections row lays it out.
    fn tile(k: f64) -> Rect {
        Rect::from_xywh(100.0, 60.0, (TILE_W * k) as f32, (TILE_H * k) as f32)
    }

    /// Front card of the deck — [`paint_tile`]'s arithmetic.
    fn front_card(rect: Rect, k: f64) -> Rect {
        let pad = 20.0 * k;
        Rect::from_xywh(
            (f64::from(rect.left) + pad) as f32,
            (f64::from(rect.top) + pad + (FAN_H - COVER_H) * k) as f32,
            (COVER_W * k) as f32,
            (COVER_H * k) as f32,
        )
    }

    fn deck_bounds(rect: Rect, k: f64) -> Rect {
        let front = front_card(rect, k);
        let mut b = Rect::new_empty();
        for n in 0..FAN {
            b.join(fan_matrix(front, n, k).map_rect(front).0);
        }
        b
    }

    /// The deck turns and lifts, so its footprint is not the front card. However far
    /// it splays it must stay inside the reserved box and off the title underneath.
    #[test]
    fn the_deck_stays_clear_of_the_tiles_own_text() {
        for k in [0.75, 1.0, 1.5, 2.0, 3.0] {
            let rect = tile(k);
            let (pad, deck) = (20.0 * k, deck_bounds(rect, k));
            let (l, t) = (f64::from(rect.left) + pad, f64::from(rect.top) + pad);
            let slack = 0.05;
            assert!(f64::from(deck.left) >= l - slack, "k={k}: {deck:?}");
            assert!(f64::from(deck.top) >= t - slack, "k={k}: {deck:?}");
            assert!(
                f64::from(deck.right) <= l + FAN_W * k + slack,
                "the deck splays past the corner it leaves for the caption at k={k}: {deck:?}"
            );
            assert!(
                f64::from(deck.bottom) <= t + FAN_H * k + slack,
                "k={k}: {deck:?}"
            );
            // Bold 23 sits on `sub_base - 22`; a full em above the baseline bounds the title's ink.
            let title_top = f64::from(rect.bottom) - pad - 22.0 * k - 23.0 * k;
            assert!(
                f64::from(deck.bottom) < title_top,
                "the deck reaches the title at k={k}: {} vs {title_top}",
                deck.bottom
            );
        }
    }

    /// Every back card is smaller, higher, and further right. One cue alone is a
    /// smaller cover.
    ///
    /// Measured on the card's own top edge, not `map_rect`'s bounds. Rotation
    /// makes the axis-aligned box *wider* than the card, so a shrinking deck
    /// reads as a growing one against the bounds.
    #[test]
    fn the_deck_recedes_in_every_cue_at_once() {
        let front = front_card(tile(1.0), 1.0);
        // Top-left and top-right, mapped: their distance is the card's real width.
        let edge = |n: usize| {
            let m = fan_matrix(front, n, 1.0);
            let src = [
                Point::new(front.left, front.top),
                Point::new(front.right, front.top),
            ];
            let mut dst = [Point::new(0.0, 0.0); 2];
            m.map_points(&mut dst, &src);
            let (dx, dy) = (dst[1].x - dst[0].x, dst[1].y - dst[0].y);
            (f64::from(dx).hypot(f64::from(dy)), m.map_rect(front).0)
        };
        let (mut prev_w, mut prev_box) = edge(0);
        for n in 1..FAN {
            let (w, bounds) = edge(n);
            assert!(w < prev_w, "slot {n} did not shrink: {w} vs {prev_w}");
            // The centre is the pivot, so the bounding box is a fair witness for lift/right.
            assert!(
                bounds.center_y() < prev_box.center_y(),
                "slot {n} did not lift"
            );
            assert!(
                bounds.center_x() > prev_box.center_x(),
                "slot {n} did not step right"
            );
            prev_w = w;
            prev_box = bounds;
        }
    }

    fn over(src: Color4f, dst: Color4f) -> Color4f {
        let m = |s: f32, d: f32| s * src.a + d * (1.0 - src.a);
        Color4f::new(m(src.r, dst.r), m(src.g, dst.g), m(src.b, dst.b), 1.0)
    }

    /// WCAG contrast: sRGB to linear, then Rec. 709 relative luminance.
    fn contrast(a: Color4f, b: Color4f) -> f32 {
        let lin = |c: f32| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        let lum = |c: Color4f| 0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b);
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    /// Initials of a group with no art must read on every palette, not just the dark one.
    ///
    /// A hardcoded near-black face under `fg()` vanishes on pale palettes, where
    /// `fg()` is itself near-black.
    #[test]
    fn the_monogram_reads_on_every_palette() {
        for p in &crate::library::PALETTES {
            crate::theme::set_ink(crate::theme::Ink::of(p));
            let ground = Color4f::new(p.ground.0 as f32, p.ground.1 as f32, p.ground.2 as f32, 1.0);
            // Tile accent over the field, then the badge face, then the glyph. Glass
            // between the first two is omitted: it pushes the backdrop away from the
            // ink at both poles, so this is the harder case.
            let panel = over(accent(0.16), ground);
            let face = over(accent(0.20), panel);
            let glyph = over(fg(0.85), face);
            let c = contrast(glyph, face);
            assert!(c > 3.0, "the monogram is unreadable on {}: {c:.2}:1", p.id);
        }
        crate::theme::set_ink(crate::theme::Ink::of(crate::library::palette("violet")));
    }
}
