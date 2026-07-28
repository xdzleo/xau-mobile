plugins {
    id("com.android.application") version "8.6.1" apply false
    id("org.jetbrains.kotlin.android") version "2.0.21" apply false
    // From Kotlin 2.0 the Compose compiler ships with Kotlin itself, so
    // there is no separate composeCompiler version to keep in step.
    id("org.jetbrains.kotlin.plugin.compose") version "2.0.21" apply false
}
