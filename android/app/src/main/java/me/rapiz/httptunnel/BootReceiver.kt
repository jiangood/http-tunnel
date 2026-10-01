package me.rapiz.httptunnel

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.SharedPreferences

/**
 * Starts the tunnel after a reboot, if the switch was left on.
 *
 * The credentials are stored by [MainActivity] in the preferences of the app.
 * They are kept in clear text, which is what the CLI does with its arguments, so
 * [Context.MODE_PRIVATE] is the only protection here.
 */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Intent.ACTION_BOOT_COMPLETED) {
            return
        }

        val prefs: SharedPreferences = Preferences.of(context)
        if (!prefs.getBoolean(Preferences.KEY_AUTOSTART, false)) {
            return
        }

        val remote = prefs.getString(Preferences.KEY_REMOTE, null) ?: return
        val name = prefs.getString(Preferences.KEY_NAME, null) ?: return
        val token = prefs.getString(Preferences.KEY_TOKEN, null) ?: return

        TunnelService.start(context, remote, name, token)
    }
}

/** The preferences shared by the activity, the service and the boot receiver. */
object Preferences {
    const val KEY_REMOTE = "remote"
    const val KEY_NAME = "name"
    const val KEY_TOKEN = "token"
    const val KEY_AUTOSTART = "autostart"

    fun of(context: Context): SharedPreferences =
        context.getSharedPreferences("http-tunnel", Context.MODE_PRIVATE)
}
