//! Upstream side of a protocol v4 session. A leaf or relay keeps one session to
//! its next hop and multiplexes every visitor of its routes over it.

use anyhow::{anyhow, bail, Context, Result};
use backoff::backoff::Backoff;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{split, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time;
use tracing::{debug, info, warn};

use crate::config::{MaskedString, ServerProxyEntry};
use crate::constants::run_session_backoff;
use crate::protocol_v4::{self as p4, Role, RouteAdvert, SessionFrame};
use crate::transport::websocket::{connect_websocket_v4, BoxedIo};

/// Where a route forwards on this side.
#[derive(Debug, Clone)]
pub(crate) struct RouteTarget {
    pub(crate) local_addr: String,
    pub(crate) auth_header: Option<MaskedString>,
    /// Whether `local_addr` is this process's own embedded listener (a relay
    /// mirrors its route set to loopback). Such a hop carries the visitor's
    /// request id across in a private header.
    pub(crate) internal: bool,
}

pub(crate) type RouteMap = BTreeMap<String, RouteTarget>;

/// One proxy operation the upstream asked this process to perform. The worker
/// in `node` handles it with the same `ManagementState` as the local page.
pub(crate) enum ProxyAdminRequest {
    List {
        reply: oneshot::Sender<Result<Vec<ServerProxyEntry>>>,
    },
    Put {
        name: String,
        domain: String,
        local_addr: String,
        auth_header: Option<MaskedString>,
        reply: oneshot::Sender<Result<Vec<ServerProxyEntry>>>,
    },
    Delete {
        name: String,
        reply: oneshot::Sender<Result<Vec<ServerProxyEntry>>>,
    },
}

pub(crate) struct SessionArgs {
    pub(crate) server_addr: String,
    pub(crate) tls: bool,
    pub(crate) token: MaskedString,
    pub(crate) node_id: String,
    pub(crate) role: Role,
    pub(crate) keepalive_interval: u64,
    pub(crate) retry_interval: u64,
    /// Whether the local `[[proxies]]` come from `HT_PROXIES`. It is announced
    /// with `CAP_PROXY_ENV`, and such a node never advertises `CAP_PROXY_WRITE`.
    pub(crate) proxies_managed: bool,
    /// The live `allow_upstream_modify` switch. A change reconnects the session
    /// so that the next hop learns the new capability bits immediately. `None`
    /// in a build without the management feature.
    pub(crate) allow_upstream_modify: Option<watch::Receiver<bool>>,
    /// The local management worker. `None` in a build without the management
    /// feature; the process then advertises no proxy capability.
    pub(crate) proxy_admin: Option<mpsc::Sender<ProxyAdminRequest>>,
    pub(crate) routes: watch::Receiver<RouteMap>,
    pub(crate) fatal_tx: mpsc::UnboundedSender<anyhow::Error>,
    pub(crate) connected: Arc<AtomicBool>,
}

enum StreamInput {
    Data(Vec<u8>, bool),
    Window(u32),
    Reset(p4::StreamError),
}

/// Run the session until the shutdown signal or a fatal error, reconnecting
/// with the same exponential backoff policy as the previous attempt.
pub(crate) async fn run(
    mut args: SessionArgs,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let mut backoff = run_session_backoff(args.retry_interval);
    loop {
        let session_shutdown = shutdown_rx.resubscribe();
        tokio::select! {
            _ = shutdown_rx.recv() => {
                args.connected.store(false, Ordering::Relaxed);
                return Ok(());
            }
            result = run_once(&mut args, session_shutdown) => {
                args.connected.store(false, Ordering::Relaxed);
                match result {
                    Ok(()) => return Ok(()),
                    Err(err) => {
                        if err
                            .downcast_ref::<crate::transport::websocket::AuthenticationRejected>()
                            .is_some()
                        {
                            let _ = args.fatal_tx.send(anyhow!(
                                "The token was rejected by the next hop"
                            ));
                            return Err(err);
                        }
                        if let Some(fatal) = err.downcast_ref::<FatalSessionError>() {
                            let _ = args.fatal_tx.send(anyhow!("{}", fatal.0));
                            return Err(err);
                        }
                        if err.downcast_ref::<ReconnectRequested>().is_some() {
                            // The switch is a configuration change, not a
                            // failure, so reconnect immediately and reset the
                            // retry backoff.
                            info!(
                                allow_upstream_modify = args
                                    .allow_upstream_modify
                                    .as_ref()
                                    .map(|rx| *rx.borrow())
                                    .unwrap_or(false),
                                "Reconnecting to announce the changed proxy capabilities"
                            );
                            backoff = run_session_backoff(args.retry_interval);
                            continue;
                        }
                        if args.fatal_tx.is_closed() {
                            return Err(err);
                        }
                        let delay = backoff
                            .next_backoff()
                            .unwrap_or(Duration::from_secs(args.retry_interval.max(1)));
                        warn!("{:#}. Retry in {:?}...", err, delay);
                        tokio::select! {
                            _ = shutdown_rx.recv() => return Ok(()),
                            _ = time::sleep(delay) => {}
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
struct FatalSessionError(anyhow::Error);

impl std::fmt::Display for FatalSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for FatalSessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

/// The session must reconnect at once, without the retry backoff, because the
/// local `allow_upstream_modify` switch changed and the next hop has to receive
/// the new capability bits in a fresh `Hello`.
#[derive(Debug)]
struct ReconnectRequested;

impl std::fmt::Display for ReconnectRequested {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "reconnecting to announce the changed capabilities")
    }
}

impl std::error::Error for ReconnectRequested {}

/// The proxy capability bits of the current attempt.
///
/// `CAP_PROXY_ADMIN` and `CAP_PROXY_AUTH` are always announced when the local
/// management worker exists: the next hop may still list the services.
/// `CAP_PROXY_WRITE` depends on `allow_upstream_modify` and is absent when
/// `HT_PROXIES` owns the list, which is announced with `CAP_PROXY_ENV`.
fn proxy_capabilities(args: &SessionArgs) -> u32 {
    if args.proxy_admin.is_none() {
        return 0;
    }
    let mut capabilities = p4::CAP_PROXY_ADMIN | p4::CAP_PROXY_AUTH;
    if args.proxies_managed {
        capabilities |= p4::CAP_PROXY_ENV;
    } else if args
        .allow_upstream_modify
        .as_ref()
        .map(|rx| *rx.borrow())
        .unwrap_or(false)
    {
        capabilities |= p4::CAP_PROXY_WRITE;
    }
    capabilities
}

async fn run_once(
    args: &mut SessionArgs,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    // Mark the current switch value as seen before connecting. Otherwise a
    // clone of a receiver that has never observed the last change would report
    // it as changed immediately and reconnect forever.
    if let Some(rx) = args.allow_upstream_modify.as_mut() {
        rx.borrow_and_update();
    }
    let mut sent: BTreeSet<String> = args.routes.borrow_and_update().keys().cloned().collect();
    let mut generation = 1u64;

    let mut conn = connect_websocket_v4(
        &args.server_addr,
        args.tls,
        &args.token,
        args.keepalive_interval,
        &args.node_id,
    )
    .await?;
    handshake(&mut conn, args).await?;
    args.connected.store(true, Ordering::Relaxed);
    info!(
        node = %args.node_id,
        routes = sent.len(),
        "v4 session established"
    );

    let (mut reader, mut writer) = split(conn);
    let (out_tx, mut out_rx) = mpsc::channel::<SessionFrame>(256);
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if p4::write_frame(&mut writer, &frame).await.is_err() {
                break;
            }
        }
    });
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Result<SessionFrame>>();
    let reader_task = tokio::spawn(async move {
        loop {
            let frame = p4::read_frame(&mut reader).await;
            let stop = frame.is_err();
            if in_tx.send(frame).is_err() || stop {
                break;
            }
        }
    });

    let mut streams: HashMap<u32, mpsc::UnboundedSender<StreamInput>> = HashMap::new();
    let (closed_tx, mut closed_rx) = mpsc::unbounded_channel::<u32>();
    let mut switch_rx = args.allow_upstream_modify.clone();

    let routes: Vec<RouteAdvert> = sent
        .iter()
        .map(|prefix| RouteAdvert {
            prefix: prefix.clone(),
            origin: args.node_id.clone(),
            hops: 0,
        })
        .collect();
    out_tx
        .send(SessionFrame::Advertise { generation, routes })
        .await
        .map_err(|_| anyhow!("The v4 session writer is gone"))?;

    let mut result: Result<()> = Err(anyhow!("The v4 session closed"));
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                let _ = out_tx.send(SessionFrame::GoAway {
                    reason: "node is shutting down".to_string(),
                    drain_seconds: 0,
                }).await;
                result = Ok(());
                break;
            }
            changed = wait_for_switch(&mut switch_rx) => {
                match changed {
                    Some(()) => {
                        info!("The allow_upstream_modify switch changed; reconnecting");
                        result = Err(ReconnectRequested.into());
                    }
                    None => {
                        // The management state is gone, so no further change is
                        // possible. The process is stopping anyway.
                        result = Ok(());
                    }
                }
                break;
            }
            changed = args.routes.changed() => {
                if changed.is_err() {
                    // The embedded server is stopping, so there is no route
                    // source left. Do not reconnect.
                    result = Ok(());
                    break;
                }
                let current: BTreeSet<String> = args.routes.borrow_and_update().keys().cloned().collect();
                let add: Vec<RouteAdvert> = current
                    .difference(&sent)
                    .map(|prefix| RouteAdvert {
                        prefix: prefix.clone(),
                        origin: args.node_id.clone(),
                        hops: 0,
                    })
                    .collect();
                let remove: Vec<String> = sent.difference(&current).cloned().collect();
                sent = current;
                if add.is_empty() && remove.is_empty() {
                    continue;
                }
                for prefix in &remove {
                    info!(domain = %prefix, "Withdrawing the mirrored route");
                }
                for advert in &add {
                    info!(domain = %advert.prefix, "Mirroring the route to the next hop");
                }
                generation += 1;
                if out_tx.send(SessionFrame::Update { generation, add, remove }).await.is_err() {
                    break;
                }
            }
            frame = in_rx.recv() => {
                let Some(frame) = frame else { break; };
                match frame {
                    Ok(frame) => match frame {
                        SessionFrame::OpenStream { stream_id, domain, header, trace_id, .. } => {
                            let request_id = request_id_from_trace_id(&trace_id);
                            let target = lookup_target(&args.routes.borrow(), &domain);
                            let Some(target) = target else {
                                warn!(
                                    request_id = %request_id,
                                    stream_id,
                                    host = %domain,
                                    "No local route matches the streamed visitor; resetting the stream"
                                );
                                // No local route is a routing/configuration problem,
                                // not a missing service: do not report it as a 502.
                                let _ = out_tx.send(SessionFrame::StreamReset {
                                    stream_id,
                                    reason: p4::StreamError::Protocol,
                                }).await;
                                continue;
                            };
                            if streams.len() >= p4::MAX_STREAMS_PER_SESSION {
                                warn!(
                                    request_id = %request_id,
                                    stream_id,
                                    limit = p4::MAX_STREAMS_PER_SESSION,
                                    "The v4 stream limit of the next hop is reached; resetting the stream"
                                );
                                let _ = out_tx.send(SessionFrame::StreamReset {
                                    stream_id,
                                    reason: p4::StreamError::Protocol,
                                }).await;
                                continue;
                            }
                            let (input_tx, input_rx) = mpsc::unbounded_channel();
                            streams.insert(stream_id, input_tx);
                            let out = out_tx.clone();
                            let closed = closed_tx.clone();
                            tokio::spawn(run_client_stream(
                                stream_id,
                                request_id,
                                args.node_id.clone(),
                                domain,
                                target,
                                header,
                                input_rx,
                                out,
                                closed,
                            ));
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
                        SessionFrame::RouteAck { generation: ack_generation, accepted, rejected } => {
                            for rejection in rejected {
                                warn!(
                                    generation = ack_generation,
                                    prefix = %rejection.prefix,
                                    reason = ?rejection.reason,
                                    "The next hop rejected a route"
                                );
                            }
                            debug!(
                                generation = ack_generation,
                                accepted = accepted.len(),
                                "The next hop accepted the advertised routes"
                            );
                        }
                        SessionFrame::Ping { seq } => {
                            let _ = out_tx.send(SessionFrame::Pong { seq }).await;
                        }
                        SessionFrame::Pong { .. } => {}
                        SessionFrame::GoAway { reason, .. } => {
                            info!("The next hop is going away: {}", reason);
                            break;
                        }
                        SessionFrame::Error { code, message } => {
                            if code == p4::ErrorCode::AuthFailed {
                                result = Err(FatalSessionError(anyhow!(
                                    "The token was rejected by the next hop"
                                )).into());
                            } else {
                                result = Err(anyhow!("The next hop reported {:?}: {}", code, message));
                            }
                            break;
                        }
                        SessionFrame::ProxyRequest { request_id, op } => {
                            let admin = args.proxy_admin.clone();
                            let allow_upstream_modify = args
                                .allow_upstream_modify
                                .as_ref()
                                .map(|rx| *rx.borrow())
                                .unwrap_or(false);
                            let proxies_managed = args.proxies_managed;
                            let out = out_tx.clone();
                            tokio::spawn(async move {
                                let (proxies, error) = match handle_proxy_request(
                                    admin,
                                    op,
                                    allow_upstream_modify,
                                    proxies_managed,
                                )
                                .await
                                {
                                    Ok(proxies) => (proxies, None),
                                    Err(error) => (Vec::new(), Some(error)),
                                };
                                let _ = out
                                    .send(SessionFrame::ProxyResponse {
                                        request_id,
                                        proxies,
                                        error,
                                    })
                                    .await;
                            });
                        }
                        other => {
                            warn!("Unexpected v4 frame: {:?}", other);
                            result = Err(anyhow!("Unexpected v4 frame"));
                            break;
                        }
                    },
                    Err(err) => {
                        debug!("v4 session read failed: {:#}", err);
                        break;
                    }
                }
            }
            id = closed_rx.recv() => {
                if let Some(id) = id {
                    streams.remove(&id);
                }
            }
        }
    }

    drop(streams);
    drop(out_tx);
    let _ = time::timeout(Duration::from_secs(1), writer_task).await;
    reader_task.abort();
    result
}

/// Render the `trace_id` of an incoming stream as a log correlation id. New
/// peers send the visitor's ASCII request id; an older peer may send arbitrary
/// bytes, which are hex-encoded instead of being mangled.
fn request_id_from_trace_id(trace_id: &[u8]) -> String {
    match std::str::from_utf8(trace_id) {
        Ok(id) if !id.is_empty() => id.to_string(),
        _ => trace_id
            .iter()
            .map(|byte| format!("{:02x}", byte))
            .collect(),
    }
}

/// Wait for a change of the optional `allow_upstream_modify` watch. Without a
/// management feature the future never resolves, so the `select!` branch stays
/// inactive.
async fn wait_for_switch(rx: &mut Option<watch::Receiver<bool>>) -> Option<()> {
    match rx {
        Some(rx) => rx.changed().await.ok(),
        None => std::future::pending().await,
    }
}

/// Run one proxy operation through the local management worker and convert its
/// result into the wire representation.
///
/// A `Put` or `Delete` is rejected here as well as in the worker: the capability
/// bits are advisory, and a next hop that ignores them must not be able to write
/// merely by sending a frame.
async fn handle_proxy_request(
    admin: Option<mpsc::Sender<ProxyAdminRequest>>,
    op: p4::ProxyOp,
    allow_upstream_modify: bool,
    proxies_managed: bool,
) -> std::result::Result<Vec<p4::ProxyEntry>, String> {
    let Some(admin) = admin else {
        return Err("the downstream node does not support remote proxy management".to_string());
    };
    let write = matches!(op, p4::ProxyOp::Put { .. } | p4::ProxyOp::Delete { .. });
    if write {
        if proxies_managed {
            return Err(
                "the local services are managed by HT_PROXIES and cannot be changed".to_string(),
            );
        }
        if !allow_upstream_modify {
            return Err(
                "the node does not allow its upstream to modify its local services; \
                 set `allow_upstream_modify = true` on that node"
                    .to_string(),
            );
        }
    }
    let (reply, reply_rx) = oneshot::channel();
    let request = match op {
        p4::ProxyOp::List => ProxyAdminRequest::List { reply },
        p4::ProxyOp::Put {
            name,
            domain,
            local_addr,
            auth_header,
        } => ProxyAdminRequest::Put {
            name,
            domain,
            local_addr,
            auth_header,
            reply,
        },
        p4::ProxyOp::Delete { name } => ProxyAdminRequest::Delete { name, reply },
    };
    match time::timeout(Duration::from_secs(5), admin.send(request)).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return Err("the local management worker is not running".to_string()),
        Err(_) => return Err("the local management worker is busy".to_string()),
    }
    match time::timeout(Duration::from_secs(10), reply_rx).await {
        Ok(Ok(Ok(proxies))) => Ok(proxies
            .into_iter()
            .map(|proxy| p4::ProxyEntry {
                name: proxy.name,
                domain: proxy.domain,
                local_addr: proxy.local_addr,
                auth_header: proxy.auth_header,
            })
            .collect()),
        Ok(Ok(Err(error))) => Err(format!("{:#}", error)),
        Ok(Err(_)) => Err("the local management worker dropped the request".to_string()),
        Err(_) => Err("the local management worker timed out".to_string()),
    }
}

/// Find the target of a stream, preferring an exact route and then the longest
/// wildcard suffix. A node advertises `*.relay.example.com` and receives streams
/// for the concrete domains below it.
fn lookup_target(routes: &RouteMap, host: &str) -> Option<RouteTarget> {
    if let Some(target) = routes.get(host) {
        return Some(target.clone());
    }
    let mut best: Option<(usize, &RouteTarget)> = None;
    for (prefix, target) in routes {
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
            best = Some((dot.len(), target));
        }
    }
    best.map(|(_, target)| target.clone())
}

async fn handshake(conn: &mut BoxedIo, args: &SessionArgs) -> Result<()> {
    p4::write_frame(
        conn,
        &SessionFrame::Hello {
            version: p4::V4_VERSION,
            node_id: args.node_id.clone(),
            role: args.role,
            capabilities: proxy_capabilities(args),
        },
    )
    .await?;
    let challenge = time::timeout(Duration::from_secs(5), p4::read_frame(conn))
        .await
        .context("Timed out waiting for the v4 challenge")??;
    let (server_id, nonce) = match challenge {
        SessionFrame::Challenge { server_id, nonce } => (server_id, nonce),
        SessionFrame::Error { code, message } => {
            return Err(anyhow!(
                "The next hop rejected the session: {:?} {}",
                code,
                message
            ));
        }
        other => bail!("Unexpected v4 handshake frame: {:?}", other),
    };
    let mut concat = Vec::from(args.token.as_bytes());
    concat.extend_from_slice(&nonce);
    concat.extend_from_slice(args.node_id.as_bytes());
    concat.extend_from_slice(server_id.as_bytes());
    let proof = crate::helper::digest(&concat).to_vec();
    p4::write_frame(conn, &SessionFrame::Auth { proof }).await?;
    let welcome = time::timeout(Duration::from_secs(5), p4::read_frame(conn))
        .await
        .context("Timed out waiting for the v4 welcome")??;
    match welcome {
        SessionFrame::Welcome { .. } => Ok(()),
        SessionFrame::Error { code, message } => {
            let err = anyhow!("The next hop rejected the session: {:?} {}", code, message);
            if code == p4::ErrorCode::AuthFailed {
                Err(FatalSessionError(err).into())
            } else {
                Err(err)
            }
        }
        other => bail!("Unexpected v4 welcome frame: {:?}", other),
    }
}

async fn run_client_stream(
    stream_id: u32,
    request_id: String,
    node_name: String,
    domain: String,
    target: RouteTarget,
    header: Vec<u8>,
    mut input_rx: mpsc::UnboundedReceiver<StreamInput>,
    out_tx: mpsc::Sender<SessionFrame>,
    closed_tx: mpsc::UnboundedSender<u32>,
) {
    let request = match crate::helper::gate_auth_header(&header, target.auth_header.as_deref()) {
        crate::helper::AuthOutcome::Allowed(request) => request,
        crate::helper::AuthOutcome::Unauthorized(failure) => {
            warn!(
                request_id = %request_id,
                stream_id,
                target = %target.local_addr,
                reason = ?failure,
                "Rejected an unauthenticated streamed visitor; answered 401"
            );
            let response = crate::http::unauthorized_response(failure, &request_id);
            let _ = out_tx
                .send(SessionFrame::StreamData {
                    stream_id,
                    data: response,
                    fin: true,
                })
                .await;
            let _ = closed_tx.send(stream_id);
            return;
        }
    };

    // A relay mirrors its routes to its own embedded listener; carrying the id
    // across that loopback hop keeps one id across every hop and every log.
    let request = if target.internal {
        crate::helper::insert_request_id(&request, &request_id)
    } else {
        request
    };

    let local = match crate::helper::connect_local(&target.local_addr).await {
        Ok(local) => local,
        Err(err) => {
            warn!(
                request_id = %request_id,
                stream_id,
                node = %node_name,
                host = %domain,
                target = %target.local_addr,
                "The v4 stream could not reach its local service: {:#}",
                err
            );
            let reason = crate::http::gateway_failure(
                "The tunnel reached this node, but the local service behind it is unavailable.",
                "local service (downstream node)",
                &[
                    ("node", &node_name),
                    ("tunnel", &domain),
                    ("target", &target.local_addr),
                    ("cause", &format!("{:#}", err)),
                ],
                "The tunnel session is healthy and this node received the request; only the \
                 service behind the tunnel could not be reached. Start it, or fix the target \
                 above on this node.",
            );
            let response = crate::http::error_response(&request_id, "502 Bad Gateway", &reason);
            let _ = out_tx
                .send(SessionFrame::StreamData {
                    stream_id,
                    data: response,
                    fin: true,
                })
                .await;
            let _ = closed_tx.send(stream_id);
            return;
        }
    };

    let (mut reader, mut writer) = split(local);
    if let Err(err) = writer.write_all(&request).await {
        warn!(
            request_id = %request_id,
            stream_id,
            node = %node_name,
            host = %domain,
            target = %target.local_addr,
            "The v4 stream failed to write the request to its local service: {:#}",
            err
        );
        let reason = crate::http::gateway_failure(
            "The local service behind this tunnel accepted the connection, then closed it.",
            "local service (downstream node)",
            &[
                ("node", &node_name),
                ("tunnel", &domain),
                ("target", &target.local_addr),
                ("cause", &format!("{:#}", err)),
            ],
            "The tunnel session is healthy; the service behind it dropped the request before \
             reading it. Check that service on this node.",
        );
        let response = crate::http::error_response(&request_id, "502 Bad Gateway", &reason);
        let _ = out_tx
            .send(SessionFrame::StreamData {
                stream_id,
                data: response,
                fin: true,
            })
            .await;
        let _ = closed_tx.send(stream_id);
        return;
    }

    let mut buffer = vec![0u8; p4::STREAM_CHUNK];
    let mut credit = p4::INITIAL_WINDOW;
    let mut outbound_done = false;
    let mut inbound_done = false;

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
                            "v4 stream local read failed: {:#}",
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
                        debug!(
                            "v4 stream {} was reset by the next hop: {:?}",
                            stream_id, reason
                        );
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
}
