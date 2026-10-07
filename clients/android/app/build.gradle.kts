import org.jetbrains.kotlin.gradle.dsl.JvmTarget

import java.util.Properties

plugins {
    id("com.android.application")
    // AGP 9 built-in Kotlin: NO org.jetbrains.kotlin.android. The Compose compiler plugin is
    // supplied by AGP, so it's applied without a version.
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "io.unom.punktfunk"
    compileSdk = 37 // Android 17 — required by androidx.core 1.19.0.
    // The NDK whose llvm-strip strips the packaged .so. Unset, AGP looks for its own default
    // NDK and packages the library unstripped when that one is not installed.
    ndkVersion = providers.gradleProperty("punktfunk.ndkVersion").get()

    defaultConfig {
        // Load from .env if it exists (local dev), otherwise from System.getenv (CI)
        val envFile = project.rootProject.file(".env")
        val props = Properties()
        if (envFile.exists()) {
            envFile.inputStream().use { props.load(it) }
        }

        applicationId = "io.unom.punktfunk"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        // Android 9. Reaches older Android TV boxes (e.g. Amlogic streamers still on Android 9–11);
        // the handful of API 31+ APIs we use are runtime-gated (Material You → brand palette, rumble
        // → legacy Vibrator, NEARBY_WIFI/lights/ADPF already gated), so nothing is lost above 28.
        minSdk = 28
        // Android 17: targeting 37 makes Local Network Protection MANDATORY — all LAN traffic (the
        // QUIC dial, mDNS, WoL, the library fetch) is blocked until the user grants the
        // ACCESS_LOCAL_NETWORK runtime permission. ConnectScreen owns that request/rationale flow;
        // don't bump past 37 without re-checking the next release's behavior changes.
        targetSdk = 37
        val vCode = (props.getProperty("VERSION_CODE") ?: System.getenv("VERSION_CODE"))
        versionCode = vCode?.toInt() ?: 1
        // versionName is the single project version, threaded from CI (a vX.Y.Z release or a
        // canary string). versionCode stays the monotonic run number (Play rejects regressions).
        // Local dev (no VERSION_NAME) falls back to the workspace version from the root Cargo.toml —
        // the single source of truth — so an on-device build shows the real current version, not a
        // stale placeholder.
        val workspaceVersion = runCatching {
            project.rootProject.file("../../Cargo.toml").readLines()
                .dropWhile { !it.trim().startsWith("[workspace.package]") }
                .firstOrNull { it.trim().startsWith("version") }
                ?.substringAfter('=')?.trim()?.trim('"')
        }.getOrNull()
        versionName = (props.getProperty("VERSION_NAME") ?: System.getenv("VERSION_NAME"))
            ?: workspaceVersion ?: "0.0.0"
        // Ship 32-bit armeabi-v7a alongside 64-bit arm64-v8a: many Google TV / Android TV streamers
        // (Walmart onn. 4K, Chromecast with Google TV, budget Amlogic boxes) run a 32-bit Android
        // userspace, and because this app carries native code, Google Play (and a sideload installer)
        // filters it as "not compatible" on those devices unless an armeabi-v7a variant is present.
        // x86_64 stays for the emulator. Google keeps delivering to 32-bit TV devices (see the Aug
        // 2025 "64-bit app compatibility for Google TV and Android TV" post) — the 64-bit lib is the
        // required half; the 32-bit lib is what actually reaches the boxes people report failing.
        ndk { abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64") }
    }

    signingConfigs {
        create("release") {
            // Load from .env if it exists (local dev), otherwise from System.getenv (CI)
            val envFile = project.rootProject.file(".env")
            val props = Properties()
            if (envFile.exists()) {
                envFile.inputStream().use { props.load(it) }
            }

            val ksFile = props.getProperty("RELEASE_KEYSTORE_FILE") ?: System.getenv("RELEASE_KEYSTORE_FILE")
            if (ksFile != null) {
                storeFile = file(ksFile)
                storePassword = props.getProperty("RELEASE_KEYSTORE_PASSWORD") ?: System.getenv("RELEASE_KEYSTORE_PASSWORD")
                keyAlias = props.getProperty("RELEASE_KEY_ALIAS") ?: System.getenv("RELEASE_KEY_ALIAS")
                keyPassword = props.getProperty("RELEASE_KEY_PASSWORD") ?: System.getenv("RELEASE_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            signingConfig = signingConfigs.getByName("release")
            // The stripped .symtab rides in the AAB for Play and in native-debug-symbols.zip.
            // FULL keeps all of it without the code; the release profile emits no DWARF.
            ndk { debugSymbolLevel = "FULL" }
        }
    }

    buildFeatures { compose = true }

    // Roborazzi/Robolectric render Compose on the host JVM (the CI screenshot harness) and need the
    // merged Android resources + the app's manifest/theme available to the unit tests.
    testOptions { unitTests { isIncludeAndroidResources = true } }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_21
        targetCompatibility = JavaVersion.VERSION_21
    }
    packaging {
        jniLibs {
            useLegacyPackaging = false
        }
    }
}

kotlin { compilerOptions { jvmTarget.set(JvmTarget.JVM_21) } }

// Debug APKs keep the .so symbol table, so a dev build's tombstone names its frames.
androidComponents {
    onVariants(selector().withBuildType("debug")) { it.packaging.jniLibs.keepDebugSymbols.add("**/*.so") }
}

dependencies {
    implementation(project(":kit"))

    val composeBom = platform("androidx.compose:compose-bom:2026.09.00")
    implementation(composeBom)

    implementation("androidx.core:core-ktx:1.19.1")
    implementation("androidx.activity:activity-compose:1.13.0")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.11.0")

    // Fold posture for the tabletop split (FoldSplit.kt) — there is no framework API for a hinge,
    // WindowInfoTracker is the platform's only source for one.
    implementation("androidx.window:window:1.5.1")

    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-core") // bottom-bar / rail tab icons
    implementation("androidx.compose.material:material-icons-extended") // settings-category icons
    debugImplementation("androidx.compose.ui:ui-tooling")

    // Cover-art loading for the game-library coverflow. Coil's OkHttp fetcher is fed the same mTLS
    // OkHttpClient the library fetch uses (reaching the host's own art proxy).
    implementation("io.coil-kt.coil3:coil-compose:3.6.3")
    implementation("io.coil-kt.coil3:coil-network-okhttp:3.6.3")

    // Real backdrop blur for the floating console legends (RenderEffect on API 31+, a translucent
    // scrim below). The gamepad UI's frosted pills sample + blur whatever scrolls behind them.
    implementation("dev.chrisbanes.haze:haze:2.0.1")

    // Android TV components (we target phone + TV) land in the TV-UI milestone:
    //   implementation("androidx.tv:tv-material:1.1.0")
    // The manifest already declares leanback so the scaffold installs on TV.

    // --- CI screenshot harness (Roborazzi on the JVM via Robolectric — no emulator/GPU). The
    // screenshot tests render the real Compose UI with mock state; never load the JNI core, so the
    // job runs `:app:testDebugUnitTest -PskipRustBuild` (see kit/build.gradle.kts). ---
    testImplementation(composeBom)
    testImplementation("androidx.compose.ui:ui-test-junit4")
    // Deterministic cover art for the library scene: FakeImageLoaderEngine answers the coverflow's
    // AsyncImage synchronously with generated posters — no network, no async race under the frozen
    // animation clock.
    testImplementation("io.coil-kt.coil3:coil-test:3.6.3")
    debugImplementation("androidx.compose.ui:ui-test-manifest") // the ComponentActivity test host
    testImplementation("junit:junit:4.13.2")
    // Real `org.json` for the shared-vectors test: the `org.json` inside `android.jar` is a stub
    // set whose every method throws "Stub!", so a plain JVM unit test cannot parse with it. Same
    // dependency, same reason, as the kit module's deeplink-vectors test.
    testImplementation("org.json:json:20260814")
    testImplementation("org.robolectric:robolectric:4.17")
    testImplementation("io.github.takahirom.roborazzi:roborazzi:1.76.0")
    testImplementation("io.github.takahirom.roborazzi:roborazzi-compose:1.76.0")

    // --- On-device tests (`:app:connectedDebugAndroidTest` against an emulator or a phone). The
    // stream screen needs the real JNI core underneath it — its native calls run against a zero
    // session handle — so its tests cannot be Robolectric ones. ---
    androidTestImplementation(composeBom)
    androidTestImplementation("androidx.compose.ui:ui-test-junit4")
    androidTestImplementation("androidx.test.ext:junit:1.3.0")
    androidTestImplementation("androidx.test:runner:1.7.0")
}

// Record (write) the screenshots when the unit tests run. These tests exist to GENERATE marketing
// images, not to diff goldens, so always capture rather than verify.
tasks.withType<Test>().configureEach {
    systemProperty("roborazzi.test.record", "true")
    // Robolectric 4.17's FileDescriptor shadow reads the JDK's SharedSecrets, which java.base does
    // not export to the unnamed module.
    jvmArgs("--add-exports=java.base/jdk.internal.access=ALL-UNNAMED")
    // -PexcludeScreenshots drops the Roborazzi scenes so the PR gate can run the whole suite.
    // They share this source set but are a release-artifact job (android-screenshots.yml, v* tags):
    // 24 @Test that write PNGs and assert nothing, and a minute nobody owes on every push.
    // Without this the gate had to be an allowlist, which silently rotted in both directions —
    // five patterns naming classes deleted with the Compose console, and six live classes that
    // gated nothing. Gradle only fails when the WHOLE filter set matches nothing, so neither
    // half was ever reported.
    if (project.hasProperty("excludeScreenshots")) {
        filter { excludeTestsMatching("io.unom.punktfunk.screenshots.*") }
    }
}
