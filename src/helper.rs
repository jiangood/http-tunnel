use anyhow::{Context, Result};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;

/// The SHA-256 digest of a byte string.
pub(crate) fn digest(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(data));
    out
}

pub trait LocalIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> LocalIo for T {}
pub type BoxLocalIo = Box<dyn LocalIo>;

pub async fn connect_local(addr: &str) -> Result<BoxLocalIo> {
    if let Some(path) = addr.strip_prefix("unix://") {
        #[cfg(unix)]
        {
            return Ok(Box::new(UnixStream::connect(path).await.with_context(
                || format!("Failed to connect to Unix socket {}", path),
            )?));
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            anyhow::bail!("Unix sockets are not supported on this platform");
        }
    }
    Ok(Box::new(TcpStream::connect(addr).await.with_context(
        || format!("Failed to connect to local TCP service {}", addr),
    )?))
}

/// The request header a visitor must present when a tunnel sets `x-auth`.
///
/// It's a custom header on purpose: applications routinely use `Authorization`
/// for their own authentication, so checking that header would conflict with
/// them. The name is matched case-insensitively, as HTTP requires.
pub const AUTH_HEADER_NAME: &str = "X-Auth";

/// The result of checking a request against the `x-auth` of a tunnel.
#[derive(Debug)]
pub enum AuthOutcome {
    /// The request may be forwarded. Its head has the auth header removed.
    Allowed(Vec<u8>),
    /// The request does not carry the expected auth header, and why.
    Unauthorized(AuthFailure),
}

/// Why a request was rejected, so that the `401` can say what is wrong.
///
/// Telling a missing header apart from a wrong one is safe here: it only ever
/// reveals whether the header was present, never the secret itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFailure {
    /// The request carried no `X-Auth` at all.
    Missing,
    /// The request carried an `X-Auth` that did not match.
    Invalid,
}

impl AuthFailure {
    /// The plain-text body of the `401` that is sent to the visitor.
    pub fn message(self) -> &'static str {
        match self {
            AuthFailure::Missing => "Missing X-Auth header\n",
            AuthFailure::Invalid => "Invalid X-Auth header\n",
        }
    }
}

/// Check `X-Auth` of a request head against the optional secret of
/// a tunnel, and strip the header before the request is forwarded.
///
/// With no secret configured the request passes through untouched. Otherwise the
/// request must send `X-Auth: <secret>`; the header is always
/// removed from the forwarded head, so the secret never reaches the backend.
/// The `Authorization` header is left alone, whatever the application makes of it.
pub fn gate_x_auth(header: &[u8], auth: Option<&str>) -> AuthOutcome {
    let Some(auth) = auth else {
        return AuthOutcome::Allowed(header.to_vec());
    };
    let Some(end) = header.windows(4).position(|w| w == b"\r\n\r\n") else {
        // A head that cannot be parsed has no usable header in it
        return AuthOutcome::Unauthorized(AuthFailure::Missing);
    };

    let mut present = false;
    let mut authorized = false;
    let mut out = Vec::with_capacity(header.len());
    let mut lines = header[..end].split(|b| *b == b'\n');
    if let Some(first) = lines.next() {
        out.extend_from_slice(first.strip_suffix(b"\r").unwrap_or(first));
        out.extend_from_slice(b"\r\n");
    }
    for raw in lines {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() {
            continue;
        }
        match line.iter().position(|b| *b == b':') {
            Some(colon) if line[..colon].eq_ignore_ascii_case(AUTH_HEADER_NAME.as_bytes()) => {
                present = true;
                let value = trim_ascii(&line[colon + 1..]);
                if constant_time_eq(value, auth.as_bytes()) {
                    authorized = true;
                }
                // The header is never forwarded, whether it matched or not.
            }
            _ => {
                out.extend_from_slice(line);
                out.extend_from_slice(b"\r\n");
            }
        }
    }

    if !authorized {
        return AuthOutcome::Unauthorized(if present {
            AuthFailure::Invalid
        } else {
            AuthFailure::Missing
        });
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&header[end + 4..]);
    AuthOutcome::Allowed(out)
}

/// Carry the request id of a visitor across the loopback hop a relay makes into
/// its own embedded listener.
///
/// Without this, the embedded listener would start a new id, and the page the
/// visitor sees would name an id that exists only in the last hop's log. The
/// header is only trusted from a loopback peer (see [`take_request_id`]).
pub(crate) fn insert_request_id(header: &[u8], request_id: &str) -> Vec<u8> {
    let Some(end) = header.windows(4).position(|w| w == b"\r\n\r\n") else {
        return header.to_vec();
    };
    let (mut out, _) = strip_request_id_header(&header[..end]);
    out.extend_from_slice(crate::http::REQUEST_ID_HEADER.as_bytes());
    out.extend_from_slice(b": ");
    out.extend_from_slice(request_id.as_bytes());
    out.extend_from_slice(b"\r\n\r\n");
    out.extend_from_slice(&header[end + 4..]);
    out
}

/// Take the request id that a relay put on a visitor when it handed it to its
/// own embedded listener, and strip the header so the local service never sees
/// it. Only a loopback peer may carry it: anyone else could invent an id and
/// collide with the operator's log search.
pub(crate) fn take_request_id(header: &mut Vec<u8>, peer_is_loopback: bool) -> Option<String> {
    if !peer_is_loopback {
        return None;
    }
    let end = header.windows(4).position(|w| w == b"\r\n\r\n")?;
    let (mut out, value) = strip_request_id_header(&header[..end]);
    // No private header: leave the request byte-for-byte untouched, so that the
    // body and its `Content-Length` still line up.
    let value = value?;
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&header[end + 4..]);
    *header = out;
    let valid = value.len() == 16 && value.bytes().all(|b| b.is_ascii_hexdigit());
    valid.then_some(value)
}

/// Drop every request-id line of a head that has no final blank line, and
/// return the rebuilt lines plus the last value seen.
fn strip_request_id_header(head: &[u8]) -> (Vec<u8>, Option<String>) {
    let mut out = Vec::with_capacity(head.len());
    let mut value = None;
    let mut lines = head.split(|b| *b == b'\n');
    if let Some(first) = lines.next() {
        out.extend_from_slice(first.strip_suffix(b"\r").unwrap_or(first));
        out.extend_from_slice(b"\r\n");
    }
    for raw in lines {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() {
            continue;
        }
        match line.iter().position(|b| *b == b':') {
            Some(colon)
                if line[..colon]
                    .eq_ignore_ascii_case(crate::http::REQUEST_ID_HEADER.as_bytes()) =>
            {
                value = Some(String::from_utf8_lossy(trim_ascii(&line[colon + 1..])).into_owned());
            }
            _ => {
                out.extend_from_slice(line);
                out.extend_from_slice(b"\r\n");
            }
        }
    }
    (out, value)
}

/// Trim ASCII whitespace (spaces and tabs) from both ends of a byte slice
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

/// Build a plain-text HTTP response that closes the connection.
///
/// It's shared by the server-side entrypoint and the upstream session, so that a
/// `401` from a tunnel gate looks the same on both sides.
pub(crate) fn response_with_headers(status: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut response = format!("HTTP/1.1 {}\r\n", status);
    for (name, value) in headers {
        response.push_str(&format!("{}: {}\r\n", name, value));
    }
    response.push_str(&format!(
        "Content-Length: {}\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    ));
    response.into_bytes()
}

/// Compare two byte strings without an early exit, so that the comparison time
/// does not reveal how many leading bytes of the secret matched.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}
#[allow(dead_code)]
pub fn feature_not_compile(feature: &str) -> ! {
    panic!(
        "The feature '{}' is not compiled in this binary. Please re-compile http-tunnel",
        feature
    )
}

/// The default address a listener binds: loopback. A server that must be
/// reachable from another machine sets `[server].bind` explicitly.
pub const DEFAULT_BIND_HOST: &str = "127.0.0.1";

/// Format the socket address of the listener, which binds every interface.
pub fn to_bind_addr(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    }
}

/// Normalize a domain coming from a `Host` header value, an absolute-form
/// authority, or a configuration key: strip the port if any, strip the trailing
/// dot of a fully qualified name, and lowercase. IPv6 literals keep their
/// brackets. A leading `*.` wildcard is preserved. Returns `None` when the
/// result is empty or contains invalid characters.
pub fn normalize_domain(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || !raw.is_ascii() {
        return None;
    }
    // Reject ASCII control characters, whitespace and DEL
    if raw.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return None;
    }
    // Reject anything that looks like a URL or a path
    if raw.contains('/') || raw.contains('?') || raw.contains('#') {
        return None;
    }
    // `*` is only allowed as the leading label of a wildcard domain
    if raw.contains('*') && !raw.starts_with("*.") {
        return None;
    }

    // Strip the port if present, handling IPv6 literals like `[::1]:8080`
    let host = if let Some(rest) = raw.strip_prefix('[') {
        let (h, tail) = rest.split_once(']')?;
        if !tail.is_empty() {
            let port = tail.strip_prefix(':')?;
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
        }
        format!("[{}]", h)
    } else {
        match raw.rsplit_once(':') {
            Some((h, port)) => {
                if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                h.to_string()
            }
            None => raw.to_string(),
        }
    };

    // Strip the trailing dot of a fully qualified name, e.g. `example.com.`
    let host = host.strip_suffix('.').unwrap_or(&host);
    if host.is_empty() {
        return None;
    }

    let valid = host.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'*' | b'[' | b']' | b':')
    });
    if !valid {
        return None;
    }

    Some(host.to_lowercase())
}

/// Whether a normalized `Host` is an IP literal (`192.168.1.5`, `[::1]`).
pub(crate) fn is_ip_host(host: &str) -> bool {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .is_ok()
}

/// Whether the address is a LAN one: loopback, RFC1918 or IPv6 ULA/link-local.
/// The control plane on an IP `Host` is only reachable from these peers, so a
/// public scanner presenting a forged private `Host` still gets a `404`.
pub(crate) fn is_lan_peer(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => {
            let seg = v6.segments();
            v6.is_loopback()
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] & 0xffc0) == 0xfe80
        }
    }
}

/// A best-effort local IPv4 of the default outbound route, only used for the
/// first-start hint. UDP `connect` only looks up the route and never sends a
/// packet. `None` when no route exists at all.
pub fn detect_local_ipv4() -> Option<String> {
    use std::net::{SocketAddr, UdpSocket};

    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(addr) => Some(addr.ip().to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn test_is_ip_host() {
        assert!(is_ip_host("192.168.1.5"));
        assert!(is_ip_host("127.0.0.1"));
        assert!(is_ip_host("[::1]"));
        assert!(is_ip_host("[fe80::1]"));
        assert!(!is_ip_host("edge.lan"));
        assert!(!is_ip_host("*.example.com"));
        assert!(!is_ip_host("999.1.1.1"));
    }

    #[test]
    fn test_is_lan_peer() {
        let lan = [
            "127.0.0.1",
            "10.0.0.3",
            "172.16.0.9",
            "192.168.1.200",
            "169.254.3.4",
            "::1",
            "fd12:3456::1",
            "fe80::1",
        ];
        for ip in lan {
            assert!(
                is_lan_peer(ip.parse::<IpAddr>().unwrap()),
                "{} is a LAN address",
                ip
            );
        }
        let wan = [
            "8.8.8.8",
            "203.0.113.7",
            "172.32.0.1",
            "192.169.0.1",
            "2606:4700::1",
            "2001:db8::1",
        ];
        for ip in wan {
            assert!(
                !is_lan_peer(ip.parse::<IpAddr>().unwrap()),
                "{} is a public address",
                ip
            );
        }
    }

    #[test]
    fn test_detect_local_ipv4_is_a_plain_address() {
        // Whatever the environment offers, it must be a bare IPv4 literal.
        if let Some(ip) = detect_local_ipv4() {
            assert!(ip.parse::<std::net::Ipv4Addr>().is_ok(), "{}", ip);
        }
    }

    fn allowed(outcome: AuthOutcome) -> Vec<u8> {
        match outcome {
            AuthOutcome::Allowed(out) => out,
            AuthOutcome::Unauthorized(reason) => {
                panic!("the request should have been allowed, got {:?}", reason)
            }
        }
    }

    fn rejected(outcome: AuthOutcome) -> AuthFailure {
        match outcome {
            AuthOutcome::Allowed(_) => panic!("the request should have been rejected"),
            AuthOutcome::Unauthorized(reason) => reason,
        }
    }

    #[test]
    fn test_gate_x_auth_passes_through_when_disabled() {
        // Without a secret the request is forwarded byte for byte, even an
        // `Authorization` that another application would use.
        let head = b"GET / HTTP/1.1\r\nHost: a.test\r\nAuthorization: Bearer app\r\n\r\n";
        assert_eq!(allowed(gate_x_auth(head, None)), head);
    }

    #[test]
    fn test_gate_x_auth_accepts_and_strips_the_token() {
        let head =
            b"GET / HTTP/1.1\r\nHost: a.test\r\nX-Auth: s3cret\r\nAccept: */*\r\n\r\n";
        let out = String::from_utf8(allowed(gate_x_auth(head, Some("s3cret")))).unwrap();
        assert!(out.contains("Host: a.test\r\n"));
        assert!(out.contains("Accept: */*\r\n"));
        assert!(!out.to_lowercase().contains("x-auth"));
        assert!(out.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_gate_x_auth_name_is_case_insensitive() {
        let head = b"GET / HTTP/1.1\r\nhost: a.test\r\nx-auth: abc\r\n\r\n";
        assert!(matches!(
            gate_x_auth(head, Some("abc")),
            AuthOutcome::Allowed(_)
        ));
    }

    #[test]
    fn test_gate_x_auth_tolerates_value_whitespace() {
        let head = b"GET / HTTP/1.1\r\nHost: a.test\r\nX-Auth:   abc \r\n\r\n";
        assert!(matches!(
            gate_x_auth(head, Some("abc")),
            AuthOutcome::Allowed(_)
        ));
    }

    #[test]
    fn test_gate_x_auth_reports_the_reason() {
        // No header at all: the visitor is told it is missing
        let missing = b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n";
        assert_eq!(
            rejected(gate_x_auth(missing, Some("s3cret"))),
            AuthFailure::Missing
        );

        // A header that does not match: the visitor is told it is invalid
        let wrong = b"GET / HTTP/1.1\r\nHost: a.test\r\nX-Auth: nope\r\n\r\n";
        assert_eq!(
            rejected(gate_x_auth(wrong, Some("s3cret"))),
            AuthFailure::Invalid
        );

        // An incomplete head cannot be inspected, so it is reported as missing
        let partial = b"GET / HTTP/1.1\r\nHost: a.test\r\n";
        assert_eq!(
            rejected(gate_x_auth(partial, Some("s3cret"))),
            AuthFailure::Missing
        );
    }

    #[test]
    fn test_auth_failure_messages_name_the_header() {
        assert!(AuthFailure::Missing
            .message()
            .contains("X-Auth"));
        assert!(AuthFailure::Invalid
            .message()
            .contains("X-Auth"));
        assert!(AuthFailure::Missing.message().ends_with('\n'));
        assert!(AuthFailure::Invalid.message().ends_with('\n'));
    }

    #[test]
    fn test_request_id_header_round_trips_through_the_loopback_hop() {
        let head = b"GET / HTTP/1.1\r\nHost: a.test\r\nAccept: */*\r\n\r\nbody";
        let carried = insert_request_id(head, "0123456789abcdef");
        let text = String::from_utf8(carried.clone()).unwrap();
        assert!(text.contains("\r\nx-http-tunnel-request-id: 0123456789abcdef\r\n"));
        assert!(text.ends_with("\r\n\r\nbody"));

        let mut received = carried;
        assert_eq!(
            take_request_id(&mut received, true).as_deref(),
            Some("0123456789abcdef")
        );
        let received = String::from_utf8(received).unwrap();
        assert!(!received.to_lowercase().contains("x-http-tunnel-request-id"));
        assert!(received.ends_with("\r\n\r\nbody"));
    }

    #[test]
    fn test_request_id_header_is_not_trusted_from_a_non_loopback_peer() {
        let mut head =
            b"GET / HTTP/1.1\r\nHost: a.test\r\nx-http-tunnel-request-id: 0123456789abcdef\r\n\r\n"
                .to_vec();
        assert_eq!(take_request_id(&mut head, false), None);
        // The head is untouched, so a visitor cannot strip its own header.
        assert!(String::from_utf8(head)
            .unwrap()
            .contains("x-http-tunnel-request-id: 0123456789abcdef"));
    }

    #[test]
    fn test_request_id_header_replaces_a_visitor_supplied_value() {
        let head =
            b"GET / HTTP/1.1\r\nHost: a.test\r\nX-HTTP-Tunnel-Request-Id: deadbeefdeadbeef\r\n\r\n";
        let carried = insert_request_id(head, "0123456789abcdef");
        let text = String::from_utf8(carried).unwrap();
        assert!(!text.contains("deadbeefdeadbeef"));
        assert_eq!(text.matches("x-http-tunnel-request-id").count(), 1);
        assert!(text.contains("x-http-tunnel-request-id: 0123456789abcdef"));
    }

    #[test]
    fn test_request_id_header_rejects_a_malformed_value() {
        let mut head =
            b"GET / HTTP/1.1\r\nHost: a.test\r\nx-http-tunnel-request-id: not-an-id\r\n\r\n"
                .to_vec();
        assert_eq!(take_request_id(&mut head, true), None);
        // A malformed value is still stripped, so the local service never sees it.
        assert!(!String::from_utf8(head)
            .unwrap()
            .to_lowercase()
            .contains("x-http-tunnel-request-id"));
    }

    #[test]
    fn test_gate_x_auth_keeps_the_body_and_the_authorization() {
        let head = b"POST / HTTP/1.1\r\nHost: a.test\r\nAuthorization: Bearer app\r\nX-Auth: s\r\n\r\nhello";
        let out = String::from_utf8(allowed(gate_x_auth(head, Some("s")))).unwrap();
        assert!(out.contains("Authorization: Bearer app\r\n"));
        assert!(out.ends_with("\r\n\r\nhello"));
    }
}
