use crate::cli::ClientArgs;
use crate::config::{ClientConfig, ClientTunnelConfig, MaskedString};
use crate::constants::run_control_chan_backoff;
use crate::protocol::Hello::{self, *};
use crate::protocol::{
    self, read_ack, read_control_cmd, read_data_cmd, read_hello, Ack, Auth, ClientConfigRequest,
    ControlChannelCmd, DataChannelCmd, ServerConfigPush, CURRENT_PROTO_VERSION,
    HASH_WIDTH_IN_BYTES,
};
use crate::transport::{connect, AddrMaybeCached, SocketOpts};
use anyhow::{anyhow, bail, Context, Result};
use backoff::backoff::Backoff;
use backoff::future::retry_notify;
use backoff::ExponentialBackoff;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::io::{copy_bidirectional_with_sizes, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};
use tokio::time::{self, Duration, Instant};
use tracing::{debug, error, info, instrument, warn, Instrument, Span};

// The interval between retries to fetch the config, before the client gets one
const DEFAULT_FETCH_RETRY_INTERVAL_SECS: u64 = 1;

// The size of the buffers that are piped between the data channel and the local
// service. Larger than the tokio default (8 KiB), which reduces the number of
// syscalls on large transfers.
const COPY_BUF_SIZE: usize = 64 * 1024;

// How long a change submitted through the administration API waits for the
// server to answer before giving up
#[cfg_attr(not(feature = "client-api"), allow(dead_code))]
const CONFIG_REQUEST_TIMEOUT_SECS: u64 = 10;

// The entrypoint of running a client
pub async fn run_client(args: ClientArgs, shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
    let mut client = Client::new(args);
    client.run(shutdown_rx).await
}

// A change to its own tunnels that the administration API asks the config
// connection to forward to the server. The `reply` carries the server's verdict
// back to the API handler.
struct TunnelRequest {
    msg: ClientConfigRequest,
    reply: oneshot::Sender<Result<(), String>>,
}

// The state of a client that is shared between the config connection and the
// administration API. The server remains the source of truth: `tunnels` is the
// config it pushed last, and a change is a request that the server validates and
// pushes back.
// The request machinery (`requester`, `request_lock` and the API-only methods)
// is unused when the API is compiled out, so the dead-code lint is relaxed there
#[cfg_attr(not(feature = "client-api"), allow(dead_code))]
pub(crate) struct ClientState {
    // The tunnels of the config that the server pushed last
    tunnels: RwLock<HashMap<String, ClientTunnelConfig>>,
    // The sender of the live config connection, if any. A request sent while no
    // connection is established is refused instead of being queued indefinitely.
    requester: RwLock<Option<mpsc::Sender<TunnelRequest>>>,
    // Serializes the requests, so that each reply matches the request sent first
    request_lock: tokio::sync::Mutex<()>,
}

#[cfg_attr(not(feature = "client-api"), allow(dead_code))]
impl ClientState {
    fn new() -> ClientState {
        ClientState {
            tunnels: RwLock::new(HashMap::new()),
            requester: RwLock::new(None),
            request_lock: tokio::sync::Mutex::new(()),
        }
    }

    // Replace the cached config with the one that the server pushed last
    async fn set_tunnels(&self, config: &ClientConfig) {
        let tunnels = config
            .tunnels
            .iter()
            .map(|t| (t.name.clone(), t.clone()))
            .collect();
        *self.tunnels.write().await = tunnels;
    }

    // The tunnels that the client is serving, sorted by name for a stable output
    pub(crate) async fn tunnels(&self) -> Vec<ClientTunnelConfig> {
        let mut tunnels: Vec<ClientTunnelConfig> =
            self.tunnels.read().await.values().cloned().collect();
        tunnels.sort_by(|a, b| a.name.cmp(&b.name));
        tunnels
    }

    // Register (or clear) the sender of the live config connection
    async fn set_requester(&self, tx: Option<mpsc::Sender<TunnelRequest>>) {
        *self.requester.write().await = tx;
    }

    // Whether a config connection is established, so that the API can report it
    pub(crate) async fn connected(&self) -> bool {
        self.requester.read().await.is_some()
    }

    // Submit a change to the server over the live config connection and wait for
    // its verdict. `Err` carries the reason that the server gave, or a message
    // that the request could not be delivered.
    pub(crate) async fn submit(&self, msg: ClientConfigRequest) -> Result<(), String> {
        // One request at a time, so that the reply of a request is never mistaken
        // for the reply of another one
        let _guard = self.request_lock.lock().await;

        let Some(tx) = self.requester.read().await.clone() else {
            return Err("The client is not connected to the server".to_string());
        };

        let (reply_tx, reply_rx) = oneshot::channel();
        let request = TunnelRequest {
            msg,
            reply: reply_tx,
        };
        if tx.send(request).await.is_err() {
            return Err("The client is not connected to the server".to_string());
        }

        match time::timeout(Duration::from_secs(CONFIG_REQUEST_TIMEOUT_SECS), reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("The connection to the server was closed".to_string()),
            Err(_) => Err("The server did not answer the request in time".to_string()),
        }
    }
}

// Holds the state of a client
struct Client {
    args: ClientArgs,
    tunnel_handles: HashMap<String, ControlChannelHandle>,
    // Shared with the config connection and the administration API
    state: Arc<ClientState>,
}

impl Client {
    fn new(args: ClientArgs) -> Client {
        Client {
            args,
            tunnel_handles: HashMap::new(),
            state: Arc::new(ClientState::new()),
        }
    }

    // The entrypoint of Client
    async fn run(&mut self, mut shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
        // The client has no config of its own. It's pushed by the server, and the
        // server keeps the config channel open to push the updates made through the
        // administration API.
        let (config_tx, mut config_rx) = mpsc::channel::<ClientConfig>(4);

        // Keep the handle instead of dropping it, so that a definitive failure of the
        // config session (unknown name, wrong token) makes the client exit non-zero
        // rather than being retried silently forever
        let config_session = {
            let args = self.args.clone();
            let state = self.state.clone();
            let shutdown_rx = shutdown_rx.resubscribe();
            tokio::spawn(
                async move { run_config_session(args, config_tx, state, shutdown_rx).await },
            )
        };

        // Run the administration API when a port is configured. A build without
        // the `client-api` feature has nothing to serve, so it only warns. A port
        // of `0` disables the API, so that several clients can share a host.
        #[cfg(feature = "client-api")]
        let api_task = match self.args.api_port {
            Some(port) if port != 0 => {
                let state = self.state.clone();
                let token = self.args.token.clone();
                let name = self.args.name.clone();
                let remote = self.args.remote.clone();
                let shutdown_rx = shutdown_rx.resubscribe();
                Some(tokio::spawn(async move {
                    if let Err(e) =
                        crate::client_api::serve(port, token, name, remote, state, shutdown_rx)
                            .await
                    {
                        error!("{:#}", e);
                    }
                }))
            }
            _ => None,
        };

        #[cfg(not(feature = "client-api"))]
        if matches!(self.args.api_port, Some(port) if port != 0) {
            warn!(
                "The client administration API is configured, but this build has \
                 no `client-api` feature. It is disabled"
            );
        }

        let mut shutting_down = false;
        loop {
            tokio::select! {
                maybe = config_rx.recv() => {
                    match maybe {
                        Some(config) => self.reconcile(config),
                        // The config session ended and dropped the sender, so its
                        // result is awaited below
                        None => break,
                    }
                }
                _ = shutdown_rx.recv() => {
                    shutting_down = true;
                    break;
                }
            }
        }

        // Stop the config session if it is still running. A requested shutdown wins,
        // otherwise its result is propagated, so that a fatal error exits the client.
        let result = if shutting_down {
            config_session.abort();
            let _ = config_session.await;
            Ok(())
        } else {
            match config_session.await {
                Ok(result) => result,
                Err(e) => Err(anyhow!("The config session panicked: {:#}", e)),
            }
        };

        // Stop the administration API if it is running
        #[cfg(feature = "client-api")]
        if let Some(api_task) = api_task {
            api_task.abort();
            let _ = api_task.await;
        }

        // Shutdown all tunnels
        for (_, handle) in self.tunnel_handles.drain() {
            handle.shutdown();
        }

        result
    }

    // Make the pushed config effective: start the new tunnels, stop the removed
    // ones, and restart the ones whose config changed.
    fn reconcile(&mut self, config: ClientConfig) {
        let token = MaskedString::from(self.args.token.as_str());

        let names: HashSet<&str> = config.tunnels.iter().map(|s| s.name.as_str()).collect();

        let stale: Vec<String> = self
            .tunnel_handles
            .keys()
            .filter(|n| !names.contains(n.as_str()))
            .cloned()
            .collect();
        for name in stale {
            if let Some(handle) = self.tunnel_handles.remove(&name) {
                info!("Stopping the tunnel `{}`", name);
                handle.shutdown();
            }
        }

        for tunnel in &config.tunnels {
            let restart = match self.tunnel_handles.get(&tunnel.name) {
                Some(handle) => !handle.matches(tunnel, config.heartbeat_timeout),
                None => true,
            };
            if !restart {
                continue;
            }

            if let Some(handle) = self.tunnel_handles.remove(&tunnel.name) {
                handle.shutdown();
            }

            let handle = ControlChannelHandle::new(
                tunnel.clone(),
                token.clone(),
                self.args.remote.clone(),
                config.heartbeat_timeout,
            );
            self.tunnel_handles.insert(tunnel.name.clone(), handle);
        }
    }
}

// Check whether the shutdown signal has arrived
fn is_shutdown(shutdown_rx: &mut broadcast::Receiver<bool>) -> bool {
    shutdown_rx.try_recv() != Err(broadcast::error::TryRecvError::Empty)
}

/// A config-session error that the server reported as definitive, e.g. the client
/// name is unknown or its token is wrong. Retrying cannot fix it, so the error is
/// marked to make the client exit instead of reconnecting forever.
#[derive(Debug)]
struct FatalConfigError(anyhow::Error);

impl std::fmt::Display for FatalConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for FatalConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

// Keep a config channel open, feeding the pushed configs to the reconciliation loop.
// It reconnects with a backoff when the channel drops, but returns immediately on a
// definitive rejection from the server.
async fn run_config_session(
    args: ClientArgs,
    tx: mpsc::Sender<ClientConfig>,
    state: Arc<ClientState>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let mut backoff = ExponentialBackoff {
        max_interval: Duration::from_secs(DEFAULT_FETCH_RETRY_INTERVAL_SECS),
        max_elapsed_time: None,
        ..Default::default()
    };

    loop {
        if let Err(e) = run_config_connection(&args, &tx, &state, &shutdown_rx).await {
            if is_shutdown(&mut shutdown_rx) {
                return Ok(());
            }
            // A definitive rejection is not going to succeed on a retry, so give the
            // error back to the caller and let the client exit
            if e.downcast_ref::<FatalConfigError>().is_some() {
                return Err(e);
            }
            error!("{:#}", e);
        }

        if is_shutdown(&mut shutdown_rx) {
            return Ok(());
        }

        let duration = backoff
            .next_backoff()
            .unwrap_or(Duration::from_secs(DEFAULT_FETCH_RETRY_INTERVAL_SECS));
        tokio::select! {
            _ = time::sleep(duration) => {}
            _ = shutdown_rx.recv() => return Ok(()),
        }
    }
}

// Fetch the config and then keep reading the config updates pushed by the
// server. It also forwards the changes requested by the administration API and
// feeds the resulting config to the reconciliation loop.
async fn run_config_connection(
    args: &ClientArgs,
    tx: &mpsc::Sender<ClientConfig>,
    state: &Arc<ClientState>,
    shutdown_rx: &broadcast::Receiver<bool>,
) -> Result<()> {
    let mut shutdown_rx = shutdown_rx.resubscribe();

    let mut remote_addr = AddrMaybeCached::new(&args.remote);
    remote_addr.resolve().await.with_context(|| {
        format!(
            "Failed to resolve the address of the server {}",
            args.remote
        )
    })?;

    let mut conn = connect(&remote_addr)
        .await
        .with_context(|| format!("Failed to connect to {}", remote_addr))?;
    SocketOpts::for_control_channel().apply(&conn);

    // Send hello. The client identifies itself by its name
    debug!("Sending hello");
    let hello_send = Hello::ConfigChannelHello(
        CURRENT_PROTO_VERSION,
        protocol::digest(args.name.as_bytes()),
    );
    conn.write_all(&bincode::serialize(&hello_send).unwrap())
        .await?;
    conn.flush().await?;

    // Read hello
    debug!("Reading hello");
    let nonce = match read_hello(&mut conn).await? {
        ConfigChannelHello(_, d) => d,
        _ => {
            bail!("Unexpected type of hello");
        }
    };

    // Send auth
    debug!("Sending auth");
    let mut concat = Vec::from(args.token.as_bytes());
    concat.extend_from_slice(&nonce);

    let session_key = protocol::digest(&concat);
    let auth = Auth(session_key);
    conn.write_all(&bincode::serialize(&auth).unwrap()).await?;
    conn.flush().await?;

    // Read ack
    debug!("Reading ack");
    match read_ack(&mut conn).await? {
        Ack::Ok => {}
        // The server rejected the config channel in a way that a retry cannot fix, so
        // mark the error fatal and let the client exit instead of retrying forever
        Ack::TunnelNotExist => {
            return Err(FatalConfigError(anyhow!(
                "The client `{}` is not configured on the server",
                args.name
            ))
            .into());
        }
        Ack::AuthFailed => {
            return Err(FatalConfigError(anyhow!(
                "The token of the client `{}` is incorrect",
                args.name
            ))
            .into());
        }
    }

    info!("Config channel established");

    let (mut rd, mut wr) = conn.into_split();

    // Register the requester before reading, so that a request made as soon as the
    // connection is up is delivered instead of being refused
    let (req_tx, mut req_rx) = mpsc::channel::<TunnelRequest>(4);
    state.set_requester(Some(req_tx)).await;

    // The API serializes its requests, so at most one is in flight
    let mut pending: Option<oneshot::Sender<Result<(), String>>> = None;

    let result = loop {
        tokio::select! {
            // A config pushed by the server, or the verdict on a request
            push = protocol::read_payload::<ServerConfigPush, _>(&mut rd) => {
                match push {
                    Ok(ServerConfigPush::Config(config)) => {
                        state.set_tunnels(&config).await;
                        info!(
                            "Got the config from the server. {} tunnel(s)",
                            config.tunnels.len()
                        );
                        if tx.send(config).await.is_err() {
                            break Err(anyhow!("The client is shutting down"));
                        }
                    }
                    Ok(ServerConfigPush::Applied(config)) => {
                        state.set_tunnels(&config).await;
                        if let Some(reply) = pending.take() {
                            let _ = reply.send(Ok(()));
                        }
                    }
                    Ok(ServerConfigPush::Rejected(reason)) => {
                        if let Some(reply) = pending.take() {
                            let _ = reply.send(Err(reason));
                        }
                    }
                    Err(e) => {
                        break Err(e).with_context(|| "Failed to read the config");
                    }
                }
            }
            // A change requested through the administration API
            req = req_rx.recv() => {
                if let Some(req) = req {
                    if let Err(e) = protocol::write_payload(&mut wr, &req.msg).await {
                        let _ = req
                            .reply
                            .send(Err("The connection to the server was closed".to_string()));
                        break Err(e).with_context(|| "Failed to write the config request");
                    }
                    pending = Some(req.reply);
                }
            }
            _ = shutdown_rx.recv() => break Ok(()),
        }
    };

    // Stop accepting requests and fail the one that is still in flight, so that
    // the API handler doesn't wait for the whole timeout
    state.set_requester(None).await;
    if let Some(reply) = pending.take() {
        let _ = reply.send(Err("The connection to the server was closed".to_string()));
    }

    result
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

    // Send nonce
    let v: &[u8; HASH_WIDTH_IN_BYTES] = args.session_key[..].try_into().unwrap();
    let hello = Hello::DataChannelHello(CURRENT_PROTO_VERSION, v.to_owned());
    conn.write_all(&bincode::serialize(&hello).unwrap()).await?;
    conn.flush().await?;

    Ok(conn)
}

async fn run_data_channel(args: Arc<RunDataChannelArgs>) -> Result<()> {
    // Do the handshake
    let mut conn = do_data_channel_handshake(args.clone()).await?;

    // Forward
    match read_data_cmd(&mut conn).await? {
        DataChannelCmd::StartForwardTcp => {
            run_data_channel_for_tcp(conn, &args.tunnel.local_addr).await?;
        }
    }
    Ok(())
}

// Simply copying back and forth for TCP
#[instrument(skip(conn))]
async fn run_data_channel_for_tcp(mut conn: TcpStream, local_addr: &str) -> Result<()> {
    debug!("New data channel starts forwarding");

    let mut local = TcpStream::connect(local_addr)
        .await
        .with_context(|| format!("Failed to connect to {}", local_addr))?;
    let _ = copy_bidirectional_with_sizes(
        &mut conn,
        &mut local,
        COPY_BUF_SIZE,
        COPY_BUF_SIZE,
    )
    .await;
    Ok(())
}

// Control channel
struct ControlChannel {
    digest: TunnelDigest,               // SHA256 of the tunnel name
    tunnel: ClientTunnelConfig,         // Pushed by the server
    token: MaskedString,                // The token given by `--token`
    shutdown_rx: oneshot::Receiver<u8>, // Receives the shutdown signal
    remote_addr: String,                // `--remote`
    heartbeat_timeout: u64,             // Application layer heartbeat timeout in secs
}

type TunnelDigest = protocol::Digest;
type Nonce = protocol::Digest;

// Handle of a control channel
// Dropping it will also drop the actual control channel
struct ControlChannelHandle {
    shutdown_tx: oneshot::Sender<u8>,
    // Keep the config that the channel was created with, so that it can be compared
    // with the config pushed later
    tunnel: ClientTunnelConfig,
    heartbeat_timeout: u64,
}

impl ControlChannelHandle {
    // Whether the channel is already serving the given config
    fn matches(&self, tunnel: &ClientTunnelConfig, heartbeat_timeout: u64) -> bool {
        &self.tunnel == tunnel && self.heartbeat_timeout == heartbeat_timeout
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

        // Send hello
        debug!("Sending hello");
        let hello_send =
            Hello::ControlChannelHello(CURRENT_PROTO_VERSION, self.digest[..].try_into().unwrap());
        conn.write_all(&bincode::serialize(&hello_send).unwrap())
            .await?;
        conn.flush().await?;

        // Read hello
        debug!("Reading hello");
        let nonce = match read_hello(&mut conn).await? {
            ControlChannelHello(_, d) => d,
            _ => {
                bail!("Unexpected type of hello");
            }
        };

        // Send auth
        debug!("Sending auth");
        let mut concat = Vec::from(self.token.as_bytes());
        concat.extend_from_slice(&nonce);

        let session_key = protocol::digest(&concat);
        let auth = Auth(session_key);
        conn.write_all(&bincode::serialize(&auth).unwrap()).await?;
        conn.flush().await?;

        // Read ack
        debug!("Reading ack");
        match read_ack(&mut conn).await? {
            Ack::Ok => {}
            v => {
                return Err(anyhow!("{}", v))
                    .with_context(|| format!("Authentication failed: {}", self.tunnel.name));
            }
        }

        // Channel ready
        info!("Control channel established");

        // Socket options for the data channel
        let socket_opts = SocketOpts::from_client_cfg(&self.tunnel);
        let data_ch_args = Arc::new(RunDataChannelArgs {
            session_key,
            remote_addr,
            socket_opts,
            tunnel: self.tunnel.clone(),
        });

        loop {
            tokio::select! {
                val = read_control_cmd(&mut conn) => {
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
    #[instrument(name="handle", skip_all, fields(tunnel = %tunnel.name))]
    fn new(
        tunnel: ClientTunnelConfig,
        token: MaskedString,
        remote_addr: String,
        heartbeat_timeout: u64,
    ) -> ControlChannelHandle {
        let digest = protocol::digest(tunnel.name.as_bytes());

        info!("Starting {}", hex::encode(digest));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let mut retry_backoff = run_control_chan_backoff(tunnel.retry_interval);

        let handle_tunnel = tunnel.clone();
        let mut s = ControlChannel {
            digest,
            tunnel,
            token,
            shutdown_rx,
            remote_addr,
            heartbeat_timeout,
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
