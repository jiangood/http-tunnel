use anyhow::{Ok, Result};
use common::run_http_tunnel_client;
use rand::Rng;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::broadcast,
    time,
};
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;

use crate::common::run_http_tunnel_server;

mod common;

const ECHO_SERVER_ADDR: &str = "127.0.0.1:8080";
const HTTP_ENTRY_ADDR: &str = "127.0.0.1:2334";
const SERVER_CONFIG: &str = "tests/for_http/server.toml";
const SERVER_ADDR: &str = "127.0.0.1:2333";
const CLIENT_NAME: &str = "home";
const CLIENT_TOKEN: &str = "a_secret_token";
const ECHO_HOST: &str = "echo.test";
const HITTER_NUM: usize = 4;

fn init() {
    let level = "info";
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::from(level)),
        )
        .try_init();
}

#[tokio::test]
async fn http_routing() -> Result<()> {
    init();

    // Spawn a echo server as the local HTTP service behind the NAT
    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(ECHO_SERVER_ADDR).await {
            panic!("Failed to run the echo server for testing: {:?}", e);
        }
    });

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    // Start the client
    info!("start the client");
    let client = tokio::spawn(async move {
        run_http_tunnel_client(CLIENT_NAME, SERVER_ADDR, CLIENT_TOKEN, client_shutdown_rx)
            .await
            .unwrap();
    });

    // Sleep for 1 second. Expect the client keep retrying to reach the server
    time::sleep(Duration::from_secs(1)).await;

    // Start the server
    info!("start the server");
    let server = tokio::spawn(async move {
        run_http_tunnel_server(SERVER_CONFIG, server_shutdown_rx)
            .await
            .unwrap();
    });
    time::sleep(Duration::from_millis(2500)).await; // Wait for the client to retry

    info!("route by Host");
    http_echo_hitter(HTTP_ENTRY_ADDR, ECHO_HOST).await.unwrap();

    info!("unknown Host returns 404");
    http_404_check(HTTP_ENTRY_ADDR, "unknown.test")
        .await
        .unwrap();

    // Simulate the client crash and restart
    info!("shutdown the client");
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(client);

    info!("restart the client");
    let client_shutdown_rx = client_shutdown_tx.subscribe();
    let client = tokio::spawn(async move {
        run_http_tunnel_client(CLIENT_NAME, SERVER_ADDR, CLIENT_TOKEN, client_shutdown_rx)
            .await
            .unwrap();
    });
    time::sleep(Duration::from_secs(1)).await; // Wait for the client to start

    info!("route by Host");
    http_echo_hitter(HTTP_ENTRY_ADDR, ECHO_HOST).await.unwrap();

    // Simulate the server crash and restart
    info!("shutdown the server");
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(server);

    info!("restart the server");
    let server_shutdown_rx = server_shutdown_tx.subscribe();
    let server = tokio::spawn(async move {
        run_http_tunnel_server(SERVER_CONFIG, server_shutdown_rx)
            .await
            .unwrap();
    });
    time::sleep(Duration::from_millis(2500)).await; // Wait for the client to retry

    // Simulate heavy load
    info!("lots of requests");

    let mut v = Vec::new();

    for _ in 0..HITTER_NUM {
        v.push(tokio::spawn(async move {
            http_echo_hitter(HTTP_ENTRY_ADDR, ECHO_HOST).await.unwrap();
        }));
    }

    for h in v {
        assert!(tokio::join!(h).0.is_ok());
    }

    // Shutdown
    info!("shutdown the server and the client");
    server_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;

    let _ = tokio::join!(server, client);

    Ok(())
}

/// Send a request with the given `Host` and expect the whole request to be echoed back
/// (the local service is a raw echo server, so it echoes the header together with the body)
async fn http_echo_hitter(addr: &'static str, host: &'static str) -> Result<()> {
    let mut conn = TcpStream::connect(addr).await?;

    let mut body = [0u8; 1024];
    rand::thread_rng().fill(&mut body);

    let head = format!(
        "POST /echo HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        host,
        body.len()
    );

    let mut request = head.into_bytes();
    request.extend_from_slice(&body);

    conn.write_all(&request).await?;

    let mut response = vec![0u8; request.len()];
    conn.read_exact(&mut response).await?;

    assert_eq!(response, request);

    Ok(())
}

async fn http_404_check(addr: &'static str, host: &'static str) -> Result<()> {
    let mut conn = TcpStream::connect(addr).await?;

    let request = format!("GET / HTTP/1.1\r\nHost: {}\r\n\r\n", host);
    conn.write_all(request.as_bytes()).await?;

    let mut response = Vec::new();
    conn.read_to_end(&mut response).await?;

    let text = String::from_utf8_lossy(&response);
    debug!("{}", text);
    assert!(
        text.starts_with("HTTP/1.1 404"),
        "unexpected response: {}",
        text
    );

    Ok(())
}
