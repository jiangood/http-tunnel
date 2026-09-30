use crate::config::{ClientConfig, MaskedString, ServerConfig, ServerServiceConfig};
use crate::constants::listen_backoff;
use crate::helper::{retry_notify_with_deadline, write_and_flush};
use crate::multi_map::MultiMap;
use crate::protocol::Hello::{ConfigChannelHello, ControlChannelHello, DataChannelHello};
use crate::protocol::{
    self, read_auth, read_hello, Ack, ControlChannelCmd, DataChannelCmd, Hello, HASH_WIDTH_IN_BYTES,
};
use crate::transport::SocketOpts;
use anyhow::{bail, Context, Result};
use backoff::backoff::Backoff;
use backoff::ExponentialBackoff;

use rand::RngCore;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{copy_bidirectional, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio::time;
use tracing::{debug, error, info, info_span, instrument, warn, Instrument, Span};

type ServiceDigest = protocol::Digest; // SHA256 of a service name
type ClientDigest = protocol::Digest; // SHA256 of a client name
type Nonce = protocol::Digest; // Also called `session_key`

const TCP_POOL_SIZE: usize = 8; // The number of cached connections for TCP services
const CHAN_SIZE: usize = 2048; // The capacity of various chans
const HANDSHAKE_TIMEOUT: u64 = 5; // Timeout for transport handshake

// A visitor accepted by the HTTP entrypoint. `prefetched` holds the bytes that were
// read while sniffing the `Host` header and must be replayed to the service.
pub(crate) struct HttpVisitor {
    pub stream: TcpStream,
    pub prefetched: Vec<u8>,
}

// A client of `[clients]`, together with the config that is pushed to it
#[derive(Clone)]
struct ServerClient {
    name: String,
    token: MaskedString,
    config: ClientConfig,
}

// A service, together with the token of the client that serves it
#[derive(Clone)]
struct ServiceRuntime {
    config: ServerServiceConfig,
    token: MaskedString,
}

// A hash map of ControlChannelHandles, indexed by ServiceDigest or Nonce
// See also MultiMap
pub(crate) type ControlChannelMap = MultiMap<ServiceDigest, Nonce, ControlChannelHandle>;

// The entrypoint of running a server
pub async fn run_server(
    config: ServerConfig,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let mut server = Server::from(config).await?;
    server.run(shutdown_rx).await?;

    Ok(())
}

// Server holds all states of running a server
struct Server {
    // The whole config
    config: Arc<ServerConfig>,

    // `[clients.<client>.services]`, indexed by ServiceDigest
    services: Arc<RwLock<HashMap<ServiceDigest, ServiceRuntime>>>,
    // `[clients]`, indexed by the digest of the client name
    clients: Arc<RwLock<HashMap<ClientDigest, ServerClient>>>,
    // Collection of control channels
    control_channels: Arc<RwLock<ControlChannelMap>>,
    // `Host` -> ServiceDigest
    routing_table: Arc<RwLock<crate::http::RoutingTable>>,
}

// Generate the services of all the clients, indexed by ServiceDigest
fn generate_service_hashmap(
    server_config: &ServerConfig,
) -> HashMap<ServiceDigest, ServiceRuntime> {
    let mut ret = HashMap::new();
    for client in server_config.clients.values() {
        for (name, s) in &client.services {
            ret.insert(
                protocol::digest(name.as_bytes()),
                ServiceRuntime {
                    config: s.clone(),
                    token: client.token.clone(),
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

// Generate a routing table which maps a `Host` to a ServiceDigest
fn generate_routing_table(server_config: &ServerConfig) -> crate::http::RoutingTable {
    let mut ret = HashMap::new();
    for client in server_config.clients.values() {
        for (name, s) in &client.services {
            let digest = protocol::digest(name.as_bytes());
            for host in &s.hosts {
                ret.insert(host.clone(), digest);
            }
        }
    }
    ret
}

impl Server {
    // Create a server from the config
    pub async fn from(config: ServerConfig) -> Result<Server> {
        let config = Arc::new(config);
        let services = Arc::new(RwLock::new(generate_service_hashmap(&config)));
        let clients = Arc::new(RwLock::new(generate_client_hashmap(&config)));
        let routing_table = Arc::new(RwLock::new(generate_routing_table(&config)));
        let control_channels = Arc::new(RwLock::new(ControlChannelMap::new()));
        Ok(Server {
            config,
            services,
            clients,
            control_channels,
            routing_table,
        })
    }

    // The entry point of Server
    pub async fn run(&mut self, mut shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
        // Listen at `bind_addr` for the control and data channels of clients
        let l: TcpListener = retry_notify_with_deadline(
            listen_backoff(),
            || async { Ok(TcpListener::bind(&self.config.bind_addr).await?) },
            |e, duration| {
                error!("{:#}. Retry in {:?}", e, duration);
            },
            &mut shutdown_rx,
        )
        .await
        .with_context(|| "Failed to listen at `bind_addr`")?;

        info!("Listening at {}", self.config.bind_addr);

        // Run the HTTP entrypoint which routes visitors by the `Host` header
        let http_task = tokio::spawn(crate::http::serve(
            self.config.http_bind_addr.clone(),
            self.routing_table.clone(),
            self.control_channels.clone(),
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
                                            let services = self.services.clone();
                                            let clients = self.clients.clone();
                                            let control_channels = self.control_channels.clone();
                                            let server_config = self.config.clone();
                                            tokio::spawn(async move {
                                                if let Err(err) = handle_connection(conn, services, clients, control_channels, server_config).await {
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

        info!("Shutdown");

        Ok(())
    }
}

// Plain TCP accepts the connection as is
async fn handshake(conn: TcpStream) -> Result<TcpStream> {
    Ok(conn)
}

// Handle connections to `bind_addr`
async fn handle_connection(
    mut conn: TcpStream,
    services: Arc<RwLock<HashMap<ServiceDigest, ServiceRuntime>>>,
    clients: Arc<RwLock<HashMap<ClientDigest, ServerClient>>>,
    control_channels: Arc<RwLock<ControlChannelMap>>,
    server_config: Arc<ServerConfig>,
) -> Result<()> {
    // Read hello
    let hello = read_hello(&mut conn).await?;
    match hello {
        ControlChannelHello(_, service_digest) => {
            do_control_channel_handshake(
                conn,
                services,
                control_channels,
                service_digest,
                server_config,
            )
            .await?;
        }
        ConfigChannelHello(_, client_digest) => {
            do_config_channel_handshake(conn, clients, client_digest).await?;
        }
        DataChannelHello(_, nonce) => {
            do_data_channel_handshake(conn, control_channels, nonce).await?;
        }
    }
    Ok(())
}

// Push the config of a client to it. The client doesn't have any config of its own
async fn do_config_channel_handshake(
    mut conn: TcpStream,
    clients: Arc<RwLock<HashMap<ClientDigest, ServerClient>>>,
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

    // Lookup the client
    let client = clients.read().await.get(&client_digest).cloned();
    let Some(client) = client else {
        conn.write_all(&bincode::serialize(&Ack::ServiceNotExist).unwrap())
            .await?;
        bail!("No such a client {}", hex::encode(client_digest));
    };

    // Calculate the checksum
    let mut concat = Vec::from(client.token.as_bytes());
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
        bail!("Client {} failed the authentication", client.name);
    }

    conn.write_all(&bincode::serialize(&Ack::Ok).unwrap())
        .await?;
    conn.flush().await?;

    // Push the config
    protocol::write_payload(&mut conn, &client.config).await?;

    info!(client = %client.name, services = client.config.services.len(), "Config pushed");

    Ok(())
}

async fn do_control_channel_handshake(
    mut conn: TcpStream,
    services: Arc<RwLock<HashMap<ServiceDigest, ServiceRuntime>>>,
    control_channels: Arc<RwLock<ControlChannelMap>>,
    service_digest: ServiceDigest,
    server_config: Arc<ServerConfig>,
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

    // Lookup the service
    let service = match services.read().await.get(&service_digest) {
        Some(v) => v,
        None => {
            conn.write_all(&bincode::serialize(&Ack::ServiceNotExist).unwrap())
                .await?;
            bail!("No such a service {}", hex::encode(service_digest));
        }
    }
    .to_owned();

    let service_config = service.config;
    let service_name = &service_config.name;

    // Calculate the checksum with the token of the client that serves the service
    let mut concat = Vec::from(service.token.as_bytes());
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
        bail!("Service {} failed the authentication", service_name);
    } else {
        let mut h = control_channels.write().await;

        // If there's already a control channel for the service, then drop the old one.
        // Because a control channel doesn't report back when it's dead,
        // the handle in the map could be stall, dropping the old handle enables
        // the client to reconnect.
        if h.remove1(&service_digest).is_some() {
            warn!(
                "Dropping previous control channel for service {}",
                service_name
            );
        }

        // Send ack
        conn.write_all(&bincode::serialize(&Ack::Ok).unwrap())
            .await?;
        conn.flush().await?;

        info!(service = %service_config.name, "Control channel established");
        let handle =
            ControlChannelHandle::new(conn, service_config, server_config.heartbeat_interval);

        // Insert the new handle
        let _ = h.insert(service_digest, session_key, handle);
    }

    Ok(())
}

async fn do_data_channel_handshake(
    conn: TcpStream,
    control_channels: Arc<RwLock<ControlChannelMap>>,
    nonce: Nonce,
) -> Result<()> {
    debug!("Try to handshake a data channel");

    // Validate
    let control_channels_guard = control_channels.read().await;
    match control_channels_guard.get2(&nonce) {
        Some(handle) => {
            SocketOpts::from_server_cfg(&handle.service).apply(&conn);

            // Send the data channel to the corresponding control channel
            handle
                .data_ch_tx
                .send(conn)
                .await
                .with_context(|| "Data channel for a stale control channel")?;
        }
        None => {
            warn!("Data channel has incorrect nonce");
        }
    }
    Ok(())
}

pub(crate) struct ControlChannelHandle {
    // Shutdown the control channel by dropping it
    _shutdown_tx: broadcast::Sender<bool>,
    data_ch_tx: mpsc::Sender<TcpStream>,
    pub(crate) visitor_tx: mpsc::Sender<HttpVisitor>,
    service: ServerServiceConfig,
}

impl ControlChannelHandle {
    // Create a control channel handle, where the control channel handling task
    // and the connection pool task are created.
    #[instrument(name = "handle", skip_all, fields(service = %service.name))]
    fn new(
        conn: TcpStream,
        service: ServerServiceConfig,
        heartbeat_interval: u64,
    ) -> ControlChannelHandle {
        // Create a shutdown channel
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);

        // Store data channels
        let (data_ch_tx, data_ch_rx) = mpsc::channel(CHAN_SIZE * 2);

        // Store data channel creation requests
        let (data_ch_req_tx, data_ch_req_rx) = mpsc::unbounded_channel();

        // Visitors handed over by the HTTP entrypoint
        let (visitor_tx, visitor_rx) = mpsc::channel(CHAN_SIZE);

        // Cache some data channels for later use
        for _i in 0..TCP_POOL_SIZE {
            if let Err(e) = data_ch_req_tx.send(true) {
                error!("Failed to request data channel {}", e);
            };
        }

        tokio::spawn(
            async move {
                if let Err(e) = run_tcp_connection_pool(data_ch_rx, visitor_rx, data_ch_req_tx)
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

        // Run the control channel
        tokio::spawn(
            async move {
                if let Err(err) = ch.run().await {
                    error!("{:#}", err);
                }
            }
            .instrument(Span::current()),
        );

        ControlChannelHandle {
            _shutdown_tx: shutdown_tx,
            data_ch_tx,
            visitor_tx,
            service,
        }
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
    async fn write_and_flush(&mut self, data: &[u8]) -> Result<()> {
        write_and_flush(&mut self.conn, data)
            .await
            .with_context(|| "Failed to write control cmds")?;
        Ok(())
    }
    // Run a control channel
    #[instrument(skip_all)]
    async fn run(mut self) -> Result<()> {
        let create_ch_cmd = bincode::serialize(&ControlChannelCmd::CreateDataChannel).unwrap();
        let heartbeat = bincode::serialize(&ControlChannelCmd::HeartBeat).unwrap();

        // Wait for data channel requests and the shutdown signal
        loop {
            tokio::select! {
                val = self.data_ch_req_rx.recv() => {
                    match val {
                        Some(_) => {
                            if let Err(e) = self.write_and_flush(&create_ch_cmd).await {
                                error!("{:#}", e);
                                break;
                            }
                        }
                        None => {
                            break;
                        }
                    }
                },
                _ = time::sleep(Duration::from_secs(self.heartbeat_interval)), if self.heartbeat_interval != 0 => {
                            if let Err(e) = self.write_and_flush(&heartbeat).await {
                                error!("{:#}", e);
                                break;
                            }
                }
                // Wait for the shutdown signal
                _ = self.shutdown_rx.recv() => {
                    break;
                }
            }
        }

        info!("Control channel shutdown");

        Ok(())
    }
}

#[instrument(skip_all)]
async fn run_tcp_connection_pool(
    mut data_ch_rx: mpsc::Receiver<TcpStream>,
    mut visitor_rx: mpsc::Receiver<HttpVisitor>,
    data_ch_req_tx: mpsc::UnboundedSender<bool>,
) -> Result<()> {
    let cmd = bincode::serialize(&DataChannelCmd::StartForwardTcp).unwrap();

    'pool: while let Some(visitor) = visitor_rx.recv().await {
        let HttpVisitor {
            mut stream,
            prefetched,
        } = visitor;

        loop {
            if let Some(mut ch) = data_ch_rx.recv().await {
                let ok = write_and_flush(&mut ch, &cmd).await.is_ok()
                    && write_and_flush(&mut ch, &prefetched).await.is_ok();

                if ok {
                    tokio::spawn(async move {
                        let _ = copy_bidirectional(&mut ch, &mut stream).await;
                    });
                    break;
                } else {
                    // Current data channel is broken. Request for a new one
                    if data_ch_req_tx.send(true).is_err() {
                        break 'pool;
                    }
                }
            } else {
                break 'pool;
            }
        }
    }

    info!("Shutdown");
    Ok(())
}
