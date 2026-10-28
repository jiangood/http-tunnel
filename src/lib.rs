mod config;
mod config_env;
mod constants;
mod helper;
#[cfg(any(feature = "server", feature = "client"))]
mod protocol_v4;
mod transport;

pub use config::{
    Config, GeneratedConfigError, MaskedString, ServerLimits, ServerProxyEntry, UpstreamConfig,
    DEFAULT_SERVER_PORT,
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

// The management page and API manage the local proxies of the same universal
// process. They need an HTTP server, so they are a feature of their own.
#[cfg(all(feature = "server", feature = "management"))]
mod management;

#[cfg(all(feature = "server", feature = "management"))]
mod management_api;

// The router is a server and a client in one process: it needs both roles.
#[cfg(all(feature = "server", feature = "client"))]
mod node;

/// Load `ht.toml` and overlay the `HT_*` environment.
pub async fn load_config(path: &Path) -> Result<Config> {
    config_env::load(path).await
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
