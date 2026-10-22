# The JNI symbols in libxau_jni.so are resolved by name at runtime
# (`Java_com_xau_rewards_core_Native_call`). R8 renaming this class or
# its methods would break the lookup with a linker error the moment the
# app touches the core — and only in release builds, where it is hardest
# to notice.
-keep class com.xau.rewards.core.Native { *; }

# Coil reflects over its own decoders.
-dontwarn coil.**

# The Windows sign-in page calls `XauLogin.postMessage` through a
# JavaScript interface, found by name at runtime: keep every
# @JavascriptInterface method, or the login silently never finishes.
-keepattributes JavascriptInterface
-keepclassmembers class * {
    @android.webkit.JavascriptInterface <methods>;
}
