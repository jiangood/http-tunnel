use std::path::PathBuf;

use anyhow::Result;
use tokio::{
    io,
    net::{TcpListener, ToSocketAddrs},
    sync::broadcast,
};

pub async fn run_http_tunnel_server(
    server_port: u16,
    server_host: &str,
    token: &str,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    run_http_tunnel_server_with_limits(
        server_port,
        server_host,
        token,
        http_tunnel::ServerLimits::default(),
        shutdown_rx,
    )
    .await
}

pub async fn run_http_tunnel_server_with_limits(
    server_port: u16,
    server_host: &str,
    token: &str,
    limits: http_tunnel::ServerLimits,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let cmd = http_tunnel::Command::Server(http_tunnel::ServerArgs {
        server_port,
        server_host: server_host.to_string(),
        // Keep the test listeners on loopback: a wildcard listener triggers the
        // Windows Firewall prompt for every test binary.
        bind_host: "127.0.0.1".to_string(),
        token: token.to_string(),
        proxies: Vec::new(),
        limits,
    });
    http_tunnel::run(cmd, shutdown_rx).await
}

/// Run a client from its configuration file. The file carries the identity
/// (`server`, `name`, `token`) and the tunnels the client serves.
pub async fn run_http_tunnel_client(
    config_path: &str,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let cmd = http_tunnel::Command::Client(http_tunnel::ClientArgs {
        config_path: PathBuf::from(config_path),
    });
    http_tunnel::run(cmd, shutdown_rx).await
}

/// Run a node from its configuration file.
pub async fn run_http_tunnel_node(
    config_path: &str,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let config = http_tunnel::NodeConfig::from_file(std::path::Path::new(config_path)).await?;
    let cmd = http_tunnel::Command::Node(http_tunnel::NodeArgs { config });
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
