//! The `ASurfaceControl` compositor layer behind the ASurfaceControl presenter backend.
//!
//! This is the Android analogue of what the Apple client gets from `CAMetalDisplayLink` +
//! `preferredFrameLatency = 1`: a present path that schedules each frame against the panel's own
//! timeline and hands back the *real* present feedback, instead of the MediaCodec→SurfaceView→
//! BufferQueue path that predicts the latch and hopes the `OnFrameRendered` callbacks arrive.
//!
//! A `Layer` owns one `ASurfaceControl` created as a child of a SurfaceView's `ANativeWindow`;
//! the decoder renders into an `AImageReader` and the presenter composites each acquired
//! `AHardwareBuffer` onto every shown layer in one `ASurfaceTransaction` ([`present`]) that
//! carries a desired present time (the single actuator both present modes drive) and an acquire
//! fence. A dual-screen handheld's second window is a second layer taking the same buffer with
//! its own crop (design `android-dual-screen.md` §4). Every applied transaction registers a
//! one-shot completion callback that reports the frame's latch time, its present fence and the
//! *previous* buffer's release fence — one per layer, merged — back through the decode loop's
//! event channel — the real vsync the slot scheduler is phased on.
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
/// `ASURFACE_TRANSACTION_VISIBILITY_HIDE`.
const VISIBILITY_HIDE: i8 = 0;

/// [`HdrMeta`](punktfunk_core::quic::HdrMeta) (ST.2086 G, B, R in 1/50000; mastering luminance in
/// 0.0001 nits) as the NDK's float structs. A block with a zero field means "unknown" and is
/// `None`, as Codec2 treats it for the decoder's own Surface: SurfaceFlinger takes a sent MaxCLL
/// as the layer's peak, so a zero one declares a 0-nit picture.
fn hdr_metadata(
    m: &punktfunk_core::quic::HdrMeta,
) -> (Option<AHdrMetadataSmpte2086>, Option<AHdrMetadataCta8613>) {
    let xy = |[x, y]: [u16; 2]| AColorXy {
        x: f32::from(x) / 50_000.0,
        y: f32::from(y) / 50_000.0,
    };
    let [g, b, r] = m.display_primaries;
    let mastering = (m.max_display_mastering_luminance > 0
        && m.min_display_mastering_luminance > 0)
        .then(|| AHdrMetadataSmpte2086 {
            red: xy(r),
            green: xy(g),
            blue: xy(b),
            white: xy(m.white_point),
            max_luminance: m.max_display_mastering_luminance as f32 / 10_000.0,
            min_luminance: m.min_display_mastering_luminance as f32 / 10_000.0,
        });
    let light_level = (m.max_cll > 0 && m.max_fall > 0).then(|| AHdrMetadataCta8613 {
        max_content_light_level: f32::from(m.max_cll),
        max_frame_average_light_level: f32::from(m.max_fall),
    });
    (mastering, light_level)
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

#[derive(Clone, Copy)]
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
        // closed — a process-lifetime handle; null is checked). Each `sym` type is the NDK header's
        // signature for that name; a required one absent = API < 29.
        unsafe {
            let lib = libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW);
            if lib.is_null() {
                return None;
            }
            use crate::sym;
            Some(Api {
                create_from_window: sym(lib, c"ASurfaceControl_createFromWindow")?,
                ac_release: sym(lib, c"ASurfaceControl_release")?,
                txn_create: sym(lib, c"ASurfaceTransaction_create")?,
                txn_delete: sym(lib, c"ASurfaceTransaction_delete")?,
                txn_apply: sym(lib, c"ASurfaceTransaction_apply")?,
                txn_set_buffer: sym(lib, c"ASurfaceTransaction_setBuffer")?,
                txn_set_visibility: sym(lib, c"ASurfaceTransaction_setVisibility")?,
                txn_set_z_order: sym(lib, c"ASurfaceTransaction_setZOrder")?,
                txn_set_geometry: sym(lib, c"ASurfaceTransaction_setGeometry")?,
                txn_set_present_time: sym(lib, c"ASurfaceTransaction_setDesiredPresentTime")?,
                txn_set_dataspace: sym(lib, c"ASurfaceTransaction_setBufferDataSpace"),
                txn_set_frame_rate: sym(lib, c"ASurfaceTransaction_setFrameRate"),
                txn_set_hdr_smpte2086: sym(lib, c"ASurfaceTransaction_setHdrMetadata_smpte2086"),
                txn_set_hdr_cta861_3: sym(lib, c"ASurfaceTransaction_setHdrMetadata_cta861_3"),
                txn_set_on_complete: sym(lib, c"ASurfaceTransaction_setOnComplete")?,
                stats_latch_time: sym(lib, c"ASurfaceTransactionStats_getLatchTime")?,
                stats_prev_release_fence: sym(
                    lib,
                    c"ASurfaceTransactionStats_getPreviousReleaseFenceFd",
                )?,
                stats_present_fence: sym(lib, c"ASurfaceTransactionStats_getPresentFenceFd")?,
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
    /// layers, every layer's fence merged into one), or `None` when the platform reports none.
    /// The loop deletes that buffer's image with this fence so it is returned to the reader's
    /// pool only once SurfaceFlinger is done with it on every display.
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
    /// Shared references to every layer's `ASurfaceControl` in the transaction, needed to read the
    /// per-surface release fences out of the stats. Holding the `Arc`s keeps each control alive
    /// for the callback even if its layer was already dropped.
    scs: Vec<Arc<ScHandle>>,
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
    // One fence per layer, merged: the buffer is free only once every display is done with it.
    let mut prev_release_fence: Option<OwnedFd> = None;
    if !stats.is_null() {
        for sc in &ctx.scs {
            // SAFETY: valid stats + a live `ASurfaceControl` (the `Arc` holds it); a returned fd
            // is owned by us and closed via `OwnedFd`. `-1` means no fence.
            let fd = unsafe { (ctx.prev_fence_fn)(stats, sc.sc) };
            // SAFETY: a non-negative fd returned by `getPreviousReleaseFenceFd` is a fresh owned
            // fence descriptor whose ownership the API transfers to us; `OwnedFd` closes it.
            let Some(fence) = (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) }) else {
                continue;
            };
            prev_release_fence = Some(match prev_release_fence.take() {
                None => fence,
                Some(prior) => merge_fences(prior, fence),
            });
        }
    }
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

/// One `ASurfaceControl` layer, a child of a SurfaceView's window, that the presenter composites
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

    /// Put `buffer` on this layer in `txn`: the crop, the destination, the colour, and on the
    /// first stage after a show the visibility, z-order and frame-rate vote. `fence_fd` is the
    /// acquire fence `setBuffer` takes ownership of (`-1` = none).
    ///
    /// # Safety
    /// `txn` is a live transaction this call's caller applies or deletes exactly once.
    #[allow(clippy::too_many_arguments)]
    unsafe fn stage(
        &mut self,
        txn: *mut ASurfaceTransaction,
        buffer: &HardwareBuffer,
        src_w: i32,
        src_h: i32,
        fence_fd: RawFd,
        dataspace: i32,
        hdr: Option<&punktfunk_core::quic::HdrMeta>,
        frame_rate: f32,
    ) {
        // SAFETY: every setter takes the caller's live transaction + this layer's live `sc` +
        // valid arguments.
        unsafe {
            let sc = self.sc.sc;
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
                // Each setter replaces the layer's whole HDR metadata, so send one block: the
                // content light level when known, else the mastering volume.
                let (mdcv, cll) = hdr_metadata(m);
                if let (Some(f), Some(cll)) = (self.api.txn_set_hdr_cta861_3, cll) {
                    f(txn, sc, &cll);
                } else if let (Some(f), Some(mdcv)) = (self.api.txn_set_hdr_smpte2086, mdcv) {
                    f(txn, sc, &mdcv);
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
        }
    }

    /// Take the layer off the screen; the next [`present`] that includes it shows it again.
    /// Dropping it does not hide it: a released child stays on display as long as its parent
    /// does, over whatever layer replaced it.
    pub(super) fn hide(&mut self) {
        // SAFETY: a fresh transaction or null, this layer's live `sc`, applied and deleted once.
        unsafe {
            let txn = (self.api.txn_create)();
            if txn.is_null() {
                return;
            }
            (self.api.txn_set_visibility)(txn, self.sc.sc, VISIBILITY_HIDE);
            (self.api.txn_apply)(txn);
            (self.api.txn_delete)(txn);
        }
        self.configured = false;
    }
}

/// Present one decoded buffer on every layer in `layers` at `desired_present_ns`
/// (`CLOCK_MONOTONIC`; `0` = ASAP), one transaction. SurfaceFlinger takes `acquire_fence` only
/// after transaction creation succeeds; otherwise the caller keeps it to release the unused image
/// safely. Each layer past the first gets its own duplicate of the fence, since `setBuffer` owns
/// the descriptor it is handed. The completion reports the latch and the merged previous-buffer
/// release fence on `ev_tx`, tagged with `seq`.
///
/// `dataspace` is the `ADataSpace` value (`0` leaves the layer default). `hdr` is the session's
/// HDR10 volume for SurfaceFlinger's tone-mapper. `frame_rate` votes once per layer (`0.0`
/// skips). `false` means the caller still owns the buffer and fence.
#[allow(clippy::too_many_arguments)]
pub(super) fn present(
    layers: &mut [&mut Layer],
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
    let Some(api) = layers.first().map(|l| l.api) else {
        return false;
    };
    // SAFETY: `txn_create` returns a fresh transaction or null; `stage` and the setters below take
    // that transaction + live layers + valid arguments; `apply`/`delete` consume it once.
    unsafe {
        let txn = (api.txn_create)();
        if txn.is_null() {
            return false;
        }
        let dups: Vec<RawFd> = (1..layers.len())
            .map(|_| {
                acquire_fence
                    .as_ref()
                    .and_then(|f| f.try_clone().ok())
                    .map_or(-1, std::os::fd::IntoRawFd::into_raw_fd)
            })
            .collect();
        let first_fd = acquire_fence
            .take()
            .map_or(-1, std::os::fd::IntoRawFd::into_raw_fd);
        for (i, layer) in layers.iter_mut().enumerate() {
            let fd = if i == 0 { first_fd } else { dups[i - 1] };
            layer.stage(txn, buffer, src_w, src_h, fd, dataspace, hdr, frame_rate);
        }
        (api.txn_set_present_time)(txn, desired_present_ns);
        // One-shot completion context, reclaimed inside the callback. The `Arc` clones keep the
        // controls alive for the callback even past a layer's own drop.
        let ctx = Box::into_raw(Box::new(CompleteCtx {
            tx: ev_tx.clone(),
            seq,
            scs: layers.iter().map(|l| l.sc.clone()).collect(),
            prev_fence_fn: api.stats_prev_release_fence,
            present_fence_fn: api.stats_present_fence,
            latch_fn: api.stats_latch_time,
        }));
        (api.txn_set_on_complete)(txn, ctx as *mut c_void, on_complete);
        (api.txn_apply)(txn);
        (api.txn_delete)(txn);
    }
    true
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
type SyncMergeFn = unsafe extern "C" fn(*const std::ffi::c_char, i32, i32) -> i32;

struct SyncApi {
    info: SyncFileInfoFn,
    free: SyncFileInfoFreeFn,
    merge: SyncMergeFn,
}

fn sync_api() -> Option<&'static SyncApi> {
    static API: OnceLock<Option<SyncApi>> = OnceLock::new();
    API.get_or_init(|| {
        // SAFETY: `dlopen` of the public `libsync.so` (process-lifetime handle; null is checked);
        // each `sym` type is libsync's documented signature for that name.
        unsafe {
            let lib = libc::dlopen(c"libsync.so".as_ptr(), libc::RTLD_NOW);
            if lib.is_null() {
                return None;
            }
            Some(SyncApi {
                info: crate::sym(lib, c"sync_file_info")?,
                free: crate::sym(lib, c"sync_file_info_free")?,
                merge: crate::sym(lib, c"sync_merge")?,
            })
        }
    })
    .as_ref()
}

/// One fence that signals once both do. Without libsync the first stands in for both: a buffer
/// could then return to the pool while the other display still reads it — a tear at worst,
/// never a fault (SurfaceFlinger holds its own reference).
fn merge_fences(a: OwnedFd, b: OwnedFd) -> OwnedFd {
    let Some(api) = sync_api() else {
        return a;
    };
    // SAFETY: two valid fence fds we own; `sync_merge` returns a new fd (or `-1`) and leaves both
    // inputs ours to close, which the `OwnedFd` drops do.
    let fd = unsafe { (api.merge)(c"punktfunk-release".as_ptr(), a.as_raw_fd(), b.as_raw_fd()) };
    if fd < 0 {
        return a;
    }
    // SAFETY: a non-negative `sync_merge` result is a fresh fd we own.
    unsafe { OwnedFd::from_raw_fd(fd) }
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
