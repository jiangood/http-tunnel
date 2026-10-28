use crate::cli::ServerArgs;
use crate::config::{domain_overlaps_server_host, MaskedString, ServerLimits};
use crate::helper::to_bind_addr;
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
    // The visitors that are currently being served
    pub(crate) activity: ActivityTracker,
}

/// The entrypoint of running a server.
pub async fn run_server(args: ServerArgs, shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
    // Bind the HTTP entrypoint before serving, so that a port that cannot be
    // bound fails startup instead of being discovered on shutdown.
    let bind_addr = to_bind_addr(&args.bind_host, args.server_port);
    let http_listener = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("Failed to listen at `server_port` ({})", bind_addr))?;
    run_server_with_listener(http_listener, args, shutdown_rx).await
}

/// Run a server on an already-bound listener.
pub(crate) async fn run_server_with_listener(
    http_listener: TcpListener,
    args: ServerArgs,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let (routes, _) = watch::channel(BTreeSet::new());
    run_server_with_listener_and_routes(http_listener, args, shutdown_rx, routes).await
}

/// Run a server on an already-bound listener, publishing its routes through
/// `routes`. A node subscribes to that watch to advertise the domains of its
/// downstream peers and local proxies to its next hop.
pub(crate) async fn run_server_with_listener_and_routes(
    http_listener: TcpListener,
    args: ServerArgs,
    shutdown_rx: broadcast::Receiver<bool>,
    routes: watch::Sender<BTreeSet<String>>,
) -> Result<()> {
    let mut server = Server::new(args, routes);
    server.run(http_listener, shutdown_rx).await
}

struct Server {
    state: Arc<ServerState>,
    proxies: Vec<crate::config::ServerProxyEntry>,
}

impl ServerState {
    fn new(
        token: MaskedString,
        server_host: String,
        limits: ServerLimits,
        routes: watch::Sender<BTreeSet<String>>,
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
            activity: ActivityTracker::default(),
        }
    }
}

impl Server {
    fn new(args: ServerArgs, routes: watch::Sender<BTreeSet<String>>) -> Server {
        // Local proxies are configured for the whole lifetime of the server, so
        // they are part of the initial route set.
        let initial: BTreeSet<String> = args
            .proxies
            .iter()
            .filter_map(|proxy| crate::helper::normalize_domain(&proxy.domain))
            .filter(|domain| !domain_overlaps_server_host(domain, &args.server_host))
            .collect();
        routes.send_modify(|routes| *routes = initial);

        let state = Arc::new(ServerState::new(
            MaskedString::from(args.token),
            args.server_host,
            args.limits,
            routes,
        ));

        Server {
            state,
            proxies: args.proxies,
        }
    }

    async fn run(
        &mut self,
        http_listener: TcpListener,
        mut shutdown_rx: broadcast::Receiver<bool>,
    ) -> Result<()> {
        let mut http_task = tokio::spawn(crate::http::serve(
            http_listener,
            self.state.clone(),
            shutdown_rx.resubscribe(),
            self.proxies.clone(),
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
