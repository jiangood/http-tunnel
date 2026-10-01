#!/usr/bin/env bash
# Build the Rust client for Android with the NDK and copy the shared library into
# the Gradle project, so that `./gradlew assembleDebug` picks it up.
#
# Prerequisites:
#   * The Rust targets:
#       rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
#   * The Android NDK, found through ANDROID_NDK_HOME or ANDROID_NDK_ROOT
#
# Usage:
#   ./android/build.sh                        # every installed target of the three
#   ./android/build.sh aarch64-linux-android   # only one, for a device (arm64-v8a)
#   ./android/build.sh x86_64-linux-android    # only one, for an emulator
#
# A target whose Rust target isn't installed is skipped with a hint.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
JNI_LIBS="$ROOT/android/app/src/main/jniLibs"

NDK="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
if [[ -z "$NDK" ]]; then
    echo "ANDROID_NDK_HOME or ANDROID_NDK_ROOT must point to the Android NDK" >&2
    exit 1
fi

# The NDK ships the clang wrappers of the API level in a versioned directory
HOST_TAG=""
case "$(uname -s)" in
    Linux) HOST_TAG="linux-x86_64" ;;
    Darwin) HOST_TAG="darwin-x86_64" ;;
    MINGW* | MSYS* | CYGWIN*) HOST_TAG="windows-x86_64" ;;
    *)
        echo "Unsupported host: $(uname -s)" >&2
        exit 1
        ;;
esac

TOOLCHAIN="$NDK/toolchains/llvm/prebuilt/$HOST_TAG"
if [[ ! -d "$TOOLCHAIN" ]]; then
    echo "The NDK toolchain was not found at $TOOLCHAIN" >&2
    exit 1
fi

# The API level of minSdk (26) of the Android project
API_LEVEL=26

targets=("$@")
if [[ ${#targets[@]} -eq 0 ]]; then
    targets=(
        aarch64-linux-android
        armv7-linux-androideabi
        x86_64-linux-android
    )
fi

for target in "${targets[@]}"; do
    case "$target" in
        aarch64-linux-android)
            abi="arm64-v8a"
            cc="$TOOLCHAIN/bin/aarch64-linux-android$API_LEVEL-clang"
            linker="$cc"
            ;;
        armv7-linux-androideabi)
            abi="armeabi-v7a"
            cc="$TOOLCHAIN/bin/armv7a-linux-androideabi$API_LEVEL-clang"
            linker="$cc"
            ;;
        x86_64-linux-android)
            abi="x86_64"
            cc="$TOOLCHAIN/bin/x86_64-linux-android$API_LEVEL-clang"
            linker="$cc"
            ;;
        i686-linux-android)
            abi="x86"
            cc="$TOOLCHAIN/bin/i686-linux-android$API_LEVEL-clang"
            linker="$cc"
            ;;
        *)
            echo "Unknown target: $target" >&2
            exit 1
            ;;
    esac

    if ! rustup target list --installed | grep -qx "$target"; then
        echo "The Rust target $target is not installed. Skipping."
        echo "  rustup target add $target"
        continue
    fi

    if [[ ! -x "$cc" ]]; then
        echo "The compiler $cc was not found. Skipping $target."
        continue
    fi

    echo "Building $target ($abi)"
    (
        cd "$ROOT"
        export "CC_$target=$cc"
        export "CARGO_TARGET_$(echo "$target" | tr 'a-z-' 'A-Z_')_LINKER=$linker"
        # The client plus the JNI cdylib (`http_tunnel_mobile`), never the server
        cargo build --release --no-default-features --features client,mobile \
            --example http_tunnel_mobile --target "$target"
    )

    # The example yields `libhttp_tunnel_mobile.so`. The JNI layer loads the
    # library as `http_tunnel`, so it is copied under a fixed name.
    mkdir -p "$JNI_LIBS/$abi"
    cp "$ROOT/target/$target/release/examples/libhttp_tunnel_mobile.so" \
        "$JNI_LIBS/$abi/libhttp_tunnel.so"
    echo "Copied to $JNI_LIBS/$abi/libhttp_tunnel.so"
done
