package me.rapiz.httptunnel

/**
 * The JNI bridge to the `http-tunnel` client, which is compiled into the shared
 * library by Cargo (`--features mobile`).
 *
 * Every parameter is a plain string: the client of `http-tunnel` takes no
 * configuration file, it only needs the address of the server, the name of the
 * client as it is defined on the server, and its token.
 */
object MobileClient {
    /** Load the shared library and initialize the Android logger. */
    fun init() {
        System.loadLibrary("http_tunnel")
        nativeInit()
    }

    /**
     * Start the client. Returns whether it was started; it fails when the client
     * is already running, or when a parameter is missing.
     *
     * The call returns before the connection is established, since the client
     * keeps retrying on its own. `adb logcat -s http-tunnel` shows the progress.
     */
    fun start(remote: String, name: String, token: String): Boolean =
        nativeStart(remote, name, token) == 0

    /** Stop the client, if it is running. */
    fun stop(): Boolean = nativeStop() == 0

    private external fun nativeInit()
    private external fun nativeStart(remote: String, name: String, token: String): Int
    private external fun nativeStop(): Int
}
