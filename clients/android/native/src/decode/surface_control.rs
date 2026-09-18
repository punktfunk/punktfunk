//! The `ASurfaceControl` compositor layer behind the ASurfaceControl presenter backend.
//!
//! This is the Android analogue of what the Apple client gets from `CAMetalDisplayLink` +
//! `preferredFrameLatency = 1`: a present path that schedules each frame against the panel's own
//! timeline and hands back the *real* present feedback, instead of the MediaCodec→SurfaceView→
//! BufferQueue path that predicts the latch and hopes the `OnFrameRendered` callbacks arrive.
//!
//! A `Layer` owns one `ASurfaceControl` created as a child of the SurfaceView's `ANativeWindow`;
//! the decoder renders into an `AImageReader` and the
//! presenter composites each acquired `AHardwareBuffer` onto this layer via an `ASurfaceTransaction`
//! that carries a desired present time (the single actuator both present modes drive) and an
//! acquire fence. Every applied transaction registers a one-shot completion callback that reports
//! the frame's latch time, its present fence and the *previous* buffer's release fence back
//! through the decode loop's event channel — the real vsync the slot scheduler is phased on.
//!
//! Every `ASurface*` entry point is **API 29** — above the crate's minSdk-28 floor — so all are
//! `dlsym`-resolved from `libandroid.so`, exactly as [`crate::adpf`] and [`super::vsync`] resolve
//! their own >-floor symbols; a hard import of any of them would make `System.loadLibrary` fail on
//! every API-28 device even where this backend is never selected. Absent (or a null layer) ⇒
//! [`Layer::create`] returns `None` and the caller falls back to the SurfaceView presenter.

use ndk::hardware_buffer::HardwareBuffer;
use ndk::native_window::NativeWindow;
use std::ffi::c_void;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, OnceLock};

use super::async_loop::DecodeEvent;

// ---- Opaque native types (not in `ndk-sys 0.6`) ------------------------------------------------

#[repr(C)]
struct ASurfaceControl {
    _p: [u8; 0],
}
#[repr(C)]
struct ASurfaceTransaction {
    _p: [u8; 0],
}
#[repr(C)]
struct ASurfaceTransactionStats {
    _p: [u8; 0],
}

/// `ARect` — the `setGeometry` source/destination rectangle (`android/native_window.h`).
#[repr(C)]
#[derive(Clone, Copy)]
struct ARect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

/// `ANATIVEWINDOW_TRANSFORM_IDENTITY` — no rotation/flip; the decoder already emits upright frames.
const TRANSFORM_IDENTITY: i32 = 0;
/// `ASURFACE_TRANSACTION_VISIBILITY_SHOW`.
const VISIBILITY_SHOW: i8 = 1;

/// [`HdrMeta`](punktfunk_core::quic::HdrMeta) (ST.2086 G, B, R in 1/50000; mastering luminance in
/// 0.0001 nits) as the NDK's float structs.
fn hdr_metadata(m: &punktfunk_core::quic::HdrMeta) -> (AHdrMetadataSmpte2086, AHdrMetadataCta8613) {
    let xy = |[x, y]: [u16; 2]| AColorXy {
        x: f32::from(x) / 50_000.0,
        y: f32::from(y) / 50_000.0,
    };
    let [g, b, r] = m.display_primaries;
    (
        AHdrMetadataSmpte2086 {
            red: xy(r),
            green: xy(g),
            blue: xy(b),
            white: xy(m.white_point),
            max_luminance: m.max_display_mastering_luminance as f32 / 10_000.0,
            min_luminance: m.min_display_mastering_luminance as f32 / 10_000.0,
        },
        AHdrMetadataCta8613 {
            max_content_light_level: f32::from(m.max_cll),
            max_frame_average_light_level: f32::from(m.max_fall),
        },
    )
}

// ---- The `dlsym`-resolved entry-point table ----------------------------------------------------

type CreateFromWindowFn = unsafe extern "C" fn(
    *mut ndk_sys::ANativeWindow,
    *const std::ffi::c_char,
) -> *mut ASurfaceControl;
type AcReleaseFn = unsafe extern "C" fn(*mut ASurfaceControl);
type TxnCreateFn = unsafe extern "C" fn() -> *mut ASurfaceTransaction;
type TxnDeleteFn = unsafe extern "C" fn(*mut ASurfaceTransaction);
type TxnApplyFn = unsafe extern "C" fn(*mut ASurfaceTransaction);
type TxnSetBufferFn = unsafe extern "C" fn(
    *mut ASurfaceTransaction,
    *mut ASurfaceControl,
    *mut ndk_sys::AHardwareBuffer,
    RawFd,
);
type TxnSetVisibilityFn = unsafe extern "C" fn(*mut ASurfaceTransaction, *mut ASurfaceControl, i8);
type TxnSetZOrderFn = unsafe extern "C" fn(*mut ASurfaceTransaction, *mut ASurfaceControl, i32);
type TxnSetGeometryFn = unsafe extern "C" fn(
    *mut ASurfaceTransaction,
    *mut ASurfaceControl,
    *const ARect,
    *const ARect,
    i32,
);
type TxnSetDesiredPresentTimeFn = unsafe extern "C" fn(*mut ASurfaceTransaction, i64);
type TxnSetBufferDataSpaceFn =
    unsafe extern "C" fn(*mut ASurfaceTransaction, *mut ASurfaceControl, i32);
type TxnSetFrameRateFn =
    unsafe extern "C" fn(*mut ASurfaceTransaction, *mut ASurfaceControl, f32, i8);

#[repr(C)]
struct AColorXy {
    x: f32,
    y: f32,
}

/// `AHdrMetadata_smpte2086`: chromaticities as floats, luminance in nits.
#[repr(C)]
struct AHdrMetadataSmpte2086 {
    red: AColorXy,
    green: AColorXy,
    blue: AColorXy,
    white: AColorXy,
    max_luminance: f32,
    min_luminance: f32,
}

/// `AHdrMetadata_cta861_3`, in nits.
#[repr(C)]
struct AHdrMetadataCta8613 {
    max_content_light_level: f32,
    max_frame_average_light_level: f32,
}

type TxnSetHdrSmpte2086Fn = unsafe extern "C" fn(
    *mut ASurfaceTransaction,
    *mut ASurfaceControl,
    *const AHdrMetadataSmpte2086,
);
type TxnSetHdrCta8613Fn = unsafe extern "C" fn(
    *mut ASurfaceTransaction,
    *mut ASurfaceControl,
    *const AHdrMetadataCta8613,
);
type OnCompleteCb = unsafe extern "C" fn(*mut c_void, *mut ASurfaceTransactionStats);
type TxnSetOnCompleteFn = unsafe extern "C" fn(*mut ASurfaceTransaction, *mut c_void, OnCompleteCb);
type StatsGetLatchTimeFn = unsafe extern "C" fn(*mut ASurfaceTransactionStats) -> i64;
type StatsGetPrevReleaseFenceFn =
    unsafe extern "C" fn(*mut ASurfaceTransactionStats, *mut ASurfaceControl) -> RawFd;
type StatsGetPresentFenceFn = unsafe extern "C" fn(*mut ASurfaceTransactionStats) -> RawFd;

struct Api {
    create_from_window: CreateFromWindowFn,
    ac_release: AcReleaseFn,
    txn_create: TxnCreateFn,
    txn_delete: TxnDeleteFn,
    txn_apply: TxnApplyFn,
    txn_set_buffer: TxnSetBufferFn,
    txn_set_visibility: TxnSetVisibilityFn,
    txn_set_z_order: TxnSetZOrderFn,
    txn_set_geometry: TxnSetGeometryFn,
    txn_set_present_time: TxnSetDesiredPresentTimeFn,
    /// `setBufferDataSpace` is present from API 29 in practice but historically under-declared —
    /// resolved optionally, so an SDR stream (which never touches it) works even where it is absent.
    txn_set_dataspace: Option<TxnSetBufferDataSpaceFn>,
    /// `setFrameRate` is **API 30** — optional, `None` on API 29.
    txn_set_frame_rate: Option<TxnSetFrameRateFn>,
    /// The HDR10 static metadata setters (API 29), optional like `setBufferDataSpace`.
    txn_set_hdr_smpte2086: Option<TxnSetHdrSmpte2086Fn>,
    txn_set_hdr_cta861_3: Option<TxnSetHdrCta8613Fn>,
    txn_set_on_complete: TxnSetOnCompleteFn,
    stats_latch_time: StatsGetLatchTimeFn,
    stats_prev_release_fence: StatsGetPrevReleaseFenceFn,
    stats_present_fence: StatsGetPresentFenceFn,
}

impl Api {
    /// Resolve the whole `ASurface*` table from `libandroid.so`, or `None` on API < 29 (any required
    /// symbol absent). The two optional entries (`setBufferDataSpace`, `setFrameRate`) do not gate.
    fn resolve() -> Option<Api> {
        // SAFETY: `dlopen` of the always-mapped `libandroid.so` (only bumps its refcount; never
        // closed — a process-lifetime handle). Each `dlsym` returns null when the symbol is absent
        // (device below API 29), checked before transmuting the non-null pointer to its fn type.
        unsafe {
            let lib = libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW);
            if lib.is_null() {
                return None;
            }
            let req = |name: &std::ffi::CStr| -> Option<*mut c_void> {
                let p = libc::dlsym(lib, name.as_ptr());
                (!p.is_null()).then_some(p)
            };
            Some(Api {
                create_from_window: std::mem::transmute::<*mut c_void, CreateFromWindowFn>(req(
                    c"ASurfaceControl_createFromWindow",
                )?),
                ac_release: std::mem::transmute::<*mut c_void, AcReleaseFn>(req(
                    c"ASurfaceControl_release",
                )?),
                txn_create: std::mem::transmute::<*mut c_void, TxnCreateFn>(req(
                    c"ASurfaceTransaction_create",
                )?),
                txn_delete: std::mem::transmute::<*mut c_void, TxnDeleteFn>(req(
                    c"ASurfaceTransaction_delete",
                )?),
                txn_apply: std::mem::transmute::<*mut c_void, TxnApplyFn>(req(
                    c"ASurfaceTransaction_apply",
                )?),
                txn_set_buffer: std::mem::transmute::<*mut c_void, TxnSetBufferFn>(req(
                    c"ASurfaceTransaction_setBuffer",
                )?),
                txn_set_visibility: std::mem::transmute::<*mut c_void, TxnSetVisibilityFn>(req(
                    c"ASurfaceTransaction_setVisibility",
                )?),
                txn_set_z_order: std::mem::transmute::<*mut c_void, TxnSetZOrderFn>(req(
                    c"ASurfaceTransaction_setZOrder",
                )?),
                txn_set_geometry: std::mem::transmute::<*mut c_void, TxnSetGeometryFn>(req(
                    c"ASurfaceTransaction_setGeometry",
                )?),
                txn_set_present_time: std::mem::transmute::<*mut c_void, TxnSetDesiredPresentTimeFn>(
                    req(c"ASurfaceTransaction_setDesiredPresentTime")?,
                ),
                txn_set_dataspace: req(c"ASurfaceTransaction_setBufferDataSpace")
                    .map(|p| std::mem::transmute::<*mut c_void, TxnSetBufferDataSpaceFn>(p)),
                txn_set_frame_rate: req(c"ASurfaceTransaction_setFrameRate")
                    .map(|p| std::mem::transmute::<*mut c_void, TxnSetFrameRateFn>(p)),
                txn_set_hdr_smpte2086: req(c"ASurfaceTransaction_setHdrMetadata_smpte2086")
                    .map(|p| std::mem::transmute::<*mut c_void, TxnSetHdrSmpte2086Fn>(p)),
                txn_set_hdr_cta861_3: req(c"ASurfaceTransaction_setHdrMetadata_cta861_3")
                    .map(|p| std::mem::transmute::<*mut c_void, TxnSetHdrCta8613Fn>(p)),
                txn_set_on_complete: std::mem::transmute::<*mut c_void, TxnSetOnCompleteFn>(req(
                    c"ASurfaceTransaction_setOnComplete",
                )?),
                stats_latch_time: std::mem::transmute::<*mut c_void, StatsGetLatchTimeFn>(req(
                    c"ASurfaceTransactionStats_getLatchTime",
                )?),
                stats_prev_release_fence: std::mem::transmute::<
                    *mut c_void,
                    StatsGetPrevReleaseFenceFn,
                >(req(
                    c"ASurfaceTransactionStats_getPreviousReleaseFenceFd",
                )?),
                stats_present_fence: std::mem::transmute::<*mut c_void, StatsGetPresentFenceFn>(
                    req(c"ASurfaceTransactionStats_getPresentFenceFd")?,
                ),
            })
        }
    }
}

/// The `ASurfaceControl` handle, reference-counted so it outlives every in-flight transaction. The
/// layer holds one `Arc`; each pending completion callback's context holds another. `release` is
/// called exactly once — when the layer is dropped AND the last outstanding callback has fired — so
/// a completion that lands after teardown never indexes a freed control (the render-callback
/// reclaim hazard, in the transaction world).
struct ScHandle {
    sc: *mut ASurfaceControl,
    release: AcReleaseFn,
}

// SAFETY: `sc` is only ever passed back to `ASurface*` C entry points (never dereferenced in Rust),
// and its release is serialised by the `Arc` refcount reaching zero on whichever thread drops last.
unsafe impl Send for ScHandle {}
// SAFETY: as above — the raw handle is opaque to Rust and only handed to the thread-safe `ASurface*`
// C API; shared read access across threads (the completion callback) never mutates it.
unsafe impl Sync for ScHandle {}

impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: created by `createFromWindow`; the `Arc` guarantees this is the sole, final release
        // and that no transaction or callback still references `sc`.
        unsafe { (self.release)(self.sc) };
    }
}

/// One presented transaction's real feedback, posted from the completion callback (a binder thread)
/// into the decode loop's event channel. The loop matches `seq` to the buffer it retired and frees
/// it once `prev_release_fence` signals.
pub(super) struct PresentComplete {
    /// The presenter's monotonically increasing submit sequence for this transaction.
    pub seq: u64,
    /// SurfaceFlinger's latch instant for this frame (`CLOCK_MONOTONIC` ns) — the truthful present
    /// clock: consecutive latches are one true panel period apart, and `latch − release` is the
    /// real `latch` stat, both of which the predicted path could only guess at.
    pub latch_ns: i64,
    /// The release fence for the buffer this transaction REPLACED (the previous frame on the
    /// layer), or `None` when the platform reports none. The loop deletes that buffer's image with
    /// this fence so it is returned to the reader's pool only once SurfaceFlinger is done with it.
    pub prev_release_fence: Option<OwnedFd>,
    /// The present fence: signals at the hardware vsync that scanned this frame out. Usually still
    /// pending when the completion arrives; [`fence_signal_ns`] reads it later, never waits.
    pub present_fence: Option<OwnedFd>,
}

/// The completion callback's per-transaction context, leaked as a raw pointer into
/// `setOnComplete` and reclaimed inside the callback (which fires exactly once per applied
/// transaction). Carries only `Send` data so the binder-thread callback is sound.
struct CompleteCtx {
    tx: mpsc::Sender<DecodeEvent>,
    seq: u64,
    /// A shared reference to the layer's `ASurfaceControl`, needed to read the per-surface release
    /// fence out of the stats. Holding the `Arc` keeps the control alive for the callback even if
    /// the layer was already dropped.
    sc: Arc<ScHandle>,
    prev_fence_fn: StatsGetPrevReleaseFenceFn,
    present_fence_fn: StatsGetPresentFenceFn,
    latch_fn: StatsGetLatchTimeFn,
}

/// The `ASurfaceTransaction_OnComplete` trampoline (a binder thread). Reclaims its leaked context,
/// reads the real latch time + the previous buffer's release fence, and forwards them to the decode
/// loop. Panic-free by construction (an unwind out of an `extern "C"` fn would abort the process).
unsafe extern "C" fn on_complete(context: *mut c_void, stats: *mut ASurfaceTransactionStats) {
    if context.is_null() {
        return;
    }
    // SAFETY: `context` is the `Box<CompleteCtx>` leaked in `Layer::present`; the platform delivers
    // it exactly once per applied transaction, so this single reclaim is correct.
    let ctx = unsafe { Box::from_raw(context as *mut CompleteCtx) };
    let latch_ns = if stats.is_null() {
        0
    } else {
        // SAFETY: `stats` is valid for the duration of this callback (platform contract).
        unsafe { (ctx.latch_fn)(stats) }
    };
    let prev_release_fence = if stats.is_null() {
        None
    } else {
        // SAFETY: valid stats + the layer's live `ASurfaceControl`; a returned fd is owned by us
        // and closed via `OwnedFd`. `-1` means no fence.
        let fd = unsafe { (ctx.prev_fence_fn)(stats, ctx.sc.sc) };
        // SAFETY: a non-negative fd returned by `getPreviousReleaseFenceFd` is a fresh owned fence
        // descriptor whose ownership the API transfers to us; wrapping it in `OwnedFd` closes it.
        (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
    };
    let present_fence = if stats.is_null() {
        None
    } else {
        // SAFETY: valid stats for the callback's duration; `-1` means no fence.
        let fd = unsafe { (ctx.present_fence_fn)(stats) };
        // SAFETY: a non-negative fd from `getPresentFenceFd` is a fresh descriptor whose
        // ownership the API transfers to us; wrapping it in `OwnedFd` closes it.
        (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
    };
    let _ = ctx.tx.send(DecodeEvent::PresentComplete(PresentComplete {
        seq: ctx.seq,
        latch_ns,
        prev_release_fence,
        present_fence,
    }));
}

/// One `ASurfaceControl` layer, a child of the SurfaceView's window, that the presenter composites
/// decoded buffers onto. Owns nothing thread-shared; lives on and is dropped by the decode loop.
pub(super) struct Layer {
    api: Api,
    sc: Arc<ScHandle>,
    /// The SurfaceView's LIVE pixel size, packed by `pack_surface_size` and re-read before every
    /// present — the destination rectangle the buffer is scaled to fill. Live rather than captured
    /// because the view resizes under a surface that is never recreated (see `dest`).
    surface_size: Arc<AtomicU64>,
    /// The visible part of the buffer (`pack_src_crop`), re-read before every present.
    src_crop: Arc<AtomicU64>,
    /// Fallback destination for as long as `surface_size` is still `0` (Kotlin hadn't measured the
    /// view when video started): the window's own buffer geometry, the best remaining guess.
    fallback_w: i32,
    fallback_h: i32,
    /// `true` once the first transaction has made the layer visible + set its z-order + frame rate.
    configured: bool,
}

impl Layer {
    /// Create the compositor layer over `window` (the SurfaceView's `ANativeWindow`), or `None` on
    /// API < 29 / a null layer — the caller then uses the SurfaceView presenter.
    ///
    /// `surface_size` carries the SurfaceView's **on-screen pixel size** — the coordinate space the
    /// child layer is composited into, which is the display footprint of the (aspect-fitted) video
    /// view, NOT the window's buffer size. `ANativeWindow_getWidth/Height` return the buffer
    /// geometry in a rotated/scaled space (observed 1260×567 for a 2800×1260 full-bleed stream) —
    /// using it shrank the picture to the top-left corner. It is read fresh on every present
    /// because that view RESIZES mid-stream under a surface that is never recreated: the stream
    /// screen hides the system bars and switches on cutout drawing a frame or two after
    /// `surfaceCreated`, and each one grows it. An empty `surface_size` (Kotlin hadn't measured the
    /// view yet) falls back to the buffer size as the best remaining guess.
    pub(super) fn create(
        window: &NativeWindow,
        surface_size: Arc<AtomicU64>,
        src_crop: Arc<AtomicU64>,
    ) -> Option<Layer> {
        let api = Api::resolve()?;
        // SAFETY: `window.ptr()` is the live `ANativeWindow` the decode thread owns; the name is a
        // static NUL-terminated string; the call returns null on failure (checked).
        let sc =
            unsafe { (api.create_from_window)(window.ptr().as_ptr(), c"punktfunk-video".as_ptr()) };
        if sc.is_null() {
            log::warn!("asc: createFromWindow returned null — falling back to SurfaceView");
            return None;
        }
        let fallback_w = window.width().max(1);
        let fallback_h = window.height().max(1);
        log::info!(
            "asc: layer created, dest {:?} (window buffer {fallback_w}x{fallback_h})",
            crate::session::unpack_surface_size(surface_size.load(Ordering::Relaxed)),
        );
        Some(Layer {
            sc: Arc::new(ScHandle {
                sc,
                release: api.ac_release,
            }),
            api,
            surface_size,
            src_crop,
            fallback_w,
            fallback_h,
            configured: false,
        })
    }

    /// The destination rectangle for this present: the live view size, or the window's buffer
    /// geometry while Kotlin has reported nothing.
    fn dest(&self) -> (i32, i32) {
        crate::session::unpack_surface_size(self.surface_size.load(Ordering::Relaxed))
            .unwrap_or((self.fallback_w, self.fallback_h))
    }

    /// Present one decoded buffer at `desired_present_ns` (`CLOCK_MONOTONIC`; `0` = ASAP).
    /// SurfaceFlinger takes `acquire_fence` only after transaction creation succeeds; otherwise
    /// the caller keeps it to release the unused image safely. The completion reports the latch
    /// and previous-buffer release fence on `ev_tx`, tagged with `seq`.
    ///
    /// `dataspace` is the `ADataSpace` value (`0` leaves the layer default). `hdr` is the
    /// session's HDR10 volume for SurfaceFlinger's tone-mapper. `frame_rate` votes once (`0.0`
    /// skips). `false` means the caller still owns the buffer and fence.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn present(
        &mut self,
        buffer: &HardwareBuffer,
        src_w: i32,
        src_h: i32,
        acquire_fence: &mut Option<OwnedFd>,
        desired_present_ns: i64,
        dataspace: i32,
        hdr: Option<&punktfunk_core::quic::HdrMeta>,
        frame_rate: f32,
        seq: u64,
        ev_tx: &mpsc::Sender<DecodeEvent>,
    ) -> bool {
        // SAFETY: `txn_create` returns a fresh transaction or null; every setter below takes that
        // transaction + this layer's live `sc` + valid arguments; `apply`/`delete` consume it once.
        unsafe {
            let txn = (self.api.txn_create)();
            if txn.is_null() {
                return false;
            }
            let sc = self.sc.sc;
            let fence_fd = acquire_fence
                .take()
                .map(std::os::fd::IntoRawFd::into_raw_fd)
                .unwrap_or(-1);
            (self.api.txn_set_buffer)(txn, sc, buffer.as_ptr(), fence_fd);
            let src = crop_rect(
                crate::session::unpack_src_crop(self.src_crop.load(Ordering::Relaxed)),
                src_w.max(1),
                src_h.max(1),
            );
            let (dest_w, dest_h) = self.dest();
            let dst = ARect {
                left: 0,
                top: 0,
                right: dest_w,
                bottom: dest_h,
            };
            (self.api.txn_set_geometry)(txn, sc, &src, &dst, TRANSFORM_IDENTITY);
            if dataspace != 0 {
                if let Some(f) = self.api.txn_set_dataspace {
                    f(txn, sc, dataspace);
                }
            }
            if let Some(m) = hdr {
                let (mdcv, cll) = hdr_metadata(m);
                if let Some(f) = self.api.txn_set_hdr_smpte2086 {
                    f(txn, sc, &mdcv);
                }
                if let Some(f) = self.api.txn_set_hdr_cta861_3 {
                    f(txn, sc, &cll);
                }
            }
            if !self.configured {
                (self.api.txn_set_visibility)(txn, sc, VISIBILITY_SHOW);
                (self.api.txn_set_z_order)(txn, sc, 0);
                // Declare the layer as fixed-rate video at the source rate (compatibility 1 =
                // FIXED_SOURCE) so a compliant display aligns its refresh to it. Best-effort: an
                // LTPO governor may still run "video" content below its own floor for power (the
                // NP3 does — no app-side rate hint raises its render-range floor; the display's
                // Minimum-refresh-rate system setting is the only lever there).
                if frame_rate > 0.0 {
                    if let Some(f) = self.api.txn_set_frame_rate {
                        f(txn, sc, frame_rate, 1);
                    }
                }
                self.configured = true;
            }
            (self.api.txn_set_present_time)(txn, desired_present_ns);
            // One-shot completion context, reclaimed inside the callback. The `Arc` clone keeps the
            // control alive for the callback even past the layer's own drop.
            let ctx = Box::into_raw(Box::new(CompleteCtx {
                tx: ev_tx.clone(),
                seq,
                sc: self.sc.clone(),
                prev_fence_fn: self.api.stats_prev_release_fence,
                present_fence_fn: self.api.stats_present_fence,
                latch_fn: self.api.stats_latch_time,
            }));
            (self.api.txn_set_on_complete)(txn, ctx as *mut c_void, on_complete);
            (self.api.txn_apply)(txn);
            (self.api.txn_delete)(txn);
        }
        true
    }
}

// ---- Present-fence timestamps (libsync, API 26) -----------------------------------------------

/// `struct sync_file_info` (`linux/sync_file.h`).
#[repr(C)]
struct SyncFileInfo {
    name: [u8; 32],
    status: i32,
    flags: u32,
    num_fences: u32,
    pad: u32,
    sync_fence_info: u64,
}

/// `struct sync_fence_info` (`linux/sync_file.h`).
#[repr(C)]
struct SyncFenceInfo {
    obj_name: [u8; 32],
    driver_name: [u8; 32],
    status: i32,
    flags: u32,
    timestamp_ns: u64,
}

type SyncFileInfoFn = unsafe extern "C" fn(i32) -> *mut SyncFileInfo;
type SyncFileInfoFreeFn = unsafe extern "C" fn(*mut SyncFileInfo);

struct SyncApi {
    info: SyncFileInfoFn,
    free: SyncFileInfoFreeFn,
}

fn sync_api() -> Option<&'static SyncApi> {
    static API: OnceLock<Option<SyncApi>> = OnceLock::new();
    API.get_or_init(|| {
        // SAFETY: `dlopen` of the public `libsync.so` (process-lifetime handle); each `dlsym`
        // is null-checked before the transmute to its documented signature.
        unsafe {
            let lib = libc::dlopen(c"libsync.so".as_ptr(), libc::RTLD_NOW);
            if lib.is_null() {
                return None;
            }
            let info = libc::dlsym(lib, c"sync_file_info".as_ptr());
            let free = libc::dlsym(lib, c"sync_file_info_free".as_ptr());
            if info.is_null() || free.is_null() {
                return None;
            }
            Some(SyncApi {
                info: std::mem::transmute::<*mut c_void, SyncFileInfoFn>(info),
                free: std::mem::transmute::<*mut c_void, SyncFileInfoFreeFn>(free),
            })
        }
    })
    .as_ref()
}

/// The instant `fence` signalled (`CLOCK_MONOTONIC` ns), or `None` while it is still pending, on
/// an invalid fence, or where libsync is missing. Never blocks.
pub(super) fn fence_signal_ns(fence: BorrowedFd<'_>) -> Option<i64> {
    let api = sync_api()?;
    // SAFETY: `sync_file_info` returns a malloc'd record (or null) that `sync_file_info_free`
    // releases; `sync_fence_info` points at `num_fences` records inside that allocation.
    unsafe {
        let info = (api.info)(fence.as_raw_fd());
        if info.is_null() {
            return None;
        }
        let signalled = (*info).status == 1;
        let mut latest = 0u64;
        if signalled {
            let fences = (*info).sync_fence_info as usize as *const SyncFenceInfo;
            for i in 0..(*info).num_fences as usize {
                let f = &*fences.add(i);
                if f.status == 1 {
                    latest = latest.max(f.timestamp_ns);
                }
            }
        }
        (api.free)(info);
        (signalled && latest > 0).then_some(latest as i64)
    }
}

/// The buffer rect for a crop in frame fractions, at least one pixel on each axis.
fn crop_rect([left, top, right, bottom]: [f32; 4], w: i32, h: i32) -> ARect {
    let px = |f: f32, size: i32| ((f * size as f32).round() as i32).clamp(0, size);
    let (l, t) = (px(left, w).min(w - 1), px(top, h).min(h - 1));
    ARect {
        left: l,
        top: t,
        right: px(right, w).max(l + 1),
        bottom: px(bottom, h).max(t + 1),
    }
}
