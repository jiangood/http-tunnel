use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::io::{self, Cursor};
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{duplex, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::{self, timeout, Instant};
use tokio_tungstenite::{
    accept_async_with_config, connect_async_with_config,
    tungstenite::{
        client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig, Message,
    },
    WebSocketStream,
};

use super::Conn;

/// Keep individual WebSocket messages bounded; the byte-stream protocol does
/// not need large WS messages because the bridge chunks writes into 16 KiB pieces.
const MAX_WS_MESSAGE_SIZE: usize = 1024 * 1024;
const WS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// The bridge gives up after this many keepalive intervals without a single
/// inbound frame, which is what a silently dropped idle connection looks like.
const KEEPALIVE_TIMEOUT_FACTOR: u32 = 3;

fn websocket_config() -> WebSocketConfig {
    WebSocketConfig {
        max_message_size: Some(MAX_WS_MESSAGE_SIZE),
        max_frame_size: Some(MAX_WS_MESSAGE_SIZE),
        ..WebSocketConfig::default()
    }
}

/// The server rejected the shared token before completing the upgrade. Retrying
/// cannot fix it, so the client turns it into a fatal startup error.
#[derive(Debug)]
pub(crate) struct AuthenticationRejected;

impl std::fmt::Display for AuthenticationRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The server rejected the shared token")
    }
}

impl std::error::Error for AuthenticationRejected {}

/// Object-safe async byte stream used by the client transport implementations.
pub(crate) trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite> AsyncReadWrite for T {}

pub(crate) type BoxedIo = Box<dyn AsyncReadWrite + Unpin + Send + Sync>;

/// Connect a protocol v4 session, announcing the node id in the upgrade so the
/// server can select a per-node credential before the handshake.
pub(crate) async fn connect_websocket_v4(
    server_addr: &str,
    tls: bool,
    token: &str,
    keepalive_interval: u64,
    node_id: &str,
) -> Result<BoxedIo> {
    connect_websocket_path(
        server_addr,
        tls,
        token,
        keepalive_interval,
        crate::protocol_v4::V4_PATH,
        Some(node_id),
    )
    .await
}

async fn connect_websocket_path(
    server_addr: &str,
    tls: bool,
    token: &str,
    keepalive_interval: u64,
    path: &str,
    node_id: Option<&str>,
) -> Result<BoxedIo> {
    let url = websocket_url_path(server_addr, tls, path);
    let mut request = url
        .as_str()
        .into_client_request()
        .with_context(|| format!("Invalid WebSocket URL {}", url))?;
    let authorization = HeaderValue::from_str(&format!("Bearer {}", token))
        .context("The token cannot be sent in an HTTP header")?;
    request.headers_mut().insert("Authorization", authorization);
    if let Some(node_id) = node_id {
        let value = HeaderValue::from_str(node_id).context("The node id is not a valid header")?;
        request.headers_mut().insert("X-HTTP-Tunnel-Node", value);
    }

    let result = timeout(
        WS_HANDSHAKE_TIMEOUT,
        // The tunnel carries small interactive frames; keep TCP_NODELAY enabled.
        connect_async_with_config(request, Some(websocket_config()), true),
    )
    .await
    .context("Timed out connecting to WebSocket endpoint")?;
    let (ws, _) = match result {
        Ok(pair) => pair,
        Err(tokio_tungstenite::tungstenite::Error::Http(response))
            if response.status().as_u16() == 401 =>
        {
            return Err(anyhow::Error::new(AuthenticationRejected));
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("Failed to connect to WebSocket endpoint {}", url))
        }
    };

    let (client, bridge) = duplex(64 * 1024);
    tokio::spawn(async move {
        if let Err(err) = bridge_websocket_to_io(ws, bridge, keepalive_interval).await {
            tracing::debug!("WebSocket stream bridge closed: {:#}", err);
        }
    });
    Ok(Box::new(client))
}

/// The URL of a tunnel WebSocket endpoint, `wss://` when TLS is enabled.
fn websocket_url_path(server_addr: &str, tls: bool, path: &str) -> String {
    let scheme = if tls { "wss" } else { "ws" };
    format!("{}://{}{}", scheme, server_addr, path)
}

/// Wait for the next keepalive tick, or forever when the keepalive is disabled.
async fn keepalive_tick(interval: &mut Option<time::Interval>) {
    match interval {
        Some(interval) => {
            interval.tick().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Bridge a WebSocket connection to an ordinary async byte stream.
///
/// Binary messages are concatenated into a byte stream in one direction; bytes
/// read from the stream are emitted as binary messages in the other direction.
/// Protocol pings keep an idle session from being silently closed by a reverse
/// proxy, and a peer that stops answering is detected within
/// `keepalive_interval * 3`.
pub(crate) async fn bridge_websocket_to_io<S, T>(
    ws: WebSocketStream<S>,
    io: T,
    keepalive_interval: u64,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut ws_sink, mut ws_stream) = ws.split();
    let (mut io_reader, mut io_writer) = tokio::io::split(io);

    let mut keepalive = (keepalive_interval > 0).then(|| {
        let mut interval = time::interval(Duration::from_secs(keepalive_interval));
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        interval
    });
    let keepalive_timeout =
        Duration::from_secs(keepalive_interval.saturating_mul(KEEPALIVE_TIMEOUT_FACTOR as u64));
    let mut last_inbound = Instant::now();
    let mut buf = vec![0u8; 16 * 1024];

    loop {
        tokio::select! {
            read = io_reader.read(&mut buf) => {
                let n = read.context("Reading tunnel byte stream")?;
                if n == 0 {
                    let _ = ws_sink.send(Message::Close(None)).await;
                    return Ok(());
                }
                ws_sink
                    .send(Message::Binary(buf[..n].to_vec()))
                    .await
                    .context("Sending WebSocket binary message")?;
            }
            message = ws_stream.next() => {
                let Some(message) = message else {
                    return Ok(());
                };
                let message = message.context("Receiving WebSocket message")?;
                last_inbound = Instant::now();
                match message {
                    Message::Binary(bytes) => {
                        io_writer
                            .write_all(&bytes)
                            .await
                            .context("Writing WebSocket data to tunnel byte stream")?;
                    }
                    Message::Close(_) => return Ok(()),
                    // tungstenite queues the matching pong while reading a ping;
                    // flushing here writes it out immediately.
                    Message::Ping(_) => {
                        ws_sink.flush().await.context("Replying to a WebSocket ping")?;
                    }
                    Message::Pong(_) | Message::Frame(_) => {}
                    Message::Text(_) => {
                        return Err(anyhow!("Text messages are not allowed on the tunnel WebSocket"));
                    }
                }
            }
            _ = keepalive_tick(&mut keepalive), if keepalive.is_some() => {
                if last_inbound.elapsed() > keepalive_timeout {
                    return Err(anyhow!(
                        "WebSocket keepalive timed out after {}s without a frame",
                        keepalive_timeout.as_secs()
                    ));
                }
                ws_sink
                    .send(Message::Ping(Vec::new()))
                    .await
                    .context("Sending a WebSocket keepalive ping")?;
            }
        }
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

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Accept a tunnel WebSocket on the HTTP listener and expose it as a byte stream.
///
/// The returned `Conn` carries a liveness flag, which the bridge clears when the
/// socket dies, so a visitor is never handed a session that was silently
/// dropped.
pub(crate) async fn accept(
    prefix: Vec<u8>,
    stream: TcpStream,
    keepalive_interval: u64,
) -> Result<Conn> {
    // This socket carries the whole session for the rest of its life, so it has
    // the same latency requirements as the visitor path.
    if let Err(e) = stream.set_nodelay(true) {
        tracing::error!("Failed to set nodelay: {:#}", e);
    }
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
        if let Err(err) = bridge_websocket_to_io(ws, bridge, keepalive_interval).await {
            tracing::debug!("WebSocket stream bridge closed: {:#}", err);
        }
    });
    Ok(Conn::new(Box::new(io)))
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

    #[tokio::test]
    async fn text_websocket_message_is_rejected_by_the_tunnel_bridge() {
        let (server_io, client_io) = duplex(64 * 1024);
        let (bridge_io, _bridge_peer) = duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let ws = accept_async_with_config(server_io, Some(websocket_config()))
                .await
                .expect("WebSocket handshake should succeed");
            bridge_websocket_to_io(ws, bridge_io, 0).await
        });

        let (mut client, _) = client_async_with_config(
            "ws://localhost/_http-tunnel/v4/connect",
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
    async fn the_bridge_answers_a_ping_with_a_pong() {
        let (server_io, client_io) = duplex(64 * 1024);
        let (bridge_io, _bridge_peer) = duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let ws = accept_async_with_config(server_io, Some(websocket_config()))
                .await
                .expect("WebSocket handshake should succeed");
            bridge_websocket_to_io(ws, bridge_io, 0).await
        });

        let (mut client, _) = client_async_with_config(
            "ws://localhost/_http-tunnel/v4/connect",
            client_io,
            Some(websocket_config()),
        )
        .await
        .expect("client handshake should succeed");
        client
            .send(Message::Ping(vec![1, 2, 3]))
            .await
            .expect("client should send a ping");
        let pong = timeout(Duration::from_secs(3), client.next())
            .await
            .expect("the bridge should answer the ping promptly")
            .expect("the connection should stay open")
            .expect("the pong should not be an error");
        match pong {
            Message::Pong(payload) => assert_eq!(&*payload, &[1, 2, 3]),
            other => panic!("expected a pong, got {:?}", other),
        }

        drop(client);
        let _ = timeout(Duration::from_secs(3), server).await;
    }

    #[tokio::test]
    async fn the_bridge_closes_an_idle_connection_without_pongs() {
        let (server_io, client_io) = duplex(64 * 1024);
        let (bridge_io, _bridge_peer) = duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let ws = accept_async_with_config(server_io, Some(websocket_config()))
                .await
                .expect("WebSocket handshake should succeed");
            bridge_websocket_to_io(ws, bridge_io, 1).await
        });

        // The client never reads, so it never answers the server's pings.
        let (_client, _) = client_async_with_config(
            "ws://localhost/_http-tunnel/v4/connect",
            client_io,
            Some(websocket_config()),
        )
        .await
        .expect("client handshake should succeed");

        let result = timeout(Duration::from_secs(6), server)
            .await
            .expect("the bridge should notice the missing pongs")
            .expect("the bridge task should not panic");
        assert!(
            result.is_err(),
            "an idle connection without pongs was not closed"
        );
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
            "ws://localhost/_http-tunnel/v4/connect",
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
        assert!(
            matches!(result, Some(Err(_))),
            "oversized message was not rejected"
        );
    }
}
