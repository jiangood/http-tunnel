mod client_env;
mod config;
mod constants;
mod helper;
#[cfg(any(feature = "server", feature = "client"))]
mod protocol_v4;
mod transport;

pub use config::{
    Config, MaskedString, ServerLimits, ServerProxyEntry, UpstreamConfig, DEFAULT_SERVER_PORT,
};

use anyhow::Result;
use std::path::Path;
use tokio::sync::broadcast;
#[cfg(feature = "server")]
use tracing::debug;

#[cfg(feature = "server")]
mod http;
#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
mod session_server;

#[cfg(feature = "client")]
mod session_client;

// The administration API manages the local proxies of the same universal
// process. It needs an HTTP server, so it is a feature of its own.
#[cfg(all(feature = "server", feature = "client-api"))]
mod client;

#[cfg(all(feature = "server", feature = "client-api"))]
mod client_api;

// The router is a server and a client in one process: it needs both roles.
#[cfg(all(feature = "server", feature = "client"))]
mod node;

/// Load `ht.toml` and overlay the `HT_*` (and legacy `HTC_*`) environment.
pub async fn load_config(path: &Path) -> Result<Config> {
    client_env::load(path).await
}

/// Run the universal process from an already-loaded configuration.
#[allow(unused_variables)]
pub async fn run(config: Config, shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
    // Raise `nofile` limit on linux and mac
    #[cfg(any(feature = "server", feature = "client"))]
    fdlimit::raise_fd_limit();

    #[cfg(not(all(feature = "server", feature = "client")))]
    crate::helper::feature_not_compile("node");

    #[cfg(all(feature = "server", feature = "client"))]
    {
        debug!(?config, "Starting the universal process");
        node::run_router(config, shutdown_rx).await
    }
}
