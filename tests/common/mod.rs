use std::path::PathBuf;

use anyhow::Result;
use tokio::{
    io,
    net::{TcpListener, ToSocketAddrs},
    sync::broadcast,
};

pub async fn run_http_tunnel_server(
    http_port: u16,
    control_host: &str,
    token: &str,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    run_http_tunnel_server_with_limits(
        http_port,
        control_host,
        token,
        http_tunnel::ServerLimits::default(),
        shutdown_rx,
    )
    .await
}

pub async fn run_http_tunnel_server_with_limits(
    http_port: u16,
    control_host: &str,
    token: &str,
    limits: http_tunnel::ServerLimits,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let cmd = http_tunnel::Command::Server(http_tunnel::ServerArgs {
        http_port,
        control_host: control_host.to_string(),
        token: token.to_string(),
        proxies: Vec::new(),
        limits,
    });
    http_tunnel::run(cmd, shutdown_rx).await
}

/// Run a client from its configuration file. The file carries the identity
/// (`server_host`, `server_port`, `name`, `token`) and the tunnels the client
/// serves.
pub async fn run_http_tunnel_client(
    config_path: &str,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let cmd = http_tunnel::Command::Client(http_tunnel::ClientArgs {
        config_path: PathBuf::from(config_path),
    });
    http_tunnel::run(cmd, shutdown_rx).await
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
