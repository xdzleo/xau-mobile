import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

// Signing details live outside the repo (see .gitignore). Absent on a
// fresh clone, in which case release builds come out unsigned rather
// than failing the whole configuration phase.
val keystoreProps = Properties().apply {
    val f = rootProject.file("keystore.properties")
    if (f.exists()) f.inputStream().use { load(it) }
}

/**
 * Copy the Rust libraries into jniLibs before they are packaged.
 *
 * jniLibs/ is a build output and therefore gitignored, so without this
 * a fresh clone silently produces an APK with no core in it — and worse,
 * an existing checkout packages whatever .so was left there last time.
 * A stale native library is invisible until the app misbehaves at
 * runtime, which is exactly the bug worth spending a task on.
 *
 * Two ABIs, for two different jobs:
 *   arm64-v8a — every real phone since ~2019. This is what ships.
 *   x86_64    — the emulator, and nothing else. An x86_64 image on an
 *               x86_64 host runs at native speed; an arm64 image has to
 *               translate every instruction, which is the difference
 *               between a testing tool and a stopwatch.
 * A missing ABI is a warning, not an error: building only arm64 is the
 * normal case for a release, and failing the build over an emulator
 * library nobody asked for would be obnoxious.
 */
val NATIVE_ABIS = mapOf(
    "arm64-v8a" to "aarch64-linux-android",
    "x86_64" to "x86_64-linux-android",
)

val syncNativeLibs by tasks.registering {
    val jniLibs = layout.projectDirectory.dir("src/main/jniLibs")
    val sources = NATIVE_ABIS.mapValues { (_, triple) ->
        rootProject.file("../target-android/$triple/release/libxau_jni.so")
    }
    // Declared so Gradle can skip the task when nothing moved, instead
    // of re-copying on every single build.
    inputs.files(sources.values.filter { it.exists() })
    outputs.dir(jniLibs)

    doLast {
        sources.forEach { (abi, src) ->
            if (!src.exists()) {
                logger.warn("no $abi core at ${src.path} — run build-android.cmd for that target")
                return@forEach
            }
            val dest = jniLibs.dir(abi).asFile
            dest.mkdirs()
            src.copyTo(File(dest, "libxau_jni.so"), overwrite = true)
        }
    }
}

tasks.named("preBuild") { dependsOn(syncNativeLibs) }

android {
    namespace = "com.xau.rewards"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.xau.rewards"
        // 26 is the floor for adaptive icons and the notification
        // channels the farm's foreground service needs. The Rust .so is
        // built against the API 24 sysroot, which 26 satisfies.
        minSdk = 26
        targetSdk = 34
        versionCode = 2
        versionName = "1.0.1"

        // Only arm64 by default. Every Android phone shipped since 2019
        // is arm64, and each extra ABI adds several MB of Rust to the
        // APK for devices this app will never run on. The debug build
        // type adds x86_64 on top, for the emulator.
        ndk { abiFilters += "arm64-v8a" }
    }

    signingConfigs {
        if (keystoreProps.isNotEmpty()) {
            create("release") {
                storeFile = rootProject.file(keystoreProps.getProperty("storeFile"))
                storePassword = keystoreProps.getProperty("storePassword")
                keyAlias = keystoreProps.getProperty("keyAlias")
                keyPassword = keystoreProps.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        debug {
            applicationIdSuffix = ".debug"
            // x86_64 rides along in debug ONLY, so the app can be run in
            // an emulator at native speed. It never reaches a release
            // APK, where it would be pure weight.
            ndk { abiFilters += "x86_64" }
        }
        release {
            // R8 is OFF for now, deliberately. Minification changes what
            // actually ships, and this build has never been run on a
            // device — an R8-stripped class would surface as a crash on
            // launch that nobody could have caught first. Turn this on
            // together with a real device test, not before.
            isMinifyEnabled = false
            isShrinkResources = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
            signingConfig = signingConfigs.findByName("release")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    buildFeatures {
        compose = true
    }

    packaging {
        resources.excludes += "/META-INF/{AL2.0,LGPL2.1}"
        // The Rust library is already stripped by the release profile;
        // letting Gradle compress it would cost a copy at load time.
        jniLibs.useLegacyPackaging = false
    }
}

dependencies {
    val composeBom = platform("androidx.compose:compose-bom:2024.09.03")
    implementation(composeBom)
    androidTestImplementation(composeBom)

    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.activity:activity-compose:1.9.2")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.6")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.6")

    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-graphics")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.animation:animation")

    // Avatars come from Xbox as URLs; Coil handles fetch, cache and the
    // placeholder without pulling in a whole image framework.
    implementation("io.coil-kt:coil-compose:2.7.0")

    debugImplementation("androidx.compose.ui:ui-tooling")
    implementation("androidx.compose.ui:ui-tooling-preview")
}
