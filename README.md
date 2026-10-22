# xau-mobile

Android app for **Microsoft Rewards on Xbox**. A cut-down fork of
[xau-rs](https://github.com/xdzleo/xau-rs) that does four things:

- **sign in** — as a Windows PC does: Microsoft's inline sign-in in a
  WebView, on a registered Windows device, then Xbox Live SISU. No Xbox
  app required, and the only sign-in Xbox accepts the presence farm from.
- **Rewards** — balance and claimable activities, for several accounts
  at once
- **claim** — on a tap, or automatically as soon as points appear
- **presence farm** — 8 fixed console titles, for up to 15 minutes

Everything else the desktop build carries is gone: the plan engine, the
bulk achievement scanner, process-memory token extraction, the
telemetry/events unlock path. What remains is the subset that is pure
HTTPS and therefore runs unchanged on a phone.

## Stack

Jetpack Compose on top of the original Rust core, reached through a JNI
bridge. There is no WebView in the app itself — only in the sign-in
flow, where Microsoft's own page has to render.

```
Compose UI (Kotlin) → Native.call(method, json) → xau-jni → xau-mobile → xau-core
```

`ARCHITECTURE.md` explains why the UI is native rather than Tauri, and
why the bridge exports one symbol instead of thirty. `DESIGN.md` is the
contract for how the app looks and behaves.

## Install

Grab the APK from
[Releases](https://github.com/xdzleo/xau-mobile/releases). It is
**arm64-only** — fine on any phone made since about 2019, but it will
not run on a standard x86_64 emulator.

Android 8.0 (API 26) or newer.

## Building

One command, on every CPU core:

```
build-apk.cmd             (or ./build-apk.sh from Git Bash)
build-apk.cmd install     ...and install it on the phone over adb
```

It builds the Rust core for arm64 with `cargo ndk`, then the app with
Gradle (which picks the fresh `.so` up by itself). Needed once, in their
default per-user places, all overridable by environment variable:

- JDK 21 under `~/.jdks` (`JAVA_HOME`)
- Android SDK with platform 34, build-tools and an NDK under
  `%LOCALAPPDATA%\Android\Sdk` (`ANDROID_HOME`, `ANDROID_NDK_HOME`)
- Gradle 8.9 under `~/.gradle-dist` (`GRADLE`)
- `cargo install cargo-ndk` and `rustup target add aarch64-linux-android`

Release signing reads `android/keystore.properties` and the key it
names, neither committed. Without them a release build comes out
unsigned. The `android` GitHub workflow builds the same APK in CI, signs
it from the repository secrets, and attaches it to `v*` releases.

## Status

Runs on a real device (moto g34 5G, Android 15): sign-in, Rewards and
the presence farm, with Xbox accepting the heartbeat for all 8 titles.

## Licence

GPL-3.0-or-later, inherited from the upstream project.
