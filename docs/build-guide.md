# Build Guide

This is for those who want to build `http-tunnel` themselves, possibly because the need of latest features or the minimal binary size.

## Build

To use default build settings, run:

```sh
cargo build --release
```

## Customize the Build

`http-tunnel` comes with some *crate features* that determine whether a certain feature will be compiled or not. Supported features can be checked out in `[features]` of [Cargo.toml](../Cargo.toml).

For example, to build `http-tunnel` as a client only:

```sh
cargo build --release --no-default-features --features client
```

## Minimalize the binary

1. Build with the `minimal` profile

The `release` build profile optimize for the program running time, not the binary size.

However, the `minimal` profile enables lots of optimization for the binary size to produce a much smaller binary.

For example, to build `http-tunnel` with `client` feature with the `minimal` profile:

```sh
cargo build --profile minimal --no-default-features --features client
```

2. `strip` and `upx`

The binary that step 1 produces can be even smaller, by using `strip` and `upx` to remove the symbols and compress the binary.

Like:

```sh
strip http-tunnel
upx --best --lzma http-tunnel
```

## Benchmarking

The HTTP path has a small benchmark harness in [`benches/`](../benches). It
starts an echo backend behind the NAT, a server and a client, then measures three
scenarios: many small keep-alive requests, many short-lived connections (the
per-connection setup cost, where the data channel pool shows up), and several
1 MiB bodies in flight at once (the large-transfer throughput).

```sh
cargo bench --bench throughput
```

It uses the same ports as the integration tests, so `cargo bench` and
`cargo test` must not run at the same time. `RUST_LOG=error` keeps the output
readable.

## Android

The client runs unchanged on Android, as it only makes outgoing TCP connections.
The Android client is a small Gradle project in [`android/`](../android) whose UI
calls the same client code through a C API (`--features mobile`, see
[`src/mobile.rs`](../src/mobile.rs) and the `http_tunnel_mobile` cdylib in
[`src/mobile_ffi.rs`](../src/mobile_ffi.rs)).

The prerequisites are the Android SDK, the Android NDK and the Rust targets:

```sh
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
```

With `ANDROID_NDK_HOME` (or `ANDROID_NDK_ROOT`) set, build the shared library and
copy it into the Gradle project. With no arguments, every installed target of the
three above is built; pass a target to build only one:

```sh
./android/build.sh                        # arm64-v8a, armeabi-v7a and x86_64
./android/build.sh x86_64-linux-android   # for an emulator
```

Then build the APK:

```sh
cd android
./gradlew assembleDebug        # android/app/build/outputs/apk/debug/app-debug.apk
./gradlew installDebug         # or install it on a connected device
```

`gradlew` is the Gradle wrapper, which isn't committed with the project. Run
`gradle wrapper` once in `android/` to create it, or build with an installed
Gradle of a compatible version. The build also needs `android/local.properties`
with `sdk.dir=<path to the Android SDK>`; see
[`android/local.properties.example`](../android/local.properties.example).
