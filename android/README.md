# Android client

An Android app that runs the `http-tunnel` client, so that a phone behind the NAT
can serve the HTTP services assigned to it by the server. It's a small Gradle
project whose UI is a thin wrapper: the client itself is the same Rust code as the
CLI, compiled into a shared library and called through JNI.

- [`app/src/main/java/me/rapiz/httptunnel/MobileClient.kt`](./app/src/main/java/me/rapiz/httptunnel/MobileClient.kt)
  is the JNI bridge, which maps to the C API of [`src/mobile.rs`](../src/mobile.rs).
- [`build.sh`](./build.sh) builds the Rust library for each Android ABI with the
  NDK and copies it into `app/src/main/jniLibs`.

The app takes the same three values as the CLI: the address of the server, the
name of the client as it is defined on the server, and its token.

## Build

See [the Android section of the build guide](../docs/build-guide.md#android).

## Use

1. On the server, the client is defined like any other one:

   ```toml
   [clients.phone]
   token = "use_a_secret_that_only_you_know"

   [clients.phone.services.my_service]
   hosts = ["my_service.example.com"]
   local_addr = "127.0.0.1:8080" # A service listening on the phone
   ```

2. Open the app, fill in `example.com:2333`, `phone` and the token, and tap
   **Start**.

The client retries on its own when the server is unreachable or when the network
changes, so a failed first attempt isn't fatal. The log can be watched with:

```sh
adb logcat -s http-tunnel
```

The same value of the log level as the CLI applies through a system property:

```sh
adb shell setprop log.tag.http-tunnel debug
```

## Caveats

- **A foreground service** keeps the client alive. Android suspends an app
  shortly after it leaves the foreground, so the client runs in a foreground
  service with a permanent notification. Doze can still throttle the network of
  the device; the built-in retry recovers when the device wakes up.
- **The credentials are stored in clear text** in the private preferences of the
  app, so that the tunnel can start again after a reboot. `MODE_PRIVATE` is the
  only protection; don't use a token that is valuable on its own.
- **The `local_addr` values name the phone itself.** A service of the client is
  reached at `local_addr`, as seen from the phone, so `127.0.0.1:<port>` is the
  app listening on the phone, not a service of the server. To reach services on
  other machines of the local network, use their addresses as seen from the
  phone.

## Out of scope

As with the CLI, the app isn't a proxy for the traffic of the phone, and it
doesn't forward the phone's own web browsing. It only serves the services
assigned to the client by the server.
