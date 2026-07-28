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
 * Copy the Rust library into jniLibs before it is packaged.
 *
 * jniLibs/ is a build output and therefore gitignored, so without this
 * a fresh clone silently produces an APK with no core in it — and worse,
 * an existing checkout packages whatever .so was left there last time.
 * A stale native library is invisible until the app misbehaves at
 * runtime, which is exactly the bug worth spending a task on.
 */
val syncNativeLib by tasks.registering(Copy::class) {
    val built = rootProject.file("../target-android/aarch64-linux-android/release/libxau_jni.so")
    from(built)
    into(layout.projectDirectory.dir("src/main/jniLibs/arm64-v8a"))
    onlyIf {
        if (!built.exists()) {
            logger.warn("libxau_jni.so not found — run build-android.cmd first")
        }
        built.exists()
    }
}

tasks.named("preBuild") { dependsOn(syncNativeLib) }

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
        versionCode = 1
        versionName = "1.0.0"

        // Only arm64. Every Android phone shipped since 2019 is arm64,
        // and each extra ABI adds ~12 MB of Rust to the APK for devices
        // this app will never run on.
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
