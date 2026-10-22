package com.xau.rewards.core

import android.content.Context

/**
 * The user's choices, kept on the device.
 *
 * Read by the farm's foreground service on every update, so a change made
 * in Configurações applies to a farm that is already running.
 */
class Prefs(context: Context) {
    private val sp = context.applicationContext.getSharedPreferences("settings", Context.MODE_PRIVATE)

    /**
     * The farm's ongoing notification with progress and time left. Off,
     * Android still requires one while the farm runs in the background;
     * it is then posted minimised, without a status-bar icon.
     */
    var ongoingNotification: Boolean
        get() = sp.getBoolean(KEY_ONGOING, true)
        set(v) = sp.edit().putBoolean(KEY_ONGOING, v).apply()

    /** A notification when an account completes its 15 minutes. */
    var notifyWhenDone: Boolean
        get() = sp.getBoolean(KEY_DONE, true)
        set(v) = sp.edit().putBoolean(KEY_DONE, v).apply()

    /** A notification when an account reaches a new Rewards level mid-farm. */
    var notifyLevelUp: Boolean
        get() = sp.getBoolean(KEY_LEVEL, true)
        set(v) = sp.edit().putBoolean(KEY_LEVEL, v).apply()

    private companion object {
        const val KEY_ONGOING = "ongoing_notification"
        const val KEY_DONE = "notify_when_done"
        const val KEY_LEVEL = "notify_level_up"
    }
}
