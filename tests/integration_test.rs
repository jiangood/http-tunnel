use anyhow::{Ok, Result};
use common::run_http_tunnel_from_config;
use rand::Rng;
use std::time::{Duration, Instant};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::broadcast,
    time,
};
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;

use crate::common::{run_http_tunnel_server, run_http_tunnel_server_with_limits};

mod common;
const ECHO_SERVER_ADDR: &str = "127.0.0.1:18080";
const HTTP_ENTRY_ADDR: &str = "127.0.0.1:2334";
const HTTP_ENTRY_PORT: u16 = 2334;
const WS_HTTP_ENTRY_ADDR: &str = "127.0.0.1:2336";
const WS_HTTP_ENTRY_PORT: u16 = 2336;
const WS_ECHO_SERVER_ADDR: &str = "127.0.0.1:8081";
const CONTROL_HOST: &str = "127.0.0.1";
const LEAF_NAME: &str = "home";
const LEAF_TOKEN: &str = "a_secret_token";
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

/// Write a universal leaf configuration to a temporary file and return its path.
///
/// The leaf always runs its own listener (an ephemeral port is fine)
/// and dials the given ingress through `[upstream]`. Its local services are
/// declared with `[[proxies]]`.
fn write_leaf_config(
    file: &str,
    name: &str,
    upstream_host: &str,
    upstream_port: u16,
    token: &str,
    listen_port: u16,
    proxies: &[(&str, &str)],
) -> Result<String> {
    let mut proxies_toml = String::new();
    for (domain, local_addr) in proxies {
        // The local-service names are required. Use the first domain label as a
        // stable name for these integration tests; a wildcard domain uses the
        // label after `*.`.
        let name = domain
            .trim_start_matches("*.")
            .split('.')
            .next()
            .unwrap_or(domain);
        proxies_toml.push_str(&format!(
            "\n[[proxies]]\nname = \"{}\"\ndomain = \"{}\"\nlocal_addr = \"{}\"\n",
            name, domain, local_addr
        ));
    }
    // The leaf listens on an ephemeral port unless the test needs a fixed one
    // to reach its management interface.
    let content = format!(
        "listen_port = {}\ncontrol_host = \"127.0.0.1\"\n\
         name = \"{}\"\ntoken = \"{}\"\n{}\
         \n[upstream]\nurl = \"ws://{}:{}\"\ntoken = \"{}\"\nmirror = true\n",
        listen_port, name, token, proxies_toml, upstream_host, upstream_port, token
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

    let client_config = write_leaf_config(
        "http_tunnel_routing_client.toml",
        LEAF_NAME,
        CONTROL_HOST,
        HTTP_ENTRY_PORT,
        LEAF_TOKEN,
        0,
        &[(ECHO_HOST, ECHO_SERVER_ADDR)],
    )?;

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    // Start the client
    info!("start the client");
    let client = tokio::spawn(async move {
        run_http_tunnel_from_config(&client_config, client_shutdown_rx)
            .await
            .unwrap();
    });

    // Sleep for 1 second. Expect the client keep retrying to reach the server
    time::sleep(Duration::from_secs(1)).await;

    // Start the server
    info!("start the server");
    let server = tokio::spawn(async move {
        run_http_tunnel_server(
            HTTP_ENTRY_PORT,
            CONTROL_HOST,
            LEAF_TOKEN,
            server_shutdown_rx,
        )
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
        run_http_tunnel_from_config(client_config.to_str().unwrap(), client_shutdown_rx)
            .await
            .unwrap();
    });
    time::sleep(Duration::from_secs(1)).await; // Wait for the client to start

    info!("tunnel by Host");
    http_echo_hitter(HTTP_ENTRY_ADDR, ECHO_HOST).await.unwrap();

    // Every visitor is multiplexed over the same v4 session, so a burst must
    // not depend on any pre-opened session.
    info!("more concurrent visitors than a single stream");
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
        run_http_tunnel_server(
            HTTP_ENTRY_PORT,
            CONTROL_HOST,
            LEAF_TOKEN,
            server_shutdown_rx,
        )
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

/// Poll `addr` until it accepts TCP connections. Tests use this instead of a
/// fixed sleep so a slow or busy machine does not turn a startup delay into a
/// spurious connection-refused failure.
async fn wait_for_tcp(addr: &'static str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "nothing accepted connections at {} within {:?}",
                addr,
                timeout
            );
        }
        time::sleep(Duration::from_millis(50)).await;
    }
}

/// Retry an echo request until the tunnel is registered and healthy again. A
/// tunnel that is not registered yet answers with an error page or closes the
/// connection, both of which `http_echo_hitter` reports as an error.
async fn wait_for_echo(addr: &'static str, host: &'static str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let last_error =
            match time::timeout(Duration::from_secs(5), http_echo_hitter(addr, host)).await {
                std::result::Result::Ok(std::result::Result::Ok(())) => return Ok(()),
                std::result::Result::Ok(Err(e)) => format!("{:#}", e),
                std::result::Result::Err(_) => "the request timed out".to_string(),
            };
        if Instant::now() >= deadline {
            anyhow::bail!(
                "the tunnel did not serve {} within {:?}: {}",
                host,
                timeout,
                last_error
            );
        }
        time::sleep(Duration::from_millis(100)).await;
    }
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
/// session was removed from the route table.
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

// ==== The management API test ====

const MANAGEMENT_ECHO_SERVER_ADDR: &str = "127.0.0.1:8093";
const MANAGEMENT_HTTP_ENTRY_PORT: u16 = 2464;
const MANAGEMENT_HTTP_ENTRY_ADDR: &str = "127.0.0.1:2464";
const MANAGEMENT_ADDR: &str = "127.0.0.1:2465";
const MANAGEMENT_TOKEN: &str = "a_secret_token";

/// A WebSocket session carries the routes and every visitor stream over the
/// HTTP entrypoint, and reconnects after a server restart.
#[tokio::test]
async fn websocket_transport_routes_http() -> Result<()> {
    init();

    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(WS_ECHO_SERVER_ADDR).await {
            panic!("Failed to run the WebSocket echo server: {:?}", e);
        }
    });

    let client_config = write_leaf_config(
        "http_tunnel_websocket_client.toml",
        LEAF_NAME,
        "127.0.0.1",
        WS_HTTP_ENTRY_PORT,
        LEAF_TOKEN,
        0,
        &[("echo-ws.test", WS_ECHO_SERVER_ADDR)],
    )?;
    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    let server = tokio::spawn(async move {
        common::run_http_tunnel_server(
            WS_HTTP_ENTRY_PORT,
            "127.0.0.1",
            LEAF_TOKEN,
            server_shutdown_rx,
        )
        .await
        .unwrap();
    });
    wait_for_tcp(WS_HTTP_ENTRY_ADDR, Duration::from_secs(10)).await?;

    let client = tokio::spawn(async move {
        run_http_tunnel_from_config(&client_config, client_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_echo(WS_HTTP_ENTRY_ADDR, "echo-ws.test", Duration::from_secs(20)).await?;

    // Restart the server while the client remains alive. The v4 session should
    // close and the existing retry loop should register the tunnel again.
    server_shutdown_tx.send(true)?;
    server.await?;

    let (server2_shutdown_tx, server2_shutdown_rx) = broadcast::channel(1);
    let server2 = tokio::spawn(async move {
        common::run_http_tunnel_server(
            WS_HTTP_ENTRY_PORT,
            "127.0.0.1",
            LEAF_TOKEN,
            server2_shutdown_rx,
        )
        .await
        .unwrap();
    });
    wait_for_tcp(WS_HTTP_ENTRY_ADDR, Duration::from_secs(10)).await?;
    wait_for_echo(WS_HTTP_ENTRY_ADDR, "echo-ws.test", Duration::from_secs(20)).await?;

    client_shutdown_tx.send(true)?;
    server2_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server2);
    Ok(())
}

// ==== WebSocket-only entrypoint ====

const WS_ONLY_HTTP_PORT: u16 = 2555;
const WS_ONLY_HTTP_ADDR: &str = "127.0.0.1:2555";
const WS_ONLY_ECHO_ADDR: &str = "127.0.0.1:2556";

/// The WebSocket transport carries the whole tunnel through the HTTP
/// entrypoint, so no extra session port exists.
#[tokio::test]
async fn websocket_transport_works_without_a_tcp_listener() -> Result<()> {
    init();

    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(WS_ONLY_ECHO_ADDR).await {
            panic!("Failed to run the WebSocket echo server: {:?}", e);
        }
    });

    let client_config = write_leaf_config(
        "http_tunnel_websocket_no_tcp.toml",
        LEAF_NAME,
        "127.0.0.1",
        WS_ONLY_HTTP_PORT,
        LEAF_TOKEN,
        0,
        &[("echo-ws-only.test", WS_ONLY_ECHO_ADDR)],
    )?;
    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    let server = tokio::spawn(async move {
        common::run_http_tunnel_server(
            WS_ONLY_HTTP_PORT,
            "127.0.0.1",
            LEAF_TOKEN,
            server_shutdown_rx,
        )
        .await
        .unwrap();
    });
    wait_for_tcp(WS_ONLY_HTTP_ADDR, Duration::from_secs(10)).await?;

    let client = tokio::spawn(async move {
        run_http_tunnel_from_config(&client_config, client_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_echo(
        WS_ONLY_HTTP_ADDR,
        "echo-ws-only.test",
        Duration::from_secs(20),
    )
    .await?;

    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    Ok(())
}

// ==== WebSocket transport with an unavailable leaf-side target ====

const WS_FAILURE_HTTP_PORT: u16 = 2546;
const WS_FAILURE_HTTP_ADDR: &str = "127.0.0.1:2546";
const WS_FAILURE_TARGET_ADDR: &str = "127.0.0.1:2547";

/// A tunnel whose local target refuses connections must not hang visitors
/// forever; v4 reports the connect failure with a `502 Bad Gateway`.
#[tokio::test]
async fn websocket_transport_handles_unavailable_local_target() -> Result<()> {
    init();
    let client_config = write_leaf_config(
        "http_tunnel_websocket_unavailable_target.toml",
        LEAF_NAME,
        "127.0.0.1",
        WS_FAILURE_HTTP_PORT,
        LEAF_TOKEN,
        0,
        &[("unavailable-ws.test", WS_FAILURE_TARGET_ADDR)],
    )?;
    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);
    let server = tokio::spawn(async move {
        run_http_tunnel_server(
            WS_FAILURE_HTTP_PORT,
            "127.0.0.1",
            LEAF_TOKEN,
            server_shutdown_rx,
        )
        .await
        .unwrap();
    });
    wait_for_tcp(WS_FAILURE_HTTP_ADDR, Duration::from_secs(10)).await?;
    let client = tokio::spawn(async move {
        run_http_tunnel_from_config(&client_config, client_shutdown_rx)
            .await
            .unwrap();
    });
    // Give the client time to register the tunnel before the visitor arrives;
    // this test is about the connect failure, not the unregistered-tunnel path.
    time::sleep(Duration::from_secs(2)).await;

    let mut visitor = TcpStream::connect(WS_FAILURE_HTTP_ADDR).await?;
    visitor
        .write_all(b"GET / HTTP/1.1\r\nHost: unavailable-ws.test\r\nConnection: close\r\n\r\n")
        .await?;
    let mut response = Vec::new();
    time::timeout(Duration::from_secs(18), visitor.read_to_end(&mut response)).await??;
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 502"),
        "expected 502 for an unavailable local target, got {}",
        response
    );
    // Let the ingress observe the visitor close before it drains for shutdown.
    drop(visitor);

    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    Ok(())
}

/// A proxy created, listed and deleted through the management API is
/// validated locally, written back to the client configuration file, and routed
/// without a restart of either side.
#[cfg(feature = "management")]
#[tokio::test]
async fn management_api_manages_proxies() -> Result<()> {
    init();

    let config_path = write_leaf_config(
        "http_tunnel_management_api_test.toml",
        "home",
        "127.0.0.1",
        MANAGEMENT_HTTP_ENTRY_PORT,
        MANAGEMENT_TOKEN,
        2465,
        &[],
    )?;

    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(MANAGEMENT_ECHO_SERVER_ADDR).await {
            panic!("Failed to run the echo server for testing: {:?}", e);
        }
    });

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    let server = tokio::spawn(async move {
        run_http_tunnel_server(
            MANAGEMENT_HTTP_ENTRY_PORT,
            "127.0.0.1",
            MANAGEMENT_TOKEN,
            server_shutdown_rx,
        )
        .await
        .unwrap();
    });

    let client_config = config_path.clone();
    let client = tokio::spawn(async move {
        run_http_tunnel_from_config(&client_config, client_shutdown_rx)
            .await
            .unwrap();
    });

    time::sleep(Duration::from_millis(2500)).await;

    info!("the management API reports the process identity");
    let (status, body) = api_request(
        MANAGEMENT_ADDR,
        "GET",
        "/api/status",
        Some(MANAGEMENT_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(body.contains("home"), "unexpected body: {}", body);

    info!("the management API has no tunnel yet");
    let (status, body) = api_request(
        MANAGEMENT_ADDR,
        "GET",
        "/api/proxies",
        Some(MANAGEMENT_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert_eq!(body.trim(), "[]", "unexpected body: {}", body);

    info!("the management API rejects a request without the token");
    let (status, _) = api_request(MANAGEMENT_ADDR, "GET", "/api/proxies", None, None).await?;
    assert_eq!(status, 401, "expected 401");

    info!("add a tunnel through the management API");
    let (status, body) = api_request(
        MANAGEMENT_ADDR,
        "PUT",
        "/api/proxies/echo",
        Some(MANAGEMENT_TOKEN),
        Some(r#"{"domain":"echo.test","local_addr":"127.0.0.1:8093"}"#),
    )
    .await?;
    assert_eq!(status, 201, "unexpected response: {}", body);
    assert!(body.contains("127.0.0.1:8093"), "unexpected body: {}", body);
    assert!(
        body.contains("\"name\":\"echo\""),
        "unexpected body: {}",
        body
    );
    assert!(
        body.contains("\"domain\":\"echo.test\""),
        "unexpected body: {}",
        body
    );

    // The client starts the tunnel
    time::sleep(Duration::from_millis(1500)).await;
    info!("the added tunnel is routed without a restart");
    http_echo_hitter(MANAGEMENT_HTTP_ENTRY_ADDR, "echo.test")
        .await
        .unwrap();

    info!("the management API now reports the client as connected");
    let (status, body) = api_request(
        MANAGEMENT_ADDR,
        "GET",
        "/api/status",
        Some(MANAGEMENT_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(
        body.contains("\"connected\":true"),
        "unexpected body: {}",
        body
    );

    info!("the management API lists the added tunnel");
    let (status, body) = api_request(
        MANAGEMENT_ADDR,
        "GET",
        "/api/proxies",
        Some(MANAGEMENT_TOKEN),
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

    info!("delete the tunnel through the management API");
    let (status, body) = api_request(
        MANAGEMENT_ADDR,
        "DELETE",
        "/api/proxies/echo",
        Some(MANAGEMENT_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 204, "unexpected response: {}", body);

    time::sleep(Duration::from_millis(500)).await;
    info!("the deleted tunnel is no longer routed");
    http_404_check(MANAGEMENT_HTTP_ENTRY_ADDR, "echo.test")
        .await
        .unwrap();

    info!("deleting an unknown tunnel is rejected with 404");
    let (status, body) = api_request(
        MANAGEMENT_ADDR,
        "DELETE",
        "/api/proxies/unknown.test",
        Some(MANAGEMENT_TOKEN),
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

const FATAL_HTTP_PORT: u16 = 2454;

/// A client whose token is rejected must exit with an error instead of retrying
/// forever. The timeout fails the test if the client keeps retrying.
#[tokio::test]
async fn client_exits_on_a_wrong_token() -> Result<()> {
    init();

    let config_path = write_leaf_config(
        "http_tunnel_fatal_test.toml",
        "home",
        "127.0.0.1",
        FATAL_HTTP_PORT,
        "wrong_token",
        0,
        &[("echo.test", "127.0.0.1:8092")],
    )?;

    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);
    let server = tokio::spawn(async move {
        run_http_tunnel_server(FATAL_HTTP_PORT, "127.0.0.1", LEAF_TOKEN, server_shutdown_rx)
            .await
            .unwrap();
    });

    // Wait for the server to listen, so that the client reaches the handshake
    time::sleep(Duration::from_secs(1)).await;

    info!("a wrong token must end the client");
    let (_tx, rx) = broadcast::channel(1);
    let err = time::timeout(
        Duration::from_secs(5),
        run_http_tunnel_from_config(&config_path, rx),
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

const OCCUPIED_HTTP_TEST_HTTP_PORT: u16 = 2476;

/// A `listen_port` that cannot be bound must fail startup with an error instead
/// of leaving a server that looks healthy but serves no traffic.
#[tokio::test]
async fn server_fails_when_listen_port_cannot_be_bound() -> Result<()> {
    init();

    // Bind every interface, which is what the listener always does.
    let _occupied =
        tokio::net::TcpListener::bind(("0.0.0.0", OCCUPIED_HTTP_TEST_HTTP_PORT)).await?;
    let (_shutdown_tx, shutdown_rx) = broadcast::channel(1);

    let err = time::timeout(
        Duration::from_secs(5),
        run_http_tunnel_server(
            OCCUPIED_HTTP_TEST_HTTP_PORT,
            "127.0.0.1",
            LEAF_TOKEN,
            shutdown_rx,
        ),
    )
    .await
    .expect("server startup hung on an unboundable listen_port")
    .expect_err("server must fail when listen_port cannot be bound");

    assert!(
        format!("{:#}", err).contains("listen_port"),
        "unexpected error: {:#}",
        err
    );

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
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1\r\n{}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
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

const WS_LIMIT_HTTP_PORT: u16 = 2536;
const WS_LIMIT_HTTP_ADDR: &str = "127.0.0.1:2536";
const WS_LIMIT: usize = 8;

/// Build an upgrade request with the shared token in the `Authorization` header.
fn websocket_request(
    url: &str,
    token: Option<&str>,
) -> tokio_tungstenite::tungstenite::http::Request<()> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut request = url.into_client_request().expect("valid WebSocket URL");
    if let Some(token) = token {
        let value = tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!(
            "Bearer {}",
            token
        ))
        .expect("valid token header");
        request.headers_mut().insert("Authorization", value);
    }
    request
}

/// Exercise the real HTTP upgrade path at the configured handshake limit:
/// authenticated excess upgrades receive 503, an unauthenticated one still
/// receives 401 even when the budget is full, and closing the sockets releases
/// their slots for subsequent clients.
#[tokio::test]
async fn websocket_connection_limit_returns_503_and_recovers() -> Result<()> {
    use tokio_tungstenite::connect_async;

    init();
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let limits = http_tunnel::ServerLimits {
        max_pending_handshakes: WS_LIMIT,
        ..http_tunnel::ServerLimits::default()
    };
    let server = tokio::spawn(async move {
        run_http_tunnel_server_with_limits(
            WS_LIMIT_HTTP_PORT,
            "127.0.0.1",
            LEAF_TOKEN,
            limits,
            shutdown_rx,
        )
        .await
        .unwrap();
    });
    // Wait until the listener is bound before creating the connection burst.
    wait_for_tcp(WS_LIMIT_HTTP_ADDR, Duration::from_secs(10)).await?;

    // Check session endpoint routing before consuming the connection budget.
    for (request, expected_status) in [
        (
            "GET /wrong HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 404",
        ),
        (
            "GET /_http-tunnel/v4/connect HTTP/1.1\r\nHost: other.test\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 404",
        ),
        (
            "GET /_http-tunnel/v4/connect HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 426",
        ),
        (
            "GET /_http-tunnel/v4/connect HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: keep-alive\r\nConnection: close\r\n\r\n",
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

    let url = format!("ws://{}/_http-tunnel/v4/connect", WS_LIMIT_HTTP_ADDR);
    // A bridged WebSocket that has not sent the tunnel protocol ClientHello is
    // reclaimed after five seconds. The burst must therefore stay inside that
    // window; retry instead of reporting a false regression on a slow runner.
    const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
    const BURST_BUDGET: Duration = Duration::from_secs(4);
    let mut held = None;
    for attempt in 1..=3 {
        let started = Instant::now();
        let attempts = futures_util::future::join_all((0..WS_LIMIT).map(|_| async {
            time::timeout(
                Duration::from_secs(8),
                connect_async(websocket_request(&url, Some(LEAF_TOKEN))),
            )
            .await
        }))
        .await;
        let mut sockets = Vec::with_capacity(WS_LIMIT);
        for (index, result) in attempts.into_iter().enumerate() {
            match result {
                std::result::Result::Ok(std::result::Result::Ok((socket, _))) => {
                    sockets.push(socket)
                }
                std::result::Result::Ok(Err(error)) => anyhow::bail!(
                    "WebSocket connection {} failed before the limit: {}",
                    index + 1,
                    error
                ),
                Err(_) => anyhow::bail!(
                    "WebSocket connection {} timed out before the limit",
                    index + 1
                ),
            }
        }
        let burst = started.elapsed();
        if burst >= BURST_BUDGET {
            debug!(
                "opening {} WebSockets took {:?} on attempt {}; retrying",
                WS_LIMIT, burst, attempt
            );
            drop(sockets);
            continue;
        }

        // Authentication is checked before the handshake permit is taken, so an
        // unauthenticated upgrade is still answered with 401, not 503.
        let unauthenticated = time::timeout(
            Duration::from_secs(3),
            connect_async(websocket_request(&url, None)),
        )
        .await;
        match unauthenticated {
            std::result::Result::Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response)))
                if response.status().as_u16() == 401 => {}
            other => anyhow::bail!(
                "an unauthenticated upgrade was not answered with 401: {:?}",
                other
            ),
        }

        let excess = time::timeout(
            Duration::from_secs(3),
            connect_async(websocket_request(&url, Some(LEAF_TOKEN))),
        )
        .await;
        match excess {
            std::result::Result::Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response)))
                if response.status().as_u16() == 503 =>
            {
                held = Some(sockets);
                break;
            }
            std::result::Result::Ok(std::result::Result::Ok((socket, _))) => {
                drop(socket);
                // The burst finished inside the hello window, so every slot was
                // still held: accepting one more connection is a regression.
                anyhow::bail!("an extra WebSocket connection unexpectedly succeeded");
            }
            std::result::Result::Ok(Err(error)) => {
                anyhow::bail!("unexpected error for the extra connection: {}", error)
            }
            std::result::Result::Err(_) => anyhow::bail!("the extra WebSocket handshake timed out"),
        }
    }
    let sockets = held.ok_or_else(|| {
        anyhow::anyhow!(
            "could not open {} WebSocket connections within the {:?} hello window",
            WS_LIMIT,
            HELLO_TIMEOUT
        )
    })?;

    // Closing held connections must release permits; a new upgrade should work.
    drop(sockets);
    let mut recovered = false;
    for _ in 0..50 {
        match connect_async(websocket_request(&url, Some(LEAF_TOKEN))).await {
            std::result::Result::Ok((socket, _)) => {
                drop(socket);
                recovered = true;
                break;
            }
            std::result::Result::Err(_) => time::sleep(Duration::from_millis(100)).await,
        }
    }
    assert!(
        recovered,
        "connection slot did not recover within five seconds of closing"
    );

    shutdown_tx.send(true)?;
    server.await?;
    std::result::Result::Ok(())
}

// ==== Multi-hop relay chain ====

const CHAIN_ECHO_ADDR: &str = "127.0.0.1:2610";
const CHAIN_INGRESS_PORT: u16 = 2611;
const CHAIN_INGRESS_ADDR: &str = "127.0.0.1:2611";
const CHAIN_NODE_A_PORT: u16 = 2612;
const CHAIN_NODE_A_ADDR: &str = "127.0.0.1:2612";
const CHAIN_NODE_B_PORT: u16 = 2613;
const CHAIN_NODE_B_ADDR: &str = "127.0.0.1:2613";
const CHAIN_INGRESS_TOKEN: &str = "chain-ingress-secret";
const CHAIN_NODE_A_TOKEN: &str = "chain-node-a-secret";
const CHAIN_NODE_B_TOKEN: &str = "chain-node-b-secret";
const CHAIN_HOST: &str = "svc.relay.test";

/// Write a node configuration to a temporary file and return its path.
///
/// `upstream` is `(url, token, claim)`: a `None` claim means route mirroring,
/// and `None` for the whole option makes the node an ingress. The node name
/// doubles as the name announced on the upstream.
fn write_node_config(
    file: &str,
    name: &str,
    listen_port: u16,
    token: &str,
    upstream: Option<(&str, &str, Option<&str>)>,
    proxies: &[(&str, &str)],
) -> Result<String> {
    let mut content = format!(
        "name = \"{}\"\nlisten_port = {}\ncontrol_host = \"127.0.0.1\"\n\
         token = \"{}\"\n",
        name, listen_port, token
    );
    for (domain, local_addr) in proxies {
        let proxy_name = domain.split('.').next().unwrap_or(domain);
        content.push_str(&format!(
            "\n[[proxies]]\nname = \"{}\"\ndomain = \"{}\"\nlocal_addr = \"{}\"\n",
            proxy_name, domain, local_addr
        ));
    }
    if let Some((upstream_url, upstream_token, claim)) = upstream {
        content.push_str(&format!(
            "\n[upstream]\nurl = \"{}\"\ntoken = \"{}\"\n",
            upstream_url, upstream_token
        ));
        match claim {
            Some(claim) => content.push_str(&format!("claim = \"{}\"\n", claim)),
            None => content.push_str("mirror = true\n"),
        }
    }

    let path = std::env::temp_dir().join(file);
    std::fs::write(&path, content)?;
    Ok(path.to_str().unwrap().to_string())
}

/// `ht` ingress -> node A (wildcard claim) -> node B (exact claim + local proxy)
/// -> echo service. A visitor to the public ingress reaches the service across
/// three hops with one process per hop.
///
/// Stopping node B withdraws its route, so the ingress answers with `404`
/// instead of hanging; restarting it claims the route again.
#[tokio::test]
async fn node_chain_forwards_and_withdraws_routes() -> Result<()> {
    init();

    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(CHAIN_ECHO_ADDR).await {
            panic!("Failed to run the echo server for testing: {:?}", e);
        }
    });

    let (ingress_shutdown_tx, ingress_shutdown_rx) = broadcast::channel(1);
    let ingress = tokio::spawn(async move {
        run_http_tunnel_server(
            CHAIN_INGRESS_PORT,
            "127.0.0.1",
            CHAIN_INGRESS_TOKEN,
            ingress_shutdown_rx,
        )
        .await
        .unwrap();
    });
    wait_for_tcp(CHAIN_INGRESS_ADDR, Duration::from_secs(10)).await?;

    let node_a_config = write_node_config(
        "http_tunnel_node_a.toml",
        "relay-a",
        CHAIN_NODE_A_PORT,
        CHAIN_NODE_A_TOKEN,
        Some((
            &format!("ws://{}", CHAIN_INGRESS_ADDR),
            CHAIN_INGRESS_TOKEN,
            Some("*.relay.test"),
        )),
        &[],
    )?;
    let (node_a_shutdown_tx, node_a_shutdown_rx) = broadcast::channel(1);
    let node_a = tokio::spawn(async move {
        common::run_http_tunnel_from_config(&node_a_config, node_a_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_tcp(CHAIN_NODE_A_ADDR, Duration::from_secs(10)).await?;

    let node_b_config = write_node_config(
        "http_tunnel_node_b.toml",
        "relay-b",
        CHAIN_NODE_B_PORT,
        CHAIN_NODE_B_TOKEN,
        Some((
            &format!("ws://{}", CHAIN_NODE_A_ADDR),
            CHAIN_NODE_A_TOKEN,
            Some(CHAIN_HOST),
        )),
        &[(CHAIN_HOST, CHAIN_ECHO_ADDR)],
    )?;
    let (node_b_shutdown_tx, node_b_shutdown_rx) = broadcast::channel(1);
    let node_b_path = node_b_config.clone();
    let node_b = tokio::spawn(async move {
        common::run_http_tunnel_from_config(&node_b_path, node_b_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_tcp(CHAIN_NODE_B_ADDR, Duration::from_secs(10)).await?;

    info!("the three-hop chain serves the leaf domain");
    wait_for_echo(CHAIN_INGRESS_ADDR, CHAIN_HOST, Duration::from_secs(20)).await?;

    info!("stopping the leaf node withdraws its route");
    node_b_shutdown_tx.send(true)?;
    let _ = tokio::join!(node_b);
    wait_for_404(CHAIN_INGRESS_ADDR, CHAIN_HOST).await?;

    info!("restarting the leaf node claims its route again");
    let (node_b_shutdown_tx, node_b_shutdown_rx) = broadcast::channel(1);
    let node_b_path = node_b_config.clone();
    let node_b = tokio::spawn(async move {
        common::run_http_tunnel_from_config(&node_b_path, node_b_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_echo(CHAIN_INGRESS_ADDR, CHAIN_HOST, Duration::from_secs(20)).await?;

    info!("shutdown the chain");
    node_b_shutdown_tx.send(true)?;
    node_a_shutdown_tx.send(true)?;
    ingress_shutdown_tx.send(true)?;
    let _ = tokio::join!(node_b, node_a, ingress);
    Ok(())
}

// ==== Fan-out under one relay ====

const FANOUT_ECHO_ADDR: &str = "127.0.0.1:2630";
const FANOUT_INGRESS_PORT: u16 = 2631;
const FANOUT_INGRESS_ADDR: &str = "127.0.0.1:2631";
const FANOUT_NODE_A_PORT: u16 = 2632;
const FANOUT_NODE_A_ADDR: &str = "127.0.0.1:2632";
const FANOUT_NODE_B1_PORT: u16 = 2633;
const FANOUT_NODE_B1_ADDR: &str = "127.0.0.1:2633";
const FANOUT_NODE_B2_PORT: u16 = 2634;
const FANOUT_NODE_B2_ADDR: &str = "127.0.0.1:2634";

/// Two downstream nodes publish different domains under the same upstream
/// node: no extra configuration is needed at the relay or the ingress.
#[tokio::test]
async fn node_fanout_publishes_two_downstream_domains() -> Result<()> {
    init();
    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(FANOUT_ECHO_ADDR).await {
            panic!("Failed to run the echo server for testing: {:?}", e);
        }
    });

    let (ingress_shutdown_tx, ingress_shutdown_rx) = broadcast::channel(1);
    let ingress = tokio::spawn(async move {
        run_http_tunnel_server(
            FANOUT_INGRESS_PORT,
            "127.0.0.1",
            CHAIN_INGRESS_TOKEN,
            ingress_shutdown_rx,
        )
        .await
        .unwrap();
    });
    wait_for_tcp(FANOUT_INGRESS_ADDR, Duration::from_secs(10)).await?;

    let node_a_config = write_node_config(
        "http_tunnel_fanout_a.toml",
        "fanout-a",
        FANOUT_NODE_A_PORT,
        CHAIN_NODE_A_TOKEN,
        Some((
            &format!("ws://{}", FANOUT_INGRESS_ADDR),
            CHAIN_INGRESS_TOKEN,
            Some("*.relay.test"),
        )),
        &[],
    )?;
    let (node_a_shutdown_tx, node_a_shutdown_rx) = broadcast::channel(1);
    let node_a = tokio::spawn(async move {
        common::run_http_tunnel_from_config(&node_a_config, node_a_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_tcp(FANOUT_NODE_A_ADDR, Duration::from_secs(10)).await?;

    let mut leaves = Vec::new();
    for (file, name, port, domain, addr) in [
        (
            "http_tunnel_fanout_b1.toml",
            "fanout-b1",
            FANOUT_NODE_B1_PORT,
            "one.relay.test",
            FANOUT_NODE_B1_ADDR,
        ),
        (
            "http_tunnel_fanout_b2.toml",
            "fanout-b2",
            FANOUT_NODE_B2_PORT,
            "two.relay.test",
            FANOUT_NODE_B2_ADDR,
        ),
    ] {
        let config = write_node_config(
            file,
            name,
            port,
            CHAIN_NODE_B_TOKEN,
            Some((
                &format!("ws://{}", FANOUT_NODE_A_ADDR),
                CHAIN_NODE_A_TOKEN,
                Some(domain),
            )),
            &[(domain, FANOUT_ECHO_ADDR)],
        )?;
        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
        let node = tokio::spawn(async move {
            common::run_http_tunnel_from_config(&config, shutdown_rx)
                .await
                .unwrap();
        });
        wait_for_tcp(addr, Duration::from_secs(10)).await?;
        leaves.push((shutdown_tx, node));
    }

    info!("both leaf domains are served through the same relay");
    wait_for_echo(
        FANOUT_INGRESS_ADDR,
        "one.relay.test",
        Duration::from_secs(20),
    )
    .await?;
    wait_for_echo(
        FANOUT_INGRESS_ADDR,
        "two.relay.test",
        Duration::from_secs(20),
    )
    .await?;

    for (shutdown_tx, node) in leaves {
        shutdown_tx.send(true)?;
        let _ = tokio::join!(node);
    }
    node_a_shutdown_tx.send(true)?;
    ingress_shutdown_tx.send(true)?;
    let _ = tokio::join!(node_a, ingress);
    Ok(())
}

// ==== Route mirroring ====

const MIRROR_ECHO_ADDR: &str = "127.0.0.1:2640";
const MIRROR_INGRESS_PORT: u16 = 2641;
const MIRROR_INGRESS_ADDR: &str = "127.0.0.1:2641";
const MIRROR_NODE_PORT: u16 = 2642;
const MIRROR_NODE_ADDR: &str = "127.0.0.1:2642";
const MIRROR_INGRESS_TOKEN: &str = "mirror-ingress-secret";
const MIRROR_NODE_TOKEN: &str = "mirror-node-secret";

/// A node with `mirror = true` publishes the routes of its downstream clients
/// as they appear and withdraws them when they disappear. The ingress sees the
/// real domain set, so a stopped leaf is answered with `404` at the ingress
/// instead of being forwarded into the relay.
#[tokio::test]
async fn node_mirror_withdraws_and_reclaims_routes() -> Result<()> {
    init();
    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(MIRROR_ECHO_ADDR).await {
            panic!("Failed to run the echo server for testing: {:?}", e);
        }
    });

    let (ingress_shutdown_tx, ingress_shutdown_rx) = broadcast::channel(1);
    let ingress = tokio::spawn(async move {
        run_http_tunnel_server(
            MIRROR_INGRESS_PORT,
            "127.0.0.1",
            MIRROR_INGRESS_TOKEN,
            ingress_shutdown_rx,
        )
        .await
        .unwrap();
    });
    wait_for_tcp(MIRROR_INGRESS_ADDR, Duration::from_secs(10)).await?;

    let node_config = write_node_config(
        "http_tunnel_mirror_node.toml",
        "mirror-relay",
        MIRROR_NODE_PORT,
        MIRROR_NODE_TOKEN,
        Some((
            &format!("ws://{}", MIRROR_INGRESS_ADDR),
            MIRROR_INGRESS_TOKEN,
            None,
        )),
        // A local proxy is part of the initial route set and is mirrored even
        // before any downstream client connects.
        &[("proxy.mirror.test", MIRROR_ECHO_ADDR)],
    )?;
    let (node_shutdown_tx, node_shutdown_rx) = broadcast::channel(1);
    let node = tokio::spawn(async move {
        common::run_http_tunnel_from_config(&node_config, node_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_tcp(MIRROR_NODE_ADDR, Duration::from_secs(10)).await?;

    info!("the local proxy is mirrored from the start");
    wait_for_echo(
        MIRROR_INGRESS_ADDR,
        "proxy.mirror.test",
        Duration::from_secs(20),
    )
    .await?;

    // The leaf client connects to the node after the node started, so the
    // routes are discovered dynamically and mirrored.
    let leaf_config = write_leaf_config(
        "http_tunnel_mirror_leaf.toml",
        "mirror-leaf",
        "127.0.0.1",
        MIRROR_NODE_PORT,
        MIRROR_NODE_TOKEN,
        0,
        &[
            ("one.mirror.test", MIRROR_ECHO_ADDR),
            ("two.mirror.test", MIRROR_ECHO_ADDR),
            // A wildcard route is mirrored as a wildcard.
            ("*.wild.mirror.test", MIRROR_ECHO_ADDR),
        ],
    )?;
    let (leaf_shutdown_tx, leaf_shutdown_rx) = broadcast::channel(1);
    let leaf_path = leaf_config.clone();
    let leaf = tokio::spawn(async move {
        run_http_tunnel_from_config(&leaf_path, leaf_shutdown_rx)
            .await
            .unwrap();
    });

    info!("the mirror publishes both leaf domains to the ingress");
    wait_for_echo(
        MIRROR_INGRESS_ADDR,
        "one.mirror.test",
        Duration::from_secs(20),
    )
    .await?;
    wait_for_echo(
        MIRROR_INGRESS_ADDR,
        "two.mirror.test",
        Duration::from_secs(20),
    )
    .await?;
    wait_for_echo(
        MIRROR_INGRESS_ADDR,
        "a.wild.mirror.test",
        Duration::from_secs(20),
    )
    .await?;

    info!("stopping the leaf withdraws both mirrored routes");
    leaf_shutdown_tx.send(true)?;
    let _ = tokio::join!(leaf);
    wait_for_404(MIRROR_INGRESS_ADDR, "one.mirror.test").await?;
    wait_for_404(MIRROR_INGRESS_ADDR, "two.mirror.test").await?;
    wait_for_404(MIRROR_INGRESS_ADDR, "a.wild.mirror.test").await?;
    // The proxy of the node itself was not withdrawn with the leaf.
    wait_for_echo(
        MIRROR_INGRESS_ADDR,
        "proxy.mirror.test",
        Duration::from_secs(20),
    )
    .await?;

    info!("restarting the leaf mirrors the routes again");
    let (leaf_shutdown_tx, leaf_shutdown_rx) = broadcast::channel(1);
    let leaf_path = leaf_config.clone();
    let leaf = tokio::spawn(async move {
        run_http_tunnel_from_config(&leaf_path, leaf_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_echo(
        MIRROR_INGRESS_ADDR,
        "one.mirror.test",
        Duration::from_secs(20),
    )
    .await?;

    info!("an ingress restart is followed by the mirrored proxies");
    ingress_shutdown_tx.send(true)?;
    let _ = tokio::join!(ingress);
    let (ingress_shutdown_tx, ingress_shutdown_rx) = broadcast::channel(1);
    let ingress = tokio::spawn(async move {
        run_http_tunnel_server(
            MIRROR_INGRESS_PORT,
            "127.0.0.1",
            MIRROR_INGRESS_TOKEN,
            ingress_shutdown_rx,
        )
        .await
        .unwrap();
    });
    wait_for_tcp(MIRROR_INGRESS_ADDR, Duration::from_secs(10)).await?;
    wait_for_echo(
        MIRROR_INGRESS_ADDR,
        "one.mirror.test",
        Duration::from_secs(20),
    )
    .await?;

    leaf_shutdown_tx.send(true)?;
    node_shutdown_tx.send(true)?;
    ingress_shutdown_tx.send(true)?;
    let _ = tokio::join!(leaf, node, ingress);
    Ok(())
}

// ==== Route-mirroring churn ====

const CHURN_ECHO_ADDR: &str = "127.0.0.1:2650";
const CHURN_INGRESS_PORT: u16 = 2651;
const CHURN_INGRESS_ADDR: &str = "127.0.0.1:2651";
const CHURN_NODE_PORT: u16 = 2652;
const CHURN_NODE_ADDR: &str = "127.0.0.1:2652";
const CHURN_HOST: &str = "churn.mirror.test";

/// Repeatedly connecting and disconnecting a leaf must neither leak a mirrored
/// claim nor leave a stale route at the ingress.
#[tokio::test]
async fn node_mirror_churn_is_idempotent() -> Result<()> {
    init();
    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(CHURN_ECHO_ADDR).await {
            panic!("Failed to run the echo server for testing: {:?}", e);
        }
    });

    let (ingress_shutdown_tx, ingress_shutdown_rx) = broadcast::channel(1);
    let ingress = tokio::spawn(async move {
        run_http_tunnel_server(
            CHURN_INGRESS_PORT,
            "127.0.0.1",
            CHAIN_INGRESS_TOKEN,
            ingress_shutdown_rx,
        )
        .await
        .unwrap();
    });
    wait_for_tcp(CHURN_INGRESS_ADDR, Duration::from_secs(10)).await?;

    let node_config = write_node_config(
        "http_tunnel_churn_node.toml",
        "churn-relay",
        CHURN_NODE_PORT,
        CHAIN_NODE_A_TOKEN,
        Some((
            &format!("ws://{}", CHURN_INGRESS_ADDR),
            CHAIN_INGRESS_TOKEN,
            None,
        )),
        &[],
    )?;
    let (node_shutdown_tx, node_shutdown_rx) = broadcast::channel(1);
    let node = tokio::spawn(async move {
        common::run_http_tunnel_from_config(&node_config, node_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_tcp(CHURN_NODE_ADDR, Duration::from_secs(10)).await?;

    let leaf_config = write_leaf_config(
        "http_tunnel_churn_leaf.toml",
        "churn-leaf",
        "127.0.0.1",
        CHURN_NODE_PORT,
        CHAIN_NODE_A_TOKEN,
        0,
        &[(CHURN_HOST, CHURN_ECHO_ADDR)],
    )?;

    for cycle in 1..=3 {
        let (leaf_shutdown_tx, leaf_shutdown_rx) = broadcast::channel(1);
        let leaf_path = leaf_config.clone();
        let leaf = tokio::spawn(async move {
            run_http_tunnel_from_config(&leaf_path, leaf_shutdown_rx)
                .await
                .unwrap();
        });
        info!("churn cycle {}: the mirrored route is claimed", cycle);
        wait_for_echo(CHURN_INGRESS_ADDR, CHURN_HOST, Duration::from_secs(20)).await?;

        leaf_shutdown_tx.send(true)?;
        let _ = tokio::join!(leaf);
        info!("churn cycle {}: the mirrored route is withdrawn", cycle);
        wait_for_404(CHURN_INGRESS_ADDR, CHURN_HOST).await?;
    }

    node_shutdown_tx.send(true)?;
    ingress_shutdown_tx.send(true)?;
    let _ = tokio::join!(node, ingress);
    Ok(())
}

// ==== A WebSocket upgrade across the chain ====

const CHAIN_WS_ECHO_ADDR: &str = "127.0.0.1:2620";
const CHAIN_WS_INGRESS_PORT: u16 = 2621;
const CHAIN_WS_INGRESS_ADDR: &str = "127.0.0.1:2621";
const CHAIN_WS_NODE_A_PORT: u16 = 2622;
const CHAIN_WS_NODE_A_ADDR: &str = "127.0.0.1:2622";
const CHAIN_WS_NODE_B_PORT: u16 = 2623;
const CHAIN_WS_NODE_B_ADDR: &str = "127.0.0.1:2623";
const CHAIN_WS_HOST: &str = "ws.relay.test";

/// A WebSocket echo service, the local target of the leaf node.
async fn websocket_echo_server(addr: &'static str) {
    use futures_util::{SinkExt, StreamExt};
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        tokio::spawn(async move {
            let mut ws = match tokio_tungstenite::accept_async(stream).await {
                std::result::Result::Ok(ws) => ws,
                Err(_) => return,
            };
            while let Some(std::result::Result::Ok(message)) = ws.next().await {
                if (message.is_text() || message.is_binary()) && ws.send(message).await.is_err() {
                    break;
                }
            }
        });
    }
}

/// The upgrade of a visitor is opaque to every hop: the chain pipes the
/// request and the `101` response through, then the WebSocket frames.
#[tokio::test]
async fn node_chain_forwards_websocket_upgrades() -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue, Message};

    init();
    tokio::spawn(websocket_echo_server(CHAIN_WS_ECHO_ADDR));

    let (ingress_shutdown_tx, ingress_shutdown_rx) = broadcast::channel(1);
    let ingress = tokio::spawn(async move {
        run_http_tunnel_server(
            CHAIN_WS_INGRESS_PORT,
            "127.0.0.1",
            CHAIN_INGRESS_TOKEN,
            ingress_shutdown_rx,
        )
        .await
        .unwrap();
    });
    wait_for_tcp(CHAIN_WS_INGRESS_ADDR, Duration::from_secs(10)).await?;

    let node_a_config = write_node_config(
        "http_tunnel_node_ws_a.toml",
        "ws-relay-a",
        CHAIN_WS_NODE_A_PORT,
        CHAIN_NODE_A_TOKEN,
        Some((
            &format!("ws://{}", CHAIN_WS_INGRESS_ADDR),
            CHAIN_INGRESS_TOKEN,
            Some("*.relay.test"),
        )),
        &[],
    )?;
    let (node_a_shutdown_tx, node_a_shutdown_rx) = broadcast::channel(1);
    let node_a = tokio::spawn(async move {
        common::run_http_tunnel_from_config(&node_a_config, node_a_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_tcp(CHAIN_WS_NODE_A_ADDR, Duration::from_secs(10)).await?;

    let node_b_config = write_node_config(
        "http_tunnel_node_ws_b.toml",
        "ws-relay-b",
        CHAIN_WS_NODE_B_PORT,
        CHAIN_NODE_B_TOKEN,
        Some((
            &format!("ws://{}", CHAIN_WS_NODE_A_ADDR),
            CHAIN_NODE_A_TOKEN,
            Some(CHAIN_WS_HOST),
        )),
        &[(CHAIN_WS_HOST, CHAIN_WS_ECHO_ADDR)],
    )?;
    let (node_b_shutdown_tx, node_b_shutdown_rx) = broadcast::channel(1);
    let node_b = tokio::spawn(async move {
        common::run_http_tunnel_from_config(&node_b_config, node_b_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_tcp(CHAIN_WS_NODE_B_ADDR, Duration::from_secs(10)).await?;

    // Wait until node B has claimed its route on node A, and node A on the
    // ingress. The leaf is a WebSocket-only service, so a successful upgrade is
    // also the readiness check; a 404 from the ingress is retried.
    let deadline = Instant::now() + Duration::from_secs(20);
    let (mut ws, response) = loop {
        let mut request = format!("ws://{}/ws", CHAIN_WS_INGRESS_ADDR).into_client_request()?;
        request
            .headers_mut()
            .insert("Host", HeaderValue::from_static(CHAIN_WS_HOST));

        match time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async(request),
        )
        .await
        {
            std::result::Result::Ok(std::result::Result::Ok(pair)) => break pair,
            std::result::Result::Ok(std::result::Result::Err(e)) => {
                debug!("the WebSocket upgrade through the chain failed: {}", e)
            }
            std::result::Result::Err(_) => {
                debug!("the WebSocket upgrade through the chain timed out")
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("the WebSocket upgrade through the chain did not succeed in time");
        }
        time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(response.status().as_u16(), 101);

    ws.send(Message::Text("hello through three hops".to_string()))
        .await?;
    let echoed = time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("the echoed frame did not arrive")
        .expect("the WebSocket closed before echoing")?;
    assert_eq!(
        echoed,
        Message::Text("hello through three hops".to_string())
    );

    let _ = ws.close(None).await;
    // Closing the visitor TCP connection lets the ingress end the relay, so the
    // shutdown below does not wait for the drain timeout.
    drop(ws);
    time::sleep(Duration::from_millis(200)).await;

    node_b_shutdown_tx.send(true)?;
    node_a_shutdown_tx.send(true)?;
    ingress_shutdown_tx.send(true)?;
    let _ = tokio::join!(node_b, node_a, ingress);
    Ok(())
}
