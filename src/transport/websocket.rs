use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::io::{self, Cursor};
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{
    duplex, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf,
};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::{
    accept_async_with_config, connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
    WebSocketStream,
};

/// Keep individual WebSocket messages bounded; the byte-stream protocol does
/// not need large WS messages because the bridge chunks writes into 16 KiB pieces.
const MAX_WS_MESSAGE_SIZE: usize = 1024 * 1024;
const WS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

fn websocket_config() -> WebSocketConfig {
    WebSocketConfig {
        max_message_size: Some(MAX_WS_MESSAGE_SIZE),
        max_frame_size: Some(MAX_WS_MESSAGE_SIZE),
        ..WebSocketConfig::default()
    }
}

/// Object-safe async byte stream used by the client transport implementations.
pub(crate) trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite> AsyncReadWrite for T {}

pub(crate) type BoxedIo = Box<dyn AsyncReadWrite + Unpin + Send>;

/// Connect to the control Host over plain WebSocket and expose it as a byte stream.
/// The existing length-prefixed protocol therefore does not need to know about WS frames.
pub(crate) async fn connect_websocket(remote: &str) -> Result<BoxedIo> {
    let url = format!("ws://{}/_http-tunnel/v1/connect", remote);
    let (ws, _) = timeout(
        WS_HANDSHAKE_TIMEOUT,
        connect_async_with_config(&url, Some(websocket_config()), false),
    )
    .await
    .context("Timed out connecting to WebSocket endpoint")?
    .with_context(|| format!("Failed to connect to WebSocket endpoint {}", url))?;

    let (client, bridge) = duplex(64 * 1024);
    tokio::spawn(async move {
        if let Err(err) = bridge_websocket_to_io(ws, bridge).await {
            tracing::debug!("WebSocket stream bridge closed: {:#}", err);
        }
    });
    Ok(Box::new(client))
}

/// Bridge a WebSocket connection to an ordinary async byte stream.
///
/// Binary messages are concatenated into a byte stream in one direction; bytes
/// read from the stream are emitted as binary messages in the other direction.
pub(crate) async fn bridge_websocket_to_io<S, T>(
    ws: WebSocketStream<S>,
    io: T,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut ws_sink, mut ws_stream) = ws.split();
    let (mut io_reader, mut io_writer) = tokio::io::split(io);

    let send = async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = io_reader.read(&mut buf).await.context("Reading tunnel byte stream")?;
            if n == 0 {
                let _ = ws_sink.send(Message::Close(None)).await;
                return Ok::<(), anyhow::Error>(());
            }
            ws_sink
                .send(Message::Binary(buf[..n].to_vec()))
                .await
                .context("Sending WebSocket binary message")?;
        }
    };

    let receive = async move {
        while let Some(message) = ws_stream.next().await {
            match message.context("Receiving WebSocket message")? {
                Message::Binary(bytes) => {
                    io_writer
                        .write_all(&bytes)
                        .await
                        .context("Writing WebSocket data to tunnel byte stream")?;
                }
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {
                    // tungstenite manages protocol control frames while polling the stream.
                }
                Message::Text(_) => {
                    return Err(anyhow!("Text messages are not allowed on the tunnel WebSocket"));
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        result = send => result,
        result = receive => result,
    }
}

/// Replays bytes already read from an HTTP request before reading from the socket.
/// This is needed because HTTP header sniffing may read beyond the end of the header.
pub(crate) struct PrefixedIo<S> {
    prefix: Cursor<Vec<u8>>,
    inner: S,
}

impl<S> PrefixedIo<S> {
    pub(crate) fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix: Cursor::new(prefix),
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedIo<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let pos = this.prefix.position() as usize;
        let bytes = this.prefix.get_ref();
        if pos < bytes.len() && buf.remaining() > 0 {
            let n = std::cmp::min(buf.remaining(), bytes.len() - pos);
            buf.put_slice(&bytes[pos..pos + n]);
            this.prefix.set_position((pos + n) as u64);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedIo<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Accept a WebSocket on the HTTP listener and bridge it to the existing TCP
/// protocol listener. The server's original protocol handlers remain unchanged.
pub(crate) async fn accept_and_bridge(
    prefix: Vec<u8>,
    stream: TcpStream,
    server_port: u16,
) -> Result<()> {
    let prefixed = PrefixedIo::new(prefix, stream);
    let ws = timeout(
        WS_HANDSHAKE_TIMEOUT,
        accept_async_with_config(prefixed, Some(websocket_config())),
    )
    .await
    .context("Timed out accepting tunnel WebSocket handshake")?
    .context("Accepting tunnel WebSocket handshake")?;
    let upstream = timeout(
        UPSTREAM_CONNECT_TIMEOUT,
        TcpStream::connect(("127.0.0.1", server_port)),
    )
    .await
    .context("Timed out connecting WebSocket bridge to local TCP listener")?
    .with_context(|| format!("Connecting WebSocket bridge to local TCP port {}", server_port))?;
    bridge_websocket_to_io(ws, upstream).await
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_config_bounds_incoming_messages_and_frames() {
        let config = websocket_config();
        assert_eq!(config.max_message_size, Some(MAX_WS_MESSAGE_SIZE));
        assert_eq!(config.max_frame_size, Some(MAX_WS_MESSAGE_SIZE));
    }
}
