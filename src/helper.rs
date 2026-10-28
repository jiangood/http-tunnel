use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;

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

/// The request header a visitor must present when a tunnel sets `auth_header`.
///
/// It's a custom header on purpose: applications routinely use `Authorization`
/// for their own authentication, so checking that header would conflict with
/// them. The name is matched case-insensitively, as HTTP requires.
pub const AUTH_HEADER_NAME: &str = "X-HTTP-TUNNEL-TOKEN";

/// The result of checking a request against the `auth_header` of a tunnel.
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
    /// The request carried no `X-HTTP-TUNNEL-TOKEN` at all.
    Missing,
    /// The request carried an `X-HTTP-TUNNEL-TOKEN` that did not match.
    Invalid,
}

impl AuthFailure {
    /// The plain-text body of the `401` that is sent to the visitor.
    pub fn message(self) -> &'static str {
        match self {
            AuthFailure::Missing => "Missing X-HTTP-TUNNEL-TOKEN header\n",
            AuthFailure::Invalid => "Invalid X-HTTP-TUNNEL-TOKEN header\n",
        }
    }
}

/// Check `X-HTTP-TUNNEL-TOKEN` of a request head against the optional secret of
/// a tunnel, and strip the header before the request is forwarded.
///
/// With no secret configured the request passes through untouched. Otherwise the
/// request must send `X-HTTP-TUNNEL-TOKEN: <secret>`; the header is always
/// removed from the forwarded head, so the secret never reaches the backend.
/// The `Authorization` header is left alone, whatever the application makes of it.
pub fn gate_auth_header(header: &[u8], auth: Option<&str>) -> AuthOutcome {
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
/// It's shared by the server-side entrypoint and the client, so that a `401` from
/// a tunnel gate looks the same on both sides and the client builds without the
/// `server` feature.
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

/// The `401` sent when a tunnel sets `auth_header` and the visitor does not
/// present the expected `X-HTTP-TUNNEL-TOKEN`. It carries no `WWW-Authenticate`:
/// that header describes an `Authorization` scheme, which this is not.
pub(crate) fn unauthorized_response(failure: AuthFailure) -> Vec<u8> {
    response_with_headers("401 Unauthorized", &[], failure.message())
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

/// The default address a listener binds: all interfaces.
pub const DEFAULT_BIND_HOST: &str = "0.0.0.0";

/// Format the socket address of a listener. `host` is the IP address to bind:
/// the default `0.0.0.0` exposes it on every interface, `127.0.0.1` keeps it on
/// the loopback interface only.
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

pub async fn write_and_flush<T>(conn: &mut T, data: &[u8]) -> Result<()>
where
    T: AsyncWrite + Unpin,
{
    conn.write_all(data)
        .await
        .with_context(|| "Failed to write data")?;
    conn.flush().await.with_context(|| "Failed to flush data")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_gate_auth_header_passes_through_when_disabled() {
        // Without a secret the request is forwarded byte for byte, even an
        // `Authorization` that another application would use.
        let head = b"GET / HTTP/1.1\r\nHost: a.test\r\nAuthorization: Bearer app\r\n\r\n";
        assert_eq!(allowed(gate_auth_header(head, None)), head);
    }

    #[test]
    fn test_gate_auth_header_accepts_and_strips_the_token() {
        let head =
            b"GET / HTTP/1.1\r\nHost: a.test\r\nX-HTTP-TUNNEL-TOKEN: s3cret\r\nAccept: */*\r\n\r\n";
        let out = String::from_utf8(allowed(gate_auth_header(head, Some("s3cret")))).unwrap();
        assert!(out.contains("Host: a.test\r\n"));
        assert!(out.contains("Accept: */*\r\n"));
        assert!(!out.to_lowercase().contains("x-http-tunnel-token"));
        assert!(out.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_gate_auth_header_name_is_case_insensitive() {
        let head = b"GET / HTTP/1.1\r\nhost: a.test\r\nx-http-tunnel-token: abc\r\n\r\n";
        assert!(matches!(
            gate_auth_header(head, Some("abc")),
            AuthOutcome::Allowed(_)
        ));
    }

    #[test]
    fn test_gate_auth_header_tolerates_value_whitespace() {
        let head = b"GET / HTTP/1.1\r\nHost: a.test\r\nX-HTTP-TUNNEL-TOKEN:   abc \r\n\r\n";
        assert!(matches!(
            gate_auth_header(head, Some("abc")),
            AuthOutcome::Allowed(_)
        ));
    }

    #[test]
    fn test_gate_auth_header_reports_the_reason() {
        // No header at all: the visitor is told it is missing
        let missing = b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n";
        assert_eq!(
            rejected(gate_auth_header(missing, Some("s3cret"))),
            AuthFailure::Missing
        );

        // A header that does not match: the visitor is told it is invalid
        let wrong = b"GET / HTTP/1.1\r\nHost: a.test\r\nX-HTTP-TUNNEL-TOKEN: nope\r\n\r\n";
        assert_eq!(
            rejected(gate_auth_header(wrong, Some("s3cret"))),
            AuthFailure::Invalid
        );

        // An incomplete head cannot be inspected, so it is reported as missing
        let partial = b"GET / HTTP/1.1\r\nHost: a.test\r\n";
        assert_eq!(
            rejected(gate_auth_header(partial, Some("s3cret"))),
            AuthFailure::Missing
        );
    }

    #[test]
    fn test_auth_failure_messages_name_the_header() {
        assert!(AuthFailure::Missing
            .message()
            .contains("X-HTTP-TUNNEL-TOKEN"));
        assert!(AuthFailure::Invalid
            .message()
            .contains("X-HTTP-TUNNEL-TOKEN"));
        assert!(AuthFailure::Missing.message().ends_with('\n'));
        assert!(AuthFailure::Invalid.message().ends_with('\n'));
    }

    #[test]
    fn test_gate_auth_header_keeps_the_body_and_the_authorization() {
        let head = b"POST / HTTP/1.1\r\nHost: a.test\r\nAuthorization: Bearer app\r\nX-HTTP-TUNNEL-TOKEN: s\r\n\r\nhello";
        let out = String::from_utf8(allowed(gate_auth_header(head, Some("s")))).unwrap();
        assert!(out.contains("Authorization: Bearer app\r\n"));
        assert!(out.ends_with("\r\n\r\nhello"));
    }
}
