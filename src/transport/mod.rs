pub(crate) mod websocket;

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::OwnedSemaphorePermit;

use websocket::BoxedIo;

/// A connection that carries the tunnel protocol.
///
/// The only transport is a WebSocket exposed as an opaque byte stream. The
/// connection also carries a liveness flag, which the bridge clears when the
/// socket dies, and the capacity permit of the control or data channel, which is
/// held until the connection is dropped.
pub(crate) struct Conn {
    io: BoxedIo,
    alive: Arc<AtomicBool>,
    // Never read: holding it is what reserves the capacity slot.
    _permit: Option<OwnedSemaphorePermit>,
}

impl Conn {
    pub(crate) fn new(io: BoxedIo, alive: Arc<AtomicBool>) -> Conn {
        Conn {
            io,
            alive,
            _permit: None,
        }
    }

    /// Attach the capacity permit of the channel, so that it is released when
    /// the connection is dropped.
    pub(crate) fn with_permit(mut self, permit: OwnedSemaphorePermit) -> Conn {
        self._permit = Some(permit);
        self
    }

    /// Whether the WebSocket bridge is still running. A pooled data channel that
    /// missed its keepalive pongs is dropped before it is handed to a visitor.
    pub(crate) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

impl AsyncRead for Conn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.get_mut().io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.get_mut().io).poll_shutdown(cx)
    }
}
