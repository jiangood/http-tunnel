pub(crate) mod websocket;

use std::io;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use websocket::BoxedIo;

/// A connection that carries the tunnel protocol, exposed as an opaque byte
/// stream.
pub(crate) struct Conn {
    io: BoxedIo,
}

impl Conn {
    pub(crate) fn new(io: BoxedIo) -> Conn {
        Conn { io }
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
