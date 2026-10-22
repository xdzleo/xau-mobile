package com.xau.rewards

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper

/**
 * Keeps the process alive while the farm is running.
 *
 * The farm itself lives in the Rust tokio runtime inside this process —
 * this service starts no work of its own. It exists because Android
 * freezes a backgrounded process within minutes of the screen going off,
 * and a presence farm of up to 15 minutes would simply stop earning
 * without the user ever being told. A foreground service is the only
 * supported way to say "this process is doing something the user asked
 * for", and the notification is the honest disclosure that comes with it.
 *
 * A farm ends by itself after 15 minutes, often with the screen off and
 * no UI polling, so the service also ends itself once the latest farm
 * started can no longer be running: the notification never outlives it.
 */
class FarmService : Service() {

    private val handler = Handler(Looper.getMainLooper())
    private val selfStop = Runnable { stopSelf() }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        handler.removeCallbacks(selfStop)
        super.onDestroy()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Every start (another account's farm included) pushes the end
        // back: each farm lasts FARM_MS, so the last one started ends last.
        handler.removeCallbacks(selfStop)
        handler.postDelayed(selfStop, FARM_MS + END_MARGIN_MS)
        // From API 34 the service type must be declared at start time as
        // well as in the manifest; the two-argument overload throws
        // MissingForegroundServiceTypeException there.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(
                NOTIFICATION_ID,
                buildNotification(),
                android.content.pm.ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
            )
        } else {
            startForeground(NOTIFICATION_ID, buildNotification())
        }
        // START_STICKY: if Android reclaims the process under memory
        // pressure, bring it back — a farm that silently died is the
        // failure mode this whole service exists to prevent.
        return START_STICKY
    }

    private fun buildNotification(): Notification {
        val manager = getSystemService(NotificationManager::class.java)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            manager.createNotificationChannel(
                NotificationChannel(
                    CHANNEL_ID,
                    getString(R.string.farm_channel),
                    // LOW: this is a persistent status, not an event.
                    // Anything higher would buzz the phone for a farm
                    // that is simply still running.
                    NotificationManager.IMPORTANCE_LOW,
                ).apply { setShowBadge(false) }
            )
        }

        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )

        return Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("Farm de presença ativo")
            .setContentText("8 títulos de console · até 15 min")
            .setSmallIcon(R.drawable.ic_xbox)
            .setContentIntent(open)
            .setOngoing(true)
            .build()
    }

    companion object {
        private const val CHANNEL_ID = "farm"
        private const val NOTIFICATION_ID = 1

        /** One farm run: `FARM_CYCLE` in xau-mobile (15 minutes, one cycle). */
        private const val FARM_MS = 15 * 60_000L

        /** Room for the last ping and the stop to land before the service goes. */
        private const val END_MARGIN_MS = 60_000L

        fun start(context: Context) {
            val intent = Intent(context, FarmService::class.java)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
            }
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, FarmService::class.java))
        }
    }
}
