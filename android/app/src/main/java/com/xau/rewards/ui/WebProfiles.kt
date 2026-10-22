package com.xau.rewards.ui

import android.content.Context
import android.webkit.CookieManager
import android.webkit.WebView
import androidx.webkit.ProfileStore
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import java.util.UUID

/**
 * One isolated browser profile per account: its own cookies, cache, local
 * storage and service workers. One account's Microsoft session never
 * reaches another account's sign-in or Store page, and removing an
 * account deletes everything its pages stored.
 *
 * Nothing on screen changes. A sign-in runs in a fresh profile, which is
 * kept as that account's once the sign-in succeeds. Accounts added before
 * this existed, and WebViews too old for profiles, use the shared default
 * profile exactly as before.
 *
 * Every call touches WebView's profile store, so all of them run on the
 * main thread, which is where Compose calls them.
 */
object WebProfiles {
    private const val FILE = "xau_web_profiles"
    private const val PREFIX = "acct-"

    fun supported(): Boolean = WebViewFeature.isFeatureSupported(WebViewFeature.MULTI_PROFILE)

    private fun prefs(ctx: Context) = ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE)

    /** The profile an account's pages use; null means the default one. */
    fun profileOf(ctx: Context, xuid: String): String? =
        if (supported()) prefs(ctx).getString(xuid, null) else null

    /**
     * A new, empty profile for a sign-in in progress, after deleting the
     * leftovers of sign-ins that never finished. Null without support.
     */
    fun newSignIn(ctx: Context): String? {
        if (!supported()) return null
        sweep(ctx)
        return PREFIX + UUID.randomUUID()
    }

    /** Use [profile] in [view]. Call right after creating the view, before it loads. */
    fun apply(view: WebView, profile: String?) {
        if (profile == null || !supported()) return
        ProfileStore.getInstance().getOrCreateProfile(profile)
        WebViewCompat.setProfile(view, profile)
    }

    /** The cookie jar of [profile]; the shared one when it is null. */
    fun cookies(profile: String?): CookieManager =
        if (profile != null && supported()) {
            ProfileStore.getInstance().getOrCreateProfile(profile).cookieManager
        } else {
            CookieManager.getInstance()
        }

    /**
     * The sign-in in [profile] succeeded for [xuid]: it becomes that
     * account's. Signing in again as the same account replaces (and
     * deletes) the profile it had.
     */
    fun bind(ctx: Context, xuid: String, profile: String?) {
        if (profile == null || !supported()) return
        val old = prefs(ctx).getString(xuid, null)
        prefs(ctx).edit().putString(xuid, profile).apply()
        if (old != null && old != profile) delete(old)
    }

    /** The account is gone: delete everything its pages stored. */
    fun forget(ctx: Context, xuid: String) {
        if (!supported()) return
        val profile = prefs(ctx).getString(xuid, null) ?: return
        prefs(ctx).edit().remove(xuid).apply()
        delete(profile)
    }

    /** Sign-in profiles no account kept (cancelled or failed sign-ins). */
    private fun sweep(ctx: Context) {
        val kept = prefs(ctx).all.values.toSet()
        ProfileStore.getInstance().allProfileNames
            .filter { it.startsWith(PREFIX) && it !in kept }
            .forEach(::delete)
    }

    // A profile still held by a live WebView cannot be deleted; that one is
    // swept at the next sign-in instead.
    private fun delete(profile: String) {
        runCatching { ProfileStore.getInstance().deleteProfile(profile) }
    }
}
