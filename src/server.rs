use crate::config::{
    ClientConfig, MaskedString, ServerClientConfig, ServerConfig, ServerTunnelConfig,
};
use crate::constants::listen_backoff;
use crate::helper::{retry_notify_with_deadline, to_bind_addr, write_and_flush};
use crate::http::RoutingTable;
use crate::multi_map::MultiMap;
use crate::protocol::Hello::{ConfigChannelHello, ControlChannelHello, DataChannelHello};
use crate::protocol::{
    self, read_auth, read_hello, Ack, ControlChannelCmd, DataChannelCmd, Hello, HASH_WIDTH_IN_BYTES,
};
use crate::transport::SocketOpts;
use anyhow::{anyhow, bail, Context, Result};
use backoff::backoff::Backoff;
use backoff::ExponentialBackoff;

use rand::RngCore;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{
    copy_bidirectional_with_sizes, split, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
    ReadBuf,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio::time;
use tracing::{debug, error, info, info_span, instrument, warn, Instrument, Span};

pub(crate) type TunnelDigest = protocol::Digest; // SHA256 of a tunnel name
pub(crate) type ClientDigest = protocol::Digest; // SHA256 of a client name
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

/// Runtime counters of the HTTP entrypoint, exposed through the administration
/// API.
#[derive(Default)]
pub(crate) struct ServerMetrics {
    pub(crate) visitors_accepted: AtomicU64,
    pub(crate) responses_400: AtomicU64,
    pub(crate) responses_404: AtomicU64,
    pub(crate) responses_408: AtomicU64,
    pub(crate) responses_431: AtomicU64,
    pub(crate) responses_503: AtomicU64,
    pub(crate) responses_504: AtomicU64,
    pub(crate) bytes_from_visitors: AtomicU64,
    pub(crate) bytes_to_visitors: AtomicU64,
    pub(crate) pool_outstanding: AtomicUsize,
    pub(crate) pool_active: AtomicUsize,
}

impl ServerMetrics {
    /// Record a visitor accepted by the HTTP entrypoint
    pub(crate) fn accept_visitor(&self) {
        self.visitors_accepted.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an HTTP response by its status line
    pub(crate) fn record_response(&self, status: &str) {
        let counter = match status {
            "400 Bad Request" => &self.responses_400,
            "404 Not Found" => &self.responses_404,
            "408 Request Timeout" => &self.responses_408,
            "431 Request Header Fields Too Large" => &self.responses_431,
            "503 Service Unavailable" => &self.responses_503,
            "504 Gateway Timeout" => &self.responses_504,
            _ => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// A JSON snapshot of the counters, for the administration API
    pub(crate) fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "visitors_accepted": self.visitors_accepted.load(Ordering::Relaxed),
            "responses": {
                "400": self.responses_400.load(Ordering::Relaxed),
                "404": self.responses_404.load(Ordering::Relaxed),
                "408": self.responses_408.load(Ordering::Relaxed),
                "431": self.responses_431.load(Ordering::Relaxed),
                "503": self.responses_503.load(Ordering::Relaxed),
                "504": self.responses_504.load(Ordering::Relaxed),
            },
            "bytes_from_visitors": self.bytes_from_visitors.load(Ordering::Relaxed),
            "bytes_to_visitors": self.bytes_to_visitors.load(Ordering::Relaxed),
            "pool_outstanding": self.pool_outstanding.load(Ordering::Relaxed),
            "pool_active": self.pool_active.load(Ordering::Relaxed),
        })
    }
}

/// Wraps a stream and counts the bytes read from and written to it, so that the
/// server can report the traffic of a visitor per direction.
struct CountingStream<T> {
    inner: T,
    metrics: Arc<ServerMetrics>,
}

impl<T> CountingStream<T> {
    fn new(inner: T, metrics: Arc<ServerMetrics>) -> Self {
        CountingStream { inner, metrics }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for CountingStream<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let res = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &res {
            let n = buf.filled().len() - before;
            self.metrics
                .bytes_from_visitors
                .fetch_add(n as u64, Ordering::Relaxed);
        }
        res
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for CountingStream<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let res = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &res {
            self.metrics
                .bytes_to_visitors
                .fetch_add(*n as u64, Ordering::Relaxed);
        }
        res
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

// A client of `[clients]`, together with the config that is pushed to it
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ServerClient {
    name: String,
    token: MaskedString,
    config: ClientConfig,
}

// A tunnel, together with the token of the client that serves it
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct TunnelRuntime {
    config: ServerTunnelConfig,
    token: MaskedString,
    heartbeat_interval: u64,
}

// A hash map of ControlChannelHandles, indexed by TunnelDigest or Nonce
// See also MultiMap
pub(crate) type ControlChannelMap = MultiMap<TunnelDigest, Nonce, ControlChannelHandle>;

// A registered config channel, so that the config updates can be pushed to the client.
// `id` identifies the connection that owns the handle, so that a connection only
// removes its own handle when it goes away.
struct ConfigChannelHandle {
    id: usize,
    tx: mpsc::Sender<ClientConfig>,
}

// ServerState holds all the mutable state of a running server. It's shared by the
// server tasks and the administration API.
pub(crate) struct ServerState {
    // The path of the config file, to write the changes back to
    pub(crate) config_path: PathBuf,
    // The authoritative configuration, mutated by the administration API
    pub(crate) config: RwLock<ServerConfig>,
    // The tunnels of all the clients, indexed by TunnelDigest
    pub(crate) tunnels: RwLock<HashMap<TunnelDigest, TunnelRuntime>>,
    // `[clients]`, indexed by the digest of the client name
    pub(crate) clients: RwLock<HashMap<ClientDigest, ServerClient>>,
    // Collection of control channels
    pub(crate) control_channels: RwLock<ControlChannelMap>,
    // `Host` -> TunnelDigest
    pub(crate) routing_table: RwLock<RoutingTable>,
    // The config channels of the connected clients, keyed by the client digest
    config_channels: RwLock<HashMap<ClientDigest, ConfigChannelHandle>>,
    // A monotonically increasing id, identifying a config channel connection
    next_conn_id: AtomicUsize,
    // Serialize the configuration changes, so that two concurrent applies can't
    // overwrite each other
    apply_lock: Mutex<()>,
    // The visitors that are currently being served
    pub(crate) activity: ActivityTracker,
    // The runtime counters
    pub(crate) metrics: Arc<ServerMetrics>,
}

// The entrypoint of running a server
pub async fn run_server(
    config: ServerConfig,
    config_path: PathBuf,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let mut server = Server::from(config, config_path).await?;
    server.run(shutdown_rx).await?;

    Ok(())
}

// The settings of the administration API, derived from the config
struct ApiConfig {
    bind_addr: String,
    token: String,
}

// Server holds all states of running a server
struct Server {
    state: Arc<ServerState>,
    bind_addr: String,
    http_bind_addr: String,
    api: Option<ApiConfig>,
}

// Generate the tunnels of all the clients, indexed by TunnelDigest
fn generate_tunnel_hashmap(server_config: &ServerConfig) -> HashMap<TunnelDigest, TunnelRuntime> {
    let mut ret = HashMap::new();
    for client in server_config.clients.values() {
        let heartbeat_interval = client.heartbeat_interval();
        for (name, s) in &client.tunnels {
            ret.insert(
                protocol::digest(name.as_bytes()),
                TunnelRuntime {
                    config: s.clone(),
                    token: client.token.clone(),
                    heartbeat_interval,
                },
            );
        }
    }
    ret
}

// Generate the clients, indexed by the digest of their name
fn generate_client_hashmap(server_config: &ServerConfig) -> HashMap<ClientDigest, ServerClient> {
    let mut ret = HashMap::new();
    for (name, client) in &server_config.clients {
        ret.insert(
            protocol::digest(name.as_bytes()),
            ServerClient {
                name: name.clone(),
                token: client.token.clone(),
                config: client.to_client_config(),
            },
        );
    }
    ret
}

// Generate a routing table which maps a `Host` domain to a TunnelDigest
fn generate_routing_table(server_config: &ServerConfig) -> RoutingTable {
    let mut ret = RoutingTable::default();
    for client in server_config.clients.values() {
        for (name, s) in &client.tunnels {
            let digest = protocol::digest(name.as_bytes());
            ret.insert(&s.domain, digest);
        }
    }
    ret
}

impl ServerState {
    fn new(config: ServerConfig, config_path: PathBuf) -> ServerState {
        let tunnels = generate_tunnel_hashmap(&config);
        let clients = generate_client_hashmap(&config);
        let routing_table = generate_routing_table(&config);
        ServerState {
            config_path,
            config: RwLock::new(config),
            tunnels: RwLock::new(tunnels),
            clients: RwLock::new(clients),
            control_channels: RwLock::new(ControlChannelMap::new()),
            routing_table: RwLock::new(routing_table),
            config_channels: RwLock::new(HashMap::new()),
            next_conn_id: AtomicUsize::new(0),
            apply_lock: Mutex::new(()),
            activity: ActivityTracker::default(),
            metrics: Arc::new(ServerMetrics::default()),
        }
    }

    /// Apply a change to the configuration and make it effective at runtime.
    ///
    /// The candidate is validated and written back to the config file first, so that
    /// a failed write leaves the runtime untouched. Then the derived maps are rebuilt,
    /// the tunnels of the removed or changed tunnels are dropped, and the client
    /// configs are pushed to the affected connected clients.
    pub(crate) async fn apply<F>(&self, f: F) -> Result<()>
    where
        F: FnOnce(&mut ServerConfig) -> Result<()>,
    {
        let _guard = self.apply_lock.lock().await;

        let mut config = self.config.read().await.clone();
        f(&mut config)?;
        ServerConfig::validate(&mut config)?;

        self.persist(&config)
            .await
            .with_context(|| "Failed to write the config back")?;

        let new_tunnels = generate_tunnel_hashmap(&config);
        let new_clients = generate_client_hashmap(&config);
        let new_routing_table = generate_routing_table(&config);

        // Diff the clients before the map is replaced
        let (changed_clients, removed_clients) = {
            let old = self.clients.read().await;
            let changed: Vec<ClientDigest> = new_clients
                .iter()
                .filter(|(d, c)| old.get(*d) != Some(*c))
                .map(|(d, _)| *d)
                .collect();
            let removed: Vec<ClientDigest> = old
                .keys()
                .filter(|d| !new_clients.contains_key(*d))
                .copied()
                .collect();
            (changed, removed)
        };

        // Drop the control channels of the removed or changed tunnels. Dropping the
        // handle shuts the control channel down, so the client reconnects and picks up
        // the change.
        {
            let old_tunnels = self.tunnels.read().await;
            let mut ccs = self.control_channels.write().await;
            for (digest, old) in old_tunnels.iter() {
                if new_tunnels.get(digest) != Some(old) {
                    ccs.remove1(digest);
                }
            }
        }

        // Replace the derived maps
        *self.tunnels.write().await = new_tunnels;
        *self.routing_table.write().await = new_routing_table;

        // Drop the config channels of the removed clients
        {
            let mut chans = self.config_channels.write().await;
            for d in &removed_clients {
                chans.remove(d);
            }
        }

        // Push the new config to the connected clients whose config changed
        {
            let chans = self.config_channels.read().await;
            for d in &changed_clients {
                if let Some(handle) = chans.get(d) {
                    if let Some(client) = new_clients.get(d) {
                        let _ = handle.tx.try_send(client.config.clone());
                    }
                }
            }
        }

        *self.clients.write().await = new_clients;
        *self.config.write().await = config;

        Ok(())
    }

    /// Write the configuration back to the config file atomically
    async fn persist(&self, config: &ServerConfig) -> Result<()> {
        let s = config.to_toml()?;
        let tmp = self.config_path.with_extension("toml.tmp");
        tokio::fs::write(&tmp, s.as_bytes())
            .await
            .with_context(|| format!("Failed to write the temporary config {:?}", tmp))?;
        tokio::fs::rename(&tmp, &self.config_path)
            .await
            .with_context(|| format!("Failed to replace the config {:?}", self.config_path))?;
        Ok(())
    }

    /// Create a client
    pub(crate) async fn create_client(
        &self,
        name: String,
        client: ServerClientConfig,
    ) -> Result<()> {
        if name.is_empty() {
            bail!("The name of the client must not be empty");
        }
        self.apply(move |config| {
            if config.clients.contains_key(&name) {
                bail!("The client `{}` already exists", name);
            }
            config.clients.insert(name, client);
            Ok(())
        })
        .await
    }

    /// Read the current configuration
    pub(crate) async fn snapshot(&self) -> ServerConfig {
        self.config.read().await.clone()
    }

    /// Update a client in place
    pub(crate) async fn update_client<F>(&self, name: &str, f: F) -> Result<()>
    where
        F: FnOnce(&mut ServerClientConfig) -> Result<()>,
    {
        let name = name.to_string();
        self.apply(move |config| {
            let client = config
                .clients
                .get_mut(&name)
                .ok_or_else(|| anyhow!("No such a client `{}`", name))?;
            f(client)
        })
        .await
    }

    /// Delete a client together with its tunnels
    pub(crate) async fn delete_client(&self, name: &str) -> Result<()> {
        let name = name.to_string();
        self.apply(move |config| {
            if config.clients.remove(&name).is_none() {
                bail!("No such a client `{}`", name);
            }
            Ok(())
        })
        .await
    }

    /// Create or replace a tunnel of a client
    pub(crate) async fn put_tunnel(
        &self,
        client: &str,
        domain: String,
        tunnel_config: ServerTunnelConfig,
    ) -> Result<()> {
        if domain.is_empty() {
            bail!("The domain of the tunnel must not be empty");
        }
        let client = client.to_string();
        self.apply(move |config| {
            let c = config
                .clients
                .get_mut(&client)
                .ok_or_else(|| anyhow!("No such a client `{}`", client))?;
            c.tunnels.insert(domain, tunnel_config);
            Ok(())
        })
        .await
    }

    /// Delete a tunnel of a client
    pub(crate) async fn delete_tunnel(&self, client: &str, domain: &str) -> Result<()> {
        let client = client.to_string();
        let domain = domain.to_string();
        self.apply(move |config| {
            let c = config
                .clients
                .get_mut(&client)
                .ok_or_else(|| anyhow!("No such a client `{}`", client))?;
            if c.tunnels.remove(&domain).is_none() {
                bail!("No such a tunnel `{}` of the client `{}`", domain, client);
            }
            Ok(())
        })
        .await
    }
}

impl Server {
    // Create a server from the config
    pub async fn from(mut config: ServerConfig, config_path: PathBuf) -> Result<Server> {
        ServerConfig::validate(&mut config)?;

        let api = match (&config.api_bind_addr, &config.api_token) {
            (Some(bind_addr), Some(token)) => Some(ApiConfig {
                bind_addr: to_bind_addr(bind_addr),
                token: token.to_string(),
            }),
            (Some(_), None) => bail!(
                "`api_bind_addr` is set but `api_token` is not. The administration API requires a token"
            ),
            (None, Some(_)) => {
                warn!("`api_token` is set but `api_bind_addr` is not. The administration API is disabled");
                None
            }
            (None, None) => None,
        };

        let bind_addr = to_bind_addr(&config.bind_addr);
        let http_bind_addr = to_bind_addr(&config.http_bind_addr);
        let state = Arc::new(ServerState::new(config, config_path));

        Ok(Server {
            state,
            bind_addr,
            http_bind_addr,
            api,
        })
    }

    // The entry point of Server
    pub async fn run(&mut self, mut shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
        // Listen at `bind_addr` for the control and data channels of clients
        let l: TcpListener = retry_notify_with_deadline(
            listen_backoff(),
            || async { Ok(TcpListener::bind(&self.bind_addr).await?) },
            |e, duration| {
                error!("{:#}. Retry in {:?}", e, duration);
            },
            &mut shutdown_rx,
        )
        .await
        .with_context(|| "Failed to listen at `bind_addr`")?;

        info!("Listening at {}", self.bind_addr);

        // Run the HTTP entrypoint which routes visitors by the `Host` header
        let http_task = tokio::spawn(crate::http::serve(
            self.http_bind_addr.clone(),
            self.state.clone(),
            shutdown_rx.resubscribe(),
        ));

        // Run the administration API and the web UI, if configured
        let api_task = match &self.api {
            Some(api) => Some(tokio::spawn(crate::admin::serve(
                api.bind_addr.clone(),
                api.token.clone(),
                self.state.clone(),
                shutdown_rx.resubscribe(),
            ))),
            None => None,
        };

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

        if let Some(api_task) = api_task {
            let _ = api_task.await;
        }

        // Drop the tunnels and the config channels, so that the clients reconnect (and
        // pick up the fresh state) when the server is restarted. Without this, the
        // in-process tasks of this server would keep the connections alive.
        *self.state.control_channels.write().await = ControlChannelMap::new();
        self.state.config_channels.write().await.clear();

        info!("Shutdown");

        Ok(())
    }
}

// Plain TCP accepts the connection as is
async fn handshake(conn: TcpStream) -> Result<TcpStream> {
    Ok(conn)
}

// Handle connections to `bind_addr`
async fn handle_connection(mut conn: TcpStream, state: Arc<ServerState>) -> Result<()> {
    // Read hello
    let hello = read_hello(&mut conn).await?;
    match hello {
        ControlChannelHello(_, tunnel_digest) => {
            do_control_channel_handshake(conn, state, tunnel_digest).await?;
        }
        ConfigChannelHello(_, client_digest) => {
            do_config_channel_handshake(conn, state, client_digest).await?;
        }
        DataChannelHello(_, nonce) => {
            do_data_channel_handshake(conn, state, nonce).await?;
        }
    }
    Ok(())
}

// Push the config of a client to it, and keep the config channel open to push the
// updates made through the administration API. The client doesn't have any config
// of its own.
async fn do_config_channel_handshake(
    mut conn: TcpStream,
    state: Arc<ServerState>,
    client_digest: ClientDigest,
) -> Result<()> {
    info!("Try to handshake a config channel");

    SocketOpts::for_control_channel().apply(&conn);

    // Generate a nonce
    let mut nonce = vec![0u8; HASH_WIDTH_IN_BYTES];
    rand::thread_rng().fill_bytes(&mut nonce);

    // Send hello
    let hello_send = Hello::ConfigChannelHello(
        protocol::CURRENT_PROTO_VERSION,
        nonce.clone().try_into().unwrap(),
    );
    conn.write_all(&bincode::serialize(&hello_send).unwrap())
        .await?;
    conn.flush().await?;

    // Read auth. It is read before the lookup so that an unknown client is rejected
    // only once its auth has been drained: closing the connection with unread data
    // sends a reset instead of the ack, and the client would misread the definitive
    // rejection as a transient network failure and retry forever
    let protocol::Auth(d) = read_auth(&mut conn).await?;

    // Lookup the client
    let client = state.clients.read().await.get(&client_digest).cloned();
    let Some(client) = client else {
        conn.write_all(&bincode::serialize(&Ack::TunnelNotExist).unwrap())
            .await?;
        conn.flush().await?;
        bail!("No such a client {}", hex::encode(client_digest));
    };

    // Calculate the checksum
    let mut concat = Vec::from(client.token.as_bytes());
    concat.append(&mut nonce);

    // Validate
    let session_key = protocol::digest(&concat);
    if session_key != d {
        conn.write_all(&bincode::serialize(&Ack::AuthFailed).unwrap())
            .await?;
        conn.flush().await?;
        debug!(
            "Expect {}, but got {}",
            hex::encode(session_key),
            hex::encode(d)
        );
        bail!("Client {} failed the authentication", client.name);
    }

    conn.write_all(&bincode::serialize(&Ack::Ok).unwrap())
        .await?;
    conn.flush().await?;

    // Register the config channel before pushing, so that a change made through the
    // API in the meantime isn't missed. Replacing a previous handle drops its sender,
    // which makes the stale connection exit.
    let (tx, mut rx) = mpsc::channel::<ClientConfig>(CHAN_SIZE);
    let conn_id = state.next_conn_id.fetch_add(1, Ordering::Relaxed);
    {
        let mut chans = state.config_channels.write().await;
        chans.insert(client_digest, ConfigChannelHandle { id: conn_id, tx });
    }

    // Push the freshest config, then the later updates, until the client goes away
    let push_result = {
        let latest = state.clients.read().await.get(&client_digest).cloned();
        match latest {
            Some(latest) => run_config_push(&mut conn, &latest, &mut rx).await,
            None => Ok(()),
        }
    };

    // Only remove our own handle, so that a reconnected client isn't dropped
    {
        let mut chans = state.config_channels.write().await;
        if chans.get(&client_digest).map(|h| h.id) == Some(conn_id) {
            chans.remove(&client_digest);
        }
    }

    push_result
}

// Push the config of a client to it, then the pushed updates, until the channel fails
async fn run_config_push(
    conn: &mut TcpStream,
    client: &ServerClient,
    rx: &mut mpsc::Receiver<ClientConfig>,
) -> Result<()> {
    protocol::write_payload(conn, &client.config).await?;
    info!(client = %client.name, tunnels = client.config.tunnels.len(), "Config pushed");

    while let Some(config) = rx.recv().await {
        if let Err(e) = protocol::write_payload(conn, &config).await {
            debug!("Failed to push the config to {}: {:#}", client.name, e);
            break;
        }
        info!(client = %client.name, tunnels = config.tunnels.len(), "Config pushed");
    }

    Ok(())
}

async fn do_control_channel_handshake(
    mut conn: TcpStream,
    state: Arc<ServerState>,
    tunnel_digest: TunnelDigest,
) -> Result<()> {
    info!("Try to handshake a control channel");

    SocketOpts::for_control_channel().apply(&conn);

    // Generate a nonce
    let mut nonce = vec![0u8; HASH_WIDTH_IN_BYTES];
    rand::thread_rng().fill_bytes(&mut nonce);

    // Send hello
    let hello_send = Hello::ControlChannelHello(
        protocol::CURRENT_PROTO_VERSION,
        nonce.clone().try_into().unwrap(),
    );
    conn.write_all(&bincode::serialize(&hello_send).unwrap())
        .await?;
    conn.flush().await?;

    // Lookup the tunnel
    let tunnel = match state.tunnels.read().await.get(&tunnel_digest) {
        Some(v) => v,
        None => {
            conn.write_all(&bincode::serialize(&Ack::TunnelNotExist).unwrap())
                .await?;
            bail!("No such a tunnel {}", hex::encode(tunnel_digest));
        }
    }
    .to_owned();

    let tunnel_config = tunnel.config;
    let tunnel_name = &tunnel_config.domain;
    let heartbeat_interval = tunnel.heartbeat_interval;

    // Calculate the checksum with the token of the client that serves the tunnel
    let mut concat = Vec::from(tunnel.token.as_bytes());
    concat.append(&mut nonce);

    // Read auth
    let protocol::Auth(d) = read_auth(&mut conn).await?;

    // Validate
    let session_key = protocol::digest(&concat);
    if session_key != d {
        conn.write_all(&bincode::serialize(&Ack::AuthFailed).unwrap())
            .await?;
        debug!(
            "Expect {}, but got {}",
            hex::encode(session_key),
            hex::encode(d)
        );
        bail!("Tunnel {} failed the authentication", tunnel_name);
    } else {
        let conn_id = state.next_conn_id.fetch_add(1, Ordering::Relaxed);

        // If there's already a control channel for the tunnel, then drop the old one.
        // Because a control channel doesn't report back when it's dead,
        // the handle in the map could be stall, dropping the old handle enables
        // the client to reconnect.
        if state
            .control_channels
            .write()
            .await
            .remove1(&tunnel_digest)
            .is_some()
        {
            warn!(
                "Dropping previous control channel for tunnel {}",
                tunnel_name
            );
        }

        // Send ack
        conn.write_all(&bincode::serialize(&Ack::Ok).unwrap())
            .await?;
        conn.flush().await?;

        info!(tunnel = %tunnel_config.domain, "Control channel established");
        let (handle, ch_task) = ControlChannelHandle::new(
            conn,
            tunnel_config,
            heartbeat_interval,
            conn_id,
            state.metrics.clone(),
        );

        // Insert the new handle
        let _ = state
            .control_channels
            .write()
            .await
            .insert(tunnel_digest, session_key, handle);

        // When the control channel is gone (the client crashed, was stopped, or lost
        // its connection), remove its handle so that the visitors are answered with a
        // 503 instead of being handed over to a tunnel that will never receive them.
        // Only remove our own handle, so that a reconnected client isn't dropped.
        tokio::spawn(async move {
            let _ = ch_task.await;
            let mut h = state.control_channels.write().await;
            if h.get1(&tunnel_digest).map(|h| h.id) == Some(conn_id) {
                h.remove1(&tunnel_digest);
            }
        });
    }

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
            SocketOpts::from_server_cfg(&handle.tunnel).apply(&conn);

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
    // Visitors currently being forwarded
    active: Arc<AtomicUsize>,
    // A decaying high-water mark of the requests that were in flight, used to
    // keep the pool warm for the next burst instead of only for the visitors
    // that are being forwarded right now
    demand: Arc<AtomicUsize>,
    // The server-wide counters, mirrored so that the pool is visible in the API
    metrics: Arc<ServerMetrics>,
}

impl DataChannelPool {
    fn new(req_tx: mpsc::UnboundedSender<bool>, metrics: Arc<ServerMetrics>) -> DataChannelPool {
        DataChannelPool {
            req_tx,
            inflight: Arc::new(AtomicUsize::new(0)),
            warm: Arc::new(AtomicUsize::new(0)),
            active: Arc::new(AtomicUsize::new(0)),
            demand: Arc::new(AtomicUsize::new(0)),
            metrics,
        }
    }

    /// Ask the client for a data channel. Returns `false` when the control
    /// channel is gone, so no data channel will ever come.
    fn request(&self) -> bool {
        self.inflight.fetch_add(1, Ordering::Relaxed);
        self.metrics.pool_outstanding.fetch_add(1, Ordering::Relaxed);
        if self.req_tx.send(true).is_err() {
            self.inflight.fetch_sub(1, Ordering::Relaxed);
            self.metrics.pool_outstanding.fetch_sub(1, Ordering::Relaxed);
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
        let _ = self.metrics.pool_outstanding.try_update(
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
        let _ = self.metrics.pool_outstanding.try_update(
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
        let lost = self.inflight.swap(0, Ordering::Relaxed);
        if lost > 0 {
            let _ = self.metrics.pool_outstanding.try_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |v| Some(v.saturating_sub(lost)),
            );
        }
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
        let _ = self.metrics.pool_outstanding.try_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |v| Some(v.saturating_sub(1)),
        );
    }

    /// The pool task is ending, so its channels go away with it. `pool_outstanding`
    /// is a server-wide counter shared by every pool, so the channels that are
    /// still counted here have to be removed from it; otherwise a control
    /// channel that reconnects would leave its old pool's count behind.
    fn discard(&self) {
        let lost = self.warm.swap(0, Ordering::Relaxed) + self.inflight.swap(0, Ordering::Relaxed);
        if lost > 0 {
            let _ = self.metrics.pool_outstanding.try_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |v| Some(v.saturating_sub(lost)),
            );
        }
    }

    fn metrics(&self) -> &Arc<ServerMetrics> {
        &self.metrics
    }

    /// A guard that counts a visitor as active for as long as it's forwarded
    fn forward_guard(&self) -> ForwardGuard {
        self.active.fetch_add(1, Ordering::Relaxed);
        self.metrics.pool_active.fetch_add(1, Ordering::Relaxed);
        self.note_demand();
        ForwardGuard {
            active: self.active.clone(),
            metrics: self.metrics.clone(),
        }
    }
}

struct ForwardGuard {
    active: Arc<AtomicUsize>,
    metrics: Arc<ServerMetrics>,
}

impl Drop for ForwardGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Relaxed);
        self.metrics.pool_active.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) struct ControlChannelHandle {
    // Identifies the connection that owns the handle, so that it only removes its own
    // handle when it goes away
    id: usize,
    // Shutdown the control channel by dropping it
    _shutdown_tx: broadcast::Sender<bool>,
    data_ch_tx: mpsc::Sender<TcpStream>,
    pub(crate) visitor_tx: mpsc::Sender<HttpVisitor>,
    // The data channel pool of the tunnel
    pub(crate) data_pool: DataChannelPool,
    tunnel: ServerTunnelConfig,
}

impl ControlChannelHandle {
    // Create a control channel handle, where the control channel handling task
    // and the connection pool task are created.
    #[instrument(name = "handle", skip_all, fields(tunnel = %tunnel.domain))]
    fn new(
        conn: TcpStream,
        tunnel: ServerTunnelConfig,
        heartbeat_interval: u64,
        id: usize,
        metrics: Arc<ServerMetrics>,
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
        let data_pool = DataChannelPool::new(data_ch_req_tx, metrics);

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
        // visitors be answered with a 503 instead of being handed over to a dead
        // tunnel.
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
                _shutdown_tx: shutdown_tx,
                data_ch_tx,
                visitor_tx,
                data_pool,
                tunnel,
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

        let create_ch_cmd = bincode::serialize(&ControlChannelCmd::CreateDataChannel).unwrap();
        let heartbeat = bincode::serialize(&ControlChannelCmd::HeartBeat).unwrap();

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
                            if let Err(e) = write_and_flush(&mut wr, &create_ch_cmd).await {
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
                            if let Err(e) = write_and_flush(&mut wr, &heartbeat).await {
                                error!("{:#}", e);
                                break;
                            }
                }
                // The client never writes on the control channel, so any read result
                // means that it's gone. Breaking the loop removes the handle from the
                // map (see `do_control_channel_handshake`), so that the visitors get a
                // 503 instead of being handed over to a tunnel that never forwards.
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

    // The channels that were still queued or on their way are gone with this
    // pool; remove them from the server-wide counter.
    data_pool.discard();

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
    let cmd = bincode::serialize(&DataChannelCmd::StartForwardTcp).unwrap();

    loop {
        let (waiter_tx, waiter_rx) = oneshot::channel();
        if waiters_tx.send(waiter_tx).is_err() {
            // The pool is gone, so no data channel will ever come.
            crate::http::respond_tunnel_unavailable(&mut stream, data_pool.metrics()).await;
            return Ok(());
        }

        match wait_for_channel(&mut stream, waiter_rx).await {
            ChannelWait::Got(mut channel) => {
                let ok = write_and_flush(&mut channel, &cmd).await.is_ok()
                    && write_and_flush(&mut channel, &prefetched).await.is_ok();

                if ok {
                    let guard = data_pool.forward_guard();
                    let stream = CountingStream::new(stream, data_pool.metrics().clone());
                    tokio::spawn(
                        async move {
                            // Keep the visitor counted as active until the flow ends
                            let _guard = guard;
                            let _activity = activity;
                            let mut stream = stream;
                            let _ = copy_bidirectional_with_sizes(
                                &mut channel,
                                &mut stream,
                                COPY_BUF_SIZE,
                                COPY_BUF_SIZE,
                            )
                            .await;
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
                debug!("Timed out waiting for a data channel");
                // The requests that were in flight never arrived; forget them
                // so that the pool can be replenished again.
                data_pool.forget_inflight();
                crate::http::respond_gateway_timeout(&mut stream, data_pool.metrics()).await;
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
                crate::http::respond_tunnel_unavailable(&mut stream, data_pool.metrics()).await;
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
