//! punktfunk Android client — the JNI bridge ("nativecore") over `punktfunk-core`.
//!
//! Architecture: the **Rust-heavy** client model (like `punktfunk-client-linux`, *not* the
//! thin-native-over-C-ABI Apple model). This `cdylib` links `punktfunk-core` directly and drives
//! the whole `punktfunk/1` protocol through [`punktfunk_core::client::NativeClient`]; Kotlin owns
//! only the Android-framework surface (Compose UI, `SurfaceView` lifecycle, input capture, the
//! Wi-Fi `MulticastLock` + permission UX, Keystore). The JNI seam below is the one place the two
//! languages meet.
//!
//! Why Rust-heavy: Kotlin cannot `import` the cbindgen C header the way Swift can, so a native
//! bridge is unavoidable. Writing it in Rust lets the Android client reuse the Linux client's
//! orchestration verbatim — audio jitter ring, the VK keymap inverse, latency/skew math, the
//! input capture state machine, trust/pairing logic, **mDNS discovery** ([`discovery`], the same
//! `mdns-sd` browse the Linux/Windows clients use) — instead of re-porting it into Kotlin. Kotlin
//! keeps only the Android-framework surface it must (Compose UI, `SurfaceView`, input capture, the
//! Wi-Fi `MulticastLock` + permission UX, Keystore identity).
//!
//! JNI symbols map to `io.unom.punktfunk.kit.NativeBridge` in the `:kit` Gradle module
//! (`clients/android`). The surface: mDNS host discovery ([`discovery`]) and the session lifecycle
//! in [`session`] — connect/pair + the trust surface, the per-plane pumps (video → AMediaCodec,
//! audio ↔ AAudio, mic uplink), input, and rumble/HID feedback ([`feedback`]), and mid-session
//! mode renegotiation.

use jni::objects::JObject;
use jni::EnvUnowned;

#[cfg(target_os = "android")]
mod adpf;
#[cfg(target_os = "android")]
mod audio;
// The Skia console UI host (design/android-skia-console-port.md): the shared `pf-console-ui`
// shell over EGL/GLES, on every ABI (the armv7 Skia archive is self-hosted — see Cargo.toml).
#[cfg(target_os = "android")]
mod console;
// "Send logs to host": the log-ring upload. Android-only, like the logcat tee that fills the ring.
#[cfg(target_os = "android")]
mod logs;
// AAudio callback arithmetic, `test`-gated on top of Android so its proof runs off-device.
#[cfg(any(target_os = "android", test))]
mod audio_format;
#[cfg(target_os = "android")]
mod decode;
// Gated like `pf_client_core::discovery`, the browse it folds: the Linux and Windows host builds
// link the JNI seam and run its tests. Kotlin only ever calls it on device.
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
mod discovery;
mod feedback;
// `decode`'s hung-decoder checks, `test`-gated like `audio_format` so their proof runs off-device.
#[cfg(any(target_os = "android", test))]
mod input_stall;
#[cfg(target_os = "android")]
mod mic;
/// Tier-A DualSense pad audio: the 0xD1 plane rendered on the pad's own USB endpoint.
mod pad_audio;
// The PyroWave lane: Vulkan compute decode + swapchain present, beside `decode`'s MediaCodec
// path (design/pyrowave-codec-plan.md). Ungated so the host build compiles the dispatch; the
// implementation inside is 64-bit-Android-only, like `pyrowave-sys`.
mod pyro;
mod session;
mod stats;
mod sys;
// Ungated: pure `jni` + `punktfunk_core::wol` (no Android framework), so it links
// into the host workspace build too. Kotlin only ever calls it on device.
mod wol;
// Ungated like `wol`: pure `jni` + `punktfunk_core::client` (the reachability probe). Kotlin calls
// it off the main thread to light saved-host "online" pips independently of mDNS.
mod probe;

/// Every `log` record, teed: to logcat (via [`android_logger::AndroidLogger`]) AND into
/// `pf_client_core::logring` — the source for the console's "Send logs to host" action
/// ([`logs`]). The ring line mirrors the desktop `ring_layer`'s shape (wallclock, level,
/// target, message) so a bundle reads the same on the host's Logs page whichever client
/// sent it. Both sinks share the crate's Info ceiling — the field ring gets exactly what
/// logcat gets, which also keeps per-frame DEBUG chatter out of it by construction.
#[cfg(target_os = "android")]
struct RingTee(android_logger::AndroidLogger);

#[cfg(target_os = "android")]
impl log::Log for RingTee {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.0.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        self.0.log(record);
        pf_client_core::logring::note(format!(
            "{} {:5} {} {}",
            pf_client_core::logring::wallclock(),
            record.level().as_str(),
            record.target(),
            record.args()
        ));
    }

    fn flush(&self) {
        self.0.flush();
    }
}

/// Initialize logging once when the JVM loads the library: logcat under the `punktfunk` tag,
/// teed into the client log ring (see [`RingTee`]). Core `tracing` events (transport warnings:
/// socket-buffer clamp, QoS failures) arrive here too: tracing's "log" feature — declared
/// explicitly in Cargo.toml rather than relied on via quinn's defaults — forwards them as
/// `log` records since no tracing subscriber is ever installed. Android-only — there is no
/// JVM (and no logcat) on the host build.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn JNI_OnLoad(
    _vm: *mut jni::sys::JavaVM,
    _reserved: *mut std::ffi::c_void,
) -> jni::sys::jint {
    let logcat = android_logger::AndroidLogger::new(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("punktfunk"),
    );
    // `set_boxed_logger` (unlike `init_once`) does not set the max level itself.
    if log::set_boxed_logger(Box::new(RingTee(logcat))).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    log::info!(
        "punktfunk_android loaded (core ABI v{})",
        punktfunk_core::ABI_VERSION
    );
    jni::sys::JNI_VERSION_1_6
}

/// `NativeBridge.nativeConsoleAvailable(): Boolean` — whether this `.so` carries the Skia
/// console host ([`console`]). Kotlin asks before it calls any `nativeConsole*` symbol, so a
/// build that ever drops the host on some ABI again degrades to the touch UI rather than an
/// `UnsatisfiedLinkError`. Today: every Android ABI.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleAvailable(
    _env: EnvUnowned,
    _this: JObject,
) -> jni::sys::jboolean {
    cfg!(target_os = "android")
}

/// The symbol `name` in the `dlopen` handle `lib`, as the fn-pointer type `F`; `None` when absent.
///
/// # Safety
/// `lib` is a live `dlopen` handle, and `F` is the `extern "C" fn` type of the symbol's C
/// signature. The size assert only rules out a non-pointer `F`.
#[cfg(target_os = "android")]
pub(crate) unsafe fn sym<F: Copy>(lib: *mut std::ffi::c_void, name: &std::ffi::CStr) -> Option<F> {
    const { assert!(size_of::<F>() == size_of::<*mut std::ffi::c_void>()) };
    // SAFETY: `lib` is live (caller) and `name` is NUL-terminated.
    let p = unsafe { libc::dlsym(lib, name.as_ptr()) };
    // SAFETY: a non-null symbol is a code address, and `F` is a pointer-sized fn type matching
    // its signature (caller).
    (!p.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut std::ffi::c_void, F>(&p) })
}

/// The `ANativeWindow` behind a Java `Surface`, holding its own reference; `None` when the
/// Surface has no window.
///
/// # Safety
/// `surface` is a non-null `android.view.Surface`.
#[cfg(target_os = "android")]
pub(crate) unsafe fn window_from_surface(
    env: &jni::Env<'_>,
    surface: &JObject<'_>,
) -> Option<ndk::native_window::NativeWindow> {
    // SAFETY: `env` is this thread's live JNIEnv and `surface` a live Surface reference (caller).
    // The casts bridge the jni-sys 0.4 (`jni`) / 0.3 (vendored `ndk`) pointer types.
    unsafe {
        ndk::native_window::NativeWindow::from_surface(
            env.get_raw() as *mut _,
            surface.as_raw() as *mut _,
        )
    }
}
