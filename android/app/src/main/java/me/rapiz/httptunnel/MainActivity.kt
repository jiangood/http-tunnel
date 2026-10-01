package me.rapiz.httptunnel

import android.content.SharedPreferences
import android.os.Bundle
import android.widget.Button
import android.widget.EditText
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity

/**
 * The only screen of the client, which mirrors the arguments of the CLI:
 * the server, the name of the client and its token.
 */
class MainActivity : AppCompatActivity() {

    private lateinit var prefs: SharedPreferences
    private lateinit var remote: EditText
    private lateinit var name: EditText
    private lateinit var token: EditText
    private lateinit var status: TextView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        MobileClient.init()
        prefs = Preferences.of(this)

        remote = findViewById(R.id.remote)
        name = findViewById(R.id.name)
        token = findViewById(R.id.token)
        status = findViewById(R.id.status)

        remote.setText(prefs.getString(Preferences.KEY_REMOTE, ""))
        name.setText(prefs.getString(Preferences.KEY_NAME, ""))
        token.setText(prefs.getString(Preferences.KEY_TOKEN, ""))

        findViewById<Button>(R.id.start).setOnClickListener { start() }
        findViewById<Button>(R.id.stop).setOnClickListener { stop() }

        updateStatus(prefs.getBoolean(Preferences.KEY_AUTOSTART, false))
    }

    private fun start() {
        val remoteValue = remote.text.toString().trim()
        val nameValue = name.text.toString().trim()
        val tokenValue = token.text.toString().trim()

        if (remoteValue.isEmpty() || nameValue.isEmpty() || tokenValue.isEmpty()) {
            Toast.makeText(this, R.string.start_failed, Toast.LENGTH_LONG).show()
            return
        }

        // Remember the credentials, so that the boot receiver can start again
        prefs.edit()
            .putString(Preferences.KEY_REMOTE, remoteValue)
            .putString(Preferences.KEY_NAME, nameValue)
            .putString(Preferences.KEY_TOKEN, tokenValue)
            .putBoolean(Preferences.KEY_AUTOSTART, true)
            .apply()

        TunnelService.start(this, remoteValue, nameValue, tokenValue)
        updateStatus(true)
    }

    private fun stop() {
        prefs.edit().putBoolean(Preferences.KEY_AUTOSTART, false).apply()
        TunnelService.stop(this)
        updateStatus(false)
    }

    private fun updateStatus(running: Boolean) {
        status.text = getString(if (running) R.string.status_running else R.string.status_stopped)
    }
}
