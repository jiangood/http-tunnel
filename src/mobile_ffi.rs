//! The shared library embedded by the Android client.
//!
//! It is a thin `cdylib` wrapper: the C API itself lives in the library
//! (`http_tunnel::mobile`), and this target only re-exports it, so that the
//! `cdylib` artifacts of the shared library don't collide with the artifacts of
//! the crate's integration tests.
//!
//! It is built by `android/build.sh` as
//! `cargo build --release --features mobile --example http_tunnel_mobile`, which
//! yields the `libhttp_tunnel_mobile.so` that the Kotlin UI loads through JNI.

pub use http_tunnel::mobile::*;
