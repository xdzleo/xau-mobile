# Contributing to xau-mobile

Read [`ARCHITECTURE.md`](ARCHITECTURE.md) before changing anything
structural, and [`DESIGN.md`](DESIGN.md) before changing anything the
user can see. Both exist because the decisions in them are easy to undo
by accident.

## There is no CI

Nothing runs on push. The local gates below are the only gates, so run
them yourself.

## Before you push

**Rust** — the workspace has strict lints (`correctness` and
`suspicious` are `deny`) and `rustfmt` is not optional:

```
build-msvc.cmd fmt --all -- --check
build-android.cmd +stable-x86_64-pc-windows-msvc clippy -p xau-jni -p xau-mobile --target aarch64-linux-android --release
build-msvc.cmd test -p xau-jni -p xau-mobile
```

`xau-jni` and `xau-mobile` must stay warning-free. `xau-core` is
inherited from the desktop project and still carries ~175 warnings — do
not add to them, and do not bulk-fix them inside a PR about something
else.

**The app**:

```
build-android.cmd build -p xau-jni --target aarch64-linux-android --release
build-apk.cmd assembleDebug
```

Gradle copies the freshly built `.so` into `jniLibs` for you. If you
skip the first step you package whatever was there last time, and a
stale native library is invisible until the app misbehaves at runtime.

The wrapper scripts are machine-specific and not all of them are
committed. Use `build-android.cmd` as the shape and point the paths at
your own SDK, NDK and JDK.

## Testing the UI

The APK is **arm64 only**, so a standard x86_64 emulator will not load
the core. Use a real device or an arm64 image.

Check anything visual the way `DESIGN.md` §10 describes: run it,
screenshot it, and critique the screenshot against the file. Layout
errors are visible in an image long before they are visible in code.

## What gets sent back

- **A shadow on a card.** `DESIGN.md` §3 reserves elevation for things
  that genuinely float. Separate with space and a hairline.
- **Brand green as text.** It measures 3.30:1 on the ground colour and
  fails WCAG AA. It is a fill, one element per screen.
- **A spacing value outside `Space`.** One scale, no exceptions.
- **A figure rendered as `0` when it was never fetched.** Null renders
  as `—`; a row claiming a balance it never asked about is a lie.
- **A real account identifier** in source, tests or fixtures. Use
  synthetic values of the same length and charset, so the validation
  tests keep exercising what they were written for.
- **An `#[allow(clippy::...)]` with no comment** saying why.
- **`println!` / `dbg!`** instead of `tracing`.

## What a good PR looks like

One change per PR — a bug fix and a feature in the same diff cannot be
evaluated. Explain *why*, not what the diff already shows; if a change
exists because something failed in a specific way, say what failed.
That sentence is usually the most valuable line in the message.

Bug fixes come with a regression test that fails against the pre-fix
code.

## Licence

GPL-3.0-or-later, inherited from
[Fumo-Unlockers/Xbox-Achievement-Unlocker](https://github.com/Fumo-Unlockers/Xbox-Achievement-Unlocker).
Contributions are accepted under the same licence.
