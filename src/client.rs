use crate::cli::ClientArgs;
use crate::config::{ClientConfig, ClientServiceConfig, MaskedString};
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
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{copy_bidirectional, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, oneshot};
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
    service_handles: HashMap<String, ControlChannelHandle>,
}

impl Client {
    fn new(args: ClientArgs) -> Client {
        Client {
            args,
            service_handles: HashMap::new(),
        }
    }

    // The entrypoint of Client
    async fn run(&mut self, mut shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
        // The client has no config of its own. It's pushed by the server
        let config = match fetch_config(&self.args, &shutdown_rx).await {
            Ok(config) => config,
            Err(e) => {
                if is_shutdown(&mut shutdown_rx) {
                    return Ok(());
                }
                return Err(e);
            }
        };

        let token = MaskedString::from(self.args.token.as_str());

        for service in &config.services {
            // Create a control channel for each service pushed by the server
            let handle = ControlChannelHandle::new(
                (*service).clone(),
                token.clone(),
                self.args.remote.clone(),
                config.heartbeat_timeout,
            );
            self.service_handles.insert(service.name.clone(), handle);
        }

        // Wait for the shutdown signal
        match shutdown_rx.recv().await {
            Ok(_) => {}
            Err(err) => {
                error!("Unable to listen for shutdown signal: {}", err);
            }
        }

        // Shutdown all services
        for (_, handle) in self.service_handles.drain() {
            handle.shutdown();
        }

        Ok(())
    }
}

// Check whether the shutdown signal has arrived
fn is_shutdown(shutdown_rx: &mut broadcast::Receiver<bool>) -> bool {
    shutdown_rx.try_recv() != Err(broadcast::error::TryRecvError::Empty)
}

// Fetch the config from the server, retrying until it succeeds or the client shuts down
async fn fetch_config(
    args: &ClientArgs,
    shutdown_rx: &broadcast::Receiver<bool>,
) -> Result<ClientConfig> {
    // Subscribe a new receiver, so that the shutdown signal isn't consumed here
    let mut shutdown_rx = shutdown_rx.resubscribe();

    // Retry at least every 100ms
    let backoff = ExponentialBackoff {
        max_interval: Duration::from_secs(DEFAULT_FETCH_RETRY_INTERVAL_SECS),
        max_elapsed_time: None,
        ..Default::default()
    };

    tokio::select! {
        v = retry_notify(
            backoff,
            || async {
                try_fetch_config(args)
                    .await
                    .map_err(backoff::Error::transient)
            },
            |e, duration| {
                error!("{:#}. Retry in {:?}", e, duration);
            },
        ) => v,
        _ = shutdown_rx.recv() => Err(anyhow!("shutdown")),
    }
}

async fn try_fetch_config(args: &ClientArgs) -> Result<ClientConfig> {
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

    // Read the config pushed by the server
    let config: ClientConfig = protocol::read_payload(&mut conn)
        .await
        .with_context(|| "Failed to read the config")?;
    info!(
        "Got the config from the server. {} service(s)",
        config.services.len()
    );

    Ok(config)
}

struct RunDataChannelArgs {
    session_key: Nonce,
    remote_addr: AddrMaybeCached,
    socket_opts: SocketOpts,
    service: ClientServiceConfig,
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
                .with_context(|| format!("Failed to connect to {}", &args.remote_addr))
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
            run_data_channel_for_tcp(conn, &args.service.local_addr).await?;
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
    digest: ServiceDigest,              // SHA256 of the service name
    service: ClientServiceConfig,       // Pushed by the server
    token: MaskedString,                // The token given by `--token`
    shutdown_rx: oneshot::Receiver<u8>, // Receives the shutdown signal
    remote_addr: String,                // `--remote`
    heartbeat_timeout: u64,             // Application layer heartbeat timeout in secs
}

type ServiceDigest = protocol::Digest;
type Nonce = protocol::Digest;

// Handle of a control channel
// Dropping it will also drop the actual control channel
struct ControlChannelHandle {
    shutdown_tx: oneshot::Sender<u8>,
}

impl ControlChannel {
    #[instrument(skip_all)]
    async fn run(&mut self) -> Result<()> {
        let mut remote_addr = AddrMaybeCached::new(&self.remote_addr);
        remote_addr.resolve().await?;

        let mut conn = connect(&remote_addr)
            .await
            .with_context(|| format!("Failed to connect to {}", &self.remote_addr))?;
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
                    .with_context(|| format!("Authentication failed: {}", self.service.name));
            }
        }

        // Channel ready
        info!("Control channel established");

        // Socket options for the data channel
        let socket_opts = SocketOpts::from_client_cfg(&self.service);
        let data_ch_args = Arc::new(RunDataChannelArgs {
            session_key,
            remote_addr,
            socket_opts,
            service: self.service.clone(),
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
    #[instrument(name="handle", skip_all, fields(service = %service.name))]
    fn new(
        service: ClientServiceConfig,
        token: MaskedString,
        remote_addr: String,
        heartbeat_timeout: u64,
    ) -> ControlChannelHandle {
        let digest = protocol::digest(service.name.as_bytes());

        info!("Starting {}", hex::encode(digest));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let mut retry_backoff = run_control_chan_backoff(service.retry_interval);

        let mut s = ControlChannel {
            digest,
            service,
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

        ControlChannelHandle { shutdown_tx }
    }

    fn shutdown(self) {
        // A send failure shows that the actor has already shutdown.
        let _ = self.shutdown_tx.send(0u8);
    }
}
