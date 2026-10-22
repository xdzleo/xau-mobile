package com.xau.rewards

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.IBinder
import com.xau.rewards.core.Api
import com.xau.rewards.core.FarmRun
import com.xau.rewards.core.Prefs
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

/**
 * Keeps the process alive while a farm is running, and reports on it.
 *
 * The farm itself lives in the Rust tokio runtime inside this process —
 * this service starts no work of its own. It exists because Android
 * freezes a backgrounded process within minutes of the screen going off,
 * and a 15-minute presence farm would simply stop earning without the
 * user ever being told. A foreground service is the supported way to say
 * "this process is doing something the user asked for".
 *
 * While it runs it reads the farms from memory every few seconds (no
 * request is made): the ongoing notification shows the account, a
 * progress bar and the time left, and an account that completes its 15
 * minutes gets a notification of its own. When no farm is left running
 * the service ends itself, so the notification never outlives the farm.
 */
class FarmService : Service() {

    private var scope: CoroutineScope? = null
    private val prefs by lazy { Prefs(this) }

    /** Accounts seen farming on the previous read. */
    private val running = mutableSetOf<String>()

    /** Last Rewards level read per account; the first read is the baseline. */
    private val levels = mutableMapOf<String, Long>()

    /** Accounts that just finished: their level is read once more, then dropped. */
    private val finishing = mutableSetOf<String>()

    private val names = mutableMapOf<String, String>()

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        scope?.cancel()
        scope = null
        super.onDestroy()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        createChannels()
        // From API 34 the service type must be declared at start time as
        // well as in the manifest; the two-argument overload throws
        // MissingForegroundServiceTypeException there.
        val first = ongoing("Iniciando o farm…", null)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(
                NOTIFICATION_ID,
                first,
                android.content.pm.ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
            )
        } else {
            startForeground(NOTIFICATION_ID, first)
        }
        if (scope == null) {
            scope = CoroutineScope(SupervisorJob() + Dispatchers.Main).also {
                it.launch { watch() }
                it.launch { watchLevels() }
            }
        }
        // START_STICKY: if Android reclaims the process under memory
        // pressure, bring it back — a farm that silently died is the
        // failure mode this whole service exists to prevent.
        return START_STICKY
    }

    private suspend fun CoroutineScope.watch() {
        // A farm is started right before the service, but give the core a
        // moment so the first read already sees it.
        delay(1_500)
        var idleReads = 0
        while (isActive) {
            val farms = runCatching { Api.farmSummary() }.getOrNull()
            if (farms != null) {
                for (f in farms) {
                    f.gamertag?.let { names[f.xuid] = it }
                    if (f.xuid in running && f.completed) {
                        finishing += f.xuid
                        if (prefs.notifyWhenDone) notifyCompleted(f)
                    }
                }
                running.clear()
                farms.filter { it.running }.forEach { running += it.xuid }

                val live = farms.filter { it.running }
                if (live.isEmpty()) {
                    // Twice in a row, so a farm being restarted between
                    // two reads does not take the service down.
                    if (++idleReads >= 2) {
                        // The last minutes of a farm are the likeliest to
                        // tip a level over; read once more before leaving.
                        checkLevels()
                        stopSelf()
                        return
                    }
                } else {
                    idleReads = 0
                    manager().notify(NOTIFICATION_ID, progress(live))
                }
            }
            delay(POLL_MS)
        }
    }

    /**
     * Reads the Rewards level of every farming account once a minute and
     * says so the moment one goes up. This one does make a request (one
     * `programInfo` per account), hence the slower pace than [watch].
     */
    private suspend fun CoroutineScope.watchLevels() {
        delay(2_000)
        while (isActive) {
            checkLevels()
            delay(LEVEL_POLL_MS)
        }
    }

    private suspend fun checkLevels() {
        if (!prefs.notifyLevelUp) return
        val ids = running + finishing
        finishing.clear()
        for (xuid in ids) {
            val now = runCatching { Api.rewardsLevel(xuid) }.getOrNull() ?: continue
            val before = levels.put(xuid, now)
            if (before != null && now > before) notifyLevelUp(xuid, now)
        }
        // A finished account had its last read above; a later farm on it
        // starts a fresh baseline.
        levels.keys.retainAll(running)
    }

    private fun notifyLevelUp(xuid: String, level: Long) {
        val who = names[xuid] ?: "Uma conta"
        val n = Notification.Builder(this, LEVEL_CHANNEL_ID)
            .setContentTitle("$who subiu para o nível $level")
            .setContentText("Nível $level no Microsoft Rewards.")
            .setSmallIcon(R.drawable.ic_xbox)
            .setContentIntent(openApp())
            .setAutoCancel(true)
            .build()
        manager().notify(LEVEL_ID_BASE + (xuid.hashCode() and 0xFFFF), n)
    }

    private fun progress(live: List<FarmRun>): Notification {
        // The account closest to finishing leads the notification.
        val lead = live.maxByOrNull { it.elapsed.toDouble() / it.total.coerceAtLeast(1) }!!
        val left = (lead.total - lead.elapsed).coerceAtLeast(0)
        val who = lead.gamertag ?: "Conta"
        val title = if (live.size == 1) "$who · farm ativo" else "${live.size} contas farmando"
        val text = "${clock(left)} restantes · 8 jogos" +
            if (lead.presence == "denied" || lead.presence == "rejected") " · Xbox recusou a presença" else ""
        return ongoing(title, text, lead.elapsed.toInt(), lead.total.toInt())
    }

    private fun ongoing(title: String, text: String?, done: Int = 0, total: Int = 0): Notification =
        if (!prefs.ongoingNotification) quiet() else Notification.Builder(this, CHANNEL_ID)
            .setContentTitle(title)
            .apply { if (text != null) setContentText(text) }
            .setSmallIcon(R.drawable.ic_xbox)
            .setContentIntent(openApp())
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setProgress(total, done, total == 0)
            .build()

    /**
     * What Android still requires while a farm runs in the background with
     * the ongoing notification switched off: minimised, no progress, and
     * no status-bar icon (MIN channel).
     */
    private fun quiet(): Notification =
        Notification.Builder(this, QUIET_CHANNEL_ID)
            .setContentTitle("Farm em segundo plano")
            .setSmallIcon(R.drawable.ic_xbox)
            .setContentIntent(openApp())
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .build()

    private fun notifyCompleted(f: FarmRun) {
        val who = f.gamertag ?: "Uma conta"
        val n = Notification.Builder(this, DONE_CHANNEL_ID)
            .setContentTitle("$who completou o farm")
            .setContentText("15 minutos em 8 jogos de console concluídos.")
            .setSmallIcon(R.drawable.ic_check)
            .setContentIntent(openApp())
            .setAutoCancel(true)
            .build()
        // One notification per account, so two finishing together both show.
        manager().notify(DONE_ID_BASE + (f.xuid.hashCode() and 0xFFFF), n)
    }

    private fun openApp(): PendingIntent = PendingIntent.getActivity(
        this,
        0,
        Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
        PendingIntent.FLAG_IMMUTABLE,
    )

    private fun manager(): NotificationManager = getSystemService(NotificationManager::class.java)

    private fun createChannels() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        manager().createNotificationChannel(
            NotificationChannel(
                CHANNEL_ID,
                getString(R.string.farm_channel),
                // LOW: a persistent status, not an event. Anything higher
                // would buzz the phone for a farm that is simply running.
                NotificationManager.IMPORTANCE_LOW,
            ).apply { setShowBadge(false) },
        )
        manager().createNotificationChannel(
            NotificationChannel(
                QUIET_CHANNEL_ID,
                getString(R.string.farm_quiet_channel),
                NotificationManager.IMPORTANCE_MIN,
            ).apply { setShowBadge(false) },
        )
        manager().createNotificationChannel(
            NotificationChannel(
                DONE_CHANNEL_ID,
                getString(R.string.farm_done_channel),
                // DEFAULT: this one IS an event the user is waiting for.
                NotificationManager.IMPORTANCE_DEFAULT,
            ),
        )
        manager().createNotificationChannel(
            NotificationChannel(
                LEVEL_CHANNEL_ID,
                getString(R.string.level_up_channel),
                NotificationManager.IMPORTANCE_DEFAULT,
            ),
        )
    }

    companion object {
        private const val CHANNEL_ID = "farm"
        private const val DONE_CHANNEL_ID = "farm_done"
        private const val QUIET_CHANNEL_ID = "farm_quiet"
        private const val NOTIFICATION_ID = 1
        private const val DONE_ID_BASE = 1_000
        private const val POLL_MS = 5_000L
        private const val LEVEL_CHANNEL_ID = "level_up"
        private const val LEVEL_ID_BASE = 2_000
        private const val LEVEL_POLL_MS = 60_000L

        private fun clock(seconds: Long): String = "%d:%02d".format(seconds / 60, seconds % 60)

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
