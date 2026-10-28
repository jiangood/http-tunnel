use anyhow::{anyhow, Context, Result};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::cli::{NodeArgs, ServerArgs};
use crate::client::ControlChannelHandle;
use crate::config::{domain_overlaps_server_host, ClientTunnelConfig, NodeConfig, UpstreamConfig};
use crate::helper::to_bind_addr;
use crate::server::run_server_with_listener_and_routes;

/// Run a node: an embedded server plus one optional upstream hop.
///
/// The data plane is recursive, so an upstream tunnel forwards the visitors it
/// receives to `127.0.0.1:<server_port>`, where the embedded server routes them
/// by `Host` again. A node without `[upstream]` is an ingress and behaves like
/// `hts`. With `[upstream].mirror = true` it mirrors every route of its
/// downstream clients (and its local proxies) to the next hop, one control
/// channel per domain; with `[upstream].claim` it claims one exact or wildcard
/// domain instead.
pub(crate) async fn run_node(args: NodeArgs, shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
    let config = args.config;
    ensure_upstream_is_not_self(&config).await?;

    if let Some(upstream) = &config.upstream {
        if upstream.token == config.token {
            warn!(
                "The downstream `token` and `[upstream].token` are the same; use a different \
                 token per hop so that each hop has its own trust boundary"
            );
        }
    }

    let role = if config.upstream.is_some() {
        "relay"
    } else {
        "ingress"
    };
    info!(
        name = %config.name,
        role = %role,
        server_host = %config.server_host,
        server_port = config.server_port,
        "Node starting"
    );

    let server_args = ServerArgs {
        server_port: config.server_port,
        server_host: config.server_host.clone(),
        bind_host: config.bind_host.clone(),
        token: String::from(&*config.token),
        proxies: config.proxies.clone(),
        limits: config.server_limits(),
    };

    // Bind before starting the upstream tunnel, so that the route is never
    // claimed before this node can serve the visitors it receives.
    let bind_addr = to_bind_addr(&config.bind_host, config.server_port);
    let http_listener = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("Failed to listen at `server_port` ({})", bind_addr))?;

    // The node owns the shutdown of the embedded server, so that it can withdraw
    // its upstream routes before the server drains the in-flight visitors.
    let (stop_tx, _) = broadcast::channel::<bool>(1);
    let (route_tx, route_rx) = watch::channel(BTreeSet::new());
    let server_task = tokio::spawn(run_server_with_listener_and_routes(
        http_listener,
        server_args,
        stop_tx.subscribe(),
        route_tx,
    ));

    match config.upstream.clone() {
        None => run_ingress(server_task, stop_tx, shutdown_rx).await,
        Some(upstream) if upstream.mirror => {
            run_mirror_relay(
                &config,
                upstream,
                server_task,
                route_rx,
                stop_tx,
                shutdown_rx,
            )
            .await
        }
        Some(upstream) => {
            run_claim_relay(&config, upstream, server_task, stop_tx, shutdown_rx).await
        }
    }
}

/// An ingress serves the visitors and downstream peers on its own.
async fn run_ingress(
    mut server_task: JoinHandle<Result<()>>,
    stop_tx: broadcast::Sender<bool>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    info!("The node has no upstream, so it is a public ingress");
    tokio::select! {
        res = &mut server_task => return flatten_server(res),
        _ = shutdown_rx.recv() => {}
    }
    let _ = stop_tx.send(true);
    flatten_server(server_task.await)
}

/// A Stage 1 relay claims one exact or wildcard domain on the next hop.
async fn run_claim_relay(
    config: &NodeConfig,
    upstream: UpstreamConfig,
    mut server_task: JoinHandle<Result<()>>,
    stop_tx: broadcast::Sender<bool>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let claim = upstream
        .claim()
        .expect("the configuration validates that claim or mirror is set");
    info!(
        upstream = %upstream.server,
        claim,
        "Claiming one upstream route; visitors are forwarded to 127.0.0.1:{}",
        config.server_port
    );

    // A definitive upstream failure (a rejected token) is reported here.
    let (fatal_tx, mut fatal_rx) = mpsc::unbounded_channel::<anyhow::Error>();
    let handle = spawn_upstream_tunnel(config, &upstream, claim.to_string(), fatal_tx);

    let mut fatal_error = None;
    let server_stopped = tokio::select! {
        res = &mut server_task => Some(res),
        err = fatal_rx.recv() => {
            fatal_error = err;
            None
        }
        _ = shutdown_rx.recv() => None,
    };

    // Withdraw the upstream route first, so no new visitor is sent to this node
    // while the embedded server waits for the in-flight ones.
    info!("Withdrawing the upstream route");
    handle.shutdown_and_wait().await;

    let server_result = match server_stopped {
        Some(res) => flatten_server(res),
        None => {
            let _ = stop_tx.send(true);
            flatten_server(server_task.await)
        }
    };

    if let Some(err) = fatal_error {
        return Err(err);
    }
    server_result
}

/// A Stage 2 relay mirrors the routes of its embedded server to the next hop,
/// one control channel per domain.
async fn run_mirror_relay(
    config: &NodeConfig,
    upstream: UpstreamConfig,
    mut server_task: JoinHandle<Result<()>>,
    route_rx: watch::Receiver<BTreeSet<String>>,
    stop_tx: broadcast::Sender<bool>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    info!(
        upstream = %upstream.server,
        "Mirroring the downstream routes; visitors are forwarded to 127.0.0.1:{}",
        config.server_port
    );

    let (fatal_tx, mut fatal_rx) = mpsc::unbounded_channel::<anyhow::Error>();
    let (mirror_stop_tx, mirror_stop_rx) = broadcast::channel::<bool>(1);
    // The upstream URL host is the `server_host` of the next hop; routes that
    // overlap it are reserved for the tunnel endpoint and cannot be registered.
    let (upstream_host, _) = upstream
        .host_port()
        .expect("the configuration validates the upstream address");
    let upstream_host = if upstream_host.contains(':') && !upstream_host.starts_with('[') {
        format!("[{}]", upstream_host)
    } else {
        upstream_host
    };
    let upstream_host = upstream_host.to_ascii_lowercase();
    let mirror_args = MirrorArgs {
        upstream,
        upstream_host,
        node_name: config.name.clone(),
        local_addr: format!("127.0.0.1:{}", config.server_port),
        keepalive_interval: config.keepalive_interval,
        heartbeat_timeout: config.heartbeat_timeout,
        retry_interval: config.retry_interval,
        max_routes: config.max_mirrored_routes,
        fatal_tx,
    };
    let mut mirror_task = tokio::spawn(run_mirror(route_rx, mirror_args, mirror_stop_rx));

    let mut server_stopped = None;
    let mut mirror_result = None;
    let mut mirror_done = false;
    let mut fatal_error = None;
    tokio::select! {
        res = &mut server_task => server_stopped = Some(res),
        res = &mut mirror_task => {
            mirror_done = true;
            mirror_result = Some(flatten_task(res, "mirror"));
        }
        err = fatal_rx.recv() => fatal_error = err,
        _ = shutdown_rx.recv() => {}
    }

    // Withdraw every mirrored route before the embedded server drains the
    // in-flight visitors.
    if !mirror_done {
        let _ = mirror_stop_tx.send(true);
        let _ = mirror_task.await;
    }

    let server_result = match server_stopped {
        Some(res) => flatten_server(res),
        None => {
            let _ = stop_tx.send(true);
            flatten_server(server_task.await)
        }
    };

    if let Some(err) = fatal_error {
        return Err(err);
    }
    server_result?;
    if let Some(result) = mirror_result {
        result?;
    }
    Ok(())
}

/// The settings every mirrored tunnel shares.
struct MirrorArgs {
    upstream: UpstreamConfig,
    // The reserved `server_host` of the next hop, used to skip routes that can
    // never be registered there.
    upstream_host: String,
    node_name: String,
    local_addr: String,
    keepalive_interval: u64,
    heartbeat_timeout: u64,
    retry_interval: u64,
    // The maximum number of routes to mirror; 0 means unlimited.
    max_routes: usize,
    fatal_tx: mpsc::UnboundedSender<anyhow::Error>,
}

/// Keep one upstream control channel per route of the embedded server.
async fn run_mirror(
    mut routes: watch::Receiver<BTreeSet<String>>,
    args: MirrorArgs,
    mut stop_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let mut handles: HashMap<String, ControlChannelHandle> = HashMap::new();
    let mut skipped: HashSet<String> = HashSet::new();
    let mut cap_warned = false;
    loop {
        let current = routes.borrow_and_update().clone();
        reconcile_mirror(&mut handles, &mut skipped, &mut cap_warned, &current, &args).await;

        tokio::select! {
            changed = routes.changed() => {
                // The embedded server is gone, so there is nothing left to
                // mirror; the handles are withdrawn below.
                if changed.is_err() {
                    break;
                }
            }
            _ = stop_rx.recv() => break,
        }
    }

    // Withdraw the remaining routes concurrently: a disconnected tunnel waits
    // for its reconnect backoff, and one slow route must not delay the others.
    let closing: Vec<_> = handles
        .drain()
        .map(|(_, handle)| handle.shutdown_and_wait())
        .collect();
    futures_util::future::join_all(closing).await;
    Ok(())
}

/// Open the control channels of the new routes and close the ones that are
/// gone. Closing first matters when a domain moves to another downstream: the
/// new route is not published until the old claim is withdrawn.
async fn reconcile_mirror(
    handles: &mut HashMap<String, ControlChannelHandle>,
    skipped: &mut HashSet<String>,
    cap_warned: &mut bool,
    routes: &BTreeSet<String>,
    args: &MirrorArgs,
) {
    let limit = if args.max_routes == 0 {
        routes.len()
    } else {
        args.max_routes.min(routes.len())
    };
    if routes.len() > limit {
        if !*cap_warned {
            warn!(
                routes = routes.len(),
                max = args.max_routes,
                "The route set exceeds `max_mirrored_routes`; only the first {} domains \
                 (in lexicographic order) are mirrored",
                limit
            );
            *cap_warned = true;
        }
    } else {
        *cap_warned = false;
    }

    // A deterministic prefix of the route set is mirrored, so the same domains
    // stay reachable across reconciles while the set is over the cap.
    let allowed: Vec<String> = routes.iter().take(limit).cloned().collect();
    let allowed_set: HashSet<&str> = allowed.iter().map(String::as_str).collect();

    let stale: Vec<String> = handles
        .keys()
        .filter(|domain| !allowed_set.contains(domain.as_str()))
        .cloned()
        .collect();
    let mut closing = Vec::with_capacity(stale.len());
    for domain in stale {
        if let Some(handle) = handles.remove(&domain) {
            info!(domain = %domain, "Withdrawing the mirrored route");
            closing.push(handle.shutdown_and_wait());
        }
    }
    // Forget the skip record of routes that are gone, so a route that comes
    // back is evaluated again.
    skipped.retain(|domain| allowed_set.contains(domain.as_str()));
    futures_util::future::join_all(closing).await;

    for domain in &allowed {
        if handles.contains_key(domain) {
            continue;
        }
        if domain_overlaps_server_host(domain, &args.upstream_host) {
            // The next hop reserves this host for its control endpoint; warn
            // once instead of on every route change.
            if skipped.insert(domain.clone()) {
                warn!(
                    domain = %domain,
                    upstream_host = %args.upstream_host,
                    "Skipping a route that overlaps the next hop's server_host"
                );
            }
            continue;
        }
        info!(domain = %domain, "Mirroring the route to the next hop");
        let handle = spawn_mirrored_tunnel(args, domain.clone());
        handles.insert(domain.clone(), handle);
    }
    if routes.len() > limit {
        tracing::debug!(
            not_mirrored = routes.len() - limit,
            "Routes beyond `max_mirrored_routes` are not mirrored"
        );
    }
}

fn spawn_mirrored_tunnel(args: &MirrorArgs, domain: String) -> ControlChannelHandle {
    let upstream = &args.upstream;
    ControlChannelHandle::new(
        ClientTunnelConfig {
            // The tunnel identity is only used for logs and the digest; the
            // domain distinguishes the mirrored channels of one node.
            name: format!("mirror:{domain}"),
            domain,
            local_addr: args.local_addr.clone(),
            // The upstream token authenticates the control channel, not the
            // visitors that arrive through it.
            auth_header: None,
            nodelay: true,
            retry_interval: args.retry_interval,
        },
        args.node_name.clone(),
        upstream.token.clone(),
        upstream
            .server_addr()
            .expect("the configuration validates the upstream address"),
        if upstream.tls() { "wss" } else { "ws" }.to_string(),
        upstream.tls(),
        args.keepalive_interval,
        args.heartbeat_timeout,
        Arc::new(AtomicUsize::new(0)),
        args.fatal_tx.clone(),
    )
}

/// Create the single upstream tunnel of a Stage 1 relay.
fn spawn_upstream_tunnel(
    config: &NodeConfig,
    upstream: &UpstreamConfig,
    domain: String,
    fatal_tx: mpsc::UnboundedSender<anyhow::Error>,
) -> ControlChannelHandle {
    ControlChannelHandle::new(
        ClientTunnelConfig {
            name: "upstream".to_string(),
            domain,
            local_addr: format!("127.0.0.1:{}", config.server_port),
            auth_header: None,
            nodelay: true,
            retry_interval: config.retry_interval,
        },
        config.name.clone(),
        upstream.token.clone(),
        upstream
            .server_addr()
            .expect("the configuration validates the upstream address"),
        if upstream.tls() { "wss" } else { "ws" }.to_string(),
        upstream.tls(),
        config.keepalive_interval,
        config.heartbeat_timeout,
        Arc::new(AtomicUsize::new(0)),
        fatal_tx,
    )
}

/// Fail startup when the upstream endpoint is this node's own listener. The
/// config validation already rejects the reserved host and the literal loopback
/// forms; this catches a DNS name that resolves back to the local machine.
async fn ensure_upstream_is_not_self(config: &NodeConfig) -> Result<()> {
    let Some(upstream) = config.upstream.as_ref() else {
        return Ok(());
    };
    let (host, port) = upstream.host_port()?;
    if port != config.server_port {
        return Ok(());
    }

    let bare = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(&host);
    let addrs = match tokio::net::lookup_host((bare, port)).await {
        Ok(addrs) => addrs,
        Err(e) => {
            // A transient DNS failure must not stop a node that would retry
            // anyway; the upstream tunnel reports the failure.
            warn!(
                "Failed to resolve the upstream host `{}`: {:#}. The self-loop check is skipped",
                host, e
            );
            return Ok(());
        }
    };
    for addr in addrs {
        if addr.ip().is_loopback() || addr.ip().is_unspecified() {
            anyhow::bail!(
                "The upstream `{}` resolves to {}, which is this node's own listener; \
                 a node must dial its next hop, not itself",
                upstream.server,
                addr
            );
        }
    }
    Ok(())
}

/// Turn the result of the embedded server task into the result of the node.
fn flatten_server(result: std::result::Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    flatten_task(result, "server").context("The embedded server stopped")
}

/// Flatten a spawned task result, naming the task in a join failure.
fn flatten_task(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
    what: &str,
) -> Result<()> {
    result
        .map_err(|e| anyhow!("The node {} task failed: {}", what, e))
        .and_then(|result| result)
}
