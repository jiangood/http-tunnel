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

use super::{Conn, SocketOpts};

/// Keep individual WebSocket messages bounded; the byte-stream protocol does
/// not need large WS messages because the bridge chunks writes into 16 KiB pieces.
const MAX_WS_MESSAGE_SIZE: usize = 1024 * 1024;
const WS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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

pub(crate) type BoxedIo = Box<dyn AsyncReadWrite + Unpin + Send + Sync>;

/// Connect to the control Host over WebSocket and expose it as a byte stream.
/// The existing length-prefixed protocol therefore does not need to know about WS frames.
///
/// `tls` selects `wss://` instead of `ws://`; the TLS handshake is part of the
/// WebSocket connect, so callers only pass the resolved setting.
pub(crate) async fn connect_websocket(server_addr: &str, tls: bool) -> Result<BoxedIo> {
    let url = websocket_url(server_addr, tls);
    let (ws, _) = timeout(
        WS_HANDSHAKE_TIMEOUT,
        // The tunnel carries small interactive frames; keep the same
        // TCP_NODELAY behaviour as the legacy TCP transport.
        connect_async_with_config(&url, Some(websocket_config()), true),
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

/// The URL of the tunnel WebSocket endpoint, `wss://` when TLS is enabled.
pub(crate) fn websocket_url(server_addr: &str, tls: bool) -> String {
    let scheme = if tls { "wss" } else { "ws" };
    format!("{}://{}/_http-tunnel/v1/connect", scheme, server_addr)
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

/// Accept a tunnel WebSocket on the HTTP listener and expose it as a byte stream.
///
/// The stream is handed to the same protocol handler as the TCP transport, so
/// the WebSocket mode needs no local TCP listener at all. `permit` keeps the
/// connection counted against the WebSocket limit until the bridge closes.
pub(crate) async fn accept(
    prefix: Vec<u8>,
    stream: TcpStream,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<Conn> {
    // This socket carries the whole tunnel for the rest of its life, so it has
    // the same latency requirements as a control channel.
    SocketOpts::for_control_channel().apply(&stream);
    let prefixed = PrefixedIo::new(prefix, stream);
    let ws = timeout(
        WS_HANDSHAKE_TIMEOUT,
        accept_async_with_config(prefixed, Some(websocket_config())),
    )
    .await
    .context("Timed out accepting tunnel WebSocket handshake")?
    .context("Accepting tunnel WebSocket handshake")?;

    let (io, bridge) = duplex(64 * 1024);
    tokio::spawn(async move {
        // Held until the bridge is done, which is when the socket is closed.
        let _permit = permit;
        if let Err(err) = bridge_websocket_to_io(ws, bridge).await {
            tracing::debug!("WebSocket stream bridge closed: {:#}", err);
        }
    });
    Ok(Conn::Stream(Box::new(io)))
}


#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::client_async_with_config;

    #[test]
    fn websocket_config_bounds_incoming_messages_and_frames() {
        let config = websocket_config();
        assert_eq!(config.max_message_size, Some(MAX_WS_MESSAGE_SIZE));
        assert_eq!(config.max_frame_size, Some(MAX_WS_MESSAGE_SIZE));
    }

    #[test]
    fn websocket_url_selects_the_scheme_from_the_tls_setting() {
        assert_eq!(
            websocket_url("control.example.com:443", true),
            "wss://control.example.com:443/_http-tunnel/v1/connect"
        );
        assert_eq!(
            websocket_url("control.example.com:80", false),
            "ws://control.example.com:80/_http-tunnel/v1/connect"
        );
        assert_eq!(
            websocket_url("[::1]:443", true),
            "wss://[::1]:443/_http-tunnel/v1/connect"
        );
    }

    #[tokio::test]
    async fn text_websocket_message_is_rejected_by_the_tunnel_bridge() {
        let (server_io, client_io) = duplex(64 * 1024);
        let (bridge_io, _bridge_peer) = duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let ws = accept_async_with_config(server_io, Some(websocket_config()))
                .await
                .expect("WebSocket handshake should succeed");
            bridge_websocket_to_io(ws, bridge_io).await
        });

        let (mut client, _) = client_async_with_config(
            "ws://localhost/_http-tunnel/v1/connect",
            client_io,
            Some(websocket_config()),
        )
        .await
        .expect("client handshake should succeed");
        client
            .send(Message::Text("not a tunnel frame".to_string()))
            .await
            .expect("test client should send text");
        let result = timeout(Duration::from_secs(3), server)
            .await
            .expect("bridge should reject text promptly")
            .expect("bridge task should not panic");
        assert!(result.is_err(), "text message was accepted by the bridge");
    }

    #[tokio::test]
    async fn oversized_websocket_message_is_rejected() {
        let (server_io, client_io) = duplex(MAX_WS_MESSAGE_SIZE * 2);
        let server = tokio::spawn(async move {
            let mut ws = accept_async_with_config(server_io, Some(websocket_config()))
                .await
                .expect("WebSocket handshake should succeed");
            ws.next().await
        });

        let client_config = WebSocketConfig {
            max_message_size: None,
            max_frame_size: None,
            ..WebSocketConfig::default()
        };
        let (mut client, _) = client_async_with_config(
            "ws://localhost/_http-tunnel/v1/connect",
            client_io,
            Some(client_config),
        )
        .await
        .expect("client handshake should succeed");
        client
            .send(Message::Binary(vec![0; MAX_WS_MESSAGE_SIZE + 1]))
            .await
            .expect("test client should send the oversized message");

        let result = timeout(Duration::from_secs(3), server)
            .await
            .expect("server should reject the oversized message promptly")
            .expect("server task should not panic");
        assert!(matches!(result, Some(Err(_))), "oversized message was not rejected");
    }
}
