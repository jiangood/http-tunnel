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

// ==== The administration API test ====

const ADMIN_ECHO_SERVER_ADDR: &str = "127.0.0.1:8091";
const ADMIN_HTTP_ENTRY_ADDR: &str = "127.0.0.1:2444";
const ADMIN_API_ADDR: &str = "127.0.0.1:2445";
const ADMIN_SERVER_ADDR: &str = "127.0.0.1:2443";
const ADMIN_TOKEN: &str = "admin_secret";
const ADMIN_CLIENT_TOKEN: &str = "a_secret_token";

const ADMIN_CONFIG: &str = r#"
bind_addr = "127.0.0.1:2443"
http_bind_addr = "127.0.0.1:2444"
api_bind_addr = "127.0.0.1:2445"
api_token = "admin_secret"

[clients.home]
token = "a_secret_token"

[clients.home.services.echo]
hosts = ["echo.test"]
local_addr = "127.0.0.1:8091"
"#;

#[tokio::test]
async fn admin_api_hot_reload() -> Result<()> {
    init();

    // Write the config to a temporary file, because the API writes it back
    let config_path = std::env::temp_dir().join("http_tunnel_admin_test.toml");
    std::fs::write(&config_path, ADMIN_CONFIG)?;

    // An echo server as the local HTTP service behind the NAT
    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(ADMIN_ECHO_SERVER_ADDR).await {
            panic!("Failed to run the echo server for testing: {:?}", e);
        }
    });

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    let config_path_str = config_path.to_str().unwrap().to_string();

    let server = tokio::spawn(async move {
        run_http_tunnel_server(&config_path_str, server_shutdown_rx)
            .await
            .unwrap();
    });

    let client = tokio::spawn(async move {
        run_http_tunnel_client(
            "home",
            ADMIN_SERVER_ADDR,
            ADMIN_CLIENT_TOKEN,
            client_shutdown_rx,
        )
        .await
        .unwrap();
    });

    time::sleep(Duration::from_millis(2500)).await;

    info!("the initial service is routed");
    http_echo_hitter(ADMIN_HTTP_ENTRY_ADDR, "echo.test")
        .await
        .unwrap();

    info!("the web UI is served without a token");
    let (status, body) = api_request(ADMIN_API_ADDR, "GET", "/", None, None).await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(body.contains("http-tunnel"), "unexpected body: {}", body);

    info!("the API rejects a request without the token");
    let (status, _) = api_request(ADMIN_API_ADDR, "GET", "/api/clients", None, None).await?;
    assert_eq!(status, 401, "expected 401");

    info!("the API rejects a request with a wrong token");
    let (status, _) =
        api_request(ADMIN_API_ADDR, "GET", "/api/clients", Some("wrong"), None).await?;
    assert_eq!(status, 401, "expected 401");

    info!("the API lists the client");
    let (status, body) = api_request(
        ADMIN_API_ADDR,
        "GET",
        "/api/clients",
        Some(ADMIN_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(body.contains("home"), "unexpected body: {}", body);

    info!("add a service through the API");
    let (status, body) = api_request(
        ADMIN_API_ADDR,
        "PUT",
        "/api/clients/home/services/echo2",
        Some(ADMIN_TOKEN),
        Some(r#"{"hosts":["echo2.test"],"local_addr":"127.0.0.1:8091"}"#),
    )
    .await?;
    assert_eq!(status, 201, "unexpected response: {}", body);

    // The client picks the new service up without a restart
    time::sleep(Duration::from_millis(1500)).await;
    info!("the added service is routed without a restart");
    http_echo_hitter(ADMIN_HTTP_ENTRY_ADDR, "echo2.test")
        .await
        .unwrap();

    info!("delete a service through the API");
    let (status, body) = api_request(
        ADMIN_API_ADDR,
        "DELETE",
        "/api/clients/home/services/echo",
        Some(ADMIN_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 204, "unexpected response: {}", body);

    time::sleep(Duration::from_millis(500)).await;
    info!("the deleted service is no longer routed");
    http_404_check(ADMIN_HTTP_ENTRY_ADDR, "echo.test")
        .await
        .unwrap();

    // The change has been written back to the config file
    let written = std::fs::read_to_string(&config_path)?;
    assert!(
        written.contains("echo2"),
        "the config wasn't written back: {}",
        written
    );
    assert!(
        !written.contains("[clients.home.services.echo]"),
        "unexpected config: {}",
        written
    );

    info!("shutdown the server and the client");
    server_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(server, client);

    Ok(())
}

/// Send a minimal HTTP request and return the status code and the body
async fn api_request(
    addr: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> Result<(u16, String)> {
    let mut conn = TcpStream::connect(addr).await?;
    let body = body.unwrap_or("");
    let auth = token
        .map(|t| format!("Authorization: Bearer {}\r\n", t))
        .unwrap_or_default();
    let request = format!(
        "{} {} HTTP/1.1\r\nHost: admin\r\n{}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        method,
        path,
        auth,
        body.len(),
        body
    );
    conn.write_all(request.as_bytes()).await?;

    let mut response = Vec::new();
    conn.read_to_end(&mut response).await?;
    let text = String::from_utf8_lossy(&response);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or("")
        .to_string();
    Ok((status, body))
}
