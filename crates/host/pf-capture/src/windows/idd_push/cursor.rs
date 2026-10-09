//! Host side of the hardware-cursor channel.
//!
//! The capturer creates an unnamed [`CursorShm`] section, delivers it to pf-vdisplay
//! (IddCx hardware cursor — DWM then excludes the pointer from consumed frames), and
//! seqlock-reads the driver's publishes at encode-tick pace into the same
//! [`pf_frame::CursorOverlay`] the Linux portal path produces. Downstream (forwarder,
//! wire, client renderer) is shared. IddCx positions are already relative to the monitor.
//! The header also carries the one fact the driver's own blend needs and cannot query in
//! session 0: the HDR desktop's SDR-white scale.

use super::*;
use pf_driver_proto::cursor::{
    shape_extent, shape_rgba, CursorShm, ShapeRgba, CURSOR_MAGIC, CURSOR_SHAPE_BYTES,
    CURSOR_SHAPE_OFFSET, CURSOR_SHM_SIZE,
};
use std::sync::atomic::AtomicU32;

/// Host end of one monitor's cursor channel. The mapping stays valid for the capturer's
/// life.
pub(super) struct CursorShared {
    section: MappedSection,
    /// Last `shape_id` whose pixels were converted. Position-only updates (the common
    /// case) reuse it — a refcount bump, no pixel work.
    cached_id: u32,
    cached: Option<ConvertedShape>,
}

struct ConvertedShape {
    rgba: std::sync::Arc<Vec<u8>>,
    w: u32,
    h: u32,
    hot_x: u32,
    hot_y: u32,
}

impl From<ShapeRgba> for ConvertedShape {
    fn from(s: ShapeRgba) -> Self {
        Self {
            rgba: std::sync::Arc::new(s.rgba),
            w: s.w,
            h: s.h,
            hot_x: s.hot_x,
            hot_y: s.hot_y,
        }
    }
}

impl CursorShared {
    /// Create + initialize the section (zeroed, magic last, seq even/zero). The origin stays
    /// 0: IddCx positions are monitor-relative, and a driver that still subtracts it must
    /// subtract nothing. The returned handle is the section itself (owned by `self`); the
    /// caller duplicates it into the WUDFHost.
    pub(super) fn create() -> Result<CursorShared> {
        // SAFETY: plain FFI. Unnamed pagefile-backed section, host-lifetime owned; the view is
        // mapped once here and unmapped exactly once by `MappedSection::drop` (which unmaps before
        // closing the mapping handle). No borrow into the view outlives the `MappedSection`: every
        // access goes through `&self` accessors on the owner.
        let section = unsafe {
            let map = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                CURSOR_SHM_SIZE as u32,
                PCWSTR::null(),
            )
            .context("CreateFileMapping(cursor)")?;
            let map = OwnedHandle::from_raw_handle(map.0 as _);
            let view = MapViewOfFile(
                HANDLE(map.as_raw_handle()),
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                CURSOR_SHM_SIZE,
            );
            if view.Value.is_null() {
                bail!("MapViewOfFile failed for the cursor section");
            }
            let shm = view.Value.cast::<CursorShm>();
            std::ptr::write_bytes(view.Value.cast::<u8>(), 0, CURSOR_SHM_SIZE);
            // Magic last: the driver validates it at adopt. Seq 0 is even = consistent.
            std::sync::atomic::fence(Ordering::Release);
            (*shm).magic = CURSOR_MAGIC;
            MappedSection { handle: map, view }
        };
        Ok(CursorShared {
            section,
            cached_id: 0,
            cached: None,
        })
    }

    pub(super) fn section_handle(&self) -> BorrowedHandle<'_> {
        self.section.handle.as_handle()
    }

    /// Tell the driver where this HDR desktop puts SDR white (1.0 = 80 nits) for its blend
    /// onto an FP16 frame; `0` = SDR. One aligned word, read by the driver per publish.
    pub(super) fn set_sdr_white_scale(&self, scale: f32) {
        let shm = self.section.ptr::<CursorShm>();
        // SAFETY: the view spans `CURSOR_SHM_SIZE` for `self`'s lifetime; the field is a
        // 4-aligned u32 in the fixed layout, written whole.
        unsafe {
            std::ptr::addr_of_mut!((*shm).sdr_white_scale).write_volatile(scale.to_bits());
        }
    }

    /// Seqlock-read the latest publish as a frame-relative [`pf_frame::CursorOverlay`].
    /// `None` until the first publish. Hidden pointer → `Some` with `visible: false` — the
    /// forwarder turns that into the client's relative-mode hint, as on Linux.
    pub(super) fn read(&mut self) -> Option<pf_frame::CursorOverlay> {
        let shm = self.section.ptr::<CursorShm>();
        // SAFETY: the view spans `CURSOR_SHM_SIZE` for `self`'s lifetime; `seq` is
        // 4-aligned at offset 4 in the fixed layout.
        let seq = unsafe { &*std::ptr::addr_of!((*shm).seq).cast::<AtomicU32>() };
        for _ in 0..64 {
            let s1 = seq.load(Ordering::Acquire);
            if s1 == 0 {
                return None; // seq 0: no publish yet (even, but not a valid snapshot)
            }
            if s1 & 1 != 0 {
                std::hint::spin_loop();
                continue; // odd seq: writer mid-update
            }
            // SAFETY: header is inside the mapped view; a torn read is discarded by the
            // seq re-check below.
            let hdr = unsafe { std::ptr::read_volatile(shm) };
            if hdr.visible != 0 && hdr.shape_id != self.cached_id {
                let (_, rows, pitch) = shape_extent(&hdr);
                let mut raw = vec![0u8; rows * pitch];
                // SAFETY: the shape region is `CURSOR_SHAPE_BYTES` from `CURSOR_SHAPE_OFFSET`
                // in the mapped view; `shape_extent` clamps `rows * pitch` to it.
                unsafe {
                    debug_assert!(rows * pitch <= CURSOR_SHAPE_BYTES);
                    std::ptr::copy_nonoverlapping(
                        self.section.ptr::<u8>().add(CURSOR_SHAPE_OFFSET),
                        raw.as_mut_ptr(),
                        rows * pitch,
                    );
                }
                // Writer raced mid-shape (seq moved) — retry without caching. The fence is what
                // keeps the payload reads above from being reordered after this check: an
                // acquire LOAD only orders what follows it. Free on x86, load-load ordering on
                // aarch64, where the reads could otherwise straddle a writer's update.
                std::sync::atomic::fence(Ordering::Acquire);
                if seq.load(Ordering::Relaxed) != s1 {
                    continue;
                }
                self.cached = Some(shape_rgba(&hdr, &raw).into());
                self.cached_id = hdr.shape_id;
            } else {
                std::sync::atomic::fence(Ordering::Acquire);
                if seq.load(Ordering::Relaxed) != s1 {
                    continue;
                }
            }
            // Hidden before any shape was seen is still a hide the client must hear.
            let Some(shape) = self.cached.as_ref() else {
                return (hdr.visible == 0).then(|| pf_frame::CursorOverlay {
                    x: hdr.x,
                    y: hdr.y,
                    w: 0,
                    h: 0,
                    rgba: std::sync::Arc::new(Vec::new()),
                    serial: 0,
                    hot_x: 0,
                    hot_y: 0,
                    visible: false,
                });
            };
            return Some(pf_frame::CursorOverlay {
                x: hdr.x,
                y: hdr.y,
                w: shape.w,
                h: shape.h,
                rgba: shape.rgba.clone(),
                serial: u64::from(hdr.shape_id),
                hot_x: shape.hot_x,
                hot_y: shape.hot_y,
                visible: hdr.visible != 0,
            });
        }
        None // writer wedged mid-seq; skip this tick
    }
}
