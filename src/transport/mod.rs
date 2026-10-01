use crate::config::{ClientTunnelConfig, ServerTunnelConfig};
use crate::helper::to_socket_addr;
use anyhow::{Context, Result};
use std::fmt::Display;
use std::net::SocketAddr;
use tokio::net::TcpStream;
use tracing::{error, trace};

#[derive(Clone)]
pub struct AddrMaybeCached {
    pub addr: String,
    pub socket_addr: Option<SocketAddr>,
}

impl AddrMaybeCached {
    pub fn new(addr: &str) -> AddrMaybeCached {
        AddrMaybeCached {
            addr: addr.to_string(),
            socket_addr: None,
        }
    }

    pub async fn resolve(&mut self) -> Result<()> {
        match to_socket_addr(&self.addr).await {
            Ok(s) => {
                self.socket_addr = Some(s);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

impl Display for AddrMaybeCached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.socket_addr {
            Some(s) => f.write_fmt(format_args!("{}", s)),
            None => f.write_str(&self.addr),
        }
    }
}

/// Connect to `addr`, reusing the resolved socket address if available
pub async fn connect(addr: &AddrMaybeCached) -> Result<TcpStream> {
    Ok(match addr.socket_addr {
        Some(s) => TcpStream::connect(s).await?,
        None => TcpStream::connect(&addr.addr).await?,
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SocketOpts {
    // None means do not change
    nodelay: Option<bool>,
}

impl SocketOpts {
    /// Socket options for the control channel
    pub fn for_control_channel() -> SocketOpts {
        SocketOpts {
            nodelay: Some(true), // Always set nodelay for the control channel
        }
    }

    pub fn from_client_cfg(cfg: &ClientTunnelConfig) -> SocketOpts {
        SocketOpts {
            nodelay: cfg.nodelay,
        }
    }

    pub fn from_server_cfg(cfg: &ServerTunnelConfig) -> SocketOpts {
        SocketOpts {
            nodelay: cfg.nodelay,
        }
    }

    pub fn apply(&self, conn: &TcpStream) {
        if let Some(nodelay) = self.nodelay {
            trace!("Set nodelay {}", nodelay);
            if let Err(e) = conn
                .set_nodelay(nodelay)
                .with_context(|| "Failed to set nodelay")
            {
                error!("{:#}", e);
            }
        }
    }
}
