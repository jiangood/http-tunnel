use crate::config::{domain_overlaps_server_host, MaskedString, ServerLimits, ServerProxyEntry};
use crate::session_server::V4SessionHandle;
use anyhow::{anyhow, Context, Result};
use std::collections::{BTreeSet, HashMap};
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
    pub(crate) server_host: String,
    // The configurable capacity and keepalive settings
    pub(crate) limits: ServerLimits,
    // WebSocket upgrades that are still waiting for their session handshake
    pub(crate) pending_connections: Arc<Semaphore>,
    // v4 sessions
    pub(crate) v4_routes: RwLock<HashMap<String, V4SessionHandle>>,
    pub(crate) v4_sessions: Arc<Semaphore>,
    pub(crate) v4_shutdown: broadcast::Sender<bool>,
    pub(crate) next_v4_session_id: AtomicU64,
    // The domains this server can advertise upstream: its downstream routes
    // plus its local proxies, excluding `server_host`.
    pub(crate) routes: watch::Sender<BTreeSet<String>>,
    // The local services served by this process. The administration API can
    // replace this list at runtime.
    pub(crate) proxies: RwLock<Vec<ServerProxyEntry>>,
    // The visitors that are currently being served
    pub(crate) activity: ActivityTracker,
}

/// Run a server on an already-bound listener, publishing its routes through
/// `routes`. A relay subscribes to that watch to advertise its downstream routes
/// and local proxies to its next hop.
pub(crate) async fn run_server_with_listener_and_state(
    http_listener: TcpListener,
    state: Arc<ServerState>,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    Server { state }.run(http_listener, shutdown_rx).await
}

struct Server {
    state: Arc<ServerState>,
}

impl ServerState {
    pub(crate) fn new(
        token: MaskedString,
        server_host: String,
        limits: ServerLimits,
        routes: watch::Sender<BTreeSet<String>>,
        proxies: Vec<ServerProxyEntry>,
    ) -> ServerState {
        let max_sessions = limits.max_control_connections;
        ServerState {
            token,
            server_host,
            pending_connections: Arc::new(Semaphore::new(limits.max_pending_handshakes)),
            limits,
            v4_routes: RwLock::new(HashMap::new()),
            v4_sessions: Arc::new(Semaphore::new(max_sessions)),
            v4_shutdown: broadcast::channel(1).0,
            next_v4_session_id: AtomicU64::new(0),
            routes,
            proxies: RwLock::new(proxies),
            activity: ActivityTracker::default(),
        }
    }

    /// Replace the local service list and publish the new route set.
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) async fn set_proxies(&self, proxies: Vec<ServerProxyEntry>) {
        *self.proxies.write().await = proxies;
        self.refresh_routes().await;
    }

    /// Recompute the advertised route set as the union of the local proxy
    /// domains and the domains claimed by downstream sessions.
    pub(crate) async fn refresh_routes(&self) {
        let proxies = self.proxies.read().await;
        let v4_routes = self.v4_routes.read().await;
        let mut routes: BTreeSet<String> = proxies
            .iter()
            .filter_map(|proxy| crate::helper::normalize_domain(&proxy.domain))
            .filter(|domain| !domain_overlaps_server_host(domain, &self.server_host))
            .collect();
        routes.extend(v4_routes.keys().cloned());
        self.routes.send_modify(|current| *current = routes);
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
pub(crate) async fn relay<T>(channel: T, stream: TcpStream)
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
                .write_all(&crate::http::response(
                    "504 Gateway Timeout",
                    "The tunnel closed without responding\n",
                ))
                .await
                .is_ok()
        {
            warn!(
                "The tunnel closed its stream without sending a response; \
                 answered the visitor with 504"
            );
        }
        let _ = v_wr.shutdown().await;
    };

    tokio::join!(upstream, downstream);
}
