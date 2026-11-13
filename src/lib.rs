mod config;
mod constants;
mod helper;
#[cfg(any(feature = "server", feature = "client"))]
mod protocol_v4;
#[cfg(any(
    all(feature = "server", feature = "client"),
    all(feature = "server", feature = "management")
))]
mod reload;
mod transport;

pub use config::{
    Config, MaskedString, ServerConfig, ServerLimits, ServerProxyEntry, UpstreamConfig,
    DEFAULT_LISTEN_PORT,
};
pub use helper::detect_local_ipv4;

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

/// Load `ht.toml`. The file is the only source of configuration.
pub async fn load_config(path: &Path) -> Result<Config> {
    Config::from_file(path).await
}

/// Run the universal process from an already-loaded configuration.
///
/// A configuration loaded from a file keeps watching it: a valid change is
/// applied without exiting the process (see `docs/internals.md`). A
/// configuration built in memory has no file to watch.
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
        node::run(config, shutdown_rx).await
    }
}
