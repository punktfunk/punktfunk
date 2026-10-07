import org.gradle.api.artifacts.dsl.LockMode

// Root build file. AGP 9.4.1 has BUILT-IN Kotlin support — modules do NOT apply
// org.jetbrains.kotlin.android (it's an error under AGP 9). The Compose compiler plugin is declared
// here (version + apply false) so modules can apply it version-less; its version pins the build's
// Kotlin (compose-compiler and Kotlin release in lockstep), keeping them matched.
// Toolchain: AGP 9.4.1 · Gradle 9.8.0 · Kotlin/Compose-compiler 2.4.20 · JDK 21 · Compose BOM
// 2026.09.00 · compileSdk 37 · targetSdk 37 · minSdk 28.
plugins {
    id("com.android.application") version "9.4.1" apply false
    id("com.android.library") version "9.4.1" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.4.20" apply false
}

allprojects {
    dependencyLocking {
        lockAllConfigurations()
        lockMode.set(LockMode.STRICT)
    }
}
