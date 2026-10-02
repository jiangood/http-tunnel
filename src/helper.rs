use anyhow::{anyhow, Context, Result};
use backoff::{backoff::Backoff, Notify};
use std::{future::Future, net::SocketAddr};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::net::{lookup_host, ToSocketAddrs};
use tokio::sync::broadcast;

#[allow(dead_code)]
pub fn feature_not_compile(feature: &str) -> ! {
    panic!(
        "The feature '{}' is not compiled in this binary. Please re-compile http-tunnel",
        feature
    )
}

pub async fn to_socket_addr<A: ToSocketAddrs>(addr: A) -> Result<SocketAddr> {
    lookup_host(addr)
        .await?
        .next()
        .ok_or_else(|| anyhow!("Failed to lookup the host"))
}

/// Format the socket address of a port. The host is always `0.0.0.0`, so that
/// every listener binds to all interfaces.
pub fn to_bind_addr(port: u16) -> String {
    format!("0.0.0.0:{}", port)
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

// Wrapper of retry_notify
pub async fn retry_notify_with_deadline<I, E, Fn, Fut, B, N>(
    backoff: B,
    operation: Fn,
    notify: N,
    deadline: &mut broadcast::Receiver<bool>,
) -> Result<I>
where
    E: std::error::Error + Send + Sync + 'static,
    B: Backoff,
    Fn: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<I, backoff::Error<E>>>,
    N: Notify<E>,
{
    tokio::select! {
        v = backoff::future::retry_notify(backoff, operation, notify) => {
            v.map_err(anyhow::Error::new)
        }
        _ = deadline.recv() => {
            Err(anyhow!("shutdown"))
        }
    }
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
