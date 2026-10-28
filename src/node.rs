use anyhow::{anyhow, Context, Result};
use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::config::Config;
use crate::helper::to_bind_addr;
use crate::protocol_v4::Role;
use crate::server::{run_server_with_listener_and_state, ServerState};
use crate::session_client::{self, RouteMap, RouteTarget, SessionArgs};

/// Run the universal process: an embedded server plus one optional upstream
/// v4 session.
///
/// The data plane is recursive, so an upstream session forwards the visitors it
/// receives to `127.0.0.1:<actual server_port>`, where the embedded server
/// routes them by `Host` again. Without `[upstream]` the process is a
/// standalone ingress. With `[upstream].mirror` it advertises every route of
/// its downstream peers and its local `[[proxies]]`; with `[upstream].claim` it
/// advertises one exact or wildcard domain.
pub(crate) async fn run_router(
    config: Config,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    // Bind before starting the upstream session, so that the route is never
    // advertised before this process can serve the visitors it receives.
    let bind_addr = to_bind_addr(&config.bind_host, config.server_port);
    let http_listener = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("Failed to listen at `server_port` ({})", bind_addr))?;
    let actual_port = http_listener
        .local_addr()
        .context("Failed to read the bound listener address")?
        .port();

    ensure_upstream_is_not_self(&config, actual_port).await?;

    if let (Some(token), Some(upstream)) = (&config.token, &config.upstream) {
        if token == &upstream.token {
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
        server_host = %config.server_host(),
        server_port = actual_port,
        "ht starting"
    );

    let token = config
        .listener_token()
        .cloned()
        .ok_or_else(|| anyhow!("The listener token is missing"))?;
    let (routes, route_rx) = watch::channel(BTreeSet::new());
    let state = Arc::new(ServerState::new(
        token,
        config.server_host().to_string(),
        config.server_limits(),
        routes,
        config.proxies.clone(),
    ));
    state.refresh_routes().await;

    // A standalone ingress has no upstream to wait for and is always ready.
    let connected = Arc::new(AtomicBool::new(config.upstream.is_none()));

    // The management page and the API share the reserved `server_host` of the
    // listener, so they need no port of their own.
    #[cfg(feature = "management")]
    let management = Some(Arc::new(crate::management::ManagementState::new(
        config.clone(),
        state.clone(),
        connected.clone(),
    )));
    #[cfg(not(feature = "management"))]
    let management = None;

    // The router owns the shutdown of the embedded server, so that it can
    // close the upstream session before the server drains in-flight visitors.
    let (stop_tx, _) = broadcast::channel::<bool>(1);
    let server_task = tokio::spawn(run_server_with_listener_and_state(
        http_listener,
        state.clone(),
        management,
        stop_tx.subscribe(),
    ));

    let runtime = RouterRuntime {
        server_task,
        stop_tx,
        shutdown_rx,
        connected,
    };

    match config.upstream.clone() {
        None => run_ingress(runtime).await,
        Some(upstream) => run_relay(&config, upstream, actual_port, route_rx, runtime).await,
    }
}

/// The tasks and channels the router has started and must shut down.
struct RouterRuntime {
    server_task: JoinHandle<Result<()>>,
    stop_tx: broadcast::Sender<bool>,
    shutdown_rx: broadcast::Receiver<bool>,
    connected: Arc<AtomicBool>,
}

/// An ingress serves the visitors and downstream peers on its own.
async fn run_ingress(mut runtime: RouterRuntime) -> Result<()> {
    info!("No upstream is configured, so ht is a standalone ingress");
    let mut server_stopped = None;
    tokio::select! {
        res = &mut runtime.server_task => server_stopped = Some(res),
        _ = runtime.shutdown_rx.recv() => {}
    }

    let _ = runtime.stop_tx.send(true);
    let server_result = match server_stopped {
        Some(res) => flatten_server(res),
        None => flatten_server(runtime.server_task.await),
    };
    server_result
}

/// A relay advertises its route set on one v4 session to its next hop.
async fn run_relay(
    config: &Config,
    upstream: crate::config::UpstreamConfig,
    actual_port: u16,
    route_rx: watch::Receiver<BTreeSet<String>>,
    runtime: RouterRuntime,
) -> Result<()> {
    let RouterRuntime {
        mut server_task,
        stop_tx,
        mut shutdown_rx,
        connected,
    } = runtime;

    let local_target = RouteTarget {
        local_addr: format!("127.0.0.1:{}", actual_port),
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
            actual_port
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
            "Advertising the mirrored routes; visitors are forwarded to 127.0.0.1:{}",
            actual_port
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
        connected,
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
        Some(err) = fatal_rx.recv() => fatal_error = Some(err),
        _ = shutdown_rx.recv() => {}
    }

    // Withdraw the advertised routes before the embedded server drains the
    // in-flight visitors.
    if !session_done {
        info!("Withdrawing the upstream routes");
        let _ = session_stop_tx.send(true);
        let _ = session_task.await;
    }

    let _ = stop_tx.send(true);
    let server_result = match server_stopped {
        Some(res) => flatten_server(res),
        None => flatten_server(server_task.await),
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

/// Fail startup when the upstream endpoint is this process's own listener. The
/// configuration validation already rejects the reserved host and the literal
/// loopback forms when the port is known; this catches a DNS name that resolves
/// back to the local machine and the actual port of an ephemeral listener.
async fn ensure_upstream_is_not_self(config: &Config, actual_port: u16) -> Result<()> {
    let Some(upstream) = config.upstream.as_ref() else {
        return Ok(());
    };
    let (host, port) = upstream.host_port()?;
    if port != actual_port {
        return Ok(());
    }

    let bare = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(&host);
    let addrs = match tokio::net::lookup_host((bare, port)).await {
        Ok(addrs) => addrs,
        Err(e) => {
            // A transient DNS failure must not stop a process that would retry
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
                "The upstream `{}` resolves to {}, which is this process's own listener; \
                 configure the next hop instead of itself",
                upstream.server,
                addr
            );
        }
    }
    Ok(())
}

/// Turn the result of the embedded server task into the result of the process.
fn flatten_server(result: std::result::Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    flatten_task(result, "server").context("The embedded server stopped")
}

/// Flatten a spawned task result, naming the task in a join failure.
fn flatten_task(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
    what: &str,
) -> Result<()> {
    result
        .map_err(|e| anyhow!("The {} task failed: {}", what, e))
        .and_then(|result| result)
}
