mod cli;
mod config;
mod constants;
mod helper;
mod multi_map;
mod protocol;
mod transport;

#[cfg(feature = "client")]
mod client_env;

pub use cli::{ClientArgs, Command, NodeArgs, ServerArgs};
pub use config::{
    ClientConfig, NodeConfig, ServerConfig, ServerLimits, ServerProxyEntry, UpstreamConfig,
};

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

#[cfg(feature = "server")]
mod http;
#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
use server::run_server;

// A node is a server and a client in one process: it needs both roles.
#[cfg(all(feature = "server", feature = "client"))]
mod node;
#[cfg(all(feature = "server", feature = "client"))]
use node::run_node;

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
                debug!("{:?}", args);
                run_server(args, shutdown_rx).await
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
        #[allow(unused_variables)]
        Command::Node(args) => {
            #[cfg(not(all(feature = "server", feature = "client")))]
            crate::helper::feature_not_compile("node");

            #[cfg(all(feature = "server", feature = "client"))]
            {
                run_node(args, shutdown_rx).await
            }
        }
    }
}
