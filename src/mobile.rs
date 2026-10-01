//! The C API embedded by the Android client.
//!
//! The client of `http-tunnel` is a plain TCP client, so it runs unchanged on
//! Android. This module wraps it in a C API, which the Kotlin UI calls through
//! JNI (`TunnelService`), and which is also usable by any other embedder.
//!
//! The client takes no configuration file: it only needs the address of the
//! server, the name of the client as it is defined on the server, and its token.
//! These are given by the UI and passed here as NUL-terminated C strings.

use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use tokio::runtime::Runtime;
use tokio::sync::broadcast;
use tracing::info;

use crate::cli::ClientArgs;
use crate::client::run_client;

// Keep at most one runtime, so that the JNI layer can stop and start the client
// without leaking a runtime per start
static RUNTIME: OnceLock<Runtime> = OnceLock::new();

// Whether the client is currently running. Used to make start/stop idempotent,
// which is needed because the JNI layer may call `stop` before `start` or twice
static RUNNING: AtomicBool = AtomicBool::new(false);

// The shutdown sender of the running client, read by `http_tunnel_mobile_stop`.
// A poisoned mutex can't happen here (no code panics while holding it), and a
// poisoned mutex is still safe to recover from, so the lock is never unwrapped.
static SHUTDOWN_TX: Mutex<Option<broadcast::Sender<bool>>> = Mutex::new(None);

fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to build the tokio runtime")
    })
}

/// Read a NUL-terminated C string, or return `None` if the pointer is null or the
/// string isn't valid UTF-8
///
/// # Safety
/// `ptr` must either be null or point to a valid NUL-terminated string.
unsafe fn read_str(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    CStr::from_ptr(ptr).to_str().ok().map(|s| s.to_owned())
}

/// Initialize the Android logger. Causes no side effect on other platforms, and
/// is safe to call more than once.
///
/// The log level can be changed with `adb shell setprop log.tag.http-tunnel debug`.
#[no_mangle]
pub extern "C" fn http_tunnel_mobile_init() {
    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(tracing::Level::INFO)
            .with_tag("http-tunnel"),
    );
}

/// Start the client.
///
/// Returns 0 on success, -1 if the client is already running or a parameter is
/// missing or invalid. The client reconnects on its own, so a failure to reach
/// the server isn't reported here; the logs are the source of truth.
///
/// # Safety
/// `remote`, `name` and `token` must be null or point to valid NUL-terminated
/// strings that stay alive for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn http_tunnel_mobile_start(
    remote: *const c_char,
    name: *const c_char,
    token: *const c_char,
) -> i32 {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return -1;
    }

    let (Some(remote), Some(name), Some(token)) =
        (read_str(remote), read_str(name), read_str(token))
    else {
        RUNNING.store(false, Ordering::SeqCst);
        return -1;
    };

    if remote.is_empty() || name.is_empty() || token.is_empty() {
        RUNNING.store(false, Ordering::SeqCst);
        return -1;
    }

    let args = ClientArgs {
        remote,
        name,
        token,
    };
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);

    // A stop that arrives before the task starts is a plain shutdown of an idle
    // client, which exits immediately, so no signal has to be lost
    if let Ok(mut slot) = SHUTDOWN_TX.lock() {
        *slot = Some(shutdown_tx);
    }

    runtime().spawn(async move {
        info!("Starting the mobile client");
        if let Err(e) = run_client(args, shutdown_rx).await {
            tracing::error!("The mobile client stopped: {:#}", e);
        }
        RUNNING.store(false, Ordering::SeqCst);
    });

    0
}

/// Stop the client. Returns 0 on success, -1 if the client isn't running.
#[no_mangle]
pub extern "C" fn http_tunnel_mobile_stop() -> i32 {
    if !RUNNING.swap(false, Ordering::SeqCst) {
        return -1;
    }

    if let Some(tx) = SHUTDOWN_TX.lock().ok().and_then(|mut slot| slot.take()) {
        let _ = tx.send(true);
    }

    0
}
