//! Session lifecycle and media-plane JNI wiring.
//!
//! Kotlin receives an opaque integer key, never a Rust pointer. A process-local table stores
//! `Arc<SessionHandle>` values so close-vs-call races retain the session until each active JNI call
//! finishes; duplicate or stale closes become no-ops.
//!
//! [`connect`] owns identity, trust, connect, and close. [`planes`] owns video/audio/mic lifecycles,
//! [`input`] forwards control events, and [`probe`] runs bandwidth measurements. Decode and audio
//! workers share the `Sync` connector while the table controls the outer session lifetime.

mod access;
mod clipboard;
mod connect;
mod input;
mod planes;
mod probe;

use punktfunk_core::client::NativeClient;
use punktfunk_core::fp::{hex, parse_hex32};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

/// Run a JNI body, catching any panic at the FFI boundary and returning `default` instead.
///
/// A panic unwinding out of an `extern "system"` function aborts the whole process on Rust ≥ 1.81 —
/// a hard crash of the embedding Android app with no logcat trace. Every entry point that does not
/// go through `EnvUnowned::with_env` (which carries its own catch) wraps its body in this; the
/// `panic = "unwind"` profile in the workspace `Cargo.toml` exists precisely so these guards work.
pub(crate) fn jni_guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        log::error!("punktfunk JNI: caught a panic at the FFI boundary (returning default)");
        default
    })
}

/// Poison-recovering lock for the JNI entry points that are NOT behind [`jni_guard`]: a
/// `.lock().unwrap()` there turns a poisoned mutex into a panic across the `extern "system"`
/// boundary — an abort of the whole app on Rust ≥ 1.81 (the panic-in-extern grep gate's class).
/// The slots behind these mutexes are plane-thread handles and last-value caches; whatever a
/// poisoned writer left is still valid to inspect or replace.
pub(crate) fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One table-owned live session: its connector, media workers, and session-scoped state.
pub(crate) struct SessionHandle {
    // Read only by the android decode path (`nativeStartVideo` → `crate::decode`); on the host
    // build (CI's workspace clippy/build) those readers are cfg'd out, so it's intentionally unused.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub client: Arc<NativeClient>,
    /// The overlay's Android facts plus a handle on the connector's window, read ~1 Hz by
    /// `nativeVideoStatsLines`. Session-lifetime (not per `VideoThread`) so the decoder label
    /// survives surface teardown and recreate.
    pub stats: Arc<crate::stats::VideoStats>,
    video: Mutex<Option<VideoThread>>,
    /// Set by the first `nativeStartVideo`. A later one (a recreated surface) builds a decoder
    /// mid-stream, which must wait for a keyframe and ask for one.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub(crate) video_started: std::sync::atomic::AtomicBool,
    /// The background keep-alive's AU drain: the decode thread is down (its Surface is gone) but
    /// something must still pop the frame queue, or the standing-queue detector jumps to live and
    /// asks the host for a keyframe every `FLUSH_COOLDOWN` for the whole background stay. Reuses
    /// [`VideoThread`] because it is the same contract — a flag and a join.
    drain: Mutex<Option<VideoThread>>,
    #[cfg(target_os = "android")]
    audio: Mutex<Option<crate::audio::AudioPlayback>>,
    #[cfg(target_os = "android")]
    mic: Mutex<Option<crate::mic::MicCapture>>,
    /// Tier-A DualSense pad audio (the 0xD1 plane), started by `nativeStartPadAudio` once Kotlin
    /// has claimed the pad's audio interface and handed its descriptor over. Session-lifetime and
    /// `Option` because a session may have no wired DualSense at all, which is the common case.
    #[cfg(target_os = "android")]
    pub(crate) pad_audio: Mutex<Option<crate::pad_audio::PadAudio>>,
    /// In-stream mic mute, set via `nativeSetMicMuted` and read per 10 ms frame by the mic's
    /// encode loop ([`crate::mic`]). Session-lifetime rather than per-[`crate::mic::MicCapture`]
    /// for the same reason the stats gate is: the mic stops and restarts across a surface
    /// recreate, and a mute the user set must come back with it — with no window in which the
    /// fresh capture could send an unmuted frame. Per session and never persisted: a new session
    /// starts unmuted.
    pub mic_muted: Arc<AtomicBool>,
    /// Count of `AccessUpdate`s drained from the connector's event plane, bumped by the
    /// `nativeAccessState` poll ([`access`]) — how the Kotlin poller tells a fresh update
    /// (the host's expiry warnings) arrived without holding a blocking event thread.
    pub(crate) access_seq: AtomicU32,
    /// The video `SurfaceView`'s LIVE on-screen pixel size ([`pack_surface_size`]), written by
    /// `nativeStartVideo` and by every `nativeVideoSurfaceSize` the `surfaceChanged` callback
    /// sends, read by the ASurfaceControl presenter before each present.
    ///
    /// Shared and live rather than a start-time parameter because the view RESIZES under a surface
    /// that is never recreated: hiding the system bars and switching the window to
    /// `LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS` both happen a frame or two AFTER `surfaceCreated`,
    /// and each one grows the video view. A destination rect captured once at creation then keeps
    /// compositing the picture at its old, smaller size anchored at the layer's origin — the
    /// "stream in the top-left corner" field report. `0` = nothing reported yet, and the layer
    /// falls back to the window's buffer geometry.
    pub surface_size: Arc<AtomicU64>,
    /// The visible part of the frame ([`pack_src_crop`]), written by `nativeVideoSourceCrop`
    /// whenever Kotlin re-places the picture. Not the full frame only under Crop to fill (or a
    /// few-pixel snap); `0` = the full frame.
    pub src_crop: Arc<AtomicU64>,
    /// The decoder's picture size ([`pack_surface_size`]), written on each output-format change.
    /// A host that frames the picture for this device (a join, a mirrored head) can send a size
    /// other than the negotiated mode, and the UI places against what is decoded. `0` = none yet.
    pub decoded_size: Arc<AtomicU64>,
    /// A dual-screen handheld's second picture window and which layers show (design
    /// `android-dual-screen.md` §4), read by the ASurfaceControl presenter.
    pub layers: Arc<PictureLayers>,
}

/// The ASurfaceControl presenter's second layer: a `SurfaceView` on the lower screen that takes
/// the same decoded buffer with its own crop and size, and the mask of layers that show. Kotlin
/// writes it from the UI thread; the decode thread reads it before every present.
#[derive(Default)]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) struct PictureLayers {
    /// The second window, or `None`. `generation` steps on every change, so the decode thread
    /// rebuilds its layer on the new window.
    #[cfg(target_os = "android")]
    pub second_window: Mutex<Option<ndk::native_window::NativeWindow>>,
    pub generation: AtomicU64,
    /// The second `SurfaceView`'s live size and the part of the frame it shows, packed as the
    /// primary's (`pack_surface_size`, `pack_src_crop`).
    pub second_size: Arc<AtomicU64>,
    pub second_crop: Arc<AtomicU64>,
    /// Bit 0 the primary layer, bit 1 the second; a clear bit hides the layer. `0` reads as the
    /// primary alone.
    pub shown: AtomicU8,
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
impl PictureLayers {
    pub(crate) const PRIMARY: u8 = 1;
    pub(crate) const SECOND: u8 = 2;

    pub(crate) fn new() -> PictureLayers {
        let l = PictureLayers::default();
        l.shown.store(Self::PRIMARY, Ordering::Relaxed);
        l
    }
}

/// A process-local table behind the opaque `jlong` keys Kotlin holds. A lookup hands out an
/// `Arc`, so a call racing a close keeps its value until it returns; a stale key finds nothing.
/// Each table seeds its own high prefix, so a key from one table never resolves in another.
/// `0` is never issued and never found.
pub(crate) struct HandleTable<T> {
    next: AtomicU64,
    map: OnceLock<Mutex<HashMap<i64, Arc<T>>>>,
}

impl<T> HandleTable<T> {
    pub(crate) const fn new(seed: u64) -> Self {
        HandleTable {
            next: AtomicU64::new(seed),
            map: OnceLock::new(),
        }
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Arc<T>>> {
        lock_recover(self.map.get_or_init(Default::default))
    }

    pub(crate) fn insert(&self, value: T) -> i64 {
        let value = Arc::new(value);
        let mut map = self.map();
        loop {
            let handle = self.next.fetch_add(1, Ordering::Relaxed) as i64;
            if handle != 0 && !map.contains_key(&handle) {
                map.insert(handle, value);
                return handle;
            }
        }
    }

    pub(crate) fn get(&self, handle: i64) -> Option<Arc<T>> {
        if handle == 0 {
            return None;
        }
        self.map().get(&handle).cloned()
    }

    pub(crate) fn remove(&self, handle: i64) -> Option<Arc<T>> {
        if handle == 0 {
            return None;
        }
        self.map().remove(&handle)
    }
}

pub(crate) static SESSIONS: HandleTable<SessionHandle> = HandleTable::new(0x1000_0000_0000_0001);

/// Pack a surface's pixel size into one `u64` — so the presenter reads width and height as a
/// single atomic load and can never see a torn pair (a new width against an old height).
/// Non-positive values pack as `0`, the "not reported yet" sentinel.
pub(crate) fn pack_surface_size(w: i32, h: i32) -> u64 {
    if w <= 0 || h <= 0 {
        return 0;
    }
    ((w as u64) << 32) | (h as u64 & 0xffff_ffff)
}

/// The inverse of [`pack_surface_size`]: `None` for the `0` sentinel.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn unpack_surface_size(packed: u64) -> Option<(i32, i32)> {
    if packed == 0 {
        return None;
    }
    Some((((packed >> 32) as u32) as i32, (packed as u32) as i32))
}

/// Pack a source crop (frame fractions, left/top/right/bottom) into one `u64`, 16 bits each, so
/// a presenter reads all four as one atomic load. An empty or out-of-range rect packs as `0`,
/// the full frame.
pub(crate) fn pack_src_crop(left: f32, top: f32, right: f32, bottom: f32) -> u64 {
    let ok = |v: f32| (0.0..=1.0).contains(&v);
    if !(ok(left) && ok(top) && ok(right) && ok(bottom)) || right <= left || bottom <= top {
        return 0;
    }
    let q = |v: f32| u64::from((v * 65535.0).round() as u16);
    (q(left) << 48) | (q(top) << 32) | (q(right) << 16) | q(bottom)
}

/// The inverse of [`pack_src_crop`]: `[left, top, right, bottom]`, the full frame for `0`.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn unpack_src_crop(packed: u64) -> [f32; 4] {
    if packed == 0 {
        return [0.0, 0.0, 1.0, 1.0];
    }
    let f = |shift: u32| f32::from((packed >> shift) as u16) / 65535.0;
    [f(48), f(32), f(16), f(0)]
}

struct VideoThread {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl SessionHandle {
    /// Stop and join the decode thread once, recovering a poisoned slot during teardown.
    fn stop_video(&self) {
        if let Some(mut vt) = lock_recover(&self.video).take() {
            vt.shutdown.store(true, Ordering::SeqCst);
            if let Some(j) = vt.join.take() {
                let _ = j.join();
            }
        }
    }

    /// Run the keep-alive AU drain, replacing any already running. Pops video AUs and discards
    /// them so the queue never stands: no jump-to-live, no keyframe cadence, and the host keeps
    /// pacing against a client that is still reading.
    pub(crate) fn start_drain(&self) {
        self.stop_drain();
        let client = self.client.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        let spawned = std::thread::Builder::new()
            .name("pf-video-drain".into())
            .spawn(move || {
                while !sd.load(Ordering::Relaxed) {
                    // `NoFrame` is the 5 ms timeout — re-check the flag and poll again. Anything
                    // else is a closed session, and there is nothing left to drain.
                    match client.next_frame(std::time::Duration::from_millis(5)) {
                        Ok(_) => {}
                        Err(punktfunk_core::PunktfunkError::NoFrame) => {}
                        Err(_) => break,
                    }
                }
            });
        match spawned {
            Ok(join) => {
                *lock_recover(&self.drain) = Some(VideoThread {
                    shutdown,
                    join: Some(join),
                });
            }
            Err(e) => log::error!("video drain thread spawn failed: {e}"),
        }
    }

    /// Stop and join the keep-alive drain once. No-op when none runs.
    pub(crate) fn stop_drain(&self) {
        if let Some(mut dt) = lock_recover(&self.drain).take() {
            dt.shutdown.store(true, Ordering::SeqCst);
            if let Some(j) = dt.join.take() {
                let _ = j.join();
            }
        }
    }

    /// Drop audio playback once; its destructor joins decode and closes AAudio.
    #[cfg(target_os = "android")]
    fn stop_audio(&self) {
        let _ = lock_recover(&self.audio).take();
    }

    /// Drop mic capture once; its destructor joins encode and closes AAudio.
    #[cfg(target_os = "android")]
    fn stop_mic(&self) {
        let _ = lock_recover(&self.mic).take();
    }

    /// Drop pad audio once and join its renderer before Kotlin closes the USB connection.
    #[cfg(target_os = "android")]
    pub(crate) fn stop_pad_audio(&self) {
        let _ = lock_recover(&self.pad_audio).take();
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        self.stop_video();
        self.stop_drain();
        #[cfg(target_os = "android")]
        self.stop_audio();
        #[cfg(target_os = "android")]
        self.stop_mic();
        #[cfg(target_os = "android")]
        self.stop_pad_audio();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        pack_src_crop, pack_surface_size, unpack_src_crop, unpack_surface_size, HandleTable,
    };

    /// A key resolves in its own table only until removed, and `0` never resolves.
    #[test]
    fn handle_table_keys_stay_in_their_table() {
        static A: HandleTable<&str> = HandleTable::new(0x1000_0000_0000_0001);
        static B: HandleTable<&str> = HandleTable::new(0x2000_0000_0000_0001);
        let a = A.insert("a");
        assert_ne!(a, 0);
        assert_eq!(A.get(a).as_deref(), Some(&"a"));
        assert!(
            B.get(a).is_none(),
            "a key from one table never resolves in another"
        );
        assert!(A.get(0).is_none() && A.remove(0).is_none());
        assert_eq!(A.remove(a).as_deref(), Some(&"a"));
        assert!(A.get(a).is_none() && A.remove(a).is_none());
    }

    /// The pair the presenter reads as one atomic load must survive the round trip — including a
    /// size wider than a signed 16-bit value, which every panel this runs on now is.
    #[test]
    fn surface_size_round_trips() {
        assert_eq!(
            unpack_surface_size(pack_surface_size(2800, 1260)),
            Some((2800, 1260))
        );
        assert_eq!(unpack_surface_size(pack_surface_size(1, 1)), Some((1, 1)));
    }

    /// "Not reported yet" — and anything nonsensical — is the one sentinel, so the layer falls back
    /// to the window's buffer geometry rather than composing into an empty rectangle.
    #[test]
    fn non_positive_sizes_are_the_sentinel() {
        assert_eq!(pack_surface_size(0, 0), 0);
        assert_eq!(pack_surface_size(1920, 0), 0);
        assert_eq!(pack_surface_size(-1, 1080), 0);
        assert_eq!(unpack_surface_size(0), None);
    }

    /// A crop survives the round trip to a sixty-thousandth of the frame; nonsense is the full
    /// frame, so a bad call can never blank the picture.
    #[test]
    fn src_crop_round_trips_and_rejects_nonsense() {
        let [l, t, r, b] = unpack_src_crop(pack_src_crop(0.0, 0.102, 1.0, 0.898));
        assert_eq!((l, r), (0.0, 1.0));
        assert!((t - 0.102).abs() < 1e-4 && (b - 0.898).abs() < 1e-4);
        assert_eq!(unpack_src_crop(0), [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(pack_src_crop(0.5, 0.0, 0.5, 1.0), 0);
        assert_eq!(pack_src_crop(-0.1, 0.0, 1.0, 1.0), 0);
        assert_eq!(pack_src_crop(0.0, 0.0, 1.0, f32::NAN), 0);
    }
}
