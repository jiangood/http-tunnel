package me.rapiz.httptunnel

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat

/**
 * Keeps the client alive in the background.
 *
 * Without a foreground service, Android suspends the app shortly after it leaves
 * the foreground, and the tunnel drops. The service is only a container: the
 * client itself runs in the Rust library, and the JNI calls are made here so
 * that they aren't tied to the lifetime of the activity.
 */
class TunnelService : Service() {

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_START -> {
                val remote = intent.getStringExtra(EXTRA_REMOTE) ?: return START_NOT_STICKY
                val name = intent.getStringExtra(EXTRA_NAME) ?: return START_NOT_STICKY
                val token = intent.getStringExtra(EXTRA_TOKEN) ?: return START_NOT_STICKY

                startForeground(NOTIFICATION_ID, buildNotification())
                MobileClient.start(remote, name, token)
            }

            ACTION_STOP -> {
                MobileClient.stop()
                stopForeground(STOP_FOREGROUND_REMOVE)
                stopSelf()
            }
        }

        // The client reconnects on its own, so there is nothing to restart
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        MobileClient.stop()
        super.onDestroy()
    }

    private fun buildNotification(): Notification {
        ensureChannel()

        val contentIntent = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )

        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle(getString(R.string.notification_title))
            .setContentText(getString(R.string.notification_text))
            .setSmallIcon(android.R.drawable.stat_sys_upload)
            .setOngoing(true)
            .setContentIntent(contentIntent)
            .build()
    }

    private fun ensureChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
            return
        }

        val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val channel = NotificationChannel(
            CHANNEL_ID,
            getString(R.string.notification_channel_name),
            NotificationManager.IMPORTANCE_LOW,
        ).apply {
            description = getString(R.string.notification_channel_description)
        }
        manager.createNotificationChannel(channel)
    }

    companion object {
        private const val CHANNEL_ID = "http-tunnel"
        private const val NOTIFICATION_ID = 1

        const val ACTION_START = "me.rapiz.httptunnel.START"
        const val ACTION_STOP = "me.rapiz.httptunnel.STOP"

        const val EXTRA_REMOTE = "remote"
        const val EXTRA_NAME = "name"
        const val EXTRA_TOKEN = "token"

        fun start(context: Context, remote: String, name: String, token: String) {
            val intent = Intent(context, TunnelService::class.java).apply {
                action = ACTION_START
                putExtra(EXTRA_REMOTE, remote)
                putExtra(EXTRA_NAME, name)
                putExtra(EXTRA_TOKEN, token)
            }
            context.startForegroundService(intent)
        }

        fun stop(context: Context) {
            val intent = Intent(context, TunnelService::class.java).apply {
                action = ACTION_STOP
            }
            context.startService(intent)
        }
    }
}
