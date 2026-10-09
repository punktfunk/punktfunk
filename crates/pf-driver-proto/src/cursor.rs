//! Hardware-cursor channel: one unnamed file mapping per monitor, delivered by handle value
//! ([`control::IOCTL_SET_CURSOR_CHANNEL`](crate::control::IOCTL_SET_CURSOR_CHANNEL)). The
//! driver's cursor thread seqlock-writes shape + position; the host reads at encode-tick pace —
//! no event crosses the boundary. Writer: bump [`CursorShm::seq`] odd, write, bump even.
//! Reader: retry while odd, copy, re-read — unchanged ⇒ consistent snapshot. Position-only
//! updates never touch shape bytes, so a reader that skips unchanged `shape_id`s never copies
//! torn pixels.

use bytemuck::{Pod, Zeroable};

/// [`CursorShm`] magic (`b"PFCU"` LE); anything else = not attached yet.
pub const CURSOR_MAGIC: u32 = u32::from_le_bytes(*b"PFCU");

/// Max cursor side (px) declared to the OS (`IDDCX_CURSOR_CAPS::MaxX/MaxY`). Windows XL
/// accessibility cursors top out here; the host's wire forwarder downscales anyway.
pub const CURSOR_SHAPE_MAX: u32 = 256;

/// Shape-buffer bytes: 32-bpp at the declared max.
pub const CURSOR_SHAPE_BYTES: usize = (CURSOR_SHAPE_MAX * CURSOR_SHAPE_MAX * 4) as usize;

/// Byte offset of the shape pixels (64-byte header).
pub const CURSOR_SHAPE_OFFSET: usize = 64;

pub const CURSOR_SHM_SIZE: usize = CURSOR_SHAPE_OFFSET + CURSOR_SHAPE_BYTES;

/// `IDDCX_CURSOR_SHAPE_TYPE` values. The driver writes the OS value into [`CursorShm::cursor_type`].
pub const CURSOR_TYPE_MASKED_COLOR: u32 = 1;
pub const CURSOR_TYPE_ALPHA: u32 = 2;

/// Section header; shape pixels follow at [`CURSOR_SHAPE_OFFSET`]. `x`/`y` are the shape's
/// top-left on this monitor (IddCx `IDARG_OUT_QUERY_HWCURSOR::X/Y` — position − hotspot,
/// negative past its top-left edge); both readers take them as they are.
/// `shape_id` bumps on every shape set. Pixels are 32-bpp rows at `pitch` (BGRA for
/// ALPHA; color+mask for MASKED_COLOR); [`shape_rgba`] converts them on either side.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct CursorShm {
    pub magic: u32,
    /// Seqlock: odd = writer mid-update.
    pub seq: u32,
    pub visible: u32,
    pub cursor_type: u32,
    pub x: i32,
    pub y: i32,
    pub shape_id: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    /// Always 0 from this host. An older driver subtracts it from `x`/`y`, and IddCx
    /// positions are already monitor-relative, so zero keeps that driver right.
    pub origin_x: i32,
    pub origin_y: i32,
    /// Host-stamped `f32` bits: where the HDR desktop puts SDR white (1.0 = 80 nits), for
    /// the driver's blend onto an FP16 frame. `0` = not stamped, the driver uses 1.0.
    pub sdr_white_scale: u32,
    pub _reserved: u32,
}

/// Straight-alpha RGBA for a pixel that XORs (inverts) the screen. No blend can honor an
/// inversion; translucent mid-gray stays visible over dark and light content. Every
/// cursor rasteriser on either side of the driver boundary draws invert as this.
pub const INVERT_RGBA: [u8; 4] = [0x80, 0x80, 0x80, 0xB4];

/// One shape as straight-alpha RGBA, `w * h * 4` bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShapeRgba {
    pub rgba: alloc::vec::Vec<u8>,
    pub w: u32,
    pub h: u32,
    pub hot_x: u32,
    pub hot_y: u32,
}

/// The part of a `w`×`h` cursor shape drawn at `(x, y)` that lands on a `width`×`height`
/// target: `(x, y, w, h)` clipped to it, or `None` when none of it does. The shape's
/// top-left is in target coordinates and may be negative — the pointer half off an edge.
///
/// What a save-under of the blend has to copy, and put back. The driver's blend is
/// Windows-only; the rule lives here so it is covered everywhere.
#[must_use]
pub fn clip_rect(
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    width: u32,
    height: u32,
) -> Option<(u32, u32, u32, u32)> {
    let (x0, y0) = (i64::from(x).max(0), i64::from(y).max(0));
    let x1 = (i64::from(x) + i64::from(w)).min(i64::from(width));
    let y1 = (i64::from(y) + i64::from(h)).min(i64::from(height));
    (x1 > x0 && y1 > y0).then(|| (x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
}

/// `(width, rows, pitch)` of the shape bytes a reader copies out for `hdr`, clamped to the
/// section so a corrupt header can never index past it.
#[must_use]
pub fn shape_extent(hdr: &CursorShm) -> (usize, usize, usize) {
    let rows = hdr.height.min(CURSOR_SHAPE_MAX) as usize;
    let width = hdr.width.min(CURSOR_SHAPE_MAX) as usize;
    let pitch = (hdr.pitch as usize).min(CURSOR_SHAPE_BYTES / rows.max(1));
    (width, rows, pitch)
}

/// Pack the pitch-strided 32-bpp rows of `raw` (at least `rows * pitch` bytes, see
/// [`shape_extent`]) into straight RGBA. ALPHA is BGRA (swap R↔B). MASKED_COLOR: `alpha ==
/// 0` is opaque color; `0xFF` XORs the screen with the color. XOR with black is the
/// transparent field around a monochrome shape (the I-beam is mostly that); XOR with
/// anything else is an inversion, drawn as [`INVERT_RGBA`].
#[must_use]
pub fn shape_rgba(hdr: &CursorShm, raw: &[u8]) -> ShapeRgba {
    let (width, rows, pitch) = shape_extent(hdr);
    let masked = hdr.cursor_type == CURSOR_TYPE_MASKED_COLOR;
    let mut rgba = alloc::vec::Vec::with_capacity(width * rows * 4);
    for y in 0..rows {
        let row = raw.get(y * pitch..).unwrap_or(&[]);
        for x in 0..width {
            let o = x * 4;
            let Some(px) = row.get(o..o + 4) else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            };
            let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
            if masked {
                if a == 0 {
                    rgba.extend_from_slice(&[r, g, b, 0xFF]);
                } else if (r, g, b) == (0, 0, 0) {
                    rgba.extend_from_slice(&[0, 0, 0, 0]);
                } else {
                    rgba.extend_from_slice(&INVERT_RGBA);
                }
            } else {
                rgba.extend_from_slice(&[r, g, b, a]);
            }
        }
    }
    ShapeRgba {
        rgba,
        w: width as u32,
        h: rows as u32,
        hot_x: hdr.hot_x.min(width.saturating_sub(1) as u32),
        hot_y: hdr.hot_y.min(rows.saturating_sub(1) as u32),
    }
}

// Layout is load-bearing across the process boundary — pin it.
const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(size_of::<CursorShm>() == 64);
    assert!(size_of::<CursorShm>() <= CURSOR_SHAPE_OFFSET);
    assert!(offset_of!(CursorShm, magic) == 0);
    assert!(offset_of!(CursorShm, seq) == 4);
    assert!(offset_of!(CursorShm, visible) == 8);
    assert!(offset_of!(CursorShm, cursor_type) == 12);
    assert!(offset_of!(CursorShm, x) == 16);
    assert!(offset_of!(CursorShm, y) == 20);
    assert!(offset_of!(CursorShm, shape_id) == 24);
    assert!(offset_of!(CursorShm, width) == 28);
    assert!(offset_of!(CursorShm, height) == 32);
    assert!(offset_of!(CursorShm, pitch) == 36);
    assert!(offset_of!(CursorShm, hot_x) == 40);
    assert!(offset_of!(CursorShm, hot_y) == 44);
    assert!(offset_of!(CursorShm, origin_x) == 48);
    assert!(offset_of!(CursorShm, origin_y) == 52);
    assert!(offset_of!(CursorShm, sdr_white_scale) == 56);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_shm_layout_is_pinned() {
        // Header must leave the shape offset intact whatever grows inside `_reserved`.
        assert_eq!(core::mem::size_of::<CursorShm>(), 64);
        assert_eq!(CURSOR_SHM_SIZE, 64 + 256 * 256 * 4);
        assert_eq!(CURSOR_MAGIC, u32::from_le_bytes(*b"PFCU"));
        let hdr = CursorShm {
            magic: CURSOR_MAGIC,
            seq: 2,
            visible: 1,
            cursor_type: CURSOR_TYPE_ALPHA,
            x: -3,
            y: 7,
            shape_id: 42,
            width: 32,
            height: 32,
            pitch: 128,
            hot_x: 4,
            hot_y: 5,
            origin_x: -1920,
            origin_y: 0,
            sdr_white_scale: 2.5f32.to_bits(),
            _reserved: 0,
        };
        let bytes = bytemuck::bytes_of(&hdr);
        assert_eq!(*bytemuck::from_bytes::<CursorShm>(bytes), hdr);
        assert_eq!(bytes[16..20], (-3i32).to_le_bytes());
        assert_eq!(bytes[48..52], (-1920i32).to_le_bytes());
        assert_eq!(f32::from_bits(hdr.sdr_white_scale), 2.5);
    }

    /// Both readers of the cursor section share one conversion: ALPHA swaps B↔R, MASKED
    /// turns the mask into opaque colour, the transparent field, or the mid-gray XOR stand-in,
    /// and a header whose extent exceeds the section is clamped rather than indexed.
    #[test]
    fn cursor_shape_converts_alpha_and_masked_rows() {
        let hdr = CursorShm {
            cursor_type: CURSOR_TYPE_ALPHA,
            width: 2,
            height: 1,
            pitch: 16,
            hot_x: 9,
            hot_y: 9,
            ..CursorShm::zeroed()
        };
        // Two BGRA pixels, then pitch padding.
        let raw = [1u8, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0];
        let s = shape_rgba(&hdr, &raw);
        assert_eq!(s.rgba, [3, 2, 1, 4, 7, 6, 5, 8]);
        assert_eq!((s.w, s.h, s.hot_x, s.hot_y), (2, 1, 1, 0));
        let masked = CursorShm {
            cursor_type: CURSOR_TYPE_MASKED_COLOR,
            width: 3,
            ..hdr
        };
        // Opaque colour, an inversion pixel, and the XOR-with-black field around a
        // monochrome shape — which must stay transparent, or an I-beam is a gray block.
        let raw = [1u8, 2, 3, 0, 5, 6, 7, 0xFF, 0, 0, 0, 0xFF];
        assert_eq!(
            shape_rgba(&masked, &raw).rgba,
            [3, 2, 1, 0xFF, 0x80, 0x80, 0x80, 0xB4, 0, 0, 0, 0]
        );
        // Short rows read as transparent; an oversized header stays inside the section.
        assert_eq!(
            shape_rgba(&hdr, &[9, 9, 9, 9]).rgba,
            [9, 9, 9, 9, 0, 0, 0, 0]
        );
        let huge = CursorShm {
            width: u32::MAX,
            height: u32::MAX,
            pitch: u32::MAX,
            ..hdr
        };
        let (w, rows, pitch) = shape_extent(&huge);
        assert_eq!((w, rows), (256, 256));
        assert!(rows * pitch <= CURSOR_SHAPE_BYTES);
    }

    #[test]
    fn a_cursor_save_under_covers_only_what_the_target_holds() {
        // Wholly inside: the shape's own box.
        assert_eq!(
            clip_rect(100, 50, 32, 32, 1920, 1080),
            Some((100, 50, 32, 32))
        );
        // Half off each edge in turn; the origin moves only where the shape starts negative.
        assert_eq!(
            clip_rect(-10, 50, 32, 32, 1920, 1080),
            Some((0, 50, 22, 32))
        );
        assert_eq!(
            clip_rect(100, -10, 32, 32, 1920, 1080),
            Some((100, 0, 32, 22))
        );
        assert_eq!(
            clip_rect(1900, 50, 32, 32, 1920, 1080),
            Some((1900, 50, 20, 32))
        );
        assert_eq!(
            clip_rect(100, 1060, 32, 32, 1920, 1080),
            Some((100, 1060, 32, 20))
        );
        // Fully off, in both directions: nothing to save.
        assert_eq!(clip_rect(-40, 50, 32, 32, 1920, 1080), None);
        assert_eq!(clip_rect(1920, 50, 32, 32, 1920, 1080), None);
        // A shape with no pixels covers nothing.
        assert_eq!(clip_rect(100, 50, 0, 32, 1920, 1080), None);
        assert_eq!(clip_rect(100, 50, 32, 0, 1920, 1080), None);
    }
}
