# xau-mobile

Android app for **Microsoft Rewards on Xbox**. A cut-down fork of
[xau-rs](https://github.com/xdzleo/xau-rs) that does four things:

- **sign in** — Microsoft OAuth in a WebView, then Xbox Live SISU. No
  Xbox app required.
- **Rewards** — balance and claimable activities, for several accounts
  at once
- **claim** — on a tap, or automatically as soon as points appear
- **presence farm** — 8 fixed console titles, 16-minute cycles

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

Two steps, because the Rust core and the app use entirely separate
toolchains.

**1. The core** — needs the Android SDK with NDK 26.3.11579264 and the
`aarch64-linux-android` Rust target:

```
build-android.cmd build -p xau-jni --target aarch64-linux-android --release
```

**2. The app** — Gradle picks up the freshly built `.so` automatically:

```
build-apk.cmd assembleRelease
```

Both wrapper scripts are machine-specific and set up toolchains that are
not on `PATH`. Without them the failures are misleading: the NDK's
per-target `*-clang.cmd` wrappers are batch files that choke on cargo's
`-Wl,--version-script=` argument, and `JAVA_HOME` on the author's
machine points at a JRE with no `javac`.

Release signing reads `android/keystore.properties`, which is not
committed. Without it a release build simply comes out unsigned.

## Status

Builds, links, and packages. The Rust bridge is unit-tested and the APK
has been verified to contain the native library, the bundled font and a
launchable activity.

**The UI has not been run on a device yet** — no screenshot review pass
has happened, so treat the visual result as unverified.

## Licence

GPL-3.0-or-later, inherited from the upstream project.
