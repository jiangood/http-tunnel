mod cli;
mod config;
mod constants;
mod helper;
mod multi_map;
mod protocol;
mod transport;

pub use cli::{Cli, ClientArgs, Command, ServerArgs};
pub use config::ServerConfig;

use anyhow::Result;
use tokio::sync::broadcast;
#[cfg(feature = "server")]
use tracing::debug;

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "client")]
use client::run_client;

// The administration API of the client. It needs an HTTP server, so it's a
// feature of its own, kept out of a minimal client build
#[cfg(all(feature = "client", feature = "client-api"))]
mod client_api;

// The C API embedded by the Android client (see `android/`). It reuses the
// client, so it can only be compiled together with it, and it is exposed for the
// `http_tunnel_mobile` cdylib (`src/mobile_ffi.rs`) to re-export.
#[cfg(all(feature = "mobile", feature = "client"))]
pub mod mobile;

#[cfg(feature = "server")]
mod admin;
#[cfg(feature = "server")]
mod http;
#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
use server::run_server;

#[allow(unused_variables)]
pub async fn run(cmd: Command, shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
    // Raise `nofile` limit on linux and mac
    #[cfg(any(feature = "server", feature = "client"))]
    fdlimit::raise_fd_limit();

    match cmd {
        #[allow(unused_variables)]
        Command::Server(args) => {
            #[cfg(not(feature = "server"))]
            crate::helper::feature_not_compile("server");

            #[cfg(feature = "server")]
            {
                let config = crate::config::ServerConfig::from_file(&args.config_path).await?;
                debug!("{:?}", config);
                run_server(config, args.config_path.clone(), shutdown_rx).await
            }
        }
        #[allow(unused_variables)]
        Command::Client(args) => {
            #[cfg(not(feature = "client"))]
            crate::helper::feature_not_compile("client");

            #[cfg(feature = "client")]
            {
                run_client(args, shutdown_rx).await
            }
        }
    }
}
