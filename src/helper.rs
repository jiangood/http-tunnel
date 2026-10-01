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

/// Normalize a bind address: a bare port (e.g. `"2333"`) is bound to all interfaces,
/// while a `host:port` is returned as-is.
pub fn to_bind_addr(addr: &str) -> String {
    if addr.parse::<u16>().is_ok() {
        format!("0.0.0.0:{}", addr)
    } else {
        addr.to_string()
    }
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
