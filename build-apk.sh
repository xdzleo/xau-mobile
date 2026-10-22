#!/usr/bin/env bash
# Build the APK on this machine: the Rust core for arm64 with the NDK,
# then Gradle, on every CPU core.
#
#   ./build-apk.sh           release APK
#   ./build-apk.sh install   ...then install it on the phone over adb
#
# Signed with the release key when android/keystore.properties exists
# (gitignored, next to the key it names), so the APK installs over the
# released app; without it Gradle leaves the release APK unsigned.
#
# Tools are found where they are installed by default and can be
# overridden: JAVA_HOME (latest under ~/.jdks), ANDROID_HOME
# (%LOCALAPPDATA%/Android/Sdk), ANDROID_NDK_HOME (latest NDK there),
# GRADLE (latest under ~/.gradle-dist).
set -euo pipefail
cd "$(dirname "$0")"

latest() { ls -d "$1"/*/ 2>/dev/null | sort -V | tail -1 | sed 's:/$::'; }
win() { if command -v cygpath >/dev/null; then cygpath -w "$1"; else echo "$1"; fi; }

LOCAL_APPDATA="${LOCALAPPDATA:-$HOME/AppData/Local}"
JAVA_HOME="${JAVA_HOME:-$(latest "$HOME/.jdks")}"
ANDROID_HOME="${ANDROID_HOME:-$(cygpath -u "$LOCAL_APPDATA" 2>/dev/null || echo "$LOCAL_APPDATA")/Android/Sdk}"
ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$(latest "$ANDROID_HOME/ndk")}"
GRADLE="${GRADLE:-$(latest "$HOME/.gradle-dist")/bin/gradle}"
JOBS="$(nproc)"

for p in "$JAVA_HOME" "$ANDROID_HOME" "$ANDROID_NDK_HOME"; do
  [ -d "$p" ] || { echo "missing: $p" >&2; exit 1; }
done
export JAVA_HOME="$(win "$JAVA_HOME")" ANDROID_HOME="$(win "$ANDROID_HOME")" ANDROID_NDK_HOME="$(win "$ANDROID_NDK_HOME")"
export PATH="$HOME/.cargo/bin:$PATH"

echo "== core (arm64, $JOBS jobs)"
# .cargo/config.toml pins an x86-64 CPU for the desktop build; an empty
# RUSTFLAGS takes precedence over it. Lands where syncNativeLibs looks:
# target-android/aarch64-linux-android/release/libxau_jni.so
RUSTFLAGS="" CARGO_TARGET_DIR=target-android \
  cargo ndk -t arm64-v8a --platform 24 build --release -p xau-jni -j "$JOBS"

echo "== apk"
(cd android && "$GRADLE" --parallel --max-workers="$JOBS" assembleRelease)

APK=android/app/build/outputs/apk/release/app-release.apk
[ -f "$APK" ] || APK=android/app/build/outputs/apk/release/app-release-unsigned.apk
ls -la "$APK"

if [ "${1:-}" = install ]; then
  ADB="${ADB:-$(win "$ANDROID_HOME")/platform-tools/adb}"
  "$ADB" install -r "$APK"
fi
