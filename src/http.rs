use crate::helper::normalize_domain;
use crate::protocol::Digest;
use crate::server::{HttpVisitor, ServerMetrics, ServerState};
use anyhow::{Context, Result};
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

/// Maps a `Host` to the digest of the tunnel that serves it. An exact match takes
/// precedence over a wildcard domain (`*.example.com`), and among the wildcards
/// the longest suffix wins.
#[derive(Default)]
pub struct RoutingTable {
    exact: HashMap<String, Digest>,
    // Keyed by the suffix including the leading dot, e.g. `.example.com`
    wildcard: HashMap<String, Digest>,
}

impl RoutingTable {
    pub fn insert(&mut self, domain: &str, digest: Digest) {
        if let Some(suffix) = domain.strip_prefix("*.") {
            if !suffix.is_empty() {
                self.wildcard.insert(format!(".{}", suffix), digest);
            }
        } else {
            self.exact.insert(domain.to_string(), digest);
        }
    }

    pub fn get(&self, host: &str) -> Option<Digest> {
        if let Some(digest) = self.exact.get(host) {
            return Some(*digest);
        }

        let mut best: Option<(usize, Digest)> = None;
        for (suffix, digest) in &self.wildcard {
            if host.len() > suffix.len()
                && host.ends_with(suffix.as_str())
                && best.map(|(len, _)| suffix.len() > len).unwrap_or(true)
            {
                best = Some((suffix.len(), *digest));
            }
        }
        best.map(|(_, digest)| digest)
    }
}

/// The HTTP entrypoint of the server. It accepts visitors on `bind_addr`, sniffs the
/// `Host` header, and hands the visitor over to the corresponding tunnel.
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
    // Count the visitor as active from the moment it's accepted, so that a
    // shutdown can wait for it instead of cutting it off.
    let activity = state.activity.guard();
    state.metrics.accept_visitor();

    let (prefetched, host) = match time::timeout(
        Duration::from_secs(HEADER_READ_TIMEOUT),
        read_header(&mut stream),
    )
    .await
    {
        Err(_) => {
            // E3: a timed out header is its own case, not a generic parse failure
            debug!("Timed out reading the HTTP header of {}", addr);
            respond(
                &mut stream,
                "408 Request Timeout",
                "Timed out reading the HTTP header\n",
                &state.metrics,
            )
            .await;
            return Ok(());
        }
        Ok(Err(e)) => {
            // E3: the error carries the reason, log it as-is
            debug!("Bad HTTP header from {}: {}", addr, e.message);
            respond(&mut stream, e.status, &format!("{}\n", e.message), &state.metrics).await;
            return Ok(());
        }
        Ok(Ok(header)) => header,
    };

    let digest = {
        let rt = state.routing_table.read().await;
        rt.get(&host)
    };

    let Some(digest) = digest else {
        debug!("No tunnel for the host `{}`", host);
        respond(
            &mut stream,
            "404 Not Found",
            "No tunnel for this host\n",
            &state.metrics,
        )
        .await;
        return Ok(());
    };

    let handle = {
        let ccs = state.control_channels.read().await;
        ccs.get1(&digest)
            .map(|h| (h.visitor_tx.clone(), h.data_pool.clone()))
    };

    let Some((visitor_tx, data_pool)) = handle else {
        debug!("No control channel for the host `{}`", host);
        respond_tunnel_unavailable(&mut stream, &state.metrics).await;
        return Ok(());
    };

    // Make sure the pool has a data channel for this visitor, or is asking for
    // one. The pool caches warm channels and tops itself up, bounded by its
    // target, so this doesn't request one per visitor. A closed pool means that
    // the control channel is gone.
    if data_pool.is_closed() {
        debug!("No control channel for the host `{}`", host);
        respond_tunnel_unavailable(&mut stream, &state.metrics).await;
        return Ok(());
    }
    data_pool.replenish();

    debug!("Routing visitor {} to the host `{}`", addr, host);

    if visitor_tx
        .send(HttpVisitor {
            stream,
            prefetched,
            activity,
        })
        .await
        .is_err()
    {
        warn!("Failed to hand over the visitor to the tunnel `{}`", host);
    }

    Ok(())
}

/// An error while reading or parsing the first request of a visitor. It carries
/// the HTTP status to answer with, so that a malformed request is answered
/// instead of the connection being silently closed.
#[derive(Debug)]
struct HeaderError {
    status: &'static str,
    message: String,
}

impl HeaderError {
    fn bad_request(message: impl Into<String>) -> Self {
        HeaderError {
            status: "400 Bad Request",
            message: message.into(),
        }
    }

    fn too_large() -> Self {
        HeaderError {
            status: "431 Request Header Fields Too Large",
            message: "The HTTP header is too large".to_string(),
        }
    }
}

/// Read from the stream until the end of the HTTP header is seen.
/// Returns all the bytes read so far (they must be replayed) and the host.
async fn read_header(stream: &mut TcpStream) -> Result<(Vec<u8>, String), HeaderError> {
    let mut buf = Vec::with_capacity(1024);

    loop {
        if let Some(end) = find_header_end(&buf) {
            if end > MAX_HEADER_SIZE {
                return Err(HeaderError::too_large());
            }
            let host = parse_host(&buf[..end])?;
            return Ok((buf, host));
        }
        if buf.len() > MAX_HEADER_SIZE {
            return Err(HeaderError::too_large());
        }

        let mut chunk = [0u8; 1024];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| HeaderError::bad_request(format!("Failed to read the HTTP header: {e}")))?;
        if n == 0 {
            return Err(HeaderError::bad_request(
                "The connection was closed before the HTTP header was complete",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// Strip a trailing carriage return from a header line
fn strip_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Trim ASCII whitespace from both ends of a byte slice
fn trim_ascii(mut s: &[u8]) -> &[u8] {
    while let Some((first, rest)) = s.split_first() {
        if first.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    while let Some((last, rest)) = s.split_last() {
        if last.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    s
}

/// Parse the `Host` of a request head, tolerating non-UTF-8 bytes in headers
/// other than the request line and the `Host`. The request-target takes
/// precedence when it is in absolute-form, as required by RFC 7230.
fn parse_host(head: &[u8]) -> Result<String, HeaderError> {
    let mut lines = head.split(|&b| b == b'\n');

    let request_line = lines.next().map(strip_cr).unwrap_or(&[]);
    if request_line.is_empty() {
        return Err(HeaderError::bad_request("The HTTP request is empty"));
    }

    // An absolute-form request-target (`GET http://host/path HTTP/1.1`) carries
    // the authority itself; the `Host` header, if any, must be ignored.
    if let Some(authority) = absolute_form_authority(request_line) {
        return normalize_domain_bytes(authority)
            .ok_or_else(|| HeaderError::bad_request("The `Host` in the request target is invalid"));
    }

    let mut host: Option<String> = None;
    for raw in lines {
        let line = strip_cr(raw);
        if line.is_empty() {
            break;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let name = &line[..colon];
        if !name.eq_ignore_ascii_case(b"host") {
            continue;
        }
        if host.is_some() {
            return Err(HeaderError::bad_request("Multiple `Host` headers"));
        }

        let value = trim_ascii(&line[colon + 1..]);
        let value = std::str::from_utf8(value)
            .map_err(|_| HeaderError::bad_request("The `Host` header is not valid UTF-8"))?;
        host = Some(
            normalize_domain(value)
                .ok_or_else(|| HeaderError::bad_request("The `Host` header is invalid"))?,
        );
    }

    host.ok_or_else(|| HeaderError::bad_request("No `Host` header found"))
}

/// The authority of an absolute-form request-target, `None` for the origin-form.
fn absolute_form_authority(request_line: &[u8]) -> Option<&[u8]> {
    let mut parts = request_line
        .split(|&b| b == b' ' || b == b'\t')
        .filter(|p| !p.is_empty());
    let _method = parts.next()?;
    let target = parts.next()?;

    if target.starts_with(b"/") {
        return None;
    }
    let sep = target.windows(3).position(|w| w == b"://")?;
    let after = &target[sep + 3..];
    let end = after
        .iter()
        .position(|&b| b == b'/' || b == b'?' || b == b'#')
        .unwrap_or(after.len());
    Some(&after[..end])
}

fn normalize_domain_bytes(raw: &[u8]) -> Option<String> {
    std::str::from_utf8(raw).ok().and_then(normalize_domain)
}

/// Answer a visitor with a `503`, either because no tunnel is connected for its
/// `Host`, or because the control channel of the tunnel went away after the visitor
/// was already handed over.
pub(crate) async fn respond_tunnel_unavailable(
    stream: &mut TcpStream,
    metrics: &ServerMetrics,
) {
    respond(
        stream,
        "503 Service Unavailable",
        "The tunnel is not connected\n",
        metrics,
    )
    .await;
}

/// Answer a visitor with a `504`, when the tunnel is connected but no data
/// channel is available in time, e.g. because the client cannot reach its local
/// service.
pub(crate) async fn respond_gateway_timeout(stream: &mut TcpStream, metrics: &ServerMetrics) {
    respond(
        stream,
        "504 Gateway Timeout",
        "The tunnel did not provide a data channel in time\n",
        metrics,
    )
    .await;
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str, metrics: &ServerMetrics) {
    metrics.record_response(status);
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

        // The trailing dot of a fully qualified name is stripped
        let head = b"GET / HTTP/1.1\r\nHost: foo.test.\r\n\r\n";
        assert_eq!(parse_host(head).unwrap(), "foo.test");

        let head = b"GET / HTTP/1.1\r\nAccept: */*\r\n\r\n";
        assert!(parse_host(head).is_err());
    }

    #[test]
    fn test_parse_host_tolerates_non_utf8_headers() {
        // A non-UTF-8 byte in a header other than `Host` must not break routing
        let head = b"GET / HTTP/1.1\r\nHost: ok.test\r\nX-Bin: \xff\xfe\r\n\r\n";
        assert_eq!(parse_host(head).unwrap(), "ok.test");
    }

    #[test]
    fn test_parse_host_rejects_duplicates() {
        let head = b"GET / HTTP/1.1\r\nHost: a.test\r\nHost: b.test\r\n\r\n";
        assert!(parse_host(head).is_err());
    }

    #[test]
    fn test_parse_host_absolute_form() {
        // The authority of an absolute-form target wins over the `Host` header
        let head = b"GET http://abs.test:8080/x HTTP/1.1\r\nHost: other.test\r\n\r\n";
        assert_eq!(parse_host(head).unwrap(), "abs.test");

        // No `Host` header at all, but an absolute-form target still routes
        let head = b"GET http://abs.test/x HTTP/1.1\r\n\r\n";
        assert_eq!(parse_host(head).unwrap(), "abs.test");
    }

    #[test]
    fn test_normalize_domain() {
        assert_eq!(normalize_domain("Foo.COM:8080").unwrap(), "foo.com");
        assert_eq!(normalize_domain("foo.com.").unwrap(), "foo.com");
        assert_eq!(normalize_domain("[::1]:80").unwrap(), "[::1]");
        assert_eq!(normalize_domain("*.Example.com").unwrap(), "*.example.com");
        assert!(normalize_domain("").is_none());
        assert!(normalize_domain("foo bar").is_none());
        assert!(normalize_domain("http://foo").is_none());
        assert!(normalize_domain("foo/bar").is_none());
        assert!(normalize_domain("*").is_none());
        assert!(normalize_domain("foo*bar").is_none());
    }

    #[test]
    fn test_routing_table_wildcard() {
        let exact = [1u8; 32];
        let wildcard = [2u8; 32];
        let deep = [3u8; 32];

        let mut rt = RoutingTable::default();
        rt.insert("exact.example.com", exact);
        rt.insert("*.example.com", wildcard);
        rt.insert("*.sub.example.com", deep);

        // An exact match wins over a wildcard
        assert_eq!(rt.get("exact.example.com"), Some(exact));
        // A wildcard matches at any depth
        assert_eq!(rt.get("a.example.com"), Some(wildcard));
        assert_eq!(rt.get("a.b.example.com"), Some(wildcard));
        // The longest wildcard suffix wins
        assert_eq!(rt.get("a.sub.example.com"), Some(deep));
        // The apex is not matched by `*.example.com`
        assert_eq!(rt.get("example.com"), None);
        // An unknown host matches nothing
        assert_eq!(rt.get("other.test"), None);
    }

    #[test]
    fn test_find_header_end() {
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n\r\n"), Some(18));
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n"), None);
    }
}
