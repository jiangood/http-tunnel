use crate::config::ServerProxyEntry;
use crate::helper::normalize_domain;
use crate::server::ServerState;
use anyhow::Result;
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
/// Timeout for a v4 session to open a stream for a visitor
const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(10);

fn find_proxy<'a>(proxies: &'a [ServerProxyEntry], host: &str) -> Option<&'a ServerProxyEntry> {
    let mut wildcard: Option<(&ServerProxyEntry, usize)> = None;
    for proxy in proxies {
        let domain = normalize_domain(&proxy.domain)?;
        if !domain.starts_with("*.") && domain == host {
            return Some(proxy);
        }
        if let Some(suffix) = domain.strip_prefix("*.") {
            if host.len() > suffix.len()
                && host.ends_with(&format!(".{}", suffix))
                && wildcard.map(|(_, len)| suffix.len() > len).unwrap_or(true)
            {
                wildcard = Some((proxy, suffix.len()));
            }
        }
    }
    wildcard.map(|(proxy, _)| proxy)
}

/// The HTTP entrypoint of the server. It accepts visitors on the already-bound
/// listener, sniffs the `Host` header, and hands the visitor over to the
/// corresponding tunnel.
///
/// The listener is bound by the server before it starts this task, so that a
/// port that cannot be bound fails startup instead of being reported only when
/// the server shuts down.
///
/// Only the first request of a connection is inspected, so routing is connection-level.
pub(crate) async fn serve(
    l: TcpListener,
    state: Arc<ServerState>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    if let Ok(addr) = l.local_addr() {
        info!("HTTP listening at {}", addr);
    }

    loop {
        tokio::select! {
            ret = l.accept() => {
                match ret {
                    Ok((stream, addr)) => {
                        let state = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_visitor(
                                stream,
                                addr,
                                state,
                            ).await {
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
            )
            .await;
            return Ok(());
        }
        Ok(Err(e)) => {
            // E3: the error carries the reason, log it as-is
            debug!("Bad HTTP header from {}: {}", addr, e.message);
            respond(&mut stream, e.status, &format!("{}\n", e.message)).await;
            return Ok(());
        }
        Ok(Ok(header)) => header,
    };

    // Reserve the configured server Host for the internal WebSocket transport.
    // Host is a routing key only; the token in the `Authorization` header
    // authenticates the upgrade before it can reserve any capacity.
    if state.server_host == host {
        let path = parse_request_path(&prefetched).unwrap_or_default();
        if path != crate::protocol_v4::V4_PATH {
            respond(&mut stream, "404 Not Found", "Unknown control endpoint\n").await;
            return Ok(());
        }
        let Some((conn, _pending_permit)) =
            accept_tunnel_websocket(prefetched, stream, &state, addr).await?
        else {
            return Ok(());
        };
        // This is a transport/session connection, not an in-flight visitor
        // request. Do not include its lifetime in the visitor drain counter.
        drop(activity);
        if let Err(e) = crate::session_server::accept(conn, state).await {
            debug!("Tunnel WebSocket connection closed: {:#}", e);
        }
        return Ok(());
    }

    if let Some(proxy) = {
        let proxies = state.proxies.read().await;
        find_proxy(&proxies, &host).cloned()
    } {
        let request =
            match crate::helper::gate_auth_header(&prefetched, proxy.auth_header.as_deref()) {
                crate::helper::AuthOutcome::Allowed(request) => request,
                crate::helper::AuthOutcome::Unauthorized(failure) => {
                    debug!(
                        "Rejected an unauthenticated visitor {} for host {}: {:?}",
                        addr, host, failure
                    );
                    respond_unauthorized(&mut stream, failure).await;
                    drop(activity);
                    return Ok(());
                }
            };
        debug!(
            "Proxying visitor {} for host {} to {}",
            addr, host, proxy.local_addr
        );
        match crate::helper::connect_local(&proxy.local_addr).await {
            Ok(mut upstream) => {
                upstream.write_all(&request).await?;
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
            }
            Err(e) => {
                warn!(
                    "Failed to connect server-side proxy {} to {}: {}",
                    host, proxy.local_addr, e
                );
                respond(
                    &mut stream,
                    "502 Bad Gateway",
                    "The server-side proxy target is unavailable\n",
                )
                .await;
            }
        }
        drop(activity);
        return Ok(());
    }

    // A v4 session multiplexes every visitor of a peer over one connection.
    if let Some(handle) = crate::session_server::lookup(&state, &host).await {
        match handle.open(&host, prefetched, STREAM_OPEN_TIMEOUT).await {
            Ok(io) => {
                debug!("Routing visitor {} to a v4 stream for `{}`", addr, host);
                tokio::spawn(async move {
                    let _activity = activity;
                    crate::server::relay(io, stream).await;
                });
                return Ok(());
            }
            Err(e) => {
                debug!("Failed to open a v4 stream for `{}`: {:#}", host, e);
                respond(
                    &mut stream,
                    "502 Bad Gateway",
                    "The tunnel is not available\n",
                )
                .await;
                return Ok(());
            }
        }
    }

    debug!("No tunnel for the host `{}`", host);
    respond(&mut stream, "404 Not Found", "No tunnel for this host\n").await;
    Ok(())
}

/// Perform the generic pre-upgrade checks of a tunnel WebSocket: the upgrade
/// header, the bearer token and the pending-handshake budget. On failure it
/// writes the response itself and returns `None`.
async fn accept_tunnel_websocket(
    prefetched: Vec<u8>,
    mut stream: TcpStream,
    state: &Arc<ServerState>,
    addr: SocketAddr,
) -> Result<Option<(crate::transport::Conn, tokio::sync::OwnedSemaphorePermit)>> {
    if !is_websocket_upgrade(&prefetched) {
        respond_upgrade_required(&mut stream).await;
        return Ok(None);
    }
    match parse_bearer_token(&prefetched) {
        Some(token) if crate::helper::constant_time_eq(token, state.token.as_bytes()) => {}
        None | Some(_) => {
            debug!("Rejected an unauthenticated tunnel upgrade from {}", addr);
            respond_unauthorized_bearer(&mut stream).await;
            return Ok(None);
        }
    }
    // The pending permit only covers the upgrade and the protocol hello. The
    // session permit is taken once the hello is authenticated, so unauthenticated
    // or slow handshakes never starve a real session.
    let pending_permit = match state.pending_connections.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            respond(
                &mut stream,
                "503 Service Unavailable",
                "WebSocket handshake limit reached\n",
            )
            .await;
            return Ok(None);
        }
    };
    let conn =
        crate::transport::websocket::accept(prefetched, stream, state.limits.keepalive_interval)
            .await?;
    Ok(Some((conn, pending_permit)))
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
        let n = stream.read(&mut chunk).await.map_err(|e| {
            HeaderError::bad_request(format!("Failed to read the HTTP header: {e}"))
        })?;
        if n == 0 {
            return Err(HeaderError::bad_request(
                "The connection was closed before the HTTP header was complete",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn parse_request_path(head: &[u8]) -> Option<String> {
    let line = head.split(|&b| b == b'\n').next().map(strip_cr)?;
    let line = std::str::from_utf8(line).ok()?;
    let mut parts = line.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    let target = parts.next()?;
    if !target.starts_with('/') {
        return None;
    }
    Some(target.split('?').next()?.to_string())
}

/// The token of an `Authorization: Bearer <token>` header, if the request has
/// one. The tunnel clients send the shared token here so the server can reject
/// an unauthenticated upgrade before reserving a connection slot.
fn parse_bearer_token(head: &[u8]) -> Option<&[u8]> {
    for raw in head.split(|&b| b == b'\n').skip(1) {
        let line = strip_cr(raw);
        if line.is_empty() {
            break;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        if !trim_ascii(&line[..colon]).eq_ignore_ascii_case(b"authorization") {
            continue;
        }
        let value = trim_ascii(&line[colon + 1..]);
        if value.len() > 7 && value[..7].eq_ignore_ascii_case(b"Bearer ") {
            let token = trim_ascii(&value[7..]);
            if !token.is_empty() {
                return Some(token);
            }
        }
    }
    None
}

fn is_websocket_upgrade(head: &[u8]) -> bool {
    let mut upgrade = false;
    let mut connection_upgrade = false;
    for raw in head.split(|&b| b == b'\n').skip(1) {
        let line = strip_cr(raw);
        if line.is_empty() {
            break;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let name = trim_ascii(&line[..colon]);
        let value = trim_ascii(&line[colon + 1..]);
        if name.eq_ignore_ascii_case(b"upgrade") {
            upgrade = value.eq_ignore_ascii_case(b"websocket");
        } else if name.eq_ignore_ascii_case(b"connection") {
            connection_upgrade = value
                .split(|&b| b == b',')
                .any(|v| trim_ascii(v).eq_ignore_ascii_case(b"upgrade"));
        }
    }
    upgrade && connection_upgrade
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
        return normalize_domain_bytes(authority).ok_or_else(|| {
            HeaderError::bad_request("The `Host` in the request target is invalid")
        });
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

/// The `401` sent when a tunnel sets `auth_header` and the visitor does not
/// present the expected `X-HTTP-TUNNEL-TOKEN`. It carries no `WWW-Authenticate`:
/// that header describes an `Authorization` scheme, which this is not.
pub(crate) fn unauthorized_response(failure: crate::helper::AuthFailure) -> Vec<u8> {
    crate::helper::unauthorized_response(failure)
}

/// Answer a visitor with a `401`, see [`unauthorized_response`].
pub(crate) async fn respond_unauthorized(
    stream: &mut TcpStream,
    failure: crate::helper::AuthFailure,
) {
    let _ = stream.write_all(&unauthorized_response(failure)).await;
    let _ = stream.flush().await;
}

/// Build a plain-text HTTP response that closes the connection. It's shared with
/// the server, which writes a `504` from a stream wrapper when a tunnel closes
/// without sending anything back.
pub(crate) fn response(status: &str, body: &str) -> Vec<u8> {
    crate::helper::response_with_headers(status, &[], body)
}

/// Like [`response`], with extra headers in front of the standard ones.
fn response_with_headers(status: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    crate::helper::response_with_headers(status, headers, body)
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let _ = stream.write_all(&response(status, body)).await;
    let _ = stream.flush().await;
}

/// The `401` sent when a tunnel upgrade does not carry the shared token.
async fn respond_unauthorized_bearer(stream: &mut TcpStream) {
    let response = response_with_headers(
        "401 Unauthorized",
        &[("WWW-Authenticate", "Bearer")],
        "Missing or invalid Authorization header\n",
    );
    let _ = stream.write_all(&response).await;
    let _ = stream.flush().await;
}

/// The `426` sent when the server host is reached without a WebSocket upgrade.
/// RFC 7231 §6.5.15 requires the response to list the acceptable protocols in
/// an `Upgrade` header.
async fn respond_upgrade_required(stream: &mut TcpStream) {
    let response = response_with_headers(
        "426 Upgrade Required",
        &[("Upgrade", "websocket")],
        "WebSocket upgrade required\n",
    );
    let _ = stream.write_all(&response).await;
    let _ = stream.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bearer_token() {
        let head = b"GET /_http-tunnel/v1/connect HTTP/1.1\r\nHost: control.test\r\nAuthorization: Bearer secret-token\r\n\r\n";
        assert_eq!(parse_bearer_token(head), Some(&b"secret-token"[..]));

        // The scheme is case-insensitive
        let head = b"GET / HTTP/1.1\r\nAuthorization: bearer secret\r\n\r\n";
        assert_eq!(parse_bearer_token(head), Some(&b"secret"[..]));

        let head = b"GET / HTTP/1.1\r\nAuthorization: Basic abc\r\n\r\n";
        assert_eq!(parse_bearer_token(head), None);

        let head = b"GET / HTTP/1.1\r\nAuthorization: Bearer\r\n\r\n";
        assert_eq!(parse_bearer_token(head), None);

        let head = b"GET / HTTP/1.1\r\nHost: control.test\r\n\r\n";
        assert_eq!(parse_bearer_token(head), None);
    }

    #[test]
    fn test_upgrade_required_response_lists_websocket() {
        let response = response_with_headers(
            "426 Upgrade Required",
            &[("Upgrade", "websocket")],
            "WebSocket upgrade required\n",
        );
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 426 Upgrade Required\r\n"));
        // RFC 7231 §6.5.15 requires the acceptable protocols in `Upgrade`.
        assert!(
            response.contains("\r\nUpgrade: websocket\r\n"),
            "{}",
            response
        );
    }

    #[test]
    fn test_parse_request_path() {
        assert_eq!(
            parse_request_path(
                b"GET /_http-tunnel/v1/connect HTTP/1.1\r\nHost: control.test\r\n\r\n"
            ),
            Some("/_http-tunnel/v1/connect".to_string())
        );
        assert_eq!(
            parse_request_path(
                b"GET /_http-tunnel/v1/connect?x=1 HTTP/1.1\r\nHost: control.test\r\n\r\n"
            ),
            Some("/_http-tunnel/v1/connect".to_string())
        );
        assert_eq!(
            parse_request_path(
                b"POST /_http-tunnel/v1/connect HTTP/1.1\r\nHost: control.test\r\n\r\n"
            ),
            None
        );
    }

    #[test]
    fn test_is_websocket_upgrade() {
        assert!(is_websocket_upgrade(
            b"GET /_http-tunnel/v1/connect HTTP/1.1\r\nHost: control.test\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade\r\n\r\n"
        ));
        assert!(!is_websocket_upgrade(
            b"GET /_http-tunnel/v1/connect HTTP/1.1\r\nHost: control.test\r\nConnection: keep-alive\r\n\r\n"
        ));
    }

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
    fn test_find_header_end() {
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n\r\n"), Some(18));
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n"), None);
    }
}
