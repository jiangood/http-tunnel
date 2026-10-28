use anyhow::{anyhow, Context, Result};
use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::cli::{NodeArgs, ServerArgs};
use crate::config::NodeConfig;
use crate::helper::to_bind_addr;
use crate::protocol_v4::Role;
use crate::server::run_server_with_listener_and_routes;
use crate::session_client::{self, RouteMap, RouteTarget, SessionArgs};

/// Run a node: an embedded server plus one optional upstream v4 session.
///
/// The data plane is recursive, so an upstream session forwards the visitors it
/// receives to `127.0.0.1:<server_port>`, where the embedded server routes them
/// by `Host` again. A node without `[upstream]` is an ingress and behaves like
/// `hts`. With `[upstream].mirror = true` it advertises every route of its
/// downstream peers and its local proxies; with `[upstream].claim` it advertises
/// one exact or wildcard domain.
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

    // Bind before starting the upstream session, so that the route is never
    // advertised before this node can serve the visitors it receives.
    let bind_addr = to_bind_addr(&config.bind_host, config.server_port);
    let http_listener = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("Failed to listen at `server_port` ({})", bind_addr))?;

    // The node owns the shutdown of the embedded server, so that it can close
    // the upstream session before the server drains the in-flight visitors.
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
        Some(upstream) => {
            run_relay(
                &config,
                upstream,
                route_rx,
                server_task,
                stop_tx,
                shutdown_rx,
            )
            .await
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

/// A relay advertises its routes on one v4 session to its next hop.
async fn run_relay(
    config: &NodeConfig,
    upstream: crate::config::UpstreamConfig,
    route_rx: watch::Receiver<BTreeSet<String>>,
    mut server_task: JoinHandle<Result<()>>,
    stop_tx: broadcast::Sender<bool>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let local_target = RouteTarget {
        local_addr: format!("127.0.0.1:{}", config.server_port),
        auth_header: None,
    };

    // Keep the claim route watch's sender alive for the whole session; a
    // dropped sender makes the session believe its route source is gone.
    let mut _claim_routes = None;
    let routes = if let Some(claim) = upstream.claim() {
        info!(
            upstream = %upstream.server,
            claim,
            "Advertising one upstream route; visitors are forwarded to 127.0.0.1:{}",
            config.server_port
        );
        let (tx, rx) = watch::channel(RouteMap::new());
        let mut map = RouteMap::new();
        map.insert(claim.to_string(), local_target);
        tx.send_replace(map);
        _claim_routes = Some(tx);
        rx
    } else {
        info!(
            upstream = %upstream.server,
            "Advertising the downstream routes; visitors are forwarded to 127.0.0.1:{}",
            config.server_port
        );
        mirrored_routes(route_rx, local_target, config.max_mirrored_routes)
    };

    let (fatal_tx, mut fatal_rx) = mpsc::unbounded_channel::<anyhow::Error>();
    let (session_stop_tx, session_stop_rx) = broadcast::channel::<bool>(1);
    let session_args = SessionArgs {
        server_addr: upstream
            .server_addr()
            .expect("the configuration validates the upstream address"),
        tls: upstream.tls(),
        token: upstream.token.clone(),
        node_id: config.name.clone(),
        role: Role::Node,
        keepalive_interval: config.keepalive_interval,
        retry_interval: config.retry_interval,
        routes,
        fatal_tx,
        connected: Arc::new(AtomicBool::new(false)),
    };
    let mut session_task = tokio::spawn(session_client::run(session_args, session_stop_rx));

    let mut server_stopped = None;
    let mut session_result = None;
    let mut session_done = false;
    let mut fatal_error = None;
    tokio::select! {
        res = &mut server_task => server_stopped = Some(res),
        res = &mut session_task => {
            session_done = true;
            session_result = Some(flatten_task(res, "session"));
        }
        err = fatal_rx.recv() => fatal_error = err,
        _ = shutdown_rx.recv() => {}
    }

    // Withdraw the advertised routes before the embedded server drains the
    // in-flight visitors.
    if !session_done {
        info!("Withdrawing the upstream routes");
        let _ = session_stop_tx.send(true);
        let _ = session_task.await;
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
    if let Some(result) = session_result {
        result?;
    }
    Ok(())
}

/// Turn the set of routes served by the embedded server into targets that all
/// point at its loopback listener.
fn mirrored_routes(
    mut route_rx: watch::Receiver<BTreeSet<String>>,
    target: RouteTarget,
    max_routes: usize,
) -> watch::Receiver<RouteMap> {
    let (tx, rx) = watch::channel(RouteMap::new());
    let limit = if max_routes == 0 {
        usize::MAX
    } else {
        max_routes
    };
    tokio::spawn(async move {
        loop {
            let map: RouteMap = route_rx
                .borrow_and_update()
                .iter()
                .take(limit)
                .map(|domain| (domain.clone(), target.clone()))
                .collect();
            tx.send_replace(map);
            if route_rx.changed().await.is_err() {
                break;
            }
        }
    });
    rx
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
            // anyway; the upstream session reports the failure.
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
