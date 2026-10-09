//! The GDI cursor poller's pure half: a cursor's AND/XOR planes to straight-alpha RGBA, and a
//! rasterised shape to the overlay one poll publishes. Pure over `&[u8]`, so its tests run on
//! every target; `idd_push/cursor_poll.rs` owns the GDI reads.

// Off Windows only the tests read this module.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

/// `rgba` is `Arc` so slot publish and every downstream attach is a refcount bump.
pub(crate) struct Shape {
    pub(crate) rgba: std::sync::Arc<Vec<u8>>,
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) hot_x: u32,
    pub(crate) hot_y: u32,
    pub(crate) serial: u64,
}

/// One poll's overlay. `pos` is the pointer relative to `rect`'s origin, and outside `rect`
/// the pointer is `visible: false`; `shown` is a drawn pointer with a cursor handle. A pointer
/// hidden before any shape was seen (a game that hid it before this session) is still a hide
/// the client must hear: an empty, invisible overlay.
pub(crate) fn compose_overlay(
    pos: (i32, i32),
    rect: (i32, i32, i32, i32),
    shown: bool,
    shape: Option<&Shape>,
) -> Option<pf_frame::CursorOverlay> {
    let (px, py) = pos;
    let Some(s) = shape else {
        return (!shown).then(|| pf_frame::CursorOverlay {
            x: px,
            y: py,
            w: 0,
            h: 0,
            rgba: std::sync::Arc::new(Vec::new()),
            serial: 0,
            hot_x: 0,
            hot_y: 0,
            visible: false,
        });
    };
    let in_rect = px >= 0 && py >= 0 && px < rect.2 && py < rect.3;
    Some(pf_frame::CursorOverlay {
        // Overlay x/y = bitmap top-left (reported position − hotspot), frame pixels.
        x: px - s.hot_x as i32,
        y: py - s.hot_y as i32,
        w: s.w,
        h: s.h,
        rgba: s.rgba.clone(),
        serial: s.serial,
        hot_x: s.hot_x,
        hot_y: s.hot_y,
        visible: shown && in_rect,
    })
}

const NEIGHBORS: [(i32, i32); 8] = [
    (-1, -1),
    (0, -1),
    (1, -1),
    (-1, 0),
    (1, 0),
    (-1, 1),
    (0, 1),
    (1, 1),
];

/// Alpha channel entirely zero: old-style cursor whose transparency (and invert)
/// live in the AND mask ([`masked_color_to_rgba`]).
pub(crate) fn alpha_is_empty(rgba: &[u8]) -> bool {
    rgba.chunks_exact(4).all(|p| p[3] == 0)
}

/// Test-only AND-as-alpha (no invert). `mask_bgra` is GetDIBits' 32bpp expansion
/// of the 1bpp mask, so any non-zero channel is "set"; white = transparent.
/// The GDI `convert` uses [`masked_color_to_rgba`]: AND=1 plus colour is invert (I-beam).
#[cfg(test)]
fn apply_and_mask_alpha(rgba: &mut [u8], mask_bgra: &[u8]) {
    for (px, m) in rgba.chunks_exact_mut(4).zip(mask_bgra.chunks_exact(4)) {
        px[3] = if m[0] != 0 { 0 } else { 0xFF };
    }
}

/// Alpha-less colour cursor: AND plus colour-as-XOR, same four states as
/// [`mono_planes_to_rgba`]. Non-zero RGB with AND=1 is invert — treating AND=1
/// as always-transparent drops the I-beam. `(false, true)` keeps the colour
/// (a painted glyph); the monochrome table can only emit white.
pub(crate) fn masked_color_to_rgba(
    color_rgba: &[u8],
    mask_bgra: &[u8],
    w: usize,
    h: usize,
) -> Vec<u8> {
    let mut rgba = vec![0u8; w * h * 4];
    let mut invert = vec![false; w * h];
    for i in 0..w * h {
        let and = mask_bgra.get(i * 4).is_some_and(|&b| b != 0);
        let c = color_rgba.get(i * 4..i * 4 + 3).unwrap_or(&[0, 0, 0]);
        let xor = c[0] != 0 || c[1] != 0 || c[2] != 0;
        let px = &mut rgba[i * 4..i * 4 + 4];
        match (and, xor) {
            (false, false) => px.copy_from_slice(&[0, 0, 0, 0xFF]),
            (false, true) => px.copy_from_slice(&[c[0], c[1], c[2], 0xFF]),
            (true, false) => {}
            (true, true) => {
                px.copy_from_slice(&[0, 0, 0, 0xFF]);
                invert[i] = true;
            }
        }
    }
    grow_invert_outline(&mut rgba, &invert, w, h);
    rgba
}

/// Monochrome-cursor truth table, plus the white outline that makes invert legible.
///
/// A monochrome `HCURSOR` has no colour bitmap: `hbmMask` is double height — AND
/// over XOR — and the pair encodes four states:
///
/// | AND | XOR | meaning     | straight-alpha result                    |
/// |-----|-----|-------------|------------------------------------------|
/// | 0   | 0   | black       | opaque black                             |
/// | 0   | 1   | white       | opaque white                             |
/// | 1   | 0   | transparent | fully transparent                        |
/// | 1   | 1   | INVERT dst  | opaque black + a grown white outline     |
///
/// Invert is unrepresentable in straight alpha (per-pixel XOR of the destination),
/// so it becomes opaque black and every transparent 8-neighbour of an invert
/// pixel is turned opaque white. That outline keeps a text I-beam — almost
/// entirely invert — legible over dark content.
pub(crate) fn mono_planes_to_rgba(
    and_plane: &[u8],
    xor_plane: &[u8],
    w: usize,
    h: usize,
) -> Vec<u8> {
    let mut rgba = vec![0u8; w * h * 4];
    let mut invert = vec![false; w * h];
    for i in 0..w * h {
        let (a, x) = (and_plane[i * 4] != 0, xor_plane[i * 4] != 0);
        let px = &mut rgba[i * 4..i * 4 + 4];
        match (a, x) {
            (false, false) => px.copy_from_slice(&[0, 0, 0, 0xFF]),
            (false, true) => px.copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]),
            (true, false) => {}
            (true, true) => {
                px.copy_from_slice(&[0, 0, 0, 0xFF]);
                invert[i] = true;
            }
        }
    }
    grow_invert_outline(&mut rgba, &invert, w, h);
    rgba
}

/// Transparent 8-neighbours of an invert pixel become opaque white. Invert
/// itself stays opaque black. Shared by the monochrome and masked-color paths.
fn grow_invert_outline(rgba: &mut [u8], invert: &[bool], w: usize, h: usize) {
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            if !invert[(y * w as i32 + x) as usize] {
                continue;
            }
            for (dx, dy) in NEIGHBORS {
                let (nx, ny) = (x + dx, y + dy);
                if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                    continue;
                }
                let o = (ny * w as i32 + nx) as usize * 4;
                if rgba[o + 3] == 0 {
                    rgba[o..o + 4].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrow() -> Shape {
        Shape {
            rgba: std::sync::Arc::new(vec![255; 2 * 2 * 4]),
            w: 2,
            h: 2,
            hot_x: 1,
            hot_y: 1,
            serial: 7,
        }
    }

    /// The overlay is the pointer less the hotspot, visible only inside the target's rect.
    #[test]
    fn the_overlay_is_visible_only_inside_the_rect() {
        let rect = (1920, 0, 1280, 720);
        let o = compose_overlay((10, 20), rect, true, Some(&arrow())).expect("overlay");
        assert_eq!((o.x, o.y, o.w, o.h, o.serial), (9, 19, 2, 2, 7));
        assert!(o.visible);
        let out = compose_overlay((1280, 20), rect, true, Some(&arrow())).expect("overlay");
        assert!(!out.visible, "past the rect's width");
        let hidden = compose_overlay((10, 20), rect, false, Some(&arrow())).expect("overlay");
        assert!(!hidden.visible, "a NULL handle or a hidden cursor");
    }

    /// Before any shape: a hide still reaches the client as an empty overlay; a shown pointer
    /// publishes nothing until it rasterises.
    #[test]
    fn a_hide_before_any_shape_is_an_empty_overlay() {
        let hidden = compose_overlay((3, 4), (0, 0, 640, 480), false, None).expect("a hide");
        assert_eq!((hidden.x, hidden.y, hidden.w, hidden.h), (3, 4, 0, 0));
        assert!(!hidden.visible && hidden.rgba.is_empty());
        assert!(compose_overlay((3, 4), (0, 0, 640, 480), true, None).is_none());
    }

    /// 1bpp plane → 32bpp as `GetDIBits` does: any non-zero channel means "bit set".
    fn plane(bits: &[u8]) -> Vec<u8> {
        bits.iter()
            .flat_map(|&b| {
                let v = if b != 0 { 0xFF } else { 0 };
                [v, v, v, 0]
            })
            .collect()
    }

    fn px(rgba: &[u8], i: usize) -> [u8; 4] {
        rgba[i * 4..i * 4 + 4].try_into().unwrap()
    }

    const OPAQUE_BLACK: [u8; 4] = [0, 0, 0, 0xFF];
    const OPAQUE_WHITE: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];
    const TRANSPARENT: [u8; 4] = [0, 0, 0, 0];

    #[test]
    fn the_monochrome_truth_table_is_exact() {
        //          (0,0) black  (0,1) white  (1,0) transparent  (1,1) invert
        let and = plane(&[0, 0, 1, 1]);
        let xor = plane(&[0, 1, 0, 1]);
        let out = mono_planes_to_rgba(&and, &xor, 4, 1);
        assert_eq!(px(&out, 0), OPAQUE_BLACK, "AND=0 XOR=0 ⇒ black");
        assert_eq!(px(&out, 1), OPAQUE_WHITE, "AND=0 XOR=1 ⇒ white");
        // Pixel 2 is transparent by the table, but it is an 8-neighbour of the invert pixel at 3,
        // so the outline claims it — that IS the documented behaviour.
        assert_eq!(
            px(&out, 2),
            OPAQUE_WHITE,
            "outline grows into adjacent transparency"
        );
        assert_eq!(px(&out, 3), OPAQUE_BLACK, "AND=1 XOR=1 ⇒ black + outline");
    }

    #[test]
    fn transparent_pixels_stay_transparent_without_an_invert_neighbour() {
        let and = plane(&[1, 1, 1, 1]);
        let xor = plane(&[0, 0, 0, 0]);
        let out = mono_planes_to_rgba(&and, &xor, 4, 1);
        for i in 0..4 {
            assert_eq!(px(&out, i), TRANSPARENT, "pixel {i}");
        }
    }

    #[test]
    fn the_invert_outline_covers_eight_neighbours_and_overwrites_nothing() {
        // 3×3, invert at the centre, everything else transparent.
        let and = plane(&[1, 1, 1, 1, 1, 1, 1, 1, 1]);
        let mut xor = plane(&[0; 9]);
        for b in &mut xor[4 * 4..4 * 4 + 3] {
            *b = 0xFF; // centre pixel's XOR bit
        }
        let out = mono_planes_to_rgba(&and, &xor, 3, 3);
        assert_eq!(px(&out, 4), OPAQUE_BLACK, "the invert pixel itself");
        for i in [0, 1, 2, 3, 5, 6, 7, 8] {
            assert_eq!(px(&out, i), OPAQUE_WHITE, "neighbour {i} outlined");
        }

        // Now surround it with BLACK shape pixels (AND=0, XOR=0): the outline must leave them alone.
        let and = plane(&[0, 0, 0, 0, 1, 0, 0, 0, 0]);
        let out = mono_planes_to_rgba(&and, &xor, 3, 3);
        for i in [0, 1, 2, 3, 5, 6, 7, 8] {
            assert_eq!(
                px(&out, i),
                OPAQUE_BLACK,
                "neighbour {i} must not be repainted"
            );
        }
    }

    #[test]
    fn the_outline_clips_at_the_edges() {
        // 2×2 with the invert at (0, 0): only (1,0), (0,1) and (1,1) can be outlined.
        let and = plane(&[1, 1, 1, 1]);
        let mut xor = plane(&[0; 4]);
        for b in &mut xor[0..3] {
            *b = 0xFF;
        }
        let out = mono_planes_to_rgba(&and, &xor, 2, 2);
        assert_eq!(px(&out, 0), OPAQUE_BLACK);
        for i in [1, 2, 3] {
            assert_eq!(px(&out, i), OPAQUE_WHITE, "in-bounds neighbour {i}");
        }
    }

    #[test]
    fn an_empty_alpha_channel_is_detected() {
        assert!(alpha_is_empty(&[1, 2, 3, 0, 4, 5, 6, 0]));
        assert!(!alpha_is_empty(&[1, 2, 3, 0, 4, 5, 6, 1]));
        assert!(alpha_is_empty(&[]), "no pixels ⇒ vacuously empty");
    }

    #[test]
    fn the_and_mask_supplies_alpha_for_an_alpha_less_cursor() {
        let mut rgba = vec![10, 20, 30, 0, 40, 50, 60, 0];
        let mask = plane(&[1, 0]); // pixel 0 masked out, pixel 1 kept
        apply_and_mask_alpha(&mut rgba, &mask);
        assert_eq!(px(&rgba, 0), [10, 20, 30, 0], "masked ⇒ transparent");
        assert_eq!(px(&rgba, 1), [40, 50, 60, 0xFF], "unmasked ⇒ opaque");
    }

    /// A short mask must not panic: `zip` stops at the shorter side. The caller
    /// already requires `mask.h >= color.h`; this is the belt.
    #[test]
    fn a_short_mask_does_not_panic() {
        let mut rgba = vec![1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0];
        apply_and_mask_alpha(&mut rgba, &plane(&[0]));
        assert_eq!(px(&rgba, 0), [1, 2, 3, 0xFF]);
        assert_eq!(px(&rgba, 1), [4, 5, 6, 0]);
    }

    /// Colour standing in for XOR. Pixel 3 is the I-beam case: AND=1 and a
    /// non-zero colour pixel is invert, not transparent — `apply_and_mask_alpha`
    /// would have dropped it.
    #[test]
    fn a_masked_color_invert_pixel_is_not_transparent() {
        //          (0,0) black  (0,1) red    (1,0) transparent  (1,1) invert
        let color = vec![0, 0, 0, 0, 0xCC, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0];
        let mask = plane(&[0, 0, 1, 1]);
        let out = masked_color_to_rgba(&color, &mask, 4, 1);
        assert_eq!(px(&out, 0), OPAQUE_BLACK, "AND=0 colour=0 ⇒ black");
        assert_eq!(
            px(&out, 1),
            [0xCC, 0, 0, 0xFF],
            "AND=0 colour ⇒ opaque colour"
        );
        // Pixel 2 is transparent by the table, but it is an 8-neighbour of the invert pixel at 3,
        // so the outline claims it — same as the monochrome table.
        assert_eq!(
            px(&out, 2),
            OPAQUE_WHITE,
            "outline grows into adjacent transparency"
        );
        assert_eq!(
            px(&out, 3),
            OPAQUE_BLACK,
            "AND=1 colour≠0 ⇒ invert, not drop"
        );
    }

    #[test]
    fn a_masked_color_transparent_pixel_stays_transparent() {
        let color = vec![0u8; 16];
        let mask = plane(&[1, 1, 1, 1]);
        let out = masked_color_to_rgba(&color, &mask, 4, 1);
        for i in 0..4 {
            assert_eq!(px(&out, i), TRANSPARENT, "pixel {i}");
        }
    }
}
