package io.unom.punktfunk

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.ServiceCompat
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.R as AppR

private const val TAG = "pf-keepalive"

/**
 * What the ongoing notification says about the running session. Built by the stream screen and
 * handed over whole, so a change of stage is one re-post rather than a set of setters.
 *
 * The two stamps are wall clock because that is what a notification's chronometer takes — the
 * platform re-bases it against `elapsedRealtime` itself. In the foreground the clock counts UP
 * from [startedAtWallMs]; while the app is away it counts DOWN to [deadlineWallMs], which is the
 * countdown the Apple client's Live Activity shows.
 */
data class StreamNote(
    val hostName: String,
    /** The launched title, when the session came off a library shelf; null for a plain connect. */
    val title: String?,
    /** e.g. `2560×1440 · 120 Hz · HEVC`. Empty until the mode is known. */
    val modeLine: String,
    val startedAtWallMs: Long,
    /** When the keep-alive gives up. Null while the app is in the foreground. */
    val deadlineWallMs: Long? = null,
)

/**
 * How long a session holds when the app leaves the screen, or null when it does not hold at all.
 *
 * One answer rather than two, because a keep-alive without a bound is a session the host can never
 * reclaim: it cannot tell a player who walked away from one who is watching. Never on a TV, where
 * the notification carrying the End action has nowhere to appear — a held session there would be
 * one nothing outside the app could stop.
 *
 * The minutes are clamped, not trusted: the picker offers 1/5/10/30, but the value is a plain
 * number in a settings document a console or a preset can write, and a zero would end the session
 * the instant it was backgrounded.
 */
fun keepAliveSpanMs(settings: Settings, isTv: Boolean): Long? {
    if (!settings.backgroundKeepAlive || isTv) return null
    return settings.backgroundTimeoutMinutes.coerceIn(1, 120) * 60_000L
}

/**
 * The foreground service behind a live session — Android's answer to the Apple client's Live
 * Activity, and the thing that makes a backgrounded stream possible at all: without it the OS
 * freezes the process, and the audio and the QUIC traffic stop with it.
 *
 * It runs for the whole session whenever the background keep-alive setting is on, not only once
 * the app is away: an app already in the background may not start a foreground service, so the
 * start has to happen while the stream screen is plainly on top. With the setting off there is no
 * service and no notification — backgrounding ends the session, so there would be nothing to keep
 * alive and nothing to show.
 */
class StreamKeepAliveService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_END) {
            onEnd?.invoke()
            return START_NOT_STICKY
        }
        val state = live ?: run {
            // Nothing to say: only reachable if the OS started us with no session behind it, and
            // a session cannot be resurrected from a notification.
            stopSelf()
            return START_NOT_STICKY
        }
        running = true
        val type = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK
        } else {
            0
        }
        runCatching { ServiceCompat.startForeground(this, NOTE_ID, build(this, state), type) }
            .onFailure { Log.w(TAG, "foreground start refused", it) }
        // START_NOT_STICKY: the session dies with the process, so a service the OS brought back
        // would post a notification for a stream that no longer exists.
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        running = false
        super.onDestroy()
    }

    companion object {
        private const val NOTE_ID = 1
        private const val CHANNEL_ID = "session"
        private const val ACTION_END = "io.unom.punktfunk.END_SESSION"

        /**
         * What the notification's End action runs. Set by the stream screen for the life of the
         * session and cleared on the way out; the service and the screen share one process, so
         * this is the whole of the plumbing.
         */
        @Volatile
        var onEnd: (() -> Unit)? = null

        @Volatile
        private var live: StreamNote? = null

        @Volatile
        private var running = false

        /** Put the notification up for a session that is starting. Safe to call twice. */
        fun start(context: Context, state: StreamNote) {
            live = state
            runCatching {
                context.startForegroundService(Intent(context, StreamKeepAliveService::class.java))
            }.onFailure { Log.w(TAG, "keep-alive service start refused", it) }
        }

        /**
         * Re-post with a new state — a mode line that arrived, or the switch to the countdown.
         * A no-op before [start] and after [stop], so a late stats tick cannot leave an orphan
         * notification standing over a finished session.
         */
        fun update(context: Context, state: StreamNote) {
            if (!running) return
            live = state
            runCatching {
                NotificationManagerCompat.from(context).notify(NOTE_ID, build(context, state))
            }.onFailure { Log.w(TAG, "notification update refused", it) }
        }

        /** Take the notification down and stop the service. Safe to call when neither is up. */
        fun stop(context: Context) {
            running = false
            live = null
            runCatching {
                context.stopService(Intent(context, StreamKeepAliveService::class.java))
            }.onFailure { Log.w(TAG, "keep-alive service stop refused", it) }
        }

        private fun build(context: Context, state: StreamNote): Notification {
            channel(context)
            val open = PendingIntent.getActivity(
                context,
                0,
                Intent(context, MainActivity::class.java)
                    .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP),
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
            )
            val end = PendingIntent.getService(
                context,
                1,
                Intent(context, StreamKeepAliveService::class.java).setAction(ACTION_END),
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
            )
            val away = state.deadlineWallMs != null
            return NotificationCompat.Builder(context, CHANNEL_ID)
                .setSmallIcon(AppR.drawable.ic_stat_stream)
                .setContentTitle(state.title ?: state.hostName)
                .setContentText(
                    when {
                        away -> context.getString(R.string.android_keepalive_background)
                        state.modeLine.isNotEmpty() -> state.modeLine
                        else -> context.getString(R.string.android_keepalive_streaming)
                    },
                )
                // The host name becomes a line of its own only once the title has taken the first.
                .setSubText(if (state.title != null) state.hostName else null)
                .setContentIntent(open)
                .addAction(
                    0,
                    context.getString(if (away) R.string.android_keepalive_end_now else R.string.android_keepalive_end_stream),
                    end,
                )
                .setWhen(state.deadlineWallMs ?: state.startedAtWallMs)
                .setUsesChronometer(true)
                .setChronometerCountDown(away)
                .setShowWhen(true)
                .setOngoing(true)
                .setSilent(true)
                .setCategory(NotificationCompat.CATEGORY_SERVICE)
                .setVisibility(NotificationCompat.VISIBILITY_PUBLIC)
                .build()
        }

        /**
         * The one channel, created on demand. Low importance: ongoing, never an interruption.
         * Declaring an existing channel again renames it and takes the new description, and
         * nothing else, so every call words it in the app's current language.
         */
        private fun channel(context: Context) {
            val mgr = context.getSystemService(NotificationManager::class.java) ?: return
            mgr.createNotificationChannel(
                NotificationChannel(
                    CHANNEL_ID, context.getString(R.string.android_keepalive_channel), NotificationManager.IMPORTANCE_LOW,
                ).apply {
                    description = context.getString(R.string.android_keepalive_channel_desc)
                    setShowBadge(false)
                },
            )
        }

        /**
         * The app's language changed: a channel that exists takes the new name and description.
         * An app that never held a session has none, and none is made.
         */
        fun renameChannel(context: Context) {
            val mgr = context.getSystemService(NotificationManager::class.java) ?: return
            if (mgr.getNotificationChannel(CHANNEL_ID) != null) channel(context)
        }
    }
}
