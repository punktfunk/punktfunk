import java.io.File
import java.net.URI
import java.security.MessageDigest
import java.util.Properties
import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    // AGP 9 built-in Kotlin compiles this module's Kotlin (NativeBridge) — no kotlin.android plugin.
    id("com.android.library")
}

val ndkVer = providers.gradleProperty("punktfunk.ndkVersion").get() // gradle.properties

android {
    namespace = "io.unom.punktfunk.kit"
    compileSdk = 37 // Android 17 — align with :app (androidx.core 1.19.0 requires it)
    ndkVersion = ndkVer

    defaultConfig {
        minSdk = 28 // Android 9 — reaches older TV boxes; API 31+ features are runtime-gated.
        // Keep in lockstep with :app — 32-bit armeabi-v7a for the many 32-bit Google TV / Android TV
        // boxes, 64-bit arm64-v8a for phones + modern TV, x86_64 for the emulator.
        ndk { abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64") }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_21
        targetCompatibility = JavaVersion.VERSION_21
    }
    packaging { jniLibs { useLegacyPackaging = false } } // 16 KB-page friendly
}

kotlin { compilerOptions { jvmTarget.set(JvmTarget.JVM_21) } }

dependencies {
    // mTLS HTTPS client for the host's management API (the game-library fetch + cover-art loads).
    // OkHttp lets us present the paired client cert and pin the host's self-signed cert by SHA-256.
    // `api`: the client type is in this module's signature, and Coil's POM pins 4.12.0 for :app.
    api("com.squareup.okhttp3:okhttp:5.5.0")
    testImplementation("junit:junit:4.13.2") // JVM unit tests for the pure parsers/migrations
    // A REAL org.json on the unit-test classpath. android.jar's org.json is stubs that throw
    // "Stub!", so the host-store migration test — which asserts over the very JSON blobs the store
    // reads and writes — cannot run without it. Explicit test deps precede the mockable android.jar.
    testImplementation("org.json:json:20260814")
}

// ------------------------------------------------------------------------------------------------
// cargo-ndk: cross-compile clients/android/native (punktfunk-client-android) into this module's jniLibs/<abi>/ so the
// resulting libpunktfunk_android.so is packaged into the app (and any AAR this module produces).
// NDK r28+ aligns to 16 KB pages by default — no extra linker flags. Prereqs (see clients/android
// /README.md): `cargo install cargo-ndk` + `rustup target add aarch64-linux-android x86_64-linux-android`.
// ------------------------------------------------------------------------------------------------
val repoRoot = rootDir.parentFile.parentFile // clients/android -> clients -> repo root
// CARGO_HOME first: rustup puts every binary in $CARGO_HOME/bin, and the CI image
// (ci/android-ci.Dockerfile) installs the shared toolchain at /usr/local/cargo — the
// historical ~/.cargo fallback is what a GUI Android Studio launch (no env) still needs.
val cargoBin = System.getenv("CARGO_HOME")?.let { "$it/bin" }
    ?: "${System.getProperty("user.home")}/.cargo/bin"

// SDK location without depending on AGP's DSL (sdkDirectory isn't in AGP 9's library extension):
// env first (set by Android Studio and by our CLI shell), then local.properties, then the default.
fun androidSdkDir(): String {
    System.getenv("ANDROID_HOME")?.let { return it }
    System.getenv("ANDROID_SDK_ROOT")?.let { return it }
    val lp = rootProject.file("local.properties")
    if (lp.exists()) {
        val props = Properties()
        lp.inputStream().use { props.load(it) }
        props.getProperty("sdk.dir")?.let { return it }
    }
    return "${System.getProperty("user.home")}/Library/Android/sdk"
}

/// The NDK's `toolchains/llvm/prebuilt/<host-tag>/lib`, holding `libclang.so`/`.dylib`. The host
/// tag is `linux-x86_64`, `darwin-x86_64` (Rosetta on Apple silicon) or `windows-x86_64`, so it is
/// resolved by listing rather than guessed — there is exactly one.
fun ndkLibclangDir(): String? {
    val prebuilt = File("${androidSdkDir()}/ndk/$ndkVer/toolchains/llvm/prebuilt")
    val host = prebuilt.listFiles()?.firstOrNull { it.isDirectory } ?: return null
    return File(host, "lib").takeIf { it.isDirectory }?.absolutePath
}

// Every cargo-ndk invocation needs the same discovery environment, and they must not drift apart:
// a lint that ran against a different toolchain/sysroot than the build is a lint about a different
// program. Applied by both `registerCargoNdk` (build) and `registerCargoNdkClippy` (lint).
fun Exec.cargoNdkEnvironment() {
    val sdk = androidSdkDir()
    // A GUI Android Studio launch does not source the login shell, so make cargo, the NDK, and
    // cmake (libopus builds via the cmake crate) discoverable explicitly — same as a bare CLI.
    val cmakeBin = "$sdk/cmake/3.22.1/bin"
    environment(
        "PATH",
        cargoBin + File.pathSeparator + cmakeBin + File.pathSeparator + System.getenv("PATH"),
    )
    environment("ANDROID_HOME", sdk)
    environment("ANDROID_NDK_HOME", "$sdk/ndk/$ndkVer")
    // CMake's built-in Android support (used by the cmake crate for libopus) finds the NDK via
    // these, and uses Ninja (bundled next to the SDK cmake) since there's no `make`.
    environment("ANDROID_NDK_ROOT", "$sdk/ndk/$ndkVer")
    environment("ANDROID_NDK", "$sdk/ndk/$ndkVer")
    environment("CMAKE_GENERATOR", "Ninja")
    // pyrowave-sys runs bindgen over the codec's C API, and bindgen dlopens libclang at build
    // time. Point it at the NDK's own copy rather than requiring one on the box: the NDK is
    // already the hard prerequisite here, its clang is the one the sysroot matches, and this
    // keeps a dev machine and the CI image (which carries no libclang-dev) on the same footing.
    ndkLibclangDir()?.let { environment("LIBCLANG_PATH", it) }
    // A GNU build-id ties a tombstone to its symbols. Target-scoped, so host build scripts and
    // desktop builds keep their flags. A RUSTFLAGS env overrides it, and check-android-strip.sh
    // then fails the build.
    for (triple in listOf("AARCH64_LINUX_ANDROID", "ARMV7_LINUX_ANDROIDEABI", "X86_64_LINUX_ANDROID")) {
        environment("CARGO_TARGET_${triple}_RUSTFLAGS", "-C link-arg=-Wl,--build-id=sha1")
    }
    // The Skia console (pf-console-ui over skia-bindings): prebuilt Skia archives are keyed by
    // target + features (see the fetchSkiaBinaries block below for provenance and digests).
    // The default hands skia-bindings a file:// template into the directory fetchSkiaBinaries
    // has already SHA-256-verified — skia-bindings itself downloads without any content check,
    // so it must only ever see verified local bytes (security-review 2026-08-31 H-2).
    // Override with `-PskiaBinariesUrl=<template>` or the `SKIA_BINARIES_URL` env (`{tag}`/`{key}`
    // placeholders, `file://` allowed) — e.g. a local mirror while cutting the next bump's
    // archives. 🛑 The override bypasses the digest check (its archives are by definition not the
    // committed ones), and skia-bindings never fails when no archive matches — it silently builds
    // Skia from source for hours; every log must show `DOWNLOAD AND INSTALL SUCCEEDED` per target.
    environment(
        "SKIA_BINARIES_URL",
        skiaBinariesOverride()
            ?: "file://${skiaBinariesDir.get().asFile.absolutePath}/skia-binaries-{key}.tar.gz",
    )
}

// ------------------------------------------------------------------------------------------------
// Verified Skia prebuilt fetch. rust-skia's GitHub releases carry no armv7-linux-androideabi
// archive, so ALL three Android keys are served from our own release — public, R2-backed,
// unauthenticated: git.unom.io/unom/skia-binaries/releases/download/{tag}/skia-binaries-{key}.tar.gz
// (the armv7 archive built by us, the two 64-bit ones byte-for-byte mirrors of rust-skia's —
// design/android-skia-console-port.md WP6; re-derive + re-upload on every skia-safe bump).
//
// A release asset is replaceable under the same name, so the download is authenticated by the
// digests COMMITTED here, not by its URL: fetchSkiaBinaries downloads each archive, verifies its
// SHA-256, and fails the build on any mismatch; the cargo-ndk tasks then read only the verified
// local files (security-review 2026-08-31 H-2).
//
// The archives at rust-skia tag 0.153.3, key `<skia-hash>-<target>-ganesh-gl-jpegd-jpege-pdf-textlayout`
// (hash b7f043e0b1e2a850e702). Re-derive tag + hash + digests on every skia-safe bump.
// armv7 provenance: SKIA_GN_ARGS='extra_cflags+=["-D__ANDROID_MIN_SDK_VERSION__=26"]'
// FORCE_SKIA_BUILD=1 cargo ndk -t armeabi-v7a build -p pf-console-ui --no-default-features, then
// OUT_DIR/skia/{libskia,libskshaper,libskparagraph,libskunicode_core,libskunicode_icu,
// libskia-bindings}.a + bindings.rs + LICENSE_SKIA + tag.txt + key.txt packed as skia-binaries/ in
// skia-binaries-<key>.tar.gz. The define is for ICU: skia-bindings passes an unversioned --target,
// so NDK 28+ hides posix_madvise from umapfile.cpp.
// ------------------------------------------------------------------------------------------------
val skiaBinariesTag = "0.153.3"
val skiaBinariesHash = "b7f043e0b1e2a850e702"
val skiaBinariesSha256 = mapOf(
    "aarch64-linux-android" to "adcadc1c7f4340bb68b15e48c008c423ac30f8b454c609d8a20c89c8d731890f",
    "x86_64-linux-android" to "b9f6266160c2cacb2762233617592d8f0492ca824192df2f659c1c97196d6781",
    "armv7-linux-androideabi" to "c099e9eed757ffb85b11a572373610e496ac5b72fb220d22c06f2caa1dcea083",
)
val skiaBinariesDir = layout.buildDirectory.dir("skia-binaries")

fun skiaBinariesOverride(): String? =
    (project.findProperty("skiaBinariesUrl") as String? ?: System.getenv("SKIA_BINARIES_URL"))
        ?.takeIf { it.isNotBlank() }

fun sha256Of(f: File): String {
    val md = MessageDigest.getInstance("SHA-256")
    f.inputStream().use { ins ->
        val buf = ByteArray(1 shl 16)
        while (true) {
            val n = ins.read(buf)
            if (n < 0) break
            md.update(buf, 0, n)
        }
    }
    return md.digest().joinToString("") { b -> "%02x".format(b) }
}

val fetchSkiaBinaries = tasks.register("fetchSkiaBinaries") {
    group = "rust"
    description = "Fetch + SHA-256-verify the prebuilt Skia archives for skia-bindings"
    doLast {
        val override = skiaBinariesOverride()
        if (override != null) {
            logger.warn(
                "SKIA_BINARIES_URL override active ($override) — skia archives are NOT digest-verified",
            )
            return@doLast
        }
        val dir = skiaBinariesDir.get().asFile
        dir.mkdirs()
        val template =
            "https://git.unom.io/unom/skia-binaries/releases/download/{tag}/skia-binaries-{key}.tar.gz"
        for ((target, expected) in skiaBinariesSha256) {
            val key = "$skiaBinariesHash-$target-ganesh-gl-jpegd-jpege-pdf-textlayout"
            val dest = dir.resolve("skia-binaries-$key.tar.gz")
            if (!dest.exists() || sha256Of(dest) != expected) {
                val url = template.replace("{tag}", skiaBinariesTag).replace("{key}", key)
                logger.lifecycle("fetching $url")
                URI(url).toURL().openStream().use { ins ->
                    dest.outputStream().use { out -> ins.copyTo(out) }
                }
            }
            val actual = sha256Of(dest)
            if (actual != expected) {
                dest.delete()
                throw GradleException(
                    "skia archive $key: sha256 $actual does not match the committed $expected — " +
                        "refusing to compile unverified native code (security-review 2026-08-31 H-2)",
                )
            }
        }
    }
}

fun registerCargoNdk(taskName: String, release: Boolean) =
    tasks.register<Exec>(taskName) {
        group = "rust"
        description = "cargo-ndk build of punktfunk-client-android (${if (release) "release" else "debug"})"
        dependsOn(fetchSkiaBinaries) // skia-bindings must only see digest-verified archives (H-2)
        workingDir = repoRoot
        // cargo-ndk only adds to jniLibs. Empty it first, so no library an older tree built ships.
        val jniLibs = file("src/main/jniLibs")
        doFirst { jniLibs.deleteRecursively() }
        cargoNdkEnvironment()
        // Resolve cargo by ABSOLUTE path: Gradle's Exec resolves command[0] via the JVM's
        // inherited PATH, NOT the environment("PATH", …) set above (that only reaches the spawned
        // child). A GUI Android Studio launch (and any daemon it started) has no ~/.cargo/bin on
        // its PATH, so a bare "cargo" fails to start. The env PATH above still lets cargo/cargo-ndk
        // find their subtools.
        val cmd = mutableListOf(
            "$cargoBin/cargo", "ndk",
            "-t", "arm64-v8a", "-t", "armeabi-v7a", "-t", "x86_64",
            // Link against the minSdk-28 sysroot (libaaudio, API 26, is present). NOTE: this does
            // NOT reject an accidental >28 hard import — a cdylib link permits undefined symbols,
            // which then fail at System.loadLibrary on every device below the symbol's API level
            // (the 0.9.0 Android-≤12 regression). The checkJniImports* task after this build is
            // what actually enforces the floor; >28 entry points must be dlsym-resolved (see
            // decode::try_set_frame_rate, decode::install_render_callback, adpf).
            "--platform", "28",
            "-o", jniLibs.absolutePath,
            "build", "-p", "punktfunk-client-android",
        )
        if (release) cmd += "--release"
        commandLine(cmd)
    }

// ------------------------------------------------------------------------------------------------
// Lint the ANDROID target. `punktfunk-client-android` and every `#[cfg(target_os = "android")]`
// module elsewhere in the workspace were, until this task existed, **completely unlinted**: ci.yml
// runs `cargo clippy --workspace` on the HOST, where all of that code is compiled out, and this
// workflow only ever ran `build`. The gap was found in 2026-08 with five lints sitting in
// clients/android/native (two of them `unnecessary_cast`, which is exactly the class that decides
// whether a cast is redundant BY POINTER WIDTH).
//
// Both widths are linted, and that is the load-bearing part: arm64-v8a is 64-bit and armeabi-v7a is
// 32-bit, so a cast that is redundant on one can be required on the other. Linting only the primary
// ABI would license "fixes" that break the 32-bit build — the shipping ABI for the many 32-bit
// Google TV / Android TV boxes this client targets. x86_64 is deliberately omitted: it is
// emulator-only and shares its pointer width with arm64, so it costs a third of the job's lint time
// for no signal these two do not already carry.
//
// `--all-targets` for the same reason ci.yml spells it out: without it the `#[cfg(test)]` modules
// are never compiled, and un-compiled test code drifts silently.
fun registerCargoNdkClippy(taskName: String) =
    tasks.register<Exec>(taskName) {
        group = "verification"
        description = "clippy (deny warnings) for punktfunk-client-android on both Android widths"
        dependsOn(fetchSkiaBinaries) // skia-bindings must only see digest-verified archives (H-2)
        workingDir = repoRoot
        cargoNdkEnvironment()
        commandLine(
            // Absolute cargo path for the same reason as the build task above.
            "$cargoBin/cargo", "ndk",
            "-t", "arm64-v8a", "-t", "armeabi-v7a",
            "--platform", "28",
            "clippy", "-p", "punktfunk-client-android", "--all-targets",
            "--", "-D", "warnings",
        )
    }

val cargoNdkClippy = registerCargoNdkClippy("cargoNdkClippy")

// Post-link floor check: every undefined symbol in the built .so must exist in the API-28 stubs,
// else System.loadLibrary fails on devices at the minSdk floor (see the script header for the
// 0.9.0 incident this guards against). Runs right after its cargo-ndk task; the APK build depends
// on this task (not the cargo one directly), so a violation fails the build, local and CI alike.
fun registerCheckJniImports(taskName: String, cargoTask: TaskProvider<Exec>) =
    tasks.register<Exec>(taskName) {
        group = "rust"
        description = "verify libpunktfunk_android.so imports stay within the API-28 floor"
        dependsOn(cargoTask)
        workingDir = repoRoot
        commandLine(
            "sh", File(repoRoot, "scripts/ci/check-android-jni-imports.sh").absolutePath,
            "${androidSdkDir()}/ndk/$ndkVer",
            file("src/main/jniLibs").absolutePath,
            "28",
        )
    }

val cargoNdkDebug = registerCargoNdk("cargoNdkDebug", release = false)
val cargoNdkRelease = registerCargoNdk("cargoNdkRelease", release = true)
val checkJniImportsDebug = registerCheckJniImports("checkJniImportsDebug", cargoNdkDebug)
val checkJniImportsRelease = registerCheckJniImports("checkJniImportsRelease", cargoNdkRelease)

afterEvaluate {
    // `-PskipRustBuild` skips the cargo-ndk native build — for JVM-only tasks (the Roborazzi
    // screenshot unit tests render Compose on the JVM and never load libpunktfunk_android.so), so
    // CI/local screenshot runs don't need the Rust toolchain or NDK. The native build stays wired
    // for every normal APK/AAR build.
    //
    // DEBUG APKs SHIP RELEASE RUST. Cargo's debug profile is not "a bit slower" for this library —
    // it is unusable: the AES-GCM data-plane decrypt runs through generic-array iterator closures
    // with per-byte UB checks instead of ARMv8 hardware AES. Profiled live on a phone (simpleperf):
    // ~800 µs of user CPU per 1.4 KB packet, the receive pump pinned over a full core yet unable to
    // drain a 20 Mbps stream — every debug-APK on-device test was silently benchmarking unoptimized
    // crypto, not the streaming pipeline. Kotlin debuggability is untouched (the APK is still a
    // debug build); only the cargo profile changes. `-PrustDebug` restores a debug-profile native
    // build for the rare session that actually steps through Rust.
    if (!project.hasProperty("skipRustBuild")) {
        val debugRust =
            if (project.hasProperty("rustDebug")) checkJniImportsDebug else checkJniImportsRelease
        tasks.named("preDebugBuild").configure { dependsOn(debugRust) }
        tasks.named("preReleaseBuild").configure { dependsOn(checkJniImportsRelease) }
    }
}
