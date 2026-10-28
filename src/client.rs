use crate::cli::{ClientArgs, DEFAULT_CLIENT_WEB_PORT};
use crate::client_env::{self, ClientEnv, LoadedConfig};
use crate::config::{ClientConfig, ClientTunnelConfig, MaskedString};
use crate::constants::run_control_chan_backoff;
use crate::protocol::{
    self, Ack, Auth, ClientHello, ControlChannelCmd, DataChannelCmd, ServerHello,
};
use crate::transport::websocket::{connect_websocket, AuthenticationRejected, BoxedIo};
use anyhow::{anyhow, bail, Context, Result};
use backoff::backoff::Backoff;
use backoff::future::retry_notify;
use backoff::ExponentialBackoff;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{copy_bidirectional_with_sizes, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock};
use tokio::time::{self, Duration, Instant};
use tracing::{debug, error, info, instrument, warn, Instrument, Span};

// The size of the buffers that are piped between the data channel and the local
// service. Larger than the tokio default (8 KiB), which reduces the number of
// syscalls on large transfers.
const COPY_BUF_SIZE: usize = 64 * 1024;

// The entrypoint of running a client
pub async fn run_client(
    args: ClientArgs,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    // The fixed TOML file is the only configuration file, but every setting has
    // an `HTC_*` environment counterpart that wins over it. A missing or
    // unusable configuration is not fatal: it is reported and the process then
    // waits for a shutdown signal. A client running in a container thus stays up
    // while its configuration is being fixed, instead of exiting and making the
    // container restart-loop.
    let loaded = match client_env::load(&args.config_path).await {
        Ok(loaded) => loaded,
        Err(e) => return wait_after_error(e, &mut shutdown_rx).await,
    };

    // Log the sources, never the values
    if !loaded.env.is_empty() {
        info!(
            "Configuration overridden by the environment: {}",
            loaded.env.vars().join(", ")
        );
    }
    if loaded.env.tunnels_managed() {
        info!(
            "The tunnels are managed by HTC_TUNNELS; the web page and the API cannot change them"
        );
    }

    let config = &loaded.effective;
    let identity = match resolve_identity(config, &args.config_path) {
        Ok(identity) => identity,
        Err(e) => return wait_after_error(e, &mut shutdown_rx).await,
    };

    info!(
        "Using the WebSocket transport over {}: {} is the server's `server_host` and `server_port`",
        if identity.tls {
            "TLS (wss)"
        } else {
            "plain text (ws); do not use it on the public Internet"
        },
        identity.server_addr
    );

    let web_port = config.web_port.unwrap_or(DEFAULT_CLIENT_WEB_PORT);
    #[cfg(feature = "client-api")]
    let web_host = config.web_host().to_string();

    // A definitive failure (a wrong token) is reported here so that the client
    // exits instead of retrying forever
    let (fatal_tx, mut fatal_rx) = mpsc::unbounded_channel::<anyhow::Error>();
    let state = Arc::new(ClientState::new(
        loaded,
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
                if let Err(e) =
                    crate::client_api::serve(&web_host, port, token, state, shutdown_rx).await
                {
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

/// Resolve the identity of a client from its configuration, rejecting a missing
/// or placeholder value.
fn resolve_identity(config: &ClientConfig, config_path: &Path) -> Result<Identity> {
    let transport = config.transport_name().to_string();
    let server_addr = config.server_addr().ok_or_else(|| {
        anyhow!(
            "The server endpoint is missing. Set `server` in {:?} or `HTC_SERVER`, e.g. \
             `server = \"wss://control.example.com:443\"`. Its host must be the server's \
             `server_host`.",
            config_path
        )
    })?;
    let name = config.name.clone().ok_or_else(|| {
        anyhow!(
            "The name of the client is missing. Set `name` in {:?} or `HTC_NAME`",
            config_path
        )
    })?;
    let token = config.token.clone().ok_or_else(|| {
        anyhow!(
            "The token is missing. Set `token` in {:?} or `HTC_TOKEN`",
            config_path
        )
    })?;
    if &*token == "replace-with-the-same-secret-as-hts" {
        bail!(
            "Replace the placeholder token in {:?} or `HTC_TOKEN`",
            config_path
        );
    }
    let tls = config.tls();
    let keepalive_interval = config.keepalive_interval();
    Ok(Identity {
        server_addr,
        name,
        token,
        transport,
        tls,
        keepalive_interval,
    })
}

/// Report a startup error and then wait for a shutdown signal instead of
/// returning it.
///
/// A client in a container should stay up while its configuration is prepared,
/// rather than exiting and being restarted in a loop.
async fn wait_after_error(
    e: anyhow::Error,
    shutdown_rx: &mut broadcast::Receiver<bool>,
) -> Result<()> {
    error!("{:#}", e);
    let _ = shutdown_rx.recv().await;
    Ok(())
}

/// The identity of a client, resolved from the command line and the configuration
/// file
#[derive(Debug, Clone)]
pub(crate) struct Identity {
    pub(crate) server_addr: String,
    pub(crate) transport: String,
    pub(crate) name: String,
    pub(crate) token: MaskedString,
    /// Whether the WebSocket connection uses TLS (`wss://`)
    pub(crate) tls: bool,
    /// The WebSocket ping interval in secs, `0` meaning disabled
    pub(crate) keepalive_interval: u64,
}

/// The state of a client, shared between the control channels and the
/// administration API.
///
/// The configuration file is the persisted source of truth: a change made
/// through the API is written back to it and applied to the running control
/// channels. The `HTC_*` environment variables override it at runtime, but they
/// are never written back.
pub(crate) struct ClientState {
    // The configuration the client runs with, environment included
    config: RwLock<ClientConfig>,
    // The configuration as read from the file, which the API writes back to
    persisted: RwLock<ClientConfig>,
    // The environment variables that override the file
    env: ClientEnv,
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
        loaded: LoadedConfig,
        config_path: PathBuf,
        identity: Identity,
        fatal_tx: mpsc::UnboundedSender<anyhow::Error>,
    ) -> ClientState {
        ClientState {
            config: RwLock::new(loaded.effective),
            persisted: RwLock::new(loaded.persisted),
            env: loaded.env,
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
    pub(crate) fn server_addr(&self) -> &str {
        &self.identity.server_addr
    }

    /// The transport mode, reported by the administration API
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) fn transport(&self) -> &str {
        &self.identity.transport
    }

    /// Whether at least one control channel is established, reported by the
    /// administration API
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) fn connected(&self) -> bool {
        self.connections.load(Ordering::Relaxed) > 0
    }

    /// Whether the tunnels come from `HTC_TUNNELS` and thus cannot be changed
    /// through the API
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) fn tunnels_managed(&self) -> bool {
        self.env.tunnels_managed()
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
        let transport = self.identity.transport.clone();
        let tls = self.identity.tls;
        let keepalive_interval = self.identity.keepalive_interval;

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
                Some(handle) => !handle.matches(
                    &tunnel,
                    heartbeat_timeout,
                    &transport,
                    tls,
                    keepalive_interval,
                ),
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
                self.identity.server_addr.clone(),
                transport.clone(),
                tls,
                keepalive_interval,
                heartbeat_timeout,
                self.connections.clone(),
                self.fatal_tx.clone(),
            );
            handles.insert(tunnel.name.clone(), handle);
        }
    }

    /// Apply a change to the persisted configuration, save it, and recompute the
    /// effective configuration with the environment overlay.
    ///
    /// The file is written first, so that a failed write leaves the runtime
    /// untouched. The environment values are reapplied afterwards: they win at
    /// runtime but must not end up in the file.
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    async fn update_persisted<F>(&self, change: F) -> Result<()>
    where
        F: FnOnce(&mut ClientConfig) -> Result<()>,
    {
        if self.env.tunnels_managed() {
            bail!(
                "The tunnels are managed by HTC_TUNNELS; change that environment variable and \
                 restart htc"
            );
        }

        let mut persisted = self.persisted.write().await;
        let mut candidate = persisted.clone();
        change(&mut candidate)?;
        // Validate after the change, so that a name or domain clash is rejected
        // before anything is written
        ClientConfig::validate(&mut candidate)?;
        candidate.save(&self.config_path).await?;

        let mut effective = candidate.clone();
        self.env.apply(&mut effective);
        ClientConfig::validate(&mut effective)?;

        *persisted = candidate;
        *self.config.write().await = effective;
        Ok(())
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

        self.update_persisted(|config| {
            config.set_tunnel(name, domain, local_addr);
            Ok(())
        })
        .await?;

        self.sync().await;
        Ok(())
    }

    /// Delete the tunnel of the given name, persist the configuration, and
    /// apply the change
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) async fn delete_tunnel(&self, name: &str) -> Result<()> {
        self.update_persisted(|config| {
            if !config.remove_tunnel(name.trim()) {
                bail!("No such a tunnel `{}`", name);
            }
            Ok(())
        })
        .await?;

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
    server_addr: String,
    tls: bool,
    token: MaskedString,
    keepalive_interval: u64,
    tunnel: ClientTunnelConfig,
}

async fn do_data_channel_handshake(args: Arc<RunDataChannelArgs>) -> Result<BoxedIo> {
    // Retry at least every 100ms, at most for 10 seconds.
    let backoff = ExponentialBackoff {
        max_interval: Duration::from_millis(100),
        max_elapsed_time: Some(Duration::from_secs(10)),
        ..Default::default()
    };

    let mut conn: BoxedIo = retry_notify(
        backoff,
        || async {
            connect_websocket(
                &args.server_addr,
                args.tls,
                &args.token,
                args.keepalive_interval,
            )
            .await
            .with_context(|| format!("Failed to connect to {}", args.server_addr))
            .map_err(backoff::Error::transient)
        },
        |e, duration| {
            warn!("{:#}. Retry in {:?}...", e, duration);
        },
    )
    .await?;

    // Identify this as a data channel with the same length-prefixed protocol.
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
            run_data_channel_forward(
                conn,
                &args.tunnel.local_addr,
                args.tunnel.auth_header.as_ref().map(|v| &**v),
            )
            .await?;
        }
    }
    Ok(())
}

// Copy the visitor stream to the local service and back
#[instrument(skip(conn))]
async fn run_data_channel_forward(
    mut conn: BoxedIo,
    local_addr: &str,
    auth: Option<&str>,
) -> Result<()> {
    debug!("New data channel starts forwarding");
    let mut header = Vec::with_capacity(1024);
    loop {
        if header.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if header.len() >= 8 * 1024 {
            bail!("HTTP request header is too large");
        }
        let mut chunk = [0u8; 1024];
        let n = conn
            .read(&mut chunk)
            .await
            .context("Failed to read HTTP request header from tunnel")?;
        if n == 0 {
            bail!("Tunnel closed before the HTTP request header was complete");
        }
        header.extend_from_slice(&chunk[..n]);
    }
    let request = match crate::helper::gate_auth_header(&header, auth) {
        crate::helper::AuthOutcome::Allowed(request) => request,
        crate::helper::AuthOutcome::Unauthorized(failure) => {
            debug!(
                "Rejected an unauthenticated request for {}: {:?}",
                local_addr, failure
            );
            conn.write_all(&crate::helper::unauthorized_response(failure))
                .await
                .context("Failed to write unauthorized response to tunnel")?;
            return Ok(());
        }
    };
    let mut local = crate::helper::connect_local(local_addr)
        .await
        .with_context(|| format!("Failed to connect to local service {}", local_addr))?;
    local
        .write_all(&request)
        .await
        .context("Failed to write HTTP request to local service")?;
    let _ =
        copy_bidirectional_with_sizes(&mut conn, &mut local, COPY_BUF_SIZE, COPY_BUF_SIZE).await;
    Ok(())
}

// Control channel
struct ControlChannel {
    name: String,                       // The client name, announced to the server
    tunnel: ClientTunnelConfig,         // The tunnel: its name, domain and `local_addr`
    token: MaskedString,                // The token given by `--token`
    shutdown_rx: oneshot::Receiver<u8>, // Receives the shutdown signal
    server_addr: String,                // The `host:port` of the server endpoint
    tls: bool,                          // Whether the WebSocket connection uses TLS
    keepalive_interval: u64,            // WebSocket ping interval in secs
    heartbeat_timeout: u64,             // Application layer heartbeat timeout in secs
    connections: Arc<AtomicUsize>,      // The number of established control channels
}

type Nonce = protocol::Digest;
// Handle of a control channel
// Dropping it will also drop the actual control channel
pub(crate) struct ControlChannelHandle {
    shutdown_tx: oneshot::Sender<u8>,
    // The task that runs the control channel, so a caller that needs the route
    // withdrawn before continuing (a node on shutdown) can wait for it
    join: tokio::task::JoinHandle<()>,
    // Keep the config that the channel was created with, so that it can be
    // compared with the one that is configured later
    tunnel: ClientTunnelConfig,
    transport: String,
    tls: bool,
    keepalive_interval: u64,
    heartbeat_timeout: u64,
}

impl ControlChannelHandle {
    // Whether the channel is already serving the given config
    fn matches(
        &self,
        tunnel: &ClientTunnelConfig,
        heartbeat_timeout: u64,
        transport: &str,
        tls: bool,
        keepalive_interval: u64,
    ) -> bool {
        &self.tunnel == tunnel
            && self.heartbeat_timeout == heartbeat_timeout
            && self.transport.as_str() == transport
            && self.tls == tls
            && self.keepalive_interval == keepalive_interval
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
        let mut conn: BoxedIo = match connect_websocket(
            &self.server_addr,
            self.tls,
            &self.token,
            self.keepalive_interval,
        )
        .await
        {
            Ok(conn) => conn,
            Err(err) if err.downcast_ref::<AuthenticationRejected>().is_some() => {
                // The token cannot change while the client runs, so retrying is
                // pointless: mark it fatal so that the client exits.
                return Err(FatalControlError(anyhow!(
                    "The token was rejected by the server for the tunnel `{}`",
                    self.tunnel.domain
                ))
                .into());
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("Failed to connect to {}", self.server_addr))
            }
        };

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

        let data_ch_args = Arc::new(RunDataChannelArgs {
            session_key,
            server_addr: self.server_addr.clone(),
            tls: self.tls,
            token: self.token.clone(),
            keepalive_interval: self.keepalive_interval,
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
    pub(crate) fn new(
        tunnel: ClientTunnelConfig,
        name: String,
        token: MaskedString,
        server_addr: String,
        transport: String,
        tls: bool,
        keepalive_interval: u64,
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
            server_addr,
            tls,
            keepalive_interval,
            heartbeat_timeout,
            connections,
        };

        let join = tokio::spawn(
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
            join,
            tunnel: handle_tunnel,
            transport,
            tls,
            keepalive_interval,
            heartbeat_timeout,
        }
    }

    fn shutdown(self) {
        // A send failure shows that the actor has already shutdown.
        let _ = self.shutdown_tx.send(0u8);
    }

    /// Stop the channel and wait until its connection is gone, so that the
    /// server of the next hop has withdrawn the route before the caller
    /// continues (used by a node on shutdown).
    pub(crate) async fn shutdown_and_wait(self) {
        let ControlChannelHandle {
            shutdown_tx, join, ..
        } = self;
        let _ = shutdown_tx.send(0u8);
        let _ = join.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_env::{ENV_TOKEN, ENV_TUNNELS};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A unique path in the temporary directory, removed by the caller.
    fn temp_path(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "http-tunnel-client-{}-{}-{}.toml",
            std::process::id(),
            tag,
            count
        ))
    }

    fn identity() -> Identity {
        Identity {
            server_addr: "127.0.0.1:8080".to_string(),
            transport: "ws".to_string(),
            name: "test".to_string(),
            token: MaskedString::from("token"),
            tls: false,
            keepalive_interval: 15,
        }
    }

    fn state(loaded: LoadedConfig, path: PathBuf) -> ClientState {
        let (fatal_tx, _fatal_rx) = mpsc::unbounded_channel();
        ClientState::new(loaded, path, identity(), fatal_tx)
    }

    #[tokio::test]
    async fn test_api_edits_persist_only_the_file_baseline() -> Result<()> {
        let path = temp_path("baseline");
        let persisted = ClientConfig::parse(
            r#"
server = "wss://file.example.com:443"
name = "file"
token = "file-token"

[[tunnels]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
"#,
        )?;
        let env = ClientEnv::from_lookup(|key| {
            Ok(match key {
                ENV_TOKEN => Some("env-token".to_string()),
                _ => None,
            })
        })?;
        let mut effective = persisted.clone();
        env.apply(&mut effective);
        let state = state(
            LoadedConfig {
                effective,
                persisted,
                env,
            },
            path.clone(),
        );

        state
            .update_persisted(|config| {
                config.set_tunnel(
                    "git".to_string(),
                    "git.example.com".to_string(),
                    "127.0.0.1:81".to_string(),
                );
                Ok(())
            })
            .await?;

        let saved = std::fs::read_to_string(&path)?;
        let _ = std::fs::remove_file(&path);

        // The environment token wins at runtime but is never written to disk
        assert!(saved.contains("file-token"), "{}", saved);
        assert!(!saved.contains("env-token"), "{}", saved);
        // The new tunnel is persisted and applied
        assert!(saved.contains("git.example.com"), "{}", saved);
        assert_eq!(state.tunnels().await.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_environment_tunnels_are_read_only() -> Result<()> {
        let path = temp_path("read-only");
        let env = ClientEnv::from_lookup(|key| {
            Ok(match key {
                ENV_TUNNELS => Some(
                    r#"[{name="nas",domain="nas.example.com",local_addr="127.0.0.1:80"}]"#
                        .to_string(),
                ),
                _ => None,
            })
        })?;
        let mut effective = ClientConfig::default();
        env.apply(&mut effective);
        ClientConfig::validate(&mut effective)?;
        let state = state(
            LoadedConfig {
                effective,
                persisted: ClientConfig::default(),
                env,
            },
            path.clone(),
        );

        assert!(state.tunnels_managed());
        let err = state
            .put_tunnel(
                "git".to_string(),
                "git.example.com".to_string(),
                "127.0.0.1:81".to_string(),
            )
            .await
            .expect_err("the tunnels of the environment must not be changed");
        assert!(format!("{:#}", err).contains("HTC_TUNNELS"), "{}", err);
        assert!(!path.exists(), "no configuration file must be created");
        Ok(())
    }
}
