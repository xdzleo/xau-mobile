plugins {
    id("com.android.application") version "8.6.1" apply false
    id("org.jetbrains.kotlin.android") version "2.0.21" apply false
    // From Kotlin 2.0 the Compose compiler ships with Kotlin itself, so
    // there is no separate composeCompiler version to keep in step.
    id("org.jetbrains.kotlin.plugin.compose") version "2.0.21" apply false
    // Renders @Preview composables to PNG on the JVM — no emulator, no
    // device. 1.3.5 deliberately: the 2.0 line is alpha and drags in
    // Gradle 9 / AGP 8.13.
    id("app.cash.paparazzi") version "1.3.5" apply false
}
