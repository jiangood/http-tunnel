use crate::cli::ServerArgs;
use crate::config::MaskedString;
use crate::constants::listen_backoff;
use crate::helper::{retry_notify_with_deadline, to_bind_addr, write_and_flush};
use crate::http::RoutingTable;
use crate::multi_map::MultiMap;
use crate::protocol::{
    self, Ack, ClientHello, ControlChannelCmd, DataChannelCmd, ServerHello, HASH_WIDTH_IN_BYTES,
};
use crate::transport::SocketOpts;
use anyhow::{anyhow, bail, Context, Result};
use backoff::backoff::Backoff;
use backoff::ExponentialBackoff;

use rand::RngCore;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{split, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};
use tokio::task::JoinHandle;
use tokio::time;
use tracing::{debug, error, info, info_span, instrument, warn, Instrument, Span};

pub(crate) type TunnelDigest = protocol::Digest; // SHA256 of a tunnel name
type Nonce = protocol::Digest; // Also called `session_key`

const POOL_MIN: usize = 8; // Minimum number of warm data channels kept per tunnel
const POOL_DEMAND_DECAY: usize = 8; // The remembered demand shrinks by 1/8 per replenish tick
const POOL_REPLENISH_INTERVAL_MS: u64 = 100; // How often the data channel pool is topped up
const DATA_CHANNEL_WAIT_TIMEOUT: u64 = 10; // Seconds to wait for a data channel before answering a visitor with 504
const DRAIN_TIMEOUT: u64 = 30; // Seconds to wait for in-flight visitors on shutdown
// The size of the buffers that are piped between a visitor, the tunnel and the
// local service. Larger than the tokio default (8 KiB), which reduces the number
// of syscalls on large transfers while keeping the per-connection memory modest.
const COPY_BUF_SIZE: usize = 64 * 1024;
const CHAN_SIZE: usize = 2048; // The capacity of various chans
const HANDSHAKE_TIMEOUT: u64 = 5; // Timeout for transport handshake
// The interval between two application-layer heartbeats sent to a client. The
// server owns it (it is the one sending the heartbeats), so it is a constant now
// that the server has no configuration file.
const HEARTBEAT_INTERVAL_SECS: u64 = 30;

// A visitor accepted by the HTTP entrypoint. `prefetched` holds the bytes that were
// read while sniffing the `Host` header and must be replayed to the tunnel.
pub(crate) struct HttpVisitor {
    pub stream: TcpStream,
    pub prefetched: Vec<u8>,
    // Keeps the visitor counted as active until it has been fully served
    pub activity: ActivityGuard,
}

/// Tracks the visitor connections that are still being served, so that a
/// shutdown can drain them instead of cutting them off.
#[derive(Clone, Default)]
pub(crate) struct ActivityTracker {
    active: Arc<AtomicUsize>,
}

impl ActivityTracker {
    /// Count one more visitor as active until the returned guard is dropped
    pub(crate) fn guard(&self) -> ActivityGuard {
        self.active.fetch_add(1, Ordering::Relaxed);
        ActivityGuard {
            active: self.active.clone(),
        }
    }

    pub(crate) fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// Wait until no visitor is being served, up to `timeout`. Returns `false`
    /// when the timeout is reached with visitors still active.
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

// A hash map of ControlChannelHandles, indexed by TunnelDigest or Nonce
// See also MultiMap
pub(crate) type ControlChannelMap = MultiMap<TunnelDigest, Nonce, ControlChannelHandle>;

// ServerState holds all the mutable state of a running server. It's shared by the
// server tasks. The server takes no configuration file: it learns the tunnels of
// its clients from the control channels they open, so the routing table grows and
// shrinks with them.
pub(crate) struct ServerState {
    // The shared token every client authenticates with
    pub(crate) token: MaskedString,
    // Collection of control channels
    pub(crate) control_channels: RwLock<ControlChannelMap>,
    // `Host` -> TunnelDigest
    pub(crate) routing_table: RwLock<RoutingTable>,
    // A monotonically increasing id, identifying a control channel connection
    next_conn_id: AtomicUsize,
    // The visitors that are currently being served
    pub(crate) activity: ActivityTracker,
}

// The entrypoint of running a server
pub async fn run_server(args: ServerArgs, shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
    let mut server = Server::new(args.server_port, args.http_port, args.token);
    server.run(shutdown_rx).await?;

    Ok(())
}

// Server holds all states of running a server
struct Server {
    state: Arc<ServerState>,
    server_addr: String,
    http_port: u16,
}

impl ServerState {
    fn new(token: MaskedString) -> ServerState {
        ServerState {
            token,
            control_channels: RwLock::new(ControlChannelMap::new()),
            routing_table: RwLock::new(RoutingTable::default()),
            next_conn_id: AtomicUsize::new(0),
            activity: ActivityTracker::default(),
        }
    }
}

impl Server {
    // Create a server from the command line options
    pub fn new(server_port: u16, http_port: u16, token: String) -> Server {
        let server_addr = to_bind_addr(server_port);
        let state = Arc::new(ServerState::new(MaskedString::from(token)));

        Server {
            state,
            server_addr,
            http_port,
        }
    }

    // The entry point of Server
    pub async fn run(&mut self, mut shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
        // Listen at `server_port` for the control and data channels of clients
        let l: TcpListener = retry_notify_with_deadline(
            listen_backoff(),
            || async { Ok(TcpListener::bind(&self.server_addr).await?) },
            |e, duration| {
                error!("{:#}. Retry in {:?}", e, duration);
            },
            &mut shutdown_rx,
        )
        .await
        .with_context(|| "Failed to listen at `server_port`")?;

        info!("Listening at {}", self.server_addr);

        // Run the HTTP entrypoint which routes visitors by the `Host` header
        let http_task = tokio::spawn(crate::http::serve(
            self.http_port,
            self.state.clone(),
            shutdown_rx.resubscribe(),
        ));

        // Retry at least every 100ms
        let mut backoff = ExponentialBackoff {
            max_interval: Duration::from_millis(100),
            max_elapsed_time: None,
            ..Default::default()
        };

        // Wait for connections and shutdown signals
        loop {
            tokio::select! {
                // Wait for incoming control and data channels
                ret = l.accept() => {
                    match ret {
                        Err(err) => {
                            // It is an IO error, so it's possibly an EMFILE.
                            // Sleep for a while and retry
                            if let Some(d) = backoff.next_backoff() {
                                error!("Failed to accept: {:#}. Retry in {:?}...", err, d);
                                time::sleep(d).await;
                            } else {
                                error!("Too many retries. Aborting...");
                                break;
                            }
                        }
                        Ok((conn, addr)) => {
                            backoff.reset();

                            // Do transport handshake with a timeout
                            match time::timeout(Duration::from_secs(HANDSHAKE_TIMEOUT), handshake(conn)).await {
                                Ok(conn) => {
                                    match conn.with_context(|| "Failed to do transport handshake") {
                                        Ok(conn) => {
                                            let state = self.state.clone();
                                            tokio::spawn(async move {
                                                if let Err(err) = handle_connection(conn, state).await {
                                                    error!("{:#}", err);
                                                }
                                            }.instrument(info_span!("connection", %addr)));
                                        }, Err(e) => {
                                            error!("{:#}", e);
                                        }
                                    }
                                },
                                Err(e) => {
                                    error!("Transport handshake timeout: {}", e);
                                }
                            }
                        }
                    }
                },
                // Wait for the shutdown signal
                _ = shutdown_rx.recv() => {
                    info!("Shuting down gracefully...");
                    break;
                }
            }
        }

        let _ = http_task.await;

        // Let the in-flight visitors finish before the tunnel is torn down. The
        // control channels are still alive here, so the data keeps flowing.
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

        // Drop the control channels and the routing table, so that the clients
        // reconnect (and register their tunnels again) when the server is
        // restarted. Without this, the in-process tasks of this server would keep
        // the connections alive.
        *self.state.control_channels.write().await = ControlChannelMap::new();
        *self.state.routing_table.write().await = RoutingTable::default();

        info!("Shutdown");

        Ok(())
    }
}

// Plain TCP accepts the connection as is
async fn handshake(conn: TcpStream) -> Result<TcpStream> {
    Ok(conn)
}

// Handle connections to `server_port`
async fn handle_connection(mut conn: TcpStream, state: Arc<ServerState>) -> Result<()> {
    // Read hello
    let hello: ClientHello = protocol::read_payload(&mut conn).await?;
    protocol::check_version(hello.version())?;

    match hello {
        ClientHello::Control {
            domain,
            name,
            nodelay,
            ..
        } => {
            do_control_channel_handshake(conn, state, domain, name, nodelay).await?;
        }
        ClientHello::Data { nonce, .. } => {
            do_data_channel_handshake(conn, state, nonce).await?;
        }
    }
    Ok(())
}

// Establish a control channel for `domain`.
//
// The server takes no configuration file: this is where it learns that a client
// serves `domain`, and where the domain is added to the routing table. The
// control channel is torn down (and the domain removed) when the client goes
// away. A second control channel for a domain that is already served takes it
// over, which is how a reconnecting client is handled; taking a domain from a
// client of another name is logged as a warning.
async fn do_control_channel_handshake(
    mut conn: TcpStream,
    state: Arc<ServerState>,
    domain: String,
    name: String,
    nodelay: bool,
) -> Result<()> {
    info!("Try to handshake a control channel");

    SocketOpts::for_control_channel().apply(&conn);

    let domain = crate::helper::normalize_domain(&domain)
        .ok_or_else(|| anyhow!("The domain `{}` of the client `{}` is invalid", domain, name))?;

    // Generate a nonce
    let mut nonce = vec![0u8; HASH_WIDTH_IN_BYTES];
    rand::thread_rng().fill_bytes(&mut nonce);
    let nonce: protocol::Digest = nonce.try_into().unwrap();

    // Send hello
    let hello_send = ServerHello {
        version: protocol::CURRENT_PROTO_VERSION,
        nonce,
    };
    protocol::write_payload(&mut conn, &hello_send).await?;

    // Read auth
    let protocol::Auth(d) = protocol::read_payload(&mut conn).await?;

    // Validate against the shared token
    let mut concat = Vec::from(state.token.as_bytes());
    concat.extend_from_slice(&nonce);
    let session_key = protocol::digest(&concat);
    if session_key != d {
        protocol::write_payload(&mut conn, &Ack::AuthFailed).await?;
        debug!(
            "Expect {}, but got {}",
            hex::encode(session_key),
            hex::encode(d)
        );
        bail!("The client `{}` failed the authentication", name);
    }

    // Send ack
    protocol::write_payload(&mut conn, &Ack::Ok).await?;

    let digest = protocol::digest(domain.as_bytes());
    let conn_id = state.next_conn_id.fetch_add(1, Ordering::Relaxed);

    // If there's already a control channel for the domain, drop the old one. A
    // control channel doesn't report back when it's dead, so the handle in the map
    // could be stale; dropping it lets the client reconnect.
    {
        let mut ccs = state.control_channels.write().await;
        if let Some(prev) = ccs.remove1(&digest) {
            if prev.name != name {
                warn!(
                    "The domain `{}` was served by the client `{}`; taking it over for `{}`",
                    domain, prev.name, name
                );
            } else {
                debug!(
                    "Dropping the previous control channel for the domain `{}`",
                    domain
                );
            }
        }
    }

    info!(tunnel = %domain, client = %name, "Control channel established");
    let (handle, ch_task) =
        ControlChannelHandle::new(conn, domain.clone(), name, nodelay, HEARTBEAT_INTERVAL_SECS, conn_id);

    // Register the handle and the route
    let _ = state
        .control_channels
        .write()
        .await
        .insert(digest, session_key, handle);
    state.routing_table.write().await.insert(&domain, digest);

    // When the control channel is gone (the client crashed, was stopped, or lost its
    // connection), remove its handle and its route, so that the visitor of that host
    // is answered with a 404 instead of being handed over to a tunnel that will
    // never receive it. Only remove our own, so that a reconnected client isn't
    // dropped.
    tokio::spawn(async move {
        let _ = ch_task.await;
        let mut ccs = state.control_channels.write().await;
        if ccs.get1(&digest).map(|h| h.id) == Some(conn_id) {
            ccs.remove1(&digest);
            drop(ccs);
            state.routing_table.write().await.remove(&domain);
        }
    });

    Ok(())
}

async fn do_data_channel_handshake(
    conn: TcpStream,
    state: Arc<ServerState>,
    nonce: Nonce,
) -> Result<()> {
    debug!("Try to handshake a data channel");

    // Validate
    let control_channels_guard = state.control_channels.read().await;
    match control_channels_guard.get2(&nonce) {
        Some(handle) => {
            SocketOpts::data_channel(handle.nodelay).apply(&conn);

            // Send the data channel to the corresponding control channel. The
            // pool is told whether it made it, so that a channel that never
            // arrives doesn't count as outstanding forever.
            match handle.data_ch_tx.send(conn).await {
                Ok(()) => handle.data_pool.arrived(),
                Err(e) => {
                    handle.data_pool.failed();
                    return Err(e).with_context(|| "Data channel for a stale control channel");
                }
            }
        }
        None => {
            warn!("Data channel has incorrect nonce");
        }
    }
    Ok(())
}

/// The data channel pool of one tunnel. It keeps a number of warm data channels
/// proportional to the number of visitors that are currently being forwarded, so
/// that a visitor usually gets one without waiting.
#[derive(Clone)]
pub(crate) struct DataChannelPool {
    // Asks the client to create a data channel
    req_tx: mpsc::UnboundedSender<bool>,
    // Channels that were requested but did not arrive yet
    inflight: Arc<AtomicUsize>,
    // Channels that arrived and are waiting to be consumed
    warm: Arc<AtomicUsize>,
    // A decaying high-water mark of the requests that were in flight, used to
    // keep the pool warm for the next burst instead of only for the visitors
    // that are being forwarded right now
    demand: Arc<AtomicUsize>,
}

impl DataChannelPool {
    fn new(req_tx: mpsc::UnboundedSender<bool>) -> DataChannelPool {
        DataChannelPool {
            req_tx,
            inflight: Arc::new(AtomicUsize::new(0)),
            warm: Arc::new(AtomicUsize::new(0)),
            demand: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Ask the client for a data channel. Returns `false` when the control
    /// channel is gone, so no data channel will ever come.
    fn request(&self) -> bool {
        self.inflight.fetch_add(1, Ordering::Relaxed);
        if self.req_tx.send(true).is_err() {
            self.inflight.fetch_sub(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Ask the client for a data channel on behalf of a visitor. The HTTP
    /// entrypoint calls this once per visitor, so the number of channels on
    /// their way follows the number of visitors and short-lived connections are
    /// not throttled by the (transient) number of active forwardings. The
    /// backlog is remembered so that the pool stays warm for the next burst.
    pub(crate) fn request_for_visitor(&self) -> bool {
        self.note_demand();
        self.request()
    }

    /// Remember how many requests were waiting, as a decaying high-water mark.
    fn note_demand(&self) {
        let sample = self.inflight.load(Ordering::Relaxed);
        self.demand.fetch_max(sample, Ordering::Relaxed);
    }

    /// A requested data channel arrived and was handed to the pool receiver. It
    /// stops counting as in flight, but is still outstanding until consumed.
    fn arrived(&self) {
        let _ = self.inflight.try_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |v| Some(v.saturating_sub(1)),
        );
        self.warm.fetch_add(1, Ordering::Relaxed);
    }

    /// A requested data channel could not be handed over. Nothing will arrive
    /// for it, so it stops counting as outstanding.
    fn failed(&self) {
        let _ = self.inflight.try_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |v| Some(v.saturating_sub(1)),
        );
    }

    /// A data channel was taken out of the pool. The visitor already asked for
    /// its own channel when it was routed, so this only updates the counters
    /// instead of asking for another one (which would double the requests).
    fn consume(&self) {
        let _ = self.warm.try_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |v| Some(v.saturating_sub(1)),
        );
        self.note_demand();
    }

    /// Forget the requests that never arrived. A visitor that waited for a data
    /// channel up to its timeout proves that they are lost, so the counter is
    /// reset and the pool can be replenished again instead of staying stuck.
    fn forget_inflight(&self) {
        self.inflight.store(0, Ordering::Relaxed);
    }

    /// The number of channels to keep warm and on their way: the base buffer
    /// plus the recent demand, so that a burst is served without waiting for a
    /// fresh round trip. The demand decays, so an idle tunnel shrinks back to
    /// the base buffer.
    fn target(&self) -> usize {
        POOL_MIN + self.demand.load(Ordering::Relaxed)
    }

    /// How many warm channels are wanted once the ones that are still on their
    /// way are accounted for.
    fn warm_target(&self) -> usize {
        self.target()
            .saturating_sub(self.inflight.load(Ordering::Relaxed))
    }

    /// Top the pool up to the target, counting both the channels that are warm
    /// and the ones that are already on their way.
    pub(crate) fn replenish(&self) {
        // Age the remembered demand, so it follows a burst down again. At
        // least one is subtracted, otherwise the integer division would floor
        // to zero and the demand would never reach zero.
        let demand = self.demand.load(Ordering::Relaxed);
        let decay = (demand / POOL_DEMAND_DECAY).max(1);
        self.demand
            .store(demand.saturating_sub(decay), Ordering::Relaxed);

        while self.warm.load(Ordering::Relaxed) + self.inflight.load(Ordering::Relaxed)
            < self.target()
        {
            if !self.request() {
                break;
            }
        }
    }

    /// Whether there are more warm channels than the target wants, so the
    /// caller should drain and drop one.
    fn warm_over_target(&self) -> bool {
        self.warm.load(Ordering::Relaxed) > self.warm_target()
    }

    /// A warm channel was dropped by the trimming. It was already counted as
    /// outstanding, so stop counting it.
    fn drop_warm(&self) {
        let _ = self.warm.try_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |v| Some(v.saturating_sub(1)),
        );
    }
}

pub(crate) struct ControlChannelHandle {
    // Identifies the connection that owns the handle, so that it only removes its own
    // handle when it goes away
    id: usize,
    // The client that owns the channel, to warn when a domain changes hands
    name: String,
    // The client's TCP_NODELAY preference for the data channels of the tunnel
    nodelay: bool,
    // Shutdown the control channel by dropping it
    _shutdown_tx: broadcast::Sender<bool>,
    data_ch_tx: mpsc::Sender<TcpStream>,
    pub(crate) visitor_tx: mpsc::Sender<HttpVisitor>,
    // The data channel pool of the tunnel
    pub(crate) data_pool: DataChannelPool,
}

impl ControlChannelHandle {
    // Create a control channel handle, where the control channel handling task
    // and the connection pool task are created.
    #[instrument(name = "handle", skip_all, fields(tunnel = %domain, client = %name))]
    fn new(
        conn: TcpStream,
        domain: String,
        name: String,
        nodelay: bool,
        heartbeat_interval: u64,
        id: usize,
    ) -> (ControlChannelHandle, JoinHandle<()>) {
        // Create a shutdown channel
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);

        // Store data channels
        let (data_ch_tx, data_ch_rx) = mpsc::channel(CHAN_SIZE * 2);

        // Store data channel creation requests
        let (data_ch_req_tx, data_ch_req_rx) = mpsc::unbounded_channel();

        // Visitors handed over by the HTTP entrypoint
        let (visitor_tx, visitor_rx) = mpsc::channel(CHAN_SIZE);

        // The pool that keeps the data channels warm
        let data_pool = DataChannelPool::new(data_ch_req_tx);

        let pool_task = data_pool.clone();
        tokio::spawn(
            async move {
                if let Err(e) = run_tcp_connection_pool(data_ch_rx, visitor_rx, pool_task)
                    .await
                    .with_context(|| "Failed to run TCP connection pool")
                {
                    error!("{:#}", e);
                }
            }
            .instrument(Span::current()),
        );

        // Create the control channel
        let ch = ControlChannel {
            conn,
            shutdown_rx,
            data_ch_req_rx,
            heartbeat_interval,
        };

        // Run the control channel. Its `JoinHandle` is returned so that the caller can
        // remove the handle from the map once the channel is gone, which lets the
        // visitor of the host be answered with a 404 instead of being handed over to a
        // dead tunnel.
        let ch_task = tokio::spawn(
            async move {
                if let Err(err) = ch.run().await {
                    error!("{:#}", err);
                }
            }
            .instrument(Span::current()),
        );

        (
            ControlChannelHandle {
                id,
                name,
                nodelay,
                _shutdown_tx: shutdown_tx,
                data_ch_tx,
                visitor_tx,
                data_pool,
            },
            ch_task,
        )
    }
}

// Control channel
struct ControlChannel {
    conn: TcpStream,                               // The connection of control channel
    shutdown_rx: broadcast::Receiver<bool>,        // Receives the shutdown signal
    data_ch_req_rx: mpsc::UnboundedReceiver<bool>, // Receives data channel creation requests
    heartbeat_interval: u64,                       // Application-layer heartbeat interval in secs
}

impl ControlChannel {
    // Run a control channel
    #[instrument(skip_all)]
    async fn run(self) -> Result<()> {
        let ControlChannel {
            conn,
            mut shutdown_rx,
            mut data_ch_req_rx,
            heartbeat_interval,
        } = self;

        // Split the connection, so that the writes and the read that watches for the
        // client going away don't borrow the same half.
        let (mut rd, mut wr) = split(conn);
        let mut buf = [0u8; 1024];

        // Wait for data channel requests and the shutdown signal
        loop {
            tokio::select! {
                val = data_ch_req_rx.recv() => {
                    match val {
                        Some(_) => {
                            if let Err(e) = protocol::write_payload(&mut wr, &ControlChannelCmd::CreateDataChannel).await {
                                error!("{:#}", e);
                                break;
                            }
                        }
                        None => {
                            break;
                        }
                    }
                },
                _ = time::sleep(Duration::from_secs(heartbeat_interval)), if heartbeat_interval != 0 => {
                            if let Err(e) = protocol::write_payload(&mut wr, &ControlChannelCmd::HeartBeat).await {
                                error!("{:#}", e);
                                break;
                            }
                }
                // The client never writes on the control channel, so any read result
                // means that it's gone. Breaking the loop removes the handle from the
                // map (see `do_control_channel_handshake`), so that the visitor of the
                // host gets a 404 instead of being handed over to a tunnel that never
                // forwards.
                ret = rd.read(&mut buf) => {
                    match ret {
                        Ok(0) => {
                            debug!("Control channel reached the end of the stream");
                            break;
                        }
                        Ok(n) => {
                            debug!("Ignoring {} unexpected byte(s) on the control channel", n);
                        }
                        Err(e) => {
                            debug!("Control channel read failed: {:#}", e);
                            break;
                        }
                    }
                }
                // Wait for the shutdown signal
                _ = shutdown_rx.recv() => {
                    break;
                }
            }
        }

        info!("Control channel shutdown");

        Ok(())
    }
}

/// A pool dispatcher. It owns the receiver of the data channels and a queue of
/// visitors waiting for one, matches them, and drops the surplus once the
/// demand decays. The waiters never hold a lock, so a visitor that gives up
/// doesn't block the others.
#[instrument(skip_all)]
async fn run_tcp_connection_pool(
    mut data_ch_rx: mpsc::Receiver<TcpStream>,
    mut visitor_rx: mpsc::Receiver<HttpVisitor>,
    data_pool: DataChannelPool,
) -> Result<()> {
    // The visitors that are waiting for a channel, in arrival order
    let (waiters_tx, mut waiters_rx) = mpsc::unbounded_channel::<oneshot::Sender<TcpStream>>();
    // The channels that arrived and were not handed to a visitor yet
    let mut warm: VecDeque<TcpStream> = VecDeque::new();
    let mut waiters: VecDeque<oneshot::Sender<TcpStream>> = VecDeque::new();

    let mut replenish = time::interval(Duration::from_millis(POOL_REPLENISH_INTERVAL_MS));
    replenish.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    // Warm the pool with the first batch
    data_pool.replenish();

    loop {
        tokio::select! {
            visitor = visitor_rx.recv() => {
                let Some(visitor) = visitor else {
                    break;
                };
                let waiters_tx = waiters_tx.clone();
                let data_pool = data_pool.clone();
                tokio::spawn(
                    async move {
                        if let Err(e) = forward_visitor(visitor, waiters_tx, data_pool).await {
                            warn!("Failed to forward a visitor: {:#}", e);
                        }
                    }
                    .in_current_span(),
                );
            }
            channel = data_ch_rx.recv() => {
                let Some(channel) = channel else {
                    break;
                };
                warm.push_back(channel);
                serve_waiters(&mut warm, &mut waiters, &data_pool);
            }
            waiter = waiters_rx.recv() => {
                let Some(waiter) = waiter else {
                    break;
                };
                waiters.push_back(waiter);
                serve_waiters(&mut warm, &mut waiters, &data_pool);
            }
            _ = replenish.tick() => {
                data_pool.replenish();
                // Collect the channels that are queued in the channel, then
                // drop the surplus that the decayed demand no longer wants, so
                // an idle tunnel doesn't hold connections open.
                while let Ok(channel) = data_ch_rx.try_recv() {
                    warm.push_back(channel);
                }
                serve_waiters(&mut warm, &mut waiters, &data_pool);
                while data_pool.warm_over_target() {
                    if warm.pop_front().is_some() {
                        data_pool.drop_warm();
                    } else {
                        break;
                    }
                }
            }
        }
    }

    info!("Shutdown");
    Ok(())
}

/// Hand the warm channels to the visitors that are waiting, dropping a channel
/// whose visitor gave up in the meantime.
fn serve_waiters(
    warm: &mut VecDeque<TcpStream>,
    waiters: &mut VecDeque<oneshot::Sender<TcpStream>>,
    data_pool: &DataChannelPool,
) {
    while !warm.is_empty() && !waiters.is_empty() {
        let channel = warm.pop_front().unwrap();
        let waiter = waiters.pop_front().unwrap();
        match waiter.send(channel) {
            Ok(()) => data_pool.consume(),
            // The visitor went away while waiting, so the channel is dropped
            Err(_) => data_pool.drop_warm(),
        }
    }
}

/// Relay a visitor and a data channel until both directions are done.
///
/// If the tunnel closes without sending a single byte back, the visitor is
/// answered with a `504` instead of being left with an empty reply. That is what
/// happens when a client cannot reach the `local_addr` of its tunnel: it opens
/// the data channel first (the handshake comes before it dials anything) and
/// then closes it with the visitor's request still unread, so the server sees a
/// reset. A reset is treated as the end of the tunnel, exactly like an end of
/// stream, so that the failure is answered instead of silently dropping the
/// visitor. `copy_bidirectional_with_sizes` cannot be used here: it returns on
/// the first IO error before it can answer the visitor.
#[instrument(skip_all)]
async fn relay(channel: TcpStream, stream: TcpStream) {
    let (mut ch_rd, mut ch_wr) = channel.into_split();
    let (mut v_rd, mut v_wr) = stream.into_split();

    // Visitor -> tunnel, until either side is done
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

    // Tunnel -> visitor. A `504` is written when the tunnel closes without ever
    // answering; it's only logged once the response is out, so a visitor that
    // went away first (the write fails) doesn't produce a misleading line.
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
                "The tunnel closed its data channel without sending a response; \
                 answered the visitor with 504"
            );
        }
        let _ = v_wr.shutdown().await;
    };

    tokio::join!(upstream, downstream);
}

/// Forward one visitor through a data channel, waiting for one when the pool is
/// empty. The wait is bounded, so a visitor is answered with a `504` instead of
/// hanging forever when the client cannot open a data channel. It also ends
/// early when the visitor goes away, so a dead visitor doesn't waste a channel.
#[instrument(skip_all)]
async fn forward_visitor(
    visitor: HttpVisitor,
    waiters_tx: mpsc::UnboundedSender<oneshot::Sender<TcpStream>>,
    data_pool: DataChannelPool,
) -> Result<()> {
    let HttpVisitor {
        mut stream,
        prefetched,
        activity,
    } = visitor;

    loop {
        let (waiter_tx, waiter_rx) = oneshot::channel();
        if waiters_tx.send(waiter_tx).is_err() {
            // The pool is gone, so no data channel will ever come.
            crate::http::respond_tunnel_unavailable(&mut stream).await;
            return Ok(());
        }

        match wait_for_channel(&mut stream, waiter_rx).await {
            ChannelWait::Got(mut channel) => {
                let ok = protocol::write_payload(&mut channel, &DataChannelCmd::StartForwardTcp)
                    .await
                    .is_ok()
                    && write_and_flush(&mut channel, &prefetched).await.is_ok();

                if ok {
                    tokio::spawn(
                        async move {
                            // Keep the visitor counted as active until the flow ends
                            let _activity = activity;
                            relay(channel, stream).await;
                        }
                        .in_current_span(),
                    );
                    return Ok(());
                }

                // The current data channel is broken. Top the pool up (bounded
                // by its target) and wait for a replacement channel.
                data_pool.replenish();
            }
            ChannelWait::TimedOut => {
                // The `504` path: the tunnel is connected (otherwise the wait
                // would end as `PoolClosed`), but its client did not provide a
                // data channel in time. This is not a routing miss (`404`): the
                // tunnel exists, so the visitor is answered with `504`. The tunnel
                // and its `local_addr` are fields of the enclosing `handle` span,
                // so the line names them at the default log level. A client that
                // cannot reach its `local_addr` does not land here: it opens the
                // data channel first and only dials the local service afterwards,
                // so that failure shows up on the client side instead (see
                // `run_data_channel_for_tcp`).
                warn!(
                    "The tunnel did not provide a data channel within {}s; \
                     answering the visitor with 504",
                    DATA_CHANNEL_WAIT_TIMEOUT
                );
                // The requests that were in flight never arrived; forget them
                // so that the pool can be replenished again.
                data_pool.forget_inflight();
                crate::http::respond_gateway_timeout(&mut stream).await;
                return Ok(());
            }
            ChannelWait::VisitorGone => {
                // The channel that was requested for this visitor is dropped by
                // the dispatcher when it arrives.
                debug!("The visitor went away while waiting for a data channel");
                return Ok(());
            }
            ChannelWait::PoolClosed => {
                // The control channel is gone, so no data channel will ever come.
                warn!(
                    "The control channel of the tunnel is gone; answering the visitor with 503"
                );
                crate::http::respond_tunnel_unavailable(&mut stream).await;
                return Ok(());
            }
        }
    }
}

/// The outcome of waiting for a data channel.
enum ChannelWait {
    Got(TcpStream),
    TimedOut,
    VisitorGone,
    PoolClosed,
}

/// Wait for a data channel, a timeout, or for the visitor to disconnect. The
/// visitor is watched with `peek`, which doesn't consume the request body; once
/// any data is seen it's left alone, since only the EOF means it's gone.
async fn wait_for_channel(
    stream: &mut TcpStream,
    mut waiter_rx: oneshot::Receiver<TcpStream>,
) -> ChannelWait {
    let deadline = time::sleep(Duration::from_secs(DATA_CHANNEL_WAIT_TIMEOUT));
    tokio::pin!(deadline);
    let mut peek_buf = [0u8; 1];
    let mut watch_disconnect = true;

    loop {
        tokio::select! {
            received = &mut waiter_rx => {
                return match received {
                    Ok(channel) => ChannelWait::Got(channel),
                    Err(_) => ChannelWait::PoolClosed,
                };
            }
            _ = &mut deadline => return ChannelWait::TimedOut,
            peeked = stream.peek(&mut peek_buf), if watch_disconnect => {
                match peeked {
                    // The visitor closed its side; stop waiting for a channel
                    Ok(0) => return ChannelWait::VisitorGone,
                    // There is a request body in flight, so the visitor is
                    // alive; stop watching the socket and wait for the channel
                    Ok(_) => watch_disconnect = false,
                    Err(_) => return ChannelWait::VisitorGone,
                }
            }
        }
    }
}
