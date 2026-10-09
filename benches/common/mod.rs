//! A minimal harness for the benchmarks: it starts an echo backend behind the
//! NAT, a server, and a client, and exposes a helper to time a batch of
//! requests.
//!
//! The ports are fixed so that the three benchmarks can run in sequence without
//! colliding. The integration tests use the same ports, so `cargo bench` and
//! `cargo test` must not run at the same time.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

pub const SERVER_PORT: u16 = 2333;
pub const HTTP_ENTRY_PORT: u16 = 2334;
pub const SERVER_ADDR: &str = "127.0.0.1:2333";
pub const HTTP_ENTRY_ADDR: &str = "127.0.0.1:2334";
pub const ECHO_SERVER_ADDR: &str = "127.0.0.1:8080";
pub const CLIENT_NAME: &str = "bench";
pub const CLIENT_TOKEN: &str = "bench_token";

const ECHO_HOST: &str = "echo.test";
const STARTUP: Duration = Duration::from_millis(2500);

/// A running `http-tunnel` setup, kept alive until the harness is dropped.
pub struct Harness {
    pub client_shutdown: broadcast::Sender<bool>,
    pub server_shutdown: broadcast::Sender<bool>,
    client: JoinHandle<()>,
    server: JoinHandle<()>,
    echo: JoinHandle<()>,
    config_guard: TempFile,
    stats: Arc<Stats>,
}

/// Counters kept by the echo backend, so a benchmark can tell how many requests
/// actually reached it.
#[derive(Default)]
struct Stats {
    requests: AtomicU64,
    bytes: AtomicU64,
}

struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl Harness {
    /// Start the echo backend, the client and the server, and wait until the
    /// tunnel is up.
    pub async fn start() -> Result<Harness> {
        let stats = Arc::new(Stats::default());

        let echo = {
            let stats = stats.clone();
            tokio::spawn(async move {
                if let Err(e) = echo_server(ECHO_SERVER_ADDR, stats).await {
                    eprintln!("bench echo server failed: {e:#}");
                }
            })
        };

        // Write a server config on the fly, so the benchmark doesn't depend on
        // the test fixtures.
        let config_path = std::env::temp_dir().join(format!(
            "http-tunnel-bench-{}.toml",
            std::process::id()
        ));
        let config = format!(
            "server_port = {SERVER_PORT}\nhttp_port = {HTTP_ENTRY_PORT}\n\n[clients.{CLIENT_NAME}]\ntoken = \"{CLIENT_TOKEN}\"\n\n[clients.{CLIENT_NAME}.tunnels]\n\"{ECHO_HOST}\" = \"{ECHO_SERVER_ADDR}\"\n"
        );
        std::fs::write(&config_path, config)?;
        let config_guard = TempFile(config_path.clone());

        let (client_shutdown, client_rx) = broadcast::channel(1);
        let (server_shutdown, server_rx) = broadcast::channel(1);

        let client = tokio::spawn(async move {
            let cmd = http_tunnel::Command::Client(http_tunnel::ClientArgs {
                name: CLIENT_NAME.to_string(),
                remote: SERVER_ADDR.to_string(),
                token: CLIENT_TOKEN.to_string(),
                api_port: None,
            });
            let _ = http_tunnel::run(cmd, client_rx).await;
        });
        // Let the client retry at least once while the server comes up
        tokio::time::sleep(Duration::from_millis(500)).await;

        let server = {
            let config_path = config_path.clone();
            tokio::spawn(async move {
                let cmd = http_tunnel::Command::Server(http_tunnel::ServerArgs { config_path });
                let _ = http_tunnel::run(cmd, server_rx).await;
            })
        };

        tokio::time::sleep(STARTUP).await;

        Ok(Harness {
            client_shutdown,
            server_shutdown,
            client,
            server,
            echo,
            config_guard,
            stats,
        })
    }

    pub fn requests_reached_backend(&self) -> u64 {
        self.stats.requests.load(Ordering::Relaxed)
    }

    /// Stop the server and the client and wait for them
    pub async fn shutdown(self) {
        let _ = self.server_shutdown.send(true);
        let _ = self.client_shutdown.send(true);
        let _ = self.server.await;
        let _ = self.client.await;
        self.echo.abort();
        let _ = self.config_guard;
    }
}

fn request(host: &str, body_len: usize, keep_alive: bool) -> Vec<u8> {
    let connection = if keep_alive { "keep-alive" } else { "close" };
    let mut req = format!(
        "POST /bench HTTP/1.1\r\nHost: {host}\r\nContent-Length: {body_len}\r\nConnection: {connection}\r\n\r\n"
    )
    .into_bytes();
    req.extend(std::iter::repeat_n(b'x', body_len));
    req
}

/// Read exactly `n` bytes, which is what the raw echo backend sends back.
async fn read_exact(conn: &mut TcpStream, n: usize) -> Result<()> {
    let mut remaining = n;
    let mut buf = [0u8; 16 * 1024];
    while remaining > 0 {
        let want = remaining.min(buf.len());
        let read = conn.read(&mut buf[..want]).await?;
        if read == 0 {
            anyhow::bail!("the connection closed before the response was complete");
        }
        remaining -= read;
    }
    Ok(())
}

/// Open a visitor connection and send `count` keep-alive requests of `body_len`
/// bytes each, reading every response. The local backend is a raw echo server,
/// so the response is exactly the request bytes.
pub async fn keep_alive_batch(addr: &str, count: usize, body_len: usize) -> Result<()> {
    let mut conn = TcpStream::connect(addr).await?;
    let req = request(ECHO_HOST, body_len, true);
    for _ in 0..count {
        conn.write_all(&req).await?;
        read_exact(&mut conn, req.len()).await?;
    }
    Ok(())
}

/// Open a fresh visitor connection for a single request, then close it.
pub async fn short_connection_request(addr: &str, body_len: usize) -> Result<()> {
    let mut conn = TcpStream::connect(addr).await?;
    let req = request(ECHO_HOST, body_len, false);
    conn.write_all(&req).await?;
    read_exact(&mut conn, req.len()).await?;
    Ok(())
}

/// Run `f` `iterations` times with `concurrency` workers in flight, and return
/// the elapsed time.
pub async fn run<F, Fut>(iterations: usize, concurrency: usize, f: F) -> Duration
where
    F: Fn() -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send,
{
    // TODO: replace the naive tail-drain with a semaphore once the shape of the
    // benchmark is settled.
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let start = Instant::now();
    let mut handles = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let f = f.clone();
        handles.push(tokio::spawn(async move {
            let r = f().await;
            drop(permit);
            r
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    start.elapsed()
}

async fn echo_server(addr: &str, stats: Arc<Stats>) -> Result<()> {
    let l = TcpListener::bind(addr).await?;
    loop {
        let (conn, _) = l.accept().await?;
        let stats = stats.clone();
        tokio::spawn(async move {
            let _ = echo(conn, stats).await;
        });
    }
}

async fn echo(conn: TcpStream, stats: Arc<Stats>) -> Result<()> {
    let (mut rd, mut wr) = conn.into_split();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = rd.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        stats.requests.fetch_add(1, Ordering::Relaxed);
        stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
        wr.write_all(&buf[..n]).await?;
    }
    Ok(())
}
