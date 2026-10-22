# Architecture — where everything lives

An Android app for Microsoft Rewards on Xbox: keep several accounts
signed in, see what each has earned and what is waiting, claim it, and
run a presence farm that earns more.

```
  Compose UI  (android/app, Kotlin)
       │  core/Native.kt — typed suspend functions
       ▼
  Native.call(method, argsJson) -> resultJson     ← one JNI symbol
       │
       ▼
  xau-jni     (cdylib)   dispatch + session registry + tokio runtime
       │
       ▼
  xau-mobile  (rlib)     login / rewards / farm — the only operations
       │
       ▼
  xau-core    (rlib)     SISU, request signing, Rewards + presence APIs
       │
       ▼
  msa-device  (rlib)     Windows-device MSA sign-in (vendored from xodus)
```

## The crates

| Crate | Role | Open when… |
|---|---|---|
| **`xau-core`** | All logic: Xbox HTTP, auth, request signing, rewards, presence. No UI. | changing behaviour |
| **`xau-mobile`** | The narrow surface the phone needs — sign in, read/claim Rewards, run the farm. Nothing else is reachable. | adding an operation |
| **`xau-jni`** | The bridge. Owns the tokio runtime and the live sessions. | changing what Kotlin can call |
| **`msa-device`** | Signing in as a Windows device: the MSA device account, RST2, the inline login. Vendored from [xodus](https://github.com/xodus-gaming/xodus) (GPL-3.0-or-later), the backend of the xodus-nexus Linux launcher. | Microsoft changes the Windows sign-in |

`xau-core` is inherited from the desktop project and carries far more
than this app uses (plan engine, bulk scanner, memory scanner). None of
it is reachable from `xau-mobile`, so the linker drops it.

## Why the UI is native, not a WebView

The first version of this app was Tauri: the same HTML/CSS/JS as the
desktop build, in a WebView. It worked, and it never felt like a phone
app — a WebView has no native scroll physics, no Material switch, no
haptics on a control, no real 120 Hz, and no genuine edge-to-edge
insets. Those are what separate an app that looks designed from one that
feels built, and no amount of CSS reaches them.

So the UI is Jetpack Compose and the Rust core is a shared library. The
Rust itself did not change: the same crates, compiled for
`aarch64-linux-android`.

## Why one JNI symbol

`xau-jni` exports exactly two symbols: `Native_init` and `Native_call`.
Everything crosses as `call(method, argsJson) -> resultJson`.

One `extern "system"` function per operation would mean a hand-written
signature, a hand-written Kotlin `external fun`, and a hand-written type
conversion for each — all of which fail at *runtime* when they drift
apart. Collapsing to one symbol puts every conversion in `serde_json`,
which does not drift, and confines `unsafe` to a single entry point that
catches unwinds (a Rust panic crossing into the JVM is undefined
behaviour).

The cost is that method names resolve at runtime. It is contained by
`dispatch` erroring on an unknown name, and by `Api` on the Kotlin side
wrapping each one in a typed function — so a rename breaks in one file.

## State

Rust owns the sessions, the balances and the farm handles.
`AppViewModel` caches the last answer so the UI has something to draw,
and forwards taps. It is authoritative for nothing.

Balances are `Long?`. Null means *never fetched* or *the last read
failed*, and renders as `—`. A row showing `0` for an account it never
asked about is a lie, and `optLong` cannot tell the two apart — hence
the explicit `isNull` checks in the decoder.

## Sign-in

No Xbox app required. The phone signs in **as a Windows PC does**,
because that is the only sign-in Xbox accepts the presence farm from:
`presence-heartbeat` answers 403 to a token whose device is anonymous
(the OAuth sign-in this app used before), whatever the client id, and
200 to one whose device is a registered Windows device. It is the
sign-in the xodus-nexus Linux launcher uses; the protocol code is
vendored from its backend into `crates/msa-device`.

1. **Once per install**, `windows_login::start` registers an MSA device
   account (`deviceaddcredential.srf`) and stores it in `accounts.json`.
2. The WebView loads Microsoft's **inline sign-in**
   (`InlineLogin.srf`) with the headers and user agent of Windows' web
   host. The page talks to its host through `window.external.notify`,
   wired by a document-start script to the `XauLogin` JavaScript
   interface; on `post.srf` the app hands Rust the page's `ServerData`
   (a DA token). Kotlin only forwards messages and carries out Rust's
   reply: run a script, open one more page, or finish.
3. Rust trades the DA token over RST2, signed with the device's key, for
   the user's long-lived **STS token**, stored per account.
4. Every xauth is then issued silently: RST2 tickets for the Xbox app
   (device and user), a device token with `AuthMethod: RPS`, and SISU.
   So these accounts outlive the 16-hour xauth: `restore` mints a fresh
   one on every launch.

The device token and SISU requests are signed with ECDSA P-256 over an
undocumented byte layout. That lives in `xau_core::oauth::signer` and is
the one part of this app that fails silently when it is wrong.

Accounts signed in the old way still load and work for Rewards; their
farm shows "Xbox recusou a presença (403)", and signing in again fixes
it.

## The farm and the foreground service

The farm runs on the tokio runtime inside the app process. Android
freezes a backgrounded process within minutes of screen-off, which would
stop a 15-minute presence farm without telling anyone — so `FarmService`
holds a foreground notification for exactly as long as a farm runs. The
service starts no work of its own.

## Design

`DESIGN.md` is the contract for how the app looks and behaves, and it is
enforced in code where possible: the brand green measures 3.30:1 on the
ground colour and so is never offered as a text colour, and `Space` is
the only spacing scale.

## Building

```bash
./build-apk.sh            # or build-apk.cmd; `install` also puts it on the phone
```

`cargo ndk` builds the core (it drives the NDK's clang directly, so the
NDK's batch-file wrappers that choke on `-Wl,--version-script=` never
come into it), and Gradle's `syncNativeLibs` copies the `.so` into
`jniLibs` itself. See README.md for the one-time toolchain setup.

Only `arm64-v8a` is built. Every Android phone shipped since 2019 is
arm64, and each extra ABI adds ~12 MB of Rust for devices this app will
never run on. This also means **the APK will not run on a standard
x86_64 emulator** — testing needs a real device or an arm64 image.
