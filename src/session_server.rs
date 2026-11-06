//! Server side of a protocol v4 session: one WebSocket per peer, multiplexing
//! route advertisements and visitor streams.

use anyhow::{anyhow, bail, Context, Result};
use rand::RngCore;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{split, AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::{mpsc, oneshot};
use tokio::time;
use tracing::{debug, info, warn};

use crate::protocol_v4::{self as p4, RouteAdvert, RouteError, RouteRejection, SessionFrame};
use crate::server::{ServerState, V4SessionInfo};
use crate::transport::Conn;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const STREAM_BUFFER: usize = p4::INITIAL_WINDOW as usize;
/// The number of management requests that may wait for a downstream answer on
/// one session. The management API also times out, so this only bounds a peer
/// that never answers.
const MAX_PENDING_MANAGEMENT: usize = 16;

/// A handle used by the visitor path to open a stream on a v4 session.
#[derive(Clone)]
pub(crate) struct V4SessionHandle {
    pub(crate) session_id: u64,
    pub(crate) node_id: String,
    tx: mpsc::Sender<SessionCommand>,
}

enum SessionCommand {
    Open {
        domain: String,
        header: Vec<u8>,
        request_id: String,
        reply: oneshot::Sender<Result<DuplexStream>>,
    },
}

/// A control operation the management API submits to a connected session.
pub(crate) enum SessionControl {
    /// Ask the session task to close after withdrawing its routes.
    Close { reply: oneshot::Sender<()> },
    /// Perform one proxy operation on the downstream node.
    Proxy {
        op: p4::ProxyOp,
        reply: oneshot::Sender<std::result::Result<Vec<p4::ProxyEntry>, String>>,
    },
}

enum StreamInput {
    Data(Vec<u8>, bool),
    Window(u32),
    Reset(p4::StreamError),
}

/// Why a visitor stream did not open on a v4 session, in the terms a gateway
/// error page needs: is the session itself gone, or did it not answer in time?
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpenFailure {
    /// The session is gone, refused the stream, or dropped the request.
    Unavailable(String),
    /// The downstream did not open the stream before the timeout.
    Timeout,
}

impl OpenFailure {
    /// The cause as one line for the visitor-facing page.
    pub(crate) fn cause(&self) -> String {
        match self {
            OpenFailure::Unavailable(message) => message.clone(),
            OpenFailure::Timeout => "the downstream did not open the stream in time".to_string(),
        }
    }
}

impl V4SessionHandle {
    /// Open one visitor stream. The returned half is the server side of the
    /// pipe; the session task drives the other half to the downstream.
    pub(crate) async fn open(
        &self,
        domain: &str,
        header: Vec<u8>,
        timeout: Duration,
        request_id: &str,
    ) -> std::result::Result<DuplexStream, OpenFailure> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(SessionCommand::Open {
                domain: domain.to_string(),
                header,
                request_id: request_id.to_string(),
                reply,
            })
            .await
            .map_err(|_| OpenFailure::Unavailable("the v4 session is gone".to_string()))?;
        match time::timeout(timeout, reply_rx).await {
            Ok(Ok(Ok(stream))) => Ok(stream),
            Ok(Ok(Err(e))) => Err(OpenFailure::Unavailable(format!("{:#}", e))),
            Ok(Err(_)) => Err(OpenFailure::Unavailable(
                "the v4 session dropped the stream request".to_string(),
            )),
            Err(_) => Err(OpenFailure::Timeout),
        }
    }
}

/// Accept a v4 session on an already-upgraded WebSocket. `peer` is the address
/// the session connected from, `forwarded` the address a gateway in front of us
/// reported and `peer_version` the software version the peer announced in the
/// upgrade. The last three are kept for the management page and the logs.
pub(crate) async fn accept(
    mut conn: Conn,
    peer: SocketAddr,
    forwarded: Option<String>,
    peer_version: Option<String>,
    state: Arc<ServerState>,
) -> Result<()> {
    let hello = time::timeout(HANDSHAKE_TIMEOUT, p4::read_frame(&mut conn))
        .await
        .context("Timed out waiting for the v4 hello")??;
    let (node_id, role, capabilities) = match hello {
        SessionFrame::Hello {
            version,
            node_id,
            role,
            capabilities,
        } => {
            if version != p4::V4_VERSION {
                let _ = p4::write_frame(
                    &mut conn,
                    &SessionFrame::Error {
                        code: p4::ErrorCode::ProtocolError,
                        message: format!("Expected v4, got {}", version),
                    },
                )
                .await;
                bail!("A v4 session announced version {}", version);
            }
            if node_id.trim().is_empty() {
                bail!("A v4 session has an empty node id");
            }
            (node_id, role, capabilities)
        }
        _ => bail!("The first v4 frame is not Hello"),
    };

    let mut nonce = vec![0u8; 32];
    rand::thread_rng().fill_bytes(&mut nonce);
    // The v4 endpoint is unreachable when no control host is configured, so a
    // missing value here never reaches a real handshake.
    let server_id = state.control_host.clone().unwrap_or_default();
    p4::write_frame(
        &mut conn,
        &SessionFrame::Challenge {
            server_id: server_id.clone(),
            nonce: nonce.clone(),
        },
    )
    .await?;

    let auth = time::timeout(HANDSHAKE_TIMEOUT, p4::read_frame(&mut conn))
        .await
        .context("Timed out waiting for the v4 auth")??;
    let proof = match auth {
        SessionFrame::Auth { proof } => proof,
        _ => bail!("The second v4 frame is not Auth"),
    };
    let mut concat = Vec::from(state.token.as_bytes());
    concat.extend_from_slice(&nonce);
    concat.extend_from_slice(node_id.as_bytes());
    concat.extend_from_slice(server_id.as_bytes());
    let expected = crate::helper::digest(&concat);
    if proof != expected {
        let _ = p4::write_frame(
            &mut conn,
            &SessionFrame::Error {
                code: p4::ErrorCode::AuthFailed,
                message: "The token was rejected".to_string(),
            },
        )
        .await;
        bail!("The v4 node `{}` failed the authentication", node_id);
    }

    let permit = state
        .v4_sessions
        .clone()
        .try_acquire_owned()
        .map_err(|_| anyhow!("The v4 session limit is reached"))?;
    let session_id = state.next_v4_session_id.fetch_add(1, Ordering::Relaxed);
    p4::write_frame(&mut conn, &SessionFrame::Welcome { session_id }).await?;
    let role_name = match role {
        p4::Role::Client => "client",
        p4::Role::Node => "node",
    };
    let (control_tx, control_rx) = mpsc::channel(MAX_PENDING_MANAGEMENT);
    state.v4_session_info.write().await.insert(
        session_id,
        V4SessionInfo {
            node_id: node_id.clone(),
            peer,
            forwarded: forwarded.clone(),
            version: peer_version.clone(),
            role: role_name,
            since: Instant::now(),
            capabilities,
            control: control_tx,
        },
    );
    info!(
        session_id,
        node = %node_id,
        peer = %peer,
        client = forwarded.as_deref().unwrap_or("-"),
        version = peer_version.as_deref().unwrap_or("-"),
        role = %role_name,
        "v4 session established"
    );

    let result = run_session(
        conn,
        state.clone(),
        session_id,
        node_id,
        forwarded,
        capabilities,
        control_rx,
        permit,
    )
    .await;
    state.v4_session_info.write().await.remove(&session_id);
    result
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    conn: Conn,
    state: Arc<ServerState>,
    session_id: u64,
    node_id: String,
    forwarded: Option<String>,
    capabilities: u32,
    mut control_rx: mpsc::Receiver<SessionControl>,
    _permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<()> {
    let (mut reader, mut writer) = split(conn);
    let (out_tx, mut out_rx) = mpsc::channel::<SessionFrame>(256);
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if let Err(err) = p4::write_frame(&mut writer, &frame).await {
                warn!(session_id, "v4 session write failed: {:#}", err);
                break;
            }
        }
    });

    let (open_tx, mut open_rx) = mpsc::channel::<SessionCommand>(64);
    let handle = V4SessionHandle {
        session_id,
        node_id: node_id.clone(),
        tx: open_tx,
    };
    let (closed_tx, mut closed_rx) = mpsc::unbounded_channel::<u32>();
    let mut shutdown_rx = state.v4_shutdown.subscribe();
    let mut owned: HashSet<String> = HashSet::new();
    let mut streams: HashMap<u32, mpsc::UnboundedSender<StreamInput>> = HashMap::new();
    let mut next_stream_id: u32 = 1;
    let mut next_request_id: u64 = 1;
    let mut pending: HashMap<
        u64,
        oneshot::Sender<std::result::Result<Vec<p4::ProxyEntry>, String>>,
    > = HashMap::new();
    let mut close_reply = None;

    loop {
        tokio::select! {
            frame = p4::read_frame(&mut reader) => {
                match frame {
                    Ok(frame) => {
                        match frame {
                            SessionFrame::Advertise { generation, routes } => {
                                let ack = apply_routes(&state, &handle, &mut owned, generation, routes, Vec::new()).await;
                                let _ = out_tx.send(SessionFrame::RouteAck {
                                    generation: ack.0,
                                    accepted: ack.1,
                                    rejected: ack.2,
                                }).await;
                            }
                            SessionFrame::Update { generation, add, remove } => {
                                let ack = apply_routes(&state, &handle, &mut owned, generation, add, remove).await;
                                let _ = out_tx.send(SessionFrame::RouteAck {
                                    generation: ack.0,
                                    accepted: ack.1,
                                    rejected: ack.2,
                                }).await;
                            }
                            SessionFrame::StreamData { stream_id, data, fin } => {
                                if let Some(tx) = streams.get(&stream_id) {
                                    let _ = tx.send(StreamInput::Data(data, fin));
                                }
                            }
                            SessionFrame::StreamWindowUpdate { stream_id, credit } => {
                                if let Some(tx) = streams.get(&stream_id) {
                                    let _ = tx.send(StreamInput::Window(credit));
                                }
                            }
                            SessionFrame::StreamReset { stream_id, reason } => {
                                if let Some(tx) = streams.remove(&stream_id) {
                                    let _ = tx.send(StreamInput::Reset(reason));
                                }
                            }
                            SessionFrame::Ping { seq } => {
                                let _ = out_tx.send(SessionFrame::Pong { seq }).await;
                            }
                            SessionFrame::Pong { .. } => {}
                            SessionFrame::GoAway { reason, .. } => {
                                info!(session_id, "v4 peer is going away: {}", reason);
                                break;
                            }
                            SessionFrame::Error { code, message } => {
                                warn!(session_id, "v4 peer error {:?}: {}", code, message);
                                break;
                            }
                            SessionFrame::ProxyResponse { request_id, proxies, error } => {
                                match pending.remove(&request_id) {
                                    Some(reply) => {
                                        let _ = reply.send(match error {
                                            Some(error) => Err(error),
                                            None => Ok(proxies),
                                        });
                                    }
                                    None => debug!(
                                        session_id,
                                        request_id,
                                        "An unexpected proxy response was ignored"
                                    ),
                                }
                            }
                            other => {
                                warn!(session_id, "Unexpected v4 frame: {:?}", other);
                                break;
                            }
                        }
                    }
                    Err(err) => {
                        warn!(session_id, "v4 session closed: {:#}", err);
                        break;
                    }
                }
            }
            command = open_rx.recv() => {
                let Some(SessionCommand::Open { domain, header, request_id, reply }) = command else {
                    break;
                };
                if streams.len() >= p4::MAX_STREAMS_PER_SESSION {
                    let _ = reply.send(Err(anyhow!("The v4 stream limit is reached")));
                    continue;
                }
                let stream_id = next_stream_id;
                next_stream_id = next_stream_id.wrapping_add(1).max(1);
                let (visitor_io, tunnel_io) = tokio::io::duplex(STREAM_BUFFER);
                let (input_tx, input_rx) = mpsc::unbounded_channel();
                streams.insert(stream_id, input_tx);
                let trace_id = request_id.as_bytes().to_vec();
                if out_tx.send(SessionFrame::OpenStream {
                    stream_id,
                    domain: domain.clone(),
                    header,
                    trace_id,
                    hop: 0,
                }).await.is_err() {
                    streams.remove(&stream_id);
                    let _ = reply.send(Err(anyhow!("The v4 session is gone")));
                    continue;
                }
                debug!(
                    request_id = %request_id,
                    session_id,
                    stream_id,
                    host = %domain,
                    "Opening a v4 stream"
                );
                let out_tx_stream = out_tx.clone();
                let closed_tx_stream = closed_tx.clone();
                tokio::spawn(run_server_stream(
                    stream_id,
                    session_id,
                    node_id.clone(),
                    domain,
                    request_id,
                    tunnel_io,
                    input_rx,
                    out_tx_stream,
                    closed_tx_stream,
                ));
                let _ = reply.send(Ok(visitor_io));
            }
            command = control_rx.recv() => {
                let Some(command) = command else {
                    break;
                };
                match command {
                    SessionControl::Close { reply } => {
                        let _ = out_tx.send(SessionFrame::GoAway {
                            reason: "disconnected by the management API".to_string(),
                            drain_seconds: 0,
                        }).await;
                        close_reply = Some(reply);
                        break;
                    }
                    SessionControl::Proxy { op, reply } => {
                        if capabilities & p4::CAP_PROXY_ADMIN == 0 {
                            let _ = reply.send(Err(
                                "the downstream node does not support remote proxy management"
                                    .to_string(),
                            ));
                            continue;
                        }
                        if pending.len() >= MAX_PENDING_MANAGEMENT {
                            let _ = reply.send(Err(
                                "too many pending management requests on this session".to_string(),
                            ));
                            continue;
                        }
                        let request_id = next_request_id;
                        next_request_id = next_request_id.wrapping_add(1).max(1);
                        if out_tx
                            .send(SessionFrame::ProxyRequest { request_id, op })
                            .await
                            .is_err()
                        {
                            let _ = reply.send(Err("the v4 session is gone".to_string()));
                            continue;
                        }
                        pending.insert(request_id, reply);
                    }
                }
            }
            id = closed_rx.recv() => {
                if let Some(id) = id {
                    streams.remove(&id);
                }
            }
            _ = shutdown_rx.recv() => {
                let _ = out_tx.send(SessionFrame::GoAway {
                    reason: "server is shutting down".to_string(),
                    drain_seconds: 0,
                }).await;
                break;
            }
        }
    }

    // A management request that cloned the sender just before the close must
    // fail fast instead of waiting for a reply that can no longer be produced.
    drop(control_rx);

    // Withdraw every route owned by this session, unless a newer session has
    // already taken it over.
    let had_routes = !owned.is_empty();
    for prefix in owned.drain() {
        remove_route(&state, session_id, &prefix).await;
    }
    if had_routes {
        state.refresh_routes().await;
    }
    for (_, reply) in pending.drain() {
        let _ = reply.send(Err("the v4 session closed".to_string()));
    }
    state.v4_session_info.write().await.remove(&session_id);
    if let Some(reply) = close_reply {
        let _ = reply.send(());
    }
    drop(streams);
    drop(out_tx);
    let _ = time::timeout(Duration::from_secs(1), writer_task).await;
    info!(
        session_id,
        node = %node_id,
        client = forwarded.as_deref().unwrap_or("-"),
        "v4 session closed"
    );
    Ok(())
}

type RouteAck = (u64, Vec<String>, Vec<RouteRejection>);

async fn apply_routes(
    state: &ServerState,
    handle: &V4SessionHandle,
    owned: &mut HashSet<String>,
    generation: u64,
    add: Vec<RouteAdvert>,
    remove: Vec<String>,
) -> RouteAck {
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    let mut changed = false;

    for prefix in remove {
        let Some(prefix) = p4::normalize_prefix(&prefix) else {
            continue;
        };
        if owned.remove(&prefix) {
            remove_route(state, handle.session_id, &prefix).await;
            changed = true;
        }
    }

    for route in add {
        let Some(prefix) = p4::normalize_prefix(&route.prefix) else {
            rejected.push(RouteRejection {
                prefix: route.prefix,
                reason: RouteError::InvalidPrefix,
            });
            continue;
        };
        if state
            .control_host
            .as_deref()
            .is_some_and(|control| crate::config::domain_overlaps_control_host(&prefix, control))
        {
            rejected.push(RouteRejection {
                prefix,
                reason: RouteError::ReservedHost,
            });
            continue;
        }
        if owned.len() >= p4::MAX_ROUTES_PER_SESSION {
            rejected.push(RouteRejection {
                prefix,
                reason: RouteError::TooManyRoutes,
            });
            continue;
        }
        let already_owned = owned.contains(&prefix);
        {
            let mut routes = state.v4_routes.write().await;
            if routes
                .get(&prefix)
                .is_some_and(|owner| owner.session_id != handle.session_id)
            {
                rejected.push(RouteRejection {
                    prefix,
                    reason: RouteError::Conflict,
                });
                continue;
            }
            routes.insert(prefix.clone(), handle.clone());
        }
        state.forget_withdrawal(&prefix).await;
        if !already_owned {
            info!(
                session_id = handle.session_id,
                domain = %prefix,
                "v4 route advertised"
            );
            changed = true;
        }
        owned.insert(prefix.clone());
        accepted.push(prefix);
    }

    for rejection in &rejected {
        warn!(
            session_id = handle.session_id,
            domain = %rejection.prefix,
            reason = ?rejection.reason,
            "Rejected a route advertised by the v4 session"
        );
    }

    if changed {
        state.refresh_routes().await;
    }

    (generation, accepted, rejected)
}

async fn remove_route(state: &ServerState, session_id: u64, prefix: &str) {
    let removed = {
        let mut routes = state.v4_routes.write().await;
        if routes
            .get(prefix)
            .is_some_and(|owner| owner.session_id == session_id)
        {
            routes.remove(prefix);
            true
        } else {
            false
        }
    };
    if removed {
        state.remember_withdrawal(prefix, session_id).await;
        info!(
            session_id,
            domain = %prefix,
            "v4 route withdrawn"
        );
    }
}

/// Look up the v4 session that serves `host`, preferring an exact match and then
/// the longest wildcard suffix.
pub(crate) async fn lookup(state: &ServerState, host: &str) -> Option<V4SessionHandle> {
    let routes = state.v4_routes.read().await;
    if let Some(handle) = routes.get(host) {
        return Some(handle.clone());
    }
    let mut best: Option<(usize, V4SessionHandle)> = None;
    for (prefix, handle) in routes.iter() {
        let Some(suffix) = prefix.strip_prefix("*.") else {
            continue;
        };
        let dot = format!(".{}", suffix);
        if host.len() > dot.len()
            && host.ends_with(&dot)
            && best
                .as_ref()
                .map(|(length, _)| dot.len() > *length)
                .unwrap_or(true)
        {
            best = Some((dot.len(), handle.clone()));
        }
    }
    best.map(|(_, handle)| handle)
}

async fn run_server_stream(
    stream_id: u32,
    session_id: u64,
    node_id: String,
    domain: String,
    request_id: String,
    tunnel: DuplexStream,
    mut input_rx: mpsc::UnboundedReceiver<StreamInput>,
    out_tx: mpsc::Sender<SessionFrame>,
    closed_tx: mpsc::UnboundedSender<u32>,
) {
    let (mut reader, mut writer) = split(tunnel);
    let mut buffer = vec![0u8; p4::STREAM_CHUNK];
    let mut credit = p4::INITIAL_WINDOW;
    let mut outbound_done = false;
    let mut inbound_done = false;
    let mut sent_to_visitor = false;

    loop {
        let limit = std::cmp::min(buffer.len(), credit as usize);
        tokio::select! {
            read = reader.read(&mut buffer[..limit]), if !outbound_done && credit > 0 => {
                match read {
                    Ok(0) => {
                        outbound_done = true;
                        if out_tx.send(SessionFrame::StreamData {
                            stream_id,
                            data: Vec::new(),
                            fin: true,
                        }).await.is_err() {
                            break;
                        }
                    }
                    Ok(n) => {
                        credit -= n as u32;
                        if out_tx.send(SessionFrame::StreamData {
                            stream_id,
                            data: buffer[..n].to_vec(),
                            fin: false,
                        }).await.is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        debug!(
                            request_id = %request_id,
                            stream_id,
                            "v4 stream read failed: {:#}",
                            err
                        );
                        let _ = out_tx.send(SessionFrame::StreamReset {
                            stream_id,
                            reason: p4::StreamError::Io,
                        }).await;
                        break;
                    }
                }
            }
            input = input_rx.recv() => {
                match input {
                    Some(StreamInput::Data(data, fin)) => {
                        if !data.is_empty() {
                            if writer.write_all(&data).await.is_err() {
                                break;
                            }
                            sent_to_visitor = true;
                            let _ = out_tx.send(SessionFrame::StreamWindowUpdate {
                                stream_id,
                                credit: data.len() as u32,
                            }).await;
                        }
                        if fin {
                            inbound_done = true;
                            let _ = writer.shutdown().await;
                        }
                    }
                    Some(StreamInput::Window(additional)) => {
                        credit = credit.saturating_add(additional);
                    }
                    Some(StreamInput::Reset(reason)) => {
                        if !sent_to_visitor {
                            let (status, summary, layer, hint) = match reason {
                                p4::StreamError::ConnectFailed => (
                                    "502 Bad Gateway",
                                    "The downstream node could not reach its local service.",
                                    "local service (downstream node)",
                                    "A new peer answers this itself with its target; an older peer \
                                     only resets the stream, so look up this Request ID in that \
                                     node's log for the local_addr it tried.",
                                ),
                                p4::StreamError::Timeout => (
                                    "504 Gateway Timeout",
                                    "The downstream node did not answer in time.",
                                    "tunnel (downstream node)",
                                    "The node accepted the stream but sent nothing back; check that \
                                     node's log with this Request ID.",
                                ),
                                _ => (
                                    "503 Service Unavailable",
                                    "The downstream node could not serve this visitor.",
                                    "tunnel (downstream node)",
                                    "The session was reset or closed, the node has no local route \
                                     for this host, or it ran out of streams; look up this Request \
                                     ID in that node's log.",
                                ),
                            };
                            warn!(
                                request_id = %request_id,
                                session_id,
                                stream_id,
                                node = %node_id,
                                host = %domain,
                                reason = ?reason,
                                status = %status,
                                "The downstream v4 hop reset the stream before answering"
                            );
                            let session = format!("#{}", session_id);
                            let message = crate::http::gateway_failure(
                                summary,
                                layer,
                                &[
                                    ("node", &node_id),
                                    ("session", &session),
                                    ("tunnel", &domain),
                                    ("cause", &format!("{:?}", reason)),
                                ],
                                hint,
                            );
                            let response = crate::http::error_response(
                                &request_id,
                                status,
                                &message,
                            );
                            let _ = writer.write_all(&response).await;
                            let _ = writer.shutdown().await;
                        }
                        break;
                    }
                    None => break,
                }
            }
        }
        if outbound_done && inbound_done {
            break;
        }
    }

    let _ = closed_tx.send(stream_id);
    debug!(request_id = %request_id, stream_id, "v4 stream finished");
}
