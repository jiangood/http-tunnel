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
use tracing::debug;

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "client")]
use client::run_client;

#[cfg(feature = "server")]
mod admin;
#[cfg(feature = "server")]
mod http;
#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
use server::run_server;

pub async fn run(args: Cli, shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
    // Raise `nofile` limit on linux and mac
    fdlimit::raise_fd_limit();

    match args.cmd {
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
