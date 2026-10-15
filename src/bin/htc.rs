use anyhow::{bail, Result};
use clap::Parser;
use http_tunnel::{run, Cli, Command};
use tokio::{signal, sync::broadcast};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let cmd = Cli::parse().into_command()?;
    if !matches!(cmd, Command::Client(_)) {
        bail!("htc only runs the client; use hts for the server");
    }

    let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);
    tokio::spawn(async move {
        if signal::ctrl_c().await.is_ok() {
            let _ = shutdown_tx.send(true);
        }
    });

    let is_atty = atty::is(atty::Stream::Stdout);
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::from("info")))
        .with_ansi(is_atty)
        .init();

    run(cmd, shutdown_rx).await
}
