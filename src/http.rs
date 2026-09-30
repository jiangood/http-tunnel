use crate::protocol::Digest;
use crate::server::{HttpVisitor, ServerState};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::time;
use tracing::{debug, error, info, warn};

/// The maximum size of the HTTP header that is buffered for sniffing the Host
const MAX_HEADER_SIZE: usize = 8 * 1024;
/// Timeout for reading the HTTP header from a visitor
const HEADER_READ_TIMEOUT: u64 = 5;

pub type RoutingTable = HashMap<String, Digest>;

/// The HTTP entrypoint of the server. It accepts visitors on `bind_addr`, sniffs the
/// `Host` header, and hands the visitor over to the corresponding service.
///
/// Only the first request of a connection is inspected, so routing is connection-level.
pub(crate) async fn serve(
    bind_addr: String,
    state: Arc<ServerState>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let l = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("Failed to listen at `http_bind_addr` ({})", bind_addr))?;
    info!("HTTP listening at {}", bind_addr);

    loop {
        tokio::select! {
            ret = l.accept() => {
                match ret {
                    Ok((stream, addr)) => {
                        let state = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_visitor(stream, addr, state).await {
                                debug!("Failed to route visitor {}: {:#}", addr, e);
                            }
                        });
                    }
                    Err(e) => error!("Failed to accept a visitor: {:#}", e),
                }
            }
            _ = shutdown_rx.recv() => break,
        }
    }

    info!("HTTP listener shutdown");
    Ok(())
}

async fn handle_visitor(
    mut stream: TcpStream,
    addr: SocketAddr,
    state: Arc<ServerState>,
) -> Result<()> {
    let (prefetched, host) = time::timeout(
        Duration::from_secs(HEADER_READ_TIMEOUT),
        read_header(&mut stream),
    )
    .await
    .map_err(|_| anyhow!("Timed out reading the HTTP header"))??;

    let digest = {
        let rt = state.routing_table.read().await;
        rt.get(&host).copied()
    };

    let Some(digest) = digest else {
        debug!("No service for the host `{}`", host);
        respond(&mut stream, "404 Not Found", "No service for this host\n").await;
        return Ok(());
    };

    let visitor_tx = {
        let ccs = state.control_channels.read().await;
        ccs.get1(&digest).map(|h| h.visitor_tx.clone())
    };

    let Some(visitor_tx) = visitor_tx else {
        debug!("No control channel for the host `{}`", host);
        respond(
            &mut stream,
            "503 Service Unavailable",
            "Service is not connected\n",
        )
        .await;
        return Ok(());
    };

    debug!("Routing visitor {} to the host `{}`", addr, host);

    if visitor_tx
        .send(HttpVisitor { stream, prefetched })
        .await
        .is_err()
    {
        warn!("Failed to hand over the visitor to the service `{}`", host);
    }

    Ok(())
}

/// Read from the stream until the end of the HTTP header is seen.
/// Returns all the bytes read so far (they must be replayed) and the host.
async fn read_header(stream: &mut TcpStream) -> Result<(Vec<u8>, String)> {
    let mut buf = Vec::with_capacity(1024);

    loop {
        if find_header_end(&buf).is_some() {
            break;
        }
        if buf.len() >= MAX_HEADER_SIZE {
            bail!("The HTTP header is too large");
        }

        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            bail!("The connection was closed before the HTTP header was complete");
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    let end = find_header_end(&buf).unwrap();
    let host = parse_host(&buf[..end]).context("Failed to parse the `Host` header")?;
    Ok((buf, host))
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn parse_host(head: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(head).context("The HTTP header is not valid UTF-8")?;
    let mut lines = text.split("\r\n");

    // Skip the request line
    let _request_line = lines.next().context("The HTTP request is empty")?;

    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("host") {
                return Ok(normalize_host(value.trim()));
            }
        }
    }

    bail!("No `Host` header found")
}

fn normalize_host(raw: &str) -> String {
    // Strip the port if present, handling IPv6 literals like `[::1]:8080`
    let host = if let Some(rest) = raw.strip_prefix('[') {
        match rest.split_once(']') {
            Some((h, _)) => format!("[{}]", h),
            None => raw.to_string(),
        }
    } else {
        match raw.split_once(':') {
            Some((h, _)) => h.to_string(),
            None => raw.to_string(),
        }
    };
    host.to_lowercase()
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Length: {}\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n{}",
        status,
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_host() {
        let head = b"GET / HTTP/1.1\r\nHost: Example.COM:8080\r\nAccept: */*\r\n\r\n";
        assert_eq!(parse_host(head).unwrap(), "example.com");

        let head = b"GET / HTTP/1.1\r\nhost:foo.test\r\n\r\n";
        assert_eq!(parse_host(head).unwrap(), "foo.test");

        let head = b"GET / HTTP/1.1\r\nHost: [::1]:2333\r\n\r\n";
        assert_eq!(parse_host(head).unwrap(), "[::1]");

        let head = b"GET / HTTP/1.1\r\nAccept: */*\r\n\r\n";
        assert!(parse_host(head).is_err());
    }

    #[test]
    fn test_find_header_end() {
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n\r\n"), Some(18));
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n"), None);
    }
}
