use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time;
use tracing::{info, warn};

use crate::config::Config;
use crate::helper::to_bind_addr;
use crate::protocol_v4::Role;
use crate::reload::{ReloadMarker, ReloadStatus};
use crate::server::{run_server_with_listener_and_state, Management, ServerState};
#[cfg(feature = "management")]
use crate::session_client::ProxyAdminRequest;
use crate::session_client::{self, RouteMap, RouteTarget, SessionArgs};

/// How many times a reload retries binding a new listener. A port the process
/// used earlier may still have connections in `TIME_WAIT`, and Windows refuses
/// the bind until they settle.
const REBIND_ATTEMPTS: usize = 20;
const REBIND_DELAY: Duration = Duration::from_millis(100);

/// A change that the running generation can apply without restarting.
#[cfg(feature = "management")]
type LiveApplyReceiver = mpsc::Receiver<Config>;
#[cfg(not(feature = "management"))]
type LiveApplyReceiver = ();

/// Run the universal process and reload it when its configuration file changes.
///
/// The process itself never exits for a configuration change: the current
/// generation (listener plus optional upstream session) is stopped, the new file
/// is applied, and a new generation starts. A file edited by the management API
/// is skipped because that API already applied the change live.
pub(crate) async fn run(config: Config, shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
    run_supervised(config, shutdown_rx).await
}

async fn run_supervised(
    mut config: Config,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let marker = Arc::new(ReloadMarker::default());
    let reload_status = Arc::new(ReloadStatus::new());
    let (mut reload_rx, watcher) = match config.source.path.clone() {
        Some(path) => {
            let (rx, task) = crate::reload::watch(
                path,
                marker.clone(),
                reload_status.clone(),
                crate::reload::WATCH_INTERVAL,
            );
            (Some(rx), Some(task))
        }
        None => (None, None),
    };
    // The management API reports a configuration it already applied live, so
    // the supervisor keeps comparing new file contents against what is running.
    // Each message names the generation that sent it: a write that raced with a
    // reload must not overwrite the baseline of the generation that replaced it.
    let (live_tx, live_rx) = mpsc::channel::<(u64, Config)>(8);
    let mut live_rx = Some(live_rx);
    // A manual restart from the management page rebuilds the runtime with the
    // configuration on disk, even when it equals the running one.
    let (restart_tx, mut restart_rx) = mpsc::channel::<(u64, Config)>(1);
    let mut generation_id = 0u64;

    // The listener stays bound across reloads when its port and address do not
    // change. Holding it here means restarting the embedded server does not
    // close the port, so a same-port reload never refuses a connection.
    //
    // A configuration built in memory (a library caller or a test) has no file
    // to watch; it still runs through the same generation code below.
    let mut listener = match config.listen_port() {
        Some(_) => {
            let listener = Arc::new(bind_listener(&config, false).await?);
            ensure_upstream_is_not_self(&config, listener_port(&listener)?).await?;
            Some(listener)
        }
        None => None,
    };

    loop {
        generation_id = generation_id.wrapping_add(1);
        // The management API sends a configuration here after it applies and
        // persists it live, so a proxy or switch edit does not restart the
        // listener. The receiver is consumed by `run_router`.
        #[cfg(feature = "management")]
        let (apply_tx, apply_rx) = mpsc::channel::<Config>(1);
        #[cfg(not(feature = "management"))]
        let apply_rx: LiveApplyReceiver = ();

        let (stop_tx, stop_rx) = broadcast::channel::<bool>(1);
        let mut generation = tokio::spawn(run_router(
            config.clone(),
            listener.clone(),
            stop_rx,
            marker.clone(),
            reload_status.clone(),
            apply_rx,
            generation_id,
            Some(live_tx.clone()),
            restart_tx.clone(),
        ));

        let (candidate, next_listener) = loop {
            tokio::select! {
                result = &mut generation => {
                    if let Some(watcher) = watcher.as_ref() {
                        watcher.abort();
                    }
                    return flatten_task(result, "router");
                }
                _ = shutdown_rx.recv() => {
                    let _ = stop_tx.send(true);
                    let _ = generation.await;
                    if let Some(watcher) = watcher.as_ref() {
                        watcher.abort();
                    }
                    return Ok(());
                }
                live = next_message(&mut live_rx) => {
                    match live {
                        // The management API already applied this change.
                        Some((id, live)) if id == generation_id => config = live,
                        // A write that raced with a reload belongs to the
                        // generation that was just replaced.
                        Some(_) => {}
                        None => live_rx = None,
                    }
                }
                changed = next_message(&mut reload_rx) => {
                    match changed {
                        None => reload_rx = None,
                        Some(candidate)
                            if crate::reload::same_effective_config(&candidate, &config) =>
                        {
                            // The file changed but describes the running
                            // configuration (for example after a failed edit was
                            // reverted). The old error is no longer relevant.
                            reload_status.record_success();
                        }
                        Some(candidate) => {
                            // A proxy or `[upstream].allow_modify` edit is applied
                            // to the running generation, exactly like the same
                            // change through the management API.
                            #[cfg(feature = "management")]
                            if crate::reload::only_live_fields_changed(&config, &candidate)
                                && apply_tx.send(candidate.clone()).await.is_ok()
                            {
                                config = candidate;
                                marker.clear().await;
                                reload_status.record_success();
                                continue;
                            }

                            // Plan the next listener while the running
                            // generation still serves. A failure leaves the
                            // running configuration untouched.
                            match plan_next_generation(&config, &candidate, listener.as_deref()).await {
                                Ok(next_listener) => break (candidate, next_listener),
                                Err(err) => {
                                    warn!(
                                        "Failed to apply the changed configuration, \
                                         keeping the running one: {:#}",
                                        err
                                    );
                                    reload_status.record_error(format!(
                                        "Failed to apply the changed configuration: {:#}",
                                        err
                                    ));
                                    continue;
                                }
                            }
                        }
                    }
                }
                restart = restart_rx.recv() => {
                    match restart {
                        // A request from a generation that a reload already
                        // replaced describes stale configuration.
                        Some((id, _)) if id != generation_id => {}
                        Some((_, candidate)) => {
                            info!("A manual restart was requested; rebuilding the runtime");
                            match plan_next_generation(&config, &candidate, listener.as_deref()).await {
                                Ok(next_listener) => break (candidate, next_listener),
                                Err(err) => {
                                    warn!(
                                        "Failed to restart with the saved configuration, \
                                         keeping the running one: {:#}",
                                        err
                                    );
                                    reload_status.record_error(format!(
                                        "Failed to restart with the saved configuration: {:#}",
                                        err
                                    ));
                                }
                            }
                        }
                        None => {}
                    }
                }
            }
        };

        info!("Rebuilding the runtime; the process keeps running");
        let _ = stop_tx.send(true);
        match generation.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!(
                "The previous runtime stopped with an error while reloading: {:#}",
                err
            ),
            Err(err) => warn!("The previous runtime task failed while reloading: {}", err),
        }
        marker.clear().await;
        // Messages from the generation that just stopped describe the previous
        // configuration; the candidate below replaces them.
        if let Some(rx) = live_rx.as_mut() {
            while rx.try_recv().is_ok() {}
        }

        let applied = match next_listener {
            ListenerPlan::Reuse => true,
            ListenerPlan::Prebound(bound) => {
                listener = Some(bound);
                true
            }
            ListenerPlan::Disable => {
                listener = None;
                true
            }
            ListenerPlan::Rebind => {
                let (bound, applied) = rebind_listener(
                    &candidate,
                    &config,
                    listener
                        .take()
                        .expect("a rebind always has a current listener"),
                    &reload_status,
                )
                .await?;
                listener = Some(bound);
                applied
            }
        };
        if applied {
            config = candidate;
            reload_status.record_success();
        }
    }
}

/// Release the old listener and bind the new bind address. If the new address
/// cannot be bound, restore the previous listener so the process keeps serving.
/// Returns the listener to use and whether the candidate configuration is now
/// active.
async fn rebind_listener(
    candidate: &Config,
    previous: &Config,
    listener: Arc<TcpListener>,
    status: &ReloadStatus,
) -> Result<(Arc<TcpListener>, bool)> {
    // The listener clone held by the stopped generation is gone; dropping the
    // supervisor's clone releases the port.
    drop(listener);

    let attempt: Result<Arc<TcpListener>> = async {
        let bound = Arc::new(bind_listener(candidate, true).await?);
        ensure_upstream_is_not_self(candidate, listener_port(&bound)?).await?;
        Ok(bound)
    }
    .await;

    match attempt {
        Ok(bound) => Ok((bound, true)),
        Err(error) => {
            let message = format!(
                "Failed to apply the changed bind address `{}:{}`: {:#}",
                candidate.bind_host(),
                candidate.listen_port().unwrap_or_default(),
                error
            );
            warn!("{}. Restoring the previous listener", message);
            status.record_error(message);
            match bind_listener(previous, true).await {
                Ok(bound) => Ok((Arc::new(bound), false)),
                Err(restore) => Err(restore).with_context(|| {
                    format!(
                        "Failed to bind the new address and to restore the previous listener: \
                         {:#}",
                        error
                    )
                }),
            }
        }
    }
}

/// Wait for the next changed configuration, forever when the watcher is gone.
async fn next_message<T>(rx: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// How the next generation gets its listener.
enum ListenerPlan {
    /// The same address and port are already bound: keep the socket.
    Reuse,
    /// A new port (or a new address on a new port) was bound before the old
    /// generation stopped.
    Prebound(Arc<TcpListener>),
    /// The port stays the same but the bind address changes. The old socket
    /// overlaps the new address on most platforms, so it must be released
    /// before the new one is bound. The supervisor performs that dance with a
    /// rollback to the previous configuration if the new address is unusable.
    Rebind,
    /// The candidate removes `[server]`: stop serving and release the socket.
    Disable,
}

/// Plan the listener of the next generation while the current one still serves.
///
/// A same-port reload reuses the bound socket, so new visitors queue instead of
/// being refused. A port change binds the new listener before anything is
/// stopped, and a failure leaves the running configuration untouched. A same-
/// port bind-address change is the exception: it is reported as `Rebind` and
/// handled after the old generation stops.
async fn plan_next_generation(
    current: &Config,
    candidate: &Config,
    listener: Option<&TcpListener>,
) -> Result<ListenerPlan> {
    let Some(candidate_port) = candidate.listen_port() else {
        return Ok(if listener.is_some() {
            ListenerPlan::Disable
        } else {
            ListenerPlan::Reuse
        });
    };

    let Some(listener) = listener else {
        let bound = Arc::new(bind_listener(candidate, true).await?);
        let next_port = listener_port(&bound)?;
        ensure_upstream_is_not_self(candidate, next_port).await?;
        return Ok(ListenerPlan::Prebound(bound));
    };

    let current_addr = listener.local_addr()?;
    let candidate_host: IpAddr = candidate
        .bind_host()
        .parse()
        .with_context(|| format!("Invalid [server].bind `{}`", candidate.bind_host()))?;
    let same_port = candidate_port == current.listen_port().unwrap_or(0)
        || (current.listen_port() == Some(0) && candidate_port == current_addr.port());
    let same_host = current_addr.ip() == candidate_host;

    if same_port && same_host {
        ensure_upstream_is_not_self(candidate, current_addr.port()).await?;
        return Ok(ListenerPlan::Reuse);
    }
    if !same_port {
        let bound = Arc::new(bind_listener(candidate, true).await?);
        let next_port = listener_port(&bound)?;
        ensure_upstream_is_not_self(candidate, next_port).await?;
        return Ok(ListenerPlan::Prebound(bound));
    }
    Ok(ListenerPlan::Rebind)
}

/// The address a relay uses to reach its own embedded listener.
///
/// A wildcard bind maps to the matching loopback address; a specific bind
/// address is used as-is. This also keeps IPv6-only listeners working, where
/// `127.0.0.1` would be unreachable.
fn local_target_addr(bound: SocketAddr) -> String {
    let ip = match bound.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, bound.port()).to_string()
}

/// Bind the HTTP listener of `config`.
///
/// A reload retries a new port because that port may still have connections in
/// `TIME_WAIT`; the initial start fails fast so a port conflict is reported
/// immediately. A same-port reload reuses the bound socket instead.
async fn bind_listener(config: &Config, retry: bool) -> Result<TcpListener> {
    let port = config
        .listen_port()
        .ok_or_else(|| anyhow!("Cannot bind a listener without `[server]`"))?;
    let bind_addr = to_bind_addr(config.bind_host(), port);
    let attempts = if retry { REBIND_ATTEMPTS } else { 1 };
    let mut last_err = None;
    for attempt in 0..attempts {
        match TcpListener::bind(&bind_addr).await {
            Ok(listener) => return Ok(listener),
            Err(err) => {
                last_err = Some(err);
                if attempt + 1 < attempts {
                    time::sleep(REBIND_DELAY).await;
                }
            }
        }
    }
    let err = last_err.expect("the bind loop always attempts once");
    Err(anyhow!(err))
        .with_context(|| format!("Failed to listen at `[server].port` ({})", bind_addr))
}

fn listener_port(listener: &TcpListener) -> Result<u16> {
    listener
        .local_addr()
        .context("Failed to read the bound listener address")
        .map(|addr| addr.port())
}

/// Run one generation of the universal process: an embedded server plus one
/// optional upstream v4 session.
///
/// With `[server]`, the data plane is recursive: an upstream session forwards
/// the visitors it receives to `127.0.0.1:<actual port>`, where the embedded
/// server routes them by `Host` again. Without `[server]`, each local proxy is
/// the stream target directly. Without `[upstream]` the process is a standalone
/// ingress. With `[upstream]` it advertises every route of its downstream peers
/// and its local `[[proxies]]`.
pub(crate) async fn run_router(
    config: Config,
    http_listener: Option<Arc<TcpListener>>,
    shutdown_rx: broadcast::Receiver<bool>,
    reload: Arc<ReloadMarker>,
    reload_status: Arc<ReloadStatus>,
    apply_rx: LiveApplyReceiver,
    generation_id: u64,
    live: Option<mpsc::Sender<(u64, Config)>>,
    restart: mpsc::Sender<(u64, Config)>,
) -> Result<()> {
    #[cfg(not(feature = "management"))]
    let _ = (&reload, &reload_status, generation_id, restart);
    let bound_addr = match http_listener.as_ref() {
        Some(listener) => Some(
            listener
                .local_addr()
                .context("Failed to read the bound listener address")?,
        ),
        None => None,
    };
    let actual_port = bound_addr.map(|addr| addr.port());
    let local_target = bound_addr.map(local_target_addr);

    if let (Some(token), Some(upstream)) = (
        config
            .server
            .as_ref()
            .and_then(|server| server.token.as_ref()),
        &config.upstream,
    ) {
        if token == &upstream.token {
            warn!(
                "The downstream `[server].token` and `[upstream].token` are the same; use a different \
                 token per hop so that each hop has its own trust boundary"
            );
        }
    }

    let role = match (config.upstream.is_some(), http_listener.is_some()) {
        (true, true) => "relay",
        (true, false) => "leaf",
        (false, true) => "ingress",
        (false, false) => "idle",
    };
    info!(
        name = %config.name,
        role = %role,
        entry_domain = ?config.entry_domain(),
        listen_port = ?actual_port,
        "ht starting"
    );

    // The listener token is only consulted if a control plane is configured;
    // a client without `[server]` still has the upstream token, which keeps the
    // state constructible even though it never authenticates a local peer.
    let token = config.listener_token().cloned().unwrap_or_default();
    let (routes, route_rx) = watch::channel(BTreeSet::new());
    let state = Arc::new(ServerState::new(
        token,
        config.name.clone(),
        bound_addr.map(|addr| addr.ip()),
        config.entry_domain().map(str::to_string),
        config.server_limits(),
        routes,
        config.proxies.clone(),
    ));
    state.refresh_routes().await;

    // A standalone ingress has no upstream to wait for and is always ready.
    let connected = Arc::new(AtomicBool::new(config.upstream.is_none()));

    // The management page and the API share the reserved `[server].domain` of
    // the listener, so they need no port of their own.
    #[cfg(feature = "management")]
    let management = Some(Arc::new(crate::management::ManagementState::new(
        config.clone(),
        state.clone(),
        connected.clone(),
        reload.clone(),
        reload_status.clone(),
        generation_id,
        live.expect("the supervisor always provides the live channel"),
        restart,
    )));
    #[cfg(not(feature = "management"))]
    let management = {
        let _ = live;
        None
    };

    #[cfg(feature = "management")]
    if let Some(management) = management.clone() {
        let mut apply_rx = apply_rx;
        tokio::spawn(async move {
            while let Some(config) = apply_rx.recv().await {
                management.apply_reloaded_config(config).await;
            }
        });
    }
    #[cfg(not(feature = "management"))]
    let _ = apply_rx;

    // The router owns the shutdown of the embedded server, so that it can
    // close the upstream session before the server drains in-flight visitors.
    let (stop_tx, _) = broadcast::channel::<bool>(1);
    let server_task = http_listener.map(|listener| {
        tokio::spawn(run_server_with_listener_and_state(
            listener,
            state.clone(),
            management.clone(),
            stop_tx.subscribe(),
        ))
    });

    let runtime = RouterRuntime {
        server_task,
        stop_tx,
        shutdown_rx,
        connected,
        management,
    };

    match config.upstream.clone() {
        None => run_ingress(runtime).await,
        Some(upstream) => {
            let routes = match &local_target {
                Some(target) => mirrored_routes(
                    route_rx,
                    RouteTarget {
                        local_addr: target.clone(),
                        auth_header: None,
                        internal: true,
                    },
                ),
                None => direct_routes(state.clone(), route_rx).await,
            };
            run_relay(&config, upstream, routes, runtime).await
        }
    }
}

/// The tasks and channels the router has started and must shut down.
struct RouterRuntime {
    server_task: Option<JoinHandle<Result<()>>>,
    stop_tx: broadcast::Sender<bool>,
    shutdown_rx: broadcast::Receiver<bool>,
    connected: Arc<AtomicBool>,
    /// The management state of the embedded server, shared with the upstream
    /// session so it can serve proxy requests from the next hop.
    management: Option<Management>,
}

/// An ingress serves the visitors and downstream peers on its own.
async fn run_ingress(mut runtime: RouterRuntime) -> Result<()> {
    info!("No upstream is configured, so ht is a standalone ingress");
    let Some(mut server_task) = runtime.server_task.take() else {
        bail!("a standalone ingress needs a `[server]` listener");
    };
    let mut server_stopped = None;
    tokio::select! {
        res = &mut server_task => server_stopped = Some(res),
        _ = runtime.shutdown_rx.recv() => {}
    }

    let _ = runtime.stop_tx.send(true);
    match server_stopped {
        Some(res) => flatten_server(res),
        None => flatten_server(server_task.await),
    }
}

/// A relay advertises its route set on one v4 session to its next hop.
async fn run_relay(
    config: &Config,
    upstream: crate::config::UpstreamConfig,
    routes: watch::Receiver<RouteMap>,
    runtime: RouterRuntime,
) -> Result<()> {
    let RouterRuntime {
        mut server_task,
        stop_tx,
        mut shutdown_rx,
        connected,
        management: _management,
    } = runtime;

    info!(
        upstream = %upstream.url,
        "Advertising the mirrored routes"
    );

    #[cfg(feature = "management")]
    let (proxy_admin, allow_upstream_modify): (
        Option<mpsc::Sender<ProxyAdminRequest>>,
        Option<watch::Receiver<bool>>,
    ) = match _management.as_ref() {
        Some(management) => {
            let (proxy_tx, proxy_rx) = mpsc::channel::<ProxyAdminRequest>(8);
            tokio::spawn(run_proxy_admin(management.clone(), proxy_rx));
            (
                Some(proxy_tx),
                Some(management.subscribe_allow_upstream_modify()),
            )
        }
        None => (None, None),
    };
    #[cfg(not(feature = "management"))]
    let (proxy_admin, allow_upstream_modify) = (None, None);

    let (fatal_tx, mut fatal_rx) = mpsc::unbounded_channel::<anyhow::Error>();
    let (session_stop_tx, session_stop_rx) = broadcast::channel::<bool>(1);
    let session_args = SessionArgs {
        server_addr: upstream
            .addr()
            .expect("the configuration validates the upstream address"),
        tls: upstream.tls(),
        token: upstream.token.clone(),
        node_id: config.name.clone(),
        role: Role::Node,
        keepalive_interval: upstream.keepalive_interval,
        retry_interval: upstream.retry_interval,
        allow_upstream_modify,
        proxy_admin,
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
        res = async {
            match server_task.as_mut() {
                Some(task) => task.await,
                None => std::future::pending().await,
            }
        } => server_stopped = Some(res),
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
        None => match server_task {
            Some(task) => flatten_server(task.await),
            None => Ok(()),
        },
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
) -> watch::Receiver<RouteMap> {
    let (tx, rx) = watch::channel(RouteMap::new());
    tokio::spawn(async move {
        loop {
            let map: RouteMap = route_rx
                .borrow_and_update()
                .iter()
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

/// The route map of a listener-less leaf: every local proxy is its own target,
/// so visitors go straight from the v4 stream to `local_addr`.
async fn direct_routes(
    state: Arc<ServerState>,
    mut route_rx: watch::Receiver<BTreeSet<String>>,
) -> watch::Receiver<RouteMap> {
    let (tx, rx) = watch::channel(direct_route_map(&state).await);
    // Do not keep the state alive in the watcher: the route sender lives in it,
    // so a strong reference would make `changed()` wait forever after a reload.
    let weak = Arc::downgrade(&state);
    tokio::spawn(async move {
        loop {
            if route_rx.changed().await.is_err() {
                break;
            }
            let Some(state) = weak.upgrade() else {
                break;
            };
            tx.send_replace(direct_route_map(&state).await);
        }
    });
    rx
}

async fn direct_route_map(state: &ServerState) -> RouteMap {
    state
        .proxies
        .read()
        .await
        .iter()
        .map(|proxy| {
            (
                proxy.domain.clone(),
                RouteTarget {
                    local_addr: proxy.local_addr.clone(),
                    auth_header: proxy.auth_header.clone(),
                    internal: false,
                },
            )
        })
        .collect()
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

    let bind_ip = config.bind_host().parse::<IpAddr>().ok();
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
        if addr.ip().is_loopback()
            || addr.ip().is_unspecified()
            || bind_ip.is_some_and(|bind| bind == addr.ip())
        {
            anyhow::bail!(
                "The upstream `{}` resolves to {}, which is this process's own listener; \
                 configure the next hop instead of itself",
                upstream.url,
                addr
            );
        }
    }
    Ok(())
}

/// Reject a proxy write that the local node does not allow its upstream to make.
///
/// This is the authoritative check: the v4 capability bits only tell the next
/// hop what to display, and a next hop that sends a `Put`/`Delete` anyway must
/// still be refused.
#[cfg(feature = "management")]
fn ensure_upstream_may_modify(management: &crate::management::ManagementState) -> Result<()> {
    if !management.allow_upstream_modify() {
        bail!(
            "The upstream is not allowed to modify this node's local services; enable \
             `[upstream].allow_modify` on the node's own management page or in ht.toml"
        );
    }
    Ok(())
}

/// Serve the proxy operations an upstream sends over the v4 session with the
/// same management state as the local page.
#[cfg(feature = "management")]
async fn run_proxy_admin(
    management: Arc<crate::management::ManagementState>,
    mut requests: mpsc::Receiver<ProxyAdminRequest>,
) {
    while let Some(request) = requests.recv().await {
        match request {
            ProxyAdminRequest::List { reply } => {
                let _ = reply.send(Ok(management.proxies().await));
            }
            ProxyAdminRequest::Put {
                name,
                domain,
                local_addr,
                auth_header,
                reply,
            } => {
                let result: Result<Vec<crate::config::ServerProxyEntry>> = async {
                    ensure_upstream_may_modify(&management)?;
                    management
                        .put_proxy(name, domain, local_addr, auth_header)
                        .await?;
                    Ok(management.proxies().await)
                }
                .await;
                let _ = reply.send(result);
            }
            ProxyAdminRequest::Delete { name, reply } => {
                let result: Result<Vec<crate::config::ServerProxyEntry>> = async {
                    ensure_upstream_may_modify(&management)?;
                    management.delete_proxy(&name).await?;
                    Ok(management.proxies().await)
                }
                .await;
                let _ = reply.send(result);
            }
        }
    }
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
