package me.rapiz.httptunnel

import android.content.Context
import android.content.SharedPreferences

/**
 * The preferences of the app: the credentials of the last start, so that the
 * fields are prefilled on the next launch, and whether the tunnel was running.
 */
object Preferences {
    const val KEY_REMOTE = "remote"
    const val KEY_NAME = "name"
    const val KEY_TOKEN = "token"
    const val KEY_RUNNING = "running"

    fun of(context: Context): SharedPreferences =
        context.getSharedPreferences("http-tunnel", Context.MODE_PRIVATE)
}
