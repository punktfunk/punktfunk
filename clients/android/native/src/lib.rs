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
//! (`clients/android`). The surface: the native-link proof (`abiVersion`/`coreVersion`), mDNS host
//! discovery ([`discovery`]), and the session lifecycle in [`session`] — connect/pair + the trust
//! surface, the per-plane pumps (video → AMediaCodec, audio ↔ AAudio, mic uplink), input, and
//! rumble/HID feedback ([`feedback`]), and mid-session mode renegotiation.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JObject, JString};
use jni::sys::jint;
use jni::EnvUnowned;

#[cfg(target_os = "android")]
mod adpf;
#[cfg(target_os = "android")]
mod audio;
// The Skia console UI host (design/android-skia-console-port.md): the shared `pf-console-ui`
// shell over EGL/GLES, on every ABI (the armv7 Skia archive is self-hosted — see Cargo.toml).
#[cfg(target_os = "android")]
mod console;
// "Send logs to host": the log-ring upload (`pf-client-core` is Android-target-only here).
#[cfg(target_os = "android")]
mod logs;
// The RESOLVED audio format + its ms ⇄ sample arithmetic, split out of `audio` and — unlike it —
// ungated, because that arithmetic is what a rate the ladder does not divide gets wrong (44 100 Hz
// used to come out 2.3 % off in every direction at once) and it must be provable without a phone.
// Nothing in it touches AAudio. `test`-gated for the host build on top of the Android one so the
// off-device leg still compiles and runs the proof; `audio` is its only non-test user.
#[cfg(any(target_os = "android", test))]
mod audio_format;
#[cfg(target_os = "android")]
mod decode;
// Ungated: pure `mdns-sd` + `jni`, so the browse + its JNI seam link into the host workspace build
// (and its unit test runs there) exactly like `session`/`stats`. Kotlin only ever calls it on device.
mod discovery;
mod feedback;
// `decode`'s hung-codec check, `test`-gated like `audio_format` so its proof runs off-device.
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
// Ungated like `discovery`: pure `jni` + `punktfunk_core::wol` (no Android framework), so it links
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
) -> jint {
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

/// `NativeBridge.abiVersion(): Int` — the core's C-ABI version. A non-error return is the
/// scaffold's proof that `System.loadLibrary` found the `.so`, the JNI symbol resolved, and the
/// linked `punktfunk-core` is the one we expect.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_abiVersion(
    _env: EnvUnowned,
    _this: JObject,
) -> jint {
    punktfunk_core::ABI_VERSION as jint
}

/// `NativeBridge.coreVersion(): String` — the crate version, proving JNI string marshaling works.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_coreVersion<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> JString<'local> {
    env.with_env(|env| env.new_string(env!("CARGO_PKG_VERSION")))
        .resolve::<LogErrorAndDefault>()
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
