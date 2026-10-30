use crate::config::{domain_overlaps_control_host, MaskedString, ServerLimits, ServerProxyEntry};
use crate::session_server::V4SessionHandle;
use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{split, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, watch, RwLock, Semaphore};
use tokio::time;
use tracing::{info, warn};

const DRAIN_TIMEOUT: u64 = 30;
const COPY_BUF_SIZE: usize = 64 * 1024;
/// How long a withdrawn v4 route is remembered, so that a visitor that arrives
/// just after the withdrawal gets an explanation instead of a bare `404`.
const RECENT_WITHDRAWAL_TTL: Duration = Duration::from_secs(300);
/// The maximum number of withdrawn routes kept for that explanation.
const MAX_RECENT_WITHDRAWALS: usize = 1024;

/// A route that a v4 session stopped owning recently. It is only diagnostic:
/// it never changes routing, it lets the `404` say that the host used to exist
/// and which session owned it.
pub(crate) struct RecentWithdrawal {
    pub(crate) at: Instant,
    pub(crate) session_id: u64,
}

/// A downstream v4 session that is connected right now. It is used by
/// `GET /api/sessions` and the management page, so an operator can see how many
/// nodes are online and which domains each one advertises.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SessionSnapshot {
    pub(crate) session_id: u64,
    pub(crate) node: String,
    /// The TCP peer that opened the session. Several nodes may announce the
    /// same or a wrong node id, so the address is what tells them apart.
    pub(crate) peer: SocketAddr,
    /// The advisory role announced in the peer's `Hello`: `client` or `node`
    pub(crate) role: &'static str,
    /// Seconds since the session was established
    pub(crate) connected_seconds: u64,
    /// The routes the session owns right now
    pub(crate) domains: Vec<String>,
}

/// The registry entry kept for a connected v4 session until it closes.
pub(crate) struct V4SessionInfo {
    pub(crate) node_id: String,
    pub(crate) peer: SocketAddr,
    pub(crate) role: &'static str,
    pub(crate) since: Instant,
}

/// One entry of the effective routing table: a local proxy or a route claimed
/// by a v4 session. It is used by the route-table log and `GET /api/routes`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RouteSnapshot {
    pub(crate) domain: String,
    /// `local` or `v4`
    pub(crate) owner: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) node: Option<String>,
}

impl RouteSnapshot {
    /// A one-line rendering for the route-table log.
    fn describe(&self) -> String {
        match self.owner {
            "local" => format!(
                "{} -> local:{} ({})",
                self.domain,
                self.name.as_deref().unwrap_or("?"),
                self.target.as_deref().unwrap_or("?")
            ),
            _ => format!(
                "{} -> v4:{} ({})",
                self.domain,
                self.session_id.unwrap_or(0),
                self.node.as_deref().unwrap_or("?")
            ),
        }
    }

    /// A compact rendering for the `404` log, where the whole table is printed
    /// next to the host that could not be routed.
    fn describe_missing_route(&self) -> String {
        match self.owner {
            "local" => format!(
                "{} (local:{})",
                self.domain,
                self.name.as_deref().unwrap_or("?")
            ),
            _ => format!("{} (v4:{})", self.domain, self.session_id.unwrap_or(0)),
        }
    }
}

/// Tracks the visitor connections that are still being served, so that a
/// shutdown can drain them instead of cutting them off.
#[derive(Clone, Default)]
pub(crate) struct ActivityTracker {
    active: Arc<AtomicUsize>,
}

impl ActivityTracker {
    /// Count one more visitor as active until the returned guard is dropped.
    pub(crate) fn guard(&self) -> ActivityGuard {
        self.active.fetch_add(1, Ordering::Relaxed);
        ActivityGuard {
            active: self.active.clone(),
        }
    }

    pub(crate) fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// Wait until no visitor is being served, up to `timeout`.
    pub(crate) async fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.active() > 0 {
            if Instant::now() >= deadline {
                return false;
            }
            time::sleep(Duration::from_millis(20)).await;
        }
        true
    }
}

pub(crate) struct ActivityGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// All mutable state of a running server, shared by the entrypoint and the v4
/// sessions.
pub(crate) struct ServerState {
    // The shared token every peers authenticates with
    pub(crate) token: MaskedString,
    // Reserved HTTP Host for the WebSocket transport; never a business tunnel
    pub(crate) control_host: String,
    // The configurable capacity and keepalive settings
    pub(crate) limits: ServerLimits,
    // WebSocket upgrades that are still waiting for their session handshake
    pub(crate) pending_connections: Arc<Semaphore>,
    // v4 sessions
    pub(crate) v4_routes: RwLock<HashMap<String, V4SessionHandle>>,
    // The connected v4 sessions by session id, for the management page
    pub(crate) v4_session_info: RwLock<HashMap<u64, V4SessionInfo>>,
    pub(crate) v4_sessions: Arc<Semaphore>,
    // The total number of v4 sessions the server admits, so that a `404` can
    // say whether any leaf is connected at all.
    pub(crate) max_v4_sessions: usize,
    pub(crate) v4_shutdown: broadcast::Sender<bool>,
    pub(crate) next_v4_session_id: AtomicU64,
    // The domains this server can advertise upstream: its downstream routes
    // plus its local proxies, excluding `control_host`.
    pub(crate) routes: watch::Sender<BTreeSet<String>>,
    // The local services served by this process. The administration API can
    // replace this list at runtime.
    pub(crate) proxies: RwLock<Vec<ServerProxyEntry>>,
    // The visitors that are currently being served
    pub(crate) activity: ActivityTracker,
    // Routes that a v4 session stopped owning recently, for `404` diagnostics
    pub(crate) recent_withdrawals: RwLock<HashMap<String, RecentWithdrawal>>,
}

/// The management state of a build with the administration API. Builds without
/// the feature still pass `None` through the same signature.
#[cfg(feature = "management")]
pub(crate) type Management = Arc<crate::management::ManagementState>;
#[cfg(not(feature = "management"))]
pub(crate) type Management = ();

/// Run a server on an already-bound listener, publishing its routes through
/// `routes`. A relay subscribes to that watch to advertise its downstream routes
/// and local proxies to its next hop.
pub(crate) async fn run_server_with_listener_and_state(
    http_listener: TcpListener,
    state: Arc<ServerState>,
    management: Option<Management>,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    Server { state, management }
        .run(http_listener, shutdown_rx)
        .await
}

struct Server {
    state: Arc<ServerState>,
    management: Option<Management>,
}

impl ServerState {
    pub(crate) fn new(
        token: MaskedString,
        control_host: String,
        limits: ServerLimits,
        routes: watch::Sender<BTreeSet<String>>,
        proxies: Vec<ServerProxyEntry>,
    ) -> ServerState {
        let max_sessions = limits.max_sessions;
        ServerState {
            token,
            control_host,
            pending_connections: Arc::new(Semaphore::new(limits.max_pending_handshakes)),
            limits,
            v4_routes: RwLock::new(HashMap::new()),
            v4_session_info: RwLock::new(HashMap::new()),
            v4_sessions: Arc::new(Semaphore::new(max_sessions)),
            max_v4_sessions: max_sessions,
            v4_shutdown: broadcast::channel(1).0,
            next_v4_session_id: AtomicU64::new(0),
            routes,
            proxies: RwLock::new(proxies),
            activity: ActivityTracker::default(),
            recent_withdrawals: RwLock::new(HashMap::new()),
        }
    }

    /// Replace the local service list and publish the new route set.
    #[cfg_attr(not(feature = "management"), allow(dead_code))]
    pub(crate) async fn set_proxies(&self, proxies: Vec<ServerProxyEntry>) {
        *self.proxies.write().await = proxies;
        self.refresh_routes().await;
    }

    /// Recompute the advertised route set as the union of the local proxy
    /// domains and the domains claimed by downstream sessions.
    ///
    /// The effective table is logged at `info` on every change, so the log
    /// shows exactly which service a domain resolves to at that moment.
    pub(crate) async fn refresh_routes(&self) {
        let snapshot = self.routes_snapshot().await;
        let routes: BTreeSet<String> = snapshot.iter().map(|route| route.domain.clone()).collect();
        self.routes.send_modify(|current| *current = routes);
        let table: Vec<String> = snapshot.iter().map(RouteSnapshot::describe).collect();
        info!(
            routes = table.len(),
            table = ?table,
            "The route table changed"
        );
    }

    /// A snapshot of the effective routing table, sorted by domain. Local
    /// proxies and v4 routes can carry the same domain; the local proxy wins,
    /// but both entries stay visible for diagnosis.
    pub(crate) async fn routes_snapshot(&self) -> Vec<RouteSnapshot> {
        let proxies = self.proxies.read().await;
        let v4_routes = self.v4_routes.read().await;
        let mut table = Vec::with_capacity(proxies.len() + v4_routes.len());
        for proxy in proxies.iter() {
            let Some(domain) = crate::helper::normalize_domain(&proxy.domain) else {
                continue;
            };
            if domain_overlaps_control_host(&domain, &self.control_host) {
                continue;
            }
            table.push(RouteSnapshot {
                domain,
                owner: "local",
                name: Some(proxy.name.clone()),
                target: Some(proxy.local_addr.clone()),
                session_id: None,
                node: None,
            });
        }
        for (domain, handle) in v4_routes.iter() {
            if domain_overlaps_control_host(domain, &self.control_host) {
                continue;
            }
            table.push(RouteSnapshot {
                domain: domain.clone(),
                owner: "v4",
                name: None,
                target: None,
                session_id: Some(handle.session_id),
                node: Some(handle.node_id.clone()),
            });
        }
        table.sort_by(|a, b| a.domain.cmp(&b.domain).then(a.owner.cmp(b.owner)));
        table
    }

    /// Up to `limit` entries of the effective route table, rendered compactly,
    /// together with the number of entries. A `404` uses it to show which hosts
    /// the server *does* serve at that moment, instead of only a count.
    pub(crate) async fn known_hosts(&self, limit: usize) -> (usize, Vec<String>) {
        let table = self.routes_snapshot().await;
        let total = table.len();
        let hosts = table
            .iter()
            .take(limit)
            .map(RouteSnapshot::describe_missing_route)
            .collect();
        (total, hosts)
    }

    /// The downstream v4 sessions connected right now, sorted by session id,
    /// with the routes each one owns. The management page lists them so an
    /// operator can see the connected nodes, not only the route table.
    pub(crate) async fn sessions_snapshot(&self) -> Vec<SessionSnapshot> {
        let infos = self.v4_session_info.read().await;
        let routes = self.v4_routes.read().await;
        let now = Instant::now();
        let mut sessions: Vec<SessionSnapshot> = infos
            .iter()
            .map(|(session_id, info)| {
                let mut domains: Vec<String> = routes
                    .iter()
                    .filter(|(_, handle)| handle.session_id == *session_id)
                    .map(|(domain, _)| domain.clone())
                    .collect();
                domains.sort();
                SessionSnapshot {
                    session_id: *session_id,
                    node: info.node_id.clone(),
                    peer: info.peer,
                    role: info.role,
                    connected_seconds: now.duration_since(info.since).as_secs(),
                    domains,
                }
            })
            .collect();
        sessions.sort_by_key(|session| session.session_id);
        sessions
    }

    /// How many v4 sessions are connected right now, and the configured limit.
    /// A `404` with no session at all means the leaf is down; a `404` with a
    /// session but no matching route means the leaf advertises something else.
    pub(crate) fn session_count(&self) -> (usize, usize) {
        let connected = self
            .max_v4_sessions
            .saturating_sub(self.v4_sessions.available_permits());
        (connected, self.max_v4_sessions)
    }

    /// Remember that a v4 session stopped owning `prefix`, so a visitor that
    /// lands in the gap can be told that its route was withdrawn instead of
    /// seeing a bare `404`.
    pub(crate) async fn remember_withdrawal(&self, prefix: &str, session_id: u64) {
        let now = Instant::now();
        let mut recent = self.recent_withdrawals.write().await;
        recent.retain(|_, withdrawal| now.duration_since(withdrawal.at) < RECENT_WITHDRAWAL_TTL);
        if recent.len() >= MAX_RECENT_WITHDRAWALS {
            if let Some(oldest) = recent
                .iter()
                .min_by_key(|(_, withdrawal)| withdrawal.at)
                .map(|(prefix, _)| prefix.clone())
            {
                recent.remove(&oldest);
            }
        }
        recent.insert(
            prefix.to_string(),
            RecentWithdrawal {
                at: now,
                session_id,
            },
        );
    }

    /// Forget a withdrawal once a route for the same prefix is advertised again.
    pub(crate) async fn forget_withdrawal(&self, prefix: &str) {
        self.recent_withdrawals.write().await.remove(prefix);
    }

    /// The most recent withdrawal that would have matched `host`, with its
    /// owning session and age, if it is still inside the diagnostic window.
    pub(crate) async fn recent_withdrawal(&self, host: &str) -> Option<(String, u64, Duration)> {
        let now = Instant::now();
        let recent = self.recent_withdrawals.read().await;
        let mut best: Option<(&String, &RecentWithdrawal, usize)> = None;
        for (prefix, withdrawal) in recent.iter() {
            if now.duration_since(withdrawal.at) >= RECENT_WITHDRAWAL_TTL {
                continue;
            }
            let score = match prefix.strip_prefix("*.") {
                Some(suffix) => {
                    let dot = format!(".{}", suffix);
                    if host.len() > dot.len() && host.ends_with(&dot) {
                        dot.len()
                    } else {
                        continue;
                    }
                }
                None => {
                    if prefix == host {
                        usize::MAX
                    } else {
                        continue;
                    }
                }
            };
            if best
                .as_ref()
                .map(|(_, _, current)| score > *current)
                .unwrap_or(true)
            {
                best = Some((prefix, withdrawal, score));
            }
        }
        best.map(|(prefix, withdrawal, _)| {
            (
                prefix.clone(),
                withdrawal.session_id,
                now.duration_since(withdrawal.at),
            )
        })
    }
}

impl Server {
    async fn run(
        &mut self,
        http_listener: TcpListener,
        mut shutdown_rx: broadcast::Receiver<bool>,
    ) -> Result<()> {
        let mut http_task = tokio::spawn(crate::http::serve(
            http_listener,
            self.state.clone(),
            self.management.clone(),
            shutdown_rx.resubscribe(),
        ));
        let mut http_task_done = false;

        tokio::select! {
            res = &mut http_task => {
                http_task_done = true;
                match res {
                    Ok(Ok(())) => info!("HTTP listener stopped"),
                    Ok(Err(err)) => {
                        return Err(err).context("The HTTP listener stopped unexpectedly")
                    }
                    Err(err) => return Err(anyhow!("The HTTP listener task failed: {}", err)),
                }
            }
            _ = shutdown_rx.recv() => {
                info!("Shuting down gracefully...");
            }
        }

        if !http_task_done {
            let _ = http_task.await;
        }

        // Let the in-flight visitors finish before the sessions are torn down.
        let active = self.state.activity.active();
        if active > 0 {
            info!("Waiting for {} in-flight visitor(s) to finish...", active);
            if !self
                .state
                .activity
                .wait_idle(Duration::from_secs(DRAIN_TIMEOUT))
                .await
            {
                warn!(
                    "{} visitor(s) still in flight after the drain timeout, shutting down anyway",
                    self.state.activity.active()
                );
            }
        }

        // Tell the v4 sessions to close and forget their routes, so that peers
        // reconnect and advertise them again when the server restarts.
        let _ = self.state.v4_shutdown.send(true);
        self.state.v4_routes.write().await.clear();

        info!("Shutdown");
        Ok(())
    }
}

/// Relay a visitor and a tunnel stream until both directions are done.
///
/// If the stream closes without sending a single byte back, the visitor is
/// answered with a `504` instead of being left with an empty reply.
pub(crate) async fn relay<T>(channel: T, stream: TcpStream, request_id: String)
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let (mut ch_rd, mut ch_wr) = split(channel);
    let (mut v_rd, mut v_wr) = stream.into_split();

    let upstream = async {
        let mut buf = vec![0u8; COPY_BUF_SIZE];
        loop {
            match v_rd.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if ch_wr.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = ch_wr.shutdown().await;
    };

    let downstream = async {
        let mut buf = vec![0u8; COPY_BUF_SIZE];
        let mut sent = 0u64;
        loop {
            match ch_rd.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if v_wr.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                    sent += n as u64;
                }
                // A reset means the tunnel is gone, same as an end of stream
                Err(_) => break,
            }
        }
        if sent == 0
            && v_wr
                .write_all(&crate::http::error_response(
                    &request_id,
                    "504 Gateway Timeout",
                    "The tunnel closed without responding",
                ))
                .await
                .is_ok()
        {
            warn!(
                request_id = %request_id,
                "The tunnel closed its stream without sending a response; \
                 answered the visitor with 504"
            );
        }
        let _ = v_wr.shutdown().await;
    };

    tokio::join!(upstream, downstream);
}
