use crate::cli::{ClientArgs, DEFAULT_CLIENT_WEB_PORT};
use crate::config::{ClientConfig, ClientTunnelConfig, MaskedString};
use crate::constants::run_control_chan_backoff;
use crate::protocol::{self, Ack, Auth, ClientHello, ControlChannelCmd, DataChannelCmd, ServerHello};
use crate::transport::{connect, AddrMaybeCached, SocketOpts};
use anyhow::{anyhow, bail, Context, Result};
use backoff::backoff::Backoff;
use backoff::future::retry_notify;
use backoff::ExponentialBackoff;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{copy_bidirectional_with_sizes, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock};
use tokio::time::{self, Duration, Instant};
use tracing::{debug, error, info, instrument, warn, Instrument, Span};

// The size of the buffers that are piped between the data channel and the local
// service. Larger than the tokio default (8 KiB), which reduces the number of
// syscalls on large transfers.
const COPY_BUF_SIZE: usize = 64 * 1024;

// The entrypoint of running a client
pub async fn run_client(args: ClientArgs, mut shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
    // The fixed TOML file is the only source of client configuration.
    let config = ClientConfig::from_file(&args.config_path).await?;

    let remote = config.remote.clone().ok_or_else(|| {
        anyhow!(
            "The address of the server is missing. Set `remote` in {:?}",
            args.config_path
        )
    })?;
    let name = config.name.clone().ok_or_else(|| {
        anyhow!(
            "The name of the client is missing. Set `name` in {:?}",
            args.config_path
        )
    })?;
    let token = config.token.clone().ok_or_else(|| {
        anyhow!(
            "The token is missing. Set `token` in {:?}",
            args.config_path
        )
    })?;
    if &*token == "replace-with-the-same-secret-as-hts" {
        bail!("Replace the placeholder token in {:?}", args.config_path);
    }

    let web_port = config.web_port.unwrap_or(DEFAULT_CLIENT_WEB_PORT);
    let identity = Identity { remote, name, token };

    // A definitive failure (a wrong token) is reported here so that the client
    // exits instead of retrying forever
    let (fatal_tx, mut fatal_rx) = mpsc::unbounded_channel::<anyhow::Error>();
    let state = Arc::new(ClientState::new(
        config,
        args.config_path.clone(),
        identity,
        fatal_tx,
    ));

    // Start the tunnels declared in the configuration
    state.sync().await;

    // Run the web management interface and administration API when a port is configured. A build without the
    // `client-api` feature has nothing to serve, so it only warns. A port of `0`
    // disables the web management service, so that several clients can share a host.
    #[cfg(feature = "client-api")]
    let web_task = match web_port {
        0 => None,
        port => {
            let token = state.token().to_owned();
            let state = state.clone();
            let shutdown_rx = shutdown_rx.resubscribe();
            Some(tokio::spawn(async move {
                if let Err(e) = crate::client_api::serve(port, token, state, shutdown_rx).await {
                    error!("{:#}", e);
                }
            }))
        }
    };

    #[cfg(not(feature = "client-api"))]
    if web_port != 0 {
        warn!(
            "The client web management interface is configured, but this build has \
             no `client-api` feature. It is disabled"
        );
    }

    let result = tokio::select! {
        err = fatal_rx.recv() => match err {
            Some(err) => Err(err),
            None => Ok(()),
        },
        _ = shutdown_rx.recv() => Ok(()),
    };

    // Stop every tunnel
    state.shutdown().await;

    // Stop the web management service if it is running
    #[cfg(feature = "client-api")]
    if let Some(web_task) = web_task {
        web_task.abort();
        let _ = web_task.await;
    }

    result
}

/// The identity of a client, resolved from the command line and the configuration
/// file
#[derive(Debug, Clone)]
pub(crate) struct Identity {
    pub(crate) remote: String,
    pub(crate) name: String,
    pub(crate) token: MaskedString,
}

/// The state of a client, shared between the control channels and the
/// administration API.
///
/// The configuration file is the source of truth: a change made through the API
/// is written back to it and applied to the running control channels.
pub(crate) struct ClientState {
    // The configuration, the source of truth of the tunnels
    config: RwLock<ClientConfig>,
    // Where the configuration is written back to
    config_path: PathBuf,
    // The resolved identity
    identity: Identity,
    // The active control channels, one per tunnel, keyed by domain
    tunnels: Mutex<HashMap<String, ControlChannelHandle>>,
    // The number of control channels that are currently established
    connections: Arc<AtomicUsize>,
    // Signals a definitive failure to the main loop, so that the client exits
    fatal_tx: mpsc::UnboundedSender<anyhow::Error>,
}

impl ClientState {
    fn new(
        config: ClientConfig,
        config_path: PathBuf,
        identity: Identity,
        fatal_tx: mpsc::UnboundedSender<anyhow::Error>,
    ) -> ClientState {
        ClientState {
            config: RwLock::new(config),
            config_path,
            identity,
            tunnels: Mutex::new(HashMap::new()),
            connections: Arc::new(AtomicUsize::new(0)),
            fatal_tx,
        }
    }

    /// The shared token, used by the administration API for authentication
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) fn token(&self) -> &str {
        &self.identity.token
    }

    /// The name of the client, reported by the administration API
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) fn name(&self) -> &str {
        &self.identity.name
    }

    /// The address of the server, reported by the administration API
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) fn remote(&self) -> &str {
        &self.identity.remote
    }

    /// Whether at least one control channel is established, reported by the
    /// administration API
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) fn connected(&self) -> bool {
        self.connections.load(Ordering::Relaxed) > 0
    }

    /// The tunnels the client is serving, sorted by domain
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) async fn tunnels(&self) -> Vec<ClientTunnelConfig> {
        self.config.read().await.resolved_tunnels()
    }

    /// Start, restart and stop the control channels so that they match the
    /// configuration
    async fn sync(&self) {
        let (tunnels, heartbeat_timeout) = {
            let config = self.config.read().await;
            (config.resolved_tunnels(), config.heartbeat_timeout())
        };

        let mut handles = self.tunnels.lock().await;

        // Stop the tunnels that are no longer configured
        let names: HashSet<&str> = tunnels.iter().map(|t| t.name.as_str()).collect();
        let stale: Vec<String> = handles
            .keys()
            .filter(|n| !names.contains(n.as_str()))
            .cloned()
            .collect();
        for name in stale {
            if let Some(handle) = handles.remove(&name) {
                info!("Stopping the tunnel `{}`", name);
                handle.shutdown();
            }
        }

        // Start the new tunnels and restart the ones whose config changed
        for tunnel in tunnels {
            let restart = match handles.get(&tunnel.name) {
                Some(handle) => !handle.matches(&tunnel, heartbeat_timeout),
                None => true,
            };
            if !restart {
                continue;
            }

            if let Some(handle) = handles.remove(&tunnel.name) {
                handle.shutdown();
            }

            let handle = ControlChannelHandle::new(
                tunnel.clone(),
                self.identity.name.clone(),
                self.identity.token.clone(),
                self.identity.remote.clone(),
                heartbeat_timeout,
                self.connections.clone(),
                self.fatal_tx.clone(),
            );
            handles.insert(tunnel.name.clone(), handle);
        }
    }

    /// Create or replace the tunnel of the given name, persist the
    /// configuration, and apply the change
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) async fn put_tunnel(
        &self,
        name: String,
        domain: String,
        local_addr: String,
    ) -> Result<()> {
        if local_addr.is_empty() {
            bail!("The `local_addr` must not be empty");
        }
        let domain = crate::helper::normalize_domain(&domain)
            .ok_or_else(|| anyhow!("The domain `{}` is invalid", domain))?;

        {
            let mut config = self.config.write().await;
            let mut candidate = config.clone();
            candidate.set_tunnel(name, domain, local_addr);
            // Validate after the change, so that a name or domain clash is
            // rejected before anything is written
            ClientConfig::validate(&mut candidate)?;
            // Persist first, so that a failed write leaves the runtime untouched
            candidate.save(&self.config_path).await?;
            *config = candidate;
        }

        self.sync().await;
        Ok(())
    }

    /// Delete the tunnel of the given name, persist the configuration, and
    /// apply the change
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) async fn delete_tunnel(&self, name: &str) -> Result<()> {
        {
            let mut config = self.config.write().await;
            let mut candidate = config.clone();
            if !candidate.remove_tunnel(name.trim()) {
                bail!("No such a tunnel `{}`", name);
            }
            candidate.save(&self.config_path).await?;
            *config = candidate;
        }

        self.sync().await;
        Ok(())
    }

    /// Stop every control channel
    async fn shutdown(&self) {
        let mut handles = self.tunnels.lock().await;
        for (_, handle) in handles.drain() {
            handle.shutdown();
        }
    }
}

/// A config-session error that the server reported as definitive, e.g. the token
/// is wrong. Retrying cannot fix it, so the error is marked to make the client
/// exit instead of reconnecting forever.
#[derive(Debug)]
struct FatalControlError(anyhow::Error);

impl std::fmt::Display for FatalControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for FatalControlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

struct RunDataChannelArgs {
    session_key: Nonce,
    remote_addr: AddrMaybeCached,
    socket_opts: SocketOpts,
    tunnel: ClientTunnelConfig,
}

async fn do_data_channel_handshake(args: Arc<RunDataChannelArgs>) -> Result<TcpStream> {
    // Retry at least every 100ms, at most for 10 seconds
    let backoff = ExponentialBackoff {
        max_interval: Duration::from_millis(100),
        max_elapsed_time: Some(Duration::from_secs(10)),
        ..Default::default()
    };

    // Connect to remote_addr
    let mut conn: TcpStream = retry_notify(
        backoff,
        || async {
            connect(&args.remote_addr)
                .await
                .with_context(|| format!("Failed to connect to {}", args.remote_addr))
                .map_err(backoff::Error::transient)
        },
        |e, duration| {
            warn!("{:#}. Retry in {:?}", e, duration);
        },
    )
    .await?;

    // Apply socket options for the data channel
    args.socket_opts.apply(&conn);

    // Send hello
    let hello = ClientHello::Data {
        version: protocol::CURRENT_PROTO_VERSION,
        nonce: args.session_key,
    };
    protocol::write_payload(&mut conn, &hello).await?;

    Ok(conn)
}

async fn run_data_channel(args: Arc<RunDataChannelArgs>) -> Result<()> {
    // Do the handshake
    let mut conn = do_data_channel_handshake(args.clone()).await?;

    // Forward
    match protocol::read_payload::<DataChannelCmd, _>(&mut conn).await? {
        DataChannelCmd::StartForwardTcp => {
            run_data_channel_for_tcp(conn, &args.tunnel.local_addr, args.tunnel.auth_header.as_ref().map(|v| &**v)).await?;
        }
    }
    Ok(())
}

// Simply copying back and forth for TCP
#[instrument(skip(conn))]
async fn run_data_channel_for_tcp(mut conn: TcpStream, local_addr: &str, auth: Option<&str>) -> Result<()> {
    debug!("New data channel starts forwarding");
    let mut header = Vec::with_capacity(1024);
    loop {
        if header.windows(4).any(|w| w == b"\r\n\r\n") { break; }
        if header.len() >= 8 * 1024 { bail!("HTTP request header is too large"); }
        let mut chunk = [0u8; 1024];
        let n = conn.read(&mut chunk).await.context("Failed to read HTTP request header from tunnel")?;
        if n == 0 { bail!("Tunnel closed before the HTTP request header was complete"); }
        header.extend_from_slice(&chunk[..n]);
    }
    let mut local = crate::helper::connect_local(local_addr)
        .await.with_context(|| format!("Failed to connect to local service {}", local_addr))?;
    let request = crate::helper::inject_auth_header(&header, auth);
    local.write_all(&request).await.context("Failed to write HTTP request to local service")?;
    let _ = copy_bidirectional_with_sizes(&mut conn, &mut local, COPY_BUF_SIZE, COPY_BUF_SIZE).await;
    Ok(())
}

// Control channel
struct ControlChannel {
    name: String,                       // The client name, announced to the server
    tunnel: ClientTunnelConfig,         // The tunnel: its name, domain and `local_addr`
    token: MaskedString,                // The token given by `--token`
    shutdown_rx: oneshot::Receiver<u8>, // Receives the shutdown signal
    remote_addr: String,                // `--remote`
    heartbeat_timeout: u64,             // Application layer heartbeat timeout in secs
    connections: Arc<AtomicUsize>,      // The number of established control channels
}

type Nonce = protocol::Digest;
// Handle of a control channel
// Dropping it will also drop the actual control channel
struct ControlChannelHandle {
    shutdown_tx: oneshot::Sender<u8>,
    // Keep the config that the channel was created with, so that it can be
    // compared with the one that is configured later
    tunnel: ClientTunnelConfig,
    heartbeat_timeout: u64,
}

impl ControlChannelHandle {
    // Whether the channel is already serving the given config
    fn matches(&self, tunnel: &ClientTunnelConfig, heartbeat_timeout: u64) -> bool {
        &self.tunnel == tunnel && self.heartbeat_timeout == heartbeat_timeout
    }
}

// Counts an established control channel while it lives, so that the API can
// report whether the client is connected to the server.
struct ConnectionsGuard {
    connections: Arc<AtomicUsize>,
}

impl ConnectionsGuard {
    fn new(connections: Arc<AtomicUsize>) -> ConnectionsGuard {
        connections.fetch_add(1, Ordering::Relaxed);
        ConnectionsGuard { connections }
    }
}

impl Drop for ConnectionsGuard {
    fn drop(&mut self) {
        self.connections.fetch_sub(1, Ordering::Relaxed);
    }
}

impl ControlChannel {
    #[instrument(skip_all)]
    async fn run(&mut self) -> Result<()> {
        let mut remote_addr = AddrMaybeCached::new(&self.remote_addr);
        remote_addr.resolve().await?;

        let mut conn = connect(&remote_addr)
            .await
            .with_context(|| format!("Failed to connect to {}", self.remote_addr))?;
        SocketOpts::for_control_channel().apply(&conn);

        // Send hello. The client announces the domain it serves, which is how the
        // server learns its tunnels.
        debug!("Sending hello");
        let hello = ClientHello::Control {
            version: protocol::CURRENT_PROTO_VERSION,
            domain: self.tunnel.domain.clone(),
            name: self.name.clone(),
            nodelay: self.tunnel.nodelay,
        };
        protocol::write_payload(&mut conn, &hello).await?;

        // Read hello
        debug!("Reading hello");
        let ServerHello { version, nonce } = protocol::read_payload(&mut conn).await?;
        protocol::check_version(version)?;

        // Send auth
        debug!("Sending auth");
        let mut concat = Vec::from(self.token.as_bytes());
        concat.extend_from_slice(&nonce);

        let session_key = protocol::digest(&concat);
        protocol::write_payload(&mut conn, &Auth(session_key)).await?;

        // Read ack
        debug!("Reading ack");
        match protocol::read_payload::<Ack, _>(&mut conn).await? {
            Ack::Ok => {}
            Ack::AuthFailed => {
                // The token cannot change while the client runs, so retrying is
                // pointless: mark the error fatal so that the client exits.
                return Err(FatalControlError(anyhow!(
                    "The token was rejected by the server for the tunnel `{}`",
                    self.tunnel.domain
                ))
                .into());
            }
        }

        // Channel ready. Keep it counted until it goes away
        info!("Control channel established");
        let _connected = ConnectionsGuard::new(self.connections.clone());

        // Socket options for the data channel
        let socket_opts = SocketOpts::data_channel(self.tunnel.nodelay);
        let data_ch_args = Arc::new(RunDataChannelArgs {
            session_key,
            remote_addr,
            socket_opts,
            tunnel: self.tunnel.clone(),
        });

        loop {
            tokio::select! {
                val = protocol::read_payload::<ControlChannelCmd, _>(&mut conn) => {
                    let val = val?;
                    debug!( "Received {:?}", val);
                    match val {
                        ControlChannelCmd::CreateDataChannel => {
                            let args = data_ch_args.clone();
                            tokio::spawn(async move {
                                if let Err(e) = run_data_channel(args).await.with_context(|| "Failed to run the data channel") {
                                    warn!("{:#}", e);
                                }
                            }.instrument(Span::current()));
                        },
                        ControlChannelCmd::HeartBeat => ()
                    }
                },
                _ = time::sleep(Duration::from_secs(self.heartbeat_timeout)), if self.heartbeat_timeout != 0 => {
                    return Err(anyhow!("Heartbeat timed out"))
                }
                _ = &mut self.shutdown_rx => {
                    break;
                }
            }
        }

        info!("Control channel shutdown");
        Ok(())
    }
}

impl ControlChannelHandle {
    #[instrument(name="handle", skip_all, fields(name = %tunnel.name, domain = %tunnel.domain, local_addr = %tunnel.local_addr))]
    fn new(
        tunnel: ClientTunnelConfig,
        name: String,
        token: MaskedString,
        remote_addr: String,
        heartbeat_timeout: u64,
        connections: Arc<AtomicUsize>,
        fatal_tx: mpsc::UnboundedSender<anyhow::Error>,
    ) -> ControlChannelHandle {
        let digest = protocol::digest(tunnel.name.as_bytes());

        info!("Starting {}", hex::encode(digest));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let mut retry_backoff = run_control_chan_backoff(tunnel.retry_interval);

        let handle_tunnel = tunnel.clone();
        let mut s = ControlChannel {
            name,
            tunnel,
            token,
            shutdown_rx,
            remote_addr,
            heartbeat_timeout,
            connections,
        };

        tokio::spawn(
            async move {
                let mut start = Instant::now();

                while let Err(err) = s
                    .run()
                    .await
                    .with_context(|| "Failed to run the control channel")
                {
                    if s.shutdown_rx.try_recv() != Err(oneshot::error::TryRecvError::Empty) {
                        break;
                    }

                    // A definitive rejection (a wrong token) is not going to
                    // succeed on a retry, so report it and stop
                    if let Some(fatal) = err.downcast_ref::<FatalControlError>() {
                        let _ = fatal_tx.send(anyhow!("{}", fatal.0));
                        break;
                    }

                    if start.elapsed() > Duration::from_secs(3) {
                        // The client runs for at least 3 secs and then disconnects
                        retry_backoff.reset();
                    }

                    if let Some(duration) = retry_backoff.next_backoff() {
                        error!("{:#}. Retry in {:?}...", err, duration);
                        time::sleep(duration).await;
                    } else {
                        // Should never reach
                        panic!("{:#}. Break", err);
                    }

                    start = Instant::now();
                }
            }
            .instrument(Span::current()),
        );

        ControlChannelHandle {
            shutdown_tx,
            tunnel: handle_tunnel,
            heartbeat_timeout,
        }
    }

    fn shutdown(self) {
        // A send failure shows that the actor has already shutdown.
        let _ = self.shutdown_tx.send(0u8);
    }
}
