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
```

## The three crates

| Crate | Role | Open when… |
|---|---|---|
| **`xau-core`** | All logic: Xbox HTTP, auth, request signing, rewards, presence. No UI. | changing behaviour |
| **`xau-mobile`** | The narrow surface the phone needs — sign in, read/claim Rewards, run the farm. Nothing else is reachable. | adding an operation |
| **`xau-jni`** | The bridge. Owns the tokio runtime and the live sessions. | changing what Kotlin can call |

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

No Xbox app required. The user completes Microsoft's own OAuth page in a
WebView, and **every navigation is classified by Rust**, never by
matching `?code=` in Kotlin: only the exact redirect endpoint is
terminal, and treating any URL carrying a `code` as success would let a
page reached mid-flow hand over a code to exchange.

After the code comes back, Xbox Live still requires a device token and a
SISU authorisation, each signed with ECDSA P-256 over an undocumented
byte layout. That lives in `xau_core::oauth::signer` and is the one part
of this app that fails silently when it is wrong.

## The farm and the foreground service

The farm runs on the tokio runtime inside the app process. Android
freezes a backgrounded process within minutes of screen-off, which would
stop a 16-minute presence cycle without telling anyone — so `FarmService`
holds a foreground notification for exactly as long as a farm runs. The
service starts no work of its own.

## Design

`DESIGN.md` is the contract for how the app looks and behaves, and it is
enforced in code where possible: the brand green measures 3.30:1 on the
ground colour and so is never offered as a text colour, and `Space` is
the only spacing scale.

## Building

```bash
./build-android.cmd build -p xau-jni --target aarch64-linux-android --release
```

Copy `target-android/aarch64-linux-android/release/libxau_jni.so` into
`android/app/src/main/jniLibs/arm64-v8a/`, then:

```bash
./build-apk.cmd assembleDebug
```

Both wrapper scripts exist because neither toolchain is on `PATH`, and
both fail confusingly without them: the NDK's per-target `*-clang.cmd`
wrappers are batch files that choke on cargo's `-Wl,--version-script=`
argument, and `JAVA_HOME` on this machine points at a JRE with no
`javac`.

Only `arm64-v8a` is built. Every Android phone shipped since 2019 is
arm64, and each extra ABI adds ~12 MB of Rust for devices this app will
never run on. This also means **the APK will not run on a standard
x86_64 emulator** — testing needs a real device or an arm64 image.
