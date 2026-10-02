# Android client

An Android app that runs the `http-tunnel` client, so that a phone behind the NAT
can serve the tunnels assigned to it by the server. It's a small Gradle
project whose UI is a thin wrapper: the client itself is the same Rust code as the
CLI, compiled into a shared library and called through JNI.

- [`app/src/main/java/me/rapiz/httptunnel/MobileClient.kt`](./app/src/main/java/me/rapiz/httptunnel/MobileClient.kt)
  is the JNI bridge, which maps to the C API of [`src/mobile.rs`](../src/mobile.rs).
  The C API is compiled by the `http_tunnel_mobile` cdylib
  ([`src/mobile_ffi.rs`](../src/mobile_ffi.rs)).
- [`build.sh`](./build.sh) builds the Rust library for each Android ABI with the
  NDK and copies it into `app/src/main/jniLibs`.
- [`.github/workflows/release.yml`](../.github/workflows/release.yml) builds the
  APK on every tag and attaches it to the release (see below).

The app takes the same three values as the CLI: the address of the server, the
name of the client as it is defined on the server, and its token.

## Build

See [the Android section of the build guide](../docs/build-guide.md#android).

## Releases

`.github/workflows/release.yml` builds the APK on every `v*` tag, next to the
CLI binaries. It is a **debug** APK (`assembleDebug`): a release APK would be
unsigned without a keystore. Set one up before publishing a release build, see
[the signing note](#signing) below.

The APK is attached to the draft release as `http-tunnel-android.apk`, and is
also uploaded as a workflow artifact. It is only attached on a tag; a manual run
(`workflow_dispatch`) just builds it.

## Use

1. On the server, the client is defined like any other one:

   ```toml
   [clients.phone]
   token = "use_a_secret_that_only_you_know"

   [clients.phone.tunnels]
   "my_service.example.com" = "127.0.0.1:8080" # A service listening on the phone
   ```

2. Open the app, fill in `example.com:2333`, `phone` and the token, and tap
   **Start**.

The client retries on its own when the server is unreachable or when the network
changes, so a failed first attempt isn't fatal. The log can be watched with:

```sh
adb logcat -s http-tunnel
```

The level defaults to `info` and follows `RUST_LOG`, like the CLI. It is read
when the client is started, so set it before tapping **Start**:

```sh
adb shell setprop debug.RUST_LOG debug
```

(If your build doesn't pick up the property, rebuild the library with a hardcoded
filter in `src/mobile.rs`.)

## Caveats

- **A foreground service** keeps the client alive. Android suspends an app
  shortly after it leaves the foreground, so the client runs in a foreground
  service with a permanent notification. Doze can still throttle the network of
  the device; the built-in retry recovers when the device wakes up.
- **The credentials are stored in clear text** in the private preferences of the
  app, so that the fields are prefilled on the next launch. `MODE_PRIVATE` is the
  only protection; don't use a token that is valuable on its own.
- **The tunnel doesn't start on its own after a reboot.** Open the app and tap
  **Start**; the fields are prefilled from the last start.
- **The `local_addr` values name the phone itself.** A tunnel of the client is
  reached at `local_addr`, as seen from the phone, so `127.0.0.1:<port>` is the
  app listening on the phone, not a service of the server. To reach services on
  other machines of the local network, use their addresses as seen from the
  phone.

## Signing

The APK that CI publishes is a **debug** APK, so that it can be installed
without a keystore. A debug APK is signed with the machine's debug key, which
differs between machines: an update can only be installed over a build that was
signed with the same key, and Android may refuse it otherwise.

To publish a signed release APK, add a keystore and the signing config:

1. Create the keystore and keep it out of the repository:

   ```sh
   keytool -genkeypair -v -keystore release.keystore -alias http-tunnel \
     -keyalg RSA -keysize 2048 -validity 10000
   ```

2. Add `signingConfigs` to `app/build.gradle.kts`, reading the path and the
   passwords from the environment, and point `buildTypes.release` at it.

3. Store the keystore and its passwords as repository secrets, decode them in
   the workflow, and switch the CI step to `assembleRelease`.

## Out of scope

As with the CLI, the app isn't a proxy for the traffic of the phone, and it
doesn't forward the phone's own web browsing. It only serves the tunnels
assigned to the client by the server.
