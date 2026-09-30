use std::path::PathBuf;

use anyhow::Result;
use tokio::{
    io,
    net::{TcpListener, ToSocketAddrs},
    sync::broadcast,
};

pub async fn run_rathole_server(
    config_path: &str,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let cli = rathole::Cli {
        cmd: rathole::Command::Server(rathole::ServerArgs {
            config_path: PathBuf::from(config_path),
        }),
    };
    rathole::run(cli, shutdown_rx).await
}

pub async fn run_rathole_client(
    name: &str,
    remote: &str,
    token: &str,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let cli = rathole::Cli {
        cmd: rathole::Command::Client(rathole::ClientArgs {
            name: name.to_string(),
            remote: remote.to_string(),
            token: token.to_string(),
        }),
    };
    rathole::run(cli, shutdown_rx).await
}

pub mod tcp {
    use super::*;

    pub async fn echo_server<A: ToSocketAddrs>(addr: A) -> Result<()> {
        let l = TcpListener::bind(addr).await?;

        loop {
            let (conn, _addr) = l.accept().await?;
            tokio::spawn(async move {
                let _ = echo(conn).await;
            });
        }
    }

    async fn echo(conn: tokio::net::TcpStream) -> Result<()> {
        let (mut rd, mut wr) = conn.into_split();
        io::copy(&mut rd, &mut wr).await?;

        Ok(())
    }
}
