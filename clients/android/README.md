# punktfunk — Android client (phone & TV)

One Compose app for phone, tablet and Android TV (the TV layout is the same app in leanback mode).
Installing it is [the docs site](https://docs.punktfunk.unom.io/docs/install-client)'s job — it is a
public Google Play listing, with a signed APK published on every build for anyone who would rather
sideload.

Built for `arm64-v8a`, `armeabi-v7a` and `x86_64`. The 32-bit slice is what keeps the app
installable on the many 32-bit Google TV / Android TV streamers (Walmart onn. 4K, Chromecast with
Google TV, budget Amlogic boxes) that reject a 64-bit-only build as "not compatible".

## Why it is Rust-heavy

Kotlin cannot `import` the cbindgen C header the way Swift can, so a native bridge is unavoidable.
Writing it in Rust and linking `punktfunk-core` directly means the Android client reuses the Linux
client's orchestration — audio jitter ring, VK keymap inverse, latency/skew math, capture state
machine, trust logic — instead of re-porting all of it into Kotlin.

| Side | Owns |
|------|------|
| **Rust** (`native/` → `libpunktfunk_android.so`) | the JNI seam, `NativeClient` (QUIC control + UDP data plane), AnnexB → `AMediaCodec` decode incl. HDR10 and PyroWave over Vulkan compute, Opus + AAudio audio and mic, controller feedback, latency math, trust/pairing, `mdns-sd` discovery |
| **Kotlin** (`app/`, `kit/`) | Compose UI, `SurfaceView` lifecycle, input capture, the Wi-Fi `MulticastLock` and permission UX, Keystore identity |

The single seam is `io.unom.punktfunk.kit.NativeBridge` ⇄
`Java_io_unom_punktfunk_kit_NativeBridge_*`. Keep it that way: a second seam is a second ABI to
version.

## Build

Pinned toolchain — AGP 9.4 · Gradle 9.8.0 · Kotlin 2.4.20 · Compose BOM 2026.09.00 · compileSdk 37 ·
minSdk 28. You need the Android SDK plus **NDK r30** (`30.0.14904198`), `platforms;android-37.0`,
`build-tools;37.0.0`, `cmake;3.22.1` (builds libopus), **JDK 21** (AGP 9.4 runs on 17–21, not a
newer default), and `cargo install cargo-ndk` with the three Android Rust targets added.

Android Studio: open `clients/android` — it uses its bundled JBR 21 and the `cargoNdk*` task builds
the `.so` as part of a normal build. From the CLI, point Gradle at JDK 21 if your machine default is
newer:

```sh
export JAVA_HOME="$(/usr/libexec/java_home -v 21)"
cd clients/android
./gradlew :app:assembleDebug     # cargo-ndk cross-compiles libpunktfunk_android.so first
./gradlew :app:installDebug
```

The debug APK lands in `app/build/outputs/apk/debug/`.
