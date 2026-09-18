//! Cursor-as-metadata: `SPA_META_Cursor` parse and CPU composite blit.
//!
//! [`update_cursor_meta`] reads a compositor-chosen bitmap; a missed bound
//! SIGSEGVs inside the PipeWire `.process` callback (`catch_unwind` cannot
//! help). The `composite_cursor*` blits clip that bitmap into a frame.
//! Isolated from stream machinery so the bounds and blits are testable
//! without a compositor.
//!
//! Tests in this module. SAFETY proofs sit on each unaligned header read
//! and `from_raw_parts`.

use super::PixelFormat;
use pipewire as pw;
use pw::spa;
use std::sync::Arc;

/// Latest cursor parsed from `SPA_META_Cursor`. Position refreshes on every
/// buffer that carries the meta (including cursor-only buffers whose frame is
/// otherwise skipped); the RGBA bitmap is replaced only when `bitmap_offset != 0`.
#[derive(Default)]
pub(super) struct CursorState {
    /// `spa_meta_cursor.id != 0`.
    visible: bool,
    /// Bitmap top-left = reported position − hotspot.
    x: i32,
    y: i32,
    /// Straight-alpha RGBA (`bw*bh*4`). `Arc` so each GPU overlay is a
    /// refcount bump, not a copy. Empty until the first bitmap arrives.
    rgba: Arc<Vec<u8>>,
    bw: u32,
    bh: u32,
    /// Bitmap identity. Stable across position-only moves so the GPU path
    /// re-uploads the cursor texture only on change.
    serial: u64,
    /// Compositor hotspot, for the cursor-forward channel. The blend path
    /// uses pre-adjusted `x`/`y` and never reads this.
    hot_x: i32,
    hot_y: i32,
    /// This stream observed a `SPA_META_Cursor` region. Per-stream: a
    /// process-wide latch made a later session look like "no meta".
    seen_meta: bool,
    /// Producer rewrites cursor meta on every buffer, so `id == 0` is an
    /// authoritative hide rather than a stale recycled region. True for
    /// KWin virtual outputs; false for stale-meta producers (Mutter).
    id0_hides: bool,
}

impl CursorState {
    pub(super) fn new(id0_hides: bool) -> CursorState {
        CursorState {
            id0_hides,
            ..CursorState::default()
        }
    }

    /// Overlay for encode/forward, or `None` before the first bitmap. Hidden
    /// still yields `Some` (`visible: false`): known-hidden ≠ no cursor yet.
    /// The encode loop strips invisible overlays before any blend.
    pub(super) fn overlay(&self) -> Option<pf_frame::CursorOverlay> {
        if self.rgba.is_empty() {
            return None;
        }
        Some(pf_frame::CursorOverlay {
            x: self.x,
            y: self.y,
            w: self.bw,
            h: self.bh,
            rgba: self.rgba.clone(),
            serial: self.serial,
            hot_x: self.hot_x.max(0) as u32,
            hot_y: self.hot_y.max(0) as u32,
            visible: self.visible,
        })
    }
}

/// Straight (R,G,B,A) from one 4-byte cursor pixel. Portals emit RGBA or
/// BGRA; ARGB/ABGR are accepted. Unknown 4-byte formats read as RGBA.
pub(super) fn decode_bitmap_pixel(vfmt: u32, s: &[u8]) -> (u8, u8, u8, u8) {
    match vfmt {
        x if x == spa::sys::SPA_VIDEO_FORMAT_RGBA => (s[0], s[1], s[2], s[3]),
        x if x == spa::sys::SPA_VIDEO_FORMAT_BGRA => (s[2], s[1], s[0], s[3]),
        x if x == spa::sys::SPA_VIDEO_FORMAT_ARGB => (s[1], s[2], s[3], s[0]),
        x if x == spa::sys::SPA_VIDEO_FORMAT_ABGR => (s[3], s[2], s[1], s[0]),
        _ => (s[0], s[1], s[2], s[3]),
    }
}

/// Apply `spa_meta_cursor.id` to visibility. `false` means skip the rest of
/// the meta (position, bitmap).
///
/// Two producer contracts on `id == 0`. A rewriting producer (KWin) writes
/// id 0 on every buffer when the pointer is off this output or hidden —
/// honour it or the composited arrow outlives every hide. A stale-meta
/// producer (Mutter) only rewrites the region when the cursor changed;
/// recycled buffers carry a stale id-0, so last-known state holds. A real
/// leave/hide simply stops producing updates.
fn note_cursor_id(cursor: &mut CursorState, id: u32) -> bool {
    if id == 0 {
        if cursor.id0_hides {
            cursor.visible = false;
        }
        return false;
    }
    cursor.visible = true;
    true
}

/// Read the newest `SPA_META_Cursor` into `cursor`. Runs before stale-frame
/// filtering so metadata-only pointer moves still update position.
/// Producer offsets and bitmap geometry are bounded before every read.
/// `bitmap_offset == 0` keeps the last complete bitmap.
pub(super) fn update_cursor_meta(cursor: &mut CursorState, spa_buf: *mut spa::sys::spa_buffer) {
    // SAFETY: `spa_buf` is the live dequeued buffer (not yet requeued).
    // `find_meta` (not `find_meta_data`) yields the region's real `size`.
    // Offsets below are producer-written; unbound they OOB-read, and a
    // SIGSEGV inside `.process` is uncatchable by `catch_unwind`.
    let meta = unsafe { spa::sys::spa_buffer_find_meta(spa_buf, spa::sys::SPA_META_Cursor) };
    if meta.is_null() {
        return;
    }
    // One-shot per stream: the producer attached a cursor-meta region.
    // Absence of this log means negotiation dropped the meta, not that
    // the pointer never moved.
    if !cursor.seen_meta {
        cursor.seen_meta = true;
        tracing::info!("cursor meta: first SPA_META_Cursor region observed on this stream");
    }
    // SAFETY: `meta` is non-null and points into the held buffer's metadata array.
    let (region_size, data) = unsafe { ((*meta).size as usize, (*meta).data as *const u8) };
    if data.is_null() || region_size < std::mem::size_of::<spa::sys::spa_meta_cursor>() {
        return;
    }
    // SAFETY: `region_size >= size_of::<spa_meta_cursor>()` above, so the
    // header is readable. `read_unaligned`: the producer need not align it.
    let cur = unsafe { (data as *const spa::sys::spa_meta_cursor).read_unaligned() };
    let (id, pos_x, pos_y, hot_x, hot_y, bmp_off) = (
        cur.id,
        cur.position.x,
        cur.position.y,
        cur.hotspot.x,
        cur.hotspot.y,
        cur.bitmap_offset,
    );
    if !note_cursor_id(cursor, id) {
        return;
    }
    cursor.x = pos_x - hot_x;
    cursor.y = pos_y - hot_y;
    cursor.hot_x = hot_x;
    cursor.hot_y = hot_y;
    if bmp_off == 0 {
        // Position-only update — keep the cached bitmap.
        return;
    }
    let bmp_off = bmp_off as usize;
    // `bitmap_offset` is producer-controlled; the `spa_meta_bitmap` header
    // must fit in the region or the next read walks off it.
    match bmp_off.checked_add(std::mem::size_of::<spa::sys::spa_meta_bitmap>()) {
        Some(end) if end <= region_size => {}
        _ => return,
    }
    // SAFETY: `bmp_off + size_of::<spa_meta_bitmap>() <= region_size` above,
    // so the header is in bounds. `read_unaligned` is required: `bmp_off` is
    // producer-written and neither SPA nor this function proves alignment.
    // The struct is `Copy` POD; one unaligned read yields an aligned local.
    let bmp = unsafe { (data.add(bmp_off) as *const spa::sys::spa_meta_bitmap).read_unaligned() };
    let (vfmt, bw, bh, stride, pix_off) = (
        bmp.format,
        bmp.size.width,
        bmp.size.height,
        bmp.stride.max(0) as usize,
        bmp.offset as usize,
    );
    // Empty or >1024 (the meta-size request cap).
    if bw == 0 || bh == 0 || bw > 1024 || bh > 1024 {
        return;
    }
    // Distinct from `bitmap_offset == 0` (position-only): `spa_meta_bitmap.offset
    // == 0` means no pixels. Treating 0 as a pixel offset would start the
    // extent at the bitmap header and cache those words as the cursor.
    if pix_off == 0 {
        return;
    }
    let row = bw as usize * 4;
    let stride = if stride < row { row } else { stride };
    let Some(extent) = bitmap_extent(bmp_off, pix_off, stride, row, bh as usize, region_size)
    else {
        return;
    };
    // SAFETY: `bitmap_extent` returned `Some`: `[bmp_off + pix_off, +len)`
    // lies inside `region_size` and `len` is exactly what the strided loop
    // reads. `data` is the producer's meta-region base, live for this callback.
    let src = unsafe { std::slice::from_raw_parts(data.add(extent.start), extent.len()) };
    let mut rgba = vec![0u8; bw as usize * bh as usize * 4];
    for y in 0..bh as usize {
        for x in 0..bw as usize {
            let so = y * stride + x * 4;
            let (r, g, b, a) = decode_bitmap_pixel(vfmt, &src[so..so + 4]);
            let d = (y * bw as usize + x) * 4;
            rgba[d] = r;
            rgba[d + 1] = g;
            rgba[d + 2] = b;
            rgba[d + 3] = a;
        }
    }
    cursor.rgba = Arc::new(rgba);
    cursor.bw = bw;
    cursor.bh = bh;
    cursor.serial = cursor.serial.wrapping_add(1);
    // First bitmap (serial 0→1). Until then `overlay()` is `None`.
    if cursor.serial == 1 {
        tracing::info!(w = bw, h = bh, "cursor meta: first cursor bitmap received");
    }
}

/// Byte range of a `bh`-row, `row`-wide, `stride`-strided bitmap at
/// `bmp_off + pix_off` inside the cursor-meta region, or `None` if it does
/// not fit or the arithmetic overflows.
///
/// Every input except `region_size` is producer-written. `region_size` is
/// the real meta-region size (`find_meta`, not `find_meta_data`). Unchecked
/// offsets OOB-read; a SIGSEGV in `.process` is uncatchable. A stride near
/// `i32::MAX` overflows the multiply, so every step is checked.
///
/// Returned `len()` is `stride·(bh−1) + row`: the last row contributes its
/// visible bytes, not a full stride, so a flush-against-end bitmap is
/// accepted. Extracted so it can be tested without a live compositor.
fn bitmap_extent(
    bmp_off: usize,
    pix_off: usize,
    stride: usize,
    row: usize,
    bh: usize,
    region_size: usize,
) -> Option<std::ops::Range<usize>> {
    if bh == 0 || row == 0 || stride < row {
        return None;
    }
    let span = stride.checked_mul(bh - 1)?.checked_add(row)?;
    let start = bmp_off.checked_add(pix_off)?;
    let end = start.checked_add(span)?;
    (end <= region_size).then_some(start..end)
}

/// Packed-RGB (R,G,B) byte offsets and bytes-per-pixel, or `None` for a
/// layout the 8-bit CPU blit does not handle (YUV / 10-bit).
pub(super) fn dst_offsets(fmt: PixelFormat) -> Option<(usize, usize, usize, usize)> {
    Some(match fmt {
        PixelFormat::Bgrx | PixelFormat::Bgra => (2, 1, 0, 4),
        PixelFormat::Rgbx | PixelFormat::Rgba => (0, 1, 2, 4),
        PixelFormat::Rgb => (0, 1, 2, 3),
        PixelFormat::Bgr => (2, 1, 0, 3),
        _ => return None,
    })
}

/// Alpha-blend the cached cursor into a packed 10-bit (`X2Rgb10`/`X2Bgr10`)
/// CPU frame: unpack, blend 8-bit channels scaled to 10 (`v<<2 | v>>6`),
/// repack. The frame is PQ, so the bitmap is re-encoded as PQ first.
/// `r_shift` is R's bit offset (20 for x:R:G:B, 0 for x:B:G:R); G is always
/// at 10 and B mirrors R.
pub(super) fn composite_cursor_rgb10(
    tight: &mut [u8],
    w: usize,
    h: usize,
    r_shift: u32,
    cursor: &CursorState,
) {
    let b_shift = 20 - r_shift; // 0 or 20 — opposite end from R
    let (bw, bh) = (cursor.bw as i32, cursor.bh as i32);
    let rgba = pf_frame::hdr::pq_rgba_cached(&cursor.rgba);
    for cy in 0..bh {
        let dy = cursor.y + cy;
        if dy < 0 || dy as usize >= h {
            continue;
        }
        for cx in 0..bw {
            let dx = cursor.x + cx;
            if dx < 0 || dx as usize >= w {
                continue;
            }
            let s = ((cy * bw + cx) as usize) * 4;
            let a = rgba[s + 3] as u32;
            if a == 0 {
                continue;
            }
            // 8-bit → 10-bit: replicate the top bits into the bottom.
            let up10 = |v: u8| ((v as u32) << 2) | ((v as u32) >> 6);
            let (sr, sg, sb) = (up10(rgba[s]), up10(rgba[s + 1]), up10(rgba[s + 2]));
            let di = (dy as usize * w + dx as usize) * 4;
            let px = u32::from_le_bytes(tight[di..di + 4].try_into().unwrap());
            let blend = |dst: u32, src: u32| (src * a + dst * (255 - a)) / 255;
            let dr = blend((px >> r_shift) & 0x3ff, sr);
            let dg = blend((px >> 10) & 0x3ff, sg);
            let db = blend((px >> b_shift) & 0x3ff, sb);
            let out = (px & 0xc000_0000) | (dr << r_shift) | (dg << 10) | (db << b_shift);
            tight[di..di + 4].copy_from_slice(&out.to_le_bytes());
        }
    }
}

/// Alpha-blend the cached cursor into the tightly-packed CPU frame, clipped
/// to the frame. Bitmap cap is 1024×1024.
pub(super) fn composite_cursor(
    tight: &mut [u8],
    w: usize,
    h: usize,
    fmt: PixelFormat,
    cursor: &CursorState,
) {
    if !cursor.visible || cursor.rgba.is_empty() {
        return;
    }
    // Packed 10-bit HDR: unpack/repack, not `dst_offsets`.
    match fmt {
        PixelFormat::X2Rgb10 => return composite_cursor_rgb10(tight, w, h, 20, cursor),
        PixelFormat::X2Bgr10 => return composite_cursor_rgb10(tight, w, h, 0, cursor),
        _ => {}
    }
    let Some((ri, gi, bi, bpp)) = dst_offsets(fmt) else {
        return;
    };
    let (bw, bh) = (cursor.bw as i32, cursor.bh as i32);
    for cy in 0..bh {
        let dy = cursor.y + cy;
        if dy < 0 || dy as usize >= h {
            continue;
        }
        for cx in 0..bw {
            let dx = cursor.x + cx;
            if dx < 0 || dx as usize >= w {
                continue;
            }
            let s = ((cy * bw + cx) as usize) * 4;
            let a = cursor.rgba[s + 3] as u32;
            if a == 0 {
                continue;
            }
            let (sr, sg, sb) = (
                cursor.rgba[s] as u32,
                cursor.rgba[s + 1] as u32,
                cursor.rgba[s + 2] as u32,
            );
            let di = (dy as usize * w + dx as usize) * bpp;
            let blend = |dst: u8, src: u32| ((src * a + dst as u32 * (255 - a)) / 255) as u8;
            tight[di + ri] = blend(tight[di + ri], sr);
            tight[di + gi] = blend(tight[di + gi], sg);
            tight[di + bi] = blend(tight[di + bi], sb);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(x: i32, y: i32, w: u32, h: u32, rgb: (u8, u8, u8), a: u8) -> CursorState {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            px.extend_from_slice(&[rgb.0, rgb.1, rgb.2, a]);
        }
        CursorState {
            visible: true,
            x,
            y,
            rgba: Arc::new(px),
            bw: w,
            bh: h,
            serial: 1,
            hot_x: 0,
            hot_y: 0,
            seen_meta: true,
            id0_hides: false,
        }
    }

    #[test]
    fn id_zero_hides_only_on_a_rewriting_producer() {
        // Rewriting producer (`id0_hides`): id 0 is written fresh on every
        // buffer, so it is the hide.
        let mut kwin = cursor(10, 10, 8, 8, (255, 255, 255), 255);
        kwin.id0_hides = true;
        assert!(!note_cursor_id(&mut kwin, 0), "id 0 parses no further");
        let o = kwin.overlay().expect("bitmap stays cached across a hide");
        assert!(!o.visible, "KWin id 0 must hide the overlay");
        assert!(note_cursor_id(&mut kwin, 1));
        assert!(kwin.overlay().expect("still cached").visible);

        // Stale-meta producer: recycled buffers carry id-0; last-known holds.
        let mut mutter = cursor(10, 10, 8, 8, (255, 255, 255), 255);
        assert!(!note_cursor_id(&mut mutter, 0));
        assert!(
            mutter.overlay().expect("cached").visible,
            "a stale-meta producer's id 0 must NOT hide"
        );
    }

    #[test]
    fn id_zero_before_any_bitmap_yields_no_overlay() {
        // Hide before any bitmap: `overlay()` stays `None`, not an empty cursor.
        let mut c = CursorState::new(true);
        assert!(!note_cursor_id(&mut c, 0));
        assert!(c.overlay().is_none());
    }

    #[test]
    fn bitmap_extent_accepts_a_bitmap_that_fits() {
        // 4×2 RGBA, tightly packed: 32 bytes at offset 0.
        assert_eq!(bitmap_extent(0, 0, 16, 16, 2, 32), Some(0..32));
        // Same bitmap behind a header + pixel offset.
        assert_eq!(bitmap_extent(24, 8, 16, 16, 2, 64), Some(32..64));
    }

    #[test]
    fn bitmap_extent_charges_the_last_row_only_its_visible_bytes() {
        // stride 32, row 16, 3 rows ⇒ 32*2 + 16 = 80, not 96. Trailing
        // stride padding is never read, so a flush-against-end bitmap fits.
        assert_eq!(bitmap_extent(0, 0, 32, 16, 3, 80), Some(0..80));
        assert_eq!(bitmap_extent(0, 0, 32, 16, 3, 79), None, "one byte short");
    }

    #[test]
    fn bitmap_extent_rejects_anything_past_the_region() {
        assert_eq!(bitmap_extent(0, 0, 16, 16, 2, 31), None);
        assert_eq!(bitmap_extent(1, 0, 16, 16, 2, 32), None);
        assert_eq!(bitmap_extent(0, 1, 16, 16, 2, 32), None);
        assert_eq!(bitmap_extent(0, 0, 16, 16, 1, 0), None);
    }

    /// `stride` and both offsets are producer-picked; each can overflow alone.
    #[test]
    fn bitmap_extent_survives_hostile_arithmetic() {
        // stride × (bh-1) overflows.
        assert_eq!(bitmap_extent(0, 0, usize::MAX, 16, 3, usize::MAX), None);
        // span + row overflows. ≥2 rows so `stride·(bh−1)` is already at the
        // ceiling: one row is `stride·0 == 0`, and `usize::MAX` row is then
        // in range — correct, because the caller already capped `bw` at 1024
        // (`row` ≤ 4096).
        assert_eq!(
            bitmap_extent(0, 0, usize::MAX, usize::MAX, 2, usize::MAX),
            None
        );
        // bmp_off + pix_off overflows.
        assert_eq!(bitmap_extent(usize::MAX, 1, 16, 16, 1, usize::MAX), None);
        // start + span overflows.
        assert_eq!(
            bitmap_extent(usize::MAX - 8, 0, 16, 16, 1, usize::MAX),
            None
        );
        // Near-`i32::MAX` stride must not wrap.
        assert_eq!(bitmap_extent(0, 0, i32::MAX as usize, 16, 1024, 4096), None);
    }

    #[test]
    fn bitmap_extent_rejects_degenerate_geometry() {
        assert_eq!(bitmap_extent(0, 0, 16, 16, 0, 4096), None, "zero rows");
        assert_eq!(bitmap_extent(0, 0, 16, 0, 2, 4096), None, "zero-width row");
        assert_eq!(bitmap_extent(0, 0, 8, 16, 2, 4096), None, "stride < row");
    }

    fn px_rgb(buf: &[u8], w: usize, x: usize, y: usize, fmt: PixelFormat) -> (u8, u8, u8) {
        let (ri, gi, bi, bpp) = dst_offsets(fmt).expect("packed layout");
        let i = (y * w + x) * bpp;
        (buf[i + ri], buf[i + gi], buf[i + bi])
    }

    #[test]
    fn every_packed_layout_lands_the_colour_in_its_own_channels() {
        for fmt in [
            PixelFormat::Bgrx,
            PixelFormat::Bgra,
            PixelFormat::Rgbx,
            PixelFormat::Rgba,
            PixelFormat::Rgb,
            PixelFormat::Bgr,
        ] {
            let bpp = dst_offsets(fmt).unwrap().3;
            let (w, h) = (4usize, 4usize);
            let mut buf = vec![0u8; w * h * bpp];
            composite_cursor(&mut buf, w, h, fmt, &cursor(1, 1, 1, 1, (255, 0, 0), 255));
            assert_eq!(px_rgb(&buf, w, 1, 1, fmt), (255, 0, 0), "{fmt:?}");
            assert_eq!(px_rgb(&buf, w, 0, 0, fmt), (0, 0, 0), "{fmt:?}");
            assert_eq!(px_rgb(&buf, w, 2, 1, fmt), (0, 0, 0), "{fmt:?}");
        }
    }

    #[test]
    fn a_cursor_hanging_off_every_edge_is_clipped_not_wrapped() {
        let (w, h, fmt) = (4usize, 4usize, PixelFormat::Bgrx);
        // Top-left: only the bottom-right quarter of a 2×2 lands, at (0, 0).
        let mut buf = vec![0u8; w * h * 4];
        composite_cursor(
            &mut buf,
            w,
            h,
            fmt,
            &cursor(-1, -1, 2, 2, (10, 20, 30), 255),
        );
        assert_eq!(px_rgb(&buf, w, 0, 0, fmt), (10, 20, 30));
        assert_eq!(px_rgb(&buf, w, 1, 0, fmt), (0, 0, 0));
        assert_eq!(px_rgb(&buf, w, 0, 1, fmt), (0, 0, 0));
        // Bottom-right: only the top-left quarter lands, at (3, 3).
        let mut buf = vec![0u8; w * h * 4];
        composite_cursor(&mut buf, w, h, fmt, &cursor(3, 3, 2, 2, (10, 20, 30), 255));
        assert_eq!(px_rgb(&buf, w, 3, 3, fmt), (10, 20, 30));
        assert_eq!(px_rgb(&buf, w, 2, 3, fmt), (0, 0, 0));
        // Fully outside in each direction: the frame is untouched.
        for pos in [(-2, 0), (0, -2), (4, 0), (0, 4), (-9, -9), (99, 99)] {
            let mut buf = vec![0u8; w * h * 4];
            composite_cursor(
                &mut buf,
                w,
                h,
                fmt,
                &cursor(pos.0, pos.1, 2, 2, (255, 255, 255), 255),
            );
            assert!(buf.iter().all(|&b| b == 0), "drew something at {pos:?}");
        }
    }

    #[test]
    fn transparent_and_hidden_cursors_draw_nothing() {
        let (w, h, fmt) = (2usize, 2usize, PixelFormat::Bgrx);
        let mut buf = vec![0u8; w * h * 4];
        composite_cursor(&mut buf, w, h, fmt, &cursor(0, 0, 2, 2, (255, 255, 255), 0));
        assert!(buf.iter().all(|&b| b == 0));
        let mut c = cursor(0, 0, 2, 2, (255, 255, 255), 255);
        c.visible = false;
        let mut buf = vec![0u8; w * h * 4];
        composite_cursor(&mut buf, w, h, fmt, &c);
        assert!(buf.iter().all(|&b| b == 0));
        let mut c = cursor(0, 0, 2, 2, (255, 255, 255), 255);
        c.rgba = Arc::new(Vec::new());
        let mut buf = vec![0u8; w * h * 4];
        composite_cursor(&mut buf, w, h, fmt, &c);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn half_alpha_blends_toward_the_destination() {
        let (w, h, fmt) = (1usize, 1usize, PixelFormat::Bgrx);
        // dst white, src black at 50% → 127: (0*128 + 255*127)/255.
        let mut buf = vec![255u8; w * h * 4];
        composite_cursor(&mut buf, w, h, fmt, &cursor(0, 0, 1, 1, (0, 0, 0), 128));
        assert_eq!(px_rgb(&buf, w, 0, 0, fmt), (127, 127, 127));
    }

    #[test]
    fn unsupported_layouts_are_declined() {
        assert!(dst_offsets(PixelFormat::Nv12).is_none());
        assert!(dst_offsets(PixelFormat::Yuv444).is_none());
        let (w, h) = (2usize, 2usize);
        let mut buf = vec![0u8; w * h * 4];
        composite_cursor(
            &mut buf,
            w,
            h,
            PixelFormat::Nv12,
            &cursor(0, 0, 2, 2, (255, 255, 255), 255),
        );
        assert!(buf.iter().all(|&b| b == 0), "NV12 must not be blitted");
    }

    /// Pack `x:R:G:B` (`X2Rgb10`): R at 20, G at 10, B at 0.
    fn pack_x2rgb10(r: u32, g: u32, b: u32) -> [u8; 4] {
        (0xC000_0000 | (r << 20) | (g << 10) | b).to_le_bytes()
    }

    #[test]
    fn the_10bit_path_round_trips_an_untouched_pixel() {
        // Alpha 0 skips the blend; the packed pixel (incl. top two alpha
        // bits) must come back bit-identical.
        for (r, g, b) in [(0, 0, 0), (1023, 1023, 1023), (940, 64, 512), (1, 2, 3)] {
            let src = pack_x2rgb10(r, g, b);
            let mut buf = src.to_vec();
            composite_cursor(
                &mut buf,
                1,
                1,
                PixelFormat::X2Rgb10,
                &cursor(0, 0, 1, 1, (255, 255, 255), 0),
            );
            assert_eq!(buf, src, "({r},{g},{b}) was modified by a zero-alpha blend");
        }
    }

    #[test]
    fn the_10bit_path_writes_pq_at_the_right_shift() {
        // Opaque sRGB red → PQ BT.2020 at 203 nits (136, 83, 56) → 10-bit (`v<<2 | v>>6`).
        let mut buf = pack_x2rgb10(0, 0, 0).to_vec();
        composite_cursor(
            &mut buf,
            1,
            1,
            PixelFormat::X2Rgb10,
            &cursor(0, 0, 1, 1, (255, 0, 0), 255),
        );
        let v = u32::from_le_bytes(buf[..4].try_into().unwrap());
        assert_eq!((v >> 20) & 0x3ff, 546, "R");
        assert_eq!((v >> 10) & 0x3ff, 333, "G");
        assert_eq!(v & 0x3ff, 224, "B");
        assert_eq!(v & 0xc000_0000, 0xc000_0000, "alpha bits preserved");

        // X2Bgr10: R at bit 0, B at 20 — same cursor, other end.
        let mut buf = pack_x2rgb10(0, 0, 0).to_vec();
        composite_cursor(
            &mut buf,
            1,
            1,
            PixelFormat::X2Bgr10,
            &cursor(0, 0, 1, 1, (255, 0, 0), 255),
        );
        let v = u32::from_le_bytes(buf[..4].try_into().unwrap());
        assert_eq!(v & 0x3ff, 546, "R at bit 0 for x:B:G:R");
        assert_eq!((v >> 20) & 0x3ff, 224, "B at bit 20");
    }

    #[test]
    fn the_10bit_path_clips_like_the_8bit_one() {
        let (w, h) = (2usize, 2usize);
        let mut buf: Vec<u8> = (0..w * h).flat_map(|_| pack_x2rgb10(0, 0, 0)).collect();
        let before = buf.clone();
        composite_cursor(
            &mut buf,
            w,
            h,
            PixelFormat::X2Rgb10,
            &cursor(-5, -5, 2, 2, (255, 255, 255), 255),
        );
        assert_eq!(buf, before);
        // Straddling the top-left corner: only (0, 0) is written.
        composite_cursor(
            &mut buf,
            w,
            h,
            PixelFormat::X2Rgb10,
            &cursor(-1, -1, 2, 2, (255, 255, 255), 255),
        );
        let p0 = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let p1 = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        // sRGB white is 203 nits in PQ: code 148 → 594.
        assert_eq!((p0 >> 20) & 0x3ff, 594);
        assert_eq!((p1 >> 20) & 0x3ff, 0);
    }

    #[test]
    fn each_bitmap_format_is_decoded_to_straight_rgba() {
        let s = [1u8, 2, 3, 4];
        assert_eq!(
            decode_bitmap_pixel(spa::sys::SPA_VIDEO_FORMAT_RGBA, &s),
            (1, 2, 3, 4)
        );
        assert_eq!(
            decode_bitmap_pixel(spa::sys::SPA_VIDEO_FORMAT_BGRA, &s),
            (3, 2, 1, 4)
        );
        assert_eq!(
            decode_bitmap_pixel(spa::sys::SPA_VIDEO_FORMAT_ARGB, &s),
            (2, 3, 4, 1)
        );
        assert_eq!(
            decode_bitmap_pixel(spa::sys::SPA_VIDEO_FORMAT_ABGR, &s),
            (4, 3, 2, 1)
        );
        // Unknown 4-byte format reads as RGBA, not rejected.
        assert_eq!(decode_bitmap_pixel(0xdead_beef, &s), (1, 2, 3, 4));
    }
}
