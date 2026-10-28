//! Protocol v4: one multiplexed session per peer.
//!
//! The v3 protocol is one WebSocket per control channel and one per data
//! channel. v4 keeps a single session per peer and multiplexes route
//! advertisements and visitor streams over it. The HTTP data plane is
//! unchanged: a stream is still a raw byte pipe to a registered `local_addr`.
//!
//! v4 sessions use their own entry path (`/_http-tunnel/v4/connect`), so a v3
//! server answers `404` instead of misparsing a v4 frame. v3 peers keep using
//! the v1 path.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The entry path of a v4 session, next to the v3 `.../v1/connect` path.
pub const V4_PATH: &str = "/_http-tunnel/v4/connect";
/// The protocol version carried in `Hello`.
pub const V4_VERSION: u16 = 4;
/// The maximum size of one encoded frame. Stream data frames are smaller.
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
/// The payload size of one `StreamData` frame. Small enough to interleave
/// streams on one session without a large head-of-line delay.
pub const STREAM_CHUNK: usize = 64 * 1024;
/// The initial per-stream window in each direction.
pub const INITIAL_WINDOW: u32 = 256 * 1024;
/// The number of routes one session may advertise.
pub const MAX_ROUTES_PER_SESSION: usize = 256;
/// The number of streams one session may run at once.
pub const MAX_STREAMS_PER_SESSION: usize = 256;

/// The role announced in `Hello`. It is advisory: authentication and ACLs
/// decide what a peer may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Client,
    Node,
}

/// One route in a snapshot or an update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteAdvert {
    /// Exact domain or `*.suffix` wildcard, normalized like a v3 domain.
    pub prefix: String,
    /// The node that first published the route.
    pub origin: String,
    /// The number of relay hops the route has crossed.
    pub hops: u8,
}

/// Why a route was rejected. The advertiser logs it once and does not retry
/// the same prefix until its advertisement changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteRejection {
    pub prefix: String,
    pub reason: RouteError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouteError {
    InvalidPrefix,
    ReservedHost,
    Conflict,
    TooManyRoutes,
    AclDenied,
    LoopDetected,
    HopsExceeded,
}

/// Why a stream ended before the normal end of both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamError {
    /// The downstream could not connect to the tunnel's `local_addr`.
    ConnectFailed,
    /// The downstream did not provide a stream in time.
    Timeout,
    /// An IO error on either side.
    Io,
    /// The session is shutting down.
    Canceled,
    /// A malformed frame or an invalid stream state.
    Protocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    AuthFailed,
    ProtocolError,
    TooManyRoutes,
    TooManyStreams,
    GoingAway,
}

/// A frame of a v4 session. It is length-prefixed and bincode-encoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionFrame {
    // Handshake
    Hello {
        version: u16,
        node_id: String,
        role: Role,
        capabilities: u32,
    },
    Challenge {
        server_id: String,
        nonce: Vec<u8>,
    },
    Auth {
        proof: Vec<u8>,
    },
    Welcome {
        session_id: u64,
    },

    // Routes
    Advertise {
        generation: u64,
        routes: Vec<RouteAdvert>,
    },
    Update {
        generation: u64,
        add: Vec<RouteAdvert>,
        remove: Vec<String>,
    },
    RouteAck {
        generation: u64,
        accepted: Vec<String>,
        rejected: Vec<RouteRejection>,
    },

    // Streams
    OpenStream {
        stream_id: u32,
        domain: String,
        /// The visitor's prefetched HTTP header. The downstream applies its
        /// `auth_header`, if any, before connecting.
        header: Vec<u8>,
        trace_id: Vec<u8>,
        hop: u8,
    },
    StreamData {
        stream_id: u32,
        data: Vec<u8>,
        fin: bool,
    },
    StreamWindowUpdate {
        stream_id: u32,
        credit: u32,
    },
    StreamReset {
        stream_id: u32,
        reason: StreamError,
    },

    // Session maintenance
    Ping {
        seq: u64,
    },
    Pong {
        seq: u64,
    },
    GoAway {
        reason: String,
        drain_seconds: u32,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

/// Write one length-prefixed frame.
pub(crate) async fn write_frame<W>(writer: &mut W, frame: &SessionFrame) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let bytes = bincode::serialize(frame).context("Failed to serialize a v4 frame")?;
    if bytes.len() > MAX_FRAME_SIZE {
        bail!(
            "A v4 frame of {} bytes exceeds the {}-byte limit",
            bytes.len(),
            MAX_FRAME_SIZE
        );
    }
    writer
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await
        .context("Failed to write a v4 frame length")?;
    writer
        .write_all(&bytes)
        .await
        .context("Failed to write a v4 frame")?;
    writer.flush().await.context("Failed to flush a v4 frame")?;
    Ok(())
}

/// Read one length-prefixed frame.
pub(crate) async fn read_frame<R>(reader: &mut R) -> Result<SessionFrame>
where
    R: AsyncRead + Unpin,
{
    let mut length = [0u8; 4];
    reader
        .read_exact(&mut length)
        .await
        .context("Failed to read a v4 frame length")?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("A v4 frame has an invalid length: {}", length),
        ))
        .context("Invalid v4 frame");
    }
    let mut bytes = vec![0u8; length];
    reader
        .read_exact(&mut bytes)
        .await
        .context("Failed to read a v4 frame")?;
    bincode::deserialize(&bytes).context("Failed to decode a v4 frame")
}

/// Normalize an advertised prefix. Invalid prefixes are reported as a route
/// rejection instead of closing the session.
pub(crate) fn normalize_prefix(prefix: &str) -> Option<String> {
    let normalized = crate::helper::normalize_domain(prefix)?;
    if normalized.starts_with("*.") && normalized.len() <= 2 {
        return None;
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn roundtrip(frame: SessionFrame) -> Result<SessionFrame> {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &frame).await?;
        read_frame(&mut &buffer[..]).await
    }

    #[tokio::test]
    async fn frames_roundtrip() -> Result<()> {
        let frames = [
            SessionFrame::Hello {
                version: V4_VERSION,
                node_id: "relay-a".to_string(),
                role: Role::Node,
                capabilities: 0,
            },
            SessionFrame::Advertise {
                generation: 3,
                routes: vec![RouteAdvert {
                    prefix: "*.example.com".to_string(),
                    origin: "relay-a".to_string(),
                    hops: 1,
                }],
            },
            SessionFrame::OpenStream {
                stream_id: 7,
                domain: "nas.example.com".to_string(),
                header: b"GET / HTTP/1.1\r\n".to_vec(),
                trace_id: vec![1, 2, 3, 4],
                hop: 2,
            },
            SessionFrame::StreamData {
                stream_id: 7,
                data: b"GET / HTTP/1.1\r\n".to_vec(),
                fin: false,
            },
            SessionFrame::StreamReset {
                stream_id: 7,
                reason: StreamError::ConnectFailed,
            },
            SessionFrame::Ping { seq: 42 },
            SessionFrame::GoAway {
                reason: "shutdown".to_string(),
                drain_seconds: 5,
            },
        ];
        for frame in frames {
            assert_eq!(roundtrip(frame.clone()).await?, frame);
        }
        Ok(())
    }

    #[tokio::test]
    async fn an_oversized_frame_is_rejected() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&(MAX_FRAME_SIZE as u32 + 1).to_be_bytes());
        assert!(read_frame(&mut &buffer[..]).await.is_err());
    }

    #[test]
    fn prefixes_are_normalized() {
        assert_eq!(
            normalize_prefix("Foo.Example.COM"),
            Some("foo.example.com".to_string())
        );
        assert_eq!(
            normalize_prefix("*.Example.com"),
            Some("*.example.com".to_string())
        );
        assert_eq!(normalize_prefix(""), None);
        assert_eq!(normalize_prefix("http://example.com"), None);
    }
}
