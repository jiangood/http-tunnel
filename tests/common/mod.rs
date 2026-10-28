use std::path::PathBuf;

use anyhow::Result;
use http_tunnel::{Config, MaskedString};
use tokio::{
    io,
    net::{TcpListener, ToSocketAddrs},
    sync::broadcast,
};

/// Build a standalone ingress configuration for the test listeners.
pub fn ingress_config(listen_port: u16, control_host: &str, token: &str) -> Config {
    let mut config = Config::default();
    config.name = "test-ingress".to_string();
    config.listen_port = listen_port;
    config.control_host = Some(control_host.to_string());
    config.token = Some(MaskedString::from(token));
    config
}

pub async fn run_http_tunnel_server(
    listen_port: u16,
    control_host: &str,
    token: &str,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    run_http_tunnel_server_with_limits(
        listen_port,
        control_host,
        token,
        http_tunnel::ServerLimits::default(),
        shutdown_rx,
    )
    .await
}

pub async fn run_http_tunnel_server_with_limits(
    listen_port: u16,
    control_host: &str,
    token: &str,
    limits: http_tunnel::ServerLimits,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let mut config = ingress_config(listen_port, control_host, token);
    config.max_pending_handshakes = limits.max_pending_handshakes;
    config.max_sessions = limits.max_sessions;
    config.keepalive_interval = limits.keepalive_interval;
    http_tunnel::run(config, shutdown_rx).await
}

/// Run a universal process from its configuration file. The file carries the
/// listener identity and local `[[proxies]]`, plus an optional `[upstream]`.
pub async fn run_http_tunnel_from_config(
    config_path: &str,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let config = http_tunnel::load_config(&PathBuf::from(config_path)).await?;
    http_tunnel::run(config, shutdown_rx).await
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
