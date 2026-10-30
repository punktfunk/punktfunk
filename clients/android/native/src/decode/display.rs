//! Display/frame-rendered tracking, render-callback registration, HDR dataspace mapping.

use ndk::data_space::DataSpace;
use ndk::media::media_codec::MediaCodec;
use ndk::native_window::NativeWindow;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::latency::{now_realtime_ns, take_by_pts};
use super::RENDERED_CAP;
use crate::sys::now_monotonic_ns;

/// State shared between the decode loop and the `AMediaCodec` `OnFrameRendered` callback (which
/// fires on a codec-internal thread): rendered frames awaiting their render timestamp, so the HUD
/// gets the spec's `display` stage (decoded→displayed) and the `capture→displayed` end-to-end
/// headline (`design/stats-unification.md` — this replaces Android's v1 `capture→decoded`
/// endpoint whenever the platform delivers render callbacks).
pub(super) struct DisplayTracker {
    stats: Arc<crate::stats::VideoStats>,
    /// Live host-minus-client clock offset (ns) for the skew-corrected end-to-end sample —
    /// loaded per callback so mid-stream re-syncs apply. Holding the handle (not the client)
    /// keeps the leaked render-callback refcount from pinning the whole session alive.
    clock_offset: Arc<AtomicI64>,
    /// Where the AUDIO plane reads the video leg it has to land with (ns) — `displayed +
    /// clock_offset − pts`, published on every confirmed present. Written here, read by
    /// [`crate::audio`]'s sync loop; the two planes never touch each other directly (the presenter
    /// must not know about audio, and the audio thread cannot see the glass).
    ///
    /// Published RAW. The HUD shaves the OS present floor off its shown display / end-to-end
    /// numbers (`StatsOverlay.osFloorMs` — metrics report what Punktfunk controls), but sound has
    /// to reach the ear when the light reaches the eye, and a floor-shaved reference would place
    /// audio a whole latch period early on every device. Presentation policy, not physics.
    video_e2e: Arc<AtomicU64>,
    /// Always-on latch/display accumulator for the presenter's 1 Hz `pf-present` line —
    /// independent of the HUD gate, so a HUD-off A/B stays measurable from logcat.
    meter: Arc<super::sink::PresentMeter>,
    /// `(pts_us, (decoded_real_ns, released_real_ns))` of frames released with `render = true`, in
    /// release order, awaiting their callback. Pushed on EVERY render (no HUD gate — the ring is
    /// a 64-tuple bound and the latch metric wants to exist when nobody is watching).
    rendered: Mutex<VecDeque<(u64, (i128, i128))>>,
}

impl DisplayTracker {
    pub(super) fn new(
        stats: Arc<crate::stats::VideoStats>,
        clock_offset: Arc<AtomicI64>,
        video_e2e: Arc<AtomicU64>,
        meter: Arc<super::sink::PresentMeter>,
    ) -> Arc<DisplayTracker> {
        Arc::new(DisplayTracker {
            stats,
            clock_offset,
            video_e2e,
            meter,
            rendered: Mutex::new(VecDeque::new()),
        })
    }

    /// Park one just-rendered frame's `(pts, decoded stamp, release stamp)` for the render
    /// callback to pair — the release stamp is the latch metric's start (release→displayed).
    pub(super) fn note_rendered(&self, pts_us: u64, decoded_ns: i128, released_ns: i128) {
        let mut g = self
            .rendered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.push_back((pts_us, (decoded_ns, released_ns)));
        if g.len() > RENDERED_CAP {
            g.pop_front(); // render callbacks stopped coming (allowed under load) — evict
        }
    }
}

/// Register [`on_frame_rendered`] on the codec (`AMediaCodec_setOnFrameRenderedCallback`,
/// **API 33** — "Available since Android T" per the NDK header; only the *Java* listener dates
/// back further). That sits above the API-28 floor, so the entry point is dlsym-resolved at
/// runtime like [`try_set_frame_rate`] — hard-linking it (as 0.9.0 shipped) made
/// `System.loadLibrary` fail on every pre-Android-13 device, taking down all of `NativeBridge`.
/// The `ndk` wrapper has no binding and the call needs the raw codec pointer, which is what the
/// vendored crate's public `as_ptr` patch is for. Returns the userdata pointer holding a leaked
/// `Arc<DisplayTracker>` refcount; the caller MUST reclaim it with [`release_render_callback`]
/// AFTER dropping the codec (`AMediaCodec_delete` is what guarantees no further callback can
/// fire). `None` (nothing to reclaim) if the symbol is absent (API < 33) or the platform refused —
/// the HUD then simply has no `display` stage, exactly the pre-callback behaviour.
pub(super) fn install_render_callback(
    codec: &MediaCodec,
    tracker: &Arc<DisplayTracker>,
) -> Option<*const DisplayTracker> {
    // media_status_t AMediaCodec_setOnFrameRenderedCallback(
    //     AMediaCodec*, AMediaCodecOnFrameRendered, void*)                            (API 33)
    type SetOnFrameRenderedFn = unsafe extern "C" fn(
        *mut ndk_sys::AMediaCodec,
        ndk_sys::AMediaCodecOnFrameRendered,
        *mut c_void,
    ) -> ndk_sys::media_status_t;
    // SAFETY: `dlopen` of `libmediandk.so`, which the `ndk` media wrapper already links — always
    // mapped, so this only bumps its refcount (never closed — process-lifetime handle; null is
    // checked). The `sym` type is the NDK header's signature; absent = API < 33.
    let set_on_frame_rendered = unsafe {
        let lib = libc::dlopen(c"libmediandk.so".as_ptr(), libc::RTLD_NOW);
        if lib.is_null() {
            return None;
        }
        let Some(set) =
            crate::sym::<SetOnFrameRenderedFn>(lib, c"AMediaCodec_setOnFrameRenderedCallback")
        else {
            // No confirmed present ⇒ no `display` stage AND no reference for the audio plane's A/V
            // sync, which then stays inert and leaves the ring exactly as it was. The release
            // instant is NOT substituted: releases target a future vsync, so it runs a whole latch
            // period (8-21 ms measured) ahead of glass — well outside the loop's deadband, i.e. it
            // would place audio early on every frame while looking like it was working.
            log::info!(
                "decode: no render callback on this API level (<33) — no display stage, no A/V sync"
            );
            return None;
        };
        set
    };
    let ud = Arc::into_raw(tracker.clone());
    // SAFETY: `codec.as_ptr()` is the live codec this thread owns; `ud` outlives the registration
    // (reclaimed only after the codec is deleted, per this function's contract).
    let status = unsafe {
        set_on_frame_rendered(codec.as_ptr(), Some(on_frame_rendered), ud as *mut c_void)
    };
    if status == ndk_sys::media_status_t::AMEDIA_OK {
        Some(ud)
    } else {
        log::warn!("decode: setOnFrameRenderedCallback failed ({status:?}) — no display stage");
        // SAFETY: registration failed, so the codec never took the reference — reclaim it now.
        unsafe { drop(Arc::from_raw(ud)) };
        None
    }
}

/// Reclaim [`install_render_callback`]'s leaked `Arc` refcount.
///
/// # Safety
/// Call exactly once, and only after the codec the callback was registered on has been dropped —
/// deleting the codec stops its internal threads, so no callback can still be running (or run
/// later) against this pointer.
pub(super) unsafe fn release_render_callback(ud: *const DisplayTracker) {
    // SAFETY: `ud` is the pointer `install_render_callback` leaked from `Arc::into_raw`, and this
    // function's contract is that it is reclaimed exactly once, after the codec is gone.
    unsafe { drop(Arc::from_raw(ud)) };
}

/// The `AMediaCodecOnFrameRendered` trampoline: fires (possibly batched) on a codec-internal
/// thread once per output frame actually placed on the output surface, with SurfaceFlinger's
/// render timestamp. That timestamp (`system_nano`) is on `CLOCK_MONOTONIC`, so it is re-based
/// onto `CLOCK_REALTIME` here — against monotonic-now at callback time, which also cancels any lag
/// between the frame rendering and the (batchable) callback delivery — to subtract against the
/// receipt/decode stamps and the host capture pts. Records the HUD's `displayed` point:
/// `end-to-end` = capture→displayed (skew-corrected) and `display` = decoded→displayed
/// (single-clock local) — and publishes that end-to-end figure for the audio plane to align
/// against ([`crate::stats::publish_e2e`]), HUD or not.
/// Panic-free by construction (poison-proof lock, saturating math) — an unwind out of an
/// `extern "C"` fn would abort the process.
unsafe extern "C" fn on_frame_rendered(
    _codec: *mut ndk_sys::AMediaCodec,
    userdata: *mut c_void,
    media_time_us: i64,
    system_nano: i64,
) {
    // SAFETY: the platform hands back exactly the `userdata` registered with the callback — the
    // `Arc::into_raw` pointer from `install_render_callback`, whose refcount is held for as long as
    // the codec exists, and the codec is what delivers this call.
    let t = unsafe { &*(userdata as *const DisplayTracker) };
    let displayed_ns =
        now_realtime_ns() - (i128::from(now_monotonic_ns()) - i128::from(system_nano));
    let pts_us = media_time_us.max(0) as u64;
    // Pair the frame back to its release record; older entries' callbacks were dropped.
    let paired = take_by_pts(
        &mut t
            .rendered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        pts_us,
    );
    let pts_ns = pts_us.saturating_mul(1000);
    // Always-on half: the presenter's pf-present line reads these with the HUD off.
    t.meter
        .note_latch(paired.and_then(|(_, r)| crate::stats::sane_us(displayed_ns - r)));
    // Above the HUD gate: the audio plane steers by `video_e2e`. Only the stats lock waits for it.
    let offset = t.clock_offset.load(Ordering::Relaxed);
    crate::stats::publish_e2e(&t.video_e2e, displayed_ns, offset, pts_ns);
    if !t.stats.enabled() {
        return;
    }
    let (decoded_ns, released_ns) = paired.unwrap_or((0, 0));
    t.stats
        .note_displayed(pts_ns, decoded_ns, released_ns, displayed_ns);
}

/// Tag the SurfaceView window with `ds` unless `applied_ds` already holds it: the negotiated
/// colour at bring-up and after a format change the decoder reports no transfer for, the
/// decoder's own report otherwise. `source` names which, for the log line. A refusal is
/// non-fatal: the window keeps what it had.
pub(super) fn apply_dataspace(
    window: &NativeWindow,
    ds: DataSpace,
    applied_ds: &mut Option<DataSpace>,
    source: &str,
) {
    if *applied_ds == Some(ds) {
        return;
    }
    match window.set_buffers_data_space(ds) {
        Ok(()) => {
            *applied_ds = Some(ds);
            log::info!("decode: Surface dataspace {ds} ({source})");
        }
        Err(e) => log::warn!("decode: set_buffers_data_space({ds}) failed (non-fatal): {e}"),
    }
}

/// The decoder's picture size: the crop rect when the output format carries one (1080 rows decode
/// into a 1088-row buffer), else its width × height. `None` before a format is known.
///
/// The NDK format holds the crop as one inclusive rect under `crop`. Only Java's `MediaFormat`
/// splits it into `crop-left`…`crop-bottom`, so those keys never exist here.
pub(super) fn picture_size(codec: &MediaCodec) -> Option<(i32, i32)> {
    let fmt = codec.output_format();
    let (w, h) = (fmt.i32("width")?, fmt.i32("height")?);
    Some(match fmt.rect("crop") {
        Some((l, t, r, b)) if r >= l && b >= t => (r - l + 1, b - t + 1),
        _ => (w, h),
    })
}

/// The dataspace a format change leaves: the stream's own colour, else the decoder's report.
pub(super) fn stream_dataspace(
    codec: &MediaCodec,
    stream: Option<punktfunk_core::quic::ColorInfo>,
) -> Option<DataSpace> {
    stream
        .map(|c| color_dataspace(&c))
        .or_else(|| reported_dataspace(codec))
}

/// Map the decoder's reported output colour to a dataspace: BT.2020 PQ or HLG, BT.709 for SDR
/// (sRGB when full range), `None` without a transfer. Codec2 fills a decoder that reports nothing
/// with its BT.709 default, so SDR here does not prove the stream is SDR. The integer values are
/// the Android MediaFormat colour constants the NDK shares: COLOR_TRANSFER SDR_VIDEO = 3,
/// ST2084 = 6 (PQ/HDR10), HLG = 7; COLOR_RANGE FULL = 1, LIMITED = 2.
fn reported_dataspace(codec: &MediaCodec) -> Option<DataSpace> {
    let fmt = codec.output_format();
    let full_range = fmt.i32("color-range") == Some(1);
    match fmt.i32("color-transfer") {
        Some(6) => Some(if full_range {
            DataSpace::Bt2020Pq
        } else {
            DataSpace::Bt2020ItuPq
        }),
        Some(7) => Some(if full_range {
            DataSpace::Bt2020Hlg
        } else {
            DataSpace::Bt2020ItuHlg
        }),
        Some(3) => Some(if full_range {
            DataSpace::Srgb
        } else {
            DataSpace::Bt709
        }),
        _ => None, // unspecified
    }
}

/// The `ADataSpace` the presenters tag buffers with, from the *negotiated* colour
/// ([`ColorInfo`], carried on Welcome): the wire contract's source, not what the decoder echoes
/// (many omit `color-transfer`). Never `0`: an untagged ASurfaceControl layer defaults to sRGB,
/// which shows studio-range black (16) as grey.
///
/// Full-range SDR is tagged sRGB: a desktop's pixels are sRGB-encoded, and SurfaceFlinger
/// treats BT.709's transfer the same way. Studio-range SDR (an older host) is BT709.
pub(super) fn color_dataspace(color: &punktfunk_core::quic::ColorInfo) -> DataSpace {
    use punktfunk_core::quic::ColorInfo;
    let full = color.full_range != 0;
    match (color.transfer, full) {
        (ColorInfo::TRC_PQ, true) => DataSpace::Bt2020Pq,
        (ColorInfo::TRC_PQ, false) => DataSpace::Bt2020ItuPq,
        (ColorInfo::TRC_HLG, true) => DataSpace::Bt2020Hlg,
        (ColorInfo::TRC_HLG, false) => DataSpace::Bt2020ItuHlg,
        (_, true) => DataSpace::Srgb,
        (_, false) => DataSpace::Bt709,
    }
}

/// [`color_dataspace`] as an ASurfaceControl layer on `sdk` takes it. Android 10 aborts the
/// process and 11 ignores anything but sRGB, scRGB, P3 and full-range PQ, so `None` there sends
/// a studio-range session to the SurfaceView, whose decoder tags its own buffers.
pub(super) fn layer_dataspace(color: &punktfunk_core::quic::ColorInfo, sdk: u32) -> Option<i32> {
    let ds = color_dataspace(color);
    (sdk >= 31 || matches!(ds, DataSpace::Srgb | DataSpace::Bt2020Pq)).then(|| i32::from(ds))
}
