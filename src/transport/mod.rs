pub(crate) mod websocket;

use crate::helper::to_socket_addr;
use anyhow::{Context, Result};
use std::fmt::Display;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
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

    /// Socket options for a data channel: the client announces its TCP_NODELAY
    /// preference for the tunnel, and the server applies the same to its side
    pub fn data_channel(nodelay: bool) -> SocketOpts {
        SocketOpts {
            nodelay: Some(nodelay),
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

/// A connection that carries the tunnel protocol.
///
/// The legacy transport is a plain TCP socket, while the WebSocket transport
/// exposes the tunnel as an opaque byte stream. Both are handed to the same
/// protocol handler, so the WebSocket mode does not need a local TCP listener.
pub(crate) enum Conn {
    Tcp(TcpStream),
    Stream(websocket::BoxedIo),
}

impl Conn {
    /// Apply socket options when the underlying transport is a TCP socket. A
    /// WebSocket connection already had its options set on the HTTP socket, so
    /// this is a no-op for it.
    pub(crate) fn apply(&self, opts: SocketOpts) {
        if let Conn::Tcp(conn) = self {
            opts.apply(conn);
        }
    }
}

impl AsyncRead for Conn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Conn::Tcp(conn) => Pin::new(conn).poll_read(cx, buf),
            Conn::Stream(conn) => Pin::new(&mut **conn).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Conn::Tcp(conn) => Pin::new(conn).poll_write(cx, buf),
            Conn::Stream(conn) => Pin::new(&mut **conn).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Conn::Tcp(conn) => Pin::new(conn).poll_flush(cx),
            Conn::Stream(conn) => Pin::new(&mut **conn).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Conn::Tcp(conn) => Pin::new(conn).poll_shutdown(cx),
            Conn::Stream(conn) => Pin::new(&mut **conn).poll_shutdown(cx),
        }
    }
}
