use anyhow::{Ok, Result};
use common::run_http_tunnel_from_config;
use rand::Rng;
use std::time::{Duration, Instant};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
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
const DIRECT_HTTP_ENTRY_PORT: u16 = 2494;
const DIRECT_ENTRY_ADDR: &str = "127.0.0.1:2494";
const DIRECT_ECHO_SERVER_ADDR: &str = "127.0.0.1:8096";
const DIRECT_HOST: &str = "direct.test";
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
        "name = \"{}\"\n\n[server]\nport = {}\ndomain = \"127.0.0.1\"\ntoken = \"{}\"\n{}\
         \n[upstream]\nurl = \"ws://{}:{}\"\ntoken = \"{}\"\n",
        name, listen_port, token, proxies_toml, upstream_host, upstream_port, token
    );

    let path = std::env::temp_dir().join(file);
    std::fs::write(&path, content)?;
    Ok(path.to_str().unwrap().to_string())
}

/// Add `allow_modify = true` to the `[upstream]` table of a config written by
/// [`write_leaf_config`].
fn enable_upstream_modify(path: &str) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let marker = "[upstream]\n";
    let index = content
        .find(marker)
        .unwrap_or_else(|| panic!("no [upstream] table in {}", path));
    let insert_at = index + marker.len();
    let mut updated = String::with_capacity(content.len() + 24);
    updated.push_str(&content[..insert_at]);
    updated.push_str("allow_modify = true\n");
    updated.push_str(&content[insert_at..]);
    std::fs::write(path, updated)?;
    Ok(())
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

/// A leaf with no `[server]` never binds a port. The v4 stream must connect
/// straight to the local proxy target instead of a loopback listener.
#[tokio::test]
async fn leaf_without_server_connects_directly_to_local_proxies() -> Result<()> {
    init();

    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(DIRECT_ECHO_SERVER_ADDR).await {
            panic!("Failed to run the echo server for testing: {:?}", e);
        }
    });

    let path = std::env::temp_dir().join(format!(
        "http_tunnel_direct_leaf_{}.toml",
        std::process::id()
    ));
    std::fs::write(
        &path,
        format!(
            "name = \"direct-leaf\"\n\n\
             [[proxies]]\nname = \"echo\"\ndomain = \"{DIRECT_HOST}\"\n\
             local_addr = \"{DIRECT_ECHO_SERVER_ADDR}\"\n\n\
             [upstream]\nurl = \"ws://127.0.0.1:{DIRECT_HTTP_ENTRY_PORT}\"\n\
             token = \"{LEAF_TOKEN}\"\nretry_interval = 1\n"
        ),
    )?;

    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);
    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);

    let server = tokio::spawn(async move {
        run_http_tunnel_server(
            DIRECT_HTTP_ENTRY_PORT,
            "127.0.0.1",
            LEAF_TOKEN,
            server_shutdown_rx,
        )
        .await
        .unwrap();
    });
    wait_for_tcp(DIRECT_ENTRY_ADDR, Duration::from_secs(10)).await?;

    let client = tokio::spawn(async move {
        run_http_tunnel_from_config(path.to_str().unwrap(), client_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_echo(DIRECT_ENTRY_ADDR, DIRECT_HOST, Duration::from_secs(20)).await?;

    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
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

/// Send one visitor request with an optional `X-HTTP-TUNNEL-TOKEN` and return
/// the status line and the first chunk of the response.
async fn visitor_status(addr: &str, host: &str, token: Option<&str>) -> Result<(u16, String)> {
    let mut conn = TcpStream::connect(addr).await?;
    let auth = token
        .map(|token| format!("X-HTTP-TUNNEL-TOKEN: {}\r\n", token))
        .unwrap_or_default();
    let request = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\n{}Connection: close\r\n\r\n",
        host, auth
    );
    conn.write_all(request.as_bytes()).await?;

    let mut buffer = [0u8; 8192];
    let read = time::timeout(Duration::from_secs(10), conn.read(&mut buffer)).await??;
    let response = String::from_utf8_lossy(&buffer[..read]).to_string();
    // A rejected visitor gets an `ht` error page. An accepted one reaches the
    // raw echo backend in these tests, which sends the request back verbatim
    // without a status line.
    let status = if response.starts_with("HTTP/1.1 ") {
        response
            .split_whitespace()
            .nth(1)
            .and_then(|status| status.parse().ok())
            .unwrap_or(0)
    } else if response.starts_with("GET ") {
        200
    } else {
        0
    };
    Ok((status, response))
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
    // The error names the host and a request id, so that the operator can find
    // the matching log line. A response without them did not come from ht.
    assert!(
        text.contains("No tunnel for this host:"),
        "unexpected response: {}",
        text
    );
    assert!(
        text.to_ascii_lowercase()
            .contains("\r\nx-http-tunnel-request-id: "),
        "the 404 should carry a request id: {}",
        text
    );
    assert!(
        text.contains("Request ID: "),
        "the 404 body should carry a request id: {}",
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

const DOWNSTREAM_ECHO_SERVER_ADDR: &str = "127.0.0.1:8094";
const DOWNSTREAM_HTTP_ENTRY_PORT: u16 = 2474;
const DOWNSTREAM_HTTP_ENTRY_ADDR: &str = "127.0.0.1:2474";
const DOWNSTREAM_LEAF_ADDR: &str = "127.0.0.1:2475";
const DOWNSTREAM_LEAF_PORT: u16 = 2475;
const DOWNSTREAM_TOKEN: &str = "a_secret_token";
const DOWNSTREAM_NODE: &str = "remote-leaf";
const DOWNSTREAM_HOST: &str = "remote.test";

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

    // A loopback visitor may hand in a request id. The ingress carries it to
    // the leaf, so the 502 the leaf generates names the same id the ingress
    // logged: that is what lets one grep find the failing hop.
    const VISITOR_REQUEST_ID: &str = "0123456789abcdef";
    let mut visitor = TcpStream::connect(WS_FAILURE_HTTP_ADDR).await?;
    visitor
        .write_all(
            format!(
                "GET / HTTP/1.1\r\nHost: unavailable-ws.test\r\n\
                 x-http-tunnel-request-id: {}\r\nConnection: close\r\n\r\n",
                VISITOR_REQUEST_ID
            )
            .as_bytes(),
        )
        .await?;
    let mut response = Vec::new();
    time::timeout(Duration::from_secs(18), visitor.read_to_end(&mut response)).await??;
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 502"),
        "expected 502 for an unavailable local target, got {}",
        response
    );
    // The page must say which layer failed and which node/target to look at,
    // so a bare 502 does not send the operator to the wrong hop.
    for expected in [
        "failed layer:",
        "local service",
        "node:",
        LEAF_NAME,
        "target:",
        WS_FAILURE_TARGET_ADDR,
        "Request ID: 0123456789abcdef",
        "x-http-tunnel-request-id: 0123456789abcdef",
    ] {
        assert!(
            response.contains(expected),
            "the 502 should name `{}`, got {}",
            expected,
            response
        );
    }
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

    info!("the management API exposes the node settings");
    let (status, body) = api_request(
        MANAGEMENT_ADDR,
        "GET",
        "/api/settings",
        Some(MANAGEMENT_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(
        body.contains("\"name\":\"home\""),
        "unexpected body: {}",
        body
    );
    assert!(body.contains("\"upstream\""), "unexpected body: {}", body);
    assert!(body.contains(MANAGEMENT_TOKEN), "unexpected body: {}", body);
    assert!(!body.contains("\"proxies\""), "unexpected body: {}", body);

    info!("a settings dry run reports the restart without writing the file");
    let changed = body.replace("\"name\":\"home\"", "\"name\":\"renamed\"");
    let (status, preview) = api_request(
        MANAGEMENT_ADDR,
        "PUT",
        "/api/settings?dry_run=true",
        Some(MANAGEMENT_TOKEN),
        Some(&changed),
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", preview);
    assert!(
        preview.contains("\"restart_required\":true"),
        "unexpected body: {}",
        preview
    );
    assert!(
        preview.contains("\"saved\":false"),
        "unexpected body: {}",
        preview
    );
    let saved = std::fs::read_to_string(&config_path)?;
    assert!(
        saved.contains("name = \"home\""),
        "a dry run must not write the config: {}",
        saved
    );

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

    info!("the status counts the visitor connection");
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
        body.contains("\"total_visitors\":"),
        "the dashboard should expose the visitor total: {}",
        body
    );

    info!("the probe dials the saved local service");
    let (status, body) = api_request(
        MANAGEMENT_ADDR,
        "POST",
        "/api/proxies/echo/probe",
        Some(MANAGEMENT_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(body.contains("\"ok\":true"), "unexpected body: {}", body);
    assert!(
        body.contains("\"latency_ms\":"),
        "unexpected body: {}",
        body
    );

    let (status, _) = api_request(
        MANAGEMENT_ADDR,
        "POST",
        "/api/proxies/no-such-service/probe",
        Some(MANAGEMENT_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 404, "an unknown service must be a 404");

    info!("the probe reports an unreachable target without failing the API");
    let closed_port = unused_port()?;
    let body = format!(
        r#"{{"domain":"probe-fail.test","local_addr":"127.0.0.1:{}"}}"#,
        closed_port
    );
    let (status, _) = api_request(
        MANAGEMENT_ADDR,
        "PUT",
        "/api/proxies/probe-fail",
        Some(MANAGEMENT_TOKEN),
        Some(&body),
    )
    .await?;
    assert_eq!(status, 201);
    let (status, body) = api_request(
        MANAGEMENT_ADDR,
        "POST",
        "/api/proxies/probe-fail/probe",
        Some(MANAGEMENT_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(body.contains("\"ok\":false"), "unexpected body: {}", body);
    let (status, _) = api_request(
        MANAGEMENT_ADDR,
        "DELETE",
        "/api/proxies/probe-fail",
        Some(MANAGEMENT_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 204);

    info!("the configuration can be exported as a backup");
    let (status, body) = api_request(
        MANAGEMENT_ADDR,
        "GET",
        "/api/settings/export",
        Some(MANAGEMENT_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(body.contains("echo.test"), "unexpected body: {}", body);
    assert!(body.contains("token ="), "unexpected body: {}", body);
    let (status, _) =
        api_request(MANAGEMENT_ADDR, "GET", "/api/settings/export", None, None).await?;
    assert_eq!(status, 401, "export must require the token");

    info!("the management API enables visitor authentication");
    let (status, body) = api_request(
        MANAGEMENT_ADDR,
        "PUT",
        "/api/proxies/echo",
        Some(MANAGEMENT_TOKEN),
        Some(
            r#"{"domain":"echo.test","local_addr":"127.0.0.1:8093","auth_header":"visitor-secret"}"#,
        ),
    )
    .await?;
    assert_eq!(status, 201, "unexpected response: {}", body);
    assert!(
        body.contains("\"auth_header\":\"visitor-secret\""),
        "unexpected body: {}",
        body
    );

    let (status, body) = api_request(
        MANAGEMENT_ADDR,
        "GET",
        "/api/proxies",
        Some(MANAGEMENT_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(
        body.contains("\"auth_header\":\"visitor-secret\""),
        "the API should return the visitor token: {}",
        body
    );

    let (status, _) = visitor_status(MANAGEMENT_HTTP_ENTRY_ADDR, "echo.test", None).await?;
    assert_eq!(status, 401, "a visitor without the token must be rejected");
    let (status, _) = visitor_status(
        MANAGEMENT_HTTP_ENTRY_ADDR,
        "echo.test",
        Some("wrong-secret"),
    )
    .await?;
    assert_eq!(status, 401, "a wrong token must be rejected");
    let (status, response) = visitor_status(
        MANAGEMENT_HTTP_ENTRY_ADDR,
        "echo.test",
        Some("visitor-secret"),
    )
    .await?;
    assert_eq!(status, 200, "the visitor token should be accepted");
    assert!(
        !response.contains("X-HTTP-TUNNEL-TOKEN"),
        "the token must be stripped before the backend: {}",
        response
    );

    info!("replacing the proxy without auth_header clears the gate");
    let (status, body) = api_request(
        MANAGEMENT_ADDR,
        "PUT",
        "/api/proxies/echo",
        Some(MANAGEMENT_TOKEN),
        Some(r#"{"domain":"echo.test","local_addr":"127.0.0.1:8093"}"#),
    )
    .await?;
    assert_eq!(status, 201, "unexpected response: {}", body);
    assert!(
        body.contains("\"auth_header\":null"),
        "unexpected body: {}",
        body
    );
    let (status, _) = visitor_status(MANAGEMENT_HTTP_ENTRY_ADDR, "echo.test", None).await?;
    assert_eq!(status, 200, "clearing auth_header should reopen the proxy");

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

    info!("the entry node lists the connected downstream node");
    let (status, body) = api_request(
        MANAGEMENT_HTTP_ENTRY_ADDR,
        "GET",
        "/api/sessions",
        Some(MANAGEMENT_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(
        body.contains("\"node\":\"home\""),
        "unexpected body: {}",
        body
    );
    assert!(
        body.contains("\"peer\":\"127.0.0.1:"),
        "the session should report the downstream peer address: {}",
        body
    );
    assert!(
        body.contains("\"role\":\"node\""),
        "unexpected body: {}",
        body
    );
    assert!(
        body.contains(&format!("\"version\":\"{}\"", env!("CARGO_PKG_VERSION"))),
        "the session should report the downstream software version: {}",
        body
    );
    assert!(
        body.contains("\"domains\":[\"echo.test\"]"),
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

    info!("the management API reports the effective route table");
    let (status, body) = api_request(
        MANAGEMENT_ADDR,
        "GET",
        "/api/routes",
        Some(MANAGEMENT_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(
        body.contains("\"domain\":\"echo.test\""),
        "unexpected body: {}",
        body
    );
    assert!(
        body.contains("\"owner\":\"local\""),
        "unexpected body: {}",
        body
    );
    assert!(
        body.contains("\"name\":\"echo\""),
        "unexpected body: {}",
        body
    );

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

    info!("an unknown management path names the path and a request id");
    let (status, body) = api_request(
        MANAGEMENT_ADDR,
        "GET",
        "/api/no-such-endpoint",
        Some(MANAGEMENT_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 404, "unexpected response: {}", body);
    assert!(
        body.contains("No such management API endpoint: GET /api/no-such-endpoint"),
        "unexpected body: {}",
        body
    );
    assert!(
        body.contains("\"request_id\":"),
        "the fallback should carry a request id: {}",
        body
    );

    info!("the management API restarts the runtime");
    let (status, body) = api_request(
        MANAGEMENT_ADDR,
        "POST",
        "/api/restart",
        Some(MANAGEMENT_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(
        body.contains("\"restarting\":true"),
        "unexpected body: {}",
        body
    );

    info!("the restarted runtime answers again on the same port");
    time::sleep(Duration::from_secs(2)).await;
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

    info!("shutdown the server and the client");
    server_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(server, client);

    Ok(())
}

/// The entry management API manages the local `[[proxies]]` of a connected
/// downstream node through the same `/api/proxies` endpoints, and can close
/// that session.
#[cfg(feature = "management")]
#[tokio::test]
async fn management_api_manages_downstream_proxies() -> Result<()> {
    init();

    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(DOWNSTREAM_ECHO_SERVER_ADDR).await {
            panic!("Failed to run the downstream echo server: {:?}", e);
        }
    });

    let config_path = write_leaf_config(
        "http_tunnel_downstream_management.toml",
        DOWNSTREAM_NODE,
        "127.0.0.1",
        DOWNSTREAM_HTTP_ENTRY_PORT,
        DOWNSTREAM_TOKEN,
        DOWNSTREAM_LEAF_PORT,
        &[],
    )?;
    enable_upstream_modify(&config_path)?;

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    let server = tokio::spawn(async move {
        run_http_tunnel_server(
            DOWNSTREAM_HTTP_ENTRY_PORT,
            "127.0.0.1",
            DOWNSTREAM_TOKEN,
            server_shutdown_rx,
        )
        .await
        .unwrap();
    });
    wait_for_tcp(DOWNSTREAM_HTTP_ENTRY_ADDR, Duration::from_secs(10)).await?;

    let client_config = config_path.clone();
    let client = tokio::spawn(async move {
        run_http_tunnel_from_config(&client_config, client_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_tcp(DOWNSTREAM_LEAF_ADDR, Duration::from_secs(10)).await?;
    time::sleep(Duration::from_millis(1500)).await;

    info!("the entry lists the downstream node as manageable");
    let (status, body) = api_request(
        DOWNSTREAM_HTTP_ENTRY_ADDR,
        "GET",
        "/api/sessions",
        Some(DOWNSTREAM_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(
        body.contains("\"node\":\"remote-leaf\""),
        "unexpected body: {}",
        body
    );
    assert!(
        body.contains("\"manageable\":true") && body.contains("\"writable\":true"),
        "the session should be manageable: {}",
        body
    );
    let session_id = session_id_for_node(&body, DOWNSTREAM_NODE)
        .unwrap_or_else(|| panic!("no session id in {}", body));

    info!("the entry lists the downstream proxies");
    let (status, body) = api_request(
        DOWNSTREAM_HTTP_ENTRY_ADDR,
        "GET",
        &format!(
            "/api/proxies?node={}&session={}",
            DOWNSTREAM_NODE, session_id
        ),
        Some(DOWNSTREAM_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert_eq!(body.trim(), "[]", "unexpected body: {}", body);

    info!("the entry adds a proxy on the downstream node");
    let (status, body) = api_request(
        DOWNSTREAM_HTTP_ENTRY_ADDR,
        "PUT",
        &format!(
            "/api/proxies/remote?node={}&session={}",
            DOWNSTREAM_NODE, session_id
        ),
        Some(DOWNSTREAM_TOKEN),
        Some(r#"{"domain":"remote.test","local_addr":"127.0.0.1:8094"}"#),
    )
    .await?;
    assert_eq!(status, 201, "unexpected response: {}", body);
    assert!(body.contains("remote.test"), "unexpected body: {}", body);
    assert!(body.contains("127.0.0.1:8094"), "unexpected body: {}", body);

    info!("the downstream route is served without a restart");
    time::sleep(Duration::from_millis(500)).await;
    http_echo_hitter(DOWNSTREAM_HTTP_ENTRY_ADDR, DOWNSTREAM_HOST).await?;

    info!("the entry sets auth_header on the downstream proxy");
    let (status, body) = api_request(
        DOWNSTREAM_HTTP_ENTRY_ADDR,
        "PUT",
        &format!(
            "/api/proxies/remote?node={}&session={}",
            DOWNSTREAM_NODE, session_id
        ),
        Some(DOWNSTREAM_TOKEN),
        Some(
            r#"{"domain":"remote.test","local_addr":"127.0.0.1:8094","auth_header":"remote-visitor"}"#,
        ),
    )
    .await?;
    assert_eq!(status, 201, "unexpected response: {}", body);
    assert!(
        body.contains("\"auth_header\":\"remote-visitor\""),
        "unexpected body: {}",
        body
    );

    let (status, _) = visitor_status(DOWNSTREAM_HTTP_ENTRY_ADDR, DOWNSTREAM_HOST, None).await?;
    assert_eq!(
        status, 401,
        "the downstream gate must reject a missing token"
    );
    let (status, response) = visitor_status(
        DOWNSTREAM_HTTP_ENTRY_ADDR,
        DOWNSTREAM_HOST,
        Some("remote-visitor"),
    )
    .await?;
    assert_eq!(status, 200, "the downstream gate must accept the token");
    assert!(
        !response.contains("X-HTTP-TUNNEL-TOKEN"),
        "the downstream token must be stripped: {}",
        response
    );

    let (status, body) = api_request(
        DOWNSTREAM_HTTP_ENTRY_ADDR,
        "GET",
        &format!("/api/proxies?node={}", DOWNSTREAM_NODE),
        Some(DOWNSTREAM_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(
        body.contains("\"auth_header\":\"remote-visitor\""),
        "the downstream list should return the visitor token: {}",
        body
    );

    info!("the downstream node wrote the change to its config");
    let written = std::fs::read_to_string(&config_path)?;
    assert!(
        written.contains("remote.test"),
        "the downstream config wasn't written: {}",
        written
    );
    assert!(
        written.contains("auth_header = \"remote-visitor\""),
        "the downstream auth_header wasn't written: {}",
        written
    );

    info!("the node name alone selects the same session");
    let (status, body) = api_request(
        DOWNSTREAM_HTTP_ENTRY_ADDR,
        "GET",
        &format!("/api/proxies?node={}", DOWNSTREAM_NODE),
        Some(DOWNSTREAM_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(body.contains("remote.test"), "unexpected body: {}", body);

    info!("an empty selector is rejected instead of falling back to the local list");
    let (status, _) = api_request(
        DOWNSTREAM_HTTP_ENTRY_ADDR,
        "GET",
        "/api/proxies?node=",
        Some(DOWNSTREAM_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 400, "expected 400");

    info!("an unknown selector is rejected instead of falling back to the local list");
    let (status, body) = api_request(
        DOWNSTREAM_HTTP_ENTRY_ADDR,
        "GET",
        "/api/proxies?ndoe=home",
        Some(DOWNSTREAM_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 400, "expected 400: {}", body);
    assert!(
        body.contains("Invalid proxy selector"),
        "unexpected body: {}",
        body
    );

    info!("an unknown node is rejected with 404");
    let (status, _) = api_request(
        DOWNSTREAM_HTTP_ENTRY_ADDR,
        "GET",
        "/api/proxies?node=no-such-node",
        Some(DOWNSTREAM_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 404, "expected 404");

    info!("a node and session that disagree are rejected with 400");
    let (status, _) = api_request(
        DOWNSTREAM_HTTP_ENTRY_ADDR,
        "GET",
        &format!("/api/proxies?node=wrong&session={}", session_id),
        Some(DOWNSTREAM_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 400, "expected 400");

    info!("the entry deletes the downstream proxy");
    let (status, _) = api_request(
        DOWNSTREAM_HTTP_ENTRY_ADDR,
        "DELETE",
        &format!(
            "/api/proxies/remote?node={}&session={}",
            DOWNSTREAM_NODE, session_id
        ),
        Some(DOWNSTREAM_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 204, "expected 204");
    time::sleep(Duration::from_millis(500)).await;
    http_404_check(DOWNSTREAM_HTTP_ENTRY_ADDR, DOWNSTREAM_HOST).await?;

    info!("deleting a missing downstream proxy is rejected with 404");
    let (status, _) = api_request(
        DOWNSTREAM_HTTP_ENTRY_ADDR,
        "DELETE",
        &format!(
            "/api/proxies/remote?node={}&session={}",
            DOWNSTREAM_NODE, session_id
        ),
        Some(DOWNSTREAM_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 404, "expected 404");

    info!("the entry disconnects the downstream session");
    let (status, _) = api_request(
        DOWNSTREAM_HTTP_ENTRY_ADDR,
        "DELETE",
        &format!("/api/sessions/{}", session_id),
        Some(DOWNSTREAM_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 204, "expected 204");

    let (status, body) = api_request(
        DOWNSTREAM_HTTP_ENTRY_ADDR,
        "GET",
        "/api/sessions",
        Some(DOWNSTREAM_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(
        !body.contains(&format!("\"session_id\":{}", session_id)),
        "the closed session still appears: {}",
        body
    );

    info!("disconnecting an unknown session is rejected with 404");
    let (status, _) = api_request(
        DOWNSTREAM_HTTP_ENTRY_ADDR,
        "DELETE",
        &format!("/api/sessions/{}", session_id),
        Some(DOWNSTREAM_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 404, "expected 404");

    server_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(server, client);
    Ok(())
}

// ==== The allow_upstream_modify switch test ====

const SECURE_ECHO_SERVER_ADDR: &str = "127.0.0.1:8095";
const SECURE_HTTP_ENTRY_PORT: u16 = 2484;
const SECURE_HTTP_ENTRY_ADDR: &str = "127.0.0.1:2484";
const SECURE_LEAF_PORT: u16 = 2485;
const SECURE_LEAF_ADDR: &str = "127.0.0.1:2485";
const SECURE_TOKEN: &str = "a_secure_token";
const SECURE_NODE: &str = "secure-leaf";

/// Poll `/api/sessions` until the node's session reports the expected
/// `writable` value, and return its current session id. The switch takes effect
/// by reconnecting the upstream session, so the id may change.
async fn wait_for_writable(addr: &str, token: &str, node: &str, writable: bool) -> Result<u64> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let expected = format!("\"writable\":{}", writable);
    loop {
        let (status, body) = api_request(addr, "GET", "/api/sessions", Some(token), None).await?;
        assert_eq!(status, 200, "unexpected response: {}", body);
        if let Some(id) = session_id_for_node(&body, node) {
            if body.contains(&expected) {
                return Ok(id);
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "the session of {} did not become writable={}: last body {}",
                node, writable, body
            );
        }
        time::sleep(Duration::from_millis(200)).await;
    }
}

/// `allow_upstream_modify` is off by default: the entry may list a downstream
/// node's services but cannot write them. The node's own page may turn it on and
/// off, and each change reconnects the upstream session.
#[cfg(feature = "management")]
#[tokio::test]
async fn management_api_respects_allow_upstream_modify() -> Result<()> {
    init();

    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(SECURE_ECHO_SERVER_ADDR).await {
            panic!("Failed to run the secure echo server: {:?}", e);
        }
    });

    let config_path = write_leaf_config(
        "http_tunnel_allow_upstream_modify.toml",
        SECURE_NODE,
        "127.0.0.1",
        SECURE_HTTP_ENTRY_PORT,
        SECURE_TOKEN,
        SECURE_LEAF_PORT,
        &[],
    )?;

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    let server = tokio::spawn(async move {
        run_http_tunnel_server(
            SECURE_HTTP_ENTRY_PORT,
            "127.0.0.1",
            SECURE_TOKEN,
            server_shutdown_rx,
        )
        .await
        .unwrap();
    });
    wait_for_tcp(SECURE_HTTP_ENTRY_ADDR, Duration::from_secs(10)).await?;

    let client_config = config_path.clone();
    let client = tokio::spawn(async move {
        run_http_tunnel_from_config(&client_config, client_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_tcp(SECURE_LEAF_ADDR, Duration::from_secs(10)).await?;
    let session_id =
        wait_for_writable(SECURE_HTTP_ENTRY_ADDR, SECURE_TOKEN, SECURE_NODE, false).await?;

    info!("the node reports the switch as off and editable");
    let (status, body) = api_request(
        SECURE_LEAF_ADDR,
        "GET",
        "/api/status",
        Some(SECURE_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(body.contains("\"allow_upstream_modify\":false"), "{}", body);
    assert!(
        body.contains("\"allow_upstream_modify_editable\":true"),
        "{}",
        body
    );

    info!("the entry may still list but not write the downstream services");
    let (status, body) = api_request(
        SECURE_HTTP_ENTRY_ADDR,
        "GET",
        &format!("/api/proxies?node={}&session={}", SECURE_NODE, session_id),
        Some(SECURE_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);

    let (status, body) = api_request(
        SECURE_HTTP_ENTRY_ADDR,
        "PUT",
        &format!(
            "/api/proxies/secure?node={}&session={}",
            SECURE_NODE, session_id
        ),
        Some(SECURE_TOKEN),
        Some(r#"{"domain":"secure.test","local_addr":"127.0.0.1:8095"}"#),
    )
    .await?;
    assert_eq!(status, 409, "a disabled switch must return 409: {}", body);
    let written = std::fs::read_to_string(&config_path)?;
    assert!(
        !written.contains("secure.test"),
        "a rejected write must not touch the config: {}",
        written
    );

    info!("the node turns the switch on through its own management API");
    let (status, body) = api_request(
        SECURE_LEAF_ADDR,
        "PUT",
        "/api/settings/allow-upstream-modify",
        Some(SECURE_TOKEN),
        Some(r#"{"allowed":true}"#),
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    assert!(body.contains("\"allow_upstream_modify\":true"), "{}", body);

    let writable_after =
        wait_for_writable(SECURE_HTTP_ENTRY_ADDR, SECURE_TOKEN, SECURE_NODE, true).await?;
    info!(%writable_after, "the reconnected session now allows writes");
    assert!(std::fs::read_to_string(&config_path)?.contains("allow_modify = true"));

    let (status, body) = api_request(
        SECURE_HTTP_ENTRY_ADDR,
        "PUT",
        &format!(
            "/api/proxies/secure?node={}&session={}",
            SECURE_NODE, writable_after
        ),
        Some(SECURE_TOKEN),
        Some(r#"{"domain":"secure.test","local_addr":"127.0.0.1:8095"}"#),
    )
    .await?;
    assert_eq!(status, 201, "unexpected response: {}", body);
    assert!(std::fs::read_to_string(&config_path)?.contains("secure.test"));

    info!("the node turns the switch off again");
    let (status, body) = api_request(
        SECURE_LEAF_ADDR,
        "PUT",
        "/api/settings/allow-upstream-modify",
        Some(SECURE_TOKEN),
        Some(r#"{"allowed":false}"#),
    )
    .await?;
    assert_eq!(status, 200, "unexpected response: {}", body);
    let writable_off =
        wait_for_writable(SECURE_HTTP_ENTRY_ADDR, SECURE_TOKEN, SECURE_NODE, false).await?;

    let (status, body) = api_request(
        SECURE_HTTP_ENTRY_ADDR,
        "DELETE",
        &format!(
            "/api/proxies/secure?node={}&session={}",
            SECURE_NODE, writable_off
        ),
        Some(SECURE_TOKEN),
        None,
    )
    .await?;
    assert_eq!(status, 409, "a disabled switch must return 409: {}", body);
    assert!(std::fs::read_to_string(&config_path)?.contains("secure.test"));

    info!("a standalone ingress rejects the switch with 409");
    let (status, body) = api_request(
        SECURE_HTTP_ENTRY_ADDR,
        "PUT",
        "/api/settings/allow-upstream-modify",
        Some(SECURE_TOKEN),
        Some(r#"{"allowed":true}"#),
    )
    .await?;
    assert_eq!(status, 409, "an ingress has no upstream: {}", body);

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

    // Occupy the exact loopback address the test helper binds.
    let _occupied =
        tokio::net::TcpListener::bind(("127.0.0.1", OCCUPIED_HTTP_TEST_HTTP_PORT)).await?;
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
        format!("{:#}", err).contains("[server].port"),
        "unexpected error: {:#}",
        err
    );

    Ok(())
}

/// Extract the session id of a node from the compact JSON of `/api/sessions`.
/// The session id is serialized before the node name, so the nearest preceding
/// `"session_id"` belongs to the matched node.
fn session_id_for_node(body: &str, node: &str) -> Option<u64> {
    let node_key = format!("\"node\":\"{}\"", node);
    let node_at = body.find(&node_key)?;
    let before = &body[..node_at];
    let key = "\"session_id\":";
    let start = before.rfind(key)? + key.len();
    let digits: String = before[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    digits.parse().ok()
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
/// `upstream` is `(url, token)`: `None` makes the node an ingress. The node
/// name doubles as the name announced on the upstream.
fn write_node_config(
    file: &str,
    name: &str,
    listen_port: u16,
    token: &str,
    upstream: Option<(&str, &str)>,
    proxies: &[(&str, &str)],
) -> Result<String> {
    let mut content = format!(
        "name = \"{}\"\n\n[server]\nport = {}\ndomain = \"127.0.0.1\"\ntoken = \"{}\"\n",
        name, listen_port, token
    );
    for (domain, local_addr) in proxies {
        let proxy_name = domain.split('.').next().unwrap_or(domain);
        content.push_str(&format!(
            "\n[[proxies]]\nname = \"{}\"\ndomain = \"{}\"\nlocal_addr = \"{}\"\n",
            proxy_name, domain, local_addr
        ));
    }
    if let Some((upstream_url, upstream_token)) = upstream {
        content.push_str(&format!(
            "\n[upstream]\nurl = \"{}\"\ntoken = \"{}\"\n",
            upstream_url, upstream_token
        ));
    }

    let path = std::env::temp_dir().join(file);
    std::fs::write(&path, content)?;
    Ok(path.to_str().unwrap().to_string())
}

/// `ht` ingress -> node A (mirrors downstream routes) -> node B (local proxy)
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
        Some((&format!("ws://{}", CHAIN_INGRESS_ADDR), CHAIN_INGRESS_TOKEN)),
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
        Some((&format!("ws://{}", CHAIN_NODE_A_ADDR), CHAIN_NODE_A_TOKEN)),
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
            Some((&format!("ws://{}", FANOUT_NODE_A_ADDR), CHAIN_NODE_A_TOKEN)),
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

/// A node with `[upstream]` publishes the routes of its downstream clients
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
        Some((&format!("ws://{}", CHURN_INGRESS_ADDR), CHAIN_INGRESS_TOKEN)),
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

/// Bind an ephemeral loopback port, then release it for the process under test.
fn unused_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// Write an ingress configuration with zero or one local service.
fn write_reload_config(
    path: &std::path::Path,
    listen_port: u16,
    proxy: Option<(&str, &str)>,
) -> Result<()> {
    write_reload_config_named(path, listen_port, "reload-test", proxy)
}

fn write_reload_config_named(
    path: &std::path::Path,
    listen_port: u16,
    name: &str,
    proxy: Option<(&str, &str)>,
) -> Result<()> {
    let mut content = format!(
        "name = \"{}\"\n\n[server]\nport = {}\ndomain = \"127.0.0.1\"\ntoken = \"reload-secret\"\n",
        name, listen_port
    );
    if let Some((domain, local_addr)) = proxy {
        content.push_str(&format!(
            "\n[[proxies]]\nname = \"backend\"\ndomain = \"{}\"\nlocal_addr = \"{}\"\n",
            domain, local_addr
        ));
    }
    std::fs::write(path, content)?;
    Ok(())
}

/// Send one visitor request and read the status line of whatever answers.
async fn reload_status(addr: &str, host: &str) -> Result<u16> {
    let mut conn = TcpStream::connect(addr).await?;
    let request = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        host
    );
    conn.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    let _ = time::timeout(Duration::from_secs(5), conn.read_to_end(&mut response)).await;
    let text = String::from_utf8_lossy(&response);
    Ok(text
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .unwrap_or(0))
}

/// Poll until `addr` answers `host` with `expected`, hiding startup and reload
/// races from the test.
async fn wait_for_reload_status(
    addr: &str,
    host: &str,
    expected: u16,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if let std::result::Result::Ok(status) = reload_status(addr, host).await {
            if status == expected {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("{} did not answer {} within {:?}", addr, expected, timeout);
        }
        time::sleep(Duration::from_millis(100)).await;
    }
}

/// Editing `ht.toml` must change the running process without restarting it.
///
/// A proxy-only edit is applied live; a listener-port edit has to rebuild the
/// listener and upstream session, which this test observes on the new port.
#[tokio::test]
async fn config_file_changes_are_hot_reloaded() -> Result<()> {
    init();

    // A tiny HTTP backend. It answers every request with a fixed 200.
    let backend = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend.local_addr()?.to_string();
    tokio::spawn(async move {
        while let std::result::Result::Ok((mut stream, _)) = backend.accept().await {
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request).await;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nbackend",
                    )
                    .await;
            });
        }
    });

    let path = std::env::temp_dir().join(format!(
        "http-tunnel-hot-reload-{}.toml",
        std::process::id()
    ));
    let port = unused_port()?;
    write_reload_config(&path, port, Some(("reload.test", &backend_addr)))?;

    let (stop_tx, stop_rx) = broadcast::channel(1);
    let path_string = path.to_str().unwrap().to_string();
    let node = tokio::spawn(async move {
        run_http_tunnel_from_config(&path_string, stop_rx)
            .await
            .unwrap();
    });

    let addr = format!("127.0.0.1:{}", port);
    wait_for_reload_status(&addr, "reload.test", 200, Duration::from_secs(10)).await?;

    // Removing the proxy is a live change: the same listener answers 404 now.
    write_reload_config(&path, port, None)?;
    wait_for_reload_status(&addr, "reload.test", 404, Duration::from_secs(10)).await?;

    // Changing the port rebuilds the listener and keeps the process alive.
    let mut new_port = unused_port()?;
    while new_port == port {
        new_port = unused_port()?;
    }
    write_reload_config(&path, new_port, Some(("reload.test", &backend_addr)))?;
    let new_addr = format!("127.0.0.1:{}", new_port);
    wait_for_reload_status(&new_addr, "reload.test", 200, Duration::from_secs(15)).await?;

    // A same-port change also rebuilds the server, but the listener socket is
    // reused, so the port keeps answering throughout the reload.
    write_reload_config_named(
        &path,
        new_port,
        "reload-test-2",
        Some(("reload-2.test", &backend_addr)),
    )?;
    wait_for_reload_status(&new_addr, "reload-2.test", 200, Duration::from_secs(10)).await?;
    wait_for_reload_status(&new_addr, "reload.test", 404, Duration::from_secs(10)).await?;

    info!("an invalid file is visible in /api/status while the old config keeps serving");
    std::fs::write(&path, "listen_port = not-a-port\n")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (status, body) =
            api_request(&new_addr, "GET", "/api/status", Some("reload-secret"), None).await?;
        if status == 200 && body.contains("last_reload_error") {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("the invalid configuration was not reported: {}", body);
        }
        time::sleep(Duration::from_millis(100)).await;
    }
    wait_for_reload_status(&new_addr, "reload-2.test", 200, Duration::from_secs(5)).await?;

    info!("fixing the file clears the reload error");
    write_reload_config_named(
        &path,
        new_port,
        "reload-test-2",
        Some(("reload-2.test", &backend_addr)),
    )?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (status, body) =
            api_request(&new_addr, "GET", "/api/status", Some("reload-secret"), None).await?;
        if status == 200 && !body.contains("last_reload_error") {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("the reload error was not cleared: {}", body);
        }
        time::sleep(Duration::from_millis(100)).await;
    }

    stop_tx.send(true)?;
    let _ = time::timeout(Duration::from_secs(10), node).await;
    let _ = std::fs::remove_file(&path);
    Ok(())
}

/// Changing `bind_host` on the same port releases the old listener before it
/// binds the new address, and a bad address rolls back to the running config.
#[tokio::test]
async fn bind_host_changes_are_hot_reloaded_and_rolled_back() -> Result<()> {
    init();

    let backend = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend.local_addr()?.to_string();
    tokio::spawn(async move {
        while let std::result::Result::Ok((mut stream, _)) = backend.accept().await {
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request).await;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nbackend",
                    )
                    .await;
            });
        }
    });

    let path = std::env::temp_dir().join(format!(
        "http-tunnel-bind-host-reload-{}.toml",
        std::process::id()
    ));
    let port = unused_port()?;
    let addr = format!("127.0.0.1:{}", port);
    let write = |bind_host: &str| -> Result<()> {
        std::fs::write(
            &path,
            format!(
                "name = \"bind-test\"\n\n[server]\nport = {}\n\
                 bind = \"{}\"\ndomain = \"127.0.0.1\"\ntoken = \"bind-secret\"\n\n\
                 [[proxies]]\nname = \"backend\"\ndomain = \"bind.test\"\n\
                 local_addr = \"{}\"\n",
                port, bind_host, backend_addr
            ),
        )?;
        Ok(())
    };

    write("127.0.0.1")?;
    let (stop_tx, stop_rx) = broadcast::channel(1);
    let path_string = path.to_str().unwrap().to_string();
    let node = tokio::spawn(async move {
        run_http_tunnel_from_config(&path_string, stop_rx)
            .await
            .unwrap();
    });
    wait_for_reload_status(&addr, "bind.test", 200, Duration::from_secs(10)).await?;

    // A same-port bind-address change goes through the stop/rebind path.
    write("0.0.0.0")?;
    wait_for_reload_status(&addr, "bind.test", 200, Duration::from_secs(10)).await?;

    info!("an unusable bind address rolls back to the previous listener");
    write("192.0.2.1")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // The rollback briefly releases the old listener, so a connection
        // refused here means the new listener is not back yet.
        let response = api_request(&addr, "GET", "/api/status", Some("bind-secret"), None).await;
        let (status, body) = match response {
            std::result::Result::Ok(pair) => pair,
            std::result::Result::Err(_) => {
                if Instant::now() >= deadline {
                    anyhow::bail!("the listener did not come back after the bad bind change");
                }
                time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if status == 200 && body.contains("last_reload_error") {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("the bad bind_host was not reported: {}", body);
        }
        time::sleep(Duration::from_millis(100)).await;
    }
    wait_for_reload_status(&addr, "bind.test", 200, Duration::from_secs(5)).await?;

    write("0.0.0.0")?;
    wait_for_reload_status(&addr, "bind.test", 200, Duration::from_secs(10)).await?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let response = api_request(&addr, "GET", "/api/status", Some("bind-secret"), None).await;
        let (status, body) = match response {
            std::result::Result::Ok(pair) => pair,
            std::result::Result::Err(_) => {
                if Instant::now() >= deadline {
                    anyhow::bail!("the listener did not come back after the bind change");
                }
                time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if status == 200 && !body.contains("last_reload_error") {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("the bind error was not cleared: {}", body);
        }
        time::sleep(Duration::from_millis(100)).await;
    }

    stop_tx.send(true)?;
    let _ = time::timeout(Duration::from_secs(10), node).await;
    let _ = std::fs::remove_file(&path);
    Ok(())
}
