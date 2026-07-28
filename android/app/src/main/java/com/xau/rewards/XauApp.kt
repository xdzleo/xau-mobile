package com.xau.rewards

import android.app.Application
import com.xau.rewards.core.Native

/**
 * Starts the Rust core before any component can reach it.
 *
 * This lives in [Application] rather than in the activity because the
 * core must be initialised before the *first* call, and the activity is
 * not the only thing Android can start — a service restarted under
 * START_STICKY comes up without it.
 *
 * `filesDir` is the app's private storage. Rust cannot work it out on
 * Android: its directory crate falls back to `$HOME/.local/share`, and
 * an app process has no `$HOME`, so it resolves to `/` and every write
 * fails. This is the one thing only the Java side knows.
 */
class XauApp : Application() {
    override fun onCreate() {
        super.onCreate()
        Native.start(filesDir.absolutePath)
    }
}
