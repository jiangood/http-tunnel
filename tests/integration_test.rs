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

use crate::common::{run_http_tunnel_server, run_http_tunnel_server_with_control_host};

mod common;
const ECHO_SERVER_ADDR: &str = "127.0.0.1:8080";
const HTTP_ENTRY_ADDR: &str = "127.0.0.1:2334";
const HTTP_ENTRY_PORT: u16 = 2334;
const WS_HTTP_ENTRY_ADDR: &str = "127.0.0.1:2336";
const WS_HTTP_ENTRY_PORT: u16 = 2336;
const WS_SERVER_PORT: u16 = 2335;
const WS_ECHO_SERVER_ADDR: &str = "127.0.0.1:8081";
const SERVER_ADDR: &str = "127.0.0.1:2333";
const SERVER_PORT: u16 = 2333;
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

/// Write a client configuration to a temporary file and return its path. The
/// identity and the tunnels both live in the client configuration, since the
/// server takes no configuration file.
fn write_client_config(
    file: &str,
    name: &str,
    remote: &str,
    token: &str,
    web_port: Option<u16>,
    tunnels: &[(&str, &str)],
) -> Result<String> {
    let mut tunnels_toml = String::new();
    for (domain, local_addr) in tunnels {
        // Tunnel names are required by the current client config schema.
        // Use the first domain label as a stable name for these integration tests.
        let name = domain.split('.').next().unwrap_or(domain);
        tunnels_toml.push_str(&format!(
            "\n[[tunnels]]\nname = \"{}\"\ndomain = \"{}\"\nlocal_addr = \"{}\"\n",
            name, domain, local_addr
        ));
    }
    let api = web_port
        .map(|p| format!("web_port = {}\n", p))
        .unwrap_or_default();
    let content = format!(
        "name = \"{}\"\nremote = \"{}\"\ntoken = \"{}\"\n{}\n{}",
        name, remote, token, api, tunnels_toml
    );

    let path = std::env::temp_dir().join(file);
    std::fs::write(&path, content)?;
    Ok(path.to_str().unwrap().to_string())
}

#[tokio::test]
async fn http_routing() -> Result<()> {
    init();

    // Spawn a echo server as the local HTTP tunnel behind the NAT
    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(ECHO_SERVER_ADDR).await {
            panic!("Failed to run the echo server for testing: {:?}", e);
        }
    });

    let client_config = write_client_config(
        "http_tunnel_routing_client.toml",
        CLIENT_NAME,
        SERVER_ADDR,
        CLIENT_TOKEN,
        None,
        &[(ECHO_HOST, ECHO_SERVER_ADDR)],
    )?;

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    // Start the client
    info!("start the client");
    let client = tokio::spawn(async move {
        run_http_tunnel_client(&client_config, client_shutdown_rx)
            .await
            .unwrap();
    });

    // Sleep for 1 second. Expect the client keep retrying to reach the server
    time::sleep(Duration::from_secs(1)).await;

    // Start the server
    info!("start the server");
    let server = tokio::spawn(async move {
        run_http_tunnel_server(SERVER_PORT, HTTP_ENTRY_PORT, CLIENT_TOKEN, server_shutdown_rx)
            .await
            .unwrap();
    });
    time::sleep(Duration::from_millis(2500)).await; // Wait for the client to retry

    info!("tunnel by Host");
    http_echo_hitter(HTTP_ENTRY_ADDR, ECHO_HOST).await.unwrap();

    info!("unknown Host returns 404");
    http_404_check(HTTP_ENTRY_ADDR, "unknown.test")
        .await
        .unwrap();

    // Simulate the client crash and restart
    info!("shutdown the client");
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(client);

    // The domain of a disconnected tunnel is no longer claimed, so a visitor gets
    // a 404 (the server has no configuration file that would keep it known)
    info!("a disconnected tunnel is answered with 404");
    wait_for_404(HTTP_ENTRY_ADDR, ECHO_HOST).await.unwrap();

    info!("restart the client");
    let client_shutdown_rx = client_shutdown_tx.subscribe();
    let client_config = std::env::temp_dir().join("http_tunnel_routing_client.toml");
    let client = tokio::spawn(async move {
        run_http_tunnel_client(client_config.to_str().unwrap(), client_shutdown_rx)
            .await
            .unwrap();
    });
    time::sleep(Duration::from_secs(1)).await; // Wait for the client to start

    info!("tunnel by Host");
    http_echo_hitter(HTTP_ENTRY_ADDR, ECHO_HOST).await.unwrap();

    // The data channel pool only caches a few connections and a visitor consumes
    // one, so more visitors than that only work if the pool is replenished.
    info!("more visitors than the data channel pool");
    for i in 0..16 {
        let r = time::timeout(
            Duration::from_secs(5),
            http_echo_hitter(HTTP_ENTRY_ADDR, ECHO_HOST),
        )
        .await;
        assert!(
            matches!(r, std::result::Result::Ok(std::result::Result::Ok(_))),
            "visitor {} hung or failed",
            i
        );
    }

    // Simulate the server crash and restart
    info!("shutdown the server");
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(server);

    info!("restart the server");
    let server_shutdown_rx = server_shutdown_tx.subscribe();
    let server = tokio::spawn(async move {
        run_http_tunnel_server(SERVER_PORT, HTTP_ENTRY_PORT, CLIENT_TOKEN, server_shutdown_rx)
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
/// (the local tunnel is a raw echo server, so it echoes the header together with the body)
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

/// Poll until the host is answered with a `404`, with a timeout. It fails if the
/// request hangs, which is what happened before the handle of a disconnected
/// control channel was removed from the map.
async fn wait_for_404(addr: &'static str, host: &'static str) -> Result<()> {
    let deadline = time::Instant::now() + Duration::from_secs(10);
    loop {
        let mut conn = TcpStream::connect(addr).await?;
        let request = format!(
            "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            host
        );
        conn.write_all(request.as_bytes()).await?;

        let mut response = Vec::new();
        if let std::result::Result::Ok(std::result::Result::Ok(_)) =
            time::timeout(Duration::from_secs(2), conn.read_to_end(&mut response)).await
        {
            let text = String::from_utf8_lossy(&response);
            if text.starts_with("HTTP/1.1 404") {
                return Ok(());
            }
        }

        if time::Instant::now() >= deadline {
            anyhow::bail!("the tunnel didn't answer `404` in time");
        }
        time::sleep(Duration::from_millis(250)).await;
    }
}

// ==== The client administration API test ====

const CLIENT_API_ECHO_SERVER_ADDR: &str = "127.0.0.1:8093";
const CLIENT_API_HTTP_ENTRY_PORT: u16 = 2464;
const CLIENT_API_HTTP_ENTRY_ADDR: &str = "127.0.0.1:2464";
const CLIENT_API_ADDR: &str = "127.0.0.1:2465";
const CLIENT_API_SERVER_ADDR: &str = "127.0.0.1:2463";
const CLIENT_API_SERVER_PORT: u16 = 2463;
const CLIENT_API_TOKEN: &str = "a_secret_token";

/// A tunnel created, listed and deleted through the client administration API is
/// validated locally, written back to the client configuration file, and routed
/// without a restart of either side.
#[cfg(feature = "client-api")]
#[tokio::test]
async fn websocket_transport_routes_http() -> Result<()> {
    init();

    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(WS_ECHO_SERVER_ADDR).await {
            panic!("Failed to run the WebSocket echo server: {:?}", e);
        }
    });

    let client_config = write_client_config(
        "http_tunnel_websocket_client.toml",
        CLIENT_NAME,
        WS_HTTP_ENTRY_ADDR,
        CLIENT_TOKEN,
        Some(0),
        &[("echo-ws.test", WS_ECHO_SERVER_ADDR)],
    )?;
    let config = std::fs::read_to_string(&client_config)?;
    let config = config.replacen(
        "\n[[tunnels]]",
        "\ntransport = \"websocket\"\n\n[[tunnels]]",
        1,
    );
    std::fs::write(&client_config, config)?;

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    let server = tokio::spawn(async move {
        common::run_http_tunnel_server_with_control_host(
            WS_SERVER_PORT,
            WS_HTTP_ENTRY_PORT,
            Some("127.0.0.1"),
            CLIENT_TOKEN,
            server_shutdown_rx,
        )
        .await
        .unwrap();
    });
    time::sleep(Duration::from_millis(250)).await;

    let client = tokio::spawn(async move {
        run_http_tunnel_client(&client_config, client_shutdown_rx)
            .await
            .unwrap();
    });
    time::sleep(Duration::from_secs(2)).await;

    http_echo_hitter(WS_HTTP_ENTRY_ADDR, "echo-ws.test").await?;

    // Restart the server while the client remains alive. The WebSocket control
    // channel should close and the existing retry loop should register the tunnel again.
    server_shutdown_tx.send(true)?;
    server.await?;
    time::sleep(Duration::from_millis(500)).await;

    let (server2_shutdown_tx, server2_shutdown_rx) = broadcast::channel(1);
    let server2 = tokio::spawn(async move {
        common::run_http_tunnel_server_with_control_host(
            WS_SERVER_PORT,
            WS_HTTP_ENTRY_PORT,
            Some("127.0.0.1"),
            CLIENT_TOKEN,
            server2_shutdown_rx,
        )
        .await
        .unwrap();
    });
    time::sleep(Duration::from_secs(3)).await;
    http_echo_hitter(WS_HTTP_ENTRY_ADDR, "echo-ws.test").await?;

    client_shutdown_tx.send(true)?;
    server2_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server2);
    Ok(())
}

// ==== WebSocket transport with an unavailable client-side target ====

const WS_FAILURE_SERVER_PORT: u16 = 2545;
const WS_FAILURE_HTTP_PORT: u16 = 2546;
const WS_FAILURE_HTTP_ADDR: &str = "127.0.0.1:2546";
const WS_FAILURE_TARGET_ADDR: &str = "127.0.0.1:2547";

/// A WebSocket tunnel whose local target refuses connections must not hang
/// visitors forever; the server eventually returns its existing 504 timeout.
#[cfg(feature = "client-api")]
#[tokio::test]
async fn websocket_transport_handles_unavailable_local_target() -> Result<()> {
    init();
    let client_config = write_client_config(
        "http_tunnel_websocket_unavailable_target.toml",
        CLIENT_NAME,
        WS_FAILURE_HTTP_ADDR,
        CLIENT_TOKEN,
        Some(0),
        &[("unavailable-ws.test", WS_FAILURE_TARGET_ADDR)],
    )?;
    let config = std::fs::read_to_string(&client_config)?;
    let config = config.replacen(
        "\n[[tunnels]]",
        "\ntransport = \"websocket\"\n\n[[tunnels]]",
        1,
    );
    std::fs::write(&client_config, config)?;

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);
    let server = tokio::spawn(async move {
        run_http_tunnel_server_with_control_host(
            WS_FAILURE_SERVER_PORT,
            WS_FAILURE_HTTP_PORT,
            Some("127.0.0.1"),
            CLIENT_TOKEN,
            server_shutdown_rx,
        )
        .await
        .unwrap();
    });
    time::sleep(Duration::from_millis(300)).await;
    let client = tokio::spawn(async move {
        run_http_tunnel_client(&client_config, client_shutdown_rx)
            .await
            .unwrap();
    });
    time::sleep(Duration::from_secs(2)).await;

    let mut visitor = TcpStream::connect(WS_FAILURE_HTTP_ADDR).await?;
    visitor
        .write_all(
            b"GET / HTTP/1.1\r\nHost: unavailable-ws.test\r\nConnection: close\r\n\r\n",
        )
        .await?;
    let mut response = Vec::new();
    time::timeout(Duration::from_secs(18), visitor.read_to_end(&mut response))
        .await??;
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 504"),
        "expected 504 for an unavailable local target, got {}",
        response
    );

    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    Ok(())
}

#[tokio::test]
async fn client_api_manages_tunnels() -> Result<()> {
    init();

    let config_path = write_client_config(
        "http_tunnel_client_api_test.toml",
        "home",
        CLIENT_API_SERVER_ADDR,
        CLIENT_API_TOKEN,
        Some(2465),
        &[],
    )?;

    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(CLIENT_API_ECHO_SERVER_ADDR).await {
            panic!("Failed to run the echo server for testing: {:?}", e);
        }
    });

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    let server = tokio::spawn(async move {
        run_http_tunnel_server(
            CLIENT_API_SERVER_PORT,
            CLIENT_API_HTTP_ENTRY_PORT,
            CLIENT_API_TOKEN,
            server_shutdown_rx,
        )
        .await
        .unwrap();
    });

    let client_config = config_path.clone();
    let client = tokio::spawn(async move {
        run_http_tunnel_client(&client_config, client_shutdown_rx)
            .await
            .unwrap();
    });

    time::sleep(Duration::from_millis(2500)).await;

    info!("the client API reports the client identity");
    let (status, body) = api_request(
        CLIENT_API_ADDR,
        "GET",
        "/api/status",
        Some(CLIENT_API_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(body.contains("home"), "unexpected body: {}", body);

    info!("the client API has no tunnel yet");
    let (status, body) = api_request(
        CLIENT_API_ADDR,
        "GET",
        "/api/tunnels",
        Some(CLIENT_API_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert_eq!(body.trim(), "[]", "unexpected body: {}", body);

    info!("the client API rejects a request without the token");
    let (status, _) = api_request(CLIENT_API_ADDR, "GET", "/api/tunnels", None, None).await?;
    assert_eq!(status, 401, "expected 401");

    info!("add a tunnel through the client API");
    let (status, body) = api_request(
        CLIENT_API_ADDR,
        "PUT",
        "/api/tunnels/echo",
        Some(CLIENT_API_TOKEN),
        Some(r#"{"domain":"echo.test","local_addr":"127.0.0.1:8093"}"#),
    )
    .await?;
    assert_eq!(status, 201, "unexpected response: {}", body);
    assert!(body.contains("127.0.0.1:8093"), "unexpected body: {}", body);
    assert!(body.contains("\"name\":\"echo\""), "unexpected body: {}", body);
    assert!(body.contains("\"domain\":\"echo.test\""), "unexpected body: {}", body);

    // The client starts the tunnel
    time::sleep(Duration::from_millis(1500)).await;
    info!("the added tunnel is routed without a restart");
    http_echo_hitter(CLIENT_API_HTTP_ENTRY_ADDR, "echo.test")
        .await
        .unwrap();

    info!("the client API now reports the client as connected");
    let (status, body) = api_request(
        CLIENT_API_ADDR,
        "GET",
        "/api/status",
        Some(CLIENT_API_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(
        body.contains("\"connected\":true"),
        "unexpected body: {}",
        body
    );

    info!("the client API lists the added tunnel");
    let (status, body) = api_request(
        CLIENT_API_ADDR,
        "GET",
        "/api/tunnels",
        Some(CLIENT_API_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(body.contains("echo.test"), "unexpected body: {}", body);

    info!("the client wrote the change back to its config");
    let written = std::fs::read_to_string(&config_path)?;
    assert!(
        written.contains("echo.test"),
        "the config wasn't written back: {}",
        written
    );

    info!("delete the tunnel through the client API");
    let (status, body) = api_request(
        CLIENT_API_ADDR,
        "DELETE",
        "/api/tunnels/echo",
        Some(CLIENT_API_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 204, "unexpected response: {}", body);

    time::sleep(Duration::from_millis(500)).await;
    info!("the deleted tunnel is no longer routed");
    http_404_check(CLIENT_API_HTTP_ENTRY_ADDR, "echo.test")
        .await
        .unwrap();

    info!("deleting an unknown tunnel is rejected with 404");
    let (status, body) = api_request(
        CLIENT_API_ADDR,
        "DELETE",
        "/api/tunnels/unknown.test",
        Some(CLIENT_API_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 404, "unexpected response: {}", body);

    info!("shutdown the server and the client");
    server_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(server, client);

    Ok(())
}

// ==== The fatal auth error test ====

const FATAL_SERVER_PORT: u16 = 2453;
const FATAL_SERVER_ADDR: &str = "127.0.0.1:2453";

/// A client whose token is rejected must exit with an error instead of retrying
/// forever. The timeout fails the test if the client keeps retrying.
#[tokio::test]
async fn client_exits_on_a_wrong_token() -> Result<()> {
    init();

    let config_path = write_client_config(
        "http_tunnel_fatal_test.toml",
        "home",
        FATAL_SERVER_ADDR,
        "wrong_token",
        None,
        &[("echo.test", "127.0.0.1:8092")],
    )?;

    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);
    let server = tokio::spawn(async move {
        run_http_tunnel_server(
            FATAL_SERVER_PORT,
            2454,
            CLIENT_TOKEN,
            server_shutdown_rx,
        )
        .await
        .unwrap();
    });

    // Wait for the server to listen, so that the client reaches the handshake
    time::sleep(Duration::from_secs(1)).await;

    info!("a wrong token must end the client");
    let (_tx, rx) = broadcast::channel(1);
    let err = time::timeout(
        Duration::from_secs(5),
        run_http_tunnel_client(&config_path, rx),
    )
    .await
    .expect("the client kept retrying on a wrong token")
    .expect_err("the client must fail on a wrong token");
    assert!(
        format!("{:#}", err).contains("token"),
        "unexpected error: {:#}",
        err
    );

    info!("shutdown the server");
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(server);

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


// ==== WebSocket resource-limit integration test ====

const WS_LIMIT_SERVER_PORT: u16 = 2535;
const WS_LIMIT_HTTP_PORT: u16 = 2536;
const WS_LIMIT_HTTP_ADDR: &str = "127.0.0.1:2536";

/// Exercise the real HTTP upgrade path at the configured WebSocket connection
/// limit, verify excess connections receive 503, and ensure closed connections
/// release their slots for subsequent clients.
#[cfg(feature = "client-api")]
#[tokio::test]
async fn websocket_connection_limit_returns_503_and_recovers() -> Result<()> {
    use tokio_tungstenite::connect_async;

    init();
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let server = tokio::spawn(async move {
        run_http_tunnel_server_with_control_host(
            WS_LIMIT_SERVER_PORT,
            WS_LIMIT_HTTP_PORT,
            Some("127.0.0.1"),
            CLIENT_TOKEN,
            shutdown_rx,
        )
        .await
        .unwrap();
    });
    // Wait until both listeners are bound before creating the connection burst.
    time::sleep(Duration::from_millis(300)).await;

    // Check control endpoint routing before consuming the connection budget.
    for (request, expected_status) in [
        (
            "GET /wrong HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 404",
        ),
        (
            "GET /_http-tunnel/v1/connect HTTP/1.1\r\nHost: other.test\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 404",
        ),
        (
            "GET /_http-tunnel/v1/connect HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 426",
        ),
        (
            "GET /_http-tunnel/v1/connect HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: keep-alive\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 426",
        ),
    ] {
        let mut conn = TcpStream::connect(WS_LIMIT_HTTP_ADDR).await?;
        conn.write_all(request.as_bytes()).await?;
        let mut response = Vec::new();
        time::timeout(Duration::from_secs(2), conn.read_to_end(&mut response))
            .await??;
        let response = String::from_utf8_lossy(&response);
        assert!(
            response.starts_with(expected_status),
            "expected {} for request {:?}, got {}",
            expected_status,
            request,
            response
        );
    }

    let url = format!("ws://{}/_http-tunnel/v1/connect", WS_LIMIT_HTTP_ADDR);
    let mut sockets = Vec::with_capacity(256);
    for index in 0..256 {
        let result = time::timeout(Duration::from_secs(3), connect_async(&url)).await;
        match result {
            std::result::Result::Ok(std::result::Result::Ok((socket, _))) => sockets.push(socket),
            std::result::Result::Ok(Err(error)) => anyhow::bail!("WebSocket connection {} failed before the limit: {}", index + 1, error),
            Err(_) => anyhow::bail!("WebSocket connection {} timed out before the limit", index + 1),
        }
    }

    let excess = time::timeout(Duration::from_secs(3), connect_async(&url)).await;
    match excess {
        std::result::Result::Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response))) => {
            assert_eq!(
                response.status().as_u16(),
                503,
                "expected HTTP 503 after the connection limit, got {}",
                response.status()
            );
        }
        std::result::Result::Ok(std::result::Result::Ok((socket, _))) => {
            drop(socket);
            anyhow::bail!("the 257th WebSocket connection unexpectedly succeeded");
        }
        std::result::Result::Ok(Err(error)) => anyhow::bail!("unexpected error for the 257th connection: {}", error),
        Err(_) => anyhow::bail!("the 257th WebSocket handshake timed out"),
    }

    // Closing held connections must release permits; a new upgrade should work.
    drop(sockets);
    time::sleep(Duration::from_millis(300)).await;
    let recovered = time::timeout(Duration::from_secs(3), connect_async(&url)).await;
    match recovered {
        std::result::Result::Ok(std::result::Result::Ok((socket, _))) => drop(socket),
        std::result::Result::Ok(Err(error)) => anyhow::bail!("connection slot did not recover after close: {}", error),
        Err(_) => anyhow::bail!("recovery WebSocket handshake timed out"),
    }

    shutdown_tx.send(true)?;
    server.await?;
    std::result::Result::Ok(())
}
