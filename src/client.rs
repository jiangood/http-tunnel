use crate::cli::ClientArgs;
use crate::config::{ClientConfig, ClientTunnelConfig, MaskedString};
use crate::constants::run_control_chan_backoff;
use crate::protocol::Hello::{self, *};
use crate::protocol::{
    self, read_ack, read_control_cmd, read_data_cmd, read_hello, Ack, Auth, ControlChannelCmd,
    DataChannelCmd, CURRENT_PROTO_VERSION, HASH_WIDTH_IN_BYTES,
};
use crate::transport::{connect, AddrMaybeCached, SocketOpts};
use anyhow::{anyhow, bail, Context, Result};
use backoff::backoff::Backoff;
use backoff::future::retry_notify;
use backoff::ExponentialBackoff;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::io::{copy_bidirectional, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::{self, Duration, Instant};
use tracing::{debug, error, info, instrument, warn, Instrument, Span};

// The interval between retries to fetch the config, before the client gets one
const DEFAULT_FETCH_RETRY_INTERVAL_SECS: u64 = 1;

// The entrypoint of running a client
pub async fn run_client(args: ClientArgs, shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
    let mut client = Client::new(args);
    client.run(shutdown_rx).await
}

// Holds the state of a client
struct Client {
    args: ClientArgs,
    tunnel_handles: HashMap<String, ControlChannelHandle>,
}

impl Client {
    fn new(args: ClientArgs) -> Client {
        Client {
            args,
            tunnel_handles: HashMap::new(),
        }
    }

    // The entrypoint of Client
    async fn run(&mut self, mut shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
        // The client has no config of its own. It's pushed by the server, and the
        // server keeps the config channel open to push the updates made through the
        // administration API.
        let (config_tx, mut config_rx) = mpsc::channel::<ClientConfig>(4);

        {
            let args = self.args.clone();
            let shutdown_rx = shutdown_rx.resubscribe();
            tokio::spawn(async move {
                run_config_session(args, config_tx, shutdown_rx).await;
            });
        }

        loop {
            tokio::select! {
                maybe = config_rx.recv() => {
                    match maybe {
                        Some(config) => self.reconcile(config),
                        // The config session gave up, which shouldn't happen
                        None => break,
                    }
                }
                _ = shutdown_rx.recv() => break,
            }
        }

        // Shutdown all tunnels
        for (_, handle) in self.tunnel_handles.drain() {
            handle.shutdown();
        }

        Ok(())
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

// Keep a config channel open, feeding the pushed configs to the reconciliation loop.
// It reconnects with a backoff when the channel drops.
async fn run_config_session(
    args: ClientArgs,
    tx: mpsc::Sender<ClientConfig>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) {
    let mut backoff = ExponentialBackoff {
        max_interval: Duration::from_secs(DEFAULT_FETCH_RETRY_INTERVAL_SECS),
        max_elapsed_time: None,
        ..Default::default()
    };

    loop {
        if let Err(e) = run_config_connection(&args, &tx, &shutdown_rx).await {
            if is_shutdown(&mut shutdown_rx) {
                return;
            }
            error!("{:#}", e);
        }

        if is_shutdown(&mut shutdown_rx) {
            return;
        }

        let duration = backoff
            .next_backoff()
            .unwrap_or(Duration::from_secs(DEFAULT_FETCH_RETRY_INTERVAL_SECS));
        tokio::select! {
            _ = time::sleep(duration) => {}
            _ = shutdown_rx.recv() => return,
        }
    }
}

// Fetch the config and then keep reading the config updates pushed by the server
async fn run_config_connection(
    args: &ClientArgs,
    tx: &mpsc::Sender<ClientConfig>,
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
        v => {
            return Err(anyhow!("{}", v))
                .with_context(|| format!("Failed to get the config of the client {}", args.name));
        }
    }

    info!("Config channel established");

    // Read the config pushed by the server, including the later updates
    loop {
        tokio::select! {
            config = protocol::read_payload::<ClientConfig, _>(&mut conn) => {
                let config = config.with_context(|| "Failed to read the config")?;
                info!(
                    "Got the config from the server. {} tunnel(s)",
                    config.tunnels.len()
                );
                if tx.send(config).await.is_err() {
                    bail!("The client is shutting down");
                }
            }
            _ = shutdown_rx.recv() => return Ok(()),
        }
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
    let _ = copy_bidirectional(&mut conn, &mut local).await;
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
